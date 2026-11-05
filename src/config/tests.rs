// Intent (README, "Setup file"): the setup checks refuse, when the setup
// loads, a setup no job could run under, and say what to change. They are
// plain functions of the setup, so this module owns them; the command
// module owns only what init, machine add and check-config do with them.

use super::*;

fn initial_setup(dir: &tempfile::TempDir) -> Config {
    let mut cfg = Config {
        state_dir: dir.path().to_string_lossy().into(),
        github: GitHub { repos: vec!["octo-org/app".into()], ..Default::default() },
        pools: vec![Pool { label: "ru-linux".into(), ..Default::default() }],
        ..Default::default()
    };
    cfg.defaults();
    cfg
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn map(k: &str, v: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(k.to_string(), v.to_string())])
}

// Intent (README, "Setup file"): a pool label GitHub's own runners, or every
// self-hosted runner, answer to is refused, so a pool's jobs never go to
// another runner.
#[test]
fn pool_labels_other_runners_answer_to_are_refused() {
    let d = tmp();
    for label in ["ubuntu-latest", "macos-14", "windows-2025", "self-hosted", "Linux", "x64", "two words", ""] {
        let mut cfg = initial_setup(&d);
        cfg.pools = vec![Pool { label: label.into(), ..Default::default() }];
        assert!(cfg.validate().is_err(), "pool label {label:?} was accepted");
    }
    initial_setup(&d).validate().unwrap();
}

// Intent (README, "Setup file": each machine's roomCores and roomMemoryMB
// keep room for its owner): a machine whose owner keeps every core leaves
// its places nothing, so it is refused.
#[test]
fn owners_room_must_leave_the_places_something() {
    let d = tmp();
    let mut cfg = initial_setup(&d);
    cfg.machines = vec![Machine {
        name: "lima".into(),
        ssh: s(&["ci@lima"]),
        cores: 4,
        memory_mb: 5910,
        room_cores: 4,
        places: vec![Place {
            id: "lima-1".into(),
            provider: PROVIDER_DOCKER.into(),
            pools: s(&["ru-linux"]),
            cores: 2,
            memory_mb: 2048,
            ..Default::default()
        }],
        ..Default::default()
    }];
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("the room kept for its owner"), "{err}");
}

// Intent (README, "Setup file": "check-config rejects partial sizes and
// sizes that never fit a machine serving the pool, after its owner's room"):
// a pool's size replaces its places' sizes, a pool without one keeps them,
// and a partial, negative or impossible size is refused before a runner
// starts.
#[test]
fn pool_sizes_that_could_never_run_are_refused() {
    // A MacBook with 10 cores and 24576 MB, as runner-up agent probe read it,
    // keeping 1 core and 1024 MB for its owner.
    let mac = Machine {
        name: "mac".into(),
        cores: 10,
        memory_mb: 24576,
        room_cores: 1,
        room_memory_mb: 1024,
        places: vec![Place {
            id: "mac-1".into(),
            provider: PROVIDER_HOST.into(),
            pools: s(&["ru-linux"]),
            cores: 2,
            memory_mb: 4096,
            ..Default::default()
        }],
        ..Default::default()
    };
    let d = tmp();
    let mut cfg = initial_setup(&d);
    cfg.machines = vec![mac.clone()];
    cfg.defaults();
    let b = mac.budget();
    let sizes = [(4, 0), (0, 8192), (-1, 8192), (b.cores + 1, 8192), (4, b.memory_mb + 1)];
    for (cores, mem) in sizes {
        cfg.pools[0].cores = cores;
        cfg.pools[0].memory_mb = mem;
        assert!(cfg.validate().is_err(), "pool size {cores}/{mem} was accepted");
    }
    cfg.pools[0].cores = b.cores;
    cfg.pools[0].memory_mb = b.memory_mb;
    cfg.validate().expect("a pool the size of the machine's budget was refused");
    assert_eq!(cfg.size_place(&cfg.places()[0], "ru-linux").share(), b);
    cfg.pools[0].cores = 0;
    cfg.pools[0].memory_mb = 0;
    cfg.validate().unwrap();
    assert_eq!(cfg.size_place(&cfg.places()[0], "ru-linux").share(), mac.places[0].share());
}

