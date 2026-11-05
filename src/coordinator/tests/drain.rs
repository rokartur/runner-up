use std::fs::File;
use std::time::Duration;

use super::service::{PR184, PUBLISH, RELEASE, SUITE, VERIFY};
use super::*;
use crate::coordinator::drain::{
    DRAIN_ADMITTING, DRAIN_DONE, DRAIN_DRAINING, DRAIN_RUNNING, DrainRequest, DrainState, drain_dir,
};
use crate::scaleset::{JobMessage, MESSAGE_TYPE_JOB_STARTED};

/// Publishes a drain as `runner-up drain` does, holding its live lock until
/// the returned file is dropped.
fn enqueue_drain(cfg: &Config, r: DrainRequest) -> File {
    let dir = drain_dir(cfg, &r.id);
    std::fs::create_dir_all(&dir).unwrap();
    let live = lock::busy(&dir.join("live.lock")).unwrap().expect("could not hold the drain");
    config::write_json(&dir.join("request.json"), &r).unwrap();
    live
}

fn want_drain(cfg: &Config, id: &str, phase: &str, detail: &str) {
    let st: DrainState = config::read_json(&drain_dir(cfg, id).join("state.json")).unwrap();
    assert!(st.phase == phase && st.detail == detail, "drain {id} is {}: {}\nwant {phase}: {detail}", st.phase, st.detail);
}

/// Records that the job named job reached place id's runner, as the place
/// runner does when the runner prints it.
fn job_reached(cfg: &Config, id: &str, job: &str) {
    let path = cfg.place_dir(id).join("place.json");
    let mut run: PlaceRun = config::read_json(&path).unwrap();
    run.started = true;
    run.ran_job = job.into();
    config::write_json(&path, &run).unwrap();
}

/// GitHub's message that runner took job, one of its scale set's.
fn started(runner: &str, job: &JobMessage) -> Message {
    let mut m = job.clone();
    m.message_type = MESSAGE_TYPE_JOB_STARTED.into();
    m.runner_name = runner.into();
    Message { job_started: vec![m], ..Default::default() }
}

fn request(places: &[&str], jobs: &[&str], repo: &str, queued: Time) -> DrainRequest {
    DrainRequest {
        id: "release".into(),
        label: "release check".into(),
        places: strings(places),
        jobs: strings(jobs),
        repo: repo.into(),
        minutes: 90,
        queued,
    }
}

// Intent (README, "Draining places"): a drain keeps its places from new jobs
// while their running jobs finish, then admits only the named pair there, and
// gives the places back at once when the pair ends. A place it does not name
// keeps placing throughout, a named job waits for the drained places rather
// than start there, no running job is stopped, and a coordinator restart keeps
// the drain. This test owns the drain detail sentences; tests elsewhere read a
// drain's phase and places.
#[test]
fn drain_lets_running_jobs_finish_admits_the_pair_then_resumes() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_box(dir.path());
    // Every place also serves ru-proof, the pool of a later recorded job.
    cfg.pools.push(pool("ru-proof"));
    for p in &mut cfg.machines[0].places {
        p.pools.push("ru-proof".into());
    }
    let launcher = FakeLauncher::new();
    let mut h = new_harness(cfg, launcher.clone(), Sets::new());
    let cfg = h.cfg.clone();
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);

    let _live = enqueue_drain(&cfg, request(&["box-1", "box-2"], &["suite", "affected-journeys"], "", h.now()));
    h.feed_range(&f, 1, f.messages.len());
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PUBLISH)]);
    want_drain(&cfg, "release", DRAIN_DRAINING, r#"box-1 runs octo-org/app "verify / linux"; box-2 runs octo-org/app "affected-journeys""#);

    // The large place serves ru-linux, and with no drain the hour-old suite
    // would take it next. Named, the suite waits for the drained places.
    h.finish_and_place("box-large");
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", RELEASE)]);
    job_reached(&cfg, "box-large", "Release");

    // A drained place stays free while the other still runs its job.
    h.finish_and_place("box-1");
    h.want(&[("box-2", PR184), ("box-large", RELEASE)]);
    want_drain(&cfg, "release", DRAIN_DRAINING, r#"box-2 runs octo-org/app "affected-journeys""#);

    let at = h.now();
    let sets = h.sets.clone();
    drop(h);
    h = new_harness((*cfg).clone(), launcher.clone(), sets);
    h.set_now(at.add(Duration::from_secs(60)));
    h.place();
    h.want(&[("box-2", PR184), ("box-large", RELEASE)]);

    // Both places are free at once: they take the pair, the suite and PR
    // #186's affected-journeys, and nothing else.
    h.finish_and_place("box-2");
    h.want(&[("box-1", SUITE), ("box-2", PR184), ("box-large", RELEASE)]);
    assert_eq!(h.run("box-2").request_id, 9104, "want box-2 to run PR #186's 9104");
    want_drain(&cfg, "release", DRAIN_ADMITTING, r#""suite" starting in box-1, "affected-journeys" starting in box-2"#);

    job_reached(&cfg, "box-1", "suite");
    job_reached(&cfg, "box-2", "affected-journeys");
    h.place();
    want_drain(&cfg, "release", DRAIN_RUNNING, r#""suite" runs in box-1, "affected-journeys" runs in box-2"#);

    // A later job the drained places serve waits while the pair runs, even
    // once one of the pair has ended.
    let later = load_fixture("assigned-started.json");
    h.feed_range(&later, 1, 2);
    h.finish_and_place("box-1");
    h.want(&[("box-2", PR184), ("box-large", RELEASE)]);
    want_drain(&cfg, "release", DRAIN_RUNNING, r#""suite" ran in box-1, "affected-journeys" runs in box-2"#);

    // The pair has ended: the later job takes a freed place in the same pass.
    h.finish_and_place("box-2");
    h.want(&[("box-1", "octo-org/app linux"), ("box-large", RELEASE)]);
    want_drain(&cfg, "release", DRAIN_DONE, r#""suite" ran in box-1, "affected-journeys" ran in box-2; placement resumed"#);
    assert!(launcher.st().stopped.is_empty(), "stopped {:?}, want no running job stopped", launcher.st().stopped);
}

// Intent (README, "Draining places"): a drain whose caller stops, or reaches
// its time limit, gives its places back at the next placement, even mid-drain,
// and the jobs running there carry on.
#[test]
fn drain_ended_by_its_caller_resumes_placement_at_once() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 0, 2);
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PUBLISH)]);
    let live = enqueue_drain(&h.cfg, request(&["box-1", "box-2"], &["suite"], "", h.now()));
    h.feed_range(&f, 2, 3);
    h.finish_and_place("box-1");
    h.want(&[("box-2", PR184), ("box-large", PUBLISH)]);

    drop(live);
    h.advance(Duration::from_secs(5));
    h.place();
    h.want(&[("box-1", SUITE), ("box-2", PR184), ("box-large", PUBLISH)]);
    assert!(!drain_dir(&h.cfg, "release").exists(), "the ended drain is still kept");
    assert!(h.launcher.st().stopped.is_empty(), "stopped {:?}, want no running job stopped", h.launcher.st().stopped);
}

