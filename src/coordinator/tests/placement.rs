// Intent (the coordinator's design, coordinator.rs): a placement pass never
// holds the coordinator's lock through a call to GitHub, so a scale set's
// messages and every other reader carry on while GitHub is slow to answer;
// and a scale set's session never closes while a pass acquires a job or
// registers a runner through it.

use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::*;
use crate::coordinator::place::{read_place_run, update_place_run, write_place_run};
use crate::coordinator::vmlimit::VM_READ_BUDGET_MS;

/// A scale set whose GitHub answers acquire calls only once the test
/// releases them.
struct SlowSet {
    set: FakeSet,
    asked: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    released: (Mutex<bool>, Condvar),
}

impl SlowSet {
    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }
}

impl SetClient for SlowSet {
    fn acquire_jobs(&self, ctx: &Ctx, ids: &[i64]) -> Result<Vec<i64>, scaleset::Error> {
        if let Some(asked) = self.asked.lock().unwrap().take() {
            let _ = asked.send(());
        }
        drop(self.released.1.wait_while(self.released.0.lock().unwrap(), |r| !*r).unwrap());
        self.set.acquire_jobs(ctx, ids)
    }

    fn generate_jit(&self, ctx: &Ctx, name: &str, work_folder: &str) -> Result<(String, i64), scaleset::Error> {
        self.set.generate_jit(ctx, name, work_folder)
    }

    fn remove_runner(&self, ctx: &Ctx, id: i64) -> Result<(), scaleset::Error> {
        self.set.remove_runner(ctx, id)
    }
}

/// Releases the slow set when the test ends, passing or not, so no thread of
/// it waits forever.
struct ReleaseOnDrop(Arc<SlowSet>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[test]
fn placement_lets_go_of_the_lock_while_github_answers() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    let (asked_tx, asked) = channel();
    let slow = Arc::new(SlowSet { set: FakeSet::default(), asked: Mutex::new(Some(asked_tx)), released: Default::default() });
    let _release = ReleaseOnDrop(slow.clone());
    let client: Arc<dyn SetClient> = slow.clone();
    h.svc.add_set("octo-org/app", "ru-linux", client.clone());
    let (first, rest) = f.messages.split_first().unwrap();
    h.svc.handle(&first.repo, &first.label, Some(&first.message));

    let (placed_tx, placed) = channel();
    let pass = h.svc.clone();
    std::thread::spawn(move || {
        pass.place(&Ctx::background());
        let _ = placed_tx.send(());
    });
    asked.recv_timeout(Duration::from_secs(60)).expect("the pass never asked GitHub");

    // While GitHub has not answered, the coordinator's lock is free: the setup
    // reads and the next messages are recorded.
    assert!(h.svc.mu.try_lock().is_ok(), "the placement pass held the coordinator's lock while GitHub answered");
    h.svc.setup();
    for m in rest {
        h.svc.handle(&m.repo, &m.label, Some(&m.message));
    }

    // The session closing waits for the call it made.
    let (removed_tx, removed) = channel();
    let closing = h.svc.clone();
    std::thread::spawn(move || {
        closing.remove_set("octo-org/app", "ru-linux", &client);
        let _ = removed_tx.send(());
    });
    assert_eq!(
        removed.recv_timeout(Duration::from_millis(300)),
        Err(RecvTimeoutError::Timeout),
        "the scale set's session closed while a placement pass acquired a job through it"
    );
    slow.release();
    placed.recv_timeout(Duration::from_secs(60)).expect("the pass never ended");
    removed.recv_timeout(Duration::from_secs(60)).expect("the session never closed");
    assert!(!slow.set.st().acquired.is_empty(), "the pass did not acquire the job GitHub answered for");
    assert!(!h.placed().is_empty(), "no place took the acquired job");
}

// Intent (the coordinator's design, place.rs): place.json has two writers, the
// coordinator recording its place runner's process and the place runner
// recording its job starting; neither loses the other's change.
#[test]
fn place_run_keeps_both_writers_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_box(dir.path());
    cfg.defaults();
    cfg.validate().unwrap();
    for p in cfg.places() {
        std::fs::create_dir_all(cfg.place_dir(&p.id)).unwrap();
    }
    let cfg = Arc::new(cfg);
    let launched = Time::parse("2026-10-20T17:10:14Z").unwrap();
    for i in 0..100 {
        let run = PlaceRun {
            runner_name: "runner-up-box-1-1792602614".into(),
            runner_id: 41 + i,
            target: "octo-org/app".into(),
            pool: "ru-linux".into(),
            repo: "octo-org/app".into(),
            request_id: 9101,
            job_name: "verify / linux".into(),
            launched_at: launched,
            reservation: config::Share { cores: 2, memory_mb: 4096 },
            ..Default::default()
        };
        write_place_run(&cfg, "box-1", &run).unwrap();
        let (c1, c2) = (cfg.clone(), cfg.clone());
        let (tx, rx) = channel();
        let tx2 = tx.clone();
        std::thread::spawn(move || tx.send(update_place_run(&c1, "box-1", |r| r.pid = 48213)).unwrap());
        std::thread::spawn(move || {
            tx2.send(update_place_run(&c2, "box-1", |r| {
                r.started = true;
                r.ran_job = "verify / linux".into();
            }))
            .unwrap()
        });
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(60)).expect("both writers to finish").unwrap();
        }
        let got = read_place_run(&cfg, "box-1").expect("place.json unreadable");
        assert!(
            got.pid == 48213 && got.started && got.ran_job == "verify / linux",
            "write {i}: place.json holds pid {}, started {}; want both writers' changes",
            got.pid,
            got.started
        );
    }
}

