use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::borrow::{Borrow, borrow_e2e, busy_lock, held};
use super::e2e::{E2e, job_message, verify_job};
use super::journeys::skip_if_short;
use crate::config;
use crate::coordinator::BusyReason;
use crate::go::time::Time;
use crate::lock;

// Intent (README, "Borrowing a virtual Mac"): the real borrow supervisor
// invokes the real Tart agent in a private store, deletes a VM left by the
// caller, and kills stray command descendants. Creating an empty Linux VM
// exercises Tart's storage without an OS download.
#[test]
fn real_tart_borrow_cleanup() {
    let Some(mut e) = real_tart_borrow_e2e() else { return };
    e.serve();
    let mut b = Borrow::start(
        &e,
        r#"set -eu
tart create "$BORROWED_VM" --linux --disk-size 1
sleep 120 &
echo $! > "$E2E_DIR/child-pid"
printf 'created=%s cwd=%s\n' "$BORROWED_VM" "$PWD"
exit 23"#,
    );
    let code = b.wait();
    assert_eq!(code, 23, "exit={code} output={}", b.said());
    e.eventually("real agent releases the place", || !held(&e));
    assert_empty_tart_store(&e);
    let pid = borrow_pid(&e.dir.join("child-pid"));
    e.eventually("stray child is killed", || gone(pid));
}

// Intent (README, "Borrowing a virtual Mac"): losing the caller without a
// graceful signal still disconnects its private socket, stops the command,
// cleans the VM, and frees the place.
#[test]
fn real_tart_borrow_disconnect() {
    let Some(mut e) = real_tart_borrow_e2e() else { return };
    e.serve();
    let mut b = Borrow::start(
        &e,
        r#"set -eu
tart create "$BORROWED_VM" --linux --disk-size 1
echo $$ > "$E2E_DIR/disconnect-pid"
exec sleep 120"#,
    );
    e.eventually("command starts before caller disconnects", || e.dir.join("disconnect-pid").exists());
    let pid = borrow_pid(&e.dir.join("disconnect-pid"));
    b.child.kill().unwrap();
    let _ = b.child.wait();
    e.eventually("disconnect stops command and frees place", || !held(&e) && gone(pid));
    assert_empty_tart_store(&e);
}

// Intent (README, "Borrowing a virtual Mac"): a granted borrow's persisted
// grant time bounds the real supervisor, stopping a live foreground command and
// cleaning its VM before exit 124.
#[test]
fn real_tart_borrow_deadline() {
    let Some(e) = real_tart_borrow_e2e() else { return };
    // The short socket address also works with macOS's 104-byte limit.
    let socket_dir = tempfile::Builder::new().prefix("ru-deadline-").tempdir_in("/tmp").unwrap();
    let socket = socket_dir.path().join("s");
    let listener = UnixListener::bind(&socket).unwrap();
    let id = "request-live-deadline";
    let dir = PathBuf::from(&e.cfg.state_dir).join("borrows").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let busy = lock::busy(&busy_lock(&e)).unwrap().unwrap();
    // Use the persisted grant contract rather than waiting a full hour.
    let granted = Time(chrono::Utc::now() - chrono::Duration::hours(1) + chrono::Duration::seconds(10));
    let grant = json!({
        "id": id, "pool": "ru-linux", "label": "deadline check", "minutes": 1,
        "place": "box-1", "socket": socket, "granted": granted,
    });
    config::write_json(&dir.join("granted.json"), &grant).unwrap();
    let out = std::fs::File::create(e.dir.join("deadline-output")).unwrap();
    let null = std::fs::File::open("/dev/null").unwrap();
    let mut cmd = e.command(&["run-borrow", "--config", &e.config, "--place", "box-1", "--borrow", id]);
    crate::proc::pass_as_fd3(&mut cmd, &busy);
    let mut child = cmd.stdout(out.try_clone().unwrap()).stderr(out.try_clone().unwrap()).spawn().unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(listener.accept().map(|(c, _)| c));
    });
    let mut conn = rx.recv_timeout(Duration::from_secs(10)).expect("run-borrow did not connect within 10 seconds").unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    crate::coordinator::borrow::unix::send_fds(&conn, &[null.as_raw_fd(), out.as_raw_fd(), out.as_raw_fd()]).unwrap();
    let script = r#"set -eu; tart create "$BORROWED_VM" --linux --disk-size 1; echo $$ > "$E2E_DIR/deadline-pid"; exec sleep 120"#;
    let mut env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    env.extend(e.env.iter().map(|(k, v)| format!("{k}={v}")));
    let mut line = serde_json::to_vec(&json!({"Args": ["/bin/sh", "-c", script], "Env": env, "Dir": e.dir})).unwrap();
    line.push(b'\n');
    conn.write_all(&line).unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    let code: i32 = answer.trim().parse().unwrap();
    let status = crate::agent::tests::wait_timeout(&mut child, Duration::from_secs(60)).expect("run-borrow did not end");
    let exit = crate::agent::exit_code(&Ok(status));
    assert!(code == 124 && exit == 124, "deadline result={code} exit={exit}");
    let pid = borrow_pid(&e.dir.join("deadline-pid"));
    assert!(gone(pid), "deadline command remains alive");
    assert_empty_tart_store(&e);
}

