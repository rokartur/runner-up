use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::Duration;

use super::e2e::E2e;
use super::journeys::skip_if_short;
use crate::config::{self, PROVIDER_TART};
use crate::coordinator::BusyReason;
use crate::lock;

// Intent (README, "Borrowing a virtual Mac"): the real borrow CLI waits beside
// jobs, passes the place and its size to the command, preserves stdio and exit
// status, survives a coordinator restart, and cleans the named VM before
// releasing the place. GitHub and the machine agent are isolated stand-ins; no
// real VM is started.
#[test]
fn borrow_cli_waits_runs_and_survives_restart() {
    skip_if_short!();
    let mut e = borrow_e2e();
    let busy = lock::busy(&busy_lock(&e)).unwrap().unwrap();
    e.serve();
    let command = r#"read -r input
printf 'place=%s vm=%s cpus=%s memory=%s input=%s\n' "$BORROWED_PLACE" "$BORROWED_VM" "$BORROWED_CPUS" "$BORROWED_MEMORY_MB" "$input"
echo live > "$E2E_DIR/vm-live"
while [ ! -e "$E2E_DIR/borrow-done" ]; do sleep 0.05; done
exit 23"#;
    let mut b = Borrow::start(&e, command);
    e.eventually("borrow reports its queue position", || b.said().contains("position 1"));
    drop(busy);
    e.eventually("command starts in borrowed place", || e.dir.join("vm-live").exists());
    let run = e.run_in("box-1").unwrap_or_default();
    assert!(
        is_borrow(&e) && run.job_name == "native check",
        "box-1 is held for {:?} by {:?}, want the borrow \"native check\"\n{}",
        e.held_by("box-1"),
        run.job_name,
        e.status()
    );
    let got = e.stop();
    assert_eq!(got, 0, "stop={got}");
    assert!(held(&e), "restart lost borrow lock");
    e.serve();
    e.eventually("new service sees borrow", || is_borrow(&e));
    // The command ends; while the VM's clean runs, the place stays held, so
    // no job or borrow can take it before the VM is gone.
    std::fs::write(e.dir.join("hold-clean"), "").unwrap();
    b.clean_go = Some(e.dir.join("clean-go"));
    std::fs::write(e.dir.join("borrow-done"), "").unwrap();
    e.eventually("the borrowed VM's clean starts", || e.dir.join("cleaning").exists());
    assert!(held(&e), "the borrow released its place before its VM was cleaned");
    std::fs::write(e.dir.join("clean-go"), "").unwrap();
    let code = b.wait();
    assert_eq!(code, 23, "exit={code} output={}", b.said());
    e.eventually("borrow frees after cleanup", || !held(&e));
    assert!(!e.dir.join("vm-live").exists(), "leftover VM was not removed");
    let transcript = b.said();
    assert!(transcript.contains("place=box-1 vm=runner-up-box-1 cpus=2 memory=4096 input=hello"), "stdio/environment: {transcript}");
}

// Intent (README, "Borrowing a virtual Mac"): stopping a task ends its borrow
// and deletes its named VM; cancelling a queued task must never launch its
// command later.
#[test]
fn borrow_cli_cancellation() {
    skip_if_short!();
    for queued in [true, false] {
        let mut e = borrow_e2e();
        let busy = if queued { lock::busy(&busy_lock(&e)).unwrap() } else { None };
        e.serve();
        let mut b = Borrow::start(&e, r#"echo live > "$E2E_DIR/vm-live"; exec sleep 60"#);
        e.eventually("borrow reaches expected state", || {
            if queued { b.said().contains("position 1") } else { e.dir.join("vm-live").exists() }
        });
        b.term();
        let code = b.wait();
        assert_eq!(code, 130, "queued={queued}: exit={code} output={}", b.said());
        drop(busy);
        e.eventually("place is free after cancellation", || !held(&e));
        assert!(!e.dir.join("vm-live").exists(), "queued={queued}: cancelled borrow left a VM");
        // Queue directories are collected on the next scheduling pass, after
        // the caller releases its live lock. Once collected, no later pass can
        // grant the request; until then the place was free for it. A grant
        // starts a borrow supervisor, which keeps a log: the queued request,
        // cancelled before any grant, has none.
        e.eventually("coordinator collects the abandoned request", || {
            std::fs::read_dir(PathBuf::from(&e.cfg.state_dir).join("borrows")).is_ok_and(|d| d.count() == 0)
        });
        let grants: Vec<_> = std::fs::read_dir(e.cfg.logs_dir())
            .unwrap()
            .filter_map(|d| d.ok())
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("borrow-") && n.ends_with(".log"))
            .collect();
        let want = if queued { 0 } else { 1 };
        assert_eq!(grants.len(), want, "queued={queued}: the coordinator granted the borrow {grants:?}\n{}", e.logs());
    }
}

// Intent (README, "Borrowing a virtual Mac"): signal termination retains its
// shell-compatible status through the real supervisor, socket protocol and
// caller, and releases the borrowed place.
#[test]
fn borrow_cli_command_signal_status() {
    skip_if_short!();
    let mut e = borrow_e2e();
    e.serve();
    let mut b = Borrow::start(&e, "printf 'terminating borrowed command with SIGTERM\\n'; kill -TERM $$");
    let code = b.wait();
    assert_eq!(code, 128 + libc::SIGTERM, "status={code} output={}", b.said());
    e.eventually("signal-terminated borrow frees its place", || !held(&e));
}

