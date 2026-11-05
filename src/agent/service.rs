//! runner-up's Windows service: the job host (jobhost.rs) under Windows'
//! service manager, as SYSTEM, listening on its pipe. runner-up setup installs
//! it and names the runner account, the one the coordinator signs in as over
//! SSH, which with SYSTEM and Administrators is the only one that may open the
//! pipe.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES, FlushFileBuffers,
    MoveFileExW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, ReadFile, WRITE_DAC, WRITE_OWNER, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, OpenSCManagerW, OpenServiceW, RegisterServiceCtrlHandlerExW,
    SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_CONNECT, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CHANGE_CONFIG,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};

use super::account::{CLIENT_ACCESS, PIPE_NAME};
use super::jobhost::{Options, Server, Sink, account_job_files};
use super::jobtree;
use super::runner::{runner_arch, runner_windows_sha256, runner_zip};
use super::win32::{self, Descriptor, Handle, Sid, check, is_code, last_error, wide};
use super::winhost::{JOB_GROUP, local, new_win_host, protect_file};
use crate::config;
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::release;
use crate::version::version;

pub const SERVICE_NAME: &str = "runner-up";

/// What runner-up setup writes to service.json in the service's own folder.
#[derive(Deserialize)]
struct ServiceSettings {
    /// The runner account.
    #[serde(default)]
    user: String,
}

/// Holds the service's settings, the account record and the log. Every
/// account may make folders in ProgramData, so the service takes its own back
/// at each start: SYSTEM owns it and only SYSTEM and Administrators may use it.
pub fn state_dir() -> PathBuf {
    let d = std::env::var("ProgramData").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| r"C:\ProgramData".into());
    Path::new(&d).join("runner-up")
}

/// Holds the job folders and the runner archive, at the root of the system
/// drive: Windows' paths stay short there, as GitHub advises for its runner.
pub fn jobs_root() -> PathBuf {
    let d = std::env::var("SystemDrive").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "C:".into());
    PathBuf::from(d + r"\runner-up")
}

const STATE_DIR_SDDL: &str = "O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// The job folders' access: job accounts may list the folders above their own
/// job folder, as the runner needs, and read the runner archive; only SYSTEM
/// and Administrators change anything. Each job folder adds its own account.
pub fn job_dirs_sddl(group: &str) -> HashMap<PathBuf, String> {
    let root = jobs_root();
    HashMap::from([
        (root.clone(), format!("O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;{group})")),
        (root.join("cache"), format!("O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;{group})")),
        (root.join("jobs"), format!("O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200a9;;;{group})")),
    ])
}

/// Makes dir, or takes it back, with the access sddl gives. A folder neither
/// SYSTEM nor Administrators own was made by someone else, who chose what is
/// in it and may hold it open to change its access later: it is moved aside,
/// never trusted, and the service makes its own.
pub fn secure_dir(dir: &Path, sddl: &str) -> Result<(), String> {
    let sd = Descriptor::parse(sddl).map_err(|e| e.to_string())?;
    let sa = sd.attributes();
    let path = dir.to_string_lossy().into_owned();
    let p = wide(&path);
    if let Err(e) = check(unsafe { CreateDirectoryW(p.as_ptr(), &sa) })
        && !is_code(&e, ERROR_ALREADY_EXISTS)
    {
        return Err(e.to_string());
    }
    if !owned_by_system_at(&path)? {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let aside = wide(&format!("{path}.untrusted-{nanos}"));
        check(unsafe { MoveFileExW(p.as_ptr(), aside.as_ptr(), 0) })
            .map_err(|e| format!("{path} was not made by the service and cannot be moved aside: {e}"))?;
        check(unsafe { CreateDirectoryW(p.as_ptr(), &sa) }).map_err(|e| e.to_string())?;
        if !owned_by_system_at(&path).unwrap_or(false) {
            return Err(format!("{path} is not the service's own folder"));
        }
    }
    let h = jobtree::open(&path, READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES).map_err(|e| e.to_string())?;
    let e = jobtree::describe(h).map_err(|e| e.to_string())?;
    if !e.dir() {
        return Err(format!("{path} is not a plain folder"));
    }
    let rc = unsafe {
        SetSecurityInfo(
            e.h.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.owner(),
            std::ptr::null_mut(),
            sd.dacl(),
            std::ptr::null(),
        )
    };
    win32::status(rc).map_err(|e| e.to_string())
}

