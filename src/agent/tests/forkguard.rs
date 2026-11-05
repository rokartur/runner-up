// Intent (README, "Pull requests from forks"): every place's runner starts
// the job only after its job-started hook allows it. The hook refuses a job
// from another repository's pull request, its follow-on run, a deleted fork's
// pull request and an event it cannot read, and allows every job from the
// repository itself. The events are GitHub payloads saved from a real fork's
// pull request, renamed (testdata/agent/events).
//
// These run the fork-guard command's verdict in this process; the program
// tests in tests/ run the hook a runner is given.

use super::{UNREADABLE_EVENT, fork_guard_say};

fn event(name: &str) -> String {
    format!("{}/testdata/agent/events/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn run(path: &str, name: &str) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = fork_guard_say(path, name, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn refuses_foreign_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.json").to_string_lossy().into_owned();
    for (path, why) in [
        (event("pull-request-521-fork.json"), "pull request from a-contributor/tool"),
        (event("pull-request-review-521-fork.json"), "pull request from a-contributor/tool"),
        (event("pull-request-521-deleted-fork.json"), "pull request from a deleted fork"),
        (event("workflow-run-fork.json"), "follow-on run of a-contributor/tool"),
        (event("garbage.txt"), "unreadable event file"),
        (event("array.json"), "unreadable event file"),
        (missing, "no event file"),
        (String::new(), "no event file"),
    ] {
        let (code, _, stderr) = run(&path, "pull_request");
        assert_ne!(code, 0, "{path}: the guard allowed the job");
        assert!(
            stderr.starts_with(&format!("runner-up refuses this job: {why}.")),
            "{path}: the guard refused without saying {why:?}: {stderr}"
        );
    }
}

#[test]
fn allows_the_repositorys_own_jobs() {
    for (file, name) in [
        ("pull-request-527-same-repo.json", "pull_request"),
        ("workflow-run-same-repo.json", "workflow_run"),
        ("push-main.json", "push"),
        ("release-published.json", "release"),
        ("workflow-dispatch.json", "workflow_dispatch"),
    ] {
        let (code, stdout, stderr) = run(&event(file), name);
        assert_eq!(code, 0, "{file}: the guard refused a job from this repository: {stderr}");
        assert_eq!(stdout, format!("runner-up: {name} job from this repository; allowed\n"), "{file}");
    }
}

// A JSON file with a second value after its object answers two ways; the
// guard refuses it rather than read only the first.
#[test]
fn refuses_trailing_values() {
    let mut b = std::fs::read(event("push-main.json")).unwrap();
    b.extend(std::fs::read(event("pull-request-521-fork.json")).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.json");
    std::fs::write(&path, b).unwrap();
    let (code, _, stderr) = run(&path.to_string_lossy(), "push");
    assert!(code != 0 && stderr.contains(UNREADABLE_EVENT), "the guard read only the first of two values: {code} {stderr}");
}
