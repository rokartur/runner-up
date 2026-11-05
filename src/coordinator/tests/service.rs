// Intent (README, "How it works", "Setup file" and "Order rules"): the planner
// keeps pool compatibility, so a job runs only in a place serving its pool; it
// keeps each machine within its core and memory budget less the room kept for
// its owner; it gives a freed place to the job the order rules pick, across
// repositories; and a coordinator restart loses no waiting job and leaves
// running jobs in their places. Fixtures are scale set messages runner-up's
// predecessor recorded from GitHub, renamed (testdata/coordinator/messages).

use std::collections::HashMap;
use std::time::Duration;

use super::*;
use crate::coordinator::runners::unused_key;
use crate::scaleset::{JobMessage, MESSAGE_TYPE_JOB_COMPLETED};

// The jobs of waiting-jobs.json, as places run them.
pub const VERIFY: &str = "octo-org/app verify / linux";
pub const PR184: &str = "octo-org/app affected-journeys";
pub const PUBLISH: &str = "octo-org/app Build and publish (a dry run publishes nothing)";
pub const SUITE: &str = "octo-user/tool suite";
pub const RELEASE: &str = "octo-user/wallet Release";

fn fresh(cfg: Config) -> Harness {
    new_harness(cfg, FakeLauncher::new(), Sets::new())
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn waiting_jobs_fill_every_place_in_order() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let h = fresh(one_box(dir.path()));
    h.set_now(f.now);
    h.feed(&f);
    // Every place runs a job: the large place the large job, the small
    // places the two oldest others.
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PUBLISH)]);
    assert_eq!(h.waiting(), 3, "want 3 waiting (suite, Release, PR #186)");

    // The large place frees. Under the default order the hour-old suite,
    // which waited longer than the wallet's release, takes it.
    h.finish_and_place("box-large");
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", SUITE)]);

    // A small place takes the remaining ru-linux job; the release waits for
    // the large place.
    h.finish_and_place("box-1");
    h.want(&[("box-1", PR184), ("box-2", PR184), ("box-large", SUITE)]);
    h.finish_and_place("box-2");
    h.want(&[("box-1", PR184), ("box-large", SUITE)]);
    assert_eq!(h.run("box-1").request_id, 9104, "box-1 runs another request than PR #186's 9104");
    h.finish_and_place("box-large");
    h.want(&[("box-1", PR184), ("box-large", RELEASE)]);
    assert_eq!(h.waiting(), 0);
    // Each job was acquired only once a place was chosen for it.
    assert_eq!(h.set("octo-org/app", "ru-linux").acquired, [9101, 9102, 9104]);
    // The registration went to the place runner, for a runner named after
    // its place.
    let jit = h.launcher.st().jits["box-1"].clone();
    assert!(jit.starts_with("jit-for-runner-up-box-1-"), "box-1 got registration {jit:?}");
}

// Intent (a bug moving a fleet to runner-up found: Linux jobs must keep their
// work in /home/runner/work, where GitHub's Ubuntu runners do, so a cache
// saved there restores where the tools look). A runner is registered with
// its place's work folder, or the runner's own _work by default.
#[test]
fn runner_is_registered_with_its_places_work_folder() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let mut cfg = one_box(dir.path());
    cfg.machines[0].places[0].work_folder = "/home/runner/work".into();
    let h = fresh(cfg);
    h.set_now(f.now);
    h.feed(&f);
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PUBLISH)]);
    let mut got = HashMap::new();
    for set in h.sets.values() {
        for (name, wf) in &set.st().work_folders {
            let parts: Vec<&str> = name.split('-').collect();
            got.insert(parts[2..4].join("-"), wf.clone());
        }
    }
    let want: HashMap<String, String> =
        [("box-1", "/home/runner/work"), ("box-2", "_work"), ("box-large", "_work")].map(|(a, b)| (a.into(), b.into())).into();
    assert_eq!(got, want, "runners registered with work folders");
}

#[test]
fn large_job_never_starts_in_a_small_place() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let cfg = one_box(dir.path());
    std::fs::create_dir_all(cfg.place_dir("box-large")).unwrap();
    let hold = lock::busy(&cfg.place_dir("box-large").join("busy.lock")).unwrap().expect("could not hold the large place");
    let h = fresh(cfg);
    h.set_now(f.now);
    // Only the two jobs asking for ru-large, with both small places free.
    for m in &f.messages {
        if m.label == "ru-large" {
            h.handle(&m.repo, &m.label, &m.message);
        }
    }
    assert!(h.launcher.st().held.is_empty(), "a ru-large job started in a place that does not serve ru-large");
    assert_eq!(h.waiting(), 2, "want both ru-large jobs still at GitHub");
    assert!(h.set("octo-org/app", "ru-large").acquired.is_empty(), "acquired jobs no place could take");

    // The large place frees and takes the older one.
    drop(hold);
    h.place();
    h.want(&[("box-large", PUBLISH)]);
    h.finish_and_place("box-large");
    h.want(&[("box-large", RELEASE)]);
}

