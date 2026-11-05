//! `runner-up init` and `runner-up machine add`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::{self, Probe};
use crate::cli;
use crate::config::{self, Config, Machine, Place};
use crate::coordinator::{self, GithubApi, agent_command, ssh_args};
use crate::ctx::Ctx;
use crate::go::flag::FlagSet;

/// `runner-up init`: checks the GitHub command line's sign-in can serve what
/// is asked and writes a setup with no machines yet.
pub fn init_main(args: &[String]) -> i32 {
    let mut fs = FlagSet::new("init");
    fs.string("config", &config::default_config_path(), "setup file to write");
    fs.list("repo", "a repository to serve, owner/name; repeat for more");
    fs.list("org", "an organization to serve; repeat for more");
    fs.string("pool", "runner-up", "the label jobs ask for in runs-on");
    fs.string("state-dir", &config::default_state_dir(), "where the coordinator keeps its state");
    fs.string("runner-group", "", "the organization runner group; Default when empty");
    fs.string("gh", "gh", "the GitHub command line program");
    fs.bool("force", false, "replace an existing setup");
    if let Some(code) = cli::parse(&mut fs, args, false) {
        return code;
    }
    let config_path = fs.get_str("config");
    if std::path::Path::new(&config_path).exists() && !fs.get_bool("force") {
        eprintln!("{config_path} already exists; add machines with runner-up machine add, or pass --force to start over");
        return 2;
    }
    let mut cfg = Config {
        state_dir: fs.get_str("state-dir"),
        pools: vec![config::Pool { label: fs.get_str("pool"), ..Default::default() }],
        ..Default::default()
    };
    cfg.github.repos = fs.get_list("repo");
    cfg.github.orgs = fs.get_list("org");
    cfg.github.runner_group = fs.get_str("runner-group");
    let gh = fs.get_str("gh");
    if gh != "gh" {
        cfg.github.gh = gh;
    }
    let api = GithubApi::new(coordinator::gh_token(cfg.gh()));
    if let Err(e) = init_setup(&Ctx::detached_timeout(Duration::from_secs(60)), &mut cfg, &api, &mut std::io::stdout()) {
        eprintln!("{e}");
        return 1;
    }
    if let Err(e) = config::save_config(&config_path, &cfg) {
        eprintln!("{e}");
        return 1;
    }
    println!("wrote {config_path}\nnext: runner-up machine add NAME --ssh user@host");
    0
}

/// Checks the setup and that the sign-in may create runners for every
/// repository and organization in it.
pub(super) fn init_setup(ctx: &Ctx, cfg: &mut Config, api: &GithubApi, out: &mut dyn Write) -> Result<(), String> {
    cfg.defaults();
    cfg.validate()?;
    let (login, scopes) = api.sign_in(ctx)?;
    let _ = writeln!(out, "signed in to GitHub as {login} through the GitHub command line");
    for r in &cfg.github.repos {
        let admin = api.repo_admin(ctx, r).map_err(|e| format!("{r}: {e}"))?;
        if !admin {
            return Err(format!("{r}: {login} is not an admin of it, which creating its runners needs"));
        }
        let _ = writeln!(out, "{r}: ok");
    }
    // A classic token lists its scopes; a token with none listed is a
    // fine-grained one, whose permissions show only when used.
    if !cfg.github.orgs.is_empty() && !scopes.is_empty() && !scopes.iter().any(|s| s == "admin:org") {
        return Err("organization runners need the admin:org scope: run gh auth refresh -h github.com -s admin:org".into());
    }
    for o in &cfg.github.orgs {
        let _ = writeln!(out, "{o}: ok");
    }
    Ok(())
}

pub const MACHINE_ADD_USAGE: &str = "usage: runner-up machine add NAME --ssh DESTINATION [flags]";

