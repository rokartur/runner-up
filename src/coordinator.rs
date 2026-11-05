//! Holds the scale set sessions, picks which waiting job each free place
//! takes and starts its runner through the place's agent.

pub(crate) mod borrow;
mod closedruns;
mod drain;
mod github;
mod github_api;
mod place;
mod placement;
mod program;
mod queue;
mod reach;
mod reads;
mod reload;
mod request;
mod runners;
mod serve;
mod setup;
mod snapshot;
mod status;
mod update;
mod usage;
mod vmlimit;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{self, Config, Place};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::go::time::{Time, round_secs};
use crate::plan::{self, Job, Rule};
use crate::scaleset;

pub use borrow::{borrow_main, run_borrow};
pub use drain::drain_main;
#[cfg(all(test, unix))]
pub use drain::{DRAIN_DONE, DRAIN_DRAINING, DRAIN_RUNNING, drains};
pub use github::{ClientFactory, gh_clients};
pub use github_api::{GithubApi, gh_token};
#[cfg(test)]
pub use place::run_args;
pub use place::{Launcher, PlaceRun, agent_command, run_place, ssh_args};
pub use program::{Program, Restart};
pub use reach::reach_agent;
pub use serve::{ServeOptions, serve};
pub use setup::SetupRead;
#[cfg(test)]
pub use snapshot::BusyReason;
#[cfg(all(test, unix))]
pub use status::held_by;
pub use status::status;
pub use usage::read_usage;

use queue::Acquired;
use reads::{RunWorkflow, WorkflowRun};

/// What the coordinator needs from one runner scale set.
pub trait SetClient: Send + Sync {
    fn acquire_jobs(&self, ctx: &Ctx, request_ids: &[i64]) -> Result<Vec<i64>, scaleset::Error>;
    /// The runner's one-time registration and its runner id.
    fn generate_jit(&self, ctx: &Ctx, runner_name: &str, work_folder: &str) -> Result<(String, i64), scaleset::Error>;
    fn remove_runner(&self, ctx: &Ctx, runner_id: i64) -> Result<(), scaleset::Error>;
}

/// What wakes the serve loop.
pub enum Event {
    /// A message or a newly opened session may have given a place work.
    Wake,
    /// A place's runner or borrow supervisor ended.
    Exited(String),
    /// The setup file holds this, after a change (setup.rs).
    Setup(SetupRead),
    /// A machine's agent changed (update.rs).
    Changed,
    /// A new version of the coordinator is ready to run (program.rs).
    Ready(Restart),
}

/// Counts calls in flight, as Go's sync.WaitGroup.
#[derive(Clone, Default)]
pub struct WaitGroup(Arc<(Mutex<usize>, Condvar)>);

impl WaitGroup {
    /// Counts one more call; dropping the guard counts it done.
    pub fn add(&self) -> Call {
        *self.0.0.lock().unwrap() += 1;
        Call(self.clone())
    }

    pub fn wait(&self) {
        let mut n = self.0.0.lock().unwrap();
        while *n > 0 {
            n = self.0.1.wait(n).unwrap();
        }
    }

    /// Waits at most d, and says whether every call was done.
    pub fn wait_timeout(&self, d: Duration) -> bool {
        let n = self.0.0.lock().unwrap();
        let (n, _) = self.0.1.wait_timeout_while(n, d, |n| *n > 0).unwrap();
        *n == 0
    }
}

pub struct Call(WaitGroup);

impl Drop for Call {
    fn drop(&mut self) {
        let mut n = self.0.0.0.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.0.0.1.notify_all();
        }
    }
}

/// A scale set's client while its session is open. calls counts the calls to
/// GitHub a pass makes through it after letting go of mu: its session closes
/// only once they are done (remove_set).
struct LiveSet {
    client: Arc<dyn SetClient>,
    calls: WaitGroup,
}

/// A scale set's client handed out for calls made with mu let go.
pub struct SetHandle {
    client: Arc<dyn SetClient>,
    _call: Call,
}

impl std::ops::Deref for SetHandle {
    type Target = dyn SetClient;
    fn deref(&self) -> &Self::Target {
        &*self.client
    }
}