#[test]
fn job_gone_at_github_gives_the_place_to_the_next() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let sets = Sets::from([(set_key("octo-org/app", "ru-linux"), FakeSet::gone(&[9101]))]);
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), sets);
    h.set_now(f.now);
    h.feed(&f);
    // verify was cancelled while it waited: nothing runs it, and its place
    // goes to the next job that fits.
    h.want(&[("box-1", SUITE), ("box-2", PR184), ("box-large", PUBLISH)]);
    assert_eq!(h.set("octo-org/app", "ru-linux").acquired, [9102], "want only PR #184's 9102 acquired");
}

fn reason(h: &Harness, p: &Place) -> (bool, String) {
    let (busy, why) = h.svc.place_busy(p);
    (busy, why.map(|w| w.0).unwrap_or_default())
}

// Intent (README, "Setup file"): a place starts a job only while its share fits
// beside the shares of its machine's running places, within the machine's cores
// and memory less the room kept for its owner.
#[test]
fn machine_budget_keeps_the_owners_room() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let mut cfg = one_box(dir.path());
    (cfg.machines[0].cores, cfg.machines[0].room_cores) = (8, 2);
    (cfg.machines[0].memory_mb, cfg.machines[0].room_memory_mb) = (32768, 4096);
    let h = fresh(cfg);
    h.set_now(f.now);
    h.feed(&f);
    // 6 of 8 cores are the places'. The two small places reserve 4; the
    // large one's 4 more would eat into the owner's 2.
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);
    let places = &h.cfg.machines[0].places;
    assert_eq!(reason(&h, &places[2]), (true, "cores 4 of 6 in use, 2 kept for the owner".into()));

    // A small place frees: the large job fits now, and takes the large
    // place; the freed small place stays empty, though ru-linux jobs wait,
    // because the machine has no cores left for it.
    h.finish_and_place("box-1");
    h.want(&[("box-2", PR184), ("box-large", PUBLISH)]);
    assert_eq!(reason(&h, &places[0]), (true, "cores 6 of 6 in use, 2 kept for the owner".into()));

    // Memory counts the same way: with room for only 12 GB of places, the
    // large place's 8 GB never fits beside a small place's 4 GB and another.
    let dir2 = tmp();
    let mut cfg2 = one_box(dir2.path());
    (cfg2.machines[0].cores, cfg2.machines[0].memory_mb, cfg2.machines[0].room_memory_mb) = (16, 16384, 4096);
    let h2 = fresh(cfg2);
    h2.set_now(f.now);
    h2.feed(&f);
    h2.want(&[("box-1", VERIFY), ("box-2", PR184)]);
    assert_eq!(reason(&h2, &h2.cfg.machines[0].places[2]).1, "memory 8192 of 12288 MB in use, 4096 MB kept for the owner");
}

#[test]
fn restart_keeps_waiting_jobs_and_running_places() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tmp();
    let h = fresh(one_box(dir.path()));
    h.set_now(f.now);
    h.feed(&f);
    let before = h.placed();

    // The coordinator restarts. The place runners carry on and keep their
    // locks; GitHub does not announce the waiting jobs again.
    let h2 = new_harness(one_box(dir.path()), h.launcher.clone(), h.sets.clone());
    h2.set_now(f.now.add(Duration::from_secs(60)));
    assert_eq!(h2.waiting(), 3, "restarted coordinator's waiting jobs");
    h2.place();
    assert_eq!(h2.placed(), before);

    h2.finish_and_place("box-1");
    h2.want(&[("box-1", SUITE), ("box-2", PR184), ("box-large", PUBLISH)]);
}

fn tick(h: &Harness, d: Duration) {
    h.advance(d);
    h.tidy();
}

