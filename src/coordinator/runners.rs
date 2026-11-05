//! What the coordinator owes the runners it started and their places, beyond
//! what each place's lock and place.json say.

use std::collections::BTreeMap;
use std::time::Duration;

use super::place::{read_place_run, retire_place_run};
use super::snapshot::Snapshot;
use super::{PlaceRun, Service, SetHandle, SetName, State, prune_logs};
use crate::config::Place;
use crate::ctx::Ctx;
use crate::go::time::{round_secs, td};
use crate::lock;
use crate::plan;
use crate::scaleset::{self, Kind};

pub fn unused_key(run: &PlaceRun) -> String {
    format!("{}|{}", run.target, run.runner_id)
}

/// A runner to remove from its scale set, through set.
struct Removal {
    key: String,
    run: PlaceRun,
    set: SetHandle,
}

/// A place's waiting runner to give up on, and why. spare names the scale
/// set whose spare runner it is, if it is one. set is its scale set's client,
/// None when its session is closed.
struct IdleRunner {
    place: String,
    run: PlaceRun,
    why: String,
    spare: Option<SetName>,
    set: Option<SetHandle>,
}

/// What came of giving up on an idle runner: whether GitHub answered, and if
/// so whether the runner was removed (true) or runs a job (false).
#[derive(Default, Clone, Copy)]
struct GiveUpAnswer {
    answered: bool,
    removed: bool,
}

/// A removal found no such runner at GitHub: GitHub named the runner
/// missing, or answered 404 Not Found.
pub fn runner_gone(err: &scaleset::Error) -> bool {
    err.is(Kind::RunnerNotFound) || err.is(Kind::NotFound)
}

impl Service {
    /// Keeps places from taking new jobs, for why, until unhold. Jobs they
    /// run carry on.
    pub fn hold(&self, ids: &[String], why: &str) {
        let mut st = self.lock();
        for id in ids {
            st.held.insert(id.clone(), why.into());
        }
        st.log.info("places take no new jobs for now", &[("places", &ids.join(",")), ("why", &why)]);
    }

    /// Lets places hold held take jobs again.
    pub fn unhold(&self, ids: &[String]) {
        let mut st = self.lock();
        for id in ids {
            st.held.remove(id);
        }
        st.log.info("places take jobs again", &[("places", &ids.join(","))]);
    }

    /// Runs on every pass of the serve loop: forgets the last run of every
    /// free place, removes the registration of every runner that ended
    /// without a job, and gives up on every runner no job reached in time and
    /// every spare runner still to go. It calls GitHub and stops places with
    /// mu let go.
    pub fn tidy(&self, ctx: &Ctx) {
        let _pass = self.pass.lock().unwrap_or_else(|e| e.into_inner());
        let (unused, log) = {
            let mut st = self.lock();
            for p in st.places() {
                if let Ok(false) = lock::held_by(&st.cfg.place_dir(&p.id).join("busy.lock")) {
                    st.retire(p);
                }
            }
            (st.unused_runners(), st.log.clone())
        };
        let removed = remove_unused(ctx, &log, unused);
        {
            let mut st = self.lock();
            for k in &removed {
                st.unused.remove(k);
            }
            if !removed.is_empty() {
                st.save();
            }
        }
        self.give_up_idle(ctx, false);
        let st = self.lock();
        let keep = st.now().sub_dur(Duration::from_secs(st.cfg.log_keep_days.max(0) as u64 * 24 * 3600));
        prune_logs(&st.cfg.logs_dir(), keep);
        prune_logs(&super::usage::usage_dir(&st.cfg), keep);
    }

    /// Gives up on the idle runners (State::idle), calling GitHub and
    /// stopping places with mu let go; spare_only leaves the runners past the
    /// start timeout for tidy. It is called with pass held.
    pub(super) fn give_up_idle(&self, ctx: &Ctx, spare_only: bool) {
        let (idle, log) = {
            let st = self.lock();
            (st.idle(spare_only), st.log.clone())
        };
        let mut answers = Vec::with_capacity(idle.len());
        let mut rest = Vec::with_capacity(idle.len());
        // GitHub can give a scale set's waiting runner a job at any moment
        // until the runner is removed, and a runner stopped first would leave
        // that job to fail when GitHub stops hearing from it. So the runner's
        // registration goes first, and its place is stopped only once GitHub
        // has accepted that. A runner GitHub will not remove because it is
        // running a job keeps running it. A runner whose removal GitHub did
        // not answer, or whose scale set has no live session, keeps its
        // place, to be asked about again at a later tick.
        for r in idle {
            answers.push(self.give_up_one(ctx, &log, &r));
            rest.push((r.run.runner_name, r.spare));
        }
        self.lock().record_give_ups(&rest, &answers);
    }