/// One runner scale set: a target's, for one pool.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SetName {
    target: String,
    pool: String,
}

pub fn set_key(target: &str, pool: &str) -> String {
    format!("{target}|{pool}")
}

/// Preserves the job queue and learned timings across a restart. Places
/// remember themselves through their locks and place.json; local borrow
/// requests are reread by refresh_borrows.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedState {
    /// Go's time.Duration: nanoseconds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    timings: BTreeMap<String, i64>,
    #[serde(default, deserialize_with = "config::null_as_default")]
    waiting: Vec<Job>,
    #[serde(default, deserialize_with = "config::null_as_default")]
    acquired: Vec<Acquired>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "config::null_as_default")]
    unused: Vec<PlaceRun>,
    /// When each resting machine was found unreachable, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "config::null_as_default")]
    unreachable: BTreeMap<String, Time>,
    /// The places a setup change removed, or moved to another machine, while
    /// their runner ran (State::departed).
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "config::null_as_default")]
    departed: Vec<DepartedPlace>,
}

/// A departed place as the setup had it, with its machine, which a place's
/// own JSON leaves out.
#[derive(Serialize, Deserialize)]
struct DepartedPlace {
    machine: String,
    place: Place,
}

pub type Labels = BTreeMap<String, BTreeMap<String, bool>>;
pub type LabelReader = Arc<dyn Fn(&Ctx, &str) -> Result<Labels, String> + Send + Sync>;
pub type WorkflowReader = Arc<dyn Fn(&Ctx, &str, i64) -> Result<String, String> + Send + Sync>;
pub type RunReader = Arc<dyn Fn(&Ctx, &str, i64) -> Result<bool, String> + Send + Sync>;
pub type Reacher = Arc<dyn Fn(&Ctx, &config::Machine) -> Result<(), String> + Send + Sync>;
pub type VmReader = Arc<dyn Fn(&Ctx, &config::Machine) -> Result<Vec<String>, String> + Send + Sync>;

/// The coordinator's state, which Service::mu guards, in four parts:
///   - queue (queue.rs): the jobs waiting for a place, and those acquired
///     for a runner that has not yet taken one
///   - runners (runners.rs): the runners it started, and what it still owes
///     them and their places
///   - machines (reach.rs, vmlimit.rs): which machines rest, and which macOS
///     VMs each Mac runs
///   - reads (reads.rs, closedruns.rs): what it read from GitHub's API for
///     the order rules and about quiet runs
///
/// and, within each placement pass, a snapshot of the places (snapshot.rs).
pub struct State {
    pub cfg: Arc<Config>,
    pub log: Logger,
    pub now: Arc<dyn Fn() -> Time + Send + Sync>,
    pub rules: Vec<Rule>,
    sets: HashMap<String, LiveSet>,

    // queue
    waiting: BTreeMap<String, Job>,
    acquired: BTreeMap<String, Acquired>,
    timings: BTreeMap<String, Duration>,
    /// The scale sets a job of which was cancelled before a runner took it,
    /// while a runner left waiting for nothing is still to go (give_up_idle).
    spare: BTreeSet<SetName>,

    // runners
    /// Runners that ended before a job reached them, by unused_key: each is
    /// still registered with its scale set until tidy removes it.
    unused: BTreeMap<String, PlaceRun>,
    /// The runners the coordinator gave up on that GitHub has answered for,
    /// by name (give_up): true once GitHub removed the runner and its place
    /// is stopping, false when GitHub said it runs a job.
    given_up: BTreeMap<String, bool>,
    /// The places resting after a failed launch, until when.
    cooling: BTreeMap<String, Time>,
    /// When each place was last logged free beside a job that fits it
    /// (note_waste).
    wasted: BTreeMap<String, Time>,
    /// The places a setup change removed, or moved to another machine, while
    /// their runner ran, by id, as the setup had them where the runner runs
    /// (reload.rs).
    departed: BTreeMap<String, Place>,
    /// The places that take no new job, by id, and why: account places while
    /// their machine's service updates (update.rs).
    held: BTreeMap<String, String>,