// Intent (README, "Borrowing a virtual Mac"): borrowing requires an explicit
// label, declared positive minutes and a tart pool, rather than silently taking
// a non-VM place. Every case reads a valid setup file, so each refusal is its
// own and no request reaches the coordinator's queue.
#[test]
fn borrow_cli_rejects_invalid_inputs() {
    let e = E2e::new(); // Its pool has only a Docker place.
    let usage = "borrow needs --pool LABEL --label TEXT --minutes N -- COMMAND";
    for (args, why) in [
        (vec!["--pool", "ru-linux", "--label", "check", "--minutes", "0", "--", "true"], usage),
        (vec!["--pool", "ru-linux", "--minutes", "20", "--", "true"], usage),
        (vec!["--pool", "ru-linux", "--label", "check", "--minutes", "20"], usage),
        (vec!["--pool", "ru-linux", "--label", "check", "--minutes", "20", "--", "true"], "pool has no tart place"),
    ] {
        let mut all = vec!["borrow", "--config", e.config.as_str()];
        all.extend(&args);
        let (code, out) = crate::agent::tests::run(&mut e.command(&all), "");
        assert!(code == 2 && out.trim() == why, "borrow {args:?}: exit {code} {out:?}, want exit 2 and {why:?}");
    }
    let left = std::fs::read_dir(PathBuf::from(&e.cfg.state_dir).join("borrows")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "a refused borrow left requests");
}

pub(super) fn busy_lock(e: &E2e) -> PathBuf {
    e.cfg.place_dir("box-1").join("busy.lock")
}

pub(super) fn held(e: &E2e) -> bool {
    lock::held_by(&busy_lock(e)).unwrap_or(false)
}

pub(super) fn is_borrow(e: &E2e) -> bool {
    e.held_by("box-1").is_some_and(|w| w.0 == BusyReason::BORROW)
}

pub(super) fn borrow_e2e() -> E2e {
    let mut e = E2e::new();
    let agent = e.dir.join("bin").join("borrow-agent");
    // The fake agent owns only the test's VM marker, exactly where the command
    // makes its stand-in VM. This exercises real agent dispatch and cleanup.
    // While hold-clean exists, a clean says so in cleaning and waits for
    // clean-go, so a test can see what holds the place during the clean.
    let script = r#"#!/bin/sh
case "$2" in
vms) if [ -e "$E2E_DIR/vm-live" ]; then echo '["runner-up-box-1"]'; else echo '[]'; fi ;;
clean)
  echo clean >> "$E2E_DIR/borrow-clean.log"
  if [ -e "$E2E_DIR/hold-clean" ]; then
    : > "$E2E_DIR/cleaning"
    until [ -e "$E2E_DIR/clean-go" ]; do sleep 0.05; done
  fi
  rm -f "$E2E_DIR/vm-live" ;;
*) exit 1 ;;
esac
"#;
    write_exec(&agent, script);
    let m = &mut e.cfg.machines[0];
    m.agent = agent.to_string_lossy().into_owned();
    m.places[0].provider = PROVIDER_TART.into();
    m.places[0].image = "local-macos".into();
    config::save_config(&e.config, &e.cfg).unwrap();
    std::fs::create_dir_all(e.cfg.place_dir("box-1")).unwrap();
    e
}

pub(super) fn write_exec(path: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A running `runner-up borrow ... -- borrow-check`, its output going to a file.
pub(super) struct Borrow {
    pub(super) child: Child,
    out: PathBuf,
    clean_go: Option<PathBuf>,
}

impl Borrow {
    pub(super) fn start(e: &E2e, script: &str) -> Borrow {
        // This executable exists only in the caller's PATH, not the service's.
        let bin = e.dir.join("caller-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("borrow-check"), &format!("#!/bin/sh\n{script}"));
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = e.command(&[
            "borrow",
            "--config",
            &e.config,
            "--pool",
            "ru-linux",
            "--label",
            "native check",
            "--minutes",
            "20",
            "--",
            "borrow-check",
        ]);
        cmd.env("PATH", path);
        Borrow::spawn(e, "borrow-output", cmd, b"hello\n")
    }

    pub(super) fn spawn(e: &E2e, output: &str, mut cmd: std::process::Command, input: &[u8]) -> Borrow {
        let out = e.dir.join(output);
        let f = std::fs::File::create(&out).unwrap();
        let mut child = cmd.stdin(Stdio::piped()).stdout(f.try_clone().unwrap()).stderr(f).spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input).unwrap();
        Borrow { child, out, clean_go: None }
    }

    pub(super) fn said(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    pub(super) fn term(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
    }

    pub(super) fn wait(&mut self) -> i32 {
        let status = crate::agent::tests::wait_timeout(&mut self.child, Duration::from_secs(60));
        let status = status.unwrap_or_else(|| panic!("the borrow did not end within 60 seconds; it said:\n{}", self.said()));
        crate::agent::exit_code(&Ok(status))
    }
}

impl Drop for Borrow {
    fn drop(&mut self) {
        if let Some(go) = &self.clean_go {
            let _ = std::fs::write(go, "");
        }
        self.term();
        let _ = self.child.wait();
    }
}
