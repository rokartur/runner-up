//! The agent runs on each machine: it makes a place's container, virtual
//! machine or host folder, runs one job in it and removes it.
//!
//! The agent is this same program on each machine, run by the coordinator over
//! SSH (or directly on the coordinator's own machine) as `runner-up agent run`.
//! It reads the runner's one-time registration from its first line of input,
//! makes a fresh place with its provider, runs the runner there for one job and
//! removes the place whatever happened. Its input stays open while the
//! coordinator's place runner wants the runner; when it closes (the place
//! runner ended, gave up, or the SSH connection dropped) the agent stops the
//! runner and cleans up. Before making a place it removes anything left of the
//! same place by an agent that was killed outright.

mod account;
pub(crate) mod container;
pub(crate) mod docker;
mod forkguard;
mod host;
pub(crate) mod hyperv;
mod iso;
pub(crate) mod jobhost;
#[cfg(windows)]
mod jobtree;
mod kvm;
pub(crate) mod launch;
pub(crate) mod runner;
#[cfg(windows)]
mod service;
mod setup;
pub(crate) mod tart;
mod usage;
mod usage_measure;
#[cfg(windows)]
mod win32;
#[cfg(windows)]
mod winhost;
#[cfg(windows)]
mod winpipe;

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::ctx::Ctx;
use crate::go::flag::FlagSet;
use crate::{cli, lock, signals, version};

pub use account::run_service;
#[cfg(all(test, unix))]
pub use docker::DEFAULT_DOCKER_IMAGE;
pub use forkguard::{fork_guard_hook, run_fork_guard};
pub use jobhost::ServiceUpdate;
pub use launch::RunnerLaunch;
pub use setup::setup_main;
pub use tart::{MACOS_VM_LIMIT, clone_name};
pub use usage::{USAGE_PREFIX, Usage, parse_usage};
pub use usage_measure::measure_main;

/// Exit codes of run-place and agent run.
pub const EXIT_DONE: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_UNSTARTED: i32 = 3;

/// Makes a machine's places.
pub trait Provider: Sync {
    /// Makes a fresh place, starts the runner in it with registration jit,
    /// writes the runner's output to out and returns once the runner has
    /// ended or ctx is done. The caller cleans up after it.
    fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String>;
    /// Removes everything left of the place, running or not.
    fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String>;
    /// What the place's runner gets besides its registration: the fork guard
    /// unless forks are allowed, and the place's size (launch.rs).
    fn launch(&self, a: &AgentRun) -> RunnerLaunch;
    /// "ok" when this machine can make the provider's places, or why not.
    /// `agent probe` reports it for `machine add`.
    fn ready(&self) -> String;
}

/// A pipe the agent talks over (the Hyper-V serial port, the service's
/// pipe): its two directions and a close that ends a read blocked on it.
pub struct Conn {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub close: Arc<dyn Fn() + Send + Sync>,
}
#[derive(Clone, Debug, Default)]
pub struct AgentRun {
    pub provider: String,
    pub place: String,
    pub cores: i64,
    pub memory_mb: i64,
    pub disk_gb: i64,
    pub image: String,
    /// The Hyper-V switch a Hyper-V place joins.
    pub switch: String,
    /// The runner's tool cache for a host or tart place.
    pub tool_cache: String,
    pub boot_timeout: Duration,
    /// How often a tart place reads its clone's boot session.
    pub guest_check: Duration,
    pub allow_forks: bool,
    /// The agent's own folder on its machine.
    pub state_dir: String,
    /// This program, which a place runs as its fork guard.
    pub self_path: String,
}

impl AgentRun {
    pub fn dir(&self) -> PathBuf {
        Path::new(&self.state_dir).join("places").join(&self.place)
    }

    /// Lets jobs on uncapped host places keep their workers within their
    /// reservation, and tells jobs in containers and VMs the same size.
    pub fn size_env(&self) -> Vec<String> {
        vec![format!("RUNNER_UP_CORES={}", self.cores), format!("RUNNER_UP_MEMORY_MB={}", self.memory_mb)]
    }
}

