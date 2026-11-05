//! Host on Windows: the job accounts, their sign-in, containment and cleanup,
//! run as SYSTEM by runner-up's service.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;

use base64::Engine;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, LUID,
    SetHandleInformation, WAIT_FAILED,
};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    PROTECTED_DACL_SECURITY_INFORMATION, SE_PRIVILEGE_ENABLED, SE_PRIVILEGE_REMOVED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    TOKEN_QUERY, TokenPrivileges,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::StationsAndDesktops::{HDESK, HWINSTA};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessAsUserW, EXTENDED_STARTUPINFO_PRESENT, GetActiveProcessorCount, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, WaitForSingleObject,
};

use super::jobhost::{Account, ERR_ACCOUNT_EXISTS, Host, Running, job_command_line};
use super::service::jobs_root;
use super::win32::{self, Descriptor, Handle, Sid, check, is_code, last_error, wide};
use super::jobtree;
use crate::config::Share;
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::proc;

/// A started job and everything it holds.
struct WinJob {
    log: Logger,
    process: Handle,
    job: Handle,
    out: Mutex<Option<std::fs::File>>,
    stop: Once,
    held: Mutex<Option<Held>>,
}

/// What a job's sign-in holds, let go of when it drops.
struct Held {
    log: Logger,
    token: Handle,
    profile: HANDLE,
    station: HWINSTA,
    desktop: HDESK,
}

unsafe impl Send for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        if !self.profile.is_null() {
            // Processes that have just ended may still hold the profile's
            // registry for a moment.
            for i in 0..10 {
                match win32::unload_user_profile(self.token.0, self.profile) {
                    Ok(()) => break,
                    Err(e) if i == 9 => self.log.warn("could not unload the profile", &[("err", &e.to_string())]),
                    Err(_) => std::thread::sleep(Duration::from_secs(1)),
                }
            }
        }
        if !self.desktop.is_null() {
            win32::close_desktop(self.desktop);
        }
        if !self.station.is_null() {
            win32::close_window_station(self.station);
        }
    }
}

fn set_dacl(path: &Path, sddl: &str) -> Result<(), String> {
    let sd = Descriptor::parse(sddl).map_err(|e| e.to_string())?;
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide(&path.to_string_lossy()).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sd.dacl(),
            std::ptr::null(),
        )
    };
    win32::status(rc).map_err(|e| e.to_string())
}

/// Keeps a file to SYSTEM and Administrators.
pub fn protect_file(path: &Path) -> Result<(), String> {
    set_dacl(path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)")
}

/// Host on Windows, run as SYSTEM.
pub struct WinHost {
    log: Logger,
    jobs_dir: String,
    group: String,
    powershell: String,
    /// The machine's processor count, which each job's processor cap is a
    /// share of.
    threads: u32,
    /// SetProcessWindowStation applies to the whole service; one job's
    /// desktop is made at a time.
    station: Mutex<()>,
}

const HIDDEN_ACCOUNTS: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";
const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";
const USERS_SID: &str = "S-1-5-32-545";
const ACCOUNT_COMMENT: &str = "runner-up job account; the runner-up service deletes it after its job";
/// Holds every job account. runner-up setup lets it sign in only as a batch
/// logon: never at the screen, over Remote Desktop or from the network.
pub const JOB_GROUP: &str = "runner-up-jobs";

pub fn new_win_host(log: Logger) -> Result<WinHost, String> {
    let root = env_or("SystemRoot", r"C:\Windows");
    let threads = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    if threads == 0 {
        return Err("no processors counted".into());
    }
    // Backup and restore open whatever a job account left, whatever its
    // permissions; debug ends a job process that locked SYSTEM out of it.
    enable_privileges(&[
        "SeBackupPrivilege",
        "SeRestorePrivilege",
        "SeDebugPrivilege",
        "SeAssignPrimaryTokenPrivilege",
        "SeIncreaseQuotaPrivilege",
    ])?;
    Ok(WinHost {
        log,
        jobs_dir: jobs_root().join("jobs").to_string_lossy().into_owned(),
        group: JOB_GROUP.into(),
        powershell: format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"),
        threads,
        station: Mutex::new(()),
    })
}

