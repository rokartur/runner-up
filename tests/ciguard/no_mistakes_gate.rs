use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use crate::release_ci_exclusions::{PathFilter, parse_workflow_on, pull_request_path_filter};
use crate::{GATE_WORKFLOW_PATH, REQUIRED_CHECK_CONTEXT, WorkflowFile, WorkflowJob, WorkflowStep, look_path, read_workflow, run, scalar};

/// release.yml's release check. The required check itself is decided by the
/// shared composite action the gate workflow delegates to (tested upstream in
/// the no-mistakes repository); release.yml's release-pr-gate-status job runs
/// this script to decide whether a PR is release-please's own, because GitHub
/// creates no workflow runs on a GITHUB_TOKEN PR and nothing else can report
/// the required contexts there.
const GATE_SCRIPT_PATH: &str = "./.github/scripts/no-mistakes-gate.sh";

/// The event-payload facts v1.80.1 must read live from one GitHub API response
/// instead of accepting as action inputs.
const CACHED_PR_LOOKUP_INPUTS: [&str; 5] = ["pr-body", "pr-head-sha", "pr-head-ref", "pr-author", "pr-number"];

/// The immutable commit the workflow must delegate to. A mutable ref such as
/// @main would let the pull request under judgement rewrite its own judge;
/// bumping this is a separate, deliberate pull request.
const REQUIRE_ACTION_PIN: &str = "kunchenguid/no-mistakes/.github/actions/require-no-mistakes@f6441c96c352a18b9cadcaef6b6c7017e9ac3970";

const NO_MISTAKES_SIGNATURE: &str = "## Pipeline\n\nUpdates from [git push no-mistakes](https://github.com/kunchenguid/no-mistakes)\n";
/// The compact v1 comment's head_sha.
const ATTESTATION_HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
/// The compact v1 comment no-mistakes >= 1.46.0 writes.
const COMPLETED_ATTESTATION: &str = r#"<!-- no-mistakes-pipeline-attestation:v1 {"head_sha":"0123456789abcdef0123456789abcdef01234567","steps":[{"step":"intent","status":"completed"},{"step":"rebase","status":"completed"},{"step":"review","status":"completed"},{"step":"test","status":"completed"},{"step":"document","status":"completed"},{"step":"lint","status":"completed"},{"step":"push","status":"completed"},{"step":"pr","status":"running"},{"step":"ci","status":"pending"}]} -->"#;
const RELEASE_BODY: &str = ":robot: I have created a release *beep* *boop*\n---\n\n## [2.1.1](https://example.invalid)\n\n---\nThis PR was generated with [Release Please](https://github.com/googleapis/release-please). See [documentation](https://github.com/googleapis/release-please#release-please).";
const REPO: &str = "kunchenguid/runner-up";
const FORK_REPO: &str = "someone-else/runner-up";

fn no_mistakes_body() -> String {
    format!("{NO_MISTAKES_SIGNATURE}\n{COMPLETED_ATTESTATION}\n")
}

// The required check on every ordinary pull request comes from the pinned
// shared action, with the bot exemptions as its input and the job always
// running. The gate script no longer exempts bots, so this workflow is the
// only place those exemptions live.
#[test]
fn gate_workflow_delegates_to_pinned_action() {
    let wf = read_workflow(GATE_WORKFLOW_PATH);
    let (job_id, job) =
        required_check_job(&wf).unwrap_or_else(|| panic!("{GATE_WORKFLOW_PATH} has no job named {REQUIRED_CHECK_CONTEXT:?}"));
    // A skipped job never reports the required context, so the PR would block
    // on a status that can never arrive: exemptions ride as action inputs.
    assert!(
        scalar(&job.condition).trim().is_empty(),
        "job {job_id:?} must not carry a job-level if:, a skipped job never reports the required check"
    );
    let step = job
        .steps
        .iter()
        .find(|s| s.uses == REQUIRE_ACTION_PIN)
        .unwrap_or_else(|| panic!("job {job_id:?} does not use the pinned shared action {REQUIRE_ACTION_PIN:?}"));
    let exempt = step.with.get("exempt-authors").map(scalar).unwrap_or_default();
    let exempt: Vec<_> = exempt.split([',', '\n', '\r']).map(str::trim).collect();
    for login in ["github-actions[bot]", "dependabot[bot]"] {
        assert!(exempt.contains(&login), "job {job_id:?} must exempt {login:?} via the action's exempt-authors input");
    }
}

