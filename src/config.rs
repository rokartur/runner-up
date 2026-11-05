//! Reads, checks and writes runner-up's setup file.

use crate::go::{self, quote as q};
use crate::quote::{WIN_PLAIN, check_win_word};
use crate::release;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Runs each job in a new container of a runner image, removed after the job.
pub const PROVIDER_DOCKER: &str = "docker";
/// Runs each job in a new KVM virtual machine on a copy-on-write disk over a
/// base cloud image, deleted after the job.
pub const PROVIDER_KVM: &str = "kvm";
/// Runs each job in a new tart clone of a macOS image on a Mac with Apple
/// silicon, deleted after the job.
pub const PROVIDER_TART: &str = "tart";
/// Runs each job on the machine itself, as the agent's user, in a runner
/// folder made for the job and removed after it.
pub const PROVIDER_HOST: &str = "host";
/// Runs each job on a Windows machine itself, as a fresh standard account
/// runner-up's Windows service makes for the job and deletes after it.
pub const PROVIDER_ACCOUNT: &str = "account";
/// Runs each job in a new Hyper-V Linux virtual machine on a Windows machine,
/// on a differencing disk over a base disk image, deleted after the job.
pub const PROVIDER_HYPERV: &str = "hyperv";
/// Runs each job in a new Linux guest made with Apple's container tool on a
/// Mac with Apple silicon, from an image with python3 and a user named
/// runner, removed after the job.
pub const PROVIDER_CONTAINER: &str = "container";

pub const PROVIDERS: [&str; 7] =
    [PROVIDER_DOCKER, PROVIDER_KVM, PROVIDER_TART, PROVIDER_HOST, PROVIDER_ACCOUNT, PROVIDER_HYPERV, PROVIDER_CONTAINER];

/// Providers that run on Windows machines, and only there.
pub const WINDOWS_PROVIDERS: [&str; 2] = [PROVIDER_ACCOUNT, PROVIDER_HYPERV];

fn zero(n: &i64) -> bool {
    *n == 0
}

pub fn null_as_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// The whole setup. `runner-up init` and `runner-up machine add` write it; a
/// person may edit it too. It never holds a token: the coordinator asks the
/// GitHub command line for one each time it opens a session.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// Where the coordinator keeps its state, each place's lock and the place logs.
    #[serde(default)]
    pub state_dir: String,
    #[serde(default)]
    pub github: GitHub,
    /// The labels jobs ask for in runs-on. Each pool gets one runner scale set
    /// per repository or organization in GitHub.
    #[serde(default, deserialize_with = "null_as_default")]
    pub pools: Vec<Pool>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub machines: Vec<Machine>,
    /// The rules that pick which waiting job a free place takes, in order.
    /// Empty means the default order; plan::parse_order reads and checks them.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_as_default")]
    pub order: Vec<String>,
    /// Lets jobs from another repository's pull request run. Off by default:
    /// every runner starts with the fork guard as its job-started hook.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_fork_pull_requests: bool,
    /// The capacity each scale set announces with every poll: GitHub assigns
    /// the set up to that many waiting jobs and tells the coordinator of each.
    /// It stays above the number of places, so the coordinator hears of every
    /// waiting job and picks the order, not GitHub.
    #[serde(default, skip_serializing_if = "zero")]
    pub announce_capacity: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub start_timeout_seconds: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub boot_timeout_seconds: i64,
    /// How often a tart place reads its clone's boot session while the job
    /// runs, to end the job if the clone's macOS restarted.
    #[serde(default, skip_serializing_if = "zero")]
    pub guest_check_seconds: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub max_job_seconds: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub tick_seconds: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub log_keep_days: i64,
    /// The runner-up release the coordinator runs and copies to every
    /// machine as its agent (coordinator::program).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Release>,
}

/// A runner-up release and each of its archives' SHA-256.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The release's version, such as v0.1.11.
    #[serde(default)]
    pub version: String,
    /// Each platform's archive's SHA-256, by platform, such as darwin-arm64
    /// or windows-amd64, as the release's checksums.txt lists it. A platform
    /// left out is one no machine of the setup needs.
    #[serde(default, deserialize_with = "null_as_default")]
    pub sha256: BTreeMap<String, String>,
    /// Where the archives are: each at url/<version>/<archive>. GitHub's
    /// runner-up releases when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

