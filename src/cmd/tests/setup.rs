// Intent (README, "Setting up", "Setup file" and "Macs"): init then machine add
// over SSH runs a job with no hand-written setup file, within each machine's
// cores and memory less its owner's room, on Linux machines and on Macs.
// machine add gives a machine as many places as fit its cores and memory less
// the room kept for its owner, and no more tart places than macOS runs VMs at
// once, refuses a machine that cannot run the places asked for, and writes a
// setup that loads as written. The setup checks themselves are owned by
// config.rs's tests.

use std::collections::BTreeMap;

use super::temp_dir;
use crate::agent::{MACOS_VM_LIMIT, Probe};
use crate::cmd::init::{PlaceOptions, add_machine};
use crate::cmd::machine_line;
use crate::config::{
    self, Config, Machine, PROVIDER_CONTAINER, PROVIDER_DOCKER, PROVIDER_HOST, PROVIDER_HYPERV, PROVIDER_KVM, PROVIDER_TART, Place, Pool,
};

/// What machine add read of the Linux test machine over SSH: a Lima VM with 4
/// cores and 5910 MB running Ubuntu 24.04 on arm64, with Docker and KVM ready.
fn lima_probe() -> Probe {
    Probe {
        version: "dev".into(),
        os: "linux".into(),
        arch: "arm64".into(),
        cores: 4,
        memory_mb: 5910,
        docker: "ok".into(),
        kvm: "ok".into(),
        ..Default::default()
    }
}

/// What runner-up agent probe read of a MacBook with Apple silicon, 10 cores
/// and 24 GB, before tart was installed; mac_tart_probe is the same Mac with
/// tart.
fn mac_probe() -> Probe {
    Probe {
        version: "dev".into(),
        os: "darwin".into(),
        arch: "arm64".into(),
        cores: 10,
        memory_mb: 24576,
        docker: r#"exec: "docker": executable file not found in $PATH"#.into(),
        kvm: "KVM places run only on Linux".into(),
        tart: "tart is not installed (brew install cirruslabs/cli/tart)".into(),
        host: "ok".into(),
        ..Default::default()
    }
}

pub fn mac_tart_probe() -> Probe {
    Probe { tart: "ok".into(), ..mac_probe() }
}

/// What runner-up init writes for one repository, and the folder that holds
/// its state.
pub fn initial_setup() -> (tempfile::TempDir, Config) {
    let dir = temp_dir();
    let mut cfg = Config {
        state_dir: dir.path().to_string_lossy().into_owned(),
        pools: vec![Pool { label: "ru-linux".into(), ..Default::default() }],
        ..Default::default()
    };
    cfg.github.repos = vec!["octo-org/app".into()];
    cfg.defaults();
    (dir, cfg)
}

fn options(f: impl FnOnce(&mut PlaceOptions)) -> PlaceOptions {
    let mut o = PlaceOptions::default();
    f(&mut o);
    o
}

fn saved_and_loaded(cfg: &Config, why: &str) -> Config {
    let dir = temp_dir();
    let path = dir.path().join("config.json").to_string_lossy().into_owned();
    config::save_config(&path, cfg).unwrap();
    config::load_config(&path).unwrap_or_else(|e| panic!("{why}: the written setup does not load: {e}"))
}

fn machine(name: &str, ssh: &str) -> Machine {
    Machine { name: name.into(), ssh: vec![ssh.into()], ..Default::default() }
}