// Intent (README, "Draining places": GitHub picks which of a scale set's jobs
// a runner takes): each name admits one job at a time, a runner the drain
// started may take the other of the pair, and the pair counts as run wherever
// in the drained places GitHub put each job.
#[test]
fn drain_admits_one_job_per_name_and_counts_githubs_pick() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 1, 3);
    h.want(&[("box-1", SUITE), ("box-large", PUBLISH)]);
    let _live = enqueue_drain(
        &h.cfg,
        request(&["box-1", "box-2"], &["verify / linux", "affected-journeys"], "octo-org/app", h.now()),
    );
    // PR #184's and PR #186's jobs share a name: one is admitted at a time.
    h.feed_range(&f, 0, 1);
    h.feed_range(&f, 4, 5);
    h.want(&[("box-1", SUITE), ("box-large", PUBLISH)]);

    h.finish_and_place("box-1");
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PUBLISH)]);
    assert_eq!(h.run("box-2").request_id, 9102, "want box-2 to run PR #184's 9102");

    // GitHub gives box-2's runner the verify job, and box-1's runner PR
    // #184's job.
    let jobs = &f.messages[0].message.job_available;
    let (box1, box2) = (h.run("box-1"), h.run("box-2"));
    h.handle("octo-org/app", "ru-linux", &started(&box2.runner_name, &jobs[0]));
    h.handle("octo-org/app", "ru-linux", &started(&box1.runner_name, &jobs[1]));
    want_drain(&h.cfg, "release", DRAIN_RUNNING, r#""verify / linux" runs in box-2, "affected-journeys" runs in box-1"#);

    // PR #186's job, named the same, waits for the drain to end.
    h.launcher.finish("box-1");
    h.finish_and_place("box-2");
    h.want(&[("box-1", PR184), ("box-large", PUBLISH)]);
    assert_eq!(h.run("box-1").request_id, 9104, "want box-1 to run PR #186's 9104 once the drain ended");
}

// Intent (README, "Draining places"): a named job GitHub gives a runner outside
// the drain will not wait again, so the drain stops waiting for it rather than
// hold its places to its time limit.
#[test]
fn drain_stops_waiting_for_a_named_job_that_started_elsewhere() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.feed_range(&f, 4, 5);
    h.want(&[("box-1", VERIFY), ("box-2", PR184), ("box-large", PR184)]);
    let _live = enqueue_drain(&h.cfg, request(&["box-1"], &["affected-journeys"], "octo-org/app", h.now()));
    h.place();

    // GitHub gives the large place's runner PR #184's job.
    let jobs = &f.messages[0].message.job_available;
    h.handle("octo-org/app", "ru-linux", &started(&h.run("box-large").runner_name, &jobs[1]));
    want_drain(&h.cfg, "release", DRAIN_DRAINING, r#"box-1 runs octo-org/app "verify / linux""#);
    h.finish_and_place("box-1");
    want_drain(&h.cfg, "release", DRAIN_DONE, r#""affected-journeys" started in box-large, outside the drain; placement resumed"#);
}

// Intent (README, "Draining places": on a machine whose places share its cores
// and memory, the named jobs keep room ahead of other work): a drained place's
// named job takes the room its drain freed, even when an older job elsewhere
// on the machine waits for more room.
#[test]
fn drained_jobs_take_the_room_ahead_of_other_work() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_box(dir.path());
    // Room for the two small places, or for the large one.
    cfg.machines[0].cores = 5;
    cfg.machines[0].room_cores = 1;
    let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 0, 2);
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);
    let _live = enqueue_drain(&h.cfg, request(&["box-1"], &["suite"], "", h.now()));
    h.feed_range(&f, 2, 3);

    // The publish job, older, waits for the large place's 4 cores.
    h.finish_and_place("box-1");
    h.want(&[("box-1", SUITE), ("box-2", PR184)]);
}
