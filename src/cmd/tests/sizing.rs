use super::setup::{initial_setup, mac_tart_probe};
use super::temp_dir;
use crate::cmd::check_config;
use crate::config::{self, Machine, PROVIDER_HOST, Place};

fn host_place(id: &str) -> Place {
    Place { id: id.into(), provider: PROVIDER_HOST.into(), pools: vec!["ru-linux".into()], cores: 2, memory_mb: 4096, ..Default::default() }
}

// Intent (README, "Setup file"): a person can configure one pool size that
// replaces all its place sizes: it survives the written setup, and
// check-config shows it. Which sizes are refused is owned by config.rs's
// pool_sizes_that_could_never_run_are_refused.
#[test]
fn configured_pool_sizes() {
    let (_dir, mut cfg) = initial_setup();
    let mac = mac_tart_probe();
    cfg.machines = vec![Machine {
        name: "mac".into(),
        cores: mac.cores,
        memory_mb: mac.memory_mb,
        room_cores: 1,
        room_memory_mb: 1024,
        places: vec![host_place("mac-1")],
        ..Default::default()
    }];
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (4, 8192);
    let dir = temp_dir();
    let path = dir.path().join("config.json").to_string_lossy().into_owned();
    config::save_config(&path, &cfg).unwrap();
    let loaded = config::load_config(&path).unwrap();
    let p = loaded.size_place(&loaded.places()[0], "ru-linux");
    assert!(p.cores == cfg.pools[0].cores && p.memory_mb == cfg.pools[0].memory_mb, "pool size lost: {p:?}");
    let mut out = Vec::new();
    let code = check_config(&loaded, &mut out);
    let out = String::from_utf8(out).unwrap();
    let want = format!("pool ru-linux: each job reserves {} cores, {} MB", cfg.pools[0].cores, cfg.pools[0].memory_mb);
    assert!(code == 0 && out.contains(&want), "check-config: {out}");
}

// Intent (README, "Setting up" and "Job sizes and readings"): status must
// describe the pool's actual resource fit and a running job's frozen
// reservation, not its place's fallback size.
#[cfg(not(windows))]
#[test]
fn status_uses_pool_sizes_and_running_reservations() {
    use crate::config::Share;
    use crate::coordinator::{BusyReason, PlaceRun, status};

    let (_dir, mut cfg) = initial_setup();
    (cfg.pools[0].cores, cfg.pools[0].memory_mb) = (4, 8192);
    let mac = mac_tart_probe();
    cfg.machines = vec![Machine {
        name: "mac".into(),
        cores: mac.cores,
        room_cores: 1,
        memory_mb: mac.memory_mb,
        places: vec![host_place("busy"), host_place("idle")],
        ..Default::default()
    }];
    cfg.defaults();
    std::fs::create_dir_all(cfg.place_dir("busy")).unwrap();
    let _held = crate::lock::busy(&cfg.place_dir("busy").join("busy.lock")).unwrap().expect("lock: held");
    let reservation = Share { cores: 6, memory_mb: 8192 };
    config::write_json(&cfg.place_dir("busy").join("place.json"), &PlaceRun { started: true, reservation, ..Default::default() }).unwrap();
    let said = |cfg: &config::Config| {
        let mut out = Vec::new();
        status(cfg.clone(), &mut out);
        String::from_utf8(out).unwrap()
    };
    let busy = format!(
        "place busy on mac (host, {} cores, {} MB) busy ({})",
        reservation.cores,
        reservation.memory_mb,
        BusyReason::JOB
    );
    let out = said(&cfg);
    let idle = format!("place idle on mac (host, 2 cores, 4096 MB) busy (cores {} of {} in use", reservation.cores, cfg.machines[0].budget().cores);
    assert!(out.contains(&busy) && out.contains(&idle), "status used fallback sizes:\n{out}");
    cfg.pools[0].cores = 2;
    let out = said(&cfg);
    assert!(
        out.contains("place idle on mac (host, 2 cores, 4096 MB) free") && out.contains(&busy),
        "status lost a running reservation or hid a fitting pool:\n{out}"
    );
}
