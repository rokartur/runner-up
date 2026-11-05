// The agent's tests, one file per Go test file they were ported from.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod bootwatch;
#[cfg(unix)]
mod container;
mod hyperv;
mod launch;
mod lock;
#[cfg(unix)]
mod macos;
#[cfg(unix)]
mod sizing;
#[cfg(unix)]
mod usage;
#[cfg(windows)]
mod service_update_windows;

/// A runner registration as GitHub hands one out, in shape.
pub const REGISTRATION: &str = "eyJydW5uZXIiOiJ7XCJBZ2VudElkXCI6MTIzfSIsIi5jcmVkZW50aWFscyI6InsuLi59In0=";

/// An event file recorded from GitHub.
pub fn event(name: &str) -> String {
    format!("{}/testdata/agent/events/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// python3, which Linux guest programs run with; None skips a test, as on
/// Windows, where the guest program's termios is missing.
pub fn python() -> Option<String> {
    if cfg!(windows) {
        eprintln!("skipped: the guest script needs a Unix python");
        return None;
    }
    let p = crate::proc::look_path("python3").ok();
    if p.is_none() {
        eprintln!("skipped: no python3");
    }
    Some(p?.to_string_lossy().into_owned())
}

/// A runner release archive holding only run.sh, and its SHA-256.
pub fn stand_in_runner(run_sh: &str) -> (Vec<u8>, String) {
    use sha2::Digest;
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tw = tar::Builder::new(gz);
    let mut h = tar::Header::new_ustar();
    h.set_mode(0o755);
    h.set_size(run_sh.len() as u64);
    tw.append_data(&mut h, "run.sh", run_sh.as_bytes()).unwrap();
    let b = tw.into_inner().unwrap().finish().unwrap();
    let sum = sha2::Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect();
    (b, sum)
}

/// Whether s holds a value left unfilled, such as @RUNNER_URL@.
pub fn has_placeholder(s: &str) -> bool {
    let parts: Vec<&str> = s.split('@').collect();
    parts.len() > 2
        && parts[1..parts.len() - 1].iter().any(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
}

/// Waits up to 10 seconds for path to appear.
pub fn wait_for(path: &Path) {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{} never appeared", path.display());
}

/// Two ends of a connection: one as the agent's Conn, the other the peer's.
pub fn tcp_pair() -> (crate::agent::Conn, std::net::TcpStream) {
    use std::net::{Shutdown, TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let agent = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    let closer = agent.try_clone().unwrap();
    let conn = crate::agent::Conn {
        reader: Box::new(agent.try_clone().unwrap()),
        writer: Box::new(agent),
        close: Arc::new(move || {
            let _ = closer.shutdown(Shutdown::Both);
        }),
    };
    (conn, peer)
}

/// cmd's exit code and its standard output and error as one, given input.
pub fn run(cmd: &mut Command, input: impl AsRef<[u8]>) -> (i32, String) {
    let (r, w) = std::io::pipe().unwrap();
    cmd.stdin(Stdio::piped()).stdout(w.try_clone().unwrap()).stderr(w);
    let mut child = cmd.spawn().unwrap();
    // The Command still holds the pipe's writing ends; reading would never end.
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let out = read_all(r);
    let code = feed_and_wait(&mut child, input);
    (code, out.join().unwrap())
}

/// cmd's exit code, standard output and standard error, given input.
pub fn run_apart(cmd: &mut Command, input: impl AsRef<[u8]>) -> (i32, String, String) {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let out = read_all(child.stdout.take().unwrap());
    let err = read_all(child.stderr.take().unwrap());
    let code = feed_and_wait(&mut child, input);
    (code, out.join().unwrap(), err.join().unwrap())
}

fn read_all(mut r: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = r.read_to_end(&mut b);
        String::from_utf8_lossy(&b).into_owned()
    })
}

/// Writes input to child and waits for it; a child still running after a
/// minute is killed and fails the test.
fn feed_and_wait(child: &mut Child, input: impl AsRef<[u8]>) -> i32 {
    let mut stdin = child.stdin.take().unwrap();
    let input = input.as_ref().to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let Some(status) = wait_timeout(child, Duration::from_secs(60)) else {
        let _ = child.kill();
        panic!("the command ran past a minute");
    };
    status.code().unwrap_or(-1)
}

/// Output a test reads back, shared with the Out writing it.
#[derive(Clone, Default)]
pub struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Child's exit, or None when it has not exited within limit.
pub fn wait_timeout(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}
