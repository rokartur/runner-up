use std::process::{Child, Stdio};
use std::time::Duration;

use serde_json::json;

use super::e2e::{E2e, alive, job_message, journeys_job, recorded_job, verify_job};
use super::journeys::skip_if_short;
use crate::config::{self, Config, Machine, PROVIDER_DOCKER, Place, Pool};
use crate::coordinator::{BusyReason, DRAIN_DONE, DRAIN_DRAINING, DRAIN_RUNNING, drains};

fn docker(id: &str, pools: &[&str]) -> Place {
    Place {
        id: id.into(),
        provider: PROVIDER_DOCKER.into(),
        pools: pools.iter().map(|p| p.to_string()).collect(),
        cores: 2,
        memory_mb: 4096,
        ..Default::default()
    }
}

/// Gives the e2e machine three Docker places: box-1 and box-2 for ru-linux
/// jobs, and box-3 for ru-linux and ru-large jobs.
fn drain_boxes(e: &mut E2e) {
    let m = &mut e.cfg.machines[0];
    (m.cores, m.memory_mb) = (8, 16384);
    m.places = vec![docker("box-1", &["ru-linux"]), docker("box-2", &["ru-linux"]), docker("box-3", &["ru-linux", "ru-large"])];
    e.cfg.pools = ["ru-linux", "ru-large"].map(|l| Pool { label: l.into(), ..Default::default() }).to_vec();
    e.save();
}

/// A running `runner-up drain`, its output going to a file.
struct DrainCli {
    child: Child,
    out: std::path::PathBuf,
}

impl DrainCli {
    fn start(e: &E2e, args: &[&str]) -> DrainCli {
        let out = e.dir.join("drain.out");
        let f = std::fs::File::create(&out).unwrap();
        let mut all = vec!["drain", "--config", &e.config];
        all.extend(args);
        let child = e.command(&all).stdin(Stdio::null()).stdout(f.try_clone().unwrap()).stderr(f).spawn().unwrap();
        DrainCli { child, out }
    }

