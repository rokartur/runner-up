//! A running coordinator applies a changed setup file in place. It reads the
//! file every SETUP_POLL, and once the file has held the same content for
//! SETUP_QUIET after a change, so a pull or an editor that rewrites the file
//! in steps is read whole, it checks the content as check-config does and
//! applies it (Service::reload, Sessions::match_setup). A file it cannot
//! read, or a setup that fails the check, leaves the running setup as it is.
//!
//! The coordinator's place runners never read the setup file itself, which
//! may hold a setup the coordinator refused or has not applied yet. Each
//! reads the setup the coordinator runs, which the coordinator keeps under
//! its state folder (write_setup).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::config::{self, Config};
use crate::ctx::Ctx;
use crate::plan::{self, Rule};

pub const SETUP_POLL: Duration = Duration::from_millis(250);
pub const SETUP_QUIET: Duration = Duration::from_secs(1);

/// Reads a setup file's content and checks it as check-config does.
pub fn check_setup(b: &[u8]) -> Result<(Config, Vec<Rule>), String> {
    let cfg = config::parse_config(b)?;
    let rules = plan::parse_order(&cfg)?;
    Ok((cfg, rules))
}

/// The setup file's content, or why it could not be read.
pub type SetupRead = Result<Vec<u8>, String>;

/// Reads the file at path every poll and hands send what it holds once it
/// has held that for quiet after a change, until ctx ends or send says stop.
/// Its first reading counts as a change, so a file changed while the
/// coordinator started is not missed; the receiver compares what it is sent
/// with the setup it runs.
pub fn watch_setup(ctx: &Ctx, path: &Path, poll: Duration, quiet: Duration, send: impl Fn(SetupRead) -> bool + Send + 'static) {
    let path = path.to_path_buf();
    let read = move || std::fs::read(&path).map_err(|e| e.to_string());
    let mut w = SetupWatch::new(quiet, Instant::now());
    w.next(read(), Instant::now());
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        while !ctx.sleep(poll) {
            if let Some(r) = w.next(read(), Instant::now())
                && !send(r)
            {
                return;
            }
        }
    });
}

/// watch_setup's memory of the setup file between readings.
pub(super) struct SetupWatch {
    quiet: Duration,
    /// The last reading, once some reading was taken.
    seen: Option<SetupRead>,
    changed: Instant,
    /// seen was sent.
    sent: bool,
}

impl SetupWatch {
    pub(super) fn new(quiet: Duration, now: Instant) -> SetupWatch {
        SetupWatch { quiet, seen: None, changed: now, sent: false }
    }

    /// Takes a reading of the file at now, and says whether a reading is to
    /// be sent: the file's content once it has held it for quiet since it
    /// last changed, once.
    pub(super) fn next(&mut self, r: SetupRead, now: Instant) -> Option<SetupRead> {
        if self.seen.as_ref() != Some(&r) {
            self.seen = Some(r);
            self.changed = now;
            self.sent = false;
        }
        if self.sent || now.duration_since(self.changed) < self.quiet {
            return None;
        }
        self.sent = true;
        self.seen.clone()
    }
}

/// How long a setup the coordinator no longer runs is kept for place runners
/// started just before the change, which read it as they start.
const SETUP_KEEP: Duration = Duration::from_secs(24 * 3600);

/// Keeps cfg under stateDir for place runners to read, and returns its path.
/// The file is named for its content and never rewritten, so a place runner
/// always reads the setup it was started with, even when the setup changes
/// as it starts.
pub fn write_setup(cfg: &Config) -> Result<String, String> {
    let mut b = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
    b.push(b'\n');
    let sum = Sha256::digest(&b);
    let dir = PathBuf::from(&cfg.state_dir).join("setups");
    config::create_dir_all_mode(&dir, 0o700).map_err(|e| e.to_string())?;
    let hex: String = sum[..8].iter().map(|x| format!("{x:02x}")).collect();
    let path = dir.join(format!("{hex}.json"));
    if std::fs::metadata(&path).is_err() {
        let tmp = dir.join(format!("{hex}.json.tmp"));
        write_private(&tmp, &b).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    }
    // A setup run again is kept as long as one run for the first time.
    let now = SystemTime::now();
    if let Ok(f) = std::fs::File::options().write(true).open(&path) {
        let _ = f.set_modified(now);
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > SETUP_KEEP);
            if p != path && old {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

fn write_private(path: &Path, b: &[u8]) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(&mut o.open(path)?, b)
}