// Intent (README, "Borrowing a virtual Mac" and "Order rules"): local requests
// share the real coordinator's queue with GitHub work, and declared duration
// wins over age until the waited rule applies. Unknown GitHub jobs use the
// default 15-minute estimate, after both these borrows.
#[test]
fn borrow_cli_live_shared_queue() {
    skip_if_short!();
    let mut e = borrow_e2e();
    let busy = lock::busy(&busy_lock(&e)).unwrap().unwrap();
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    let name = job["jobDisplayName"].as_str().unwrap().to_string();
    e.eventually("GitHub job enters the queue", || e.status().contains(&name));
    let mut long = start_live(&e, "long", 10);
    e.eventually("long request enters queue", || long.said().contains("position"));
    let mut short = start_live(&e, "short", 5);
    e.eventually("short borrow ranks ahead of older work", || short.said().contains("position 1"));
    drop(busy);
    started(&e, "short");
    assert!(!e.dir.join("long-started").exists(), "long command ran before short");
    assert!(e.actions.read().acquired.is_empty(), "GitHub job acquired before shortest borrow");
    finish(&e, "short", &mut short);
    started(&e, "long");
    assert!(e.actions.read().acquired.is_empty(), "GitHub job acquired before known borrow");
    finish(&e, "long", &mut long);
    e.eventually("GitHub work is acquired after borrows", || !e.actions.read().acquired.is_empty());
}

// Intent (README, "Borrowing a virtual Mac"): two independent borrows with
// equal labels may run concurrently, including across restart and a reported
// named VM, but a third must wait. GitHub and the VM inventory are isolated
// stand-ins; supervisors are real.
#[test]
fn borrow_cli_live_two_slot_accounting() {
    skip_if_short!();
    let mut e = borrow_e2e();
    add_places(&mut e, 8, 16384, &["box-2", "box-3"]);
    e.serve();
    let mut one = start_live(&e, "one", 20);
    started(&e, "one");
    // The isolated agent reports the first borrow's named VM as running.
    std::fs::write(e.dir.join("vm-live"), "").unwrap();
    let code = e.stop();
    assert_eq!(code, 0, "stop={code}");
    e.serve();
    let mut two = start_live(&e, "two", 20);
    started(&e, "two");
    let mut three = start_live(&e, "three", 20);
    e.eventually("third borrow reports the two-VM boundary", || three.said().contains("macOS VM limit"));
    let borrows = e.cfg.places().iter().filter(|p| e.held_by(&p.id).is_some_and(|w| w.0 == BusyReason::BORROW)).count();
    assert_eq!(borrows, 2, "expected two active borrows: {}", e.status());
    assert!(!e.dir.join("three-started").exists(), "third borrow exceeded VM limit");
    finish(&e, "one", &mut one);
    started(&e, "three");
    finish(&e, "two", &mut two);
    finish(&e, "three", &mut three);
}