impl Release {
    /// Where the release's file named name is.
    pub fn archive_url(&self, name: &str) -> String {
        let base = if self.url.is_empty() { release::DEFAULT_URL } else { &self.url };
        format!("{}/{}/{}", base.trim_end_matches('/'), self.version, name)
    }

    fn check(&self) -> Result<(), String> {
        if !release::valid_version(&self.version) {
            return Err(format!("release: version {} is not a release version such as v0.1.11", q(&self.version)));
        }
        if self.sha256.is_empty() {
            return Err("release: sha256 names no platform's archive".into());
        }
        for (platform, sum) in &self.sha256 {
            if !release::PLATFORMS.contains(&platform.as_str()) {
                return Err(format!("release: sha256 names platform {}; a release has {}", q(platform), go::list(&release::PLATFORMS)));
            }
            if !release::valid_sha256(sum) {
                return Err(format!("release: {platform}'s sha256 is not 64 lowercase hex digits"));
            }
        }
        if !self.url.is_empty() && !self.url.starts_with("https://") && !self.url.starts_with("http://") {
            return Err("release: url must be an http or https address".into());
        }
        Ok(())
    }
}

/// What the coordinator serves. The sign-in is the GitHub command line's:
/// `gh auth token` gives the coordinator its token.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GitHub {
    /// Repositories, owner/name, with one scale set per pool each. A personal
    /// account has no organization-wide runners, so its repositories are
    /// listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_as_default")]
    pub repos: Vec<String>,
    /// Organizations with one scale set per pool each, serving every
    /// repository the runner group allows. The sign-in needs admin:org.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_as_default")]
    pub orgs: Vec<String>,
    /// The organization runner group the scale sets join; "Default" when
    /// empty. Repository scale sets always use the default group.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runner_group: String,
    /// The GitHub command line program; "gh" when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh: String,
}

/// One kind of job, named by the label workflows ask for. GitHub chooses
/// which job a runner takes within a pool, so all its jobs share a size.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Pool {
    #[serde(default)]
    pub label: String,
    /// Set both to give every job this size, on any place serving the pool.
    /// Leave both out to keep the place's size, for existing setups.
    #[serde(default, skip_serializing_if = "zero")]
    pub cores: i64,
    #[serde(default, rename = "memoryMB", skip_serializing_if = "zero")]
    pub memory_mb: i64,
}

impl Pool {
    /// The pool's configured reservation, or zero for a legacy pool.
    pub fn size(&self) -> Share {
        Share { cores: self.cores, memory_mb: self.memory_mb }
    }
}

/// One computer whose places run jobs. The coordinator reaches it over SSH,
/// or runs its agent directly when ssh is empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Machine {
    #[serde(default)]
    pub name: String,
    /// The destination and any options for ssh, such as ["you@box"] or
    /// ["-p", "2222", "ci@10.0.0.5"]. Empty is the coordinator's own machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_as_default")]
    pub ssh: Vec<String>,
    /// The runner-up program on the machine; "runner-up" when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// "windows" for a Windows machine, whose SSH server runs cmd or
    /// PowerShell rather than a POSIX shell; empty for Linux and macOS.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// What the machine's places may reserve together, less the room kept
    /// for the machine's owner. A place takes a job only while its share fits
    /// beside the shares of the machine's places whose runners are alive.
    /// Zero counts nothing.
    #[serde(default, skip_serializing_if = "zero")]
    pub cores: i64,
    #[serde(default, rename = "memoryMB", skip_serializing_if = "zero")]
    pub memory_mb: i64,
    /// Kept for the machine's owner: places reserve at most cores-roomCores
    /// and memoryMB-roomMemoryMB.
    #[serde(default, skip_serializing_if = "zero")]
    pub room_cores: i64,
    #[serde(default, rename = "roomMemoryMB", skip_serializing_if = "zero")]
    pub room_memory_mb: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub places: Vec<Place>,
}