fn provider_for(name: &str) -> Result<&'static dyn Provider, String> {
    Ok(match name {
        config::PROVIDER_DOCKER => &docker::Docker,
        config::PROVIDER_KVM => &kvm::Kvm,
        config::PROVIDER_TART => &tart::Tart,
        config::PROVIDER_HOST => &host::Host,
        config::PROVIDER_ACCOUNT => &account::Account,
        config::PROVIDER_HYPERV => &hyperv::HyperV,
        config::PROVIDER_CONTAINER => &container::AppleContainer { tool: "container" },
        _ => return Err(format!("unknown provider {}", crate::go::quote(name))),
    })
}

/// The agent's commands, the ones agent_main takes.
pub const AGENT_COMMANDS: [&str; 7] = ["probe", "vms", "run", "clean", "measure", "service-version", "update-service"];

/// `runner-up agent`.
pub fn agent_main(args: &[String]) -> i32 {
    let usage = format!("usage: runner-up agent {}", AGENT_COMMANDS.join("|"));
    if cli::help(args) {
        println!("{usage}");
        return 0;
    }
    let Some(command) = args.first() else {
        eprintln!("{usage}");
        return 2;
    };
    if !AGENT_COMMANDS.contains(&command.as_str()) {
        eprintln!("unknown agent command {command}");
        eprintln!("{usage}");
        return 2;
    }
    // Only run, clean and measure take more than their name.
    if matches!(command.as_str(), "service-version" | "update-service" | "probe" | "vms") {
        let mut fs = FlagSet::new(&format!("agent {command}"));
        if let Some(code) = cli::parse(&mut fs, &args[1..], false) {
            return code;
        }
    }
    match command.as_str() {
        "service-version" => {
            // The version of runner-up's Windows service here.
            match account::service_version(&Ctx::detached_timeout(Duration::from_secs(60))) {
                Ok(v) => {
                    println!("{v}");
                    return 0;
                }
                Err(e) => {
                    eprintln!("{e}");
                    return EXIT_FAILED;
                }
            }
        }
        "update-service" => {
            // Hand runner-up's Windows service here a signed release.
            return jobhost::update_service_main(&mut std::io::stdin().lock(), &mut std::io::stdout(), &account::update_service);
        }
        "measure" => return measure_main(&args[1..]),
        "probe" => {
            println!("{}", serde_json::to_string_pretty(&probe()).unwrap());
            return 0;
        }
        "vms" => {
            // The macOS virtual machines running here, for the coordinator's
            // count against the framework's limit.
            match tart::running_mac_vms(&Ctx::detached_timeout(Duration::from_secs(60))) {
                Ok(vms) => {
                    println!("{}", serde_json::to_string(&vms).unwrap());
                    return 0;
                }
                Err(e) => {
                    eprintln!("{e}");
                    return EXIT_FAILED;
                }
            }
        }
        _ => {}
    }
    let mut fs = FlagSet::new(&format!("agent {command}"));
    fs.string("provider", "", "docker, kvm, tart, host, account, hyperv or container");
    fs.string("place", "", "the place's id");
    fs.int("cores", 2, "cores the place gets");
    fs.int("memory", 4096, "memory the place gets, in MB");
    fs.int("disk-gb", 0, "a KVM place's disk size, 40 GB by default, or the size a tart clone or Hyper-V disk grows to");
    fs.string("image", "", "the runner image, a KVM or Hyper-V place's base disk image, the tart image a tart place clones, or the image an Apple container place starts from");
    fs.string("switch", config::DEFAULT_SWITCH, "the Hyper-V switch a Hyper-V place joins");
    fs.string(
        "tool-cache",
        "",
        "the runner's tool cache: a host place's, kept between jobs (in the place's folder by default), or a folder inside a tart clone",
    );
    fs.int("boot-timeout", 300, "seconds a KVM, tart or Hyper-V place may take to boot");
    fs.int("guest-check", 30, "seconds between readings of a tart clone's boot session");
    fs.bool("allow-forks", false, "run jobs from other repositories' pull requests");
    fs.string(
        "state-dir",
        "",
        "the agent's folder; ~/.local/state/runner-up/agent by default, or %LOCALAPPDATA%\\runner-up\\agent on Windows",
    );
    if let Some(code) = cli::parse(&mut fs, &args[1..], false) {
        return code;
    }
    let mut a = AgentRun {
        provider: fs.get_str("provider"),
        place: fs.get_str("place"),
        cores: fs.get_int("cores"),
        memory_mb: fs.get_int("memory"),
        disk_gb: fs.get_int("disk-gb"),
        image: fs.get_str("image"),
        switch: fs.get_str("switch"),
        tool_cache: fs.get_str("tool-cache"),
        boot_timeout: Duration::from_secs(fs.get_int("boot-timeout").max(0) as u64),
        guest_check: Duration::from_secs(fs.get_int("guest-check").max(1) as u64),
        allow_forks: fs.get_bool("allow-forks"),
        state_dir: fs.get_str("state-dir"),
        self_path: String::new(),
    };
    if a.state_dir.is_empty() {
        a.state_dir = Path::new(&config::default_state_dir()).join("agent").to_string_lossy().into_owned();
    }
    if a.place.is_empty() || a.place.contains(['/', ' ', '\t', '\n']) {
        eprintln!("a plain --place is needed");
        return 2;
    }
    let p = match provider_for(&a.provider) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match std::env::current_exe() {
        Ok(path) => a.self_path = path.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("{e}");
            return EXIT_FAILED;
        }
    }
    if command == "clean" {
        return agent_clean(&a, p);
    }
    agent_run_place(&a, p, Box::new(std::io::stdin()), Box::new(std::io::stdout()))
}