    // machines
    /// The resting machines, by name, with when each was found unreachable,
    /// and reach_asked when each was last asked.
    unreachable: BTreeMap<String, Time>,
    reach_asked: BTreeMap<String, Time>,
    /// Asks a resting machine's agent for an answer.
    pub reach: Option<Reacher>,
    /// Set when a place runs in tart: reads each Mac's running macOS VMs;
    /// vms is the latest reading by machine, and vms_read when it was taken.
    pub vm_reader: Option<VmReader>,
    vms: BTreeMap<String, Vec<String>>,
    vms_read: BTreeMap<String, Time>,

    // reads
    /// The readers the order rules need, set only when the rules use them.
    pub label_reader: Option<LabelReader>,
    pub workflow_reader: Option<WorkflowReader>,
    /// Set whenever GitHub's API can be read: says whether GitHub has closed
    /// a workflow run, cancelled or finished, and every job of it with it
    /// (closedruns.rs).
    pub run_reader: Option<RunReader>,
    runs_checked: BTreeMap<WorkflowRun, Time>,
    run_reads: Vec<Time>,
    labels_read: BTreeMap<String, Time>,
    labels: BTreeMap<String, Labels>,
    run_workflows: BTreeMap<WorkflowRun, RunWorkflow>,

    /// The live drains, reread on every placement (drain.rs).
    drains: Vec<drain::Drain>,
}

/// mu guards the state, and is never held through a call to GitHub, a
/// machine or a launch: a pass reads under mu, lets go of it to call out, and
/// takes it again to record the answer. pass makes the placement and tidying
/// passes take turns, so only one acts on a place at a time. Only the serve
/// loop (serve.rs) runs them, with the serving context; a scale set's
/// session hands its messages to handle, which records them and wakes the
/// loop.
pub struct Service {
    pub mu: Mutex<State>,
    launcher: Arc<dyn Launcher>,
    pass: Mutex<()>,
    events: Sender<Event>,
    wake_sent: AtomicBool,
    /// The questions to resting machines not yet answered (reach.rs).
    asking: WaitGroup,
}

pub type Guard<'a> = std::sync::MutexGuard<'a, State>;

impl State {
    /// A state that knows only cfg: no readers, nothing restored.
    fn new(cfg: Arc<Config>, log: Logger, rules: Vec<Rule>) -> State {
        State {
            cfg,
            log,
            now: Arc::new(Time::now),
            rules,
            sets: HashMap::new(),
            waiting: BTreeMap::new(),
            acquired: BTreeMap::new(),
            timings: BTreeMap::new(),
            spare: BTreeSet::new(),
            unused: BTreeMap::new(),
            given_up: BTreeMap::new(),
            cooling: BTreeMap::new(),
            wasted: BTreeMap::new(),
            departed: BTreeMap::new(),
            held: BTreeMap::new(),
            unreachable: BTreeMap::new(),
            reach_asked: BTreeMap::new(),
            reach: None,
            vm_reader: None,
            vms: BTreeMap::new(),
            vms_read: BTreeMap::new(),
            label_reader: None,
            workflow_reader: None,
            run_reader: None,
            runs_checked: BTreeMap::new(),
            run_reads: Vec::new(),
            labels_read: BTreeMap::new(),
            labels: BTreeMap::new(),
            run_workflows: BTreeMap::new(),
            drains: Vec::new(),
        }
    }
}

impl Service {
    pub fn new(cfg: Arc<Config>, log: Logger, launcher: Arc<dyn Launcher>, events: Sender<Event>) -> Result<Service, String> {
        let mut st = State::new(cfg.clone(), log, plan::parse_order(&cfg)?);
        st.reach = Some(Arc::new(reach_agent));
        reload::prepare(&cfg)?;
        launcher.use_setup(&cfg)?;
        match std::fs::read(st.state_path()) {
            Ok(b) => st.restore(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("open {}: {e}", st.state_path().display())),
        }
        Ok(Service {
            mu: Mutex::new(st),
            launcher,
            pass: Mutex::new(()),
            events,
            wake_sent: AtomicBool::new(false),
            asking: WaitGroup::default(),
        })
    }

    pub fn lock(&self) -> Guard<'_> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The setup the coordinator runs now. A setup change replaces it whole
    /// and never changes it in place.
    pub fn setup(&self) -> Arc<Config> {
        self.lock().cfg.clone()
    }