/// One slot on a machine that runs one job at a time, each in a fresh
/// container or virtual machine, or on the machine itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Place {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub provider: String,
    /// The labels whose jobs the place takes.
    #[serde(default, deserialize_with = "null_as_default")]
    pub pools: Vec<String>,
    /// The fallback size for pools without a size: container/VM limits and
    /// the reservation against the machine's budget. A sized pool overrides
    /// both. A host job is not held to its reservation.
    #[serde(default)]
    pub cores: i64,
    #[serde(default, rename = "memoryMB")]
    pub memory_mb: i64,
    /// The runner image for a Docker place, the base disk image on the
    /// machine for a KVM or Hyper-V place, the tart image a tart place
    /// clones, such as ghcr.io/cirruslabs/macos-sequoia-base:latest or a
    /// local VM's name, or the image an Apple container place's guests start
    /// from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// Gives some of the place's pools their own image, by pool label: a job
    /// of a named pool runs on that image, any other job on image. The place
    /// stays one place; its job's pool chooses both the image and, when
    /// configured, its reservation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_as_default")]
    pub pool_images: BTreeMap<String, String>,
    /// A KVM place's disk size, DEFAULT_DISK_GB when zero; a tart place's
    /// clone or a Hyper-V place's disk grows to it when its image is smaller.
    #[serde(default, rename = "diskGB", skip_serializing_if = "zero")]
    pub disk_gb: i64,
    /// The Hyper-V virtual switch a Hyper-V place's virtual machine joins;
    /// DEFAULT_SWITCH when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub switch: String,
    /// The runner's tool cache (RUNNER_TOOL_CACHE), where actions such as
    /// setup-node keep the tools they download. A host place keeps it between
    /// jobs, in its folder under the agent's when empty, so a tool is
    /// downloaded once. For a tart place it is a folder inside the clone,
    /// such as one the image was set up with; empty leaves the runner's own,
    /// under its work folder, which every job starts without.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_cache: String,
    /// Where the runner keeps its jobs' work, relative to the runner's folder
    /// or absolute; "_work" when empty. GitHub's Ubuntu runners use
    /// /home/runner/work, where a cache saved there restores.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub work_folder: String,

    /// The place's machine's name, filled in on load.
    #[serde(skip)]
    pub machine: String,
}

/// What a job reserves of its machine while its place runs.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    #[serde(default)]
    pub cores: i64,
    #[serde(default, rename = "memoryMB")]
    pub memory_mb: i64,
}

/// GitHub's own runners answer to these labels; a pool that looked like one
/// would let them take a job meant for a place, or the other way round.
const HOSTED_LABEL_PREFIXES: [&str; 3] = ["ubuntu-", "windows-", "macos-"];
const RESERVED_LABELS: [&str; 8] = ["self-hosted", "linux", "windows", "macos", "x64", "arm64", "arm", "x86"];

pub const DEFAULT_DISK_GB: i64 = 40;

/// The switch Windows makes with Hyper-V, which gives virtual machines the
/// internet through the machine's own connection.
pub const DEFAULT_SWITCH: &str = "Default Switch";

/// Keeps a place's id short enough for the names Windows gives what it makes
/// for the place.
pub const MAX_PLACE_ID: usize = 40;

impl Machine {
    /// What the machine's places may reserve together, after the room kept
    /// for its owner; a zero field counts nothing.
    pub fn budget(&self) -> Share {
        let mut b = Share::default();
        if self.cores > 0 {
            b.cores = self.cores - self.room_cores;
        }
        if self.memory_mb > 0 {
            b.memory_mb = self.memory_mb - self.room_memory_mb;
        }
        b
    }

    pub fn agent_path(&self) -> &str {
        if self.agent.is_empty() { "runner-up" } else { &self.agent }
    }

    pub fn local(&self) -> bool {
        self.ssh.is_empty()
    }

    pub fn windows(&self) -> bool {
        self.os == "windows"
    }
}

