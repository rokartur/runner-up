//! The jobs the coordinator knows of: waiting for a place, or acquired for a
//! runner that has not yet taken one; and how long each kind of job took.

use serde::{Deserialize, Serialize};

use super::{PlaceRun, State, set_key};
use super::place::read_place_run;
use crate::go::time::{Time, td};
use crate::plan::{self, Job};
use crate::scaleset::JobMessage;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Acquired {
    pub job: Job,
    pub place: String,
    pub at: Time,
}

impl Acquired {
    /// Whether run is the one the coordinator started in place for this job,
    /// which a placement pass launches at the moment it acquires the job.
    pub fn started_by(&self, place: &str, run: &PlaceRun) -> bool {
        self.place == place && self.at == run.launched_at
    }
}

/// Names the job a message is about, both ways.
pub fn keys_of(target: &str, pool: &str, m: &JobMessage) -> Vec<String> {
    let mut keys = vec![plan::run_key(target, pool, m.workflow_run_id, &m.job_display_name)];
    if m.runner_request_id != 0 {
        keys.push(plan::request_key(target, pool, m.runner_request_id));
    }
    keys
}

/// The repository a job message belongs to: its own owner and name, or the
/// target's for a repository scale set that left them out.
pub fn repo_of(target: &str, m: &JobMessage) -> String {
    if !m.owner_name.is_empty() && !m.repository_name.is_empty() {
        return format!("{}/{}", m.owner_name, m.repository_name);
    }
    target.to_string()
}

/// The job a message of target's scale set for pool is about, as seen at now.
pub fn job_of(target: &str, pool: &str, m: &JobMessage, assigned: bool, now: Time) -> Job {
    Job {
        target: target.into(),
        pool: pool.into(),
        repo: repo_of(target, m),
        request_id: m.runner_request_id,
        queue_time: m.queue_time,
        name: m.job_display_name.clone(),
        workflow: m.job_workflow_ref.clone(),
        run_id: m.workflow_run_id,
        seen_at: now,
        heard_at: now,
        assigned,
        job_id: m.job_id.clone(),
        ..Default::default()
    }
}

impl State {
    /// Records a job GitHub offered or assigned. Offered or assigned again, it
    /// keeps its place in line.
    pub(super) fn wait(&mut self, mut j: Job) {
        let k = j.key();
        if j.queue_time.is_zero() {
            j.queue_time = j.seen_at;
        }
        if let Some(old) = self.waiting.get(&k) {
            j.seen_at = old.seen_at;
            j.queue_time = old.queue_time;
        }
        if let Some(a) = self.acquired.get(&k).cloned() {
            // Offered or assigned again after its runner was started. While
            // that runner is alive this is a redelivery, or GitHub assigning
            // again a job it took back before the runner was up; the runner
            // takes it, and a second runner would wait for nothing. Once that
            // runner is gone, GitHub has given the job back.
            if self.runner_alive(&a.place) {
                let a = self.acquired.get_mut(&k).unwrap();
                a.job.taken_back = false;
                a.job.job_id = j.job_id;
                a.job.heard_at = j.heard_at;
                return;
            }
            self.acquired.remove(&k);
            j.seen_at = a.job.seen_at;
            j.queue_time = a.job.queue_time;
        }
        self.log.info(
            "job waiting",
            &[
                ("repo", &j.repo),
                ("pool", &j.pool),
                ("job", &j.name),
                ("request", &j.request_id),
                ("assigned", &j.assigned),
                ("queued", &j.queue_time.rfc3339_secs()),
            ],
        );
        self.waiting.insert(k, j);
    }

    /// Marks an assigned job whose assignment GitHub took back, and reports
    /// whether the message was a take-back. The job keeps its place in line,
    /// and its starting runner, until GitHub assigns it again. GitHub can
    /// send the take-back of an assignment in the same message as the next
    /// assignment, whose job id is new, and a message's assignments are read
    /// before its completions: a take-back of an assignment other than the
    /// job's latest changes nothing.
    pub(super) fn take_back(&mut self, k: &str, job_id: &str, now: Time) -> bool {
        let latest = |j: &Job| j.job_id.is_empty() || j.job_id == job_id;
        if let Some(j) = self.waiting.get_mut(k)
            && j.assigned
        {
            if latest(j) {
                j.taken_back = true;
            }
            j.heard_at = now;
            return true;
        }
        if let Some(a) = self.acquired.get_mut(k)
            && a.job.assigned
        {
            if latest(&a.job) {
                a.job.taken_back = true;
            }
            a.job.heard_at = now;
            return true;
        }
        false
    }

