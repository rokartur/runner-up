//! Orders waiting jobs by the configured rules and gives free places to them.

use crate::config::{self, Config, Place, Share};
use crate::go::time::Time;
use crate::go::{self, pathmatch};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// Waiting work: a job offered or assigned by a scale set, or a local borrow.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    /// Identifies a local borrow; it competes with jobs but takes only tart places.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub borrow_id: String,
    /// The repository or organization whose scale set has the job; repo is
    /// the repository the job belongs to, the same for a repository's set.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub request_id: i64,
    #[serde(default)]
    pub queue_time: Time,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub run_id: i64,
    #[serde(default)]
    pub seen_at: Time,
    /// GitHub assigned the job to its scale set itself, so there is nothing
    /// to acquire; any runner the set starts may take it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assigned: bool,
    /// GitHub took the job's assignment back, no runner having reached it,
    /// and has not assigned it again yet. Until it does, no runner the set
    /// starts would get it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub taken_back: bool,
    /// Names GitHub's latest offer or assignment of the job: each assignment
    /// has a new one, and a take-back names the one it takes back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub job_id: String,
    /// When GitHub last sent anything about the job: an offer, an assignment
    /// or a take-back. GitHub takes an assigned job no runner took back after
    /// five minutes, so a job GitHub has long stopped mentioning may belong
    /// to a run it has closed.
    #[serde(default)]
    pub heard_at: Time,

    // Filled in before planning, never saved.
    /// The pool's size, or zero to use each place's own size.
    #[serde(skip)]
    pub size: Share,
    /// A borrow's declared duration, or a job's last successful wall time
    /// from runner assignment to finish; zero for a job with no successful run.
    #[serde(skip)]
    pub estimate: Duration,
    /// The top-level workflow path read from the run's metadata.
    #[serde(skip)]
    pub run_workflow: String,
    /// The labels of the open pull request the job's ref belongs to.
    #[serde(skip)]
    pub labels: BTreeMap<String, bool>,
    /// The drain whose places the job may take, or "" for places no drain
    /// holds (coordinator::drain).
    #[serde(skip)]
    pub drain: String,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A job GitHub offers (JobAvailable) is named by its runner request. A job
/// it assigns to the scale set itself (JobAssigned) comes with request 0, and
/// with a new job id each time it assigns the job again, so it is named by
/// its workflow run and name, which every assignment of it shares.
pub fn request_key(target: &str, pool: &str, request_id: i64) -> String {
    format!("{target}|{pool}|{request_id}")
}

pub fn run_key(target: &str, pool: &str, run_id: i64, name: &str) -> String {
    format!("{target}|{pool}|run {run_id}|{name}")
}

/// Drops the branch or tag from a workflow ref, so a successful run teaches
/// every branch's runs.
pub fn workflow_path(r: &str) -> &str {
    r.split_once('@').map_or(r, |(p, _)| p)
}

impl Job {
    pub fn key(&self) -> String {
        if !self.borrow_id.is_empty() {
            return format!("borrow|{}", self.borrow_id);
        }
        if self.request_id != 0 {
            return request_key(&self.target, &self.pool, self.request_id);
        }
        run_key(&self.target, &self.pool, self.run_id, &self.name)
    }

    /// Scopes a learned estimate by repository, pool, workflow file and the
    /// job's full display name, matrix values included.
    pub fn timing_key(&self) -> String {
        format!("{}|{}|{}|{}", self.repo, self.pool, workflow_path(&self.workflow), self.name)
    }

    /// The git ref of a job's own repository workflow, or "" for a workflow
    /// in another repository, so a reusable workflow elsewhere cannot claim
    /// a label.
    fn workflow_ref(&self) -> &str {
        if !self.workflow.starts_with(&format!("{}/.github/workflows/", self.repo)) {
            return "";
        }
        self.workflow.split_once('@').map_or("", |(_, r)| r)
    }

