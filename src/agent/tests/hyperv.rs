// Intent (README, "Windows machines" and "Trade-offs"): cloud-init in the VM
// reads its seed from a disc runner-up writes; the runner's registration
// reaches the VM only over its serial port, never on the disc; the guest runs
// the runner pinned to its published sum; and the fork guard inside the VM,
// which cannot run runner-up's own Windows program, decides every event the way
// runner-up's fork-guard command does.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use base64::Engine;

use super::{Buf, REGISTRATION, event, python, run, run_apart, stand_in_runner, tcp_pair, has_placeholder};
use crate::agent::forkguard::fork_guard_say;
use crate::agent::hyperv::{GUEST_READY, HyperV, hyperv_seed, linux_guest, talk_to_guest};
use crate::agent::iso::seed_iso;
use crate::agent::launch::{RunnerLaunch, guest_launch};
use crate::agent::runner::{RUNNER_VERSION, runner_sha256};
use crate::agent::{AgentRun, Out, Provider};
use crate::ctx::Ctx;
use crate::testserver::{Response, Server};

const SEED_TIME: i64 = 1_793_230_000_000_000_000;

#[test]
fn seed_disc_reads_as_cloud_inits_no_cloud_seed() {
    let Some(bsdtar) = disc_reader() else { return };
    let seed = hyperv_seed("win-pc-1", "x64", &hyperv_place_launch(), SEED_TIME).unwrap();
    let iso = seed_iso(&seed).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seed.iso");
    std::fs::write(&path, &iso).unwrap();
    // libarchive reads the Joliet names, as Linux does.
    let (code, list, _) = run_apart(Command::new(&bsdtar).arg("-tf").arg(&path), "");
    assert_eq!(code, 0, "bsdtar -tf");
    let got: Vec<&str> = list.split_whitespace().filter(|f| *f != ".").collect();
    assert_eq!(got.join(" "), "meta-data user-data", "the disc's files");
    for (name, want) in &seed {
        let (code, got, _) = run_apart(Command::new(&bsdtar).arg("-xOf").arg(&path).arg(name), "");
        assert!(code == 0 && got.as_bytes() == want.as_slice(), "{name} reads back as {got:?} ({code})");
    }
    // cloud-init finds its seed by the volume label.
    let label = String::from_utf8_lossy(&iso[16 * 2048 + 40..16 * 2048 + 72]).trim().to_string();
    assert_eq!(label, "cidata", "the disc's label");
}