    /// Finds the job a runner was started for when GitHub gave that runner
    /// another job of the same scale set: GitHub gives a set's runner
    /// whichever of the set's jobs it picks, and the job the runner was
    /// started for still needs one. When the job the runner took had a runner
    /// of its own still waiting, that runner is free now and the job handed
    /// back is its own, so no further runner starts only to wait out the
    /// start timeout with nothing to run. Otherwise the job waits again.
    pub(super) fn hand_back(&mut self, target: &str, pool: &str, runner: &str, started: &[String]) {
        let mut freed: Option<Acquired> = None;
        for k in started {
            if let Some(a) = self.acquired.get(k)
                && let Some(run) = read_place_run(&self.cfg, &a.place)
                && run.runner_name != runner
                && a.started_by(&a.place, &run)
                && !run.started
                && self.runner_alive(&a.place)
            {
                freed = Some(a.clone());
            }
        }
        let keys: Vec<String> = self.acquired.keys().cloned().collect();
        for k in keys {
            let a = self.acquired[&k].clone();
            if a.job.target != target || a.job.pool != pool || started.contains(&k) {
                continue;
            }
            if read_place_run(&self.cfg, &a.place).is_none_or(|run| run.runner_name != runner) {
                continue;
            }
            if let Some(f) = freed.take() {
                self.log.info(
                    "the runner took another job of its scale set; its own job goes to the runner started for that one",
                    &[("place", &a.place), ("job", &a.job.name), ("to", &f.place)],
                );
                self.acquired.insert(k, Acquired { job: a.job, place: f.place, at: f.at });
                continue;
            }
            self.acquired.remove(&k);
            self.log.info(
                "the runner took another job of its scale set; its own job waits again",
                &[("place", &a.place), ("job", &a.job.name)],
            );
            self.waiting.insert(k, a.job);
        }
    }

    /// The jobs a place could take now, with what the rules read filled in.
    /// A job whose scale set has no live session cannot be acquired now, and
    /// a job GitHub took back is assigned to nothing until it is assigned
    /// again. A job a drain names may take only that drain's places.
    pub(super) fn waiting_jobs(&self) -> Vec<Job> {
        let mut out = Vec::with_capacity(self.waiting.len());
        for j in self.waiting.values() {
            let live = self.sets.contains_key(&set_key(&j.target, &j.pool));
            if j.borrow_id.is_empty() && (!live || j.taken_back) {
                continue;
            }
            let mut j = j.clone();
            j.size = self.cfg.pool_size(&j.pool);
            if j.borrow_id.is_empty() {
                j.estimate = self.timings.get(&j.timing_key()).copied().unwrap_or_default();
            }
            j.run_workflow = self
                .run_workflows
                .get(&super::WorkflowRun { repo: j.repo.clone(), id: j.run_id })
                .map(|w| w.path.clone())
                .unwrap_or_default();
            let r = j.label_ref();
            if !r.is_empty() {
                j.labels = self.labels.get(&j.repo).and_then(|l| l.get(&r)).cloned().unwrap_or_default();
            }
            j.drain = self.drain_for(&j);
            out.push(j);
        }
        out
    }

    /// Forgets jobs GitHub has certainly given up on: a job that waited over
    /// a day, and an acquired job no runner took within the start and boot
    /// allowance. A job whose own runner has not started stays acquired
    /// while that runner lives, and once it has ended waits again, in line
    /// from where it was, as retire does.
    pub(super) fn expire(&mut self) {
        let now = self.now();
        self.waiting.retain(|_, j| now.sub(j.queue_time) <= td(std::time::Duration::from_secs(25 * 3600)));
        let limit = td(self.cfg.seconds(self.cfg.start_timeout_seconds + self.cfg.boot_timeout_seconds));
        let keys: Vec<String> = self.acquired.keys().cloned().collect();
        for k in keys {
            let a = self.acquired[&k].clone();
            if now.sub(a.at) <= limit {
                continue;
            }
            let own = read_place_run(&self.cfg, &a.place).is_some_and(|run| a.started_by(&a.place, &run) && !run.started);
            if own && self.runner_alive(&a.place) {
                continue;
            }
            self.acquired.remove(&k);
            if own {
                self.waiting.insert(k, a.job);
            }
        }
    }
}