/// `runner-up machine add`: reaches the machine over SSH, installs the agent
/// there when it is missing, reads the machine's cores, memory and
/// providers, and adds it with as many places as its budget fits.
pub fn machine_add_main(args: &[String]) -> i32 {
    let mut fs = FlagSet::new("machine add");
    fs.usage = Some(Box::new(|fs, w| {
        let _ = writeln!(w, "{MACHINE_ADD_USAGE}");
        fs.print_defaults(w);
    }));
    fs.string("config", &config::default_config_path(), "setup file");
    fs.string("ssh", "", "the machine for ssh, with any options, such as \"ci@box\" or \"-p 2222 ci@10.0.0.5\"; empty is this machine");
    fs.string("agent", "", "runner-up's path on the machine; runner-up on its PATH by default");
    fs.string("install", "", "a runner-up build for the machine to copy there when it has none; this program by default when the machine matches it");
    fs.string("provider", "", "docker or kvm on Linux, tart, host or container on a Mac, account or hyperv on Windows; docker, tart or account by default");
    fs.string("image", "", "the runner image (docker), the base cloud image's path on the machine (kvm, hyperv), the tart image to clone (tart), or the Linux image guests start from (container)");
    fs.string("switch", "", "the Hyper-V switch a hyperv place joins; the Default Switch by default");
    fs.int("places", 0, "how many places; as many as the budget fits by default, at most 2 tart places and 1 host place");
    fs.int("place-cores", 0, "cores each place gets; 4 for tart, 2 otherwise");
    fs.int("place-memory", 0, "memory each place gets, in MB; 8192 for tart, 4096 otherwise");
    fs.int("disk-gb", 0, "a KVM or Hyper-V place's disk size in GB, or the size a tart clone grows to");
    fs.int("room-cores", 1, "cores kept for the machine's owner");
    fs.int("room-memory", 1024, "memory kept for the machine's owner, in MB");
    fs.list("pool", "a pool the places serve; every pool by default");
    // The machine's name comes first: runner-up machine add NAME [flags].
    let Some(name) = args.first().filter(|a| !a.starts_with('-')) else {
        if let Some(code) = cli::parse(&mut fs, args, false) {
            return code;
        }
        fs.print_usage();
        return 2;
    };
    if let Some(code) = cli::parse(&mut fs, &args[1..], false) {
        return code;
    }
    let mut cfg = match config::load_config(&fs.get_str("config")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut m = Machine {
        name: name.clone(),
        ssh: fs.get_str("ssh").split_whitespace().map(String::from).collect(),
        agent: fs.get_str("agent"),
        ..Default::default()
    };
    if !m.local() && remote_windows(&m) {
        m.os = "windows".into();
    }
    if let Err(e) = ensure_agent(&mut m, &fs.get_str("install"), &mut std::io::stdout()) {
        eprintln!("{e}");
        return 1;
    }
    let pr = match probe_machine(&m) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if pr.os == "windows" {
        println!("{name}: {}/{}, {} cores, {} MB, account: {}, hyperv: {}", pr.os, pr.arch, pr.cores, pr.memory_mb, pr.account, pr.hyperv);
    } else {
        print!(
            "{name}: {}/{}, {} cores, {} MB, docker: {}, kvm: {}, tart: {}, host: {}",
            pr.os, pr.arch, pr.cores, pr.memory_mb, pr.docker, pr.kvm, pr.tart, pr.host
        );
        if pr.os == "darwin" && !pr.container.is_empty() {
            print!(", container: {}", pr.container);
        }
        println!();
    }
    let o = PlaceOptions {
        provider: fs.get_str("provider"),
        image: fs.get_str("image"),
        switch: fs.get_str("switch"),
        places: fs.get_int("places"),
        cores: fs.get_int("place-cores"),
        memory_mb: fs.get_int("place-memory"),
        disk_gb: fs.get_int("disk-gb"),
        room_cores: fs.get_int("room-cores"),
        room_memory_mb: fs.get_int("room-memory"),
        pools: fs.get_list("pool"),
    };
    if let Err(e) = add_machine(&mut cfg, m, &pr, o) {
        eprintln!("{e}");
        return 1;
    }
    let config_path = fs.get_str("config");
    if let Err(e) = config::save_config(&config_path, &cfg) {
        eprintln!("{e}");
        return 1;
    }
    println!("{}", super::machine_line(&cfg.machine(name)));
    println!("wrote {config_path}; a running runner-up serve applies it, or start runner-up serve");
    0
}

#[derive(Default)]
pub(super) struct PlaceOptions {
    pub provider: String,
    pub image: String,
    pub switch: String,
    pub places: i64,
    pub cores: i64,
    pub memory_mb: i64,
    pub disk_gb: i64,
    pub room_cores: i64,
    pub room_memory_mb: i64,
    pub pools: Vec<String>,
}

/// Names each provider's places in messages.
fn place_kind(provider: &str) -> Option<&'static str> {
    Some(match provider {
        config::PROVIDER_DOCKER => "Docker",
        config::PROVIDER_KVM => "KVM",
        config::PROVIDER_TART => "tart",
        config::PROVIDER_HOST => "host",
        config::PROVIDER_ACCOUNT => "account",
        config::PROVIDER_HYPERV => "Hyper-V",
        config::PROVIDER_CONTAINER => "Apple container",
        _ => return None,
    })
}

