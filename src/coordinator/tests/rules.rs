// Intent (README, "Order rules"): a freed place goes to the job the
// configured order rules pick, across repositories and pools, with the pull
// request labels, run workflows and learned runtimes the coordinator reads
// from GitHub. What each rule does is owned by plan (plan/tests.rs).

use std::collections::BTreeMap;
use std::time::Duration;

use super::service::{PR184, PUBLISH, RELEASE, SUITE, VERIFY};
use super::*;
use crate::coordinator::GithubApi;
use crate::testserver::{Response, Server};

/// One machine with one place for every pool, so the order the place takes
/// waiting jobs in is the order the rules pick.
pub fn one_place(dir: &Path, order: &[&str]) -> Config {
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        order: strings(order),
        github: config::GitHub { repos: strings(&["octo-org/app", "octo-user/tool", "octo-user/wallet"]), ..Default::default() },
        pools: vec![pool("ru-linux"), pool("ru-large")],
        machines: vec![config::Machine {
            name: "box".into(),
            ssh: strings(&["ci@box"]),
            places: vec![Place {
                id: "box-1".into(),
                provider: config::PROVIDER_DOCKER.into(),
                pools: strings(&["ru-linux", "ru-large"]),
                cores: 2,
                memory_mb: 4096,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Feeds every waiting job while the place is busy, then frees it one job at
/// a time and returns the jobs it took, in order.
pub fn take_order(cfg: Config, setup: impl FnOnce(&Harness)) -> Vec<String> {
    let f = load_fixture("waiting-jobs.json");
    let busy = occupy(&cfg, "box-1");
    let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
    setup(&h);
    h.set_now(f.now);
    h.feed(&f);
    drop(busy);
    let mut got = Vec::new();
    h.place();
    for _ in 0..=f.messages.len() {
        let Some(job) = h.placed().get("box-1").cloned() else {
            break;
        };
        got.push(format!("{job} {}", request_name(h.run("box-1").request_id)));
        h.finish_and_place("box-1");
    }
    got
}

pub fn request_name(id: i64) -> &'static str {
    match id {
        9101 => "#9101",
        9102 => "#9102",
        9103 => "#9103",
        7201 => "#7201",
        6301 => "#6301",
        9104 => "#9104",
        _ => "",
    }
}

pub fn want_order(got: &[String], want: &[String]) {
    assert!(got == want, "the place took\n  {}\nwant\n  {}", got.join("\n  "), want.join("\n  "));
}

fn o(job: &str, request: &str) -> String {
    format!("{job} {request}")
}

// The order rules themselves are owned by plan (plan/tests.rs); this test owns
// their wiring: the labels the label reader finds, the run workflows the
// workflow reader finds and the runtimes GitHub's completion messages teach all
// reach the rules, and the place takes jobs in the order they give. Each
// reading moves one job: PR #186 goes first only by its label, verify counts as
// release only by its run's top-level workflow, and the suite beats PR #184
// only by its learned runtime.
#[test]
fn order_rules_read_labels_workflows_and_learned_runtimes() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = one_place(dir.path(), &["label:urgent", "workflow:release", "shortest"]);
    let got = take_order(cfg, |h| {
        label_pr186(h);
        h.svc.lock().workflow_reader = Some(Arc::new(|_: &Ctx, _: &str, id: i64| {
            // The runs' top-level workflows. app's run 39361345555 is its
            // release, which calls verify.yml as a reusable workflow.
            Ok(match id {
                39361345555 | 39370000002 => ".github/workflows/release.yml",
                39372831799 | 39381269010 => ".github/workflows/journeys.yml",
                39370000001 => ".github/workflows/suite.yml",
                _ => "",
            }
            .to_string())
        }));
        // GitHub's completion of an earlier successful suite run teaches its
        // runtime, 4 minutes from runner assignment to finish; a cancelled
        // verify teaches nothing.
        let at = Time::parse("2026-10-19T22:00:00Z").unwrap();
        h.set_now(at);
        let completed =
            |result: &str, runner: &str, repo: &str, owner: &str, name: &str, run: i64, workflow: &str, took: u64| scaleset::JobMessage {
                result: result.into(),
                runner_name: runner.into(),
                repository_name: repo.into(),
                owner_name: owner.into(),
                job_display_name: name.into(),
                workflow_run_id: run,
                job_workflow_ref: workflow.into(),
                runner_assign_time: at.sub_dur(Duration::from_secs(took)),
                finish_time: at,
                ..Default::default()
            };
        let msg = Message {
            message_id: 1,
            job_completed: vec![
                completed(
                    "succeeded",
                    "runner-up-box-1-1",
                    "tool",
                    "octo-user",
                    "suite",
                    39360000001,
                    "octo-user/tool/.github/workflows/suite.yml@refs/heads/main",
                    240,
                ),
                completed(
                    "canceled",
                    "runner-up-box-1-2",
                    "app",
                    "octo-org",
                    "verify / linux",
                    39360000002,
                    "octo-org/app/.github/workflows/verify.yml@refs/heads/main",
                    60,
                ),
            ],
            ..Default::default()
        };
        h.handle("octo-user/tool", "ru-linux", &msg);
    });
    let want = [
        o("octo-org/app affected-journeys", "#9104"),
        o(VERIFY, "#9101"),
        o(PUBLISH, "#9103"),
        o(RELEASE, "#6301"),
        o(SUITE, "#7201"),
        o(PR184, "#9102"),
    ];
    want_order(&got, &want);
}

/// Labels app's open pull request #186 urgent.
fn label_pr186(h: &Harness) {
    h.svc.lock().label_reader = Some(Arc::new(|_: &Ctx, repo: &str| {
        let mut out = BTreeMap::new();
        if repo == "octo-org/app" {
            out.insert("refs/pull/186/merge".to_string(), BTreeMap::from([("urgent".to_string(), true)]));
        }
        Ok(out)
    }));
}

// pull_labels reads labels from the repository's own open pull requests only:
// a fork's pull request cannot label its way to the front.
#[test]
fn pull_request_labels_come_only_from_the_repository() {
    let server = Server::start(|r| {
        if r.path != "/repos/octo-org/app/pulls" || r.header("Authorization") != "Bearer local-only" {
            return Response::new(404, "unexpected");
        }
        // The fields GitHub's list of pull requests carries that the reader
        // uses, for one pull request from a branch and one from a fork.
        Response::json(
            200,
            &serde_json::json!([
                {"number": 186, "state": "open", "labels": [{"name": "urgent"}],
                    "head": {"ref": "fix/session-header", "repo": {"full_name": "octo-org/app"}}},
                {"number": 187, "state": "open", "labels": [{"name": "urgent"}],
                    "head": {"ref": "main", "repo": {"full_name": "a-contributor/app"}}},
            ]),
        )
    });
    let mut api = GithubApi::new(Arc::new(|_: &Ctx| Ok("local-only".to_string())));
    api.base = server.url.clone();
    let got = api.pull_labels(&Ctx::background(), "octo-org/app").unwrap();
    for r in ["refs/pull/186/merge", "refs/pull/186/head", "refs/heads/fix/session-header"] {
        assert!(got.get(r).is_some_and(|l| l.get("urgent") == Some(&true)), "{r} is not labeled urgent: {got:?}");
    }
    for r in ["refs/pull/187/merge", "refs/heads/main"] {
        assert!(got.get(r).is_none_or(|l| l.is_empty()), "the fork's pull request labeled {r}: {got:?}");
    }
}

// Intent (README, "Order rules", last:NAME). last:NAME puts a pool's jobs
// behind every other job, so Linux guests sharing a Mac's budget never take the
// room a waiting Mac job needs: that job either starts or keeps its room first,
// and the guests use only what is left. The 14-core budget, 12-core Mac pool
// and 2-core guests are the README's illustration in "Job sizes and readings".
#[test]
fn last_pool_leaves_room_for_other_jobs_first() {
    let mini = |dir: &Path, order: &[&str]| {
        let mut cfg = one_place(dir, order);
        (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 8192);
        (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (12, 32768);
        let m = &mut cfg.machines[0];
        (m.cores, m.room_cores) = (22, 8);
        let place = |id: &str, provider: &str, image: &str, pool: &str, cores, memory_mb| Place {
            id: id.into(),
            provider: provider.into(),
            image: image.into(),
            pools: strings(&[pool]),
            cores,
            memory_mb,
            ..Default::default()
        };
        m.places = vec![
            place("mac", config::PROVIDER_TART, "ci-base", "ru-large", 12, 32768),
            place("guest-1", config::PROVIDER_CONTAINER, "runner-up-linux", "ru-linux", 2, 8192),
            place("guest-2", config::PROVIDER_CONTAINER, "runner-up-linux", "ru-linux", 2, 8192),
        ];
        cfg
    };
    // Every job waits while the places are busy, then they free together.
    let place = |cfg: Config| {
        let f = load_fixture("waiting-jobs.json");
        let busy: Vec<File> = ["mac", "guest-1", "guest-2"].iter().map(|id| occupy(&cfg, id)).collect();
        let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
        h.set_now(f.now);
        h.feed(&f);
        drop(busy);
        h.place();
        h
    };

    // Without the rule the older Linux jobs take both guests first, and the
    // 12-core publish no longer fits beside their 4 cores.
    let d1 = tempfile::tempdir().unwrap();
    let h = place(mini(d1.path(), &["waited:60m", "shortest"]));
    h.want(&[("guest-1", VERIFY), ("guest-2", PR184)]);

    // With it publish starts first, and one guest takes the 2 cores left.
    let d2 = tempfile::tempdir().unwrap();
    let h = place(mini(d2.path(), &["last:ru-linux", "waited:60m", "shortest"]));
    h.want(&[("mac", PUBLISH), ("guest-1", VERIFY)]);
    h.launcher.finish("mac");
    h.launcher.finish("guest-1");
}