    /// The ref a pull request label may reach: a pull request's merge or
    /// head ref, or a branch. A branch named after a label grants nothing
    /// unless the reader found that labeled pull request's head there.
    pub fn label_ref(&self) -> String {
        let r = self.workflow_ref();
        if let Some(p) = r.strip_prefix("refs/pull/") {
            if let Some((n, tail)) = p.split_once('/') {
                if (tail == "merge" || tail == "head") && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                    return r.into();
                }
            }
            return String::new();
        }
        match r.strip_prefix("refs/heads/") {
            Some(b) if !b.is_empty() => r.into(),
            _ => String::new(),
        }
    }
}

/// What the planner needs to know about a place right now.
#[derive(Clone, Debug, Default)]
pub struct PlaceView {
    pub place: Place,
    pub busy: bool,
    /// The drain holding the place: it takes only the jobs whose drain matches.
    pub drain: String,
}

/// What a counted machine's places may still reserve right now: its budget
/// less the shares of places with held locks. A machine that counts no
/// memory has memory false.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Headroom {
    pub cores: i64,
    pub memory_mb: i64,
    pub memory: bool,
}

impl Headroom {
    fn fits(&self, sh: Share) -> bool {
        sh.cores <= self.cores && (!self.memory || sh.memory_mb <= self.memory_mb)
    }