/// Adds machine m, as probed, to cfg with its places.
pub(super) fn add_machine(cfg: &mut Config, mut m: Machine, pr: &Probe, mut o: PlaceOptions) -> Result<(), String> {
    if cfg.machines.iter().any(|x| x.name == m.name) {
        return Err(format!("machine {} is already in the setup", crate::go::quote(&m.name)));
    }
    match pr.os.as_str() {
        "linux" => {
            if o.provider.is_empty() {
                o.provider = config::PROVIDER_DOCKER.into();
            }
        }
        "darwin" => {
            if o.provider.is_empty() {
                o.provider = config::PROVIDER_TART.into();
            }
            if o.provider == config::PROVIDER_DOCKER || o.provider == config::PROVIDER_KVM {
                return Err(format!("{} runs {}; {} places run on Linux machines only: use tart, host or container", m.name, pr.os, o.provider));
            }
        }
        "windows" => {
            m.os = "windows".into();
            if o.provider.is_empty() {
                o.provider = config::PROVIDER_ACCOUNT.into();
            }
        }
        _ => return Err(format!("{} runs {}; this version runs places on Linux, macOS and Windows only", m.name, pr.os)),
    }
    if config::WINDOWS_PROVIDERS.contains(&o.provider.as_str()) && pr.os != "windows" {
        return Err(format!("{} runs {}; {} places run on Windows machines only", m.name, pr.os, o.provider));
    }
    let tart = o.provider == config::PROVIDER_TART;
    if o.cores == 0 {
        o.cores = if tart { 4 } else { 2 };
    }
    if o.memory_mb == 0 {
        o.memory_mb = if tart { 8192 } else { 4096 };
    }
    // An agent older than its container reading gives none for container.
    if let Some(kind) = place_kind(&o.provider) {
        let r = pr.ready(&o.provider);
        if r != "ok" && (!r.is_empty() || o.provider != config::PROVIDER_CONTAINER) {
            return Err(format!("{} cannot run {kind} places: {r}", m.name));
        }
    }
    let missing_image = match o.provider.as_str() {
        config::PROVIDER_TART => Some(format!(
            "a tart place needs --image, a macOS tart image pulled on {}, such as ghcr.io/cirruslabs/macos-sequoia-base:latest",
            m.name
        )),
        config::PROVIDER_HYPERV => {
            Some(format!("a Hyper-V place needs --image, the full path of a VHDX base disk on {}, such as C:\\runner-up\\noble.vhdx", m.name))
        }
        config::PROVIDER_KVM => {
            Some(format!("a KVM place needs --image, the path of a cloud image on {}, such as Ubuntu's noble-server-cloudimg", m.name))
        }
        config::PROVIDER_CONTAINER => {
            if pr.os != "darwin" {
                return Err(format!("{} runs {}; Apple container places run on Macs only", m.name, pr.os));
            }
            Some(format!("an Apple container place needs --image, a Linux image on {} with python3 and a user named runner", m.name))
        }
        _ => None,
    };
    if let Some(msg) = missing_image
        && o.image.is_empty()
    {
        return Err(msg);
    }
    (m.cores, m.memory_mb) = (pr.cores, pr.memory_mb);
    m.room_cores = o.room_cores.min((m.cores - 1).max(0));
    m.room_memory_mb = o.room_memory_mb.min((m.memory_mb - 1).max(0));
    let b = m.budget();
    let mut n = o.places;
    if n == 0 {
        n = b.cores / o.cores;
        if b.memory_mb > 0 {
            n = n.min(b.memory_mb / o.memory_mb);
        }
        if n == 0 {
            return Err(format!(
                "{} has {} cores and {} MB for places, less than one place of {} cores and {} MB",
                m.name, b.cores, b.memory_mb, o.cores, o.memory_mb
            ));
        }
        // More tart places than macOS runs VMs would only take turns, and
        // host places share one user's home folder.
        match o.provider.as_str() {
            config::PROVIDER_TART => n = n.min(agent::MACOS_VM_LIMIT as i64),
            config::PROVIDER_HOST => n = 1,
            _ => {}
        }
    }
    let pools = if o.pools.is_empty() { cfg.labels() } else { o.pools };
    for i in 1..=n {
        m.places.push(Place {
            id: format!("{}-{i}", m.name),
            provider: o.provider.clone(),
            pools: pools.clone(),
            cores: o.cores,
            memory_mb: o.memory_mb,
            image: o.image.clone(),
            disk_gb: o.disk_gb,
            switch: o.switch.clone(),
            ..Default::default()
        });
    }
    cfg.machines.push(m);
    cfg.defaults();
    if let Err(e) = cfg.validate() {
        cfg.machines.pop();
        return Err(e);
    }
    Ok(())
}