impl Place {
    /// The image a job of pool runs on in the place.
    pub fn image_for(&self, pool: &str) -> &str {
        self.pool_images.get(pool).unwrap_or(&self.image)
    }

    pub fn runner_work_folder(&self) -> &str {
        if self.work_folder.is_empty() { "_work" } else { &self.work_folder }
    }

    pub fn share(&self) -> Share {
        Share { cores: self.cores, memory_mb: self.memory_mb }
    }
}

/// The user's config folder, as Go's os.UserConfigDir.
fn user_config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("AppData").map(PathBuf::from);
    }
    if cfg!(target_os = "macos") {
        return home().map(|h| h.join("Library").join("Application Support"));
    }
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => Some(PathBuf::from(x)),
        _ => home().map(|h| h.join(".config")),
    }
}

/// The user's cache folder, as Go's os.UserCacheDir.
pub fn user_cache_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LocalAppData").map(PathBuf::from);
    }
    if cfg!(target_os = "macos") {
        return home().map(|h| h.join("Library").join("Caches"));
    }
    match std::env::var_os("XDG_CACHE_HOME") {
        Some(x) if !x.is_empty() => Some(PathBuf::from(x)),
        _ => home().map(|h| h.join(".cache")),
    }
}

/// The user's home folder, as Go's os.UserHomeDir.
pub fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from)
}

pub fn default_config_path() -> String {
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => Some(PathBuf::from(x)),
        _ => user_config_dir(),
    };
    match dir {
        Some(d) => d.join("runner-up").join("config.json").to_string_lossy().into_owned(),
        None => "runner-up.json".into(),
    }
}

pub fn default_state_dir() -> String {
    if cfg!(windows) {
        if let Some(dir) = user_cache_dir() {
            return dir.join("runner-up").to_string_lossy().into_owned();
        }
    }
    if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
        return Path::new(&x).join("runner-up").to_string_lossy().into_owned();
    }
    match home() {
        Some(h) => h.join(".local").join("state").join("runner-up").to_string_lossy().into_owned(),
        None => String::new(),
    }
}

/// A full Windows path on a drive, such as C:\runner-up.
fn win_abs(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 4 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' && b[3] != b'\\'
}

pub fn is_abs(p: &str) -> bool {
    Path::new(p).is_absolute()
}

pub fn load_config(path: &str) -> Result<Config, String> {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no setup file at {path}; start one with runner-up init, or name another with --config"));
        }
        Err(e) => return Err(format!("open {path}: {e}")),
    };
    parse_config(&b).map_err(|e| format!("{path}: {e}"))
}

/// Reads a setup from its file's content, fills in the defaults and checks it.
pub fn parse_config(b: &[u8]) -> Result<Config, String> {
    let mut c: Config = serde_json::from_slice(b).map_err(|e| e.to_string())?;
    c.defaults();
    c.validate()?;
    Ok(c)
}

/// Writes c to path for a person to read and edit.
pub fn save_config(path: &str, c: &Config) -> Result<(), String> {
    if let Some(dir) = Path::new(path).parent() {
        create_dir_all_mode(dir, 0o700).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    write_json(Path::new(path), c)
}

pub fn create_dir_all_mode(dir: &Path, mode: u32) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, mode);
    #[cfg(not(unix))]
    let _ = mode;
    b.create(dir)
}

/// Writes v to path whole: a reader sees the old file or the new one. Each
/// write goes through its own temporary file beside path, so two writers of
/// the same path never rename each other's half-written file.
pub fn write_json(path: &Path, v: &impl Serialize) -> Result<(), String> {
    let mut b = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    b.push(b'\n');
    write_whole(path, &b)
}