    /// Wakes the serve loop to run a pass; wakes not yet seen count once.
    fn poke(&self) {
        if !self.wake_sent.swap(true, Ordering::SeqCst) {
            let _ = self.events.send(Event::Wake);
        }
    }

    /// The serve loop calls this as it takes an Event::Wake.
    pub fn woken(&self) {
        self.wake_sent.store(false, Ordering::SeqCst);
    }

    /// Makes a scale set's client available for acquiring and placing.
    pub fn add_set(&self, target: &str, pool: &str, c: Arc<dyn SetClient>) {
        self.lock().sets.insert(set_key(target, pool), LiveSet { client: c, calls: WaitGroup::default() });
        self.poke();
    }

    /// Makes c, a scale set's client, unavailable once its session ends, and
    /// returns once no pass calls GitHub through it any more, so no job is
    /// acquired or runner registered through a closing session. A newer
    /// session's client for the same scale set stays.
    pub fn remove_set(&self, target: &str, pool: &str, c: &Arc<dyn SetClient>) {
        let calls = {
            let mut st = self.lock();
            let key = set_key(target, pool);
            match st.sets.get(&key) {
                Some(ls) if same_client(&ls.client, c) => st.sets.remove(&key).map(|ls| ls.calls),
                _ => None,
            }
        };
        if let Some(calls) = calls {
            calls.wait();
        }
    }

    /// Takes one message from a scale set's session: records what changed
    /// and wakes the serve loop to place whatever now fits. It calls nothing
    /// outside the coordinator, so the session acknowledges the message at
    /// once.
    pub fn handle(&self, target: &str, pool: &str, msg: Option<&scaleset::Message>) {
        let Some(msg) = msg else { return };
        self.lock().handle(target, pool, msg);
        self.poke();
    }
}