// Intent (README, "Setup file", poolImages): a pool image no job could run on
// is refused when the setup loads, not when a job reaches it.
#[test]
fn pool_images_no_job_could_run_on_are_refused() {
    let d = tmp();
    // An absolute path on the system the test runs on, as the coordinator's
    // own system reads the KVM rule.
    let noble = d.path().join("noble.img").to_string_lossy().into_owned();
    let place = |provider: &str, image: &str, pi: BTreeMap<String, String>| Place {
        provider: provider.into(),
        image: image.into(),
        pool_images: pi,
        ..Default::default()
    };
    let cases = [
        (
            "a pool the place does not serve",
            place(PROVIDER_DOCKER, "", map("ru-large", "runner:large")),
            r#"place "lima-1": poolImages names pool "ru-large", which the place does not serve"#,
        ),
        ("no image", place(PROVIDER_DOCKER, "", map("ru-linux", "")), r#"place "lima-1": poolImages gives pool "ru-linux" no image"#),
        (
            "a host place",
            place(PROVIDER_HOST, "", map("ru-linux", "runner:2604")),
            r#"place "lima-1": pool "ru-linux"'s image: a host place runs on the machine itself and takes no image"#,
        ),
        (
            "a KVM disk that is not an absolute path",
            place(PROVIDER_KVM, &noble, map("ru-linux", "resolute.img")),
            r#"place "lima-1": pool "ru-linux"'s image: a KVM place needs the absolute path of its base disk image on the machine"#,
        ),
        (
            "a tart image with a space",
            place(PROVIDER_TART, "ci-base", map("ru-linux", "ci base")),
            r#"place "lima-1": pool "ru-linux"'s image: a tart place needs the tart image it clones"#,
        ),
    ];
    for (why, mut p, want) in cases {
        let mut cfg = initial_setup(&d);
        cfg.pools.push(Pool { label: "ru-large".into(), ..Default::default() });
        p.id = "lima-1".into();
        p.pools = s(&["ru-linux"]);
        p.cores = 2;
        p.memory_mb = 2048;
        let big = Place {
            id: "lima-2".into(),
            provider: PROVIDER_DOCKER.into(),
            pools: s(&["ru-large"]),
            cores: 2,
            memory_mb: 2048,
            ..Default::default()
        };
        cfg.machines = vec![Machine { name: "lima".into(), ssh: s(&["ci@lima"]), places: vec![p, big], ..Default::default() }];
        match cfg.validate() {
            Err(e) if e.starts_with(want) => {}
            other => panic!("{why}: validate said {other:?}, want {want:?}"),
        }
    }
}

// Intent (README, "Updating runner-up": the setup names a runner-up version
// and each platform's SHA-256): a release named as the release workflow tags
// it, with each platform's archive SHA-256 as its checksums.txt lists it, is
// accepted; a release the coordinator could not fetch or check is refused
// when the setup loads.
#[test]
fn the_setups_release_names_a_version_and_each_archives_sha256() {
    // runner-up-v9.0.2-windows-amd64.zip's line in the release fixture's
    // checksums.txt (testdata/release).
    const SUM: &str = "1a2dc4c30dbf507b3762d4a738564654c94213cb4879b801c2d994c62099f154";
    let d = tmp();
    let mut cfg = initial_setup(&d);
    let mut sums = map("darwin-arm64", SUM);
    sums.insert("windows-amd64".into(), SUM.into());
    cfg.release = Some(Release { version: "v0.1.11".into(), sha256: sums, url: String::new() });
    cfg.validate().expect("a release as the workflow publishes it was refused");
    let rel = |v: &str, sums: BTreeMap<String, String>, url: &str| Release { version: v.into(), sha256: sums, url: url.into() };
    let cases = [
        (
            "a version without its v",
            rel("0.1.11", map("darwin-arm64", SUM), ""),
            r#"release: version "0.1.11" is not a release version such as v0.1.11"#,
        ),
        ("no archive", rel("v0.1.11", BTreeMap::new(), ""), "release: sha256 names no platform's archive"),
        ("a platform no release has", rel("v0.1.11", map("macos-arm64", SUM), ""), r#"release: sha256 names platform "macos-arm64""#),
        (
            "a SHA-256 in capitals",
            rel("v0.1.11", map("linux-amd64", &SUM.to_uppercase()), ""),
            "release: linux-amd64's sha256 is not 64 lowercase hex digits",
        ),
        (
            "a URL that is not a web address",
            rel("v0.1.11", map("linux-amd64", SUM), "file:///srv/releases"),
            "release: url must be an http or https address",
        ),
    ];
    for (why, r, want) in cases {
        let mut cfg = initial_setup(&d);
        cfg.release = Some(r);
        match cfg.validate() {
            Err(e) if e.starts_with(want) => {}
            other => panic!("{why}: validate said {other:?}, want {want:?}"),
        }
    }
}

// Intent (README, "Windows machines"): the coordinator reaches a Windows
// machine over SSH, whose sign-in runs cmd or PowerShell, so a setup that a
// Windows machine's shell could not pass unchanged, or a provider the
// machine's system cannot run, is refused before any job.
#[test]
fn windows_setup_that_the_shell_would_change_is_refused() {
    let place = |id: &str, provider: &str, image: &str, switch: &str, pi: BTreeMap<String, String>| Place {
        id: if id.is_empty() { "win-pc-1".into() } else { id.into() },
        provider: if provider.is_empty() { PROVIDER_ACCOUNT.into() } else { provider.into() },
        image: image.into(),
        switch: switch.into(),
        pool_images: pi,
        cores: 2,
        memory_mb: 4096,
        pools: s(&["ru-linux"]),
        ..Default::default()
    };
    let none = BTreeMap::new;
    let acct = || place("", "", "", "", none());
    let win = |ssh: bool, agent: &str, p: Place| Machine {
        name: "pc".into(),
        os: "windows".into(),
        ssh: if ssh { s(&["ci@pc"]) } else { vec![] },
        agent: agent.into(),
        places: vec![p],
        ..Default::default()
    };
    let cases = [
        ("a Windows machine without ssh", win(false, "", acct()), "a Windows machine needs ssh"),
        (
            "an agent path with a space",
            win(true, r"C:\Program Files\runner-up\runner-up.exe", acct()),
            "must be a plain program name or path",
        ),
        (
            "a Docker place on Windows",
            win(true, "", place("", PROVIDER_DOCKER, "", "", none())),
            "docker places do not run on Windows machines",
        ),
        (
            "an account place on Linux",
            Machine { name: "lima".into(), ssh: s(&["ci@lima"]), places: vec![acct()], ..Default::default() },
            "account places run only on Windows machines",
        ),
        (
            "a Hyper-V image that is not a full path",
            win(true, "", place("", PROVIDER_HYPERV, "noble.vhdx", "", none())),
            "a Hyper-V place needs the full path",
        ),
        (
            "an image with a variable cmd would expand",
            win(true, "", place("", PROVIDER_HYPERV, r"C:\Users\%USERNAME%\noble.vhdx", "", none())),
            "cannot pass to a Windows machine's shell unchanged",
        ),
        (
            "a pool's image with a variable cmd would expand",
            win(true, "", place("", PROVIDER_HYPERV, r"C:\vm\noble.vhdx", "", map("ru-linux", r"C:\Users\%USERNAME%\resolute.vhdx"))),
            r#"pool "ru-linux"'s image: "C:\\Users\\%USERNAME%\\resolute.vhdx" cannot pass to a Windows machine's shell unchanged"#,
        ),
        (
            "a switch name PowerShell would expand",
            win(true, "", place("", PROVIDER_HYPERV, r"C:\vm\noble.vhdx", "$env:x", none())),
            "cannot pass to a Windows machine's shell unchanged",
        ),
        ("a place id that is a path", win(true, "", place(r"pc\1", "", "", "", none())), "has only letters, digits"),
        ("a switch on an account place", win(true, "", place("", "", "", "Default Switch", none())), "switch"),
        ("an image on an account place", win(true, "", place("", "", r"C:\vm\noble.vhdx", "", none())), "image"),
    ];
    let d = tmp();
    for (why, m, want) in cases {
        let mut cfg = initial_setup(&d);
        cfg.machines = vec![m];
        match cfg.validate() {
            Err(e) if e.contains(want) => {}
            other => panic!("{why}: validate said {other:?}, want {want:?}"),
        }
    }
}
