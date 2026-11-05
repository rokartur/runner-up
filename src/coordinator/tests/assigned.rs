// Intent (README, "How it works" and "When GitHub closes a job"): GitHub sends
// most jobs as JobAssigned messages with runner request 0 and no JobAvailable
// message at all. A job GitHub assigns to a scale set gets a runner in a free
// place, keeps its place in line and its starting runner while GitHub takes the
// assignment back and assigns it again, lets go of both once it is cancelled,
// and goes back in line when GitHub gives its runner another of the set's jobs
// or its runner ends without it.

use std::time::Duration;

use super::service::{PR184, SUITE, VERIFY};
use super::*;
use crate::plan;
use crate::scaleset::MESSAGE_TYPE_JOB_STARTED;

/// The recorded job, as the place runs it.
pub const PROOF_JOB: &str = "octo-org/app linux";

/// One machine whose places serve the scale set the messages were recorded
/// from.
pub fn proof_box(dir: &Path, ids: &[&str]) -> Config {
    let mut m = config::Machine { name: "box".into(), ssh: strings(&["ci@box"]), ..Default::default() };
    for id in ids {
        m.places.push(Place {
            id: id.to_string(),
            provider: config::PROVIDER_DOCKER.into(),
            pools: strings(&["ru-proof"]),
            cores: 2,
            memory_mb: 4096,
            ..Default::default()
        });
    }
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        github: config::GitHub { repos: strings(&["octo-org/app", "octo-user/tool"]), ..Default::default() },
        pools: vec![pool("ru-proof")],
        machines: vec![m],
        ..Default::default()
    }
}

fn fresh(cfg: Config) -> Harness {
    new_harness(cfg, FakeLauncher::new(), Sets::new())
}

#[test]
fn assigned_job_gets_a_runner() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let h = fresh(proof_box(dir.path(), &["box-1", "box-2"]));
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", PROOF_JOB)]);
    // GitHub already gave the job to the scale set: there is nothing to
    // acquire, and request 0 names no job.
    assert!(h.set("octo-org/app", "ru-proof").acquired.is_empty());
}

#[test]
fn reassigned_job_keeps_its_place_in_line() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let cfg = proof_box(dir.path(), &["box-1", "box-2"]);
    let h = fresh(cfg);
    let one = occupy(&h.cfg, "box-1");
    let _two = occupy(&h.cfg, "box-2");

    // Every place is busy for the ten minutes GitHub assigns the job three
    // times: it stays one waiting job, in line from its first assignment.
    h.feed_range(&f, 0, 5);
    {
        let st = h.svc.lock();
        assert_eq!(st.waiting.len(), 1, "waiting jobs: {:?}", st.waiting);
        for j in st.waiting.values() {
            assert_eq!(j.queue_time, f.messages[0].at, "the job's place in line");
        }
    }

    // A place frees: the job gets its runner.
    drop(one);
    h.advance(Duration::from_secs(60));
    h.place();
    h.want(&[("box-1", PROOF_JOB), ("box-2", SUITE)]);
}

#[test]
fn reassignment_keeps_the_starting_runner() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let h = fresh(proof_box(dir.path(), &["box-1", "box-2"]));
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", PROOF_JOB)]);

    // Its place still starting, GitHub takes the assignment back and assigns
    // the job again, twice. The starting runner will take it: no second
    // runner starts, and the first one is not stopped.
    h.feed_range(&f, 1, 5);
    h.advance(Duration::from_secs(60));
    h.place();
    h.want(&[("box-1", PROOF_JOB)]);
    assert!(h.launcher.st().stopped.is_empty());
}