fn required_check_job(wf: &WorkflowFile) -> Option<(&String, &WorkflowJob)> {
    wf.jobs.iter().find(|(_, job)| job.name == REQUIRED_CHECK_CONTEXT)
}

/// What the gate script can see of a pull request: the variables release.yml's
/// release-pr-gate-status job exports from the pull request's API reading.
/// author and head_sha are exported too, though the script must ignore them, so
/// a case can prove neither one passes a PR.
struct PullRequest {
    number: u32,
    body: String,
    author: &'static str,
    head_ref: &'static str,
    head_repo: &'static str,
    head_sha: &'static str,
}

/// Runs the gate script from the repository root, as release.yml does, and
/// reports whether it accepts the pull request. None: no bash here.
fn run_gate(pr: &PullRequest) -> Option<(bool, String)> {
    let bash = look_path("bash")?;
    let mut cmd = Command::new(bash);
    cmd.arg(GATE_SCRIPT_PATH);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PR_") {
            cmd.env_remove(name);
        }
    }
    cmd.env("PR_NUMBER", pr.number.to_string())
        .env("PR_BODY", &pr.body)
        .env("PR_AUTHOR", pr.author)
        .env("PR_HEAD_REF", pr.head_ref)
        .env("PR_HEAD_REPO", pr.head_repo)
        .env("PR_HEAD_SHA", pr.head_sha)
        .env("PR_BASE_REPO", REPO);
    let (code, output) = run(&mut cmd, Duration::from_secs(60));
    assert!(code.is_some(), "gate script ended by a signal: {output}");
    Some((code == Some(0), output))
}

// release.yml stamps the required statuses only on a pull request this script
// accepts, so it must accept exactly release-please's own release PR: a
// reserved branch prefix, a same-repository head and the Release Please
// footer. Author identity and a no-mistakes attestation must never be enough,
// since the shared action, not this script, judges those (RUU-54 audit, fix 5).
#[test]
fn no_mistakes_gate_decisions() {
    let pr = |number, body: &str, author, head_ref, head_repo| PullRequest {
        number,
        body: body.to_string(),
        author,
        head_ref,
        head_repo,
        head_sha: "",
    };
    let release_branch = "release-please--branches--main";
    let cases = [
        // The live shape of a release-please release PR.
        ("release-please release PR is exempt", pr(78, RELEASE_BODY, "github-actions[bot]", release_branch, REPO), true),
        // Structural, not identity: the same PR under a PAT identity, which is
        // what release-please would become if it ever used one.
        (
            "release-please release PR authored by a human identity is exempt",
            pr(79, RELEASE_BODY, "kunchenguid", release_branch, REPO),
            true,
        ),
        ("legacy release-please branch prefix is exempt", pr(80, RELEASE_BODY, "kunchenguid", "release-please/branches/main", REPO), true),
        (
            "human PR without the signature fails",
            pr(84, "## Summary\n\nA hand-written pull request.", "kunchenguid", "fix/something", REPO),
            false,
        ),
        ("empty body fails", pr(85, "", "kunchenguid", "fix/something", REPO), false),
        ("borrowed release branch name alone fails", pr(86, "## Summary\n\nNot a release PR.", "kunchenguid", release_branch, REPO), false),
        ("fork copying the release body fails", pr(87, RELEASE_BODY, "outside-contributor", release_branch, FORK_REPO), false),
        ("release body on an ordinary same-repo branch fails", pr(88, RELEASE_BODY, "kunchenguid", "feat/pretend", REPO), false),
        // The script used to exempt bots; that exemption now lives only in the
        // shared action's exempt-authors input.
        ("bot author on an ordinary branch fails", pr(81, "chore: routine bot update", "github-actions[bot]", "chore/bot", REPO), false),
        // The script used to pass a completed attestation bound to the head.
        // Even with the head exported, it must not stamp a release status.
        (
            "completed attestation bound to the head fails",
            PullRequest { head_sha: ATTESTATION_HEAD_SHA, ..pr(83, &no_mistakes_body(), "kunchenguid", release_branch, REPO) },
            false,
        ),
    ];
    for (name, pr, pass) in cases {
        let Some((got, output)) = run_gate(&pr) else {
            eprintln!("bash not available; the gate script runs on ubuntu-latest");
            return;
        };
        assert_eq!(got, pass, "{name}: gate pass = {got}, want {pass}\n{output}");
    }
}

