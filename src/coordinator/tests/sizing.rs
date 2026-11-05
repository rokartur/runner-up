// Intent (README, "Job sizes and readings"): a pool has one size whichever
// place takes it; a waiting large job keeps its room instead of being starved
// by later small jobs, without stopping compatible work on another machine.
// The budgets are the README's illustrations, not proposed production
// measurements. Job messages are the project's renamed GitHub recording.

use std::collections::HashMap;
use std::time::Duration;

use super::rules::one_place;
use super::service::{PUBLISH, VERIFY};
use super::*;
use crate::coordinator::place::{reserved_place, run_args};

fn host(id: &str, pools: &[&str], cores: i64, memory_mb: i64) -> Place {
    Place { id: id.into(), provider: config::PROVIDER_HOST.into(), pools: strings(pools), cores, memory_mb, ..Default::default() }
}

fn write_run(cfg: &Config, id: &str, run: &PlaceRun) {
    config::write_json(&cfg.place_dir(id).join("place.json"), run).unwrap();
}

#[test]
fn pool_sizes_keep_room_for_the_first_job() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_place(dir.path(), &["pool:ru-large"]);
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 4096);
    (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (12, 32768);
    let m = &mut cfg.machines[0];
    (m.cores, m.room_cores) = (22, 8);
    (m.memory_mb, m.room_memory_mb) = (65536, 16384);
    let mut phone = host("phone", &["ru-linux"], 4, 16384);
    phone.provider = config::PROVIDER_TART.into();
    phone.image = "ci-base".into();
    m.places = vec![host("host-1", &["ru-large", "ru-linux"], 6, 16384), host("host-2", &["ru-large", "ru-linux"], 6, 16384), phone];
    let phone = occupy(&cfg, "phone");
    write_run(&cfg, "phone", &PlaceRun { started: true, pool: "ru-linux".into(), ..Default::default() });
    let h = new_harness(cfg.clone(), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    // An already-running legacy phone job reserves four cores from its place.
    h.feed_range(&f, 1, 2);
    h.feed_range(&f, 0, 1);
    assert!(h.launcher.st().held.is_empty(), "later work took the large job's room: {:?}", h.placed());
    // Phone holds 4 of 14 cores; the first-ranked 12-core job and later
    // 2-core jobs wait.
    drop(phone);
    h.place();
    h.want(&[("host-1", PUBLISH), ("host-2", VERIFY)]);
    for (id, want) in [("host-1", cfg.pools[1].size()), ("host-2", cfg.pools[0].size())] {
        assert_eq!(h.run(id).reservation, want, "{id} holds the wrong size");
    }
    // Phone ended; the 12-core job starts beside one 2-core job, with the
    // same pool sizes on either host place.

    // A coordinator restart and a configuration edit cannot shrink live use.
    (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (4, 8192);
    let restarted = new_harness(cfg.clone(), h.launcher.clone(), h.sets.clone());
    restarted.set_now(f.now.add(Duration::from_secs(60)));
    let got = restarted.svc.in_use("box", "");
    assert!(
        got.cores == cfg.machines[0].budget().cores && got.memory_mb == 32768 + cfg.pools[0].memory_mb,
        "restart shrank live reservations: {got:?}"
    );
    restarted.place();
    restarted.want(&[("host-1", PUBLISH), ("host-2", VERIFY)]);
    h.launcher.finish("host-1");
    h.launcher.finish("host-2");
}

// Intent (README, "Setup file"): a job waits only when its cores OR memory do
// not fit. Pool sizes reach VM launch arguments, not just planner accounting:
// the size the coordinator wrote at launch is the size the agent is given.
#[test]
fn pool_size_fits_memory_and_sizes_the_agent() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_place(dir.path(), &["pool:ru-large"]);
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 4096);
    (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (4, 16384);
    let m = &mut cfg.machines[0];
    (m.cores, m.memory_mb, m.room_memory_mb) = (14, 24576, 8192);
    let mut first = m.places[0].clone();
    first.provider = config::PROVIDER_TART.into();
    first.image = "ci-base".into();
    (first.cores, first.memory_mb) = (2, 8192);
    let mut second = first.clone();
    second.id = "box-2".into();
    m.places = vec![first.clone(), second];
    let busy = occupy(&cfg, "box-2");
    write_run(&cfg, "box-2", &PlaceRun { started: true, pool: "ru-linux".into(), ..Default::default() });
    let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 1, 2);
    h.feed_range(&f, 0, 1);
    assert!(h.launcher.st().held.is_empty(), "later job used reserved memory: {:?}", h.placed());
    drop(busy);
    h.place();
    h.want(&[("box-1", PUBLISH)]);
    // The coordinator reserved the pool's size, not the place's own, and the
    // place runner gives the agent what the coordinator reserved.
    let cfg = &h.cfg;
    assert_eq!(h.run("box-1").reservation, cfg.pools[1].size(), "box-1 holds the wrong size, want the ru-large pool's");
    let args = run_args(cfg, &reserved_place(cfg, &first, "ru-large"), "ru-large");
    let values: HashMap<&str, &str> = args.windows(2).map(|w| (w[0].as_str(), w[1].as_str())).collect();
    assert!(
        values.get("--cores") == Some(&cfg.pools[1].cores.to_string().as_str())
            && values.get("--memory") == Some(&cfg.pools[1].memory_mb.to_string().as_str()),
        "agent arguments: {args:?}"
    );
    h.launcher.finish("box-1");
}