/// Whether SYSTEM or Administrators own path itself, a link included. No
/// standard account can give either one a folder.
pub fn owned_by_system_at(path: &str) -> Result<bool, String> {
    let h = jobtree::open(path, READ_CONTROL).map_err(|e| e.to_string())?;
    for k in [WinLocalSystemSid, WinBuiltinAdministratorsSid] {
        let trusted = Sid::well_known(k).map_err(|e| e.to_string())?;
        if jobtree::owner_is(h.0, &trusted).map_err(|e| e.to_string())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `runner-up service`, as Windows' service manager starts it.
pub fn run_service() -> Result<(), String> {
    const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: u32 = 1063;
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    match check(unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }) {
        Err(e) if is_code(&e, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) => {
            Err("the service runs only under Windows' service manager, as runner-up; install it with runner-up setup".into())
        }
        r => r.map_err(|e| e.to_string()),
    }
}

enum Control {
    Stop,
    Restart(String),
}

static CONTROL: OnceLock<Mutex<mpsc::Sender<Control>>> = OnceLock::new();

unsafe extern "system" fn control_handler(control: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    if (control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN)
        && let Some(tx) = CONTROL.get()
    {
        let _ = tx.lock().unwrap().send(Control::Stop);
    }
    // SERVICE_CONTROL_INTERROGATE and the rest: the status last set stands.
    NO_ERROR
}

struct Status(SERVICE_STATUS_HANDLE);

impl Status {
    fn set(&self, state: u32, accepts: u32, wait_hint: u32, exit: Option<u32>) {
        const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
        let mut s: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        s.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
        s.dwCurrentState = state;
        s.dwControlsAccepted = accepts;
        s.dwWaitHint = wait_hint;
        if let Some(code) = exit.filter(|c| *c != 0) {
            s.dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR;
            s.dwServiceSpecificExitCode = code;
        }
        unsafe { SetServiceStatus(self.0, &s) };
    }
}

unsafe extern "system" fn service_main(_: u32, _: *mut windows_sys::core::PWSTR) {
    let (tx, rx) = mpsc::channel();
    let _ = CONTROL.set(Mutex::new(tx.clone()));
    let name = wide(SERVICE_NAME);
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    let status = Status(h);
    let code = execute(&status, tx, rx);
    status.set(SERVICE_STOPPED, 0, 0, Some(code));
}

/// The service's life, from start to the exit code it stops with.
fn execute(status: &Status, tx: mpsc::Sender<Control>, rx: mpsc::Receiver<Control>) -> u32 {
    status.set(SERVICE_START_PENDING, 0, 180_000, None);
    let state_err = secure_dir(&state_dir(), STATE_DIR_SDDL).err();
    let log = open_log(state_err.is_none());
    if let Some(e) = state_err {
        log.error("cannot make its own folder", &[("path", &state_dir().display()), ("err", &e)]);
        return 1;
    }
    if begin_service_trial(&log) {
        // The service manager starts the version before, which the service
        // now names, as it starts a service that failed.
        return 2;
    }
    let (settings, me) = match prepare(&log) {
        Ok(v) => v,
        Err(e) => {
            log.error("cannot start", &[("err", &e)]);
            return 1;
        }
    };
    let host = match new_win_host(log.clone()) {
        Ok(h) => h,
        Err(e) => {
            log.error("cannot start", &[("err", &e)]);
            return 1;
        }
    };
    let download = Mutex::new(());
    let program = me.to_string_lossy().into_owned();
    let files = Box::new(move |req: &super::jobhost::JobRequest| {
        // One download at a time: a second would replace the archive while a
        // job reads it.
        let _one = download.lock().unwrap();
        let ctx = Ctx::detached_timeout(Duration::from_secs(15 * 60));
        let zip = runner_zip(&ctx, &jobs_root().join("cache"))?;
        let sum = runner_windows_sha256(runner_arch()).unwrap_or_default();
        Ok(account_job_files(&zip.to_string_lossy(), sum, &program, req))
    });
    let mut opts = Options::new(state_dir().join("job-accounts.json"), files);
    let update_log = log.clone();
    let update_me = me.clone();
    opts.update = Some(Box::new(move |v: &str, archive: &[u8]| install_service(&update_log, &update_me, v, archive, &tx)));
    opts.platform = release::platform("windows", release::goarch());
    opts.keys = release::signing_keys();
    let server = Arc::new(Server::new(opts, Box::new(host), log.clone()));
    // After a restart or a power cut, delete whatever a cut-off job left
    // before taking another.
    if let Err(e) = server.sweep("") {
        log.error("leftover job accounts could not all be deleted; those places take no job until they are", &[("err", &e)]);
    }
    let ln = match listen(&settings.user) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            log.error("cannot listen", &[("err", &e)]);
            return 1;
        }
    };
    let ctx = Ctx::background().with_cancel();
    let jobs = Arc::new(Jobs::default());
    let serving = {
        let (ln, server, log, jobs, ctx) = (ln.clone(), server.clone(), log.clone(), jobs.clone(), ctx.clone());
        std::thread::spawn(move || ln.serve(&ctx, &server, &log, &jobs))
    };
    status.set(SERVICE_RUNNING, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0, None);
    log.info("runner-up service running", &[("version", &version()), ("user", &settings.user)]);
    let started = Instant::now();
    let mut kept = false;
    let restarting = loop {
        let wait = if kept { Duration::MAX } else { SERVICE_TRIAL_WINDOW.saturating_sub(started.elapsed()) };
        match rx.recv_timeout(wait) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Ok(Control::Restart(v)) => break Some(v),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kept = true;
                keep_service_trial(&log);
            }
        }
    };
    status.set(SERVICE_STOP_PENDING, 0, 180_000, None);
    ctx.cancel();
    ln.close();
    let _ = serving.join();
    if !jobs.wait(Duration::from_secs(170)) {
        log.error("jobs were still cleaning up at stop; the next start sweeps them", &[]);
    }
    if let Some(v) = restarting {
        // A stop that is not asked for is a failure to the service manager,
        // which starts the service again, now as the new version.
        log.info("runner-up service stopped to restart as the new version", &[("version", &v)]);
        return 3;
    }
    end_service_trial(&log);
    log.info("runner-up service stopped", &[]);
    0
}

