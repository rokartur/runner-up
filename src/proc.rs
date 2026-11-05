//! Starts, detaches and ends processes. The coordinator and its place
//! runners run on Linux or macOS; on Windows runner-up is only an agent, and
//! these refuse there.

use std::process::Command;

pub const COORDINATOR_OS: &str =
    "the coordinator runs on Linux or macOS; on Windows runner-up runs as a machine's agent (see runner-up setup)";

/// Replaces this process with the program at path, run with argv and env:
/// the same process, so whatever supervises it (launchd, systemd) keeps
/// supervising the program it becomes. Returns only on failure.
#[cfg(unix)]
pub fn exec(path: &str, argv: &[String], env: &[(String, String)]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(path);
    cmd.args(&argv[1..]).arg0(&argv[0]).env_clear().envs(env.iter().cloned());
    cmd.exec()
}

#[cfg(windows)]
pub fn exec(_: &str, _: &[String], _: &[(String, String)]) -> std::io::Error {
    std::io::Error::other(COORDINATOR_OS)
}

/// Starts cmd in its own session, so it outlives the coordinator.
#[cfg(unix)]
pub fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
pub fn detach(_: &mut Command) {}

/// Hands f to the process cmd starts as its file descriptor 3.
#[cfg(unix)]
pub fn pass_as_fd3(cmd: &mut Command, f: &std::fs::File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fd = f.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            // dup2 onto itself keeps close-on-exec set, so clear it by hand.
            let ok = if fd == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(fd, 3) };
            if ok < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
pub fn pass_as_fd3(_: &mut Command, _: &std::fs::File) {}

/// Takes over file descriptor fd, handed over by the parent, keeping it out
/// of every process started from here.
#[cfg(unix)]
pub fn take_fd(fd: i32) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return None;
    }
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(windows)]
pub fn take_fd(_: i32) -> Option<std::fs::File> {
    None
}

/// Asks process pid to stop.
#[cfg(unix)]
pub fn terminate(pid: u32) -> std::io::Result<()> {
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
pub fn terminate(_: u32) -> std::io::Result<()> {
    Err(std::io::Error::other(COORDINATOR_OS))
}

/// Ok where the coordinator can run.
pub fn coordinator_runs() -> Result<(), &'static str> {
    if cfg!(windows) { Err(COORDINATOR_OS) } else { Ok(()) }
}

/// Runs cmd to its end with stdout and stderr in one stream, as Go's
/// CombinedOutput; it is killed once ctx is done, as exec.CommandContext.
pub fn combined_output(ctx: &crate::ctx::Ctx, cmd: &mut Command) -> (Vec<u8>, Result<(), String>) {
    combined_output_with(ctx, cmd, Vec::new())
}

/// combined_output with input on stdin.
pub fn combined_output_with(ctx: &crate::ctx::Ctx, cmd: &mut Command, input: Vec<u8>) -> (Vec<u8>, Result<(), String>) {
    use std::io::{Read, Write};
    let (mut r, w) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Err(e.to_string())),
    };
    let w2 = match w.try_clone() {
        Ok(w2) => w2,
        Err(e) => return (Vec::new(), Err(e.to_string())),
    };
    cmd.stdin(std::process::Stdio::piped()).stdout(w).stderr(w2);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Vec::new(), Err(e.to_string())),
    };
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    // The Command still holds the pipe's writing ends; reading would never end.
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = r.read_to_end(&mut out);
        out
    });
    let status = wait_ctx(ctx, &mut child);
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    (out, status)
}

/// Waits for child, killing it once ctx is done.
pub fn wait_ctx(ctx: &crate::ctx::Ctx, child: &mut std::process::Child) -> Result<(), String> {
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(s.to_string()),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if ctx.is_done() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ctx.err().unwrap_or("signal: killed").to_string());
        }
        ctx.sleep(std::time::Duration::from_millis(20));
    }
}

/// This machine's name, as Go's os.Hostname; empty when unknown.
#[cfg(unix)]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for its whole length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(windows)]
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// What output gathered from a command.
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Result<(), String>,
}

/// Runs cmd to its end with input on stdin, keeping stdout and stderr
/// apart, as Go's Output; it is killed once ctx is done.
pub fn output(ctx: &crate::ctx::Ctx, cmd: &mut Command, input: Vec<u8>) -> Output {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let failed = |e: std::io::Error| Output { stdout: Vec::new(), stderr: Vec::new(), status: Err(e.to_string()) };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let read_all = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            b
        })
    };
    let out = read_all(Box::new(child.stdout.take().unwrap()));
    let err = read_all(Box::new(child.stderr.take().unwrap()));
    let status = wait_ctx(ctx, &mut child);
    let _ = writer.join();
    Output { stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default(), status }
}

/// Finds program as a shell would, on the caller's PATH, as Go's LookPath.
#[cfg(unix)]
pub fn look_path(program: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Path, PathBuf};
    let executable =
        |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0);
    if program.contains('/') {
        let p = PathBuf::from(program);
        return if executable(&p) { Ok(p) } else { Err(format!("exec: {}: not found", crate::go::quote(program))) };
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let p = Path::new(if dir.is_empty() { "." } else { dir }).join(program);
        if executable(&p) {
            return Ok(p);
        }
    }
    Err(format!("exec: {}: executable file not found in $PATH", crate::go::quote(program)))
}

/// Finds program on PATH with one of PATHEXT's extensions, as Go's LookPath.
#[cfg(windows)]
pub fn look_path(program: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Path;
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or(".com;.exe;.bat;.cmd".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_lowercase())
        .collect();
    let find = |base: &Path| {
        let lower = base.to_string_lossy().to_lowercase();
        if exts.iter().any(|e| lower.ends_with(e.as_str())) && base.is_file() {
            return Some(base.to_path_buf());
        }
        exts.iter().map(|e| std::path::PathBuf::from(format!("{}{e}", base.display()))).find(|p| p.is_file())
    };
    if program.contains(['\\', '/', ':']) {
        return find(Path::new(program)).ok_or_else(|| format!("exec: {}: file does not exist", crate::go::quote(program)));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(';').filter(|d| !d.is_empty()) {
        if let Some(p) = find(&Path::new(dir).join(program)) {
            return Ok(p);
        }
    }
    Err(format!("exec: {}: executable file not found in %PATH%", crate::go::quote(program)))
}