    fn give_up_one(&self, ctx: &Ctx, log: &crate::go::slog::Logger, r: &IdleRunner) -> GiveUpAnswer {
        if r.run.runner_id > 0 {
            let Some(set) = &r.set else { return GiveUpAnswer::default() };
            match set.remove_runner(ctx, r.run.runner_id) {
                Err(e) if e.is(Kind::JobStillRunning) => {
                    log.info(
                        &format!("{}, but GitHub says it is running a job; keeping it", r.why),
                        &[("place", &r.place), ("runner", &r.run.runner_name)],
                    );
                    return GiveUpAnswer { answered: true, removed: false };
                }
                Err(e) if !runner_gone(&e) => {
                    log.warn(
                        &format!("{}; could not remove it from its scale set, so it keeps its place until GitHub answers", r.why),
                        &[("place", &r.place), ("runner", &r.run.runner_name), ("err", &e)],
                    );
                    return GiveUpAnswer::default();
                }
                _ => {}
            }
        }
        log.info(&format!("{}; removed it from its scale set, stopping it", r.why), &[("place", &r.place), ("runner", &r.run.runner_name)]);
        if let Err(e) = self.launcher.stop(&r.place) {
            log.warn("stop failed", &[("place", &r.place), ("err", &e)]);
            return GiveUpAnswer::default();
        }
        GiveUpAnswer { answered: true, removed: true }
    }
}

/// Removes unused runners from their scale sets, and names those GitHub no
/// longer has.
fn remove_unused(ctx: &Ctx, log: &crate::go::slog::Logger, unused: Vec<Removal>) -> Vec<String> {
    let mut removed = Vec::new();
    for r in unused {
        let res = r.set.remove_runner(ctx, r.run.runner_id);
        drop(r.set);
        match res {
            Err(e) if e.is(Kind::JobStillRunning) => {
                // Its place has ended, yet GitHub gave the runner a job: no
                // runner is left to run it, and GitHub fails it once it stops
                // hearing from the runner.
                log.error(
                    "a runner whose place has ended is running a job at GitHub; the job is stranded",
                    &[("runner", &r.run.runner_name), ("target", &r.run.target), ("pool", &r.run.pool)],
                );
                continue;
            }
            Err(e) if !runner_gone(&e) => {
                log.warn("could not remove an unused runner; will retry", &[("runner", &r.run.runner_name), ("err", &e)]);
                continue;
            }
            _ => {}
        }
        log.info("removed an unused runner", &[("runner", &r.run.runner_name)]);
        removed.push(r.key);
    }
    removed
}

impl State {
    /// The unused runners whose scale set has a live session; once the
    /// others' has, a later tidy removes them.
    fn unused_runners(&self) -> Vec<Removal> {
        let mut out = Vec::new();
        for (k, run) in &self.unused {
            if let Some(set) = self.set(&run.target, &run.pool) {
                out.push(Removal { key: k.clone(), run: run.clone(), set });
            }
        }
        out
    }

    /// The waiting runners to give up on: every runner that has waited the
    /// start timeout with no job, unless spare_only, and the spare runners of
    /// each scale set a job of which was cancelled before a runner took it. A
    /// set has then more runners waiting for a job than jobs on their way,
    /// and its newest waiting runners would otherwise hold their places until
    /// the start timeout. A runner given up on before, which is stopping or
    /// which GitHub says is running a job, is no longer waiting.
    fn idle(&self, spare_only: bool) -> Vec<IdleRunner> {
        let mut out: Vec<IdleRunner> = Vec::new();
        let mut waiting: Vec<(String, PlaceRun)> = Vec::new();
        for p in self.places() {
            let Ok(true) = lock::held_by(&self.cfg.place_dir(&p.id).join("busy.lock")) else { continue };
            let Some(run) = read_place_run(&self.cfg, &p.id) else { continue };
            if !run.borrow_id.is_empty() || run.started {
                continue;
            }
            if !self.given_up.contains_key(&run.runner_name) {
                waiting.push((p.id, run));
            }
        }
        let add = |out: &mut Vec<IdleRunner>, place: &str, run: &PlaceRun, why: &str, spare: Option<SetName>| {
            let set = if run.runner_id > 0 { self.set(&run.target, &run.pool) } else { None };
            out.push(IdleRunner { place: place.into(), run: run.clone(), why: why.into(), spare, set });
        };
        let limit = td(self.cfg.seconds(self.cfg.start_timeout_seconds));
        let now = self.now();
        for (place, run) in &waiting {
            if !spare_only && now.sub(run.launched_at) >= limit {
                add(&mut out, place, run, "no job reached the runner in time", None);
            }
        }
        for set in &self.spare {
            let pending = self.acquired.values().filter(|a| a.job.target == set.target && a.job.pool == set.pool).count();
            let mut idles: Vec<&(String, PlaceRun)> = Vec::new();
            for w in &waiting {
                let picked = out.iter().any(|r| r.run.runner_name == w.1.runner_name);
                if w.1.target == set.target && w.1.pool == set.pool && !picked {
                    idles.push(w);
                }
            }
            idles.sort_by(|a, b| b.1.launched_at.cmp(&a.1.launched_at));
            let n = idles.len().saturating_sub(pending);
            for (place, run) in &idles[..n] {
                add(&mut out, place, run, "the runner's job was cancelled before it started", Some(set.clone()));
            }
        }
        out
    }