/// Makes the service's folders and reads its settings.
fn prepare(_log: &Logger) -> Result<(ServiceSettings, PathBuf), String> {
    let b = std::fs::read(state_dir().join("service.json")).map_err(|e| format!("no settings; run runner-up setup: {e}"))?;
    let st = match serde_json::from_slice::<ServiceSettings>(&b) {
        Ok(s) if !s.user.is_empty() => s,
        _ => return Err("service.json names no runner account; run runner-up setup".into()),
    };
    let q = local(JOB_GROUP)?;
    let group = match win32::lookup_sid(&q) {
        Ok(Some(s)) => s,
        Ok(None) => return Err(format!("no {JOB_GROUP} group; run runner-up setup: the account is not found")),
        Err(e) => return Err(format!("no {JOB_GROUP} group; run runner-up setup: {e}")),
    };
    let root = jobs_root();
    let sddl = job_dirs_sddl(&group);
    for dir in [root.clone(), root.join("cache"), root.join("jobs")] {
        secure_dir(&dir, &sddl[&dir])?;
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok((st, me))
}

/// The jobs being served, for the stop to wait on.
#[derive(Default)]
pub(crate) struct Jobs {
    n: Mutex<usize>,
    cv: std::sync::Condvar,
}

impl Jobs {
    fn add(&self) {
        *self.n.lock().unwrap() += 1;
    }

    fn done(&self) {
        *self.n.lock().unwrap() -= 1;
        self.cv.notify_all();
    }

    /// Whether every job ended within d.
    pub(crate) fn wait(&self, d: Duration) -> bool {
        let n = self.n.lock().unwrap();
        let (n, _) = self.cv.wait_timeout_while(n, d, |n| *n > 0).unwrap();
        *n == 0
    }
}

// The service on trial: runner-up's service replaces itself with a signed
// release (Server.update, install_service), and the new version is on trial
// for SERVICE_TRIAL_WINDOW. If it stops in that time other than when asked -
// it fails or crashes, and the service manager starts it again - it names the
// version before as the service's program again, refuses its own version, and
// stops for the service manager to start the version before. update.json in
// the service's folder holds the trial and the refused versions.

const SERVICE_TRIAL_WINDOW: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ServiceTrial {
    /// Version is on trial, replacing the program at from; started is when
    /// it began serving.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<chrono::DateTime<chrono::Local>>,
    /// The versions that stopped on trial.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
}

impl ServiceTrial {
    fn clear(&mut self) {
        self.version.clear();
        self.from.clear();
        self.started = None;
    }
}

fn service_trial_path() -> PathBuf {
    state_dir().join("update.json")
}

pub fn read_service_trial() -> ServiceTrial {
    config::read_json(&service_trial_path()).unwrap_or_default()
}

pub fn write_service_trial(t: &ServiceTrial) -> Result<(), String> {
    config::write_json(&service_trial_path(), t)?;
    protect_file(&service_trial_path())
}

/// Starts this version's trial, or, when this version stopped on trial
/// before, goes back to the version before and says so.
pub fn begin_service_trial(log: &Logger) -> bool {
    let mut t = read_service_trial();
    if t.version.is_empty() {
        return false;
    }
    if t.version != version() {
        // Set up again by an administrator, or back on the version before.
        t.clear();
    } else if t.started.is_none() {
        t.started = Some(chrono::Local::now());
        log.info(
            "runner-up service runs a new version on trial",
            &[("version", &t.version), ("before", &t.from), ("trial", &crate::go::duration::format(SERVICE_TRIAL_WINDOW))],
        );
    } else {
        log.error("runner-up service stopped on trial; going back to the version before", &[("version", &t.version), ("before", &t.from)]);
        let back = set_service_program(&t.from);
        if let Err(e) = &back {
            log.error("could not go back to the version before; the new version runs on", &[("err", e)]);
        }
        let v = t.version.clone();
        t.refused.retain(|r| *r != v);
        t.refused.push(v);
        t.clear();
        if let Err(e) = write_service_trial(&t) {
            log.error("could not record the refused version", &[("err", &e)]);
        }
        return back.is_ok();
    }
    if let Err(e) = write_service_trial(&t) {
        log.error("could not record the trial", &[("err", &e)]);
    }
    false
}

/// Keeps this version once it has served its trial.
pub fn keep_service_trial(log: &Logger) {
    let mut t = read_service_trial();
    if t.version == version() && t.started.is_some() {
        t.clear();
        if write_service_trial(&t).is_ok() {
            log.info("runner-up service kept the new version after its trial", &[("version", &version())]);
        }
    }
}

/// Ends this version's trial when it was asked to stop: nothing was wrong
/// with it.
pub fn end_service_trial(_log: &Logger) {
    let mut t = read_service_trial();
    if t.version == version() && t.started.is_some() {
        t.clear();
        let _ = write_service_trial(&t);
    }
}

/// Holds the service's programs, each in a folder of its version and its
/// SHA-256, as runner-up setup installs them: only SYSTEM and Administrators
/// change it.
pub fn program_dir() -> PathBuf {
    let d = std::env::var("ProgramFiles").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| r"C:\Program Files".into());
    Path::new(&d).join("runner-up")
}

