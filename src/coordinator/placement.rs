use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use super::place::write_place_run;
use super::queue::Acquired;
use super::snapshot::{Snapshot, share};
use super::{Guard, PlaceRun, Service, SetHandle, set_key};
use crate::config::{Config, Place};
use crate::ctx::Ctx;
use crate::go::time::{Time, round_secs};
use crate::lock;
use crate::plan::{self, Assignment, Job};

/// What came of start when it failed.
enum StartError {
    /// GitHub did not answer the acquire.
    Acquire(String),
    /// The job is no longer available.
    Gone,
    Other(String),
}

impl Service {
    /// Gives every free place to the job the rules pick, until nothing more
    /// fits. It starts a runner for a job only once its place is chosen, and
    /// acquires a job GitHub offered only then. The first-ranked job waiting
    /// only for resources keeps room in the plan, without acquiring a runner
    /// or lock. It plans under mu, and acquires the job, registers its runner
    /// and launches it with mu let go (assign). ctx is the serving context:
    /// placement carries on while one scale set's session restarts.
    pub fn place(self: &Arc<Self>, ctx: &Ctx) {
        let _pass = self.pass.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh_labels(ctx);
        self.refresh_run_workflows(ctx);
        self.forget_closed_runs(ctx);
        self.refresh_vms(ctx);
        let mut st = self.lock();
        self.ask_resting(ctx, &mut st);
        st.expire();
        st.refresh_drains();
        st.refresh_borrows();
        for _ in 0..=st.cfg.places().len() {
            let cfg = st.cfg.clone();
            let mut v = Snapshot::default();
            let (views, headroom) = (v.views(&st), v.headroom(&st));
            let assignments = plan::plan(&st.waiting_jobs(), &views, &headroom, &st.rules, st.now());
            if assignments.is_empty() {
                break;
            }
            let mut progressed = false;
            for a in assignments {
                // A setup change applied while a runner started makes the
                // rest of this plan stale: plan again.
                if !Arc::ptr_eq(&st.cfg, &cfg) {
                    progressed = true;
                    break;
                }
                let assigned;
                (st, assigned) = self.assign(ctx, st, &mut v, a);
                progressed |= assigned;
            }
            if !progressed {
                break;
            }
        }
        st.advance_drains();
        st.note_waste();
        st.save();
    }

    /// Gives place a.place to job a.job, and reports whether that changed
    /// anything worth planning again for. It is called with mu held, and
    /// takes the place's lock and the job out of line before it lets go of
    /// mu to acquire the job, register the job's runner and launch it, so
    /// nothing done meanwhile places the job or the place twice. It takes mu
    /// again to record what came of it.
    fn assign<'a>(&'a self, ctx: &Ctx, mut st: Guard<'a>, v: &mut Snapshot, a: Assignment) -> (Guard<'a>, bool) {
        let p = a.place;
        let key = a.job.key();
        // The job may have started, ended or been taken back while the
        // runner before it in the plan started.
        let Some(j) = st.waiting.get(&key).cloned() else { return (st, false) };
        if j.taken_back && j.borrow_id.is_empty() {
            return (st, false);
        }
        if !st.sets.contains_key(&set_key(&j.target, &j.pool)) && j.borrow_id.is_empty() {
            return (st, false);
        }
        // A place given in the same plan as another may have to wait for it now.
        if v.busy(&st, &p, true).0 {
            return (st, false);
        }
        let Ok(Some(busy_lock)) = lock::busy(&st.cfg.place_dir(&p.id).join("busy.lock")) else { return (st, false) };
        v.forget(&p.id);
        // This place's last run may have ended since the last tidy, and its
        // place.json is about to be replaced. That run may have found the
        // machine unreachable.
        st.retire(p.clone());
        if st.resting(&p.machine) {
            return (st, true);
        }
        if !j.borrow_id.is_empty() {
            let done = self.assign_borrow(&mut st, &p, &a.job, &busy_lock);
            return (st, done);
        }
        let Some(set) = st.set(&j.target, &j.pool) else { return (st, false) };
        let (now, cfg) = (st.now(), st.cfg.clone());
        st.waiting.remove(&key);
        st.acquired.insert(key.clone(), Acquired { job: j.clone(), place: p.id.clone(), at: now });
        let ours = |st: &Guard| st.acquired.get(&key).is_some_and(|a| a.place == p.id && a.at == now);

        drop(st);
        let res = self.start(ctx, &cfg, &set, &p, &j, now, &busy_lock);
        drop(set);
        let mut st = self.lock();

        let name = match res {
            Err(StartError::Acquire(e)) => {
                // GitHub did not answer: the job waits on, in line from where it was.
                if ours(&st) {
                    let a = st.acquired.remove(&key).unwrap();
                    st.waiting.insert(key, a.job);
                }
                st.log.error(
                    "acquire failed",
                    &[("repo", &j.repo), ("job", &j.name), ("err", &format!("could not acquire the job: {e}"))],
                );
                return (st, false);
            }
            Err(StartError::Gone) => {
                // Taken, cancelled or expired at GitHub while it waited here.
                if ours(&st) {
                    st.acquired.remove(&key);
                }
                st.log.info("job no longer available", &[("repo", &j.repo), ("job", &j.name), ("request", &j.request_id)]);
                return (st, true);
            }
            Err(StartError::Other(e)) => {
                // GitHub gives back an acquired job no runner took.
                st.log.error(
                    "could not start a runner; the place rests a minute",
                    &[("place", &p.id), ("repo", &j.repo), ("job", &j.name), ("err", &e)],
                );
                st.cooling.insert(p.id.clone(), now.add(Duration::from_secs(60)));
                return (st, true);
            }
            Ok(name) => name,
        };
        st.log.info(
            "placed",
            &[
                ("place", &p.id),
                ("machine", &p.machine),
                ("repo", &j.repo),
                ("pool", &j.pool),
                ("job", &j.name),
                ("request", &j.request_id),
                ("runner", &name),
                ("waited", &round_secs(now.sub(j.queue_time))),
            ],
        );
        st.wasted.remove(&p.id);
        (st, true)
    }

    /// Acquires job j for place p, unless GitHub assigned it to the scale set
    /// already, registers a runner for it and launches the runner with the
    /// place's lock. It is called with mu let go, and names the runner.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &self,
        ctx: &Ctx,
        cfg: &Config,
        set: &SetHandle,
        p: &Place,
        j: &Job,
        at: Time,
        busy_lock: &File,
    ) -> Result<String, StartError> {
        if !j.assigned {
            let got = set.acquire_jobs(ctx, &[j.request_id]).map_err(|e| StartError::Acquire(e.to_string()))?;
            if !got.contains(&j.request_id) {
                return Err(StartError::Gone);
            }
        }
        let name = format!("runner-up-{}-{}", p.id, at.unix());
        let fail = |e: String| StartError::Other(e);
        let (jit, runner_id) = set.generate_jit(ctx, &name, p.runner_work_folder()).map_err(|e| fail(e.to_string()))?;
        let _ = std::fs::remove_file(cfg.place_dir(&p.id).join("usage.json"));
        let run = PlaceRun {
            runner_name: name.clone(),
            runner_id,
            target: j.target.clone(),
            pool: j.pool.clone(),
            repo: j.repo.clone(),
            request_id: j.request_id,
            job_name: j.name.clone(),
            launched_at: at,
            reservation: share(p),
            ..Default::default()
        };
        write_place_run(cfg, &p.id, &run).map_err(fail)?;
        self.launcher.launch(p, &j.pool, busy_lock, &jit).map_err(fail)?;
        Ok(name)
    }
}
