// Intent (README, "When GitHub closes a job"): GitHub can stop mentioning a job
// it assigned to the scale set without ever sending its cancellation, and the
// coordinator kept such a job waiting for a day, starting runners for it while
// it read as assigned. A waiting job GitHub has gone quiet about is asked about
// by its run, with few reads (at most ten runs at a time and sixty an hour),
// and forgotten once GitHub says the run is closed; a job of an open run, an
// unanswered question, a job GitHub assigns again meanwhile, and a job with a
// runner are never forgotten.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::assigned::{PROOF_JOB, proof_box};
use super::service::SUITE;
use super::*;
use crate::coordinator::GithubApi;
use crate::coordinator::closedruns::{
    QUIET_ASSIGNED, QUIET_TAKEN_BACK, RECHECK_ASSIGNED, RECHECK_TAKEN_BACK, RUN_READS_PER_HOUR, RUN_READS_PER_PASS,
    RUN_READS_WINDOW,
};
use crate::coordinator::queue::job_of;
use crate::plan;
use crate::testserver::{Response, Server};

/// The recorded job's run.
const PROOF_RUN: i64 = 39463703013;
const MINUTE: Duration = Duration::from_secs(60);

/// Stands in for GitHub's answer about runs and counts the questions.
#[derive(Default)]
struct RunAnswers {
    closed: bool,
    err: Option<String>,
    asked: Vec<i64>,
    during: Option<Box<dyn Fn() + Send>>,
}

type Answers = Arc<Mutex<RunAnswers>>;

fn answers(closed: bool, err: Option<&str>) -> Answers {
    Arc::new(Mutex::new(RunAnswers { closed, err: err.map(String::from), ..Default::default() }))
}

fn asked(a: &Answers) -> Vec<i64> {
    a.lock().unwrap().asked.clone()
}

fn read_runs(h: &Harness, a: &Answers) {
    let a = a.clone();
    h.svc.lock().run_reader = Some(Arc::new(move |_: &Ctx, repo: &str, id: i64| {
        let mut r = a.lock().unwrap();
        r.asked.push(id);
        if repo != "octo-org/app" {
            return Err(format!("unexpected repository {repo}"));
        }
        if let Some(during) = &r.during {
            during();
        }
        match &r.err {
            Some(e) => Err(e.clone()),
            None => Ok(r.closed),
        }
    }));
}

/// Feeds the recorded messages [0, to) while both places are busy, so the job
/// waits. Returns the harness, the time of the last message, box-1's lock,
/// whose drop frees it, and box-2's.
fn quiet_job(to: usize, a: &Answers, dir: &Path) -> (Harness, Time, File, File) {
    let f = load_fixture("assigned-takebacks.json");
    let h = new_harness(proof_box(dir, &["box-1", "box-2"]), FakeLauncher::new(), Sets::new());
    read_runs(&h, a);
    let one = occupy(&h.cfg, "box-1");
    let two = occupy(&h.cfg, "box-2");
    h.feed_range(&f, 0, to);
    assert_eq!(h.waiting(), 1, "want the recorded job waiting");
    (h, f.messages[to - 1].at, one, two)
}

#[test]
fn job_of_a_run_github_closed_is_forgotten() {
    let cases = [
        // The job is assigned, and its cancellation never reaches the
        // coordinator, as when a session ends before it is delivered. GitHub
        // takes an assigned job no runner took back after five minutes, so
        // ten quiet minutes are suspect.
        ("assigned, its cancellation lost", 5, QUIET_ASSIGNED),
        // GitHub took the assignment back, then the run was cancelled: GitHub
        // assigns it to nothing and says nothing more. A taken-back job of a
        // long queue can wait quietly for its next assignment, and no runner
        // starts for it, so only an hour is suspect.
        ("taken back, then cancelled", 4, QUIET_TAKEN_BACK),
    ];
    for (name, to, quiet) in cases {
        let dir = tempfile::tempdir().unwrap();
        let a = answers(true, None);
        let (h, last, one, _two) = quiet_job(to, &a, dir.path());

        // Just short of its quiet time, nothing is asked.
        h.set_now(last.add(quiet - MINUTE));
        h.place();
        assert!(asked(&a).is_empty() && h.waiting() == 1, "{name}: asked {:?}, want no question and the job waiting", asked(&a));

        // At its quiet time: one question about its run, which GitHub says is
        // closed, and the job is forgotten, also across a restart.
        h.set_now(last.add(quiet));
        h.place();
        assert!(
            asked(&a) == [PROOF_RUN] && h.waiting() == 0,
            "{name}: asked {:?}, waiting {}; want one question about run {PROOF_RUN} and nothing waiting",
            asked(&a),
            h.waiting()
        );
        let restarted = Service::new(h.cfg.clone(), Logger::discard(), h.launcher.clone(), channel().0).unwrap();
        assert_eq!(restarted.lock().waiting.len(), 0, "{name}: after a restart something waits");

        // A freed place starts no runner for it.
        drop(one);
        h.place();
        h.want(&[("box-2", SUITE)]);
    }
}