/// Installs v, from archive, beside the service's program, names it the
/// service's program, starts its trial and has the service restart.
fn install_service(log: &Logger, me: &Path, v: &str, archive: &[u8], restart: &mpsc::Sender<Control>) -> Result<(), String> {
    let mut t = read_service_trial();
    if t.refused.iter().any(|r| r == v) {
        return Err(format!(
            "runner-up service {v} stopped on trial here before and is not tried again; an administrator's runner-up setup installs it"
        ));
    }
    let prog = release::program(archive, &release::platform("windows", release::goarch()))?;
    let sum = release::sha256(&prog);
    let dir = program_dir().join(format!("{v}-{}", &sum[..12]));
    let exe = dir.join("runner-up.exe");
    if !owned_by_system_at(&program_dir().to_string_lossy()).unwrap_or(false) {
        return Err(format!("{} is not the service's own folder; run runner-up setup", program_dir().display()));
    }
    if std::fs::read(&exe).map(|b| release::sha256(&b) != sum).unwrap_or(true) {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let new = dir.join("runner-up.exe.new");
        std::fs::write(&new, &prog).map_err(|e| e.to_string())?;
        std::fs::rename(&new, &exe).map_err(|e| e.to_string())?;
    }
    let ctx = Ctx::detached_timeout(Duration::from_secs(60));
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("version");
    let out = crate::proc::output(&ctx, &mut cmd, Vec::new());
    let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.is_err() || got != v {
        return Err(format!("the new program does not run as {v}: {got:?}, {}", out.status.err().unwrap_or_else(|| "<nil>".into())));
    }
    write_service_trial(&ServiceTrial {
        version: v.into(),
        from: me.to_string_lossy().into_owned(),
        started: None,
        refused: t.refused.clone(),
    })?;
    if let Err(e) = set_service_program(&exe.to_string_lossy()) {
        t.clear();
        let _ = write_service_trial(&t);
        return Err(e);
    }
    log.info("installed the new version as the service's program", &[("version", &v), ("path", &exe.display())]);
    let _ = restart.send(Control::Restart(v.into()));
    Ok(())
}