#[test]
fn machine_add_fits_places_in_the_budget_less_the_owners_room() {
    let set = |cores, memory_mb, room_cores, room_memory_mb| {
        options(|o| {
            o.cores = cores;
            o.memory_mb = memory_mb;
            o.room_cores = room_cores;
            o.room_memory_mb = room_memory_mb;
        })
    };
    // 3 cores and 4886 MB are left for places.
    for (why, o, places) in [
        ("the defaults", set(2, 4096, 1, 1024), 1),
        ("cores run out first", set(1, 1024, 1, 1024), 3),
        ("memory runs out first", set(1, 2048, 1, 1024), 2),
        ("no room kept", set(1, 1024, 0, 0), 4),
    ] {
        let (_dir, mut cfg) = initial_setup();
        let (cores, memory_mb, room_cores, room_memory_mb) = (o.cores, o.memory_mb, o.room_cores, o.room_memory_mb);
        add_machine(&mut cfg, machine("lima", "ci@lima"), &lima_probe(), o).unwrap_or_else(|e| panic!("{why}: {e}"));
        let loaded = saved_and_loaded(&cfg, why);
        let mut ids = Vec::new();
        for p in loaded.places() {
            assert!(
                p.machine == "lima"
                    && p.provider == PROVIDER_DOCKER
                    && p.pools == ["ru-linux"]
                    && p.cores == cores
                    && p.memory_mb == memory_mb,
                "{why}: place {p:?}"
            );
            ids.push(p.id);
        }
        assert_eq!(ids.len(), places, "{why}: {} places {ids:?}, want {places}", ids.len());
        let b = loaded.machine("lima").budget();
        assert!(b.cores == 4 - room_cores && b.memory_mb == 5910 - room_memory_mb, "{why}: places may reserve {b:?}");
    }
}

#[test]
fn machine_add_refuses_machines_that_cannot_run_the_places() {
    let docker_mac = Probe { docker: "ok".into(), ..mac_tart_probe() };
    let no_docker = Probe {
        docker: "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock".into(),
        ..lima_probe()
    };
    // The readiness Apple container places gained with the provider's ready
    // method: a Mac without the container tool is refused up front.
    let no_container = Probe { container: "Apple's container tool is not installed (brew install container)".into(), ..mac_tart_probe() };
    let provider = |p: &str, image: &str| {
        options(|o| {
            o.provider = p.into();
            o.image = image.into();
        })
    };
    let sized = |p: &str, cores, memory_mb, room_cores, room_memory_mb| {
        options(|o| {
            o.provider = p.into();
            o.cores = cores;
            o.memory_mb = memory_mb;
            o.room_cores = room_cores;
            o.room_memory_mb = room_memory_mb;
        })
    };
    const SEQUOIA: &str = "ghcr.io/cirruslabs/macos-sequoia-base:latest";
    for (pr, o, want) in [
        (docker_mac, provider(PROVIDER_DOCKER, ""), "lima runs darwin; docker places run on Linux machines only"),
        (mac_probe(), provider("", SEQUOIA), "lima cannot run tart places: tart is not installed"),
        (mac_tart_probe(), provider("", ""), "a tart place needs --image"),
        (mac_tart_probe(), provider(PROVIDER_HYPERV, r"C:\vm\noble.vhdx"), "lima runs darwin; hyperv places run on Windows machines only"),
        (lima_probe(), provider(PROVIDER_TART, SEQUOIA), "lima cannot run tart places"),
        (no_docker, sized("", 2, 4096, 0, 0), "lima cannot run Docker places: permission denied"),
        (lima_probe(), sized(PROVIDER_KVM, 2, 2048, 0, 0), "a KVM place needs --image"),
        (
            no_container,
            provider(PROVIDER_CONTAINER, "runner-up-linux:2026-11-03"),
            "lima cannot run Apple container places: Apple's container tool is not installed",
        ),
        (lima_probe(), sized("", 4, 4096, 1, 1024), "lima has 3 cores and 4886 MB for places, less than one place of 4 cores and 4096 MB"),
    ] {
        let (_dir, mut cfg) = initial_setup();
        let err = add_machine(&mut cfg, machine("lima", "ci@lima"), &pr, o);
        assert!(matches!(&err, Err(e) if e.starts_with(want)), "machine add said {err:?}, want {want:?}");
        assert!(cfg.machines.is_empty(), "a refused machine joined the setup: {:?}", cfg.machines);
    }
}

