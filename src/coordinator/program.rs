// When the setup names a release (config::Release), the coordinator runs that
// version of runner-up. It keeps its programs under its state folder, in
// program/:
//
//	<version>/runner-up  each version it ran or installed
//	current              the version it runs and kept, for a supervisor to start
//	archives/            the release archives it fetched, for the agents
//	trial.json           the version on trial, and the version before it
//	refused.json         the versions it went back from, and why
//
// When the setup names another version, the coordinator fetches its archive
// for this platform, checks its SHA-256 against the setup's, unpacks it, and
// asks the new program its version and to check the setup (check-config). If
// all holds, it stops serving as a stop signal would - running jobs carry
// on - and replaces itself with the new program, in the same process, so
// launchd or systemd keeps supervising it.
//
// The new version is on trial for trial_window, and current points at it
// only once it has served that long. If it stops sooner - it fails, crashes
// or never starts serving - the supervisor starts the program it always
// starts, the version before, which sees the trial left behind, refuses that
// version and serves on. A refused version stays refused until the setup
// names another version or another SHA-256. A stop signal during the trial
// ends it with no refusal: a person or a restart stopped it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent;
use crate::config::{self, Release};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::go::time::{Time, td};
use crate::{proc, release};

/// How long a new version must serve before it is kept.
pub const TRIAL_WINDOW: Duration = Duration::from_secs(60);

/// How long a version's folder outlives its last use: place runners started
/// by an earlier version run as its program until their job ends.
const PROGRAM_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

/// The coordinator's own program and the versions under dir.
pub struct Program {
    pub dir: PathBuf,
    /// The running program's version, and self_path its path.
    pub running: String,
    pub self_path: String,
    /// The setup file a new program must accept before the coordinator
    /// restarts into it.
    pub setup: String,
    pub log: Logger,
    pub trial_window: Duration,
    pub now: Arc<dyn Fn() -> Time + Send + Sync>,
}

