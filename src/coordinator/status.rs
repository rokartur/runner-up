//! `runner-up status`: each place, the drains and the waiting jobs, read
//! from the state folder without a running coordinator.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use super::drain::{DRAIN_DONE, drains};
use super::snapshot::Snapshot;
use super::{PlaceRun, SavedState, State};
use crate::config::{self, Config};
use crate::go::quote;
use crate::go::slog::Logger;

/// Prints each place, the drains and the waiting jobs, for a person or a
/// check.
pub fn status(cfg: Config, w: &mut dyn Write) -> i32 {
    let cfg = Arc::new(cfg);
    let st = State::new(cfg.clone(), Logger::discard(), Vec::new());
    let saved: Option<SavedState> = config::read_json(&std::path::Path::new(&cfg.state_dir).join("state.json")).ok();
    let drains = drains(&cfg);
    let mut drained_for = BTreeMap::new();
    for d in drains.iter().rev() {
        if d.phase != DRAIN_DONE {
            for id in &d.places {
                drained_for.insert(id.clone(), d.label.clone());
            }
        }
    }
    let busy = |p: &config::Place| Snapshot::default().busy(&st, p, true);
    for mut p in cfg.places() {
        let (mut is_busy, mut why) = busy(&p);
        if let Some(first) = p.pools.first() {
            (is_busy, why) = busy(&cfg.size_place(&p, first));
        }
        if is_busy && p.pools.iter().any(|pool| !busy(&cfg.size_place(&p, pool)).0) {
            (is_busy, why) = (false, None);
        }
        let runs = why.as_ref().is_some_and(|w| w.runs());
        let state = if is_busy {
            format!("busy ({})", why.as_ref().map(|w| w.to_string()).unwrap_or_default())
        } else if let Some(since) = saved.as_ref().and_then(|s| s.unreachable.get(&p.machine)) {
            format!("resting (agent unreachable since {})", since.local_rfc3339())
        } else if let Some(label) = drained_for.get(&p.id) {
            format!("drained (for {})", quote(label))
        } else {
            "free".into()
        };
        let mut job = String::new();
        if runs && let Ok(run) = config::read_json::<PlaceRun>(&cfg.place_dir(&p.id).join("place.json")) {
            let started = if run.started { "running" } else { "waiting for its job" };
            job = format!(": {} {} {started}", run.repo, quote(&run.job_name));
            if !run.borrow_id.is_empty() {
                job = format!(": borrow {} since {}", quote(&run.job_name), run.launched_at.local_rfc3339());
            }
        }
        if runs {
            let sh = Snapshot::default().reservation(&st, &p);
            (p.cores, p.memory_mb) = (sh.cores, sh.memory_mb);
        }
        let _ = writeln!(w, "place {} on {} ({}, {} cores, {} MB) {state}{job}", p.id, p.machine, p.provider, p.cores, p.memory_mb);
    }
    for d in &drains {
        let mut jobs: Vec<String> = d.jobs.iter().map(|n| quote(n)).collect();
        if !d.repo.is_empty() {
            jobs[0] = format!("{} {}", d.repo, jobs[0]);
        }
        let mut phase = "waiting for the coordinator".to_string();
        if !d.phase.is_empty() {
            phase = d.phase.clone();
            if !d.detail.is_empty() {
                phase += &format!(": {}", d.detail);
            }
        }
        let _ = writeln!(
            w,
            "drain {} of {} for {} since {}: {phase}",
            quote(&d.label),
            d.places.join(", "),
            jobs.join(", "),
            d.queued.local_rfc3339()
        );
    }
    if let Some(st) = saved {
        for j in &st.waiting {
            if !j.borrow_id.is_empty() {
                let _ = writeln!(w, "waiting borrow {} {} since {}", j.pool, quote(&j.name), j.queue_time.local_rfc3339());
                continue;
            }
            let _ = writeln!(w, "waiting {} {} {} since {}", j.repo, j.pool, quote(&j.name), j.queue_time.local_rfc3339());
        }
        for a in &st.acquired {
            let _ = writeln!(w, "starting {} {} {} in {}", a.job.repo, a.job.pool, quote(&a.job.name), a.place);
        }
    }
    0
}

/// Why place id is busy, as status reads it: None when free.
#[cfg(all(test, unix))]
pub fn held_by(cfg: &Config, id: &str) -> Option<super::BusyReason> {
    let st = State::new(Arc::new(cfg.clone()), Logger::discard(), Vec::new());
    Snapshot::default().busy(&st, &cfg.place(id)?, true).1
}
