// Intent (README, "Windows machines"): the coordinator reaches a Windows
// machine over SSH, whose sign-in runs cmd or PowerShell, with a command line
// both read the same way; machine add sets a Windows machine up with account
// places, or Hyper-V places on a base disk. A setup a Windows machine's shell
// could not pass unchanged is refused by the setup checks (config.rs's tests).

use std::collections::BTreeMap;

use super::setup::initial_setup;
use crate::agent::Probe;
use crate::cmd::init::{PlaceOptions, add_machine};
use crate::config::{Machine, PROVIDER_ACCOUNT, PROVIDER_DOCKER, PROVIDER_HYPERV, Place};
use crate::coordinator::{agent_command, run_args, ssh_args};

/// What `runner-up agent probe` reports on a Windows machine set up with
/// runner-up setup --hyperv.
fn windows_probe() -> Probe {
    Probe {
        version: "dev".into(),
        os: "windows".into(),
        arch: "amd64".into(),
        cores: 4,
        memory_mb: 16383,
        docker: "Windows machines run account and hyperv places".into(),
        kvm: "Windows machines run account and hyperv places".into(),
        account: "ok".into(),
        hyperv: "ok".into(),
        ..Default::default()
    }
}

/// The Windows test machine as machine add reads it over SSH.
fn win_pc() -> Machine {
    Machine { name: "win-pc".into(), ssh: vec!["runner@win-pc".into()], os: "windows".into(), ..Default::default() }
}

#[test]
fn windows_command_line_reads_the_same_in_cmd_and_powershell() {
    let (_dir, cfg) = initial_setup();
    let p = Place {
        id: "win-pc-1".into(),
        provider: PROVIDER_HYPERV.into(),
        cores: 4,
        memory_mb: 8192,
        image: r"C:\runner-up\noble.vhdx".into(),
        switch: "Default Switch".into(),
        disk_gb: 64,
        ..Default::default()
    };
    let argv = agent_command(&win_pc(), &run_args(&cfg, &p, "ru-linux"));
    let (line, ssh) = argv.split_last().unwrap();
    // Plain words pass as they are; the one with a space is in double
    // quotes, which cmd and PowerShell both take as one argument.
    let want = format!(
        r#"runner-up agent run --provider hyperv --place win-pc-1 --cores 4 --memory 8192 --boot-timeout {} --image C:\runner-up\noble.vhdx --disk-gb 64 --switch "Default Switch""#,
        cfg.boot_timeout_seconds
    );
    assert_eq!(*line, want, "the command line for win-pc is\n{line}\nwant\n{want}");
    assert_eq!(ssh, ssh_args(&win_pc()), "ssh runs as {ssh:?}");
}

// Intent (README, "Setup file", poolImages): the same Hyper-V places run Ubuntu
// 24.04 and 26.04 jobs, each on the base disk of the pool its job asked for,
// with the same share of the PC whichever disk boots.
#[test]
fn hyperv_place_boots_the_pools_base_disk() {
    let (_dir, cfg) = initial_setup();
    let p = Place {
        id: "win-pc-1".into(),
        provider: PROVIDER_HYPERV.into(),
        pools: vec!["ru-linux".into(), "ru-linux-gate".into(), "ru-linux-2604".into()],
        cores: 4,
        memory_mb: 8192,
        image: r"C:\runner-up\job.vhdx".into(),
        pool_images: BTreeMap::from([("ru-linux-2604".into(), r"C:\runner-up\job-2604.vhdx".into())]),
        ..Default::default()
    };
    let head = format!(
        "runner-up agent run --provider hyperv --place win-pc-1 --cores 4 --memory 8192 --boot-timeout {} --image ",
        cfg.boot_timeout_seconds
    );
    for (pool, image) in [
        ("ru-linux", r"C:\runner-up\job.vhdx"),
        ("ru-linux-gate", r"C:\runner-up\job.vhdx"),
        ("ru-linux-2604", r"C:\runner-up\job-2604.vhdx"),
    ] {
        let argv = agent_command(&win_pc(), &run_args(&cfg, &p, pool));
        let want = format!("{head}{image}");
        assert_eq!(argv.last().unwrap(), &want, "a {pool} job runs");
    }
}

#[test]
fn machine_add_sets_up_a_windows_machine() {
    let pr = windows_probe();
    let machine = || Machine { name: "win-pc".into(), ssh: vec!["runner@win-pc".into()], ..Default::default() };
    let sized = |provider: &str, image: &str, room_cores, room_memory_mb| PlaceOptions {
        provider: provider.into(),
        image: image.into(),
        cores: 2,
        memory_mb: 4096,
        room_cores,
        room_memory_mb,
        ..Default::default()
    };
    let (_dir, mut cfg) = initial_setup();
    add_machine(&mut cfg, machine(), &pr, sized("", "", 1, 1024)).unwrap();
    let m = cfg.machine("win-pc");
    assert!(m.windows() && !m.places.is_empty(), "machine add made {m:?}");
    for p in &m.places {
        assert_eq!(p.provider, PROVIDER_ACCOUNT, "place {} runs {}, want account places by default on Windows", p.id, p.provider);
    }

    let no_service = Probe { account: "runner-up's service is not running here; run runner-up setup".into(), ..pr.clone() };
    let no_hyperv = Probe { hyperv: "no Hyper-V for this account (run runner-up setup --hyperv): Access denied".into(), ..pr.clone() };
    for (pr, o, want) in [
        (no_service, sized("", "", 0, 0), "win-pc cannot run account places: runner-up's service is not running here"),
        (no_hyperv, sized(PROVIDER_HYPERV, r"C:\vm\noble.vhdx", 0, 0), "win-pc cannot run Hyper-V places: no Hyper-V"),
        (pr.clone(), sized(PROVIDER_HYPERV, "", 0, 0), "a Hyper-V place needs --image"),
        (pr.clone(), sized(PROVIDER_DOCKER, "", 0, 0), "win-pc cannot run Docker places: Windows machines run account and hyperv places"),
    ] {
        let (_dir, mut cfg) = initial_setup();
        let err = add_machine(&mut cfg, machine(), &pr, o);
        assert!(matches!(&err, Err(e) if e.starts_with(want)), "machine add said {err:?}, want {want:?}");
    }
}