/// A program to replace the coordinator with. trial is set when it is a new
/// version on trial, and None when it is the version the coordinator goes
/// back to.
#[derive(Clone, Debug)]
pub struct Restart {
    pub path: PathBuf,
    pub version: String,
    pub trial: Option<Trial>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Trial {
    #[serde(default)]
    pub version: String,
    /// The setup's SHA-256 of the archive it came from.
    #[serde(default)]
    pub sha256: String,
    /// The version it replaced, to go back to.
    #[serde(default)]
    pub from: String,
    /// When it began serving; zero until it does.
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub started: Time,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Refusal {
    #[serde(default)]
    version: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    at: Time,
}

fn refused_error(r: &Refusal) -> String {
    format!(
        "runner-up refused {} at {} because {}; name another version or SHA-256 in the setup to try again",
        r.version,
        r.at.local_rfc3339(),
        r.why
    )
}

impl Program {
    /// The coordinator's program at self_path, running version, with its
    /// versions under state_dir.
    pub fn new(state_dir: &Path, setup: &str, self_path: &str, version: &str, log: Logger) -> Program {
        Program {
            dir: state_dir.join("program"),
            running: version.into(),
            self_path: self_path.into(),
            setup: setup.into(),
            log,
            trial_window: crate::testprog::knob("RUNNER_UP_TEST_TRIAL")
                .and_then(|d| crate::go::duration::parse(&d).ok())
                .unwrap_or(TRIAL_WINDOW),
            now: Arc::new(Time::now),
        }
    }

    fn path(&self, version: &str) -> PathBuf {
        self.dir.join(version).join("runner-up")
    }

    fn trial_path(&self) -> PathBuf {
        self.dir.join("trial.json")
    }

    fn refused_path(&self) -> PathBuf {
        self.dir.join("refused.json")
    }

    /// The link to the version the coordinator runs, for a supervisor to start.
    pub fn current(&self) -> PathBuf {
        self.dir.join("current")
    }

    fn read_trial(&self) -> Option<Trial> {
        config::read_json(&self.trial_path()).ok()
    }

    /// Reads the trial a restart left, as the coordinator starts. A trial of
    /// this version that has not started yet starts now. Any other trial
    /// stopped before it was kept: begin refuses its version and, unless this
    /// is the version before it, returns that version to run instead. None
    /// means serve on.
    pub fn begin(&self) -> Option<Restart> {
        let mut t = self.read_trial()?;
        let window = crate::go::duration::format(self.trial_window);
        if t.version == self.running && t.started.is_zero() {
            t.started = (self.now)();
            if let Err(e) = config::write_json(&self.trial_path(), &t) {
                self.log.error("could not start the trial of the new version", &[("err", &e)]);
            }
            self.log.info(
                "runner-up runs a new version on trial: if it stops within the trial, the version before it runs again",
                &[("version", &t.version), ("before", &t.from), ("trial", &window)],
            );
            return None;
        }
        let mut why = format!("it stopped within {window} of starting");
        if t.started.is_zero() {
            why = "it never started serving".into();
        }
        self.refuse(&t.version, &t.sha256, &why);
        let _ = std::fs::remove_file(self.trial_path());
        self.log.error(
            "runner-up refused a new version and goes back to the version before it",
            &[("version", &t.version), ("why", &why), ("before", &t.from)],
        );
        if let Err(e) = self.point("current", &t.from) {
            self.log.error("could not point current back at the version before", &[("err", &e)]);
        }
        if t.from == self.running {
            return None;
        }
        Some(Restart { path: self.path(&t.from), version: t.from, trial: None })
    }

    /// Ends a trial of the running version once it has served trial_window,
    /// and points current at it.
    pub fn keep(&self) -> bool {
        let Some(t) = self.read_trial() else {
            return false;
        };
        if t.version != self.running || t.started.is_zero() || (self.now)().sub(t.started) < td(self.trial_window) {
            return false;
        }
        if let Err(e) = self.point("current", &self.running) {
            self.log.error("could not point current at the new version; trying again", &[("err", &e)]);
            return false;
        }
        let _ = std::fs::remove_file(self.trial_path());
        self.log.info("runner-up kept the new version after its trial", &[("version", &t.version), ("current", &self.current().display())]);
        self.prune();
        true
    }

    /// Whether the running version is on trial.
    fn on_trial(&self) -> bool {
        self.read_trial().is_some_and(|t| t.version == self.running)
    }

    /// Ends the running version's trial when a stop signal stops the
    /// coordinator: nothing was wrong with it.
    pub fn stopped(&self) {
        if self.read_trial().is_some_and(|t| t.version == self.running && !t.started.is_zero()) {
            let _ = std::fs::remove_file(self.trial_path());
        }
    }

    /// Readies the setup's release rel. When rel is the running version it
    /// makes current that version's program, fetching it if need be, unless
    /// the version is on trial (keep does that), and returns None. Otherwise
    /// it returns the checked new program to restart into, unless it refused
    /// that release before.
    pub fn want(&self, ctx: &Ctx, rel: Option<&Release>) -> Result<Option<Restart>, String> {
        let Some(rel) = rel else {
            return Ok(None);
        };
        let platform = release::this_platform();
        let Some(sum) = rel.sha256.get(&platform) else {
            return Err(format!("the setup's release names no SHA-256 for {platform}, the coordinator's platform"));
        };
        if rel.version == self.running {
            let current = std::fs::read_link(self.current()).is_ok_and(|t| t == Path::new(&self.running).join("runner-up"));
            if current || self.on_trial() {
                return Ok(None);
            }
            self.install(ctx, rel)?;
            self.point("current", &self.running).map_err(|e| e.to_string())?;
            return Ok(None);
        }
        if let Some(r) = self.refused(&rel.version, sum) {
            return Err(refused_error(&r));
        }
        let path = self.install(ctx, rel)?;
        if let Err(e) = self.preflight(ctx, &path, &rel.version) {
            self.refuse(&rel.version, sum, &e);
            return Err(e);
        }
        let trial = Trial { version: rel.version.clone(), sha256: sum.clone(), from: self.running.clone(), started: Time::default() };
        Ok(Some(Restart { path, version: rel.version.clone(), trial: Some(trial) }))
    }

    /// Fetches and checks rel's program for this platform and points current
    /// at it, for a supervisor to start, without restarting anything. It
    /// returns current's path.
    pub fn ready(&self, ctx: &Ctx, rel: &Release) -> Result<PathBuf, String> {
        let sum = rel.sha256.get(&release::this_platform()).cloned().unwrap_or_default();
        if let Some(r) = self.refused(&rel.version, &sum) {
            return Err(refused_error(&r));
        }
        let path = self.install(ctx, rel)?;
        self.preflight(ctx, &path, &rel.version)?;
        self.point("current", &rel.version).map_err(|e| e.to_string())?;
        Ok(self.current())
    }

    /// Fetches rel's program for this platform into its version's folder,
    /// unless it is there already, and returns its path.
    pub fn install(&self, ctx: &Ctx, rel: &Release) -> Result<PathBuf, String> {
        let platform = release::this_platform();
        let sum = rel.sha256.get(&platform).cloned().unwrap_or_default();
        let path = self.path(&rel.version);
        let dir = path.parent().unwrap().to_path_buf();
        if std::fs::read(dir.join("archive.sha256")).is_ok_and(|b| b == sum.as_bytes()) && path.exists() {
            return Ok(path);
        }
        let archive = self.archive(ctx, rel, &platform)?;
        let prog = release::program(&archive, &platform)?;
        let tmp = self.write_new(&prog, &sum).map_err(|e| e.to_string())?;
        let moved = (|| {
            if dir.exists() {
                // A folder of this version from another archive: place runners
                // may still run its program, which stays readable once moved aside.
                let aside = format!("{}.replaced-{}", dir.display(), (self.now)().unix_nanos());
                std::fs::rename(&dir, aside)?;
            }
            std::fs::rename(&tmp, &dir)
        })();
        if let Err(e) = moved {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e.to_string());
        }
        self.log.info("runner-up fetched a release", &[("version", &rel.version), ("platform", &platform), ("path", &path.display())]);
        Ok(path)
    }

    /// Writes prog and its archive's sum into a new hidden folder of dir.
    fn write_new(&self, prog: &[u8], sum: &str) -> std::io::Result<PathBuf> {
        config::create_dir_all_mode(&self.dir, 0o700)?;
        let tmp = self.dir.join(format!(".new-{}", config::random_digits()));
        std::fs::create_dir(&tmp)?;
        let written = (|| {
            write_mode(&tmp.join("runner-up"), prog, 0o755)?;
            write_mode(&tmp.join("archive.sha256"), sum.as_bytes(), 0o600)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(tmp)
    }

    /// rel's archive for platform, checked against the setup's SHA-256. It is
    /// kept under archives/ for the next machine that needs it.
    pub fn archive(&self, _ctx: &Ctx, rel: &Release, platform: &str) -> Result<Vec<u8>, String> {
        let Some(sum) = rel.sha256.get(platform) else {
            return Err(format!("the setup's release names no SHA-256 for {platform}"));
        };
        let name = release::archive_name(&rel.version, platform);
        let dir = self.dir.join("archives");
        let kept = dir.join(&name);
        if let Ok(b) = std::fs::read(&kept)
            && release::sha256(&b) == *sum
        {
            return Ok(b);
        }
        let url = rel.archive_url(&name);
        let b = release::fetch(&url, release::MAX_ARCHIVE).map_err(|e| format!("could not fetch {name}: {e}"))?;
        let got = release::sha256(&b);
        if got != *sum {
            return Err(format!("{name} from {url} has SHA-256 {got}, not the {sum} the setup names"));
        }
        if config::create_dir_all_mode(&dir, 0o700).is_ok() {
            let tmp = dir.join(format!("{name}.tmp"));
            if write_mode(&tmp, &b, 0o600).is_ok() {
                let _ = std::fs::rename(&tmp, &kept);
            }
        }
        // Archives of other versions are no longer needed.
        let prefix = format!("runner-up-{}-", rel.version);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(b)
    }

    /// Asks the program at path its version, which must be version, and to
    /// check the setup file.
    fn preflight(&self, ctx: &Ctx, path: &Path, version: &str) -> Result<(), String> {
        let c = ctx.with_timeout(Duration::from_secs(60));
        let (out, res) = proc::combined_output(&c, Command::new(path).arg("version"));
        let out = String::from_utf8_lossy(&out);
        if let Err(e) = res {
            return Err(format!("runner-up {version} does not run here: {}", agent::first_line(&out, &e)));
        }
        let got = out.trim();
        if got != version {
            return Err(format!("the program in runner-up {version}'s archive says it is {}", crate::go::quote(got)));
        }
        let (out, res) = proc::combined_output(&c, Command::new(path).args(["check-config", "--config", &self.setup]));
        if let Err(e) = res {
            return Err(format!(
                "runner-up {version} does not accept the setup: {}",
                agent::first_line(&String::from_utf8_lossy(&out), &e)
            ));
        }
        Ok(())
    }

    /// Replaces the coordinator with r's program, serving the setup file. For
    /// a new version it first keeps the running program as the version to go
    /// back to and starts the trial. It returns only when the program could
    /// not be run.
    pub fn exec(&self, r: &Restart) -> String {
        if let Some(t) = &r.trial {
            if let Err(e) = self.keep_running() {
                return format!("could not keep the running program to go back to: {e}");
            }
            if let Err(e) = config::write_json(&self.trial_path(), t) {
                return e;
            }
        }
        let path = r.path.to_string_lossy().into_owned();
        let env: Vec<(String, String)> = std::env::vars().collect();
        let err = proc::exec(&path, &[path.clone(), "serve".into(), "--config".into(), self.setup.clone()], &env).to_string();
        if let Some(t) = &r.trial {
            let _ = std::fs::remove_file(self.trial_path());
            self.refuse(&r.version, &t.sha256, &format!("it could not be run: {err}"));
        }
        err
    }

    /// Copies the running program into its version's folder, unless it runs
    /// from there, so the coordinator can go back to it.
    fn keep_running(&self) -> std::io::Result<()> {
        let path = self.path(&self.running);
        if path.exists() {
            return Ok(());
        }
        config::create_dir_all_mode(path.parent().unwrap(), 0o700)?;
        let tmp = path.with_extension("tmp");
        write_mode(&tmp, &std::fs::read(&self.self_path)?, 0o755)?;
        std::fs::rename(&tmp, &path)
    }

    /// Makes the link name point at version's program.
    fn point(&self, name: &str, version: &str) -> std::io::Result<()> {
        let link = self.dir.join(name);
        config::create_dir_all_mode(&self.dir, 0o700)?;
        let tmp = self.dir.join(format!("{name}.new"));
        let _ = std::fs::remove_file(&tmp);
        symlink(&Path::new(version).join("runner-up"), &tmp)?;
        std::fs::rename(&tmp, &link)
    }

    fn refusals(&self) -> Vec<Refusal> {
        config::read_json(&self.refused_path()).unwrap_or_default()
    }

    fn refused(&self, version: &str, sum: &str) -> Option<Refusal> {
        self.refusals().into_iter().find(|r| r.version == version && r.sha256 == sum)
    }

    fn refuse(&self, version: &str, sum: &str, why: &str) {
        let mut rs = self.refusals();
        rs.retain(|r| !(r.version == version && r.sha256 == sum));
        rs.push(Refusal { version: version.into(), sha256: sum.into(), why: why.into(), at: (self.now)() });
        let saved = config::create_dir_all_mode(&self.dir, 0o700)
            .map_err(|e| e.to_string())
            .and_then(|_| config::write_json(&self.refused_path(), &rs));
        if let Err(e) = saved {
            self.log.error("could not record the refused version; it may be tried again", &[("version", &version), ("err", &e)]);
        }
    }

    /// Removes the folders of versions neither running nor current that have
    /// not changed for PROGRAM_KEEP.
    fn prune(&self) {
        let mut keep = vec![self.running.clone()];
        if let Ok(target) = std::fs::read_link(self.current())
            && let Some(dir) = target.parent()
        {
            keep.push(dir.to_string_lossy().into_owned());
        }
        let now = (self.now)();
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(info) = e.metadata() else { continue };
            if !info.is_dir() || keep.contains(&name) || name == "archives" || name.starts_with('.') {
                continue;
            }
            let modified = info.modified().map(Time::from_system).unwrap_or(now);
            if now.sub(modified) < td(PROGRAM_KEEP) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(e.path())
                && e.kind() != std::io::ErrorKind::NotFound
            {
                self.log.warn("could not remove an old version's program", &[("version", &name), ("err", &e)]);
            }
        }
    }
}

fn write_mode(path: &Path, b: &[u8], mode: u32) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, mode);
    #[cfg(windows)]
    let _ = mode;
    std::io::Write::write_all(&mut o.open(path)?, b)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