struct Service(*mut core::ffi::c_void);

impl Drop for Service {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Names exe the service's program, which the service manager starts again 10
/// seconds after any stop it did not ask for.
pub fn set_service_program(exe: &str) -> Result<(), String> {
    let m = Service(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) });
    if m.0.is_null() {
        return Err(last_error().to_string());
    }
    let s =
        Service(unsafe { OpenServiceW(m.0, wide(SERVICE_NAME).as_ptr(), SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG | SERVICE_START) });
    if s.0.is_null() {
        return Err(last_error().to_string());
    }
    let bin = wide(&format!("\"{exe}\" service"));
    let null = std::ptr::null();
    check(unsafe {
        ChangeServiceConfigW(
            s.0,
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            bin.as_ptr(),
            null,
            std::ptr::null_mut(),
            null,
            null,
            null,
            null,
        )
    })
    .map_err(|e| format!("could not name the service's program: {e}"))?;
    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 10_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 10_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 60_000 },
    ];
    let fa = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86400,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    check(unsafe { ChangeServiceConfig2W(s.0, SERVICE_CONFIG_FAILURE_ACTIONS, &fa as *const _ as _) }).map_err(|e| e.to_string())?;
    let flag = SERVICE_FAILURE_ACTIONS_FLAG { fFailureActionsOnNonCrashFailures: 1 };
    check(unsafe { ChangeServiceConfig2W(s.0, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, &flag as *const _ as _) }).map_err(|e| e.to_string())
}