#[test]
fn machine_add_gives_a_mac_no_more_tart_places_than_macos_runs_vms() {
    const IMAGE: &str = "ghcr.io/cirruslabs/macos-sequoia-base:latest";
    let set = |provider: &str, image: &str, places, cores, memory_mb| {
        options(|o| {
            o.provider = provider.into();
            o.image = image.into();
            o.places = places;
            o.cores = cores;
            o.memory_mb = memory_mb;
            o.room_cores = 1;
            o.room_memory_mb = 1024;
        })
    };
    // 9 cores and 23552 MB are left: room for four 2-core places or two
    // 4-core ones, but macOS runs two VMs at once.
    for (why, o, provider, places, cores, mem_mb) in [
        ("tart by default", set("", IMAGE, 0, 0, 0), PROVIDER_TART, 2, 4, 8192),
        ("small tart places", set("", IMAGE, 0, 2, 4096), PROVIDER_TART, 2, 2, 4096),
        ("asked for three", set("", IMAGE, 3, 0, 0), PROVIDER_TART, 3, 4, 8192),
        ("the Mac itself", set(PROVIDER_HOST, "", 0, 0, 0), PROVIDER_HOST, 1, 2, 4096),
    ] {
        let (_dir, mut cfg) = initial_setup();
        add_machine(&mut cfg, machine("mac", "you@mac"), &mac_tart_probe(), o).unwrap_or_else(|e| panic!("{why}: {e}"));
        let loaded = saved_and_loaded(&cfg, why);
        let found = loaded.places();
        assert_eq!(found.len(), places, "{why}: {} places, want {places}", found.len());
        for p in &found {
            assert!(p.provider == provider && p.cores == cores && p.memory_mb == mem_mb, "{why}: place {p:?}");
            let want_image = if provider == PROVIDER_TART { IMAGE } else { "" };
            assert_eq!(p.image, want_image, "{why}: place {} clones {:?}", p.id, p.image);
        }
        let line = machine_line(&loaded.machine("mac"));
        let turns = line.contains("tart places take turns");
        assert_eq!(turns, places > MACOS_VM_LIMIT && provider == PROVIDER_TART, "{why}: check-config says {line:?}");
    }
}

// Intent (README, "Setup file", poolImages): a PC's five Hyper-V places serve
// the 24.04 and 26.04 pools alike, each pool on its own base disk: poolImages
// loads as written and leaves the places, their shares and the machine's budget
// as they were.
#[test]
fn pool_images_keep_the_places_and_their_shares() {
    let pc = |pool_images: BTreeMap<String, String>| {
        let (dir, mut cfg) = initial_setup();
        cfg.pools = ["ru-linux", "ru-linux-gate", "ru-linux-2604"].map(|l| Pool { label: l.into(), ..Default::default() }).to_vec();
        let mut m =
            Machine { os: "windows".into(), cores: 24, memory_mb: 65536, room_cores: 4, room_memory_mb: 16384, ..machine("pc", "ci@pc") };
        for i in 1..=5 {
            m.places.push(Place {
                id: format!("pc-{i}"),
                provider: PROVIDER_HYPERV.into(),
                pools: vec!["ru-linux".into(), "ru-linux-gate".into(), "ru-linux-2604".into()],
                cores: 4,
                memory_mb: 8192,
                image: r"C:\runner-up\job.vhdx".into(),
                pool_images: pool_images.clone(),
                ..Default::default()
            });
        }
        cfg.machines = vec![m];
        (dir, cfg)
    };
    let (_dir, cfg) = pc(BTreeMap::from([("ru-linux-2604".into(), r"C:\runner-up\job-2604.vhdx".into())]));
    let loaded = saved_and_loaded(&cfg, "the setup");
    let (_one, one_image) = pc(BTreeMap::new());
    let (got, want) = (machine_line(&loaded.machine("pc")), machine_line(&one_image.machines[0]));
    assert_eq!(got, want, "check-config says\n{got}\nwant, as with one image\n{want}");
    assert_eq!(loaded.places().len(), 5);
    for p in loaded.places() {
        for (pool, want) in [
            ("ru-linux", r"C:\runner-up\job.vhdx"),
            ("ru-linux-gate", r"C:\runner-up\job.vhdx"),
            ("ru-linux-2604", r"C:\runner-up\job-2604.vhdx"),
        ] {
            assert_eq!(p.image_for(pool), want, "place {} runs a {pool} job on {:?}, want {want:?}", p.id, p.image_for(pool));
        }
    }
}
