// Intent (README, "Order rules"): each rule puts the jobs it matches first,
// except last:, which puts them last; later rules order jobs tied by earlier
// ones, and the oldest job breaks a final tie. The rules are plain functions
// of the waiting jobs, so this module owns what each rule kind does; the
// coordinator's own test owns only that the label, workflow and runtime
// readings reach these rules.

use super::*;
use crate::config::Pool;

/// The jobs that waited for one machine's places at the same time, from the
/// coordinator's recorded scale set messages, as the coordinator turns a
/// JobAvailable message into a waiting job, with the moment the order was
/// taken.
fn waiting() -> (Vec<Job>, Time) {
    let path = format!("{}/testdata/coordinator/messages/waiting-jobs.json", env!("CARGO_MANIFEST_DIR"));
    let f: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let now = Time::parse(f["now"].as_str().unwrap()).unwrap();
    let mut jobs = vec![];
    for m in f["messages"].as_array().unwrap() {
        let repo = m["repo"].as_str().unwrap();
        for a in m["message"]["JobAvailableMessages"].as_array().unwrap() {
            jobs.push(Job {
                target: repo.into(),
                repo: repo.into(),
                pool: m["label"].as_str().unwrap().into(),
                request_id: a["runnerRequestId"].as_i64().unwrap(),
                name: a["jobDisplayName"].as_str().unwrap().into(),
                workflow: a["jobWorkflowRef"].as_str().unwrap().into(),
                run_id: a["workflowRunId"].as_i64().unwrap(),
                queue_time: Time::parse(a["queueTime"].as_str().unwrap()).unwrap(),
                ..Default::default()
            });
        }
    }
    assert_eq!(jobs.len(), 6, "the recording holds {} waiting jobs, want 6", jobs.len());
    (jobs, now)
}

// The recorded jobs by their runner request, oldest first. ru-large jobs are
// publish and the wallet's release; the rest ask for ru-linux.
const VERIFY: &str = "verify"; // 9101, app's release run, verify.yml, waited 3h46m
const PR184: &str = "pr184"; // 9102, app's pull request #184, waited 1h37m
const PUBLISH: &str = "publish"; // 9103, app's release run, release.yml, ru-large, waited 1h27m
const SUITE: &str = "suite"; // 7201, tool's suite, waited 1h20m
const RELEASE: &str = "release"; // 6301, wallet's release, ru-large, waited 1h10m
const PR186: &str = "pr186"; // 9104, app's pull request #186, waited 5m

fn name(id: i64) -> &'static str {
    match id {
        9101 => VERIFY,
        9102 => PR184,
        9103 => PUBLISH,
        7201 => SUITE,
        6301 => RELEASE,
        9104 => PR186,
        _ => "",
    }
}

fn order_of(jobs: &[Job]) -> Vec<&'static str> {
    jobs.iter().map(|j| name(j.request_id)).collect()
}

/// A setup with the recording's two pools and the order rules.
fn setup_with(rules: &[&str]) -> Config {
    Config {
        order: rules.iter().map(|s| s.to_string()).collect(),
        pools: vec![
            Pool { label: "ru-linux".into(), ..Default::default() },
            Pool { label: "ru-large".into(), ..Default::default() },
        ],
        ..Default::default()
    }
}

type Fill = Box<dyn Fn(&mut Job)>;

fn label_pr186() -> Fill {
    Box::new(|j| {
        if name(j.request_id) == PR186 {
            j.labels = BTreeMap::from([("urgent".to_string(), true)]);
        }
    })
}

/// app's run 39361345555 is its release, which calls verify.yml as a
/// reusable workflow; the other runs' top-level files are their own.
fn run_workflows() -> Fill {
    Box::new(|j| {
        j.run_workflow = if j.run_id == 39361345555 {
            "octo-org/app/.github/workflows/release.yml@refs/heads/main".into()
        } else {
            j.workflow.clone()
        }
    })
}