fn enable_privileges(names: &[&str]) -> Result<(), String> {
    let mut t = std::ptr::null_mut();
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut t) }).map_err(|e| e.to_string())?;
    let t = Handle(t);
    for n in names {
        let luid = privilege_value(n)?;
        let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
        check(unsafe { AdjustTokenPrivileges(t.0, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) })
            .map_err(|e| format!("{n}: {e}"))?;
    }
    Ok(())
}

/// Qualifies a name with this machine's, so a domain account of the same
/// name can never be meant.
pub fn local(name: &str) -> Result<String, String> {
    Ok(format!("{}\\{name}", win32::computer_name().map_err(|e| e.to_string())?))
}

impl Host for WinHost {
    fn lookup_account(&self, name: &str) -> Result<String, String> {
        Ok(win32::lookup_sid(&local(name)?).map_err(|e| e.to_string())?.unwrap_or_default())
    }

    fn create_account(&self, name: &str, password: &[u16]) -> Result<String, String> {
        let mut pw: Vec<u16> = password.iter().copied().chain([0]).collect();
        let made = win32::net_user_add(&wide(name), &pw, &wide(ACCOUNT_COMMENT));
        pw.fill(0);
        match made {
            Err(e) if is_code(&e, win32::NERR_USER_EXISTS) => return Err(ERR_ACCOUNT_EXISTS.into()),
            Err(e) => return Err(e.to_string()),
            Ok(()) => {}
        }
        let sid = self.lookup_account(name)?;
        if sid.is_empty() {
            return Err("made, but not found".into());
        }
        Ok(sid)
    }