fn same_client(a: &Arc<dyn SetClient>, b: &Arc<dyn SetClient>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

impl State {
    fn state_path(&self) -> PathBuf {
        Path::new(&self.cfg.state_dir).join("state.json")
    }

    fn now(&self) -> Time {
        (self.now)()
    }

    fn restore(&mut self, b: &[u8]) {
        let st: SavedState = match serde_json::from_slice(b) {
            Ok(st) => st,
            Err(e) => {
                self.log.warn("state file unreadable; starting with no waiting jobs", &[("err", &e)]);
                SavedState::default()
            }
        };
        for (k, n) in st.timings {
            self.timings.insert(k, Duration::from_nanos(n.max(0) as u64));
        }
        for j in st.waiting {
            self.waiting.insert(j.key(), j);
        }
        for a in st.acquired {
            self.acquired.insert(a.job.key(), a);
        }
        for run in st.unused {
            self.unused.insert(runners::unused_key(&run), run);
        }
        for (name, since) in st.unreachable {
            if self.cfg.machines.iter().any(|m| m.name == name) {
                self.unreachable.insert(name, since);
            }
        }
        for d in st.departed {
            let mut p = d.place;
            p.machine = d.machine;
            if self.cfg.place(&p.id).is_none_or(|q| q.machine != p.machine) {
                self.departed.insert(p.id.clone(), p);
            }
        }
        self.log.info(
            "restored state",
            &[
                ("waiting", &self.waiting.len()),
                ("acquired", &self.acquired.len()),
                ("unused", &self.unused.len()),
                ("resting", &self.unreachable.len()),
            ],
        );
    }

    /// Hands out the client of target's scale set for pool, while its
    /// session is open, for calls made after letting go of mu.
    fn set(&self, target: &str, pool: &str) -> Option<SetHandle> {
        let ls = self.sets.get(&set_key(target, pool))?;
        Some(SetHandle { client: ls.client.clone(), _call: ls.calls.add() })
    }

    fn handle(&mut self, target: &str, pool: &str, msg: &scaleset::Message) {
        let now = self.now();
        for m in &msg.job_available {
            self.wait(queue::job_of(target, pool, m, false, now));
        }
        // GitHub assigns most jobs to the scale set itself, as many as the
        // capacity each poll announces, and waits for a runner from the set.
        for m in &msg.job_assigned {
            self.wait(queue::job_of(target, pool, m, true, now));
        }
        for m in &msg.job_started {
            let keys = queue::keys_of(target, pool, m);
            self.hand_back(target, pool, &m.runner_name, &keys);
            self.drain_started(target, pool, m);
            // An assigned job carries no queue time: it is in line from when
            // the coordinator first saw it, or else from its assignment.
            let mut queued = m.queue_time;
            for k in &keys {
                if let Some(a) = self.acquired.get(k)
                    && queued.is_zero()
                {
                    queued = a.job.queue_time;
                }
                if let Some(j) = self.waiting.get(k)
                    && queued.is_zero()
                {
                    queued = j.queue_time;
                }
                self.acquired.remove(k);
                self.waiting.remove(k);
            }
            if queued.is_zero() {
                queued = m.scale_set_assign_time;
            }
            self.log.info(
                "job started",
                &[
                    ("repo", &queue::repo_of(target, m)),
                    ("job", &m.job_display_name),
                    ("run", &m.workflow_run_id),
                    ("runner", &m.runner_name),
                    ("waited", &round_secs(m.runner_assign_time.sub(queued))),
                ],
            );
        }
        for m in &msg.job_completed {
            // Runner assignment to finish is wall runtime, not queue or boot
            // time. Cancellations, failures and assignment take-backs teach
            // nothing.
            if m.result == "succeeded"
                && !m.runner_name.is_empty()
                && !m.runner_assign_time.is_zero()
                && m.finish_time.after(m.runner_assign_time)
            {
                let j = queue::job_of(target, pool, m, false, now);
                let took = m.finish_time.sub(m.runner_assign_time).to_std().unwrap_or_default();
                self.timings.insert(j.timing_key(), took);
            }
            for k in queue::keys_of(target, pool, m) {
                // When no runner reached an assigned job in five minutes,
                // GitHub takes the assignment back, with no finish time, and
                // assigns the job again, a moment later or in this same
                // message. A cancelled job has a finish time.
                if m.runner_name.is_empty() && m.finish_time.is_zero() && self.take_back(&k, &m.job_id, now) {
                    continue;
                }
                if self.acquired.contains_key(&k) && m.runner_name.is_empty() {
                    // Cancelled before a runner took it: the next tidy stops
                    // the runner left waiting for nothing.
                    self.spare.insert(SetName { target: target.into(), pool: pool.into() });
                }
                self.acquired.remove(&k);
                self.waiting.remove(&k);
            }
            self.log.info(
                "job completed",
                &[
                    ("repo", &queue::repo_of(target, m)),
                    ("job", &m.job_display_name),
                    ("run", &m.workflow_run_id),
                    ("runner", &m.runner_name),
                    ("result", &m.result),
                ],
            );
        }
        if let Some(stats) = &msg.statistics {
            let known = self.waiting.values().filter(|j| j.target == target && j.pool == pool).count() as i64;
            if stats.total_available_jobs > known {
                self.log.warn(
                    "GitHub counts more waiting jobs than the coordinator knows",
                    &[("target", &target), ("pool", &pool), ("github", &stats.total_available_jobs), ("known", &known)],
                );
            }
        }
        self.save();
    }

    fn save(&self) {
        let mut st = SavedState::default();
        for (k, d) in &self.timings {
            st.timings.insert(k.clone(), d.as_nanos().min(i64::MAX as u128) as i64);
        }
        st.unreachable = self.unreachable.clone();
        // The maps are ordered by key already, as the file wants them.
        st.waiting = self.waiting.values().cloned().collect();
        st.acquired = self.acquired.values().cloned().collect();
        st.unused = self.unused.values().cloned().collect();
        for p in self.departed.values() {
            st.departed.push(DepartedPlace { machine: p.machine.clone(), place: p.clone() });
        }
        if let Err(e) = config::write_json(&self.state_path(), &st) {
            self.log.error("could not save state", &[("err", &e)]);
        }
    }
}

/// Removes place logs older than cutoff.
fn prune_logs(dir: &Path, cutoff: Time) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(modified) = e.metadata().and_then(|m| m.modified()) else { continue };
        if Time(modified.into()).before(cutoff) {
            let p = e.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}
