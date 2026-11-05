//! Account places run each job in a fresh Windows account that runner-up's
//! service makes and deletes; the agent only asks the service over its pipe.
//! On other machines they, and the service, refuse.

use std::io::{Read, Write};
use std::sync::Arc;

use super::jobhost::{GUARD_FILE, ServiceUpdate, account_guard_script};
use super::launch::RunnerLaunch;
use super::{AgentRun, Out, Provider};
use crate::ctx::Ctx;

pub struct Account;

pub fn launch(a: &AgentRun) -> RunnerLaunch {
    a.launch_with(&format!("$dir\\{GUARD_FILE}"), &account_guard_script(&a.self_path))
}

#[cfg(not(windows))]
const WINDOWS_ONLY: &str = "runs only on Windows";

#[cfg(not(windows))]
impl Provider for Account {
    fn run(&self, _: &Ctx, _: &AgentRun, _: &str, _: &Arc<Out>) -> Result<(), String> {
        Err(WINDOWS_ONLY.into())
    }
    fn clean(&self, _: &Ctx, _: &AgentRun) -> Result<(), String> {
        Ok(())
    }
    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        launch(a)
    }
    fn ready(&self) -> String {
        WINDOWS_ONLY.into()
    }
}

#[cfg(not(windows))]
pub fn service_version(_: &Ctx) -> Result<String, String> {
    Err(WINDOWS_ONLY.into())
}

#[cfg(not(windows))]
pub fn update_service(_: &Ctx, _: &ServiceUpdate, _: &mut dyn Read, _: &mut dyn Write) -> Result<(), String> {
    Err(WINDOWS_ONLY.into())
}

#[cfg(not(windows))]
pub fn run_service() -> Result<(), String> {
    Err(format!("runner-up's service {WINDOWS_ONLY}"))
}

/// The machine's memory from /proc/meminfo, or a Mac's from sysctl, or 0
/// where neither answers.
#[cfg(not(windows))]
pub fn memory_mb() -> i64 {
    let Ok(b) = std::fs::read_to_string("/proc/meminfo") else {
        let Ok(out) = std::process::Command::new("/usr/sbin/sysctl").args(["-n", "hw.memsize"]).output() else {
            return 0;
        };
        if !out.status.success() {
            return 0;
        }
        let n: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        return n / (1024 * 1024);
    };
    for line in b.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 2 && f[0] == "MemTotal:" {
            return f[1].parse::<i64>().unwrap_or(0) / 1024;
        }
    }
    0
}

/// Keeps the job account record to its owner.
#[cfg(not(windows))]
#[allow(dead_code)] // for jobhost's Server, which only tests run off Windows
pub fn protect_file(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())
}

#[cfg(windows)]
pub use self::windows::*;

#[cfg(windows)]
mod windows {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_WRITE_DATA};

    use super::*;
    use crate::agent::jobhost::{JOB_REQUEST_VERSION, JobRequest, ask_service, ready_version};
    use crate::agent::{Conn, winpipe};

    pub const PIPE_NAME: &str = r"\\.\pipe\runner-up";
    /// What the runner account may do with the pipe, and all the agent asks
    /// for when it opens it: read and write data, and read its owner
    /// (READ_CONTROL, in FILE_GENERIC_READ). Windows refuses an open that asks
    /// for more. A client cannot make a pipe instance of its own
    /// (FILE_CREATE_PIPE_INSTANCE is FILE_APPEND_DATA).
    pub const CLIENT_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA;

    fn request(op: &str) -> JobRequest {
        JobRequest { version: JOB_REQUEST_VERSION.into(), op: op.into(), ..Default::default() }
    }

    impl Provider for Account {
        fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
            let conn = dial_service()?;
            let req = JobRequest {
                place: a.place.clone(),
                cores: a.cores,
                memory_mb: a.memory_mb,
                allow_forks: a.allow_forks,
                registration: jit.into(),
                ..request("run")
            };
            ask_service(ctx, conn, &req, None, &mut OutWriter(out))
        }

        fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String> {
            let conn = dial_service()?;
            ask_service(ctx, conn, &JobRequest { place: a.place.clone(), ..request("clean") }, None, &mut std::io::sink())
        }

        fn launch(&self, a: &AgentRun) -> RunnerLaunch {
            launch(a)
        }

        /// "ok" when runner-up's service answers here.
        fn ready(&self) -> String {
            let conn = match dial_service() {
                Ok(c) => c,
                Err(e) => return e,
            };
            let ctx = Ctx::detached_timeout(Duration::from_secs(20));
            match ask_service(&ctx, conn, &request("probe"), None, &mut std::io::sink()) {
                Ok(()) => "ok".into(),
                Err(e) => e,
            }
        }
    }

    struct OutWriter<'a>(&'a Arc<Out>);

    impl Write for OutWriter<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.write(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Asks the service here its version.
    pub fn service_version(ctx: &Ctx) -> Result<String, String> {
        let conn = dial_service()?;
        let mut answer = Vec::new();
        ask_service(ctx, conn, &request("probe"), None, &mut answer)?;
        ready_version(&String::from_utf8_lossy(&answer))
    }

    /// Hands the service here a release.
    pub fn update_service(ctx: &Ctx, u: &ServiceUpdate, archive: &mut dyn Read, out: &mut dyn Write) -> Result<(), String> {
        let conn = dial_service()?;
        let req = JobRequest {
            archive: u.archive.clone(),
            size: u.size,
            checksums: u.checksums.clone(),
            signature: u.signature.clone(),
            ..request("update")
        };
        ask_service(ctx, conn, &req, Some(archive), out)
    }

    /// Opens the service's pipe and checks SYSTEM owns it: any account may
    /// make a pipe of the same name while the service is stopped, to take the
    /// runner's registration.
    pub fn dial_service() -> Result<Conn, String> {
        dial_pipe(PIPE_NAME, "S-1-5-18")
    }

    /// Opens the service's pipe at path, waiting up to a minute for a free
    /// instance, asking only for what the service grants the runner account,
    /// and checks owner owns it.
    pub fn dial_pipe(path: &str, owner: &str) -> Result<Conn, String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let h = loop {
            let err = match winpipe::open(path, CLIENT_ACCESS) {
                Ok(h) => break h,
                Err(e) => e,
            };
            let code = err.raw_os_error().unwrap_or(0) as u32;
            if code == ERROR_FILE_NOT_FOUND {
                return Err("runner-up's service is not running here; run runner-up setup".into());
            }
            if code != ERROR_PIPE_BUSY || Instant::now() > deadline {
                return Err(format!("could not reach runner-up's service: {err}"));
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let owned = winpipe::owner(h)
            .and_then(|s| if s == owner { Ok(()) } else { Err("the pipe named runner-up is not the service's own".to_string()) });
        if let Err(e) = owned {
            winpipe::close(h);
            return Err(e);
        }
        winpipe::conn(h).map_err(|e| e.to_string())
    }

    /// The machine's memory.
    pub fn memory_mb() -> i64 {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
            return 0;
        }
        (m.ullTotalPhys >> 20) as i64
    }

    /// Whether this process runs elevated, as an administrator.
    pub fn elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut n = 0;
        let ok = unsafe {
            GetTokenInformation(token, TokenElevation, &mut e as *mut _ as _, std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut n)
        };
        unsafe { CloseHandle(token) };
        ok != 0 && e.TokenIsElevated != 0
    }

    pub use crate::agent::service::run_service;
    pub use crate::agent::winhost::protect_file;
}