    fn prepare(&self, a: &Account, files: &HashMap<String, Vec<u8>>) -> Result<(), String> {
        let sid = Sid::parse(&a.sid).map_err(|e| e.to_string())?;
        let users = win32::lookup_account_name(&Sid::parse(USERS_SID).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        win32::net_local_group_add_member(&users, &sid).map_err(|e| format!("Users: {e}"))?;
        win32::net_local_group_add_member(&self.group, &sid).map_err(|e| format!("{}: {e}", self.group))?;
        win32::reg_set_dword(HIDDEN_ACCOUNTS, &a.name, 0).map_err(|e| e.to_string())?;
        // Only the account, SYSTEM and Administrators; Modify for the account,
        // and the same for whatever it owns, so it cannot lock SYSTEM out.
        let dir = self.folder(a);
        std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
        set_dacl(&dir, &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;{})(A;OICI;0x1301bf;;;OW)", a.sid))?;
        for (name, b) in files {
            std::fs::write(dir.join(name), b).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn start(&self, a: &Account, password: &[u16], input: &[u8], share: Share) -> Result<Box<dyn Running>, String> {
        let log = self.log.with(&[("account", &a.name)]);
        let mut pw: Vec<u16> = password.iter().copied().chain([0]).collect();
        let token = win32::logon_user(&a.name, &pw);
        pw.fill(0);
        let token = token.map_err(|e| format!("sign-in: {e}"))?;
        let null = std::ptr::null_mut();
        let mut held = Held { log: log.clone(), token, profile: null, station: null, desktop: null };
        without_host_control_privileges(held.token.0).map_err(|e| format!("host-control rights: {e}"))?;
        held.profile = win32::load_user_profile(held.token.0, &a.name).map_err(|e| format!("profile: {e}"))?;
        let env = win32::Environment::of(held.token.0).map_err(|e| format!("environment: {e}"))?;
        self.make_desktop(&mut held, a).map_err(|e| format!("desktop: {e}"))?;
        let job = self.make_job_object(share).map_err(|e| format!("job object: {e}"))?;

        // The child's ends of its standard input and output are the only
        // handles it inherits.
        let (child_in, parent_in) = win32::inheritable_pipe().map_err(|e| e.to_string())?;
        let (parent_out, child_out) = win32::inheritable_pipe().map_err(|e| e.to_string())?;
        unsafe {
            SetHandleInformation(parent_in.0, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(parent_out.0, HANDLE_FLAG_INHERIT, 0);
        }
        let out = win32::into_file(parent_out);
        let mut stdin = win32::into_file(parent_in);

        let handles = [child_in.0, child_out.0];
        let mut attrs = win32::AttributeList::handles(&handles).map_err(|e| e.to_string())?;
        let mut desktop = wide(&format!("{}\\default", station_name(a)));
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = child_in.0;
        si.StartupInfo.hStdOutput = child_out.0;
        si.StartupInfo.hStdError = child_out.0;
        si.lpAttributeList = attrs.ptr();

        let dir = self.folder(a);
        let app = wide(&self.powershell);
        let mut cmd = wide(&job_command_line(&self.powershell, &dir.to_string_lossy()));
        let cwd = wide(&dir.to_string_lossy());
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        check(unsafe {
            CreateProcessAsUserW(
                held.token.0,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env.ptr(),
                cwd.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            )
        })
        .map_err(|e| format!("start: {e}"))?;
        let process = Handle(pi.hProcess);
        let thread = Handle(pi.hThread);
        if let Err(e) = check(unsafe { AssignProcessToJobObject(job.0, process.0) }) {
            unsafe { TerminateProcess(process.0, 1) };
            return Err(format!("contain: {e}"));
        }
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            return Err(last_error().to_string());
        }
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        Ok(Box::new(WinJob {
            log,
            process,
            job,
            out: Mutex::new(Some(out)),
            stop: Once::new(),
            held: Mutex::new(Some(held)),
        }))
    }

    /// Stops anything signing in as the account again, a scheduled task or
    /// another service on its behalf included, before its processes are
    /// ended.
    fn disable_account(&self, a: &Account) -> Result<(), String> {
        let sid = self.lookup_account(&a.name)?;
        if sid.is_empty() {
            return Ok(());
        }
        if sid != a.sid {
            return Err(format!("{} now has another SID, {sid}; leaving it", a.name));
        }
        match win32::net_user_set_flags(&a.name, win32::ACCOUNT_FLAGS | win32::UF_ACCOUNTDISABLE) {
            Err(e) if is_code(&e, win32::NERR_USER_NOT_FOUND) => Ok(()),
            r => r.map_err(|e| e.to_string()),
        }
    }

    /// Deletes every scheduled task that would run as the account.
    fn remove_tasks(&self, a: &Account) -> Result<(), String> {
        self.run_powershell(
            "$sid = $env:RUNNER_UP_JOB_SID; $name = $env:RUNNER_UP_JOB_NAME
$tasks = @(Get-ScheduledTask | Where-Object { $u = [string]$_.Principal.UserId; $u -eq $sid -or $u -eq $name -or $u.EndsWith('\\' + $name) })
$tasks | Unregister-ScheduledTask -Confirm:$false
",
            &[("RUNNER_UP_JOB_SID", &a.sid), ("RUNNER_UP_JOB_NAME", &a.name)],
        )
    }

    /// Ends every process signed in as the account, inside its job or not
    /// (a process it started through Windows' own services is not).
    fn end_processes(&self, a: &Account) -> Result<(), String> {
        for _ in 0..10 {
            let (ps, res) = processes_of(&a.sid);
            for p in &ps {
                unsafe {
                    TerminateProcess(p.0, 1);
                    WaitForSingleObject(p.0, 5000);
                }
            }
            res?;
            if ps.is_empty() {
                return Ok(());
            }
        }
        Err("its processes keep coming back".into())
    }

    /// Deletes what the account owns in the shared folders. It never follows
    /// a link: a link the account made is deleted itself.
    fn remove_shared_files(&self, a: &Account) -> Result<(), String> {
        let sid = Sid::parse(&a.sid).map_err(|e| e.to_string())?;
        let mut errs = Vec::new();
        for (path, deep) in shared_folders() {
            let root = match jobtree::open_root(&path) {
                Err(e) if not_found(&e) => continue,
                Err(e) => {
                    errs.push(e.to_string());
                    continue;
                }
                Ok(r) => r,
            };
            if let Err(e) = jobtree::remove_owned(&root, &sid, deep) {
                errs.push(format!("{path}: {e}"));
            }
        }
        errs_join(errs)
    }

    /// Deletes the account's profile: Windows' own way first, then by hand
    /// for whatever that left.
    fn delete_profile(&self, a: &Account) -> Result<(), String> {
        let folders = self.profile_folders(a);
        let mut err = None;
        for _ in 0..5 {
            match win32::delete_profile(&a.sid) {
                Ok(()) => {
                    err = None;
                    break;
                }
                Err(e) if not_found(&e) => {
                    err = None;
                    break;
                }
                Err(e) => err = Some(e),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        if let Some(e) = err {
            self.log.warn("Windows did not delete the profile; deleting it by hand", &[("account", &a.name), ("err", &e.to_string())]);
        }
        let mut errs = Vec::new();
        for name in folders {
            if let Err(e) = jobtree::remove_at(&users_dir(), &name) {
                errs.push(e);
            }
        }
        if let Err(e) = win32::reg_delete_key(&format!("{PROFILE_LIST}\\{}", a.sid))
            && !is_code(&e, ERROR_FILE_NOT_FOUND)
        {
            errs.push(e.to_string());
        }
        errs_join(errs)
    }

    fn remove_folder(&self, a: &Account) -> Result<(), String> {
        jobtree::remove_at(&self.jobs_dir, &a.name)
    }

    fn delete_account(&self, a: &Account) -> Result<(), String> {
        // Only the account this record names: the same name with another SID
        // is someone else's.
        let sid = self.lookup_account(&a.name)?;
        if !sid.is_empty() && sid != a.sid {
            return Err(format!("{} now has another SID, {sid}; leaving it", a.name));
        }
        if !sid.is_empty()
            && let Err(e) = win32::net_user_del(&a.name)
            && !is_code(&e, win32::NERR_USER_NOT_FOUND)
        {
            return Err(e.to_string());
        }
        match win32::reg_delete_value(HIDDEN_ACCOUNTS, &a.name) {
            Err(e) if !is_code(&e, ERROR_FILE_NOT_FOUND) => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    fn verify(&self, a: &Account) -> Result<(), String> {
        let mut left = Vec::new();
        if self.lookup_account(&a.name).map_or(true, |sid| sid == a.sid) {
            left.push("the account".to_string());
        }
        let (ps, res) = processes_of(&a.sid);
        if res.is_err() || !ps.is_empty() {
            left.push("processes".into());
        }
        if jobtree::exists_at(&self.jobs_dir, &a.name) {
            left.push("its job folder".into());
        }
        for name in self.profile_folders(a) {
            if jobtree::exists_at(&users_dir(), &name) {
                left.push(format!("its profile folder {name}"));
            }
        }
        if win32::reg_key_exists(&format!("{PROFILE_LIST}\\{}", a.sid)) {
            left.push("its profile record".into());
        }
        if left.is_empty() { Ok(()) } else { Err(format!("left behind: {}", left.join(", "))) }
    }
}

impl WinHost {
    /// Runs script as the service, with env added, and fails on any error
    /// the script stops on.
    fn run_powershell(&self, script: &str, env: &[(&str, &str)]) -> Result<(), String> {
        let script = format!("$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'\n{script}");
        let b: Vec<u8> = script.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let mut cmd = std::process::Command::new(&self.powershell);
        cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]);
        cmd.arg(base64::engine::general_purpose::STANDARD.encode(b));
        cmd.envs(env.iter().copied());
        let (out, res) = proc::combined_output(&Ctx::background(), &mut cmd);
        res.map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&out).trim()))
    }

    fn folder(&self, a: &Account) -> PathBuf {
        Path::new(&self.jobs_dir).join(&a.name)
    }

    fn profile_path(&self, sid: &str) -> String {
        let Some(p) = win32::reg_string(&format!("{PROFILE_LIST}\\{sid}"), "ProfileImagePath") else {
            return String::new();
        };
        win32::expand_env(&p)
    }

    /// The names in the Users folder the account's profile may have: its
    /// own, and the one Windows recorded.
    fn profile_folders(&self, a: &Account) -> Vec<String> {
        let mut out = vec![a.name.clone()];
        let p = self.profile_path(&a.sid);
        let p = Path::new(&p);
        let (Some(dir), Some(base)) = (p.parent(), p.file_name()) else {
            return out;
        };
        let base = base.to_string_lossy();
        if dir.to_string_lossy().eq_ignore_ascii_case(&users_dir())
            && base.to_lowercase().starts_with(&a.name.to_lowercase())
            && !base.eq_ignore_ascii_case(&a.name)
        {
            out.push(base.into_owned());
        }
        out
    }
}

/// The folders every account on the machine may write in. Only C:\'s own
/// entries are read: anything deeper there belongs to a folder below.
fn shared_folders() -> Vec<(String, bool)> {
    let drive = env_or("SystemDrive", "C:");
    let root = env_or("SystemRoot", &format!("{drive}\\Windows"));
    let data = env_or("ProgramData", &format!("{drive}\\ProgramData"));
    let mut out = vec![(format!("{drive}\\"), false), (data, true), (format!("{drive}\\Users\\Public"), true)];
    for p in [
        "Temp",
        "Tasks",
        "tracing",
        "Registration\\CRMLog",
        "System32\\Tasks",
        "System32\\spool\\drivers\\color",
        "System32\\Microsoft\\Crypto\\RSA\\MachineKeys",
        "System32\\FxsTmp",
        "System32\\com\\dmp",
        "SysWOW64\\Tasks",
        "SysWOW64\\FxsTmp",
        "SysWOW64\\com\\dmp",
    ] {
        out.push((format!("{root}\\{p}"), true));
    }
    out
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn users_dir() -> String {
    format!("{}\\Users", env_or("SystemDrive", "C:"))
}

fn not_found(e: &io::Error) -> bool {
    is_code(e, ERROR_FILE_NOT_FOUND) || is_code(e, ERROR_PATH_NOT_FOUND)
}

fn errs_join(errs: Vec<String>) -> Result<(), String> {
    if errs.is_empty() { Ok(()) } else { Err(errs.join("\n")) }
}

const HOST_CONTROL_PRIVILEGES: [&str; 3] = ["SeShutdownPrivilege", "SeRemoteShutdownPrivilege", "SeTimeZonePrivilege"];

/// Deletes these rights from the job's token and checks they are gone. A
/// deleted privilege cannot be enabled again, and every token copied from
/// this one lacks it too: the job's processes, and those Windows' services
/// start for it, such as WMI's. Task Scheduler signs a job account in afresh
/// for no task: it refuses one that would (S4U), and an interactive-only one
/// never runs.
fn without_host_control_privileges(t: HANDLE) -> Result<(), String> {
    let mut removed = Vec::new();
    for n in HOST_CONTROL_PRIVILEGES {
        let luid = privilege_value(n)?;
        // Removing one the token never held is not an error.
        let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_REMOVED }] };
        check(unsafe { AdjustTokenPrivileges(t, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) })
            .map_err(|e| format!("{n}: {e}"))?;
        removed.push((luid, n));
    }
    for p in token_privileges(t)? {
        if let Some((_, n)) = removed.iter().find(|(l, _)| l.LowPart == p.Luid.LowPart && l.HighPart == p.Luid.HighPart) {
            return Err(format!("the token still holds {n}"));
        }
    }
    Ok(())
}