// GitHub runs no workflow on a release-please PR, so every required context
// must be stamped there or the release PR can never merge; and only the
// structural release check may stamp them.
#[cfg(unix)]
#[test]
fn release_workflow_gate_status_wiring() {
    let step = load_release_gate_status_step();

    let (output, posts) = run_release_gate_status_step(&step, RELEASE_BODY);
    let posts_trimmed = posts.strip_prefix("CALL\n").unwrap_or(&posts);
    let calls: Vec<_> = posts_trimmed.split("CALL\n").collect();
    assert_eq!(calls.len(), crate::REQUIRED_CHECK_CONTEXTS.len(), "one status write per required context\nrecorded calls:\n{posts}");
    for (call, context) in calls.iter().zip(crate::REQUIRED_CHECK_CONTEXTS) {
        for want in [
            "ARG=--method\nARG=POST".to_string(),
            format!("ARG=repos/{REPO}/statuses/{ATTESTATION_HEAD_SHA}"),
            "ARG=state=success".to_string(),
            format!("ARG=context={context}\n"),
            "ARG=target_url=https://example.invalid/actions/runs/123".to_string(),
        ] {
            assert!(call.contains(&want), "status write missing {want:?}\noutput:\n{output}\nrecorded calls:\n{posts}");
        }
    }

    for (name, body) in [
        ("release branch without the Release Please footer", "not a release-please body".to_string()),
        // A completed attestation bound to the release PR's head cannot widen the structural path.
        ("attestation-only release PR", no_mistakes_body()),
    ] {
        let (output, posts) = run_release_gate_status_step(&step, &body);
        assert!(posts.is_empty(), "{name} wrote a status:\n{posts}");
        assert!(output.contains("not a structural release-please PR"), "{name}: missing rejection output:\n{output}");
    }
}

#[cfg(unix)]
fn load_release_gate_status_step() -> WorkflowStep {
    let wf = read_workflow(crate::RELEASE_WORKFLOW_PATH);
    let job = wf.jobs.get("release-pr-gate-status").expect("release workflow has no release-pr-gate-status job");
    let statuses = job.permissions.as_ref().and_then(|p| p.get("statuses")).cloned().unwrap_or_default();
    assert_eq!(statuses, "write", "release-pr-gate-status statuses permission");
    assert!(
        job.steps.iter().any(|s| s.uses.starts_with("actions/checkout@")),
        "release-pr-gate-status must check out the repository before running its local gate script"
    );
    let step = job
        .steps
        .iter()
        .find(|s| s.name == "Publish the required statuses on the release PR")
        .expect("release workflow has no gate status publishing step");
    assert!(!step.run.trim().is_empty(), "release gate status step has no executable run block");
    step.clone()
}