// Intent (README, "Borrowing a virtual Mac"): active borrows reserve their
// share across coordinator restart and cannot take the cores or memory
// explicitly kept for the machine's owner.
#[test]
fn borrow_cli_live_owner_room() {
    skip_if_short!();
    for (name, cores, memory, reason) in [
        ("cores", 4, 16384, "cores 2 of 3 in use, 1 kept for the owner"),
        ("memory", 8, 8192, "memory 4096 of 7168 MB in use, 1024 MB kept for the owner"),
    ] {
        let mut e = borrow_e2e();
        add_places(&mut e, cores, memory, &["box-2"]);
        e.serve();
        let mut one = start_live(&e, "one", 20);
        started(&e, "one");
        let code = e.stop();
        assert_eq!(code, 0, "{name}: stop={code}");
        e.serve();
        let mut two = start_live(&e, "two", 20);
        e.eventually("owner room blocks second borrow after restart", || two.said().contains(reason));
        assert!(!e.dir.join("two-started").exists(), "{name}: second command consumed the owner's room");
        finish(&e, "one", &mut one);
        started(&e, "two");
        finish(&e, "two", &mut two);
    }
}

/// Copies box-1 as each of ids, on a machine of cores and memory.
fn add_places(e: &mut E2e, cores: i64, memory: i64, ids: &[&str]) {
    let m = &mut e.cfg.machines[0];
    (m.cores, m.memory_mb) = (cores as _, memory as _);
    let first = m.places[0].clone();
    for id in ids {
        m.places.push(config::Place { id: id.to_string(), ..first.clone() });
    }
    for id in ids {
        std::fs::create_dir_all(e.cfg.place_dir(id)).unwrap();
    }
    config::save_config(&e.config, &e.cfg).unwrap();
}

/// The harness of the real-Tart tests. CI installs no Tart, so there they
/// skip; make test-real-tart, the check run before a release (CONTRIBUTING.md,
/// "Before a release"), sets RUNNER_UP_REAL_TART so a missing Tart fails them
/// instead of letting them pass unrun. Every test that uses it is named
/// real_tart_*, which is how that check selects them.
fn real_tart_borrow_e2e() -> Option<E2e> {
    if std::process::Command::new("tart").arg("--version").output().is_err() {
        assert!(std::env::var_os("RUNNER_UP_REAL_TART").is_none(), "RUNNER_UP_REAL_TART is set, but Tart is not installed");
        eprintln!("requires Tart");
        return None;
    }
    let mut e = borrow_e2e();
    e.cfg.machines[0].agent = super::runner_up().to_string();
    config::save_config(&e.config, &e.cfg).unwrap();
    let home = e.dir.join("tart");
    std::fs::create_dir_all(&home).unwrap();
    e.env.push(("TART_HOME".into(), home.to_string_lossy().into_owned()));
    Some(e)
}

fn assert_empty_tart_store(e: &E2e) {
    let mut list = std::process::Command::new("tart");
    list.args(["list", "--format", "json"]).envs(e.env.iter().map(|(k, v)| (k, v)));
    let (code, out) = crate::agent::tests::run(&mut list, "");
    assert_eq!(code, 0, "tart list: {out}");
    let vms: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|err| panic!("leftover VMs: {out} ({err})"));
    assert!(vms.is_empty(), "leftover VMs: {out}");
}

fn borrow_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn gone(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) }
}

fn started(e: &E2e, name: &str) {
    e.eventually(&format!("{name} command starts"), || e.dir.join(format!("{name}-started")).exists());
}

fn finish(e: &E2e, name: &str, b: &mut Borrow) {
    std::fs::write(e.dir.join(format!("{name}-done")), "").unwrap();
    let code = b.wait();
    assert_eq!(code, 0, "{name} exited {code}: {}", b.said());
}

fn start_live(e: &E2e, name: &str, minutes: i64) -> Borrow {
    let script = format!(
        r#"printf 'place=%s vm=%s\n' "$BORROWED_PLACE" "$BORROWED_VM"; touch "$E2E_DIR/{name}-started"; while [ ! -e "$E2E_DIR/{name}-done" ]; do sleep 0.05; done"#
    );
    let minutes = minutes.to_string();
    let cmd = e.command(&[
        "borrow",
        "--config",
        &e.config,
        "--pool",
        "ru-linux",
        "--label",
        "same label",
        "--minutes",
        &minutes,
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    Borrow::spawn(e, &format!("{name}-output"), cmd, b"")
}