fn privilege_value(name: &str) -> Result<LUID, String> {
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    check(unsafe { LookupPrivilegeValueW(std::ptr::null(), wide(name).as_ptr(), &mut luid) }).map_err(|e| format!("{name}: {e}"))?;
    Ok(luid)
}

fn token_privileges(t: HANDLE) -> Result<Vec<LUID_AND_ATTRIBUTES>, String> {
    let mut n = 0u32;
    unsafe { GetTokenInformation(t, TokenPrivileges, std::ptr::null_mut(), 0, &mut n) };
    if n == 0 {
        return Err("the token's privileges could not be read".into());
    }
    let mut b = vec![0u64; (n as usize).div_ceil(8)];
    check(unsafe { GetTokenInformation(t, TokenPrivileges, b.as_mut_ptr() as _, n, &mut n) }).map_err(|e| e.to_string())?;
    let tp = b.as_ptr() as *const TOKEN_PRIVILEGES;
    let count = unsafe { (*tp).PrivilegeCount } as usize;
    Ok(unsafe { std::slice::from_raw_parts((*tp).Privileges.as_ptr(), count) }.to_vec())
}

fn station_name(a: &Account) -> String {
    format!("runner-up-{}", a.name)
}

impl WinHost {
    /// Gives the job a window station and desktop of its own, which only it,
    /// SYSTEM and Administrators may use: a window can open there, and
    /// nothing reaches the machine's own desktop or another job's.
    fn make_desktop(&self, j: &mut Held, a: &Account) -> io::Result<()> {
        let sd = Descriptor::parse(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{})", a.sid))?;
        let sa = sd.attributes();
        let _station = self.station.lock().unwrap();
        j.station = win32::create_window_station(&station_name(a), &sa)?;
        let old = win32::process_window_station();
        win32::set_process_window_station(j.station)?;
        let made = win32::create_desktop("default", &sa);
        let back = win32::set_process_window_station(old);
        j.desktop = made?;
        back
    }