    /// Keeps what GitHub answered for each runner given up on. A scale set's
    /// spare runners are all gone once GitHub answered for each; while a
    /// removal it did not answer leaves one, the next tidy asks again.
    fn record_give_ups(&mut self, idle: &[(String, Option<SetName>)], answers: &[GiveUpAnswer]) {
        let mut unanswered = std::collections::BTreeSet::new();
        for ((runner, spare), a) in idle.iter().zip(answers) {
            if a.answered {
                self.given_up.insert(runner.clone(), a.removed);
            } else if let Some(set) = spare {
                unanswered.insert(set.clone());
            }
        }
        self.spare.retain(|set| unanswered.contains(set));
    }

    /// Forgets the last run of a free place, which tidy or the next placement
    /// there may reach first. A runner that ended before its job reached it
    /// is still registered with its scale set, unless the coordinator removed
    /// it there when it gave up on it, so it joins the unused runners, saved
    /// before its place.json goes. The job it was started for waits again, in
    /// line from where it was: GitHub keeps an assigned job assigned to the
    /// scale set and sends nothing more, so a job dropped here would never be
    /// placed again. A run that could not reach its machine's agent rests the
    /// machine. A departed place's run ran as the setup had the place then;
    /// once it is retired the place is forgotten or, where the setup still
    /// has it, takes jobs as the setup has it now.
    pub(super) fn retire(&mut self, mut p: Place) {
        if let Some(d) = self.departed.remove(&p.id) {
            p = d;
            self.save();
        }
        let Some(run) = read_place_run(&self.cfg, &p.id) else { return };
        if run.unreachable && self.cfg.machines.iter().any(|m| m.name == p.machine) {
            self.rest(&p.machine);
        }
        let removed = self.given_up.remove(&run.runner_name).unwrap_or(false);
        if !run.started {
            if run.runner_id > 0 && !removed {
                self.unused.insert(unused_key(&run), run.clone());
            }
            let keys: Vec<String> = self.acquired.iter().filter(|(_, a)| a.started_by(&p.id, &run)).map(|(k, _)| k.clone()).collect();
            for k in keys {
                let a = self.acquired.remove(&k).unwrap();
                self.log.info(
                    "the runner ended before its job reached it; the job waits again",
                    &[("place", &p.id), ("repo", &a.job.repo), ("job", &a.job.name), ("runner", &run.runner_name)],
                );
                self.waiting.insert(k, a.job);
            }
            self.save();
        }
        if let Err(e) = self.record_usage(&p, &run) {
            self.log.warn("could not retain resource readings", &[("place", &p.id), ("err", &e)]);
        }
        if let Err(e) = retire_place_run(&self.cfg, &p.id)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            self.log.warn("could not retire the place's last run", &[("place", &p.id), ("err", &e)]);
        }
    }

    /// Logs any place left free while a job that fits it has waited over a
    /// minute. It should never fire; when it does, the line names the place
    /// and the job.
    pub(super) fn note_waste(&mut self) {
        let now = self.now();
        let old: Vec<plan::Job> = self.waiting_jobs().into_iter().filter(|j| now.sub(j.seen_at) > td(Duration::from_secs(60))).collect();
        let mut v = Snapshot::default();
        let (views, headroom) = (v.views(self), v.headroom(self));
        let mut wasted: BTreeMap<String, crate::go::time::Time> = BTreeMap::new();
        for a in plan::plan(&old, &views, &headroom, &self.rules, now) {
            let fresh = self.wasted.get(&a.place.id).is_some_and(|t| now.sub(*t) <= td(Duration::from_secs(600)));
            if !fresh {
                self.log.warn(
                    "waste: a place is free while a job that fits it waits",
                    &[
                        ("place", &a.place.id),
                        ("repo", &a.job.repo),
                        ("job", &a.job.name),
                        ("waiting", &round_secs(now.sub(a.job.queue_time))),
                    ],
                );
                wasted.insert(a.place.id.clone(), now);
            }
        }
        self.wasted.extend(wasted);
    }
}
