use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::PlaceRun;
use super::State;
use super::snapshot::share;
use crate::agent::Usage;
use crate::config::{self, Config, Place, Share};
use crate::go::time::Time;

/// Retains the reservation and observation of one finished runner. A
/// missing reading is explicit, never an observation of zero. GitHub may
/// give a runner a different job of its pool; pool is always authoritative.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub pool: String,
    pub machine: String,
    pub provider: String,
    pub place: String,
    pub runner: String,
    pub started_at: Time,
    pub finished_at: Time,
    pub reservation: Share,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

pub fn usage_dir(cfg: &Config) -> PathBuf {
    PathBuf::from(&cfg.state_dir).join("usage")
}

impl State {
    pub(super) fn record_usage(&self, p: &Place, run: &PlaceRun) -> Result<(), String> {
        if !run.started || !run.borrow_id.is_empty() {
            return Ok(());
        }
        let mut r = UsageRecord {
            pool: run.pool.clone(),
            machine: p.machine.clone(),
            provider: p.provider.clone(),
            place: p.id.clone(),
            runner: run.runner_name.clone(),
            started_at: run.launched_at,
            finished_at: self.now(),
            reservation: run.reservation,
            usage: None,
        };
        if r.reservation.cores == 0 {
            r.reservation = share(p);
        }
        r.usage = config::read_json(&self.cfg.place_dir(&p.id).join("usage.json")).ok();
        let dir = usage_dir(&self.cfg);
        config::create_dir_all_mode(&dir, 0o700).map_err(|e| e.to_string())?;
        // Replaying retirement after a crash rewrites the same record, not a
        // second sample. No job payloads, environment or registrations are
        // stored.
        let nanos = run.launched_at.0.timestamp_nanos_opt().unwrap_or(i64::MIN);
        config::write_json(&dir.join(format!("{}-{nanos}.json", p.id)), &r)
    }
}

pub fn read_usage(cfg: &Config) -> Result<Vec<UsageRecord>, String> {
    let dir = usage_dir(cfg);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e.map_err(|e| e.to_string())?;
        let path = e.path();
        if path.is_dir() || path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let r: UsageRecord = config::read_json(&path).map_err(|e| e.to_string())?;
        out.push(r);
    }
    out.sort_by_key(|r| r.started_at);
    Ok(out)
}
