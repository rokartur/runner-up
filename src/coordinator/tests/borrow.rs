use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use super::vmlimit::one_mac;
use super::*;
use crate::coordinator::borrow::unix::{BorrowCommand, BorrowRequest, borrow_dir, borrow_limit, run_borrow_command};
use crate::coordinator::snapshot::BusyReason;
use crate::ctx::Ctx;
use crate::plan::{self, PlaceView};

fn enqueue_borrow(cfg: &Config, r: BorrowRequest) -> File {
    let dir = borrow_dir(cfg, &r.id);
    std::fs::create_dir_all(&dir).unwrap();
    let live = lock::busy(&dir.join("live.lock")).unwrap().expect("could not hold the borrow");
    config::write_json(&dir.join("request.json"), &r).unwrap();
    live
}

fn borrow_request(id: &str, label: &str, minutes: i64, queued: Time) -> BorrowRequest {
    let mut r = BorrowRequest::default();
    (r.id, r.pool, r.label, r.minutes, r.queued) = (id.into(), "ru-mac".into(), label.into(), minutes, queued);
    r
}

// Intent (README, "Borrowing a virtual Mac"): borrows rank among recorded jobs
// by the same rules, using declared minutes rather than the unknown or learned
// job estimate. They may borrow only tart places, never a host or retired crew
// place.
#[test]
fn borrow_ranks_among_recorded_jobs() {
    let cases: [(&str, &[&str], i64, i64, &str); 4] = [
        ("short borrow", &["shortest"], 5, 0, "borrow"),
        ("long borrow", &["shortest"], 30, 0, "job"),
        ("aged job goes first", &["waited:60m", "shortest"], 5, 0, "job"),
        ("oldest borrow", &["waited:60m", "shortest"], 30, -5, "borrow"),
    ];
    for (name, order, minutes, queued_hours, first) in cases {
        let f = load_fixture("waiting-jobs.json");
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = one_mac(dir.path());
        cfg.order = strings(order);
        cfg.machines[0].places.truncate(1);
        let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
        h.set_now(f.now);
        let busy = lock::busy(&h.cfg.place_dir("mac-1").join("busy.lock")).unwrap();
        h.feed_as_mac(&f);
        let queued = Time(f.now.0 + chrono::TimeDelta::hours(queued_hours));
        let _live = enqueue_borrow(&h.cfg, borrow_request("request-check", "native check", minutes, queued));
        drop(busy);
        h.place();
        let run = h.run("mac-1");
        assert_eq!(!run.borrow_id.is_empty(), first == "borrow", "{name}: first run {run:?}, want {first}");
        h.launcher.finish("mac-1");
        h.tidy();
        h.place();
        let next = h.run("mac-1");
        assert!(!(first == "borrow" && !next.borrow_id.is_empty()), "{name}: job did not take the place after borrow");
        h.launcher.finish("mac-1");
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = one_mac(dir.path());
    let rules = plan::parse_order(&Config::default()).unwrap();
    let host = cfg.machines[0].places[3].clone();
    let job = plan::Job { borrow_id: "request-host".into(), pool: "ru-mac".into(), ..Default::default() };
    let got = plan::plan(&[job], &[PlaceView { place: host, busy: false, drain: String::new() }], &HashMap::new(), &rules, Time::now());
    assert!(got.is_empty(), "borrow took a host place");
}

// Intent (README, "Borrowing a virtual Mac"): a borrow survives a service
// restart and counts its pool's launch-time size and named VM once, even after
// a size edit and when the agent reports that VM as running.
#[test]
fn borrow_counts_once_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_mac(dir.path());
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (3, 6144);
    let reservation = cfg.pools[0].size();
    cfg.machines[0].places.truncate(2);
    let l = FakeLauncher::new();
    let h = new_harness(cfg.clone(), l.clone(), Sets::new());
    h.set_now(Time::now());
    let _live = enqueue_borrow(&h.cfg, borrow_request("request-check", "native check", 20, h.now()));
    h.place();
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 4096);
    cfg.defaults();
    let (events, _rx) = channel();
    let s = Service::new(Arc::new(cfg.clone()), Logger::discard(), l.clone(), events).unwrap();
    s.lock().vms.insert("mac".into(), vec![crate::agent::clone_name("mac-1")]);
    let (busy, why) = s.place_busy(&cfg.machines[0].places[1]);
    assert!(!busy, "second slot blocked by a double count: {why:?}");
    assert_eq!(s.in_use("mac", ""), reservation, "want the frozen reservation");
    let (busy, why) = s.place_busy(&cfg.machines[0].places[0]);
    assert!(busy && why.as_ref().is_some_and(|w| w.0 == BusyReason::BORROW), "borrow status: {busy} {why:?}");
    s.lock().refresh_borrows();
    assert!(s.lock().waiting.is_empty(), "granted borrow queued again after restart");
    l.finish("mac-1");
}