    /// Contains a job to its share: a hard cap of share.cores of the
    /// machine's processors and share.memory_mb for all its processes
    /// together.
    fn make_job_object(&self, share: Share) -> Result<Handle, String> {
        let mut rate = (share.cores as u32).saturating_mul(10000) / self.threads;
        if !(1..=10000).contains(&rate) {
            rate = 10000;
        }
        let job = Handle::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) }).map_err(|e| e.to_string())?;
        // Every process in it ends when the service closes it, none may leave
        // it, and together they run below normal priority within their memory.
        // It sets no window limits: Chromium's sandbox, Electron's included,
        // cannot start a page while any is set. The job's windows, clipboard
        // and atoms belong to its own window station, which only it may open.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_PRIORITY_CLASS | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        limits.BasicLimitInformation.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS;
        limits.JobMemoryLimit = ((share.memory_mb as u64) << 20) as usize;
        check(unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })
        .map_err(|e| format!("limits: {e}"))?;
        let cap = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: rate },
        };
        check(unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectCpuRateControlInformation,
                &cap as *const _ as _,
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        })
        .map_err(|e| format!("processor cap: {e}"))?;
        Ok(job)
    }
}

impl Running for WinJob {
    fn output(&self) -> Box<dyn Read + Send> {
        Box::new(self.out.lock().unwrap().take().expect("the job's output is read once"))
    }