#[cfg(unix)]
fn shell_assignment(name: &str, value: &str) -> String {
    format!("{name}='{}'", value.replace('\'', "'\\''"))
}

/// Runs the publishing step's script against a stand-in gh, returning the
/// step's output and the status writes gh recorded.
#[cfg(unix)]
fn run_release_gate_status_step(step: &WorkflowStep, body: &str) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let fake_bin = dir.path().join("bin");
    std::fs::create_dir(&fake_bin).unwrap();
    let post_log = dir.path().join("posts.log");
    crate::write_programs(
        &fake_bin,
        &[(
            "gh",
            r#"#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  *"pulls?state=open"*)
    echo 78
    ;;
  *"pulls/78"*)
    printf '%s\n' "$GH_PULL_ASSIGNMENTS"
    ;;
  *"statuses/"*)
    {
      echo CALL
      printf 'ARG=%s\n' "$@"
    } >> "$GH_POST_LOG"
    ;;
  *)
    echo "unexpected gh invocation: $*" >&2
    exit 64
    ;;
esac
"#,
        )],
    );

    let assignments = [
        shell_assignment("PR_NUMBER", "78"),
        shell_assignment("PR_HEAD_REF", "release-please--branches--main"),
        shell_assignment("PR_HEAD_REPO", REPO),
        shell_assignment("PR_BASE_REPO", REPO),
        shell_assignment("PR_HEAD_SHA", ATTESTATION_HEAD_SHA),
        shell_assignment("PR_BODY", body),
    ];
    let path =
        std::env::join_paths(std::iter::once(fake_bin.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))
            .unwrap();
    let values: BTreeMap<&str, std::ffi::OsString> = BTreeMap::from([
        ("GH_TOKEN", "test-token".into()),
        ("REPO", REPO.into()),
        ("RUN_URL", "https://example.invalid/actions/runs/123".into()),
        ("GH_PULL_ASSIGNMENTS", assignments.join(" ").into()),
        ("GH_POST_LOG", post_log.clone().into()),
        ("PATH", path),
    ]);
    for name in ["GH_TOKEN", "REPO", "RUN_URL"] {
        assert!(step.env.contains_key(name), "release gate status step does not declare required environment input {name:?}");
    }
    for name in step.env.keys() {
        assert!(values.contains_key(name.as_str()), "release gate status step declares unhandled environment input {name:?}");
    }

    let bash = crate::look_path("bash").expect("bash");
    let mut cmd = Command::new(bash);
    cmd.args(["-e", "-c", &step.run]);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PR_") {
            cmd.env_remove(name);
        }
    }
    cmd.envs(&values);
    let (code, output) = run(&mut cmd, Duration::from_secs(60));
    assert_eq!(code, Some(0), "run release gate status step:\n{output}");
    let posts = std::fs::read_to_string(&post_log).unwrap_or_default();
    (output, posts)
}

// The required check must always report on a pull request. A path filter would
// silently withhold the status and block the PR forever.
#[test]
fn gate_workflow_has_no_path_filter() {
    let data = std::fs::read_to_string(GATE_WORKFLOW_PATH).unwrap();
    let on = parse_workflow_on(&data).unwrap();
    let filter = pull_request_path_filter(&on).unwrap().expect("gate workflow must trigger on pull_request");
    assert_eq!(filter, PathFilter::None, "gate workflow must not filter by path");
}

// Locks the v1.80.1 caller contract on the shipped workflow: the shared action
// reads the live PR body and head from one API response, which fails closed
// without pull-requests: read. A pin-only assertion stayed green if a later
// edit restored cached PR inputs, restored the Fetch current PR body step, or
// dropped that permission.
#[test]
fn gate_workflow_live_lookup() {
    let violations = gate_live_lookup_violations(&read_workflow(GATE_WORKFLOW_PATH));
    assert!(violations.is_empty(), "live lookup contract broken:\n{}", violations.join("\n"));
}