/// Writes b to path whole, through a temporary file beside it.
pub fn write_whole(path: &Path, b: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (tmp, mut f) = create_temp(dir, &format!("{base}."), ".tmp").map_err(|e| e.to_string())?;
    let mut res = f.write_all(b).map_err(|e| e.to_string());
    drop(f);
    if res.is_ok() {
        res = std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()));
    }
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Creates a new file prefix<random>suffix in dir, as Go's os.CreateTemp.
pub fn create_temp(dir: &Path, prefix: &str, suffix: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    loop {
        let p = dir.join(format!("{prefix}{}{suffix}", random_digits()));
        match opts.open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// A random decimal number for a temporary name.
pub fn random_digits() -> String {
    let mut b = [0u8; 4];
    crate::random::fill(&mut b);
    u32::from_le_bytes(b).to_string()
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, std::io::Error> {
    let b = std::fs::read(path)?;
    serde_json::from_slice(&b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

impl Config {
    pub fn defaults(&mut self) {
        let set = |v: &mut i64, d: i64| {
            if *v == 0 {
                *v = d
            }
        };
        set(&mut self.announce_capacity, 50);
        set(&mut self.start_timeout_seconds, 600);
        set(&mut self.boot_timeout_seconds, 300);
        set(&mut self.guest_check_seconds, 30);
        set(&mut self.max_job_seconds, 6 * 60 * 60);
        set(&mut self.tick_seconds, 10);
        set(&mut self.log_keep_days, 30);
        for m in &mut self.machines {
            for p in &mut m.places {
                p.machine = m.name.clone();
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !is_abs(&self.state_dir) {
            return Err("stateDir must be absolute".into());
        }
        if self.github.repos.len() + self.github.orgs.len() == 0 {
            return Err("github needs at least one repository or organization".into());
        }
        let mut seen = HashSet::new();
        for r in &self.github.repos {
            let ok = match r.split_once('/') {
                Some((owner, name)) => !owner.is_empty() && !name.is_empty() && !name.contains('/'),
                None => false,
            };
            if !ok || !seen.insert(r.as_str()) {
                return Err(format!("repository {} must be owner/name, listed once", q(r)));
            }
        }
        for o in &self.github.orgs {
            if o.is_empty() || o.contains('/') || !seen.insert(o.as_str()) {
                return Err(format!("organization {} must be a plain name, listed once", q(o)));
            }
        }
        if self.pools.is_empty() {
            return Err("at least one pool is needed".into());
        }
        let mut labels = HashSet::new();
        for p in &self.pools {
            check_label(&p.label)?;
            if labels.contains(p.label.as_str()) {
                return Err(format!("pool {} is listed twice", q(&p.label)));
            }
            if p.cores < 0 || p.memory_mb < 0 || (p.cores == 0) != (p.memory_mb == 0) {
                return Err(format!("pool {}: set positive cores and memoryMB together, or leave both out", q(&p.label)));
            }
            labels.insert(p.label.as_str());
        }
        let mut machines = HashSet::new();
        let mut ids = HashSet::new();
        let mut served = HashSet::new();
        for m in &self.machines {
            if m.name.is_empty() || machines.contains(m.name.as_str()) || m.name.contains(['/', ' ']) {
                return Err(format!("machine names must be unique, set and plain: {}", q(&m.name)));
            }
            machines.insert(m.name.as_str());
            if m.cores < 0 || m.memory_mb < 0 || m.room_cores < 0 || m.room_memory_mb < 0 {
                return Err(format!("machine {}: cores, memoryMB and their room must not be negative", q(&m.name)));
            }
            if m.room_cores > 0 && m.room_cores >= m.cores || m.room_memory_mb > 0 && m.room_memory_mb >= m.memory_mb {
                return Err(format!("machine {}: the room kept for its owner must leave its places some cores and memory", q(&m.name)));
            }
            if !m.os.is_empty() && m.os != "windows" {
                return Err(format!("machine {}: os must be windows or left out", q(&m.name)));
            }
            if m.windows() {
                if m.local() {
                    return Err(format!("machine {}: the coordinator runs on Linux or macOS, so a Windows machine needs ssh", q(&m.name)));
                }
                check_win_word(m.agent_path(), true).map_err(|e| format!("machine {}: agent {e}", q(&m.name)))?;
            }
            let b = m.budget();
            for p in &m.places {
                self.validate_place(m, p, b, &labels, &mut ids, &mut served)?;
            }
        }
        for p in &self.pools {
            if !self.machines.is_empty() && !served.contains(p.label.as_str()) {
                return Err(format!("pool {}: no place serves it", q(&p.label)));
            }
        }
        if let Some(r) = &self.release {
            return r.check();
        }
        Ok(())
    }

    fn validate_place<'a>(
        &self,
        m: &Machine,
        p: &'a Place,
        b: Share,
        labels: &HashSet<&str>,
        ids: &mut HashSet<&'a str>,
        served: &mut HashSet<&'a str>,
    ) -> Result<(), String> {
        let id = q(&p.id);
        if p.id.is_empty() || ids.contains(p.id.as_str()) || p.id.contains(['/', ' ', '\t', '\n']) || p.id.len() > MAX_PLACE_ID {
            return Err(format!("place ids must be unique, set, plain and at most {MAX_PLACE_ID} characters: {id}"));
        }
        ids.insert(&p.id);
        if !PROVIDERS.contains(&p.provider.as_str()) {
            return Err(format!("place {id}: provider must be one of {}", go::list(&PROVIDERS)));
        }
        let win = WINDOWS_PROVIDERS.contains(&p.provider.as_str());
        if win != m.windows() {
            if win {
                return Err(format!("place {id}: {} places run only on Windows machines", p.provider));
            }
            return Err(format!("place {id}: {} places do not run on Windows machines; use {}", p.provider, go::list(&WINDOWS_PROVIDERS)));
        }
        if m.windows() {
            // The id names the place's pipe, VM and folders on the machine.
            if !go::only(&p.id, WIN_PLAIN) || p.id.contains(['\\', ':', '=', '+']) {
                return Err(format!("place {id}: a Windows machine's place id has only letters, digits, -, _ and ."));
            }
            for w in [&p.image, &p.switch] {
                check_win_word(w, false).map_err(|e| format!("place {id}: {e}"))?;
            }
        }
        if p.pools.is_empty() {
            return Err(format!("place {id}: it serves no pool"));
        }
        for l in &p.pools {
            if !labels.contains(l.as_str()) {
                return Err(format!("place {id}: unknown pool {}", q(l)));
            }
            served.insert(l);
        }
        if p.cores <= 0 || p.memory_mb <= 0 {
            return Err(format!("place {id}: it needs cores and memoryMB"));
        }
        for label in &p.pools {
            let sized = self.size_place(p, label);
            if b.cores > 0 && sized.cores > b.cores {
                return Err(format!(
                    "place {id}, pool {}: its {} cores never fit its machine's {}, less the owner's room",
                    q(label),
                    sized.cores,
                    b.cores
                ));
            }
            if b.memory_mb > 0 && sized.memory_mb > b.memory_mb {
                return Err(format!(
                    "place {id}, pool {}: its {} MB never fit its machine's {} MB, less the owner's room",
                    q(label),
                    sized.memory_mb,
                    b.memory_mb
                ));
            }
        }
        check_image(&p.provider, &p.image).map_err(|e| format!("place {id}: {e}"))?;
        for (l, image) in &p.pool_images {
            if !p.pools.contains(l) {
                return Err(format!("place {id}: poolImages names pool {}, which the place does not serve", q(l)));
            }
            if image.is_empty() {
                return Err(format!("place {id}: poolImages gives pool {} no image", q(l)));
            }
            let mut res = check_image(&p.provider, image);
            if res.is_ok() && m.windows() {
                res = check_win_word(image, false);
            }
            res.map_err(|e| format!("place {id}: pool {}'s image: {e}", q(l)))?;
        }
        if !p.switch.is_empty() && p.provider != PROVIDER_HYPERV {
            return Err(format!("place {id}: only a Hyper-V place joins a switch"));
        }
        if !p.tool_cache.is_empty() {
            if p.provider != PROVIDER_HOST && p.provider != PROVIDER_TART {
                return Err(format!("place {id}: only a host or tart place sets its tool cache"));
            }
            if !p.tool_cache.starts_with('/') || p.tool_cache.contains(['\n', '\r']) {
                return Err(format!("place {id}: the tool cache must be an absolute path"));
            }
        }
        if p.work_folder.contains(['\n', '\r']) {
            return Err(format!("place {id}: the work folder must be one line"));
        }
        Ok(())
    }

    /// The repositories and organizations, each with one scale set per pool.
    pub fn targets(&self) -> Vec<String> {
        self.github.repos.iter().chain(&self.github.orgs).cloned().collect()
    }

    pub fn labels(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.label.clone()).collect()
    }

    /// The size every job in label reserves, or zero for a legacy pool.
    pub fn pool_size(&self, label: &str) -> Share {
        self.pools.iter().find(|p| p.label == label).map(Pool::size).unwrap_or_default()
    }

    /// Keeps the place's identity and provider but applies the job kind's
    /// size. A pool without a size retains the place's existing reservation.
    pub fn size_place(&self, p: &Place, label: &str) -> Place {
        let mut p = p.clone();
        let sh = self.pool_size(label);
        if sh.cores > 0 {
            p.cores = sh.cores;
            p.memory_mb = sh.memory_mb;
        }
        p
    }

    /// Every machine's places, in order.
    pub fn places(&self) -> Vec<Place> {
        self.machines.iter().flat_map(|m| m.places.iter().cloned()).collect()
    }

    pub fn machine(&self, name: &str) -> Machine {
        self.machines.iter().find(|m| m.name == name).cloned().unwrap_or_else(|| Machine { name: name.into(), ..Default::default() })
    }

    pub fn place(&self, id: &str) -> Option<Place> {
        self.machines.iter().flat_map(|m| &m.places).find(|p| p.id == id).cloned()
    }

    pub fn gh(&self) -> &str {
        if self.github.gh.is_empty() { "gh" } else { &self.github.gh }
    }

    pub fn runner_group(&self) -> &str {
        if self.github.runner_group.is_empty() { "Default" } else { &self.github.runner_group }
    }

    pub fn place_dir(&self, id: &str) -> PathBuf {
        Path::new(&self.state_dir).join("places").join(id)
    }

    pub fn logs_dir(&self) -> PathBuf {
        Path::new(&self.state_dir).join("logs")
    }

    pub fn seconds(&self, n: i64) -> Duration {
        Duration::from_secs(n.max(0) as u64)
    }

    /// Whether any place runs in tart.
    pub fn has_tart(&self) -> bool {
        self.machines.iter().flat_map(|m| &m.places).any(|p| p.provider == PROVIDER_TART)
    }
}

/// Refuses an image a place of provider cannot run its jobs on.
fn check_image(provider: &str, image: &str) -> Result<(), String> {
    let blank = image.is_empty() || image.contains([' ', '\t', '\n']);
    let msg = match provider {
        PROVIDER_KVM if !is_abs(image) => "a KVM place needs the absolute path of its base disk image on the machine",
        PROVIDER_TART if blank => "a tart place needs the tart image it clones, such as ghcr.io/cirruslabs/macos-sequoia-base:latest",
        PROVIDER_HOST if !image.is_empty() => "a host place runs on the machine itself and takes no image",
        PROVIDER_HYPERV if !win_abs(image) => {
            r"a Hyper-V place needs the full path of its base disk image on the machine, such as C:\runner-up\noble.vhdx"
        }
        PROVIDER_CONTAINER if blank => "an Apple container place needs the image its guests start from, such as runner-up-linux:latest",
        PROVIDER_ACCOUNT if !image.is_empty() => "an account place runs on the machine itself and takes no image",
        _ => return Ok(()),
    };
    Err(msg.into())
}

/// Refuses a pool label GitHub's own runners, or every self-hosted runner,
/// would also answer to.
fn check_label(l: &str) -> Result<(), String> {
    if l.is_empty() || l.contains([' ', ',', '\t', '\n']) {
        return Err(format!("pool label {} must be one plain word", q(l)));
    }
    let low = l.to_lowercase();
    if RESERVED_LABELS.contains(&low.as_str()) {
        return Err(format!("pool label {} is one every self-hosted runner has; pick your own", q(l)));
    }
    for p in HOSTED_LABEL_PREFIXES {
        if low.starts_with(p) {
            return Err(format!("pool label {} looks like GitHub's own runners ({p}*); pick another", q(l)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