// Intent (README, "When GitHub closes a job"): a runner whose job was cancelled
// before it started keeps its place while GitHub cannot be reached, and is
// asked about again at the next tick. Once GitHub removes it, its place is
// stopped, once; when the place ends the runner is not removed a second time,
// and the place takes the next job.
#[test]
fn cancelled_before_runner_stops_the_spare_runner_once_github_removes_it() {
    let dir = tmp();
    let h = fresh(one_box(dir.path()));
    let spare = spare_runner(&h);

    // PR #184 is cancelled while GitHub cannot be reached: its runner keeps
    // its place.
    h.set("octo-org/app", "ru-linux").unreachable = 1;
    cancel_before_runner(&h);
    assert!(h.set("octo-org/app", "ru-linux").removed.is_empty());
    assert!(h.launcher.st().stopped.is_empty());
    // At the next tick GitHub removes it, and its place is stopped once.
    let every = h.cfg.seconds(h.cfg.tick_seconds);
    tick(&h, every);
    tick(&h, every);
    assert_eq!(h.set("octo-org/app", "ru-linux").removed, [spare.runner_id]);
    assert_eq!(h.launcher.st().stopped, ["box-2"]);

    // Its place ends. The runner is gone at GitHub already, and the place
    // takes the suite.
    h.launcher.give_up("box-2");
    tick(&h, Duration::from_secs(60));
    h.place();
    tick(&h, every);
    assert_eq!(h.set("octo-org/app", "ru-linux").removed.len(), 1, "want the runner removed once");
    h.want(&[("box-1", VERIFY), ("box-2", SUITE), ("box-large", PUBLISH)]);
}

// A runner that ended before its job reached it, as a virtual machine that
// never boots does, stays in the repository's runner list, offline, unless the
// coordinator removes it, even when its place takes a new job before tidy
// reads the ended runner.
#[test]
fn unused_runner_is_removed_when_its_place_is_taken_before_tidy() {
    let dir = tmp();
    let h = fresh(one_box(dir.path()));
    let ended = spare_runner(&h);
    take_freed_place_before_tidy(&h);

    // The next tidy still removes the ended runner, and only it.
    h.tidy();
    assert_eq!(h.set("octo-org/app", "ru-linux").removed, [ended.runner_id]);
    h.tidy();
    assert_eq!(h.set("octo-org/app", "ru-linux").removed.len(), 1, "want the ended runner removed once");
}

// A removal GitHub does not answer is tried again, after the place has taken
// its next job and after a coordinator restart.
#[test]
fn unused_runner_outlives_a_failed_removal_and_a_restart() {
    let dir = tmp();
    let h = fresh(one_box(dir.path()));
    let ended = spare_runner(&h);
    h.set("octo-org/app", "ru-linux").unreachable = 1;
    take_freed_place_before_tidy(&h);
    h.tidy();
    assert!(h.set("octo-org/app", "ru-linux").removed.is_empty(), "removed runners while GitHub could not be reached");

    let h2 = new_harness(one_box(dir.path()), h.launcher.clone(), h.sets.clone());
    h2.set_now(h.now().add(Duration::from_secs(60)));
    h2.tidy();
    assert_eq!(h2.set("octo-org/app", "ru-linux").removed, [ended.runner_id]);
}

// Intent (README, "Setup file", startTimeoutSeconds): a runner no job reached
// in time keeps its place while GitHub cannot be reached, and is asked about
// again at the next tick. Once GitHub removes it, its place is stopped; when
// the place ends the runner is not removed a second time, and its job gets a
// new runner.
#[test]
fn idle_runner_keeps_its_place_until_github_removes_it() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tmp();
    let h = fresh(super::assigned::proof_box(dir.path(), &["box-1"]));
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", super::assigned::PROOF_JOB)]);
    let first = h.run("box-1");
    let set = || h.set("octo-org/app", "ru-proof");

    // A second before the start timeout the runner is left alone.
    tick(&h, h.cfg.seconds(h.cfg.start_timeout_seconds) - Duration::from_secs(1));
    assert!(set().removed.is_empty() && h.launcher.st().stopped.is_empty(), "before the start timeout");
    // At the start timeout GitHub cannot be reached: the runner keeps its
    // place.
    set().unreachable = 1;
    tick(&h, Duration::from_secs(1));
    assert!(set().removed.is_empty() && h.launcher.st().stopped.is_empty(), "while GitHub could not be reached");
    // At the next tick GitHub removes it, and its place is stopped once.
    let every = h.cfg.seconds(h.cfg.tick_seconds);
    tick(&h, every);
    tick(&h, every);
    assert_eq!(set().removed, [first.runner_id]);
    assert_eq!(h.launcher.st().stopped, ["box-1"]);

    // Its place ends. The runner is gone at GitHub already, and its job
    // gets a new runner.
    h.launcher.give_up("box-1");
    tick(&h, Duration::from_secs(1));
    h.place();
    tick(&h, every);
    assert_eq!(set().removed.len(), 1, "want the runner removed once");
    h.want(&[("box-1", super::assigned::PROOF_JOB)]);
    assert_ne!(h.run("box-1").runner_name, first.runner_name, "box-1 still names the first runner");
}