/// `agent run`.
fn agent_run_place(a: &AgentRun, p: &dyn Provider, input: Box<dyn Read + Send>, stdout: Box<dyn Write + Send>) -> i32 {
    // Rust ignores SIGPIPE, so a write to a dropped SSH connection fails rather
    // than kill the agent before it cleans up; Out drops the error.
    let out = Arc::new(Out::new(stdout));
    let mut r = BufReader::new(input);
    let mut jit = String::new();
    let read = r.read_line(&mut jit);
    let jit = jit.trim().to_string();
    if jit.is_empty() {
        let why = match read {
            Ok(_) => "EOF".to_string(),
            Err(e) => e.to_string(),
        };
        out.say(&format!("no runner registration on input ({why})"));
        return EXIT_FAILED;
    }
    let ctx = signals::notify_stop(&Ctx::background());
    let watched = ctx.clone();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        watched.cancel();
    });

    let _held = match lock_place(&ctx, a) {
        Ok(h) => h,
        Err(e) => {
            out.say(&e);
            return EXIT_FAILED;
        }
    };
    let clean_up = || {
        let c = Ctx::detached_timeout(Duration::from_secs(120));
        if let Err(e) = p.clean(&c, a) {
            out.say(&format!("could not clean up place {}: {e}", a.place));
        }
    };
    clean_up();
    out.say(&format!("making place {} with {} ({} cores, {} MB)", a.place, a.provider, a.cores, a.memory_mb));
    let result = p.run(&ctx, a, &jit, &out);
    let code = match result {
        Err(e) => {
            out.say(&format!("place {} failed: {e}", a.place));
            EXIT_FAILED
        }
        Ok(()) if ctx.is_done() => {
            out.say(&format!("place {} stopped", a.place));
            EXIT_DONE
        }
        Ok(()) => {
            out.say(&format!("place {} finished", a.place));
            EXIT_DONE
        }
    };
    clean_up();
    code
}

fn agent_clean(a: &AgentRun, p: &dyn Provider) -> i32 {
    let ctx = Ctx::detached_timeout(Duration::from_secs(180));
    let _held = match lock_place(&ctx, a) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_FAILED;
        }
    };
    if let Err(e) = p.clean(&ctx, a) {
        eprintln!("{e}");
        return EXIT_FAILED;
    }
    EXIT_DONE
}