#[test]
fn cancelled_assigned_job_lets_go_of_its_place() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let h = fresh(proof_box(dir.path(), &["box-1", "box-2"]));

    // The job gets a runner, is taken back and assigned again twice, then
    // the run is cancelled before the runner took it. The spare runner is
    // stopped.
    h.feed(&f);
    assert_eq!(h.launcher.st().stopped, ["box-1"], "want the spare runner in box-1 stopped");

    // Cancelled while it waited for a place, it is forgotten: a freed place
    // starts nothing for it.
    let dir2 = tempfile::tempdir().unwrap();
    let cfg2 = proof_box(dir2.path(), &["box-1", "box-2"]);
    let h2 = fresh(cfg2);
    let one = occupy(&h2.cfg, "box-1");
    let _two = occupy(&h2.cfg, "box-2");
    h2.feed(&f);
    drop(one);
    h2.advance(Duration::from_secs(60));
    h2.place();
    h2.want(&[("box-2", SUITE)]);
    assert_eq!(h2.waiting(), 0);
}

#[test]
fn runner_that_takes_another_job_hands_its_own_back() {
    let takebacks = load_fixture("assigned-takebacks.json");
    let proof = load_fixture("assigned-started.json");
    let dir = tempfile::tempdir().unwrap();
    let cfg = proof_box(dir.path(), &["proof-a"]);
    let h = fresh(cfg);
    let first = &takebacks.messages[0].message.job_assigned[0];
    let first_key = plan::run_key("octo-org/app", "ru-proof", first.workflow_run_id, &first.job_display_name);
    let started = &proof.messages[2].message.job_started[0];

    // The first recorded job and the second run's job wait for the one
    // place, the first one first in line. The place frees the moment the
    // second run's job is assigned, and its runner is started for the first.
    let busy = occupy(&h.cfg, "proof-a");
    h.feed_range(&takebacks, 0, 1);
    h.feed_range(&proof, 1, 2);
    drop(busy);
    h.place();
    let run = h.run("proof-a");
    assert!(h.svc.lock().acquired.contains_key(&first_key), "the first job is not acquired");
    assert_eq!(run.runner_name, started.runner_name);

    // GitHub gives that runner the second run's job, as it recorded: the
    // first job waits again, still first in line.
    h.feed_range(&proof, 2, 3);
    mark_started(&h.cfg, "proof-a");
    let queued = h.svc.lock().waiting.get(&first_key).map(|j| j.queue_time);
    assert_eq!(queued, Some(takebacks.messages[0].at), "want the first job in line from its first assignment");

    // The second run's job ends; the first job gets the place.
    h.feed_range(&proof, 3, 4);
    h.finish_and_place("proof-a");
    let place = h.svc.lock().acquired.get(&first_key).map(|a| a.place.clone());
    assert_eq!(place.as_deref(), Some("proof-a"));
    h.want(&[("proof-a", PROOF_JOB)]);
}

// Intent (the fix that counts a runner whose job another took as free at
// hand-back, so no surplus runner idles to the start timeout): the runner
// started for verify takes PR #184's job, whose own runner is still waiting.
// That waiting runner is free now and takes verify: no third runner starts in
// the free large place, and once it takes verify nothing waits.
#[test]
fn runner_whose_job_another_took_serves_the_job_handed_back() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = fresh(one_box(dir.path()));
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);
    let jobs = &f.messages[0].message.job_available;
    let start = |job: usize, place: &str| {
        let mut m = jobs[job].clone();
        m.message_type = MESSAGE_TYPE_JOB_STARTED.into();
        m.runner_name = h.run(place).runner_name;
        h.advance(Duration::from_secs(1));
        h.handle("octo-org/app", "ru-linux", &Message { job_started: vec![m], ..Default::default() });
        mark_started(&h.cfg, place);
    };

    // GitHub gives box-1's runner PR #184's job.
    start(1, "box-1");
    let verify_key = plan::request_key("octo-org/app", "ru-linux", jobs[0].runner_request_id);
    {
        let st = h.svc.lock();
        let place = st.acquired.get(&verify_key).map(|a| a.place.as_str());
        assert!(place == Some("box-2") && st.waiting.is_empty(), "want verify on box-2's runner and nothing waiting");
    }
    h.place();
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);

    // box-2's runner takes verify: no job is left without a runner.
    start(0, "box-2");
    {
        let st = h.svc.lock();
        assert!(st.acquired.is_empty() && st.waiting.is_empty(), "want both jobs running");
    }
    h.place();
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);
}