// Intent (the fix that logs a refused removal for an ended place as a stranded
// job, so the next one is diagnosed at once): when a runner's place has ended
// but GitHub will not remove the runner because it gave it a job, the
// coordinator says that job is stranded, naming the runner, and keeps trying to
// remove the runner.
#[test]
fn refused_removal_of_an_ended_runner_names_the_stranded_job() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tmp();
    let h = fresh(super::assigned::proof_box(dir.path(), &["box-1"]));
    let out = LogBuffer::default();
    h.svc.lock().log = Logger::new(Box::new(out.clone()));
    h.feed_range(&f, 0, 1);
    let run = h.run("box-1");

    // GitHub gives the runner a job just as its place ends, as when SSH to
    // the machine drops.
    h.set("octo-org/app", "ru-proof").running.insert(run.runner_id);
    h.launcher.give_up("box-1");
    h.advance(Duration::from_secs(60));
    h.tidy();
    let want = format!("msg=\"a runner whose place has ended is running a job at GitHub; the job is stranded\" runner={}", run.runner_name);
    assert!(out.text().contains(&want), "the coordinator logged:\n{}", out.text());
    assert!(h.svc.lock().unused.contains_key(&unused_key(&run)), "want the runner still to remove");
    assert!(h.set("octo-org/app", "ru-proof").removed.is_empty());
}

/// Places the waiting jobs and has box-1's runner start verify, leaving PR
/// #184's runner in box-2 waiting for its job. It returns that runner's run.
pub fn spare_runner(h: &Harness) -> PlaceRun {
    let f = load_fixture("waiting-jobs.json");
    h.set_now(f.now);
    h.feed(&f);
    let first = h.run("box-1");
    let spare = h.run("box-2");
    let started = JobMessage { runner_name: first.runner_name, runner_request_id: 9101, ..Default::default() };
    h.handle("octo-org/app", "ru-linux", &Message { message_id: 2, job_started: vec![started], ..Default::default() });
    mark_started(&h.cfg, "box-1");
    spare
}

/// Has GitHub say PR #184 was cancelled before its runner in box-2 took it.
pub fn cancel_before_runner(h: &Harness) {
    let completed = JobMessage {
        result: "canceled".into(),
        runner_request_id: 9102,
        job_display_name: "affected-journeys".into(),
        finish_time: h.now(),
        message_type: MESSAGE_TYPE_JOB_COMPLETED.into(),
        ..Default::default()
    };
    h.handle("octo-org/app", "ru-linux", &Message { message_id: 3, job_completed: vec![completed], ..Default::default() });
}

/// Ends box-2's runner before its job reached it, as a virtual machine that
/// never boots does, and a scale set's next poll places the suite there
/// before tidy runs.
fn take_freed_place_before_tidy(h: &Harness) {
    h.launcher.give_up("box-2");
    h.advance(Duration::from_secs(5));
    h.place();
    h.want(&[("box-1", VERIFY), ("box-2", SUITE), ("box-large", PUBLISH)]);
}

// Intent (README, "How it works" and "Order rules"): a second machine shares
// the one line of waiting jobs rather than keeping its own: whichever
// machine's place frees takes the next job in line. Two machines, each with
// one place, take the recorded jobs in the very order one place on one
// machine takes them (take_order), alternating.
#[test]
fn second_machine_shares_one_line() {
    use super::rules::{one_place, request_name, take_order, want_order};
    let d1 = tmp();
    let want = take_order(one_place(d1.path(), &[]), |_| {});
    let dir = tmp();
    let mut cfg = one_place(dir.path(), &[]);
    let mut spare = cfg.machines[0].clone();
    (spare.name, spare.ssh) = ("spare".into(), strings(&["ci@spare"]));
    spare.places.truncate(1);
    spare.places[0].id = "spare-1".into();
    cfg.machines.push(spare);
    let f = load_fixture("waiting-jobs.json");
    let mut busy = HashMap::from([("box-1", occupy(&cfg, "box-1")), ("spare-1", occupy(&cfg, "spare-1"))]);
    let h = fresh(cfg);
    h.set_now(f.now);
    h.feed(&f);
    let mut got = Vec::new();
    let mut took: HashMap<&str, usize> = HashMap::new();
    // Records the job place id runs now, if any.
    let mut record = |id: &'static str| -> bool {
        let Some(job) = h.placed().get(id).cloned() else {
            return false;
        };
        got.push(format!("{job} {}", request_name(h.run(id).request_id)));
        *took.entry(id).or_default() += 1;
        true
    };
    for id in ["box-1", "spare-1"] {
        busy.remove(id);
        h.place();
        record(id);
    }
    let mut running = true;
    while running {
        running = false;
        for id in ["box-1", "spare-1"] {
            if h.placed().contains_key(id) {
                h.finish_and_place(id);
                running = record(id) || running;
            }
        }
    }
    want_order(&got, &want);
    assert!(took["box-1"] >= 2 && took["spare-1"] >= 2, "the machines took {took:?} jobs, want both to take several");
    assert_eq!(h.waiting(), 0, "jobs still waiting");
}