    fn less(mut self, sh: Share) -> Headroom {
        self.cores -= sh.cores;
        self.memory_mb -= sh.memory_mb;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Assignment {
    pub job: Job,
    pub place: Place,
}

/// Orders waiting jobs by the order rules; first-in, first-out breaks every tie.
pub fn rank(waiting: &[Job], rules: &[Rule], now: Time) -> Vec<Job> {
    let mut jobs = waiting.to_vec();
    jobs.sort_by(|a, b| order(a, b, rules, now));
    jobs
}

fn order(a: &Job, b: &Job, rules: &[Rule], now: Time) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    if before(a, b, rules, now) {
        Less
    } else if before(b, a, rules, now) {
        Greater
    } else {
        Equal
    }
}

fn fifo(a: &Job, b: &Job) -> bool {
    if a.queue_time != b.queue_time {
        return a.queue_time.before(b.queue_time);
    }
    a.key() < b.key()
}

/// Gives free places to waiting jobs. Jobs are taken in rank order; each
/// takes a free place serving its pool whose job size fits what its machine
/// has left, preferring the least-busy machine, then the order places are
/// listed in. When a job waits only for room, it keeps its size on one
/// eligible machine. Later jobs use only the remainder; no running job is
/// interrupted. Jobs without a free eligible place (or with an unavailable VM
/// slot) hold nothing. Each machine keeps room for only its first such
/// waiting job. A place a drain holds takes only the jobs that drain admits,
/// and those jobs go first, so on a machine shared with other places they
/// keep its room.
pub fn plan(
    waiting: &[Job],
    places: &[PlaceView],
    headroom: &HashMap<String, Headroom>,
    rules: &[Rule],
    now: Time,
) -> Vec<Assignment> {
    let mut jobs = rank(waiting, rules, now);
    jobs.sort_by_key(|j| j.drain.is_empty());
    let mut left = headroom.clone();
    let mut busy: HashMap<String, i64> = HashMap::new();
    let mut free: Vec<PlaceView> = vec![];
    for p in places {
        if p.busy {
            *busy.entry(p.place.machine.clone()).or_default() += 1;
        } else {
            free.push(p.clone());
        }
    }
    let busy_of = |busy: &HashMap<String, i64>, m: &str| busy.get(m).copied().unwrap_or(0);
    let mut out = vec![];
    let mut held: HashMap<String, bool> = HashMap::new();
    for j in jobs {
        let size = |p: &PlaceView| if j.size.cores > 0 { j.size } else { p.place.share() };
        let (mut best, mut blocked): (Option<usize>, Option<usize>) = (None, None);
        for (i, p) in free.iter().enumerate() {
            if p.drain != j.drain
                || !p.place.pools.contains(&j.pool)
                || !j.borrow_id.is_empty() && p.place.provider != config::PROVIDER_TART
            {
                continue;
            }
            let m = &p.place.machine;
            if let Some(h) = left.get(m) {
                if !h.fits(size(p)) {
                    let held_here = held.get(m).copied().unwrap_or(false);
                    if !held_here
                        && blocked.is_none_or(|b| busy_of(&busy, m) < busy_of(&busy, &free[b].place.machine))
                    {
                        blocked = Some(i);
                    }
                    continue;
                }
            }
            if best.is_none_or(|b| busy_of(&busy, m) < busy_of(&busy, &free[b].place.machine)) {
                best = Some(i);
            }
        }
        let Some(best) = best else {
            if let Some(b) = blocked {
                let p = &free[b];
                let m = p.place.machine.clone();
                let h = left.get(&m).copied().unwrap_or_default().less(size(p));
                left.insert(m.clone(), h);
                held.insert(m, true);
            }
            continue;
        };
        let mut p = free.remove(best);
        let sh = size(&p);
        p.place.cores = sh.cores;
        p.place.memory_mb = sh.memory_mb;
        *busy.entry(p.place.machine.clone()).or_default() += 1;
        if let Some(h) = left.get(&p.place.machine).copied() {
            left.insert(p.place.machine.clone(), h.less(sh));
        }
        out.push(Assignment { job: j, place: p.place });
    }
    out
}

// The order rules decide which waiting job a free place takes. Each rule
// splits the line in two, matching jobs first except for last:, which puts
// them last. Later rules order ties; first-in, first-out breaks the last tie:
//
//	pool:NAME         jobs asking for the pool NAME, such as short jobs a
//	                  merge waits on, on the places it shares with other pools
//	last:NAME         the one rule that puts its jobs last: jobs asking for
//	                  the pool NAME, such as Linux guests sharing a Mac's
//	                  budget, go behind other jobs tied by earlier rules, so
//	                  a waiting job of another pool keeps its room first
//	label:NAME        jobs from an open pull request labeled NAME, from the
//	                  same repository, including runs on its head branch
//	waited:DURATION   jobs waiting at least DURATION, oldest first among them,
//	                  ahead of everything the later rules would prefer
//	workflow:GLOB,..  jobs whose top-level workflow file, without .yml or
//	                  .yaml, matches a glob, such as release or release-*
//	shortest          shorter learned runtimes first; a job never seen to
//	                  succeed counts as UNKNOWN_DURATION
//
// GitHub picks which of a scale set's assigned jobs a new runner takes, so
// the order is exact between pools and repositories but only approximate
// inside one scale set.
const DEFAULT_ORDER: [&str; 2] = ["waited:60m", "shortest"];

const UNKNOWN_DURATION: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq)]
pub enum Rule {
    Pool(String),
    Last(String),
    Label(String),
    Waited(Duration),
    Workflow(Vec<String>),
    Shortest,
}