#[test]
fn job_of_an_open_run_waits_and_is_asked_about_rarely() {
    let unanswered = Some("GET /repos/octo-org/app/actions/runs/39463703013 returned HTTP 403");
    let cases = [
        ("assigned, GitHub says the run is open", None, 5, QUIET_ASSIGNED, RECHECK_ASSIGNED),
        ("assigned, GitHub does not answer", unanswered, 5, QUIET_ASSIGNED, RECHECK_ASSIGNED),
        ("taken back, GitHub says the run is open", None, 4, QUIET_TAKEN_BACK, RECHECK_TAKEN_BACK),
        ("taken back, GitHub does not answer", unanswered, 4, QUIET_TAKEN_BACK, RECHECK_TAKEN_BACK),
    ];
    for (name, err, to, quiet, recheck) in cases {
        let dir = tempfile::tempdir().unwrap();
        let a = answers(err.is_some(), err);
        let (h, last, one, _two) = quiet_job(to, &a, dir.path());
        h.set_now(last.add(quiet));
        h.place();
        assert!(asked(&a).len() == 1 && h.waiting() == 1, "{name}: asked {:?}, want one question and the job waiting", asked(&a));

        // Every placement each minute asks nothing more until the recheck
        // time is up.
        for _ in 0..recheck.as_secs() / 60 - 1 {
            h.advance(MINUTE);
            h.place();
        }
        assert_eq!(asked(&a).len(), 1, "{name}: asked more than once within {recheck:?}");
        h.advance(MINUTE);
        h.place();
        assert_eq!(asked(&a).len(), 2, "{name}: want a second question after {recheck:?}");

        // The job is kept: once GitHub assigns it, as it recorded, it gets the
        // freed place.
        drop(one);
        if to < 5 {
            let msg = load_fixture("assigned-takebacks.json").messages[4].message.clone();
            h.handle("octo-org/app", "ru-proof", &msg);
        }
        h.place();
        h.want(&[("box-1", PROOF_JOB), ("box-2", SUITE)]);
    }
}

#[test]
fn job_assigned_again_while_its_run_is_asked_about_waits() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let a = answers(true, None);
    let (h, last, one, _two) = quiet_job(5, &a, dir.path());
    // GitHub answers about the run as it was, closed; meanwhile the run is run
    // again and its job assigned anew, the way GitHub assigns it after a
    // take-back.
    let again = f.messages[2].message.job_assigned[0].clone();
    let svc = h.svc.clone();
    a.lock().unwrap().during = Some(Box::new(move || {
        svc.lock().wait(job_of("octo-org/app", "ru-proof", &again, true, last.add(QUIET_ASSIGNED + Duration::from_secs(1))));
    }));
    h.set_now(last.add(QUIET_ASSIGNED));
    h.place();
    assert!(
        asked(&a).len() == 1 && h.waiting() == 1,
        "asked {:?}, waiting {}; want one question and the job assigned again waiting",
        asked(&a),
        h.waiting()
    );
    drop(one);
    h.advance(MINUTE);
    h.place();
    h.want(&[("box-1", PROOF_JOB), ("box-2", SUITE)]);
}

#[test]
fn job_with_a_runner_is_never_asked_about() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let a = answers(true, None);
    let h = new_harness(proof_box(dir.path(), &["box-1", "box-2"]), FakeLauncher::new(), Sets::new());
    read_runs(&h, &a);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", PROOF_JOB)]);

    // Its runner lives, waiting for the job, past the quiet minutes.
    h.set_now(f.messages[0].at.add(QUIET_ASSIGNED + MINUTE));
    h.place();
    let first = &f.messages[0].message.job_assigned[0];
    let key = plan::run_key("octo-org/app", "ru-proof", first.workflow_run_id, &first.job_display_name);
    assert!(
        h.svc.lock().acquired.contains_key(&key) && asked(&a).is_empty(),
        "asked {:?}; want the job kept with its runner and no question",
        asked(&a)
    );
}