/// A held place lock. Dropping it deletes the lock file and the place's
/// folder, if nothing else is left in it, so a finished place leaves nothing
/// behind. The file is deleted while still locked, so an agent that opened it
/// before then and locks it after finds it is no longer the place's lock file
/// and tries again.
pub struct PlaceLock {
    file: Option<File>,
    path: PathBuf,
    dir: PathBuf,
}

impl Drop for PlaceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        drop(self.file.take());
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Keeps two agents out of one place: a new run waits for an earlier agent
/// still cleaning up, for up to two minutes.
pub fn lock_place(ctx: &Ctx, a: &AgentRun) -> Result<PlaceLock, String> {
    let dir = a.dir();
    let path = dir.join("agent.lock");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        config::create_dir_all_mode(&dir, 0o700).map_err(|e| e.to_string())?;
        let f = match lock::busy(&path) {
            // The holder before deleted the folder in between.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
            Ok(f) => f,
        };
        if let Some(f) = f {
            if is_file_at(&f, &path) {
                return Ok(PlaceLock { file: Some(f), path, dir });
            }
            // The holder before deleted it: lock the file now at path.
            continue;
        }
        if Instant::now() > deadline {
            return Err(format!("place {} is still held by another agent", a.place));
        }
        if ctx.sleep(Duration::from_secs(1)) {
            return Err(format!("stopped while waiting for place {}", a.place));
        }
    }
}

/// Whether the open file f is the one now at path.
#[cfg(unix)]
fn is_file_at(f: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(now)) => held.dev() == now.dev() && held.ino() == now.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn is_file_at(f: &File, path: &Path) -> bool {
    // Windows will not delete a file another handle has open without
    // FILE_SHARE_DELETE, so the file at path is the one held when it opens.
    let _ = f;
    path.exists()
}

/// Output for the coordinator's log. Write errors are dropped: a lost
/// connection must not stop the clean-up.
pub struct Out(Mutex<Box<dyn Write + Send>>);

impl Out {
    pub fn new(w: Box<dyn Write + Send>) -> Out {
        Out(Mutex::new(w))
    }

    pub fn write(&self, b: &[u8]) {
        let mut w = self.0.lock().unwrap();
        let _ = w.write_all(b);
        let _ = w.flush();
    }

    pub fn line(&self, s: &str) {
        self.write(format!("{s}\n").as_bytes());
    }

    pub fn say(&self, s: &str) {
        self.line(&format!("runner-up agent: {s}"));
    }
}

/// The threads copying a child's output into an Out.
pub struct Piped(Vec<JoinHandle<()>>);

impl Piped {
    /// Waits for the child's output to end, for at most limit: a process the
    /// job leaves running may keep it open.
    pub fn finish(self, limit: Duration) {
        let until = Instant::now() + limit;
        for h in self.0 {
            while !h.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

/// Starts cmd with its stdout and stderr copied into out.
pub fn spawn_into(cmd: &mut Command, out: &Arc<Out>) -> std::io::Result<(Child, Piped)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let copy = |mut r: Box<dyn Read + Send>, out: Arc<Out>| {
        std::thread::spawn(move || {
            let mut b = [0u8; 8192];
            while let Ok(n) = r.read(&mut b) {
                if n == 0 {
                    break;
                }
                out.write(&b[..n]);
            }
        })
    };
    let so: ChildStdout = child.stdout.take().unwrap();
    let se: ChildStderr = child.stderr.take().unwrap();
    let handles = vec![copy(Box::new(so), out.clone()), copy(Box::new(se), out.clone())];
    Ok((child, Piped(handles)))
}

/// Waits for child. When ctx is done first, it calls stop, then kills the
/// process if it has not ended within grace.
pub fn wait_or_stop(ctx: &Ctx, child: &mut Child, stop: &dyn Fn(), grace: Duration) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        if ctx.sleep(Duration::from_millis(50)) {
            break;
        }
    }
    stop();
    let until = Instant::now() + grace;
    while Instant::now() < until {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    child.wait()
}

/// What `agent probe` reports about its machine, for `machine add`: for each
/// provider, "ok" when it can run here, or why not.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cores: i64,
    #[serde(default, rename = "memoryMB")]
    pub memory_mb: i64,
    #[serde(default)]
    pub docker: String,
    #[serde(default)]
    pub kvm: String,
    #[serde(default)]
    pub tart: String,
    #[serde(default)]
    pub host: String,
    /// Account and HyperV are for Windows machines.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    #[serde(default, rename = "hyperv", skip_serializing_if = "String::is_empty")]
    pub hyperv: String,
    /// Empty from an agent older than its reading.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub container: String,
}