// Intent (README, "Job sizes and readings"): keeping room for a waiting job on
// one machine must not stop compatible work on another machine. The messages
// are the same recorded release and check offers used by the placement tests.
#[test]
fn pool_reservation_leaves_another_machine_available() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_place(dir.path(), &["pool:ru-large"]);
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 4096);
    (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (4, 8192);
    let m = &mut cfg.machines[0];
    (m.cores, m.memory_mb) = (4, 8192);
    let mut other = m.places[0].clone();
    other.id = "box-busy".into();
    m.places.push(other);
    cfg.machines.push(config::Machine {
        name: "other".into(),
        cores: 2,
        memory_mb: 4096,
        places: vec![Place {
            id: "other-1".into(),
            provider: config::PROVIDER_DOCKER.into(),
            pools: strings(&["ru-linux"]),
            cores: 2,
            memory_mb: 4096,
            ..Default::default()
        }],
        ..Default::default()
    });
    let _busy = occupy(&cfg, "box-busy");
    write_run(&cfg, "box-busy", &PlaceRun { started: true, reservation: cfg.pools[0].size(), ..Default::default() });
    let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 1, 2);
    h.feed_range(&f, 0, 1);
    {
        let held = &h.launcher.st().held;
        assert!(held.len() == 1 && held.contains_key("other-1"), "the unrelated machine was held: {:?}", h.placed());
    }
    h.launcher.finish("other-1");
}

// Intent (README, "Setup file"): a running job keeps its reservation. A
// missing record must not make a large running job look like a smaller place
// default, even after the configured kind size changes.
#[test]
fn unknown_running_reservation_keeps_the_counted_budget() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = one_place(dir.path(), &[]);
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (2, 4096);
    (cfg.pools[1].cores, cfg.pools[1].memory_mb) = (4, 8192);
    let m = &mut cfg.machines[0];
    (m.cores, m.memory_mb) = (14, 49152);
    let mut other = m.places[0].clone();
    other.id = "box-2".into();
    m.places.push(other);
    let _busy = occupy(&cfg, "box-1");
    // occupy writes a readable record; remove it to model a live runner
    // whose reservation record is unavailable.
    std::fs::remove_file(cfg.place_dir("box-1").join("place.json")).unwrap();
    let h = new_harness(cfg, FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_range(&f, 0, 1);
    assert!(h.launcher.st().held.is_empty(), "unknown live reservation allowed new work: {:?}", h.placed());
}