#[test]
fn gate_live_lookup_violations_reject_stale_caller() {
    let action = WorkflowStep {
        uses: REQUIRE_ACTION_PIN.into(),
        with: BTreeMap::from([("exempt-authors".into(), "github-actions[bot]".into())]),
        ..Default::default()
    };
    let perms = |pairs: &[(&str, &str)]| Some(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
    let read_pr = perms(&[("contents", "read"), ("pull-requests", "read")]);
    let violations =
        |workflow_perm, job_perm, steps| gate_live_lookup_violations(&live_lookup_workflow(workflow_perm, job_perm, steps)).join("\n");

    let got = violations(perms(&[("contents", "read")]), None, vec![action.clone()]);
    assert!(got.contains("pull-requests"), "missing pull-requests permission: want a violation, got {got:?}");

    let got = violations(read_pr.clone(), perms(&[("contents", "read")]), vec![action.clone()]);
    assert!(got.contains("pull-requests"), "job-level permissions drop pull-requests read: want a violation, got {got:?}");

    let fetch = WorkflowStep { name: "Fetch current PR body".into(), run: "gh api repos/.../pulls/...".into(), ..Default::default() };
    let got = violations(read_pr.clone(), None, vec![fetch, action.clone()]);
    assert!(got.contains("inline"), "restored fetch step: want an inline fetch-step violation, got {got:?}");

    let mut stale = action.clone();
    for (key, value) in [
        ("pr-body", "${{ steps.pr-body.outputs.body }}"),
        ("pr-head-sha", "${{ github.event.pull_request.head.sha }}"),
        ("pr-head-ref", "${{ github.event.pull_request.head.ref }}"),
        ("pr-author", "${{ github.event.pull_request.user.login }}"),
        ("pr-number", "${{ github.event.pull_request.number }}"),
    ] {
        stale.with.insert(key.into(), value.into());
    }
    let got = violations(read_pr.clone(), None, vec![stale]);
    for key in CACHED_PR_LOOKUP_INPUTS {
        assert!(got.contains(key), "restored cached PR inputs: want a cached {key} input violation, got {got:?}");
    }

    let got = violations(read_pr, None, vec![action]);
    assert!(got.is_empty(), "clean caller reported violations:\n{got}");
}

fn live_lookup_workflow(
    workflow_perm: Option<BTreeMap<String, String>>,
    job_perm: Option<BTreeMap<String, String>>,
    steps: Vec<WorkflowStep>,
) -> WorkflowFile {
    let job = WorkflowJob { name: REQUIRED_CHECK_CONTEXT.into(), permissions: job_perm, steps, ..Default::default() };
    WorkflowFile { permissions: workflow_perm, jobs: BTreeMap::from([("check".into(), job)]) }
}

fn gate_live_lookup_violations(wf: &WorkflowFile) -> Vec<String> {
    let Some((job_id, job)) = required_check_job(wf) else { return vec![format!("no job named {REQUIRED_CHECK_CONTEXT:?}")] };
    let mut out = Vec::new();
    let perm = job.permissions.as_ref().or(wf.permissions.as_ref()).and_then(|p| p.get("pull-requests")).cloned().unwrap_or_default();
    if perm != "read" && perm != "write" {
        out.push(format!("job {job_id:?} effective pull-requests permission {perm:?}, want read"));
    }
    for (i, step) in job.steps.iter().enumerate() {
        if !step.run.trim().is_empty() {
            out.push(format!("job {job_id:?} step {i} still runs inline commands; live lookup must not fetch the PR body first"));
        }
        if !step.uses.contains("/.github/actions/require-no-mistakes@") {
            continue;
        }
        for key in CACHED_PR_LOOKUP_INPUTS {
            if step.with.contains_key(key) {
                out.push(format!(
                    "job {job_id:?} passes cached {key} into require-no-mistakes; live lookup reads body and head from one response"
                ));
            }
        }
    }
    out
}