// Intent (the design, vmlimit.rs): a placement pass reads the Macs' running
// macOS VMs at once, within one time budget, so a Mac slow to answer delays
// placement by at most that budget and no other Mac's reading waits on it.

/// Two Macs, each with one tart place for ru-mac jobs.
fn two_macs(dir: &Path) -> Config {
    let mac = |name: &str| config::Machine {
        name: name.into(),
        ssh: strings(&[&format!("you@{name}")]),
        places: vec![Place {
            id: format!("{name}-1"),
            provider: config::PROVIDER_TART.into(),
            pools: strings(&["ru-mac"]),
            cores: 4,
            memory_mb: 8192,
            image: "ghcr.io/cirruslabs/macos-sequoia-base:latest".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        github: config::GitHub { repos: strings(&["octo-org/app"]), ..Default::default() },
        pools: vec![pool("ru-mac")],
        machines: vec![mac("mac-a"), mac("mac-b")],
        ..Default::default()
    }
}

/// Holds the VM read budget at ms for one test, as it is one for the process.
fn short_vm_read_budget(ms: u64) -> impl Drop {
    static BUDGET: Mutex<()> = Mutex::new(());
    struct Restore {
        _held: std::sync::MutexGuard<'static, ()>,
        old: u64,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            VM_READ_BUDGET_MS.store(self.old, Ordering::Relaxed);
        }
    }
    let held = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    Restore { _held: held, old: VM_READ_BUDGET_MS.swap(ms, Ordering::Relaxed) }
}

#[test]
fn macs_are_read_at_once() {
    let _budget = short_vm_read_budget(5000);
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(two_macs(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(Time::parse("2026-10-20T17:10:14Z").unwrap());
    // Each Mac answers only once both were asked: read one after the other,
    // the first would wait out the budget.
    let arrived = Arc::new((Mutex::new(0), Condvar::new()));
    h.svc.lock().vm_reader = Some(Arc::new(move |ctx: &Ctx, _: &config::Machine| {
        let (n, cv) = &*arrived;
        let mut n = n.lock().unwrap();
        *n += 1;
        cv.notify_all();
        while *n < 2 {
            if ctx.is_done() {
                return Err("given up".into());
            }
            n = cv.wait_timeout(n, Duration::from_millis(10)).unwrap().0;
        }
        Ok(strings(&["sequoia-dev"]))
    }));
    h.svc.refresh_vms(&Ctx::background());
    for name in ["mac-a", "mac-b"] {
        assert_eq!(h.svc.lock().vms.get(name), Some(&strings(&["sequoia-dev"])), "{name}'s VMs, want its reading");
    }
}

#[test]
fn slow_mac_delays_placement_by_the_budget_at_most() {
    let _budget = short_vm_read_budget(300);
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(two_macs(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(Time::parse("2026-10-20T17:10:14Z").unwrap());
    let hung = Arc::new((Mutex::new(false), Condvar::new()));
    let reader_hung = hung.clone();
    h.svc.lock().vm_reader = Some(Arc::new(move |_: &Ctx, m: &config::Machine| {
        if m.name == "mac-b" {
            // An agent that never answers, and ignores being given up on.
            let (over, cv) = &*reader_hung;
            drop(cv.wait_while(over.lock().unwrap(), |o| !*o).unwrap());
            return Ok(vec![]);
        }
        Ok(strings(&["sequoia-dev"]))
    }));
    let svc = h.svc.clone();
    let start = std::time::Instant::now();
    wait_for("reading the Macs", move || svc.refresh_vms(&Ctx::background()));
    assert!(start.elapsed() < Duration::from_secs(5), "reading the Macs took {:?}", start.elapsed());
    let vms = h.svc.lock().vms.clone();
    *hung.0.lock().unwrap() = true;
    hung.1.notify_all();
    assert_eq!(vms.get("mac-a"), Some(&strings(&["sequoia-dev"])), "mac-a's VMs, want its reading");
    assert_eq!(vms.get("mac-b"), None, "want no reading from a Mac that did not answer");
}
