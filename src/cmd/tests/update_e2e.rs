// Intent (README, "Updating runner-up"): the setup names a runner-up version
// and a per-platform SHA-256; runner-up downloads, verifies and restarts into
// it; it copies agents to its machines; and if a new version stops within a
// minute, runner-up returns to the previous one.
//
// The coordinator, its place runners and the agent are the real programs, and
// GitHub, ssh and docker the stand-ins of e2e.rs. The releases are this test
// program, which is runner-up v9.0.2 once installed in v9.0.2's folder
// (version.rs) and serves GitHub's stand-in as the coordinator does here, and
// a v9.0.3 that passes every check but stops as soon as it serves. A stand-in
// for GitHub's release downloads serves both. The machine's home, where its
// agent goes, is a test folder.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::e2e::{E2e, alive, job_message, verify_job};
use super::journeys::skip_if_short;
use crate::config::{self, Release};
use crate::release;
use crate::testserver::{Response, Server};

/// A release archive holding program as runner-up.
fn tar_gz(program: &[u8]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tw = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(program.len() as u64);
    header.set_mode(0o755);
    tw.append_data(&mut header, "runner-up", program).unwrap();
    tw.into_inner().unwrap().finish().unwrap()
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// A release that answers the coordinator's checks - its version, and that
/// the setup is good - but stops as soon as it serves.
const STOPS_AT_ONCE: &str = r#"#!/bin/sh
case $1 in
version) echo v9.0.3 ;;
check-config) echo "setup ok" ;;
*) echo "runner-up v9.0.3 cannot serve" >&2; exit 1 ;;
esac
"#;

// The coordinator restarts as the release its setup names, keeps it after its
// trial and copies it to its machine, whose jobs then run with it. A later
// release that stops at once is refused when the supervisor starts the
// coordinator again: it serves on as the version before, and a job running
// throughout carries on.
#[test]
fn coordinator_runs_the_setups_release_and_goes_back_from_one_that_stops() {
    skip_if_short!();
    let mut e = E2e::new();
    let platform = release::platform(release::goos(), release::goarch());
    let good = std::fs::read(super::runner_up()).unwrap();
    let good_archive = tar_gz(&good);
    let bad_archive = tar_gz(STOPS_AT_ONCE.as_bytes());
    let files = BTreeMap::from([
        (format!("/v9.0.2/{}", release::archive_name("v9.0.2", &platform)), good_archive.clone()),
        (format!("/v9.0.3/{}", release::archive_name("v9.0.3", &platform)), bad_archive.clone()),
    ]);
    let downloads = Server::start(move |r| match files.get(&r.path) {
        Some(b) => Response::new(200, b.clone()),
        None => Response::new(404, "404 page not found\n"),
    });
    let home = e.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    e.env.push(("HOME".into(), home.to_string_lossy().into_owned()));
    e.env.push(("RUNNER_UP_TEST_TRIAL".into(), "3s".into()));
    let release_of = |version: &str, archive: &[u8]| Release {
        version: version.into(),
        sha256: BTreeMap::from([(platform.clone(), sha(archive))]),
        url: downloads.url.clone(),
    };
    e.cfg.release = Some(release_of("v9.0.2", &good_archive));
    e.save();
    let program = std::path::PathBuf::from(&e.cfg.state_dir).join("program");
    let current = || std::fs::read_link(program.join("current")).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();

    e.serve();
    e.eventually("the coordinator restarts as v9.0.2", || {
        e.logged(r#"msg="runner-up runs another version" version=v9.0.2"#) && e.logged(r#"msg="runner-up starting" version=v9.0.2"#)
    });
    e.eventually("v9.0.2 is kept after its trial", || e.logged(r#"msg="runner-up kept the new version after its trial" version=v9.0.2"#));
    assert_eq!(current(), "v9.0.2/runner-up", "program/current");
    assert!(
        std::fs::read(program.join("v9.0.2/runner-up")).is_ok_and(|b| b == good),
        "program/v9.0.2/runner-up is not the release's program"
    );

    // The machine's agent is the release, in its version's folder in the
    // machine's home, and its jobs run with it.
    let copied = ".local/share/runner-up/v9.0.2/runner-up";
    e.eventually("the coordinator copies v9.0.2 to the machine", || {
        e.logged(r#"msg="copied runner-up to the machine as its agent" machine=box version=v9.0.2"#)
    });
    assert!(std::fs::read(home.join(copied)).is_ok_and(|b| b == good), "the machine's {copied} is not the release's program");
    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({}))]);
    e.eventually("a container starts for the job", || e.container().0 > 0);
    let ssh = e.lines(&e.dir.join("ssh.log"));
    let with_copied = format!("'{copied}' 'agent' 'run'");
    assert!(ssh.iter().any(|l| l.contains(&with_copied)), "no place ran its job with the copied agent; ssh ran:\n{}", ssh.join("\n"));
    let (pid, _) = e.container();
    e.start_job(pid);
    e.eventually("the job reaches its runner", || e.place_run().is_some_and(|r| r.started));

    // The setup moves to v9.0.3, which stops as soon as it serves.
    e.cfg.release = Some(release_of("v9.0.3", &bad_archive));
    e.save();
    let code = e.exited();
    assert_eq!(code, 1, "v9.0.3 exited {code}, want 1\n{}", e.logs());
    assert!(
        e.logged(r#"msg="runner-up runs another version" version=v9.0.3"#) && e.logged("runner-up v9.0.3 cannot serve"),
        "the coordinator did not restart as v9.0.3\n{}",
        e.logs()
    );

    // Its supervisor starts program/current again, which is still v9.0.2.
    e.serve_as(&program.join("current"));
    e.eventually("v9.0.2 refuses v9.0.3 and serves on", || {
        e.logged(
            r#"msg="runner-up refused a new version and goes back to the version before it" version=v9.0.3 why="it never started serving" before=v9.0.2"#,
        ) && e.logged(r#"msg="runner-up starting" version=v9.0.2"#)
            && e.logged(r#"msg="the setup names a version runner-up refused; it stays on its version" version=v9.0.3 running=v9.0.2"#)
    });
    let refused: Vec<Value> = config::read_json(&program.join("refused.json")).unwrap_or_default();
    assert!(
        refused.len() == 1 && refused[0]["version"] == "v9.0.3" && refused[0]["sha256"] == sha(&bad_archive),
        "refused.json holds {refused:?}, want v9.0.3 with its archive's SHA-256"
    );
    assert_eq!(current(), "v9.0.2/runner-up", "program/current after the refusal");

    // The job ran through both restarts, and ends as usual.
    assert!(alive(pid), "the job stopped when the coordinator restarted\n{}", e.logs());
    e.end_job(pid);
    e.eventually("the place frees", || !e.busy());
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    let seen = e.actions.read();
    assert!(seen.stray.is_empty(), "unknown requests {:?}", seen.stray);
}