fn learned(m: &[(&'static str, u64)]) -> Fill {
    let m: HashMap<&str, u64> = m.iter().copied().collect();
    Box::new(move |j| j.estimate = Duration::from_secs(60 * m.get(name(j.request_id)).copied().unwrap_or(0)))
}

#[test]
fn each_order_rule_kind_puts_its_jobs_first() {
    let cases: Vec<(&str, Vec<&str>, Vec<Fill>, [&str; 6])> = vec![
        (
            "the default order, nothing learned, is oldest first",
            vec![],
            vec![],
            [VERIFY, PR184, PUBLISH, SUITE, RELEASE, PR186],
        ),
        ("pool: puts its pool's jobs first", vec!["pool:ru-large"], vec![], [PUBLISH, RELEASE, VERIFY, PR184, SUITE, PR186]),
        (
            "last: puts its pool's jobs behind the rest",
            vec!["last:ru-linux"],
            vec![],
            [PUBLISH, RELEASE, VERIFY, PR184, SUITE, PR186],
        ),
        (
            "last: of the other pool keeps the rest in age order",
            vec!["last:ru-large"],
            vec![],
            [VERIFY, PR184, SUITE, PR186, PUBLISH, RELEASE],
        ),
        (
            "label: puts the labeled pull request first",
            vec!["label:urgent"],
            vec![label_pr186()],
            [PR186, VERIFY, PR184, PUBLISH, SUITE, RELEASE],
        ),
        (
            "waited: puts jobs past the wait first, oldest first, ahead of shorter ones",
            vec!["waited:90m", "shortest"],
            vec![learned(&[(PR184, 2), (SUITE, 4)])],
            [VERIFY, PR184, SUITE, PUBLISH, RELEASE, PR186],
        ),
        (
            "workflow: reads the run's top-level file, so a reusable verify counts",
            vec!["workflow:release"],
            vec![run_workflows()],
            [VERIFY, PUBLISH, RELEASE, PR184, SUITE, PR186],
        ),
        (
            "workflow: falls back to the job's own file when the run's is unread",
            vec!["workflow:release"],
            vec![],
            [PUBLISH, RELEASE, VERIFY, PR184, SUITE, PR186],
        ),
        (
            "workflow: takes a glob list",
            vec!["workflow:nightly,jour*"],
            vec![],
            [PR184, PR186, VERIFY, PUBLISH, SUITE, RELEASE],
        ),
        (
            "shortest puts the shortest learned runtime first",
            vec!["shortest"],
            vec![learned(&[(SUITE, 4)])],
            [SUITE, VERIFY, PR184, PUBLISH, RELEASE, PR186],
        ),
        (
            "shortest counts a job never seen to succeed as 15 minutes",
            vec!["shortest"],
            vec![learned(&[(VERIFY, 20), (RELEASE, 14)])],
            [RELEASE, PR184, PUBLISH, SUITE, PR186, VERIFY],
        ),
        (
            "an earlier rule wins over a later one",
            vec!["pool:ru-large", "label:urgent", "waited:60m", "shortest"],
            vec![label_pr186()],
            [PUBLISH, RELEASE, PR186, VERIFY, PR184, SUITE],
        ),
        (
            "a later rule orders the earlier rule's ties",
            vec!["label:urgent", "workflow:release", "shortest"],
            vec![label_pr186(), run_workflows(), learned(&[(SUITE, 4)])],
            [PR186, VERIFY, PUBLISH, RELEASE, SUITE, PR184],
        ),
    ];
    for (why, rules, fills, want) in cases {
        let (mut jobs, now) = waiting();
        for j in &mut jobs {
            for f in &fills {
                f(j);
            }
        }
        let rules = parse_order(&setup_with(&rules)).unwrap_or_else(|e| panic!("{why}: {e}"));
        // Rank must not lean on the order jobs arrive in.
        jobs.reverse();
        assert_eq!(order_of(&rank(&jobs, &rules, now)), want, "{why}");
    }
}

// Intent (README, "Order rules" and "Setup file"): a rule the coordinator
// could not follow, or a pool: or last: rule naming a pool the setup does not
// have, which would order nothing and say nothing, is refused when the setup
// loads.
#[test]
fn order_rules_are_checked() {
    for bad in [
        "label:",
        "pool:",
        "pool:ru-lagre",
        "last:",
        "last:ru-linx",
        "waited:soon",
        "waited:-5m",
        "workflow:[",
        "workflow:release,",
        "shortest:yes",
        "fastest",
    ] {
        match parse_order(&setup_with(&[bad])) {
            Err(e) if e.contains(&format!("\"{bad}\"")) => {}
            other => panic!("order rule {bad:?}: the check said {other:?}"),
        }
    }
    parse_order(&setup_with(&[
        "pool:ru-large",
        "last:ru-linux",
        "label:urgent",
        "waited:30m",
        "workflow:release,deploy-*",
        "shortest",
    ]))
    .unwrap();
    assert_eq!(parse_order(&setup_with(&[])).unwrap().len(), DEFAULT_ORDER.len());
}

// Intent (README, "Order rules": label: matches jobs "in the same repository
// (not from forks), including runs on its branch"): a label reaches a job only
// through its own repository's pull request or branch ref, so a reusable
// workflow in another repository, a tag, or a ref shaped like a pull
// request's that is not one, cannot claim a label.
#[test]
fn only_the_repositorys_own_pull_request_or_branch_can_carry_a_label() {
    for (workflow, want) in [
        // The recording's refs.
        ("octo-org/app/.github/workflows/journeys.yml@refs/pull/184/merge", "refs/pull/184/merge"),
        ("octo-org/app/.github/workflows/verify.yml@refs/heads/main", "refs/heads/main"),
        ("octo-org/app/.github/workflows/journeys.yml@refs/pull/186/head", "refs/pull/186/head"),
        ("octo-org/app/.github/workflows/journeys.yml@refs/heads/fix/session-header", "refs/heads/fix/session-header"),
        // A reusable workflow from another repository.
        ("octo-org/shared/.github/workflows/journeys.yml@refs/pull/184/merge", ""),
        ("octo-org/app/.github/workflows/release.yml@refs/tags/v1.2.0", ""),
        ("octo-org/app/.github/workflows/journeys.yml@refs/pull/184/files", ""),
        ("octo-org/app/.github/workflows/journeys.yml@refs/pull/x184/merge", ""),
        ("octo-org/app/.github/workflows/journeys.yml@refs/heads/", ""),
        ("octo-org/app/.github/workflows/journeys.yml", ""),
    ] {
        let j = Job { repo: "octo-org/app".into(), workflow: workflow.into(), ..Default::default() };
        assert_eq!(j.label_ref(), want, "{workflow}");
    }
}

// Intent (README, "Order rules"): the coordinator reads pull request labels
// and run workflows from GitHub only when a rule uses them.
#[test]
fn rules_say_which_readings_they_need() {
    for (rules, labels, workflows) in [
        (vec![], false, false),
        (vec!["label:urgent", "shortest"], true, false),
        (vec!["workflow:release"], false, true),
        (vec!["pool:ru-large", "label:urgent", "workflow:release"], true, true),
    ] {
        let parsed = parse_order(&setup_with(&rules)).unwrap();
        assert_eq!(needs(&parsed), (labels, workflows), "{rules:?}");
    }
}