    fn wait(&self) -> Result<i32, String> {
        if unsafe { WaitForSingleObject(self.process.0, INFINITE) } == WAIT_FAILED {
            return Err(last_error().to_string());
        }
        let mut code = 0u32;
        check(unsafe { GetExitCodeProcess(self.process.0, &mut code) }).map_err(|e| e.to_string())?;
        Ok(code as i32)
    }

    fn stop(&self) {
        self.stop.call_once(|| {
            if let Err(e) = check(unsafe { TerminateJobObject(self.job.0, 1) }) {
                self.log.warn("could not end the job's processes", &[("err", &e.to_string())]);
            }
        });
    }

    fn close(&self) {
        self.stop();
        drop(self.out.lock().unwrap().take());
        drop(self.held.lock().unwrap().take());
    }
}

/// The processes signed in as sid, opened to be ended. Windows' own list
/// names each process's account, so a process cannot hide by locking its
/// token; one SYSTEM cannot open is an error. A process started after the
/// list was taken has only reused a listed number, and is left alone.
fn processes_of(sid: &str) -> (Vec<Handle>, Result<(), String>) {
    let mut listed = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimeAsFileTime(&mut listed) };
    let ids = match win32::process_ids_of(sid) {
        Ok(ids) => ids,
        Err(e) => return (Vec::new(), Err(e.to_string())),
    };
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for id in ids {
        let p = match Handle::new(unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, id) }) {
            Ok(p) => p,
            // It has ended.
            Err(e) if is_code(&e, ERROR_INVALID_PARAMETER) => continue,
            Err(e) => {
                errs.push(format!("process {id}: {e}"));
                continue;
            }
        };
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        if let Err(e) = check(unsafe { GetProcessTimes(p.0, &mut created, &mut exited, &mut kernel, &mut user) }) {
            errs.push(format!("process {id}: {e}"));
            continue;
        }
        if filetime(created) > filetime(listed) {
            continue;
        }
        out.push(p);
    }
    (out, if errs.is_empty() { Ok(()) } else { Err(errs.join("\n")) })
}

fn filetime(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}