impl Probe {
    fn reading(&mut self, provider: &str) -> Option<&mut String> {
        Some(match provider {
            config::PROVIDER_DOCKER => &mut self.docker,
            config::PROVIDER_KVM => &mut self.kvm,
            config::PROVIDER_TART => &mut self.tart,
            config::PROVIDER_HOST => &mut self.host,
            config::PROVIDER_ACCOUNT => &mut self.account,
            config::PROVIDER_HYPERV => &mut self.hyperv,
            config::PROVIDER_CONTAINER => &mut self.container,
            _ => return None,
        })
    }

    /// The answer for provider: "ok", why not, or empty when the agent gave
    /// none.
    pub fn ready(&self, provider: &str) -> String {
        self.clone().reading(provider).map(|r| r.clone()).unwrap_or_default()
    }
}

pub fn num_cpu() -> i64 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as i64)
}

/// Reads the machine. A Windows machine answers only for its own providers,
/// and other machines leave those out.
fn probe() -> Probe {
    let mut p = Probe {
        version: version::version().to_string(),
        os: crate::release::goos().to_string(),
        arch: crate::release::goarch().to_string(),
        cores: num_cpu(),
        memory_mb: account::memory_mb(),
        ..Default::default()
    };
    let windows = cfg!(windows);
    for name in config::PROVIDERS {
        let pr = provider_for(name).unwrap();
        if config::WINDOWS_PROVIDERS.contains(&name) == windows {
            *p.reading(name).unwrap() = pr.ready();
        } else if windows {
            *p.reading(name).unwrap() = "Windows machines run account and hyperv places".into();
        }
    }
    p
}

/// A process's exit code as Go's ExitCode reports it: -1 when it did not
/// exit by itself (a signal) or could not be waited for.
pub fn exit_code(status: &std::io::Result<ExitStatus>) -> i32 {
    match status {
        Ok(s) => s.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

/// The first line of a command's output, or its error when it wrote nothing.
pub fn first_line(out: &str, err: &str) -> String {
    let line = out.trim().split('\n').next().unwrap_or("");
    if line.is_empty() { err.to_string() } else { line.to_string() }
}

/// Runs cmd to its end and gives its combined output, or its first line as
/// the error when it fails.
pub fn run_quiet(ctx: &Ctx, cmd: &mut Command) -> Result<String, String> {
    let (b, status) = crate::proc::combined_output(ctx, cmd);
    let out = String::from_utf8_lossy(&b).into_owned();
    match status {
        Ok(()) => Ok(out),
        Err(e) => Err(first_line(&out, &e)),
    }
}

/// Makes dir and returns it.
pub fn ensure_dir(dir: &Path) -> Result<PathBuf, String> {
    config::create_dir_all_mode(dir, 0o700).map_err(|e| e.to_string())?;
    Ok(dir.to_path_buf())
}

/// A command line that runs script in Windows PowerShell, which cmd and
/// PowerShell read the same: the script travels base64 encoded.
pub fn encoded_script(script: &str) -> Vec<String> {
    use base64::Engine;
    let b: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]
        .into_iter()
        .map(String::from)
        .chain([base64::engine::general_purpose::STANDARD.encode(b)])
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
