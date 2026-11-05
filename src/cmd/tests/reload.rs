// Intent (README, "Changing the setup"): serve watches its setup file and waits
// about a second after a change, because a pull rewrites the file. It checks it
// as check-config does and applies pools, places, sizes and order in place,
// without dropping running jobs or open sessions. It keeps the running setup
// when the new one fails.
//
// The coordinator, its place runners and the agent are the real programs, and
// GitHub, ssh and docker the stand-ins of e2e.rs. The setup file is rewritten
// whole, as a pull or an editor rewrites it.

use serde_json::json;

use super::e2e::{E2e, alive, job_message, journeys_job, verify_job};
use super::journeys::skip_if_short;
use crate::config::{self, PROVIDER_DOCKER, Place, Pool};

// A pool and a place added to the setup while a job runs take a job at once,
// at the pool's size, and the running job and its scale set's session carry
// on. Dropped from the setup again while the new place's job runs, the pool's
// session closes and that job runs to its end too.
#[test]
fn setup_change_applies_while_a_job_runs() {
    skip_if_short!();
    let mut e = E2e::new();
    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (pid, _) = e.container();
    e.start_job(pid);
    e.eventually("the job reaches its runner", || e.place_run().is_some_and(|r| r.started));
    let original = e.cfg.clone();

    // The setup gains a pool of 1-core jobs and a place serving it. The
    // machine's places may reserve 3 cores, box-1's job holds 2, so the new
    // place's job fits only at the pool's size, not the place's 2 cores.
    let mut grown = config::load_config(&e.config).unwrap();
    grown.pools.push(Pool { label: "ru-small".into(), cores: 1, memory_mb: 1024 });
    grown.machines[0].places.push(Place {
        id: "box-2".into(),
        provider: PROVIDER_DOCKER.into(),
        pools: vec!["ru-small".into()],
        cores: 2,
        memory_mb: 4096,
        ..Default::default()
    });
    e.cfg = grown;
    e.save();
    e.eventually("the coordinator applies the changed setup", || {
        e.logged(r#"msg="applied the changed setup" pools=+ru-small places=+box-2"#)
    });
    e.eventually("a session opens for the new pool", || e.actions.read().opened == 2);

    e.actions.send_to("ru-small", &[job_message(&journeys_job(), "JobAvailable", json!({}))]);
    e.eventually("a container starts in the new place", || e.container_in("box-2").0 > 0);
    let (small, _) = e.container_in("box-2");
    let want_docker = "run --rm -i --name runner-up-box-2 --label runner-up.place=box-2 --cpus 1 --memory 1024m";
    let docker = e.lines(&e.dir.join("docker").join("docker.log"));
    assert!(docker.iter().any(|l| l.starts_with(want_docker)), "docker ran {docker:?}, want a line starting {want_docker:?}");
    let seen = e.actions.read();
    assert!(
        seen.closed == 0 && alive(pid),
        "at the setup change {} sessions closed and box-1's job is alive {}, want none closed and the job alive\n{}",
        seen.closed,
        alive(pid),
        e.logs()
    );

    // The setup drops the pool and the place while the place's job runs.
    e.start_job(small);
    e.cfg = original;
    e.save();
    e.eventually("the coordinator applies the setup without the pool", || {
        e.logged(r#"msg="applied the changed setup" pools=-ru-small places=-box-2"#)
    });
    e.eventually("the dropped pool's session closes", || e.actions.read().closed == 1);
    assert!(alive(small), "box-2's job stopped when the setup dropped its place\n{}", e.logs());
    e.end_job(small);
    e.eventually("the coordinator retires box-2's finished run", || e.cfg.place_dir("box-2").join("last-place.json").exists());

    // box-1's job ran throughout, and its pool's session was never reopened.
    let seen = e.actions.read();
    assert!(alive(pid) && seen.opened == 2, "box-1's job is alive {} and {} sessions opened, want alive and 2", alive(pid), seen.opened);
    e.end_job(pid);
    e.eventually("box-1 frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    let seen = e.actions.read();
    assert!(seen.closed == 2 && seen.stray.is_empty(), "{} sessions closed, want 2; unknown requests {:?}", seen.closed, seen.stray);
}

// A setup file that fails check-config is refused, and the coordinator keeps
// its running setup: it places the next job as before, and the job's place
// runner starts from the setup the coordinator runs, not from the refused
// file. Once the file holds the running setup again, the coordinator says so.
#[test]
fn refused_setup_keeps_the_running_setup() {
    skip_if_short!();
    let mut e = E2e::new();
    e.serve();
    e.eventually("the coordinator polls its session", || e.actions.read().polls >= 1);

    // A hand edit misspells the pool the place serves.
    let mut broken = config::load_config(&e.config).unwrap();
    broken.machines[0].places[0].pools = vec!["ru-linx".into()];
    config::save_config(&e.config, &broken).unwrap();
    e.eventually("the coordinator refuses the setup", || {
        e.logged(r#"msg="the setup file was not applied; keeping the running setup" err="place \"box-1\": unknown pool \"ru-linx\"""#)
    });

    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({}))]);
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let (pid, _) = e.container();
    let run = e.place_run().unwrap_or_default();
    let mut ps = std::process::Command::new("ps");
    ps.args(["-ww", "-o", "args=", "-p", &run.pid.to_string()]);
    let (_, args) = crate::agent::tests::run(&mut ps, "");
    let want = format!("--config {}/", std::path::Path::new(&e.cfg.state_dir).join("setups").display());
    assert!(args.contains(&want), "the place runner runs as {args:?}, want it to read the coordinator's setup under {want:?}");

    // The edit is put right.
    e.save();
    e.eventually("the coordinator says the file holds its setup again", || {
        e.logged(r#"msg="the setup file holds the running setup again""#)
    });
    e.start_job(pid);
    e.end_job(pid);
    e.eventually("the place frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    let seen = e.actions.read();
    assert!(seen.opened == 1 && seen.stray.is_empty(), "{} sessions opened, want 1; unknown requests {:?}", seen.opened, seen.stray);
}
