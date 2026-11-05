use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, GetAce, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::*;
use crate::agent::account::dial_pipe;
use crate::agent::service::{Jobs, handle, listen_at, pipe_sddl};
use crate::agent::win32::{self, Descriptor, Handle, Sid, check, is_code, sid_is, wide};

/// This test's account, as a SID string.
fn me() -> String {
    let mut token = std::ptr::null_mut();
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }).unwrap();
    let token = Handle(token);
    let mut buf = vec![0u64; 64];
    let mut n = 0;
    check(unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr() as _, (buf.len() * 8) as u32, &mut n) }).unwrap();
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    win32::sid_string(user.User.Sid).unwrap()
}

fn test_pipe_path() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!(r"\\.\pipe\runner-up-test-{}-{nanos}", std::process::id())
}

/// The agent's output as ask_service writes it, one line a write.
struct Listening(Sender<String>);

impl Write for Listening {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(String::from_utf8_lossy(b).into_owned());
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads lines for up to 20 seconds until one holds want, and gives it.
fn hear(lines: &Receiver<String>, heard: &mut String, want: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let line = lines.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()?;
        heard.push_str(&line);
        if line.contains(want) {
            return Some(line);
        }
    }
}

// Intent (a bug found on a real PC): a Windows account job ran to its end,
// but the agent heard nothing from runner-up's service - not even its
// "running as" line - until the agent left, so the coordinator never saw the
// job reach the runner and held the place until its start timeout. The
// service's pipe did synchronous I/O, so every write waited behind the read
// that watches for the agent leaving. Over a real pipe served by the
// service's own listener, the agent hears "running as" and the job's output
// while the job runs and the agent stays connected, the service still stops
// the job and deletes its account when the agent leaves, and a job that ends
// on its own ends the agent's request with the start script's exit code.
#[test]
fn the_agent_hears_its_job_while_connected_and_the_service_hears_it_leave() {
    let me = me();
    // A pipe like the service's, owned by this test's account and open to it
    // alone, so the agent's owner check holds without SYSTEM.
    let path = test_pipe_path();
    let ln = Arc::new(listen_at(&path, &format!("O:{me}D:P(A;;GA;;;{me})")).unwrap());
    let h = new_fake_host();
    h.state().block = true;
    let dir = tempfile::tempdir().unwrap();
    let s = new_test_server(&h, dir.path());
    let service = Ctx::background().with_cancel();
    let jobs = Arc::new(Jobs::default());
    let serving = {
        let (ln, s, jobs, service) = (ln.clone(), s.clone(), jobs.clone(), service.clone());
        std::thread::spawn(move || ln.serve(&service, &s, &Logger::discard(), &jobs))
    };
    struct Stop(Option<Box<dyn FnOnce()>>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }
    let _stop = {
        let (ln, jobs, service) = (ln.clone(), jobs.clone(), service.clone());
        Stop(Some(Box::new(move || {
            service.cancel();
            ln.close();
            let _ = serving.join();
            assert!(jobs.wait(Duration::from_secs(30)), "the service's listener did not stop");
        })))
    };

    // A job that keeps running while the agent stays connected.
    let conn = dial_pipe(&path, &me).unwrap();
    let agent = Ctx::background().with_cancel();
    let (tx, lines) = channel();
    let (asked_tx, asked) = channel();
    {
        let agent = agent.clone();
        std::thread::spawn(move || {
            let _ = asked_tx.send(ask_service(&agent, conn, &run_request("win-pc-1"), None, &mut Listening(tx)));
        });
    }
    let mut heard = String::new();
    let running = hear(&lines, &mut heard, &format!("{SERVICE_PREFIX}{SERVICE_RUNNING_AS}"))
        .unwrap_or_else(|| panic!("the agent, still connected while the job ran, never heard the service's running as line; it heard {heard:?}"));
    assert!(
        hear(&lines, &mut heard, "Running job: build").is_some(),
        "the agent, still connected while the job ran, never heard the job's output; it heard {heard:?}"
    );
    let name = running.trim_start_matches(&format!("{SERVICE_PREFIX}{SERVICE_RUNNING_AS}")).trim().to_string();

    // The coordinator stopped the place: the agent leaves while the job runs.
    agent.cancel();
    let res = asked.recv_timeout(Duration::from_secs(30)).expect("the agent to end once it left");
    assert_eq!(res, Ok(()), "a job stopped by the agent leaving failed the agent");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !h.steps().join("\n").contains(&format!("verify {name}")) {
        assert!(
            Instant::now() < deadline,
            "the service did not stop and delete {name} after the agent left; it went through {:?}",
            steps_of(&h.steps(), &name)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(steps_of(&h.steps(), &name).join(" "), job_steps(), "{name} went through these steps after the agent left");

    // A job that ends on its own while the agent stays connected: the
    // service's last line ends the agent's request well, before the agent
    // would give up.
    h.state().block = false;
    let conn = dial_pipe(&path, &me).unwrap();
    let waiting = Ctx::background().with_timeout(Duration::from_secs(30));
    let mut all = Vec::new();
    let res = ask_service(&waiting, conn, &run_request("win-pc-2"), None, &mut all);
    let all = String::from_utf8_lossy(&all);
    assert!(!waiting.is_done(), "the agent heard no end of a job that ended while it stayed connected; it heard {all:?}");
    assert_eq!(res, Ok(()), "a job that ran to its end failed the agent:\n{all}");
    for want in [format!("{SERVICE_PREFIX}{SERVICE_RUNNING_AS}"), "Running job: build".into(), format!("{SERVICE_PREFIX}{SERVICE_SCRIPT_ENDED}0")] {
        assert!(all.contains(&want), "the agent did not hear {want:?} from a job that ran to its end; it heard {all:?}");
    }
}

/// What runner-up's service has granted the runner account on its pipe in
/// every release so far. A newer agent must fit it to reach a service an
/// older runner-up setup installed.
const RELEASED_GRANT: u32 = 0x12008b;

/// The access the service's pipe gives the runner account sid.
fn runner_grant(sid: &str) -> u32 {
    let sd = Descriptor::parse(&pipe_sddl(sid)).unwrap();
    let want = Sid::parse(sid).unwrap();
    let dacl = sd.dacl();
    for i in 0..unsafe { (*dacl).AceCount } as u32 {
        let mut ace = std::ptr::null_mut();
        check(unsafe { GetAce(dacl, i, &mut ace) }).unwrap();
        let ace = ace as *const ACCESS_ALLOWED_ACE;
        if sid_is(unsafe { &raw const (*ace).SidStart } as _, &want) {
            return unsafe { (*ace).Mask };
        }
    }
    panic!("the service's pipe gives the runner account nothing: {}", pipe_sddl(sid));
}

// Intent (a bug found on a real PC): the agent asked for more of the
// service's pipe than the service grants the runner account, so Windows
// refused every runner account that is not an administrator with "Access is
// denied", and no account place could take a job. An account holding only
// the runner account's grant reaches the service the way the agent does.
#[test]
fn an_account_granted_only_the_runners_rights_reaches_the_service() {
    let me = me();
    let grant = runner_grant(&me);
    assert_eq!(grant, RELEASED_GRANT, "the service grants the runner account this; released services grant {RELEASED_GRANT:#x}, which an agent must still fit");
    // A pipe like the service's, owned by this test's account and open to it
    // alone, with only the runner account's grant. With no SYSTEM or
    // Administrators entry, an administrator running the test holds no more
    // than a standard runner account does.
    let path = test_pipe_path();
    let sd = Descriptor::parse(&format!("O:{me}D:P(A;;{grant:#x};;;{me})")).unwrap();
    let sa = sd.attributes();
    let name = wide(&path);
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 << 10,
            64 << 10,
            0,
            &sa,
        )
    };
    assert_ne!(pipe, INVALID_HANDLE_VALUE, "{}", std::io::Error::last_os_error());
    let pipe = Handle(pipe);

    // What the agent asked for before, GENERIC_WRITE among it: Windows
    // refuses it here, so this pipe lets in no more than the grant.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if h != INVALID_HANDLE_VALUE {
        drop(Handle(h));
        panic!("the test's pipe let in more than the runner account's grant, so it proves nothing");
    }
    let err = std::io::Error::last_os_error();
    assert!(is_code(&err, ERROR_ACCESS_DENIED), "opening with more than the grant gave {err}, want access denied");

    let conn = dial_pipe(&path, &me).unwrap_or_else(|e| panic!("an account granted only the runner account's rights could not reach the service: {e}"));
    if let Err(e) = check(unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) }) {
        assert!(is_code(&e, ERROR_PIPE_CONNECTED), "{e}");
    }
    let dir = tempfile::tempdir().unwrap();
    let s = new_test_server(&new_fake_host(), dir.path());
    let (served_tx, served) = channel();
    std::thread::spawn(move || {
        handle(&Ctx::background(), pipe, &s, &Logger::discard());
        let _ = served_tx.send(());
    });
    let ctx = Ctx::background().with_timeout(Duration::from_secs(20));
    let mut out = Vec::new();
    let req = JobRequest { version: JOB_REQUEST_VERSION.into(), op: "probe".into(), ..Default::default() };
    let res = ask_service(&ctx, conn, &req, None, &mut out);
    let out = String::from_utf8_lossy(&out);
    assert_eq!(res, Ok(()), "the probe over the pipe failed:\n{out}");
    assert!(out.starts_with(SERVICE_READY), "the service answered {out:?}, want {SERVICE_READY:?}");
    assert!(served.recv_timeout(Duration::from_secs(10)).is_ok(), "the service kept the pipe after the agent closed it");
}