/// libarchive's tar, which reads an ISO disc. Windows, where runner-up writes
/// the seed disc, ships it as tar.exe rather than bsdtar, so looking only for
/// bsdtar skipped the test there; on Windows a missing reader fails the test
/// instead of skipping it.
fn disc_reader() -> Option<String> {
    if let Ok(p) = crate::proc::look_path("bsdtar") {
        return Some(p.to_string_lossy().into_owned());
    }
    // A GNU tar, such as Git for Windows', can come first on the PATH.
    let mut tars = vec!["tar".to_string()];
    if cfg!(windows) {
        tars.push(format!(r"{}\System32\tar.exe", std::env::var("SystemRoot").unwrap_or_default()));
    }
    for tar in tars {
        let Ok(p) = crate::proc::look_path(&tar) else { continue };
        let (_, out, _) = run_apart(Command::new(&p).arg("--version"), "");
        if out.starts_with("bsdtar") {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    assert!(!cfg!(windows), "no bsdtar or libarchive tar.exe to read the disc with");
    eprintln!("skipped: no bsdtar to read the disc with");
    None
}

// Intent (README, "Windows machines"): the seed's guest program
// runs GitHub's runner only when its download matches the published sum it is
// pinned to. The guest's own download check runs here against a stand-in
// download: bytes that do not match the pin are refused before anything is
// unpacked, and bytes that match are unpacked. (The seed holds no
// registration: hyperv_seed takes none, and the registration's own way in is
// registration_goes_to_the_vm_over_its_serial_port_once.)
#[test]
fn seed_pins_the_runner_and_the_guest_refuses_any_other() {
    let seed = hyperv_seed("win-pc-1", "x64", &hyperv_place_launch(), SEED_TIME).unwrap();
    let user = String::from_utf8(seed["user-data"].clone()).unwrap();
    assert!(user.starts_with("#cloud-config\n"), "user-data is not cloud-config:\n{user}");
    let content = user.lines().find_map(|l| l.strip_prefix("    content: ")).unwrap_or_else(|| panic!("user-data writes no guest program:\n{user}"));
    let guest = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(content).unwrap()).unwrap();
    for want in [
        format!(r#"RUNNER_URL = "https://github.com/actions/runner/releases/download/v{RUNNER_VERSION}/actions-runner-linux-x64-{RUNNER_VERSION}.tar.gz""#),
        format!(r#"RUNNER_SHA256 = "{}""#, runner_sha256("linux-x64").unwrap()),
        format!("LAUNCH = {}", guest_launch(&hyperv_place_launch())),
    ] {
        assert!(guest.contains(&want), "the guest program lacks {want}");
    }
    assert!(!has_placeholder(&guest), "the guest program has a value left unfilled");
    let meta = String::from_utf8_lossy(&seed["meta-data"]);
    assert!(meta.contains("instance-id: runner-up-win-pc-1-"), "meta-data is {meta:?}");

    // The guest's download check, run on the guest program the seed holds,
    // with only the download's address and the runner's home moved here.
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("guest.py");
    std::fs::write(&program, &guest).unwrap();
    let (tarball, sum) = stand_in_runner("#!/bin/sh\necho stand-in runner\n");
    let download = Server::start(move |_| Response::new(200, tarball.clone()));
    const DRIVER: &str = r#"import sys
g = {"__name__": "guest"}
exec(compile(open(sys.argv[1]).read(), sys.argv[1], "exec"), g)
g["RUNNER_URL"], g["HOME"] = sys.argv[2], sys.argv[3]
if len(sys.argv) > 4:
    g["RUNNER_SHA256"] = sys.argv[4]
try:
    g["prepare"]()
except Exception as e:
    print("failed:", e)
"#;
    let prepare = |home: &Path, pin: &[&str]| {
        std::fs::create_dir_all(home).unwrap();
        let (code, out) = run(Command::new(&py).args(["-I", "-c", DRIVER]).arg(&program).arg(&download.url).arg(home).args(pin), "");
        assert_eq!(code, 0, "the guest's download check did not run:\n{out}");
        out
    };
    let pinned = dir.path().join("pinned");
    let out = prepare(&pinned, &[]);
    assert!(out.contains("failed: the runner's download does not match its pin"), "the guest took a download its pin does not name: {out:?}");
    assert!(!pinned.join("actions-runner").exists(), "the guest unpacked a download its pin does not name");
    // The same check takes bytes that match: the refusal above is the pin's.
    let matching = dir.path().join("matching");
    let out = prepare(&matching, &[&sum]);
    assert!(
        !out.contains("does not match its pin") && matching.join("actions-runner").join("run.sh").exists(),
        "the guest did not unpack a download that matches its pin: {out:?}"
    );
}

/// The launch of a Hyper-V place of 4 cores and 8 GB that runs the fork guard.
fn hyperv_place_launch() -> RunnerLaunch {
    HyperV.launch(&AgentRun { cores: 4, memory_mb: 8192, ..Default::default() })
}

/// The guest program as the agent fills it in for a Hyper-V place that runs
/// the fork guard.
pub fn filled_guest() -> String {
    linux_guest("x64", &hyperv_place_launch()).unwrap()
}

/// The guest program's last line after a job its runner ran to the end;
/// container_guest_runs_the_pinned_runner_with_the_fork_guard owns its words.
const GUEST_ENDED: &str = "runner-up guest: the runner ended with 0\n";

/// Plays the VM's side of the serial port: it says it is ready until it reads
/// the registration, then prints the runner's output and turns off.
fn fake_guest(vm: TcpStream, got: Sender<String>) {
    let (tx, lines) = channel();
    let mut r = BufReader::new(vm.try_clone().unwrap());
    std::thread::spawn(move || {
        let mut l = String::new();
        if r.read_line(&mut l).is_ok_and(|_| l.ends_with('\n')) {
            let _ = tx.send(l.trim_end().to_string());
        }
    });
    std::thread::spawn(move || {
        let mut vm = vm;
        loop {
            let _ = writeln!(vm, "{GUEST_READY}");
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(l) => {
                    let _ = got.send(l);
                    // One more ready line was already on its way.
                    let _ = writeln!(vm, "{GUEST_READY}");
                    let _ = write!(vm, "runner-up guest: starting the runner\n√ Connected to GitHub\nRunning job: build\n{GUEST_ENDED}");
                    let _ = vm.shutdown(Shutdown::Both);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

#[test]
fn registration_goes_to_the_vm_over_its_serial_port_once() {
    let (agent, vm) = tcp_pair();
    let (got_tx, got) = channel();
    fake_guest(vm, got_tx);
    let buf = Buf::default();
    let out = Out::new(Box::new(buf.clone()));
    let res = talk_to_guest(&Ctx::background(), agent, REGISTRATION, &out, Duration::from_secs(5));
    let text = buf.text();
    assert_eq!(res, Ok(()), "a job that ran to its end failed:\n{text}");
    let l = got.recv_timeout(Duration::from_secs(5)).expect("the VM to read a line");
    assert_eq!(l, REGISTRATION, "the VM read this, want the registration");
    assert!(!text.contains(GUEST_READY) && !text.contains(REGISTRATION), "the agent's output holds the handshake or the registration:\n{text}");
    assert!(text.contains(&format!("Running job: build\n{GUEST_ENDED}")), "the runner's output did not reach the agent:\n{text}");
}

#[test]
fn a_vm_that_never_answers_fails_at_the_boot_timeout() {
    let (agent, mut vm) = tcp_pair();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut vm, &mut std::io::sink());
    });
    let start = Instant::now();
    let res = talk_to_guest(&Ctx::background(), agent, REGISTRATION, &Out::new(Box::new(std::io::sink())), Duration::from_millis(200));
    assert!(
        res.as_ref().is_err_and(|e| e.contains("did not start runner-up's guest program within 200ms")),
        "a silent VM gave {res:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(5), "the agent waited {:?} for a silent VM", start.elapsed());
}

#[test]
fn stopping_the_place_lets_go_of_the_vm() {
    let (agent, vm) = tcp_pair();
    // The VM says it is ready and reads the registration: the job is under
    // way when the place stops.
    let (sent_tx, sent) = channel();
    std::thread::spawn(move || {
        let mut w = vm.try_clone().unwrap();
        let _ = writeln!(w, "{GUEST_READY}");
        let mut r = BufReader::new(vm);
        let mut l = String::new();
        let _ = r.read_line(&mut l);
        let _ = sent_tx.send(l);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
    });
    let ctx = Ctx::background().with_cancel();
    let (done_tx, done) = channel();
    let c = ctx.clone();
    std::thread::spawn(move || {
        let _ = done_tx.send(talk_to_guest(&c, agent, REGISTRATION, &Out::new(Box::new(std::io::sink())), Duration::from_secs(5)));
    });
    let l = sent.recv_timeout(Duration::from_secs(5)).expect("the agent never sent the VM its registration");
    assert_eq!(l, format!("{REGISTRATION}\n"), "the VM read this, want the registration");
    ctx.cancel();
    let res = done.recv_timeout(Duration::from_secs(5)).expect("the agent kept the VM's serial port after the place stopped");
    assert_eq!(res, Ok(()), "a stopped place gave an error");
}

#[test]
fn guest_fork_guard_decides_like_runner_ups() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let guest = dir.path().join("guest.py");
    std::fs::write(&guest, filled_guest()).unwrap();
    let mut events: Vec<String> = std::fs::read_dir(Path::new(&event("")))
        .unwrap()
        .map(|e| e.unwrap().path().to_string_lossy().into_owned())
        .collect();
    assert!(events.len() >= 5, "events {events:?}");
    let extra = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("nan.json", r#"{"pull_request": NaN}"#),
        ("float-ids.json", r#"{"pull_request": {"head": {"repo": {"id": 1.0}}, "base": {"repo": {"id": 1.0}}}}"#),
        ("empty.json", ""),
        ("pr-null.json", r#"{"pull_request": null, "workflow_run": null}"#),
        ("pr-string.json", r#"{"pull_request": "x"}"#),
    ] {
        let p = extra.path().join(name);
        std::fs::write(&p, body).unwrap();
        events.push(p.to_string_lossy().into_owned());
    }
    events.push(extra.path().join("missing.json").to_string_lossy().into_owned());
    for ev in &events {
        let (mut ours, mut ours_err) = (Vec::new(), Vec::new());
        let code = fork_guard_say(ev, "pull_request", &mut ours, &mut ours_err);
        let (ours, ours_err) = (String::from_utf8(ours).unwrap(), String::from_utf8(ours_err).unwrap());
        let (py_code, py_out, py_err) = run_apart(
            Command::new(&py).arg("-I").arg(&guest).arg("guard").env("GITHUB_EVENT_PATH", ev).env("GITHUB_EVENT_NAME", "pull_request"),
            "",
        );
        assert_eq!(
            (py_code, &py_out, &py_err),
            (code, &ours, &ours_err),
            "{}: the guest's guard, then runner-up's",
            Path::new(ev).file_name().unwrap().to_string_lossy()
        );
    }
}

// The guest's half of the handshake, on a pseudo-terminal as on the VM's
// serial port: it says it is ready until a line arrives, and takes that line.
#[test]
fn guest_waits_for_the_registration_on_its_tcp_pair() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let guest = dir.path().join("guest.py");
    std::fs::write(&guest, filled_guest()).unwrap();
    let script = r#"
import importlib.util, os, sys, threading, time, tty
spec = importlib.util.spec_from_file_location("guest", sys.argv[1])
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
agent, port = os.openpty()
tty.setraw(port); tty.setraw(agent)
def host():
    seen = b""
    while g.READY not in seen:
        seen += os.read(agent, 4096)
    os.write(agent, sys.argv[2].encode() + b"\n")
threading.Thread(target=host, daemon=True).start()
print(g.registration(port))
"#;
    let (code, out) = run(Command::new(&py).args(["-I", "-c", script]).arg(&guest).arg(REGISTRATION), "");
    assert!(code == 0 && out.trim() == REGISTRATION, "the guest took {out:?} ({code}), want the registration");
}

// Intent (a bug moving a fleet to runner-up found: Linux jobs in Hyper-V VMs
// must find tools such as Node in the job image's tool cache, as on GitHub's
// Ubuntu runners, whose images set RUNNER_TOOL_CACHE in /etc/environment). The
// runner gets what the image's /etc/environment sets, but never a
// registration, hook or identity from it.
#[test]
fn guest_runner_gets_the_images_environment() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let guest = dir.path().join("guest.py");
    std::fs::write(&guest, filled_guest()).unwrap();
    // Ubuntu 24.04's own line, then a job image's.
    let etc = dir.path().join("environment");
    std::fs::write(
        &etc,
        "PATH=\"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin\"\n# set by the job image\nRUNNER_TOOL_CACHE=/opt/hostedtoolcache\nACTIONS_RUNNER_INPUT_JITCONFIG=from-the-image\nHOME=/root\n",
    )
    .unwrap();
    let script = r#"
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("guest", sys.argv[1])
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
print(json.dumps(g.runner_env(sys.argv[3], g.image_environment(sys.argv[2])), sort_keys=True))
"#;
    let runner_env = |etc: &Path| {
        let (code, out) = run(Command::new(&py).args(["-I", "-c", script]).arg(&guest).arg(etc).arg(REGISTRATION), "");
        assert_eq!(code, 0, "{out}");
        serde_json::from_str::<HashMap<String, String>>(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    };
    let env = runner_env(&etc);
    for (k, want) in [
        ("RUNNER_TOOL_CACHE", "/opt/hostedtoolcache"),
        ("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin"),
        ("ACTIONS_RUNNER_INPUT_JITCONFIG", REGISTRATION),
        ("ACTIONS_RUNNER_HOOK_JOB_STARTED", "/opt/runner-up/fork-guard.sh"),
        ("HOME", "/home/runner"),
        ("USER", "runner"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(want), "the runner gets {k}");
    }
    // An image without the file still runs the runner.
    let env = runner_env(&dir.path().join("missing"));
    assert!(
        env.get("ACTIONS_RUNNER_INPUT_JITCONFIG").map(String::as_str) == Some(REGISTRATION)
            && env.get("PATH").is_some_and(|p| !p.is_empty())
            && env.get("RUNNER_TOOL_CACHE").is_none_or(|p| p.is_empty()),
        "with no /etc/environment the runner gets {env:?}"
    );
}

// Intent (a bug a live Hyper-V check found): a VM job that runs to its end
// failed the place with "No process is on the other end of the pipe" once the
// VM turned off and Hyper-V closed its serial port's pipe. That end is the
// job's normal end.
#[cfg(windows)]
#[test]
fn a_vm_turning_off_ends_its_job_normally() {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    // The VM's serial port as Windows reads it: the guest's lines, then the
    // error a read gives once Hyper-V has closed the pipe.
    struct EndedPipe {
        r: std::io::Cursor<String>,
        closed: i32,
    }
    impl Read for EndedPipe {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            match self.r.read(b)? {
                0 if !b.is_empty() => Err(std::io::Error::from_raw_os_error(self.closed)),
                n => Ok(n),
            }
        }
    }
    struct Sent(Arc<Mutex<Vec<u8>>>);
    impl Write for Sent {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // ERROR_PIPE_NOT_CONNECTED, ERROR_BROKEN_PIPE.
    for closed in [233, 109] {
        let lines = format!("{GUEST_READY}\nrunner-up guest: starting the runner\n{GUEST_ENDED}");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let conn = crate::agent::Conn {
            reader: Box::new(EndedPipe { r: std::io::Cursor::new(lines), closed }),
            writer: Box::new(Sent(sent.clone())),
            close: Arc::new(|| {}),
        };
        let buf = Buf::default();
        let res = talk_to_guest(&Ctx::background(), conn, REGISTRATION, &Out::new(Box::new(buf.clone())), Duration::from_secs(5));
        assert_eq!(res, Ok(()), "a VM that turned off after its job failed the place");
        let sent = String::from_utf8_lossy(&sent.lock().unwrap()).into_owned();
        assert!(buf.text().contains(GUEST_ENDED.trim_end()) && sent == format!("{REGISTRATION}\n"), "output {:?}, sent {sent:?}", buf.text());
    }
}
