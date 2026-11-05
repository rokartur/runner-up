use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;
use crate::coordinator::update::agent_copy;
use crate::{quote, release};

/// The long name of path, as Windows gives it to the account.
fn long_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut b = vec![0u16; 32768];
    // SAFETY: wide is NUL-terminated and b holds len(b) elements.
    let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetLongPathNameW(wide.as_ptr(), b.as_mut_ptr(), b.len() as u32) };
    assert!(n > 0 && (n as usize) < b.len(), "GetLongPathNameW: {}", std::io::Error::last_os_error());
    PathBuf::from(std::ffi::OsString::from_wide(&b[..n as usize]))
}

// Intent (README, "Updating runner-up"): the coordinator sends a release's
// program to a Windows machine on standard input of the line it hands the
// machine's SSH server, which runs it with cmd.exe. The machine keeps the
// program byte for byte in the account's own folder for that version and prints
// its path, a path the coordinator can run over SSH; a copy whose SHA-256 is
// not the release's is refused, and the program already there stays.
#[test]
fn a_windows_machine_keeps_the_agent_sent_on_its_input() {
    // This test program: megabytes of a real Windows program.
    let prog = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    // The account's folder, by its long name: the runner's temporary folder
    // may be named in 8.3 form.
    let dir = tempfile::tempdir().unwrap();
    let local = long_path(dir.path());
    let m = config::Machine { name: "pc".into(), os: "windows".into(), ssh: strings(&["runner@pc"]), ..Default::default() };
    let send = |prog: &[u8], sum: &str| -> Result<String, String> {
        let (line, _) = agent_copy(&m, "v9.0.2", sum);
        let cmd_exe = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        // As Windows' OpenSSH server starts a command: cmd.exe /c and the line.
        let mut child = Command::new(cmd_exe)
            .raw_arg("/c")
            .raw_arg(&line)
            .env("LOCALAPPDATA", &local)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = prog.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = wait_with_deadline(child);
        writer.join().unwrap();
        if !out.status.success() {
            return Err(format!("{}: {}", out.status, String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let path = send(&prog, &release::sha256(&prog)).unwrap_or_else(|e| panic!("the copy failed: {e}"));
    let want = local.join("runner-up").join("v9.0.2").join("runner-up.exe");
    assert_eq!(Path::new(&path), want, "the machine printed the wrong path");
    if let Err(e) = quote::check_win_word(&path, true) {
        panic!("the coordinator cannot run the copy over SSH: {e}");
    }
    assert!(std::fs::read(&path).unwrap() == prog, "the copy does not hold the bytes sent");

    let mut other = prog.clone();
    other.push(0);
    let refused = send(&other, &release::sha256(&prog));
    assert!(refused.as_ref().is_err_and(|e| e.contains("not the release's")), "a copy with another SHA-256 was not refused: {refused:?}");
    assert!(std::fs::read(&path).unwrap() == prog, "a refused copy changed the program already there");
}

/// Waits for the child with a deadline, so a hung cmd.exe fails the test with
/// its cause rather than hanging it.
fn wait_with_deadline(child: std::process::Child) -> std::process::Output {
    let id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(out) => out.unwrap(),
        Err(_) => {
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &id.to_string()]).status();
            panic!("cmd.exe did not finish in 120s");
        }
    }
}