/// The environment block Windows gives a fresh sign-in of this account.
fn fresh_environment() -> Vec<(String, String)> {
    let mut token = std::ptr::null_mut();
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }).unwrap();
    let token = Handle(token);
    let block = win32::Environment::of(token.0).unwrap();
    let mut p = block.ptr() as *const u16;
    let mut env = Vec::new();
    loop {
        let kv = win32::from_wide_ptr(p);
        if kv.is_empty() {
            return env;
        }
        p = unsafe { p.add(kv.encode_utf16().count() + 1) };
        // Entries such as "=C:=C:\" name a drive's folder, not a variable.
        if let Some(i) = kv.char_indices().skip(1).find(|(_, c)| *c == '=').map(|(i, _)| i) {
            env.push((kv[..i].to_string(), kv[i + 1..].to_string()));
        }
    }
}

// Intent (README, "Windows machines" and "Pull requests from forks"; AGENTS.md,
// "Invariants"): an account place's start script unpacks the runner archive only
// when its bytes match the pinned sum, hands the runner the registration it
// read from standard input in its environment, and gives it the fork guard as
// its job-started hook unless the place allows forks. The script runs in
// Windows PowerShell on the command line the service starts it with, in a
// fresh environment block as the service gives it (not the test's own, which
// under PowerShell 7 points Windows PowerShell at the wrong modules); only
// the runner archive is a stand-in, whose run.cmd prints what it was given.
#[test]
fn account_job_start_script_runs_only_the_pinned_runner() {
    use std::os::windows::process::CommandExt;

    let powershell = format!(r"{}\System32\WindowsPowerShell\v1.0\powershell.exe", std::env::var("SystemRoot").unwrap());
    assert!(Path::new(&powershell).exists(), "no Windows PowerShell");
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("actions-runner-win-x64.zip");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zw.start_file("run.cmd", zip::write::SimpleFileOptions::default()).unwrap();
    zw.write_all(
        b"@echo off\r\n\
          echo registration=%ACTIONS_RUNNER_INPUT_JITCONFIG%\r\n\
          echo hook=%ACTIONS_RUNNER_HOOK_JOB_STARTED%\r\n\
          echo size=%RUNNER_UP_CORES% %RUNNER_UP_MEMORY_MB%\r\n\
          exit /b 7\r\n",
    )
    .unwrap();
    zw.finish().unwrap();
    let env = fresh_environment();
    let sum = crate::release::sha256(&std::fs::read(&archive).unwrap());
    const REG: &str = "stand-in-registration";

    // Writes the job's files for pin into a fresh folder and runs its start
    // script as the service does, with the registration on its input.
    let run = |pin: &str, allow_forks: bool| -> (tempfile::TempDir, String, i32) {
        let folder = tempfile::tempdir().unwrap();
        let req = JobRequest { allow_forks, cores: 2, memory_mb: 4096, ..Default::default() };
        for (name, body) in account_job_files(&archive.to_string_lossy(), pin, r"C:\Program Files\runner-up\runner-up.exe", &req) {
            std::fs::write(folder.path().join(name), body).unwrap();
        }
        let line = job_command_line(&powershell, &folder.path().to_string_lossy());
        let args = line.strip_prefix(&format!("\"{powershell}\" ")).unwrap();
        let (mut r, w) = std::io::pipe().unwrap();
        let mut cmd = std::process::Command::new(&powershell);
        cmd.raw_arg(args).env_clear().envs(env.iter().cloned()).stdin(std::process::Stdio::piped()).stdout(w.try_clone().unwrap()).stderr(w);
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        child.stdin.take().unwrap().write_all(format!("{REG}\n").as_bytes()).unwrap();
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            let _ = r.read_to_string(&mut out);
            out
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the start script ran past 120s");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        (folder, reader.join().unwrap(), status.code().unwrap_or(-1))
    };

    // An archive whose bytes are not the pinned ones is never unpacked.
    let (folder, out, code) = run(&"0".repeat(64), false);
    assert!(code != 0 && out.contains("does not match its pin") && !out.contains("registration="), "a runner archive off its pin gave exit {code}:\n{out}");
    assert!(!folder.path().join("runner").exists(), "a runner archive off its pin was unpacked");

    // The pinned archive runs, with the registration and the fork guard.
    let (_folder, out, code) = run(&sum, false);
    assert_eq!(code, 7, "the pinned runner gave this exit, want its own 7:\n{out}");
    let lines: HashMap<String, String> =
        out.replace('\r', "").lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    assert!(lines.get("registration").map(String::as_str) == Some(REG) && lines.get("size").map(String::as_str) == Some("2 4096"), "the runner got {lines:?}:\n{out}");
    let hook = lines.get("hook").cloned().unwrap_or_default();
    let base = Path::new(&hook).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    assert_eq!(base, GUARD_FILE.to_lowercase(), "the runner's job-started hook is {hook:?}, want the place's {GUARD_FILE}:\n{out}");
    assert!(Path::new(&hook).exists(), "the runner's job-started hook {hook:?} is not the place's");

    // A place that allows forks gives the runner no job-started hook (a batch
    // file expands an unset variable to nothing).
    let (_folder, out, code) = run(&sum, true);
    let no_hook = out.replace('\r', "").lines().any(|l| l == "hook=");
    assert!(code == 7 && out.contains(&format!("registration={REG}")) && no_hook, "a place that allows forks gave exit {code}:\n{out}");
}
