// Intent (README, "Changing the setup"): serve watches its setup file and waits
// about a second after a change, because a pull rewrites the file. It checks it
// as check-config does and applies pools, places, sizes and order in place,
// without dropping running jobs or open sessions. It keeps the running setup
// when the new one fails.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::rules::{one_place, request_name, want_order};
use super::service::{PR184, PUBLISH, RELEASE, SUITE, VERIFY};
use super::*;
use crate::coordinator::setup::{SetupRead, SetupWatch};
use crate::plan;

// A pull rewrites the setup file in steps; the watcher sends nothing until the
// file has held one content for its quiet time, then sends that once. A file
// that cannot be read is sent the same way, for the coordinator to keep its
// running setup.
#[test]
fn setup_watch_sends_the_file_once_it_has_settled() {
    let old = serde_json::to_vec_pretty(&one_box(Path::new("/var/lib/runner-up"))).unwrap();
    let mut cfg = one_box(Path::new("/var/lib/runner-up"));
    cfg.pools.push(config::Pool { label: "ru-mac".into(), cores: 4, memory_mb: 8192, ..Default::default() });
    let pulled = serde_json::to_vec_pretty(&cfg).unwrap();
    let missing: SetupRead = Err("open config.json: no such file or directory".into());
    let start = Instant::now();
    let mut w = SetupWatch::new(Duration::from_secs(1), start);
    let ms = Duration::from_millis;
    let ok = |b: &[u8]| -> SetupRead { Ok(b.to_vec()) };
    let steps: Vec<(Duration, SetupRead, Option<SetupRead>)> = vec![
        (ms(0), ok(&old), None),
        // The pull truncates the file and writes it in two parts.
        (ms(250), ok(&pulled[..pulled.len() / 2]), None),
        (ms(500), ok(&pulled), None),
        (ms(1250), ok(&pulled), None),
        (ms(1500), ok(&pulled), Some(ok(&pulled))),
        (ms(1750), ok(&pulled), None),
        // The file goes missing for a while, then comes back.
        (ms(2000), missing.clone(), None),
        (ms(3000), missing.clone(), Some(missing.clone())),
        (ms(3250), ok(&pulled), None),
        (ms(4250), ok(&pulled), Some(ok(&pulled))),
    ];
    for (at, read, want) in steps {
        let got = w.next(read, start + at);
        assert!(got == want, "at {at:?} the watcher sent {:?}, want {:?}", got.map(|r| r.map(|b| b.len())), want.map(|r| r.map(|b| b.len())));
    }
}

fn held(cfg: &Config, id: &str) -> bool {
    lock::held_by(&cfg.place_dir(id).join("busy.lock")).unwrap()
}

fn reload(svc: &Service, mut next: Config) -> Arc<Config> {
    next.defaults();
    next.validate().unwrap();
    let rules = plan::parse_order(&next).unwrap();
    let next = Arc::new(next);
    assert_eq!(svc.reload(next.clone(), rules), Ok(true), "reload");
    next
}

fn docker(id: &str) -> Place {
    Place {
        id: id.into(),
        provider: config::PROVIDER_DOCKER.into(),
        pools: strings(&["ru-linux", "ru-large"]),
        cores: 2,
        memory_mb: 4096,
        ..Default::default()
    }
}

fn machine(name: &str, places: Vec<Place>) -> config::Machine {
    config::Machine {
        name: name.into(),
        ssh: strings(&[&format!("ci@{name}")]),
        cores: 4,
        memory_mb: 16384,
        room_cores: 1,
        room_memory_mb: 1024,
        places,
        ..Default::default()
    }
}

fn setup(dir: &Path, machines: Vec<config::Machine>) -> Config {
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        github: config::GitHub { repos: strings(&["octo-org/app", "octo-user/tool", "octo-user/wallet"]), ..Default::default() },
        pools: vec![pool("ru-linux"), pool("ru-large")],
        machines,
        ..Default::default()
    }
}