// GitHub's run record says whether the run is closed; a record of another run
// or repository answers nothing.
#[test]
fn run_closed_reads_the_runs_status() {
    let record = Arc::new(Mutex::new(("completed", "octo-org/app")));
    let served = record.clone();
    let server = Server::start(move |r| {
        if r.path != "/repos/octo-org/app/actions/runs/39463703013" || r.header("Authorization") != "Bearer local-only" {
            return Response::new(404, "unexpected");
        }
        let (status, repo) = *served.lock().unwrap();
        // The fields of GitHub's workflow run record the reader uses.
        Response::json(
            200,
            &serde_json::json!({"id": PROOF_RUN, "status": status, "conclusion": "cancelled", "repository": {"full_name": repo}}),
        )
    });
    let mut api = GithubApi::new(Arc::new(|_: &Ctx| Ok("local-only".to_string())));
    api.base = server.url.clone();
    let ctx = Ctx::background();
    for (status, repo, closed, err) in [
        ("completed", "octo-org/app", true, false),
        ("queued", "octo-org/app", false, false),
        ("in_progress", "octo-org/app", false, false),
        ("completed", "octo-org/other", false, true),
    ] {
        *record.lock().unwrap() = (status, repo);
        let got = api.run_closed(&ctx, "octo-org/app", PROOF_RUN);
        assert!(
            got.as_ref().is_ok_and(|c| *c == closed) || (err && got.is_err()),
            "status {status} of {repo}: got {got:?}"
        );
    }
    assert!(api.run_closed(&ctx, "octo-org/app", 1).is_err(), "a run GitHub has no record of read as an answer");
}

// However many runs are quiet, a pass reads ten and an hour sixty: here the
// recorded job's message stands for one job of each of seventy runs, all
// quiet, all open.
#[test]
fn run_reads_are_bounded_per_pass_and_per_hour() {
    let f = load_fixture("assigned-takebacks.json");
    let dir = tempfile::tempdir().unwrap();
    let a = answers(false, None);
    let h = new_harness(proof_box(dir.path(), &["box-1", "box-2"]), FakeLauncher::new(), Sets::new());
    read_runs(&h, &a);
    let _busy = [occupy(&h.cfg, "box-1"), occupy(&h.cfg, "box-2")];
    h.set_now(f.messages[0].at);
    for i in 0..70 {
        let mut msg = f.messages[0].message.clone();
        msg.job_assigned[0].workflow_run_id = PROOF_RUN + i;
        h.handle("octo-org/app", "ru-proof", &msg);
    }
    h.advance(QUIET_ASSIGNED);
    h.place();
    assert_eq!(asked(&a).len(), RUN_READS_PER_PASS, "one pass asked about the wrong number of runs");
    let start = h.now();
    while h.now().sub(start) < crate::go::time::td(RUN_READS_WINDOW - MINUTE) {
        h.advance(MINUTE);
        h.place();
        h.place();
    }
    assert_eq!(asked(&a).len(), RUN_READS_PER_HOUR, "an hour asked the wrong number of times");
    h.set_now(start.add(RUN_READS_WINDOW));
    h.place();
    assert_eq!(asked(&a).len(), RUN_READS_PER_HOUR + RUN_READS_PER_PASS, "the next hour's first pass asked the wrong number of times");
    assert_eq!(h.waiting(), 70, "want all seventy jobs waiting");
}

// Intent (README.md, "When GitHub closes a job"): the README's quiet times,
// recheck times and read bounds are the coordinator's. This test owns those
// numbers; the other tests here use the constants.
#[test]
fn closed_run_times_are_the_readmes() {
    let readme = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let text = readme.split_whitespace().collect::<Vec<_>>().join(" ");
    let number = |n: u64| match n {
        1 => "one".to_string(),
        5 => "five".into(),
        10 => "ten".into(),
        15 => "fifteen".into(),
        30 => "thirty".into(),
        60 => "sixty".into(),
        n => n.to_string(),
    };
    let spoken = |d: Duration| {
        let s = d.as_secs();
        if s == 3600 {
            "an hour".to_string()
        } else if s % 3600 == 0 {
            format!("{} hours", number(s / 3600))
        } else {
            format!("{} minutes", number(s / 60))
        }
    };
    let every = |d: Duration| format!("every {}", spoken(d).trim_start_matches("an "));
    for want in [
        format!(
            "said nothing for {} about a job it assigned, or for {} about a job it took back,",
            spoken(QUIET_ASSIGNED),
            spoken(QUIET_TAKEN_BACK)
        ),
        format!("reads each such run again {}, or {}, while the job stays quiet,", every(RECHECK_ASSIGNED), every(RECHECK_TAKEN_BACK)),
        format!(
            "reads at most {} runs at a time and {} {}.",
            number(RUN_READS_PER_PASS as u64),
            number(RUN_READS_PER_HOUR as u64),
            spoken(RUN_READS_WINDOW)
        ),
    ] {
        assert!(text.contains(&want), "the README does not say {want:?}");
    }
}