/// Where machine add installs runner-up on a machine, relative to the home
/// folder ssh starts commands in.
pub(super) const INSTALLED_AGENT: &str = ".local/bin/runner-up";

/// A finished command's stdout, stderr and exit code; the code is None when
/// it did not start or a signal ended it, and err then says why.
struct Ran {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    err: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Go's exec error: "exit status N", or why it did not run.
    fn failure(&self) -> String {
        match self.code {
            Some(c) => format!("exit status {c}"),
            None => self.err.clone(),
        }
    }
}

// shortcut: no deadline, as Go's 5-minute context gave; ssh_args time out a
// machine that does not answer, so only a hung remote command waits.
fn run(argv: &[String], stdin: Stdio) -> Ran {
    match Command::new(&argv[0]).args(&argv[1..]).stdin(stdin).output() {
        Ok(o) => Ran {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            code: o.status.code(),
            err: o.status.to_string(),
        },
        Err(e) => Ran { stdout: String::new(), stderr: String::new(), code: None, err: e.to_string() },
    }
}

/// Checks runner-up runs on the machine, and installs it in ~/.local/bin
/// when it does not: from build, or from this program when the machine's
/// system and architecture match it.
pub(super) fn ensure_agent(m: &mut Machine, build: &str, out: &mut dyn Write) -> Result<(), String> {
    let version = vec!["version".to_string()];
    if m.windows() {
        let r = run(&agent_command(m, &version), Stdio::null());
        if !r.ok() {
            return Err(format!(
                "runner-up is not set up on {}, a Windows machine: on it, run runner-up setup as an administrator (see the README), then add it again",
                m.name
            ));
        }
        let _ = writeln!(out, "{}: runner-up {}", m.name, r.stdout.trim());
        return Ok(());
    }
    let mut tries = vec![m.agent.clone()];
    if m.agent.is_empty() && !m.local() {
        // An earlier machine add installed it where ssh's shell may not look.
        tries.push(INSTALLED_AGENT.into());
    }
    let mut last = None;
    for a in tries {
        let tried = Machine { agent: a.clone(), ..m.clone() };
        let r = run(&agent_command(&tried, &version), Stdio::null());
        if r.ok() {
            m.agent = a;
            let _ = writeln!(out, "{}: runner-up {}", m.name, r.stdout.trim());
            return Ok(());
        }
        // 127 is the shell's "not found"; anything else is a real failure.
        let not_found = r.code == Some(127);
        last = Some(r);
        if !not_found {
            break;
        }
    }
    let last = last.expect("tries is never empty");
    if m.local() {
        return Err(format!("runner-up is not on this machine's PATH as {}: pass --agent", m.agent_path()));
    }
    if last.code != Some(127) {
        return Err(format!(
            "could not run runner-up on {} over ssh: {} {}",
            m.name,
            last.failure(),
            agent::first_line(&last.stderr, "")
        ));
    }
    let mut build = build.to_string();
    if build.is_empty() {
        let mut uname = ssh_args(m);
        uname.push("uname -sm".into());
        let u = run(&uname, Stdio::null());
        if !u.ok() {
            return Err(format!("could not read {}'s system: {}", m.name, u.failure()));
        }
        let want = uname_of(crate::release::goos(), crate::release::goarch());
        if u.stdout.trim() != want {
            return Err(format!(
                "runner-up is not on {}, which runs {} and this program is built for {want}: build one for it and pass --install FILE",
                m.name,
                u.stdout.trim()
            ));
        }
        build = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    }
    let f = std::fs::File::open(&build).map_err(|e| format!("open {build}: {e}"))?;
    let mut cp = ssh_args(m);
    cp.push(format!(
        "mkdir -p .local/bin && cat > {INSTALLED_AGENT}.new && chmod 0755 {INSTALLED_AGENT}.new && mv {INSTALLED_AGENT}.new {INSTALLED_AGENT}"
    ));
    let r = run(&cp, Stdio::from(f));
    if !r.ok() {
        return Err(format!("could not copy runner-up to {}: {}", m.name, agent::first_line(&format!("{}{}", r.stdout, r.stderr), &r.failure())));
    }
    m.agent = INSTALLED_AGENT.into();
    let _ = writeln!(out, "{}: installed runner-up in ~/.local/bin", m.name);
    Ok(())
}