fn open_log(ok: bool) -> Logger {
    if !ok {
        return Logger::stderr();
    }
    let path = state_dir().join("service.log");
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return Logger::stderr();
    };
    let _ = protect_file(&path);
    Logger::new(Box::new(f))
}

/// The pipe the service listens on.
pub struct Listener {
    path: String,
    sd: Descriptor,
    /// The wait for the next client's event.
    event: Handle,
    state: Mutex<ListenState>,
}

struct ListenState {
    closed: bool,
    pending: HANDLE,
}

unsafe impl Send for Listener {}
unsafe impl Sync for Listener {}

/// Makes the pipe, which only SYSTEM, Administrators and the machine's runner
/// account may open.
fn listen(runner: &str) -> Result<Listener, String> {
    let q = local(runner)?;
    let sid = match win32::lookup_sid(&q) {
        Ok(Some(s)) => s,
        Ok(None) => return Err(format!("{runner}: no such account")),
        Err(e) => return Err(format!("{runner}: {e}")),
    };
    listen_at(PIPE_NAME, &pipe_sddl(&sid))
}

/// Makes the pipe at path with the access sddl gives.
pub fn listen_at(path: &str, sddl: &str) -> Result<Listener, String> {
    let sd = Descriptor::parse(sddl).map_err(|e| e.to_string())?;
    let event = Handle::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) }).map_err(|e| e.to_string())?;
    let l = Listener { path: path.into(), sd, event, state: Mutex::new(ListenState { closed: false, pending: std::ptr::null_mut() }) };
    // The first instance proves no one else holds the name.
    let h = l.instance(true).map_err(|e| e.to_string())?;
    l.state.lock().unwrap().pending = h;
    Ok(l)
}

/// The pipe's access, runner being the runner account's SID. SYSTEM owns the
/// pipe, which the agent checks: no other account can make a pipe SYSTEM owns.
pub fn pipe_sddl(runner: &str) -> String {
    format!("O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;{CLIENT_ACCESS:#x};;;{runner})")
}

