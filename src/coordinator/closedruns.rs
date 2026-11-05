//! GitHub may never tell the scale set that a job it assigned is gone: when
//! a run is cancelled while one of its jobs is taken back, or its
//! cancellation is lost with a session, the job stays waiting here for a
//! day, and while it reads as assigned every free place starts a runner for
//! it that waits for nothing. So the coordinator asks GitHub about the run of
//! a waiting job it has heard nothing about for a while, and forgets the job
//! once the run is closed.
//!
//! An assigned job no runner takes is taken back after five minutes, so one
//! quiet for QUIET_ASSIGNED is suspect, and its run is asked about again
//! every RECHECK_ASSIGNED while it stays quiet. A taken-back job may wait
//! quietly for its next assignment for as long as the queue is long, and no
//! runner starts for it meanwhile, so it is asked about only after
//! QUIET_TAKEN_BACK, and again every RECHECK_TAKEN_BACK. However many runs
//! are quiet, at most RUN_READS_PER_PASS are read in one pass and
//! RUN_READS_PER_HOUR in RUN_READS_WINDOW.

use std::collections::BTreeMap;
use std::time::Duration;

use super::Service;
use super::reads::WorkflowRun;
use crate::ctx::Ctx;
use crate::go::time::{Time, td};
use crate::plan::Job;

pub(super) const QUIET_ASSIGNED: Duration = Duration::from_secs(10 * 60);
pub(super) const RECHECK_ASSIGNED: Duration = Duration::from_secs(15 * 60);
pub(super) const QUIET_TAKEN_BACK: Duration = Duration::from_secs(3600);
pub(super) const RECHECK_TAKEN_BACK: Duration = Duration::from_secs(3600);
pub(super) const RUN_READS_PER_PASS: usize = 10;
pub(super) const RUN_READS_PER_HOUR: usize = 60;
pub(super) const RUN_READS_WINDOW: Duration = Duration::from_secs(3600);

/// How long GitHub must have said nothing about a waiting job before its run
/// is asked about, and how often it is asked again; zero for a job that is
/// never asked about: a borrow, or one GitHub offered, which acquiring
/// checks.
fn quiet_for(j: &Job) -> (Duration, Duration) {
    if !j.borrow_id.is_empty() || !j.assigned || j.run_id <= 0 {
        (Duration::ZERO, Duration::ZERO)
    } else if j.taken_back {
        (QUIET_TAKEN_BACK, RECHECK_TAKEN_BACK)
    } else {
        (QUIET_ASSIGNED, RECHECK_ASSIGNED)
    }
}

/// Whether GitHub has said nothing about j since before at less its quiet
/// time.
fn quiet_since(j: &Job, at: Time) -> bool {
    let (quiet, _) = quiet_for(j);
    !quiet.is_zero() && at.sub(j.heard_at) >= td(quiet)
}

impl Service {
    /// Asks GitHub about the runs of waiting assigned jobs it has gone quiet
    /// about, and forgets those jobs of a run GitHub has closed. A job is
    /// never forgotten on a run GitHub calls open, on an answer it could not
    /// give, or once GitHub has sent anything about it since the pass began:
    /// the run may have been run again. A job with a runner is not waiting,
    /// and is never asked about. The pass has five seconds and never holds
    /// the queue lock while it reads.
    pub(super) fn forget_closed_runs(&self, ctx: &Ctx) {
        let (reader, now, reads) = {
            let mut st = self.lock();
            let Some(reader) = st.run_reader.clone() else { return };
            let now = st.now();
            // Each quiet run is asked about again after the shortest recheck
            // of its quiet jobs.
            let mut recheck: BTreeMap<WorkflowRun, Duration> = BTreeMap::new();
            for j in st.waiting.values() {
                if !quiet_since(j, now) {
                    continue;
                }
                let every = quiet_for(j).1;
                let e = recheck.entry(WorkflowRun { repo: j.repo.clone(), id: j.run_id }).or_insert(every);
                *e = (*e).min(every);
            }
            st.runs_checked.retain(|k, _| recheck.contains_key(k));
            st.run_reads.retain(|t| now.sub(*t) < td(RUN_READS_WINDOW));
            let mut reads: Vec<WorkflowRun> = Vec::new();
            for (key, every) in &recheck {
                if st.runs_checked.get(key).is_none_or(|last| now.sub(*last) >= td(*every)) {
                    reads.push(key.clone());
                }
            }
            // The runs asked about longest ago, or never, first; then by run.
            let checked = |k: &WorkflowRun| st.runs_checked.get(k).copied().unwrap_or_default();
            reads.sort_by(|a, b| checked(a).cmp(&checked(b)).then_with(|| a.cmp(b)));
            let n = reads.len().min(RUN_READS_PER_PASS).min(RUN_READS_PER_HOUR.saturating_sub(st.run_reads.len()));
            reads.truncate(n);
            (reader, now, reads)
        };
        let ctx = ctx.with_timeout(Duration::from_secs(5));
        for key in reads {
            if ctx.is_done() {
                break;
            }
            let res = reader(&ctx, &key.repo, key.id);
            let mut st = self.lock();
            st.runs_checked.insert(key.clone(), now);
            st.run_reads.push(now);
            match res {
                Err(e) => st.log.warn(
                    "could not ask GitHub whether a run is closed; its waiting jobs stay",
                    &[("repo", &key.repo), ("run", &key.id), ("err", &e)],
                ),
                Ok(false) => {}
                Ok(true) => {
                    // Quiet since before the question: GitHub has assigned it
                    // again since, by a new attempt of the run, if not.
                    let gone: Vec<String> = st
                        .waiting
                        .iter()
                        .filter(|(_, j)| j.repo == key.repo && j.run_id == key.id && quiet_since(j, now))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in gone {
                        let j = st.waiting.remove(&k).unwrap();
                        st.log.info(
                            "GitHub closed the job's run; forgot the job",
                            &[
                                ("repo", &j.repo),
                                ("pool", &j.pool),
                                ("job", &j.name),
                                ("run", &j.run_id),
                                ("takenBack", &j.taken_back),
                                ("queued", &j.queue_time.rfc3339_secs()),
                            ],
                        );
                    }
                }
            }
        }
        ctx.cancel();
    }
}
