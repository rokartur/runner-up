// Intent (README, "When a machine is down"). Without it, a place runner whose
// SSH was refused ended at once, and the coordinator gave the same place the
// same job again, registering a runner with GitHub for each try as fast as SSH
// refused.
//
// The far machine is reached with the real ssh, at a port of this computer
// nothing listens on; the box is reached through the stand-in ssh.

use serde_json::json;

use super::borrow::write_exec;
use super::e2e::{E2e, FAKE_SSH, job_message, ssh_to, verify_job};
use super::journeys::skip_if_short;
use crate::config::{Machine, PROVIDER_DOCKER, Place};

/// A port on this computer nothing listens on.
fn closed_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port().to_string()
}

/// The stand-in ssh, except that it hands ssh to the far machine to the real
/// ssh.
fn far_ssh(real_ssh: &str) -> String {
    FAKE_SSH.replacen("for last;", &format!("case \" $* \" in *\" far@127.0.0.1 \"*) exec '{real_ssh}' \"$@\";; esac\nfor last;"), 1)
}

pub(super) fn look_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').map(|d| std::path::Path::new(d).join(name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

#[test]
fn unreachable_machine_rests_until_its_agent_answers() {
    skip_if_short!();
    let Some(real_ssh) = look_path("ssh") else {
        eprintln!("needs ssh");
        return;
    };
    let mut e = E2e::new();
    // The far machine is listed first, so a job goes there while both
    // machines are idle.
    let far = Machine {
        name: "far".into(),
        ssh: ["-F", "/dev/null", "-p", &closed_port(), "far@127.0.0.1"].map(String::from).to_vec(),
        agent: super::runner_up().to_string(),
        places: vec![Place {
            id: "far-1".into(),
            provider: PROVIDER_DOCKER.into(),
            pools: vec!["ru-linux".into()],
            cores: 2,
            memory_mb: 4096,
            ..Default::default()
        }],
        ..Default::default()
    };
    e.cfg.machines.insert(0, far.clone());
    e.save();
    let ssh = e.dir.join("bin").join("ssh");
    write_exec(&ssh, &far_ssh(&real_ssh));

    let job = verify_job();
    e.actions.send(&[job_message(&job, "JobAvailable", json!({}))]);
    e.serve();

    // The far machine refuses the first runner's SSH. The job waits again
    // and goes to the box, the healthy place, with a second registration.
    e.eventually("a container starts in box-1", || e.container().0 > 0);
    e.eventually("the far runner nobody used is removed", || e.actions.read().removed == [1]);
    let seen = e.actions.read();
    assert!(
        seen.jits.len() == 2 && e.registrations() == seen.jits[1..],
        "made {} registrations and box-1 got {:?}, want two, the second box-1's\n{}",
        seen.jits.len(),
        e.registrations(),
        e.logs()
    );
    let to_far = format!("{} ", ssh_to(&far));
    let refused =
        e.lines(&e.dir.join("ssh.log")).iter().any(|l| l.starts_with(&to_far) && l.contains("'agent' 'run'") && l.contains("'far-1'"));
    assert!(refused, "no ssh to far ran far-1's agent with a connect timeout:\n{}", e.logs());

    // The far machine rests: over several passes its free place takes no job
    // and nothing more is registered.
    e.passes(3);
    let seen = e.actions.read();
    assert!(
        seen.jits.len() == 2 && seen.acquired.len() == 2,
        "while far rests the coordinator acquired {:?} and made {} registrations, want 2 of each\n{}",
        seen.acquired,
        seen.jits.len(),
        e.logs()
    );
    assert!(e.saved()["unreachable"].get("far").is_some(), "the coordinator saved no rest for far\n{}", e.logs());
    e.status_as_read();

    // The coordinator stops while far rests, and far comes back. The
    // coordinator starts again still resting far, asks its agent at once, and,
    // once it answers, far takes the next job.
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    write_exec(&ssh, FAKE_SSH);
    let mut next = job.clone();
    next["runnerRequestId"] = json!(9150);
    next["jobDisplayName"] = json!("verify / arm");
    e.actions.send(&[job_message(&next, "JobAvailable", json!({}))]);
    e.serve();
    e.eventually("a container starts in far-1", || e.dir.join("docker").join("runner-up-far-1.pid").exists());
    let version = format!("far@127.0.0.1 '{}' 'version'", super::runner_up());
    let asked = e.lines(&e.dir.join("ssh.log")).iter().any(|l| l.contains(&version));
    assert!(asked, "the coordinator never asked far's agent its version:\n{}", e.logs());
    let seen = e.actions.read();
    assert!(
        seen.jits.len() == 3 && seen.acquired == [9101, 9101, 9150],
        "acquired {:?} with {} registrations, want 9101 twice and 9150, with three",
        seen.acquired,
        seen.jits.len()
    );
    e.no_secrets(&seen.jits);
}