// GitHub assigns a job again and takes back its previous assignment in one
// message; the take-back names the earlier assignment's job id and must not
// hold the new one, or the job is never placed when its place frees.
#[test]
fn take_back_of_an_earlier_assignment_leaves_the_new_one() {
    let f = load_fixture("shared-reassigned.json");
    let dir = tempfile::tempdir().unwrap();
    let h = fresh(proof_box(dir.path(), &["proof-b"]));
    let history = &f.messages[2].message.job_assigned[0];
    let history_key = plan::run_key("octo-org/app", "ru-proof", history.workflow_run_id, &history.job_display_name);

    // tool's job takes the one place.
    h.feed_range(&f, 0, 2);
    mark_started(&h.cfg, "proof-b");
    h.want(&[("proof-b", "octo-user/tool suite / Full test suite")]);

    // The history job is assigned, then assigned again twice, each time
    // with the take-back of the assignment before it: it waits, in line
    // from its first assignment, held by none of those take-backs.
    h.feed_range(&f, 2, 5);
    {
        let st = h.svc.lock();
        let j = st.waiting.get(&history_key).expect("the history job is not waiting");
        assert!(!j.taken_back && j.queue_time == f.messages[2].at, "want the history job in line from its first assignment");
    }

    // tool's job ends: the history job gets the place, and the job's
    // successful run teaches its estimate.
    h.feed_range(&f, 5, 6);
    h.finish_and_place("proof-b");
    h.want(&[("proof-b", "octo-org/app history")]);
    let learned = h.svc.lock().timings["octo-user/tool|ru-proof|octo-user/tool/.github/workflows/suite.yml|suite / Full test suite"];
    assert!(learned >= Duration::from_secs(3600) && learned <= Duration::from_secs(70 * 60), "learned {learned:?}");
}

// GitHub keeps a job queued and assigned to the scale set once the runner
// started for it fails to come up, and sends nothing more. A job whose runner
// ends without starting it waits again, in line from its first assignment,
// and the next free place starts a runner for it: when its place gives up
// at the boot timeout, and when its runner outlives the start and boot
// allowance and a placement runs before tidy.
#[test]
fn job_whose_runner_never_started_waits_again() {
    let cases: [(&str, fn(&Config) -> Duration, bool); 2] = [
        ("place gave up, tidy first", |cfg| cfg.seconds(cfg.boot_timeout_seconds), true),
        (
            "runner outlives the allowance, placement first",
            |cfg| cfg.seconds(cfg.start_timeout_seconds + cfg.boot_timeout_seconds) + Duration::from_secs(60),
            false,
        ),
    ];
    for (name, lives, tidy) in cases {
        let f = load_fixture("assigned-takebacks.json");
        let dir = tempfile::tempdir().unwrap();
        let cfg = proof_box(dir.path(), &["box-1", "box-2"]);
        let h = fresh(cfg);
        let _two = occupy(&h.cfg, "box-2");
        h.feed_range(&f, 0, 1);
        h.want(&[("box-1", PROOF_JOB), ("box-2", SUITE)]);
        let first = h.run("box-1");

        // The runner ends without a job; GitHub sends nothing more.
        h.advance(lives(&h.cfg));
        if !tidy {
            // Its allowance passes while it still lives.
            h.place();
        }
        h.launcher.give_up("box-1");
        if tidy {
            h.tidy();
        }
        h.place();
        h.want(&[("box-1", PROOF_JOB), ("box-2", SUITE)]);

        let assigned = &f.messages[0].message.job_assigned[0];
        let key = plan::run_key("octo-org/app", "ru-proof", assigned.workflow_run_id, &assigned.job_display_name);
        {
            let st = h.svc.lock();
            let a = st.acquired.get(&key);
            assert!(
                a.is_some_and(|a| a.place == "box-1" && a.job.queue_time == f.messages[0].at),
                "{name}: want the job placed again in box-1, in line from its first assignment"
            );
        }
        assert_ne!(h.run("box-1").runner_name, first.runner_name, "{name}: box-1 still names the first runner");
    }
}