/// Reads and checks the setup's order rules, or the default order when it
/// has none. A pool or last rule must name a pool of the setup: one that
/// names no pool would order nothing, and say nothing.
pub fn parse_order(cfg: &Config) -> Result<Vec<Rule>, String> {
    let order: Vec<&str> = if cfg.order.is_empty() {
        DEFAULT_ORDER.to_vec()
    } else {
        cfg.order.iter().map(String::as_str).collect()
    };
    let mut out = vec![];
    for s in order {
        let (kind, arg) = s.split_once(':').unwrap_or((s, ""));
        let qs = go::quote(s);
        let r = match kind {
            "label" => {
                if arg.is_empty() {
                    return Err(format!("order rule {qs} names no label"));
                }
                Rule::Label(arg.into())
            }
            "pool" | "last" => {
                if arg.is_empty() {
                    return Err(format!("order rule {qs} names no pool"));
                }
                if !cfg.pools.iter().any(|p| p.label == arg) {
                    return Err(format!("order rule {qs} names no pool of this setup"));
                }
                if kind == "pool" { Rule::Pool(arg.into()) } else { Rule::Last(arg.into()) }
            }
            "waited" => match go::duration::parse_nanos(arg) {
                Ok(n) if n > 0 => Rule::Waited(Duration::from_nanos(n as u64)),
                _ => return Err(format!("order rule {qs} needs a duration, such as waited:60m")),
            },
            "workflow" => {
                let mut globs = vec![];
                for g in arg.split(',') {
                    let g = g.trim();
                    if g.is_empty() || pathmatch::matches(g, "").is_err() {
                        return Err(format!("order rule {qs} has a bad glob {}", go::quote(g)));
                    }
                    globs.push(g.to_string());
                }
                Rule::Workflow(globs)
            }
            "shortest" => {
                if !arg.is_empty() {
                    return Err(format!("order rule {qs} takes no argument"));
                }
                Rule::Shortest
            }
            _ => {
                return Err(format!(
                    "unknown order rule {qs}: use pool:, last:, label:, waited:, workflow: or shortest"
                ));
            }
        };
        out.push(r);
    }
    Ok(out)
}

/// Reports whether job x goes ahead of job y.
fn before(x: &Job, y: &Job, rules: &[Rule], now: Time) -> bool {
    for r in rules {
        match r {
            Rule::Pool(pool) => {
                let (a, b) = (&x.pool == pool, &y.pool == pool);
                if a != b {
                    return a;
                }
            }
            Rule::Last(pool) => {
                let (a, b) = (&x.pool == pool, &y.pool == pool);
                if a != b {
                    return b;
                }
            }
            Rule::Label(label) => {
                let has = |j: &Job| j.labels.get(label).copied().unwrap_or(false);
                let (a, b) = (has(x), has(y));
                if a != b {
                    return a;
                }
            }
            Rule::Waited(wait) => {
                let waited = |j: &Job| {
                    !j.queue_time.is_zero() && now.sub(j.queue_time).to_std().is_ok_and(|d| d >= *wait)
                };
                let (a, b) = (waited(x), waited(y));
                if a != b {
                    return a;
                }
                if a {
                    return fifo(x, y);
                }
            }
            Rule::Workflow(globs) => {
                let (a, b) = (matches(globs, x), matches(globs, y));
                if a != b {
                    return a;
                }
            }
            Rule::Shortest => {
                let est = |j: &Job| if j.estimate.is_zero() { UNKNOWN_DURATION } else { j.estimate };
                let (dx, dy) = (est(x), est(y));
                if dx != dy {
                    return dx < dy;
                }
            }
        }
    }
    fifo(x, y)
}

/// Reads the run's top-level workflow file, falling back to the job's own
/// when the run's could not be read. A display name or branch never counts,
/// nor does a workflow in another repository's folder.
fn matches(globs: &[String], j: &Job) -> bool {
    let w = if j.run_workflow.is_empty() { &j.workflow } else { &j.run_workflow };
    let path = workflow_path(w);
    let name = path.rsplit('/').next().unwrap_or(path);
    if !name.ends_with(".yml") && !name.ends_with(".yaml") {
        return false;
    }
    let name = name.strip_suffix(".yml").unwrap_or(name);
    let name = name.strip_suffix(".yaml").unwrap_or(name);
    globs.iter().any(|g| pathmatch::matches(g, name) == Ok(true))
}

/// Reports which readers the rules use: pull request labels, and the run's
/// top-level workflow.
pub fn needs(rules: &[Rule]) -> (bool, bool) {
    let labels = rules.iter().any(|r| matches!(r, Rule::Label(_)));
    let workflows = rules.iter().any(|r| matches!(r, Rule::Workflow(_)));
    (labels, workflows)
}

#[cfg(test)]
mod tests;