// A setup change that removes a place while its job runs leaves the job
// running, and the job's share keeps counting against its machine until it
// ends, through a coordinator restart too: a place the change adds there does
// not start a job that would not fit beside it.
#[test]
fn removed_place_keeps_its_share_until_its_job_ends() {
    let dir = tempfile::tempdir().unwrap();
    // One machine whose places may reserve 3 cores, with one 2-core place.
    let box_with = |p: &str| setup(dir.path(), vec![machine("box", vec![docker(p)])]);
    let launcher = FakeLauncher::new();
    let f = load_fixture("waiting-jobs.json");
    let mut h = new_harness(box_with("box-1"), launcher.clone(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", VERIFY)]);
    mark_started(&h.cfg, "box-1");

    // The setup now has box-2 on the machine in box-1's stead.
    let next = reload(&h.svc, box_with("box-2"));
    h.cfg = next.clone();
    h.tidy();
    h.place();
    assert!(
        held(&next, "box-1") && launcher.st().stopped.is_empty(),
        "box-1's job stopped at the setup change: stopped {:?}",
        launcher.st().stopped
    );
    h.want(&[]);
    assert_eq!(h.svc.in_use("box", "").cores, 2, "want box-1's running job's 2 cores reserved");

    // The coordinator restarts with the new setup while the job runs.
    let sets = h.sets.clone();
    drop(h);
    let h = new_harness((*next).clone(), launcher.clone(), sets);
    h.set_now(f.now.add(Duration::from_secs(60)));
    h.tidy();
    h.place();
    h.want(&[]);

    // The job ends. Its place is retired, and box-2 takes the next job.
    launcher.finish("box-1");
    h.advance(Duration::from_secs(60));
    h.tidy();
    h.place();
    h.want(&[("box-2", PR184)]);
    assert!(next.place_dir("box-1").join("last-place.json").exists(), "box-1's finished run was not retired");
    assert!(h.svc.lock().departed.is_empty(), "the coordinator still looks after {:?}", h.svc.lock().departed.keys());
}

// A setup change that moves a place to another machine while its job runs
// leaves the job's share on the machine the job runs on, until the job ends.
#[test]
fn moved_place_counts_where_its_job_runs() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = FakeLauncher::new();
    let f = load_fixture("waiting-jobs.json");
    let h = new_harness(
        setup(dir.path(), vec![machine("box", vec![docker("box-1")]), machine("big", vec![])]),
        launcher.clone(),
        Sets::new(),
    );
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", VERIFY)]);
    mark_started(&h.cfg, "box-1");

    // The setup now has box-1 on big.
    reload(&h.svc, setup(dir.path(), vec![machine("box", vec![]), machine("big", vec![docker("box-1")])]));
    let (b, big) = (h.svc.in_use("box", "").cores, h.svc.in_use("big", "").cores);
    assert!(b == 2 && big == 0, "box's places reserve {b} cores and big's {big}, want box-1's running job's 2 on box and none on big");

    // The job ends; box-1 counts nowhere until it takes a job on big.
    launcher.finish("box-1");
    h.tidy();
    let (b, big) = (h.svc.in_use("box", "").cores, h.svc.in_use("big", "").cores);
    assert!(b == 0 && big == 0, "after box-1's job box's places reserve {b} cores and big's {big}, want none");
}

// A changed order picks the job a freed place takes from the jobs already
// waiting, with no restart.
#[test]
fn changed_order_picks_the_next_job() {
    let dir = tempfile::tempdir().unwrap();
    let f = load_fixture("waiting-jobs.json");
    let busy = occupy(&one_place(dir.path(), &[]), "box-1");
    let h = new_harness(one_place(dir.path(), &[]), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed(&f);
    // The default order would give the freed place verify, which waited
    // longest; the new order puts ru-large's jobs first.
    reload(&h.svc, one_place(dir.path(), &["pool:ru-large", "waited:60m", "shortest"]));
    drop(busy);
    h.place();
    let mut got = Vec::new();
    for _ in 0..=f.messages.len() {
        let Some(job) = h.placed().remove("box-1") else { break };
        got.push(format!("{job} {}", request_name(h.run("box-1").request_id)));
        h.finish_and_place("box-1");
    }
    let o = |job: &str, request: &str| format!("{job} {request}");
    want_order(
        &got,
        &[
            o(PUBLISH, "#9103"),
            o(RELEASE, "#6301"),
            o(VERIFY, "#9101"),
            o(PR184, "#9102"),
            o(SUITE, "#7201"),
            o("octo-org/app affected-journeys", "#9104"),
        ],
    );
}

// A setup that moves the state folder, where running places hold their locks,
// is refused, and the coordinator places jobs as before.
#[test]
fn setup_moving_the_state_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_box(dir.path()), FakeLauncher::new(), Sets::new());
    let elsewhere = tempfile::tempdir().unwrap();
    let mut moved = one_box(elsewhere.path());
    moved.defaults();
    let got = h.svc.reload(Arc::new(moved), vec![]);
    assert!(
        got.as_ref().is_err_and(|e| e.contains("stateDir changes only when runner-up serve restarts")),
        "a setup moving the state folder: {got:?}"
    );
    assert!(Arc::ptr_eq(&h.svc.setup(), &h.cfg), "the refused setup replaced the running one");
    let f = load_fixture("waiting-jobs.json");
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    h.want(&[("box-1", VERIFY), ("box-2", PR184)]);
    assert!(!elsewhere.path().join("places").exists(), "the refused setup's state folder was made");
}