    fn said(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn wait(&mut self) -> i32 {
        let status = crate::agent::tests::wait_timeout(&mut self.child, Duration::from_secs(30));
        let status = status.unwrap_or_else(|| panic!("the drain did not end within 30 seconds; it said:\n{}", self.said()));
        crate::agent::exit_code(&Ok(status))
    }
}

impl Drop for DrainCli {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Checks status prints each line in want.
fn status_says(e: &E2e, want: &[&str]) -> String {
    let out = e.status();
    for w in want {
        assert!(out.lines().any(|l| l.starts_with(w)), "status said:\n{out}\nwant a line starting {w:?}");
    }
    out
}

fn job_held(e: &E2e, id: &str) -> bool {
    e.held_by(id).is_some_and(|w| w.0 == BusyReason::JOB)
}

// Intent (README, "Draining places"): with a real coordinator serving,
// `runner-up drain` keeps two places from new jobs while the job running there
// finishes, admits only the named pair there, and exits 0 once the pair has
// run, the places then free; a place it does not name takes a job meanwhile,
// and status shows the drain throughout.
#[test]
fn drain_command_admits_the_pair_only_once_its_places_are_quiet() {
    skip_if_short!();
    let mut e = E2e::new();
    drain_boxes(&mut e);
    // PR #186's affected-journeys runs in box-1 before the drain.
    e.actions.send(&[job_message(&recorded_job(4, 0), "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("PR #186's job starts in box-1", || e.container_in("box-1").0 > 0);
    let (before, _) = e.container_in("box-1");
    e.start_job_named(before, "affected-journeys");
    e.eventually("box-1's runner takes the job", || e.run_in("box-1").is_some_and(|r| r.started));

    let d = DrainCli::start(
        &e,
        &[
            "--place", "box-1", "--place", "box-2", "--repo", "octo-org/app", "--job", "verify / linux", "--job", "affected-journeys",
            "--label", "release check", "--minutes", "5",
        ],
    );
    e.eventually("the drain waits for box-1's job", || {
        let ds = drains(&e.cfg);
        ds.len() == 1 && ds[0].phase == DRAIN_DRAINING && d.said().contains(&format!("drain: {DRAIN_DRAINING}: "))
    });

    // The pair arrives with the dry-run publish, which only box-3 takes: the
    // drained box-2 and box-3 are free, yet the pair waits.
    e.actions.send_to("ru-large", &[job_message(&recorded_job(1, 0), "JobAvailable", json!({}))]);
    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({})), job_message(&journeys_job(), "JobAvailable", json!({}))]);
    e.eventually("box-3 takes the publish", || e.container_in("box-3").0 > 0);
    e.eventually("the pair waits", || {
        let w = e.waiting();
        w.iter().any(|n| n == "verify / linux") && w.iter().any(|n| n == "affected-journeys")
    });
    let run = e.run_in("box-1").unwrap_or_default();
    assert!(
        run.started && run.job_name == "affected-journeys" && job_held(&e, "box-3"),
        "box-1 runs {run:?} and box-3 is held for {:?}, want box-1 running affected-journeys and box-3 a job",
        e.held_by("box-3")
    );
    // This test owns status's drain lines;
    // status_says_each_place_and_job_in_one_line owns the rest.
    status_says(
        &e,
        &[
            r#"place box-2 on box (docker, 2 cores, 4096 MB) drained (for "release check")"#,
            r#"drain "release check" of box-1, box-2 for octo-org/app "verify / linux", "affected-journeys" since "#,
        ],
    );
    assert_eq!(e.container_in("box-2").0, 0, "the drained box-2 took a job before box-1's job finished");

    // box-1's job finishes: the pair takes box-1 and box-2.
    e.end_job(before);
    let mut pair = std::collections::BTreeMap::new();
    e.eventually("the pair starts in box-1 and box-2", || {
        for id in ["box-1", "box-2"] {
            let (pid, _) = e.container_in(id);
            let Some(run) = e.run_in(id) else { return false };
            if pid == 0 || pid == before {
                return false;
            }
            pair.insert(run.job_name, pid);
        }
        pair.len() == 2
    });
    assert!(pair.contains_key("verify / linux") && pair.contains_key("affected-journeys"), "box-1 and box-2 run {pair:?}, want the pair");
    for (name, pid) in &pair {
        e.start_job_named(*pid, name);
    }
    e.eventually("the drain sees the pair run", || d.said().contains(&format!("drain: {DRAIN_RUNNING}: ")));
    status_says(&e, &[r#"drain "release check" of box-1, box-2 for octo-org/app "verify / linux", "affected-journeys" since "#]);

    for pid in pair.values() {
        e.end_job(*pid);
    }
    let mut d = d;
    let code = d.wait();
    assert_eq!(code, 0, "the drain exited {code}, want 0; it said:\n{}", d.said());
    let said = d.said();
    // The drain's detail words are the coordinator's drain tests'.
    let last = said.trim().lines().last().unwrap_or("");
    assert!(
        last.starts_with(&format!("drain: {DRAIN_DONE}: ")) && last.contains(r#""verify / linux""#) && last.contains(r#""affected-journeys""#),
        "the drain said:\n{said}\nwant it done last, with both jobs"
    );
    e.eventually("box-1 and box-2 are free, no longer drained", || {
        e.held_by("box-1").is_none() && e.held_by("box-2").is_none() && drains(&e.cfg).is_empty()
    });
    assert!(alive(e.container_in("box-3").0), "box-3's job did not carry on through the drain");
}

// Intent (README, "Draining places": the drain ends early when its command is
// stopped): stopping the drain mid-way exits 130, gives its places back at once
// to a job it held, and the job running in a drained place carries on.
#[test]
fn stopping_the_drain_resumes_placement_and_keeps_running_jobs() {
    skip_if_short!();
    let mut e = E2e::new();
    drain_boxes(&mut e);
    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("verify / linux starts in box-1", || e.container_in("box-1").0 > 0);
    let (running, _) = e.container_in("box-1");
    e.start_job_named(running, "verify / linux");

    let mut d = DrainCli::start(
        &e,
        &["--place", "box-1", "--place", "box-2", "--place", "box-3", "--job", "affected-journeys", "--label", "release check", "--minutes", "5"],
    );
    e.eventually("the drain waits for box-1's job", || d.said().contains(&format!("drain: {DRAIN_DRAINING}: ")));
    e.actions.send(&[job_message(&journeys_job(), "JobAvailable", json!({}))]);
    e.eventually("the named job waits", || e.status().contains(r#"waiting octo-org/app ru-linux "affected-journeys""#));

    unsafe { libc::kill(d.child.id() as i32, libc::SIGTERM) };
    let code = d.wait();
    assert_eq!(code, 130, "the stopped drain exited {code}, want 130; it said:\n{}", d.said());
    assert!(
        d.said().ends_with("drain: stopped; placement resumes, and running jobs carry on\n"),
        "the stopped drain said:\n{}",
        d.said()
    );
    e.eventually("the held job takes a freed place", || e.container_in("box-2").0 > 0);
    assert!(alive(running), "box-1's job did not carry on after the drain stopped");
    let out = e.status();
    assert!(!out.contains("drain"), "status still shows the stopped drain:\n{out}");
    let entries = std::fs::read_dir(std::path::Path::new(&e.cfg.state_dir).join("drains")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(entries, 0, "the stopped drain left entries behind");
}

// Intent (README, "Draining places"; a mistyped place or job must not pause
// anything): the drain refuses a place the setup lacks, a job named twice and
// missing input, exiting 2 before it publishes anything.
#[test]
fn drain_refuses_what_it_cannot_do() {
    let dir = super::temp_dir();
    let path = dir.path().join("config.json").to_string_lossy().into_owned();
    let mut cfg = Config {
        state_dir: dir.path().join("state").to_string_lossy().into_owned(),
        pools: vec![Pool { label: "ru-linux".into(), ..Default::default() }],
        machines: vec![Machine {
            name: "box".into(),
            ssh: vec!["ci@box".into()],
            places: vec![docker("box-1", &["ru-linux"])],
            ..Default::default()
        }],
        ..Default::default()
    };
    cfg.github.repos = vec!["octo-org/app".into()];
    config::save_config(&path, &cfg).unwrap();
    for (args, says) in [
        (vec!["--place", "box-1", "--job", "build", "--label", "check"], "drain needs --place ID --job NAME --label TEXT --minutes N"),
        (vec!["--place", "box-9", "--job", "build", "--label", "check", "--minutes", "5"], r#"no place "box-9" in the setup"#),
        (
            vec!["--place", "box-1", "--job", "build", "--job", "build", "--label", "check", "--minutes", "5"],
            r#"job "build" is named twice; each name admits one job"#,
        ),
    ] {
        let mut all = vec!["drain", "--config", path.as_str()];
        all.extend(&args);
        let (code, out) = crate::agent::tests::run(&mut super::command(&all), "");
        assert!(code == 2 && out.contains(says), "drain {args:?} exited {code} saying {out:?}, want 2 saying {says:?}");
    }
    let entries = std::fs::read_dir(dir.path().join("state/drains")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(entries, 0, "a refused drain left entries behind");
}