impl Listener {
    /// Makes one pipe instance for the next client. It is overlapped: a run
    /// request keeps a read waiting, to hear the agent leave, while it writes
    /// the job's output. Windows runs one call at a time on a handle made
    /// without FILE_FLAG_OVERLAPPED, so every write would wait until the
    /// agent left.
    fn instance(&self, first: bool) -> io::Result<HANDLE> {
        let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let sa = self.sd.attributes();
        let h = unsafe {
            CreateNamedPipeW(
                wide(&self.path).as_ptr(),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 << 10,
                64 << 10,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(h)
    }

    pub(crate) fn serve(&self, ctx: &Ctx, server: &Arc<Server>, log: &Logger, jobs: &Arc<Jobs>) {
        loop {
            let h = self.state.lock().unwrap().pending;
            let err = self.connect(h).err();
            if self.state.lock().unwrap().closed {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                return;
            }
            // The next client's instance, made before this one is served.
            let mut wait = Duration::from_millis(100);
            let closed = loop {
                let next = self.instance(false);
                let mut s = self.state.lock().unwrap();
                let closed = s.closed;
                match next {
                    Ok(n) => {
                        s.pending = n;
                        break closed;
                    }
                    Err(_) if closed => break closed,
                    Err(e) => {
                        drop(s);
                        log.error("cannot make the next pipe instance; trying again", &[("err", &e)]);
                        std::thread::sleep(wait);
                        wait = (wait * 2).min(Duration::from_secs(10));
                    }
                }
            };
            if closed {
                // The service is stopping: neither this client nor a new
                // instance is served.
                let mut s = self.state.lock().unwrap();
                if s.pending != h {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(s.pending) };
                }
                s.pending = std::ptr::null_mut();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                return;
            }
            if let Some(e) = err.filter(|e| !is_code(e, ERROR_PIPE_CONNECTED)) {
                log.warn("a client could not connect", &[("err", &e)]);
                unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                continue;
            }
            jobs.add();
            let (ctx, server, log, jobs) = (ctx.clone(), server.clone(), log.clone(), jobs.clone());
            let h = Handle(h);
            std::thread::spawn(move || {
                handle(&ctx, h, &server, &log);
                jobs.done();
            });
        }
    }

    /// Waits until a client opens h, the pending instance.
    fn connect(&self, h: HANDLE) -> io::Result<()> {
        check(unsafe { ResetEvent(self.event.0) })?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        if let Err(e) = check(unsafe { ConnectNamedPipe(h, &mut ov) }) {
            if !is_code(&e, ERROR_IO_PENDING) {
                return Err(e);
            }
            let mut n = 0;
            check(unsafe { GetOverlappedResult(h, &ov, &mut n, 1) })?;
        }
        Ok(())
    }

    /// Stops taking clients: it wakes the waiting ConnectNamedPipe by
    /// connecting to it.
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        let h =
            unsafe { CreateFileW(wide(&self.path).as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h != INVALID_HANDLE_VALUE {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
        }
    }
}

/// Serves one agent. The agent's end of the pipe closing is how the service
/// learns it has gone: it stopped the job, or it or its connection went.
pub(crate) fn handle(ctx: &Ctx, h: Handle, server: &Server, _log: &Logger) {
    let pipe = Arc::new(Overlapped { h, gone: AtomicBool::new(false) });
    let out: Sink = Arc::new(Mutex::new(Box::new(PipeEnd(pipe.clone()))));
    let _ = server.serve(ctx, Box::new(PipeEnd(pipe.clone())), &out);
    unsafe {
        FlushFileBuffers(pipe.h.0);
        DisconnectNamedPipe(pipe.h.0);
    }
}

/// An overlapped pipe instance: each read and write waits on its own event,
/// so a read waiting to hear the agent leave does not hold up the job's
/// output.
struct Overlapped {
    h: Handle,
    gone: AtomicBool,
}

unsafe impl Send for Overlapped {}
unsafe impl Sync for Overlapped {}

impl Overlapped {
    fn io(&self, write: bool, buf: *mut u8, len: u32) -> io::Result<usize> {
        if self.gone.load(SeqCst) {
            return Ok(0);
        }
        let event = Handle::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.0;
        let mut n = 0u32;
        let ok =
            unsafe { if write { WriteFile(self.h.0, buf, len, &mut n, &mut ov) } else { ReadFile(self.h.0, buf, len, &mut n, &mut ov) } };
        if let Err(e) = check(ok) {
            if !is_code(&e, ERROR_IO_PENDING) {
                return self.ended(e);
            }
            if let Err(e) = check(unsafe { GetOverlappedResult(self.h.0, &ov, &mut n, 1) }) {
                return self.ended(e);
            }
        }
        Ok(n as usize)
    }

    /// A broken pipe reads as its end: the agent has gone.
    fn ended(&self, e: io::Error) -> io::Result<usize> {
        const ERROR_BROKEN_PIPE: u32 = 109;
        const ERROR_NO_DATA: u32 = 232;
        const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
        if is_code(&e, ERROR_BROKEN_PIPE) || is_code(&e, ERROR_PIPE_NOT_CONNECTED) || is_code(&e, ERROR_NO_DATA) {
            self.gone.store(true, SeqCst);
            return Ok(0);
        }
        Err(e)
    }
}

struct PipeEnd(Arc<Overlapped>);

impl Read for PipeEnd {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.io(false, b.as_mut_ptr(), b.len().min(u32::MAX as usize) as u32)
    }
}

impl Write for PipeEnd {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.0.io(true, b.as_ptr() as *mut u8, b.len().min(u32::MAX as usize) as u32)?;
        if n == 0 && !b.is_empty() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
