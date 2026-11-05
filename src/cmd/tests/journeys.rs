//! The journeys of e2e.rs's harness through the real coordinator, place
//! runners and agent.

use std::collections::BTreeMap;

use serde_json::json;

use super::e2e::{E2e, alive, cancelled, job_message, journeys_job, ssh_to, verify_job};
use super::runner_up;
use crate::agent::{DEFAULT_DOCKER_IMAGE, fork_guard_hook};
use crate::config::Pool;
use crate::coordinator::BusyReason;
use crate::go::time::Time;

macro_rules! skip_if_short {
    () => {
        if super::e2e::short() {
            return;
        }
    };
}
pub(crate) use skip_if_short;

fn ps_args(pid: i64) -> String {
    let out = std::process::Command::new("ps").args(["-ww", "-o", "args=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// Intent (README, "How it works" and "Running as a service"): the real
// coordinator/agent journey keeps its runner through a restart, hands
// registration only on stdin, and gives the job its configured size and fork
// guard.
#[test]
fn job_runs_through_the_agent_and_outlives_a_coordinator_restart() {
    skip_if_short!();
    let mut e = E2e::new();
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();

    e.eventually("a container starts for the job", || e.container().0 > 0);
    let seen = e.actions.read();
    assert!(
        seen.acquired == [9101] && seen.jits.len() == 1,
        "acquired {:?} with {} registrations, want request 9101 with one",
        seen.acquired,
        seen.jits.len()
    );
    assert_eq!(e.registrations(), seen.jits, "the runner got the wrong registration");
    let (pid, agent_pid) = e.container();
    let run = e.place_run().unwrap();
    for p in [run.pid, agent_pid as i64] {
        let args = ps_args(p);
        assert!(!args.is_empty() && !args.contains(&seen.jits[0]), "process {p}'s command line is {args:?}");
    }
    // The agent ran over SSH with every word quoted, and gave the container
    // the place's share and the fork guard as its job-started hook.
    let ssh = e.lines(&e.dir.join("ssh.log"));
    let want_run = format!(
        "{} '{}' 'agent' 'run' '--provider' 'docker' '--place' 'box-1' '--cores' '2' '--memory' '4096' '--boot-timeout' '{}'",
        ssh_to(&e.cfg.machines[0]),
        runner_up(),
        e.cfg.boot_timeout_seconds
    );
    assert!(ssh.contains(&want_run), "ssh ran {ssh:?}, want {want_run:?} among them");
    let docker = e.lines(&e.dir.join("docker/docker.log"));
    let hook = e.dir.join("machine/runner-up/agent/places/box-1/fork-guard.sh");
    let place = &e.cfg.places()[0];
    let want_docker = format!(
        "run --rm -i --name runner-up-box-1 --label runner-up.place=box-1 --cpus 2 --memory 4096m -v {}:/opt/runner-up/runner-up:ro -v {}:/opt/runner-up/fork-guard.sh:ro -e ACTIONS_RUNNER_HOOK_JOB_STARTED=/opt/runner-up/fork-guard.sh -e RUNNER_UP_CORES={} -e RUNNER_UP_MEMORY_MB={} --entrypoint /bin/sh {DEFAULT_DOCKER_IMAGE} -c read -r ACTIONS_RUNNER_INPUT_JITCONFIG && export ACTIONS_RUNNER_INPUT_JITCONFIG && exec \"$@\" runner-up /home/runner/run.sh",
        runner_up(),
        hook.display(),
        place.cores,
        place.memory_mb
    );
    assert!(docker.contains(&want_docker), "docker ran {docker:?}, want {want_docker:?} among them");
    // AGENTS.md: the registration reaches the runner only on standard input.
    // The runner got it above, yet the container's settings - docker's
    // arguments and the runner variables docker could copy from its own
    // environment, all of which docker inspect shows - never held it.
    let settings = e.lines(&e.dir.join("docker/settings"));
    assert!(!settings[0].is_empty(), "docker recorded no container settings");
    for l in &settings {
        assert!(
            !l.contains(&seen.jits[0]) && !l.contains("ACTIONS_RUNNER_INPUT_JITCONFIG="),
            "the container's settings held the registration: {l:?}"
        );
    }
    let hook_text = std::fs::read_to_string(&hook).unwrap_or_default();
    assert_eq!(hook_text, fork_guard_hook("/opt/runner-up/runner-up"), "the container's job-started hook");

    e.start_job(pid);
    e.eventually("the place runner sees the job start", || e.place_run().is_some_and(|r| r.started));
    let why = e.held_by("box-1");
    assert_eq!(why.as_ref().map(|w| w.0.as_str()), Some(BusyReason::JOB), "box-1 is held for {why:?}");
    e.status_as_read();

    // The coordinator stops; the job carries on in its place.
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    assert_eq!(e.actions.read().closed, 1, "the coordinator closed the wrong number of sessions on stopping");
    assert!(
        e.busy() && alive(pid) && alive(agent_pid),
        "the place did not outlive the coordinator: busy {}, container {}, agent {}",
        e.busy(),
        alive(pid),
        alive(agent_pid)
    );

    // The coordinator starts again: it opens a new session, leaves the busy
    // place alone and starts no second runner.
    e.serve();
    // It places jobs as soon as its session is open, before its first poll,
    // and again after every poll.
    e.eventually("the coordinator polls its new session twice", || {
        let seen = e.actions.read();
        seen.opened == 2 && seen.polls >= 2
    });
    let seen = e.actions.read();
    assert!(
        seen.jits.len() == 1 && seen.acquired.len() == 1,
        "after the restart the coordinator acquired {:?} and made {} registrations",
        seen.acquired,
        seen.jits.len()
    );
    let cleans = e.lines(&e.dir.join("ssh.log")).iter().filter(|l| l.contains("'agent' 'clean'")).count();
    assert_eq!(cleans, 1, "the agent cleaned the place {cleans} times, want once, at the first start while it was free");
    assert!(alive(pid), "the restart stopped the running job");

    // The job ends and GitHub says so; the place frees and its runner, which
    // ran its job, is not removed.
    e.end_job(pid);
    let runner = run.runner_name.clone();
    e.actions.send(&[
        job_message(&job, "JobStarted", json!({"runnerId": 1, "runnerName": runner})),
        job_message(&job, "JobCompleted", json!({"result": "succeeded", "runnerId": 1, "runnerName": runner})),
    ]);
    e.eventually("the place frees", || !e.busy());
    e.eventually("the coordinator forgets the finished run", || e.place_run().is_none());
    assert!(!alive(pid) && !alive(agent_pid), "the container or agent outlived the job: {} {}", alive(pid), alive(agent_pid));
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(seen.removed.is_empty() && seen.stray.is_empty(), "removed runners {:?}, unknown requests {:?}", seen.removed, seen.stray);
    assert_eq!(e.registrations().len(), 1, "containers ran");
    e.no_secrets(&seen.jits);
}

#[test]
fn crashed_place_is_cleaned_up_and_its_job_placed_again() {
    skip_if_short!();
    let mut e = E2e::new();
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (first, _) = e.container();

    // The place runner dies before the job reaches its runner, as when the
    // coordinator's machine loses it or the SSH connection drops: the agent
    // stops the container, the runner nobody used is removed, and the job
    // gets a new runner in the place.
    let run = e.place_run().unwrap();
    unsafe { libc::kill(run.pid as i32, libc::SIGKILL) };
    e.eventually("a second container starts", || e.registrations().len() == 2);
    e.eventually("the second container is recorded", || {
        let (pid, _) = e.container();
        pid > 0 && pid != first
    });
    assert!(!alive(first), "the first container outlived its place runner");
    e.eventually("the unused runner is removed", || e.actions.read().removed == [1]);

    // The agent itself is killed outright and cannot clean up: its container
    // is left running until the next agent in the place removes it first.
    let (second, agent_pid) = e.container();
    unsafe { libc::kill(agent_pid, libc::SIGKILL) };
    e.eventually("a third container starts", || e.registrations().len() == 3);
    e.eventually("the third container is recorded", || {
        let (pid, _) = e.container();
        pid > 0 && pid != second
    });
    assert!(!alive(second), "the container a killed agent left behind is still running");
    let docker = e.lines(&e.dir.join("docker/docker.log"));
    let runs: Vec<usize> = (0..docker.len()).filter(|&i| docker[i].starts_with("run ")).collect();
    assert!(
        runs.len() == 3 && docker[runs[1]..runs[2]].iter().any(|l| l == "rm -f runner-up-box-1"),
        "the third agent did not remove what the second left before starting:\n{}",
        docker.join("\n")
    );
    e.eventually("the second unused runner is removed", || e.actions.read().removed == [1, 2]);

    // The third runner runs the job.
    let (third, _) = e.container();
    e.start_job(third);
    e.eventually("the job starts", || e.place_run().is_some_and(|r| r.started));
    e.end_job(third);
    e.eventually("the place frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(
        seen.acquired == [9101, 9101, 9101] && seen.removed == [1, 2],
        "acquired {:?} and removed runners {:?}, want 9101 three times and runners 1 and 2",
        seen.acquired,
        seen.removed
    );
    assert_eq!(e.registrations(), seen.jits, "the containers got the wrong registrations");
    e.no_secrets(&seen.jits);
}

/// job's start and its success, on runner 1 named runner.
fn ran(job: &serde_json::Value, runner: &str) -> [serde_json::Value; 2] {
    [
        job_message(job, "JobStarted", json!({"runnerId": 1, "runnerName": runner})),
        job_message(job, "JobCompleted", json!({"result": "succeeded", "runnerId": 1, "runnerName": runner})),
    ]
}

// Intent (README, "Setup file", startTimeoutSeconds, and the fixed bug:
// runner-up's idle timer powered off a runner the same second GitHub gave it a
// job, and GitHub then failed the job with "lost communication"): GitHub can
// give a scale set's idle runner a job until the runner is removed, so a runner
// no job reached within the start timeout is removed from its scale set while
// it still runs, and only then stopped. Its job gets a new runner.
#[test]
fn idle_runner_is_removed_from_its_scale_set_before_it_stops() {
    skip_if_short!();
    let mut e = E2e::new();
    e.cfg.start_timeout_seconds = 2;
    e.save();
    e.watch_container();
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (first, _) = e.container();

    // No job reaches the runner: once the start timeout passes it is removed,
    // with its container still running, and then its container stops.
    e.eventually("the idle runner is removed", || !e.actions.read().removed.is_empty());
    e.eventually("the idle runner's container stops", || !alive(first));
    let seen = e.actions.read();
    assert!(
        seen.removed[0] == 1 && seen.up[0],
        "removed runners {:?} with the container running {:?}, want runner 1 removed while it ran\n{}",
        seen.removed,
        seen.up,
        e.logs()
    );

    // The job waits again and gets a new runner in the place.
    e.eventually("the job gets a second runner", || e.registrations().len() == 2);
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(
        !seen.removed[1..].contains(&1) && seen.stray.is_empty(),
        "removed runners {:?}, unknown requests {:?}; want runner 1 removed once",
        seen.removed,
        seen.stray
    );
    assert!(seen.acquired.starts_with(&[9101, 9101]), "acquired {:?}, want 9101 for the first runner and again for the second", seen.acquired);
    e.no_secrets(&seen.jits);
}

// Intent (README, "Setup file", startTimeoutSeconds): a runner GitHub gave a
// job just as the start timeout passed is one GitHub refuses to remove, saying
// it is running a job. That runner keeps running and runs the job; no second
// runner starts.
#[test]
fn runner_given_a_job_at_the_start_timeout_keeps_running() {
    skip_if_short!();
    let mut e = E2e::new();
    e.cfg.start_timeout_seconds = 2;
    e.save();
    e.actions.give_job(1);
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (pid, _) = e.container();
    let run = e.place_run().unwrap();

    // The start timeout passes before the runner says its job started.
    // GitHub refuses the removal; the runner keeps running through the next
    // ticks, and is not asked about again.
    e.eventually("GitHub refuses to remove the runner", || !e.actions.read().refused.is_empty());
    e.passes(2);
    let seen = e.actions.read();
    assert!(
        alive(pid) && seen.refused == [1] && seen.removed.is_empty(),
        "container running {}, refused removals {:?}, removed runners {:?}; want it running, refused once\n{}",
        alive(pid),
        seen.refused,
        seen.removed,
        e.logs()
    );

    // Its job starts and ends; the place frees.
    e.start_job(pid);
    e.eventually("the place runner sees the job start", || e.place_run().is_some_and(|r| r.started));
    e.end_job(pid);
    e.actions.send(&ran(&job, &run.runner_name));
    e.eventually("the place frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(
        e.registrations().len() == 1 && seen.removed.is_empty() && seen.refused == [1] && seen.stray.is_empty(),
        "{} containers ran, removed {:?}, refused {:?}, unknown requests {:?}; want one container, one refusal",
        e.registrations().len(),
        seen.removed,
        seen.refused,
        seen.stray
    );
    e.no_secrets(&seen.jits);
}

// Intent (README, "When GitHub closes a job"): GitHub can give a scale set's
// waiting runner another of the set's jobs until the runner is removed, so the
// runner of a job cancelled before it started is removed from its scale set
// while it still runs, and only then stopped. The cancelled job gets no new
// runner, and the runner is not removed again once its place ends.
#[test]
fn spare_runner_is_removed_from_its_scale_set_before_it_stops() {
    skip_if_short!();
    let mut e = E2e::new();
    e.watch_container();
    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (first, _) = e.container();

    // The run is cancelled before the job reached the runner: the runner is
    // removed, with its container still running, and then its container
    // stops.
    e.actions.send(&[cancelled(&job)]);
    e.eventually("the spare runner is removed", || !e.actions.read().removed.is_empty());
    e.eventually("the spare runner's container stops", || !alive(first));
    let seen = e.actions.read();
    assert!(
        seen.removed[0] == 1 && seen.up[0],
        "removed runners {:?} with the container running {:?}, want runner 1 removed while it ran\n{}",
        seen.removed,
        seen.up,
        e.logs()
    );

    // The place frees, and stays free through the next ticks.
    e.eventually("the place frees", || !e.busy());
    e.eventually("the coordinator forgets the stopped run", || e.place_run().is_none());
    e.passes(2);
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(seen.removed == [1] && seen.stray.is_empty(), "removed runners {:?}, unknown requests {:?}; want runner 1 removed once", seen.removed, seen.stray);
    assert!(
        e.registrations().len() == 1 && seen.acquired == [9101],
        "{} containers ran and acquired {:?}, want one container for 9101",
        e.registrations().len(),
        seen.acquired
    );
    e.no_secrets(&seen.jits);
}

// Intent (README, "When GitHub closes a job"): GitHub assigns the scale set a
// second job and gives it to the waiting runner just as the runner's own job is
// cancelled. GitHub refuses to remove that runner, so it keeps running, is not
// asked about again, and runs the job; no second runner starts.
#[test]
fn spare_runner_given_a_job_as_its_job_is_cancelled_keeps_running() {
    skip_if_short!();
    let mut e = E2e::new();
    let (job, other) = (verify_job(), journeys_job());
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (pid, _) = e.container();
    let run = e.place_run().unwrap();

    e.actions.give_job(1);
    e.actions.send(&[job_message(&other, "JobAssigned", json!({})), cancelled(&job)]);
    e.eventually("GitHub refuses to remove the runner", || !e.actions.read().refused.is_empty());
    e.passes(2);
    let seen = e.actions.read();
    assert!(
        alive(pid) && seen.refused == [1] && seen.removed.is_empty(),
        "container running {}, refused removals {:?}, removed runners {:?}; want it running, refused once\n{}",
        alive(pid),
        seen.refused,
        seen.removed,
        e.logs()
    );

    // The second job starts on the runner and ends; the place frees.
    e.start_job(pid);
    e.eventually("the place runner sees the job start", || e.place_run().is_some_and(|r| r.started));
    let started = job_message(
        &other,
        "JobStarted",
        json!({"runnerId": 1, "runnerName": run.runner_name, "runnerAssignTime": Time::now().rfc3339()}),
    );
    e.actions.send(&[started]);
    e.eventually("the coordinator hears the second job started", || {
        e.logs().contains(r#"msg="job started" repo=octo-org/app job=affected-journeys"#)
    });
    e.end_job(pid);
    let [_, completed] = ran(&other, &run.runner_name);
    e.actions.send(&[completed]);
    e.eventually("the place frees", || !e.busy());
    e.passes(2);
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");
    let seen = e.actions.read();
    assert!(
        e.registrations().len() == 1 && seen.removed.is_empty() && seen.refused == [1] && seen.stray.is_empty(),
        "{} containers ran, removed {:?}, refused {:?}, unknown requests {:?}; want one container, one refusal",
        e.registrations().len(),
        seen.removed,
        seen.refused,
        seen.stray
    );
    e.no_secrets(&seen.jits);
}

// Intent (README, "Setup file", poolImages): a place serving two pools runs
// each pool's job on that pool's image - two Ubuntu releases on the same place,
// picked by the label the job asks for - and stays one place with one share of
// its machine: the second job waits for it rather than getting a place of its
// own.
#[test]
fn place_runs_each_job_on_its_pools_image() {
    skip_if_short!();
    let mut e = E2e::new();
    e.cfg.pools.push(Pool { label: "ru-linux-2604".into(), ..Default::default() });
    let bx = &mut e.cfg.machines[0].places[0];
    bx.pools = vec!["ru-linux".into(), "ru-linux-2604".into()];
    bx.image = "runner-2404:latest".into();
    bx.pool_images = BTreeMap::from([("ru-linux-2604".into(), "runner-2604:latest".into())]);
    e.save();
    let (code, checked) = crate::agent::tests::run(&mut e.command(&["check-config", "--config", &e.config]), "");
    let want_checked = "ok: 1 targets, 2 pools, 1 machines, 1 places\nbox (ci@box): box-1 docker 2 cores 4096 MB; places may reserve 3 of 4 cores and 7168 of 8192 MB, all at once\n";
    assert!(code == 0 && checked == want_checked, "check-config exited {code} and said {checked:?}, want {want_checked:?}");

    let mut newer = verify_job();
    newer["runnerRequestId"] = json!(9102);
    newer["jobDisplayName"] = json!("verify / linux 26.04");
    let older = verify_job();
    e.actions.send_to("ru-linux-2604", &[job_message(&newer, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the 26.04 job", || e.container().0 > 0);
    let (first, _) = e.container();
    let pool = e.place_run().unwrap().pool;
    assert_eq!(pool, "ru-linux-2604", "the place runs a job of the wrong pool");

    // The 24.04 job arrives while the only place is busy: it waits.
    e.actions.send(&[job_message(&older, "JobAvailable", json!({}))]);
    e.eventually("the 24.04 job waits for the place", || e.waiting().iter().any(|n| n == "verify / linux"));
    assert_eq!(e.registrations().len(), 1, "containers ran while the place was busy");

    e.start_job(first);
    e.end_job(first);
    let run = e.place_run().unwrap();
    e.actions.send_to("ru-linux-2604", &ran(&newer, &run.runner_name));
    e.eventually("a container starts for the 24.04 job", || {
        let (pid, _) = e.container();
        pid > 0 && pid != first && e.registrations().len() == 2
    });
    let (second, _) = e.container();
    let pool = e.place_run().unwrap().pool;
    assert_eq!(pool, "ru-linux", "the place runs a job of the wrong pool");
    e.start_job(second);
    e.end_job(second);
    e.eventually("the place frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM");

    // Each job's agent and container got its pool's image, in the one place.
    let agent_run = |image: &str| {
        format!(
            "{} '{}' 'agent' 'run' '--provider' 'docker' '--place' 'box-1' '--cores' '2' '--memory' '4096' '--boot-timeout' '{}' '--image' '{image}'",
            ssh_to(&e.cfg.machines[0]),
            runner_up(),
            e.cfg.boot_timeout_seconds
        )
    };
    let runs: Vec<String> = e.lines(&e.dir.join("ssh.log")).into_iter().filter(|l| l.contains("'agent' 'run'")).collect();
    let want = [agent_run("runner-2604:latest"), agent_run("runner-2404:latest")];
    assert_eq!(runs, want, "ssh ran the agent wrongly");
    let images = e.lines(&e.dir.join("docker/images"));
    assert_eq!(images, ["runner-2604:latest", "runner-2404:latest"], "the containers ran the wrong images");
    let logs = e.logs();
    for want in [
        r#"msg="agent started" place=box-1 machine=box provider=docker pool=ru-linux-2604 image=runner-2604:latest"#,
        r#"msg="agent started" place=box-1 machine=box provider=docker pool=ru-linux image=runner-2404:latest"#,
    ] {
        assert!(logs.contains(want), "no place log says {want:?}\n{logs}");
    }
    let seen = e.actions.read();
    assert!(seen.acquired == [9102, 9101] && seen.stray.is_empty(), "acquired {:?}, unknown requests {:?}, want 9102 then 9101", seen.acquired, seen.stray);
    e.no_secrets(&seen.jits);
}

// Intent (README, "Setup file", poolImages): a place runner started for a pool
// its place does not serve starts no agent, since no image there was chosen for
// that pool; nor does one started with no pool, as a coordinator from before
// pools had images would start it, in a place whose pools have their own
// images.
#[test]
fn place_runner_refuses_a_pool_its_place_does_not_serve() {
    skip_if_short!();
    let mut e = E2e::new();
    e.cfg.machines[0].places[0].pool_images = BTreeMap::from([("ru-linux".into(), "runner-2604:latest".into())]);
    e.save();
    for (pool, logged) in [("ru-large", "ru-large"), ("", r#""""#)] {
        let mut args = vec!["run-place", "--config", &e.config, "--place", "box-1"];
        if !pool.is_empty() {
            args.extend(["--pool", pool]);
        }
        let (code, out) = crate::agent::tests::run(&mut e.command(&args), "stand-in-registration\n");
        let want = format!("msg=\"the place does not serve the pool\" place=box-1 pool={logged}\n");
        assert!(code == crate::agent::EXIT_FAILED && out.contains(&want), "run-place for pool {pool:?} exited {code} saying {out:?}");
    }
    assert!(!e.dir.join("ssh.log").exists(), "a refused place runner reached the machine");
}