// Intent (README, "Borrowing a virtual Mac"): an abandoned waiter holds nothing
// and cannot later be granted a place. Equal labels still identify independent
// requests.
#[test]
fn borrow_abandoned_waiter_and_equal_labels() {
    let dir = tempfile::tempdir().unwrap();
    let l = FakeLauncher::new();
    let h = new_harness(one_mac(dir.path()), l.clone(), Sets::new());
    h.set_now(Time::now());
    // a waited longer, so only its abandonment lets b go first.
    let a = enqueue_borrow(&h.cfg, borrow_request("request-a", "check", 20, h.now().sub_dur(Duration::from_secs(60))));
    let _b = enqueue_borrow(&h.cfg, borrow_request("request-b", "check", 20, h.now()));
    h.svc.lock().refresh_borrows();
    assert_eq!(h.svc.lock().waiting.len(), 2, "equal labels overwrote a request");
    drop(a);
    h.place();
    let granted: Vec<String> = l
        .st()
        .jits
        .iter()
        .filter_map(|(id, jit)| jit.strip_prefix("borrow:").map(|b| format!("{id} {b}")))
        .collect();
    assert!(granted.len() == 1 && granted[0].ends_with(" request-b"), "places granted {granted:?}, want request-b alone");
    assert!(!borrow_dir(&h.cfg, "request-a").exists(), "the abandoned request's folder remains");
    assert!(h.svc.lock().waiting.is_empty(), "still waiting: {:?}", h.svc.lock().waiting.keys());
    // A later pass, with places free, grants nothing more.
    h.place();
    let held: Vec<String> = l.st().held.keys().cloned().collect();
    assert_eq!(held.len(), 1, "places run borrows after a later pass: {held:?}, want 1");
    for id in held {
        l.finish(&id);
    }
}

// Intent (README, "Borrowing a virtual Mac"): a borrow ends after max(1 hour,
// declared minutes). The duration feeds the same command cancellation path
// exercised below.
#[test]
fn borrow_limit_is_at_least_an_hour() {
    for minutes in [1, 20, 60, 90, 180] {
        assert_eq!(borrow_limit(minutes), Duration::from_secs(60 * minutes.max(60) as u64), "{minutes} minutes");
    }
}

fn command_output() -> (tempfile::TempDir, std::path::PathBuf, File) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output");
    let out = File::create(&path).unwrap();
    (dir, path, out)
}

fn borrow_command(script: &str) -> BorrowCommand {
    BorrowCommand {
        args: strings(&["sh", "-c", script]),
        env: std::env::vars().map(|(k, v)| format!("{k}={v}")).collect(),
        dir: String::new(),
    }
}

// Intent (README, "Borrowing a virtual Mac"): a stopped or expired borrow stops
// its command and returns an explanation, and ordinary command exits retain
// their exit status.
#[test]
fn borrow_command_exit_and_deadline() {
    let p = Place { id: "mini-vm1".into(), cores: 4, memory_mb: 16384, ..Default::default() };
    let _spawns = spawns();

    let (_dir, path, out) = command_output();
    let script = r#"printf '%s %s %s %s' "$BORROWED_PLACE" "$BORROWED_VM" "$BORROWED_CPUS" "$BORROWED_MEMORY_MB"; exit 23"#;
    let code = run_borrow_command(&Ctx::background(), &p, borrow_command(script), File::open("/dev/null").unwrap(), out.try_clone().unwrap(), &out);
    assert_eq!(code, 23);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "mini-vm1 runner-up-mini-vm1 4 16384");

    let (dir, path, out) = command_output();
    let pid_file = dir.path().join("pid");
    let script = format!(r#"echo $$ > "{}"; exec sleep 60"#, pid_file.display());
    let ctx = Ctx::background().with_timeout(Duration::from_millis(200));
    let code = run_borrow_command(&ctx, &p, borrow_command(&script), File::open("/dev/null").unwrap(), out.try_clone().unwrap(), &out);
    assert_eq!(code, 124);
    let b = std::fs::read_to_string(&path).unwrap();
    assert!(b.contains("deadline exceeded"), "missing timeout explanation: {b}");
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only asks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    assert!(!alive, "expired command still alive: {pid}");
}

// Intent (README, "Borrowing a virtual Mac"): a borrowed command that a signal
// ends reports the shell-compatible status 128 plus the signal, not a lost
// status.
#[test]
fn borrow_command_signal_status() {
    let _spawns = spawns();
    let (_dir, _path, out) = command_output();
    let p = Place { id: "mini-vm1".into(), ..Default::default() };
    let code = run_borrow_command(&Ctx::background(), &p, borrow_command("kill -TERM $$"), File::open("/dev/null").unwrap(), out.try_clone().unwrap(), &out);
    assert_eq!(code, 128 + libc::SIGTERM);
}

// Intent (README, "Borrowing a virtual Mac"): a request enters the line only
// after its live lock is held. A scheduling pass during caller initialization
// must not delete the caller's folder before it can publish the request.
#[test]
fn borrow_unpublished_request_is_not_collected() {
    let state = tempfile::tempdir().unwrap();
    let h = new_harness(one_mac(state.path()), FakeLauncher::new(), Sets::new());
    let dir = borrow_dir(&h.cfg, "request-starting");
    std::fs::create_dir_all(&dir).unwrap();
    for with_lock_file in [false, true] {
        if with_lock_file {
            std::fs::write(dir.join("live.lock"), b"").unwrap();
        }
        h.svc.lock().refresh_borrows();
        assert!(dir.exists(), "collected unpublished request (lock file {with_lock_file})");
    }
    let live = enqueue_borrow(&h.cfg, borrow_request("request-starting", "check", 20, Time::now()));
    h.svc.lock().refresh_borrows();
    assert_eq!(h.svc.lock().waiting.len(), 1, "published request did not enter the line");
    drop(live);
    h.svc.lock().refresh_borrows();
    assert!(!dir.exists(), "abandoned published request not collected");
}