/// Says whether the machine m reaches over SSH runs Windows, whose sign-in
/// runs cmd or PowerShell, either of which runs cmd /c ver.
fn remote_windows(m: &Machine) -> bool {
    let mut argv = ssh_args(m);
    argv.push("cmd /c ver".into());
    let r = run(&argv, Stdio::null());
    r.ok() && r.stdout.contains("Microsoft Windows")
}

/// What uname -sm prints on goos/goarch: Linux calls arm64 aarch64, and
/// macOS calls it arm64.
pub(super) fn uname_of(goos: &str, goarch: &str) -> String {
    let arch = match (goos, goarch) {
        ("darwin", "arm64") => "arm64",
        (_, "amd64") => "x86_64",
        (_, "arm64") => "aarch64",
        (_, a) => a,
    };
    let system = match goos {
        "linux" => "Linux",
        "darwin" => "Darwin",
        _ => "",
    };
    format!("{system} {arch}")
}

fn probe_machine(m: &Machine) -> Result<Probe, String> {
    let r = run(&agent_command(m, &["agent".into(), "probe".into()]), Stdio::null());
    if !r.ok() {
        return Err(format!("could not probe {}: {} {}", m.name, r.failure(), agent::first_line(&r.stderr, "")));
    }
    serde_json::Deserializer::from_str(&r.stdout)
        .into_iter::<Probe>()
        .next()
        .and_then(Result::ok)
        .ok_or_else(|| format!("{} answered the probe with something unreadable", m.name))
}
