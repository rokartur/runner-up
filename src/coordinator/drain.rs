// A drain pauses new placement on named places while the coordinator keeps
// running, so that a step needing those places idle can run there as soon as
// their running jobs finish (`runner-up drain`). Its caller owns it: the
// caller publishes the request and holds its live.lock for as long as the
// drain lasts, so a caller that stops, crashes or reaches its time limit
// always resumes placement. No running job or borrow is ever stopped for a
// drain, and places it does not name place as before.
//
// A drain goes through these phases:
//
//	draining   its places take no new job or borrow; what runs there carries on
//	admitting  once all its places were free at once, they take only the
//	           named jobs, one job for each name
//	running    every named job has run or runs there; the places take nothing
//	           more until those jobs end
//	done       each named job ran in one of its places and that runner ended;
//	           its places take any job again at once
//
// A named job waits for the drain's places while the drain lasts, rather than
// start elsewhere. GitHub picks which of a scale set's jobs a runner takes, so
// a runner started for a named job may run another job of the same pool and
// repository; that job finishes, and the named job is admitted again.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::State;
use super::place::{PlaceRun, read_place_run};
use super::queue::repo_of;
use super::request::{open_request, scan_requests};
use crate::config::{self, Config};
use crate::ctx::Ctx;
use crate::go::flag::FlagSet;
use crate::go::quote;
use crate::go::time::{Time, round_secs};
use crate::plan::Job;
use crate::scaleset::JobMessage;
use crate::{cli, signals};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DrainRequest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, deserialize_with = "config::null_as_default")]
    pub places: Vec<String>,
    #[serde(default, deserialize_with = "config::null_as_default")]
    pub jobs: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(default)]
    pub minutes: i64,
    #[serde(default)]
    pub queued: Time,
}

/// The coordinator's side of a drain, kept beside its request so that a
/// coordinator restart neither loses nor repeats its progress. Go's omitempty
/// never drops a time, so the times are always written.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DrainState {
    /// When all the drain's places were first free at once.
    #[serde(default)]
    pub drained: Time,
    /// The place and runner that ran each named job, by job name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "config::null_as_default")]
    pub ran: BTreeMap<String, DrainRun>,
    #[serde(default)]
    pub done: Time,
    /// Where the drain stands, for its caller and status.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Where a drain stands; the comment at the top says what each phase means.
pub const DRAIN_DRAINING: &str = "draining";
pub const DRAIN_ADMITTING: &str = "admitting";
pub const DRAIN_RUNNING: &str = "running";
pub const DRAIN_DONE: &str = "done";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DrainRun {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    #[serde(default)]
    pub runner: String,
    /// GitHub gave the job to a runner the drain did not start for it,
    /// outside its places or before they were drained. The drain no longer
    /// waits for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
}

pub struct Drain {
    pub req: DrainRequest,
    pub st: DrainState,
    saved: Vec<u8>,
}

/// heldForDrain matches no place: the job waits for its drain.
pub const HELD_FOR_DRAIN: &str = "held for a drain";

fn drains_dir(cfg: &Config) -> PathBuf {
    std::path::Path::new(&cfg.state_dir).join("drains")
}

pub(super) fn drain_dir(cfg: &Config, id: &str) -> PathBuf {
    drains_dir(cfg).join(id)
}

impl Drain {
    /// Whether the drain still holds its places.
    pub fn active(&self) -> bool {
        self.st.done.is_zero()
    }

    /// Whether job name, of repository repo, is one the drain admits and has
    /// not yet run.
    fn names(&self, name: &str, repo: &str) -> bool {
        !self.st.ran.contains_key(name) && self.req.jobs.iter().any(|j| j == name) && (self.req.repo.is_empty() || self.req.repo == repo)
    }

    /// Whether the drain started place's run for one of its jobs: the drain's
    /// place, launched once its places were drained.
    fn admitted(&self, place: &str, run: &PlaceRun) -> bool {
        self.req.places.iter().any(|p| p == place)
            && run.borrow_id.is_empty()
            && !self.st.drained.is_zero()
            && !run.launched_at.before(self.st.drained)
    }
}

/// Reads the published drains whose callers hold them, in the order they
/// were made, and names those whose callers let go (request.rs).
fn list_drains(cfg: &Config) -> (Vec<Drain>, Vec<String>) {
    let (held, gone) = scan_requests(&drains_dir(cfg), &["request.json"]);
    let mut live = Vec::new();
    for id in held {
        let dir = drain_dir(cfg, &id);
        let Ok(req) = config::read_json::<DrainRequest>(&dir.join("request.json")) else {
            continue;
        };
        if req.id != id {
            continue;
        }
        let saved = std::fs::read(dir.join("state.json")).unwrap_or_default();
        let st = serde_json::from_slice(&saved).unwrap_or_default();
        live.push(Drain { req, st, saved });
    }
    live.sort_by_key(|d| d.req.queued);
    (live, gone)
}

impl State {
    /// Rereads the drains, forgets those whose callers let go, and moves each
    /// live one on: a drain whose places are all free starts admitting its
    /// jobs, and one whose jobs have all run is done.
    pub fn refresh_drains(&mut self) {
        let (live, gone) = list_drains(&self.cfg);
        for id in gone {
            let dir = drain_dir(&self.cfg, &id);
            let r: DrainRequest = config::read_json(&dir.join("request.json")).unwrap_or_default();
            let ended = config::read_json::<DrainState>(&dir.join("state.json")).is_ok_and(|st| !st.done.is_zero());
            if !ended {
                self.log.info("drain ended by its caller; placement resumes", &[("label", &r.label), ("places", &r.places.join(","))]);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.drains = live;
        self.advance_drains();
    }

    /// Moves each active drain on by what its places run. A placement pass
    /// calls it before placing, so a drain that just drained admits its jobs
    /// in that pass, and after, so its caller sees what the pass started.
    pub fn advance_drains(&mut self) {
        let mut drains = std::mem::take(&mut self.drains);
        for d in drains.iter_mut().filter(|d| d.active()) {
            self.advance_drain(d);
        }
        self.drains = drains;
    }

    fn advance_drain(&self, d: &mut Drain) {
        let now = self.now();
        if d.st.phase.is_empty() {
            self.log.info(
                "drain started",
                &[("label", &d.req.label), ("places", &d.req.places.join(",")), ("jobs", &d.req.jobs.join(",")), ("repo", &d.req.repo)],
            );
        }
        let mut running = Vec::new();
        for id in d.req.places.clone() {
            let dir = self.cfg.place_dir(&id);
            // A place's run that ended before this pass is already its last run.
            for name in ["place.json", "last-place.json"] {
                let Ok(run) = config::read_json::<PlaceRun>(&dir.join(name)) else {
                    continue;
                };
                if d.admitted(&id, &run) && run.started && d.names(&run.ran_job, &run.repo) {
                    d.st.ran.insert(run.ran_job.clone(), DrainRun { place: id.clone(), runner: run.runner_name.clone(), outside: false });
                    self.log.info(
                        "drain: a named job reached its runner",
                        &[("label", &d.req.label), ("place", &id), ("job", &run.ran_job), ("runner", &run.runner_name)],
                    );
                }
            }
            if self.runner_alive(&id) {
                running.push(format!("{id} runs {}", self.describe_run(&id)));
            }
        }
        if d.st.drained.is_zero() {
            if !running.is_empty() {
                d.st.phase = DRAIN_DRAINING.into();
                d.st.detail = running.join("; ");
                self.save_drain(d);
                return;
            }
            d.st.drained = now;
            self.log.info(
                "drained; admitting the named jobs",
                &[("label", &d.req.label), ("places", &d.req.places.join(",")), ("waited", &round_secs(now.sub(d.req.queued)))],
            );
        }
        let mut states = Vec::new();
        let (mut all_ran, mut finished) = (true, true);
        for name in &d.req.jobs {
            if let Some(r) = d.st.ran.get(name) {
                if r.outside {
                    let mut place = "outside the drain".to_string();
                    if !r.place.is_empty() {
                        place = format!("in {}, {place}", r.place);
                    }
                    states.push(format!("{} started {place}", quote(name)));
                } else if self.runner_runs(&r.place, &r.runner) {
                    finished = false;
                    states.push(format!("{} runs in {}", quote(name), r.place));
                } else {
                    states.push(format!("{} ran in {}", quote(name), r.place));
                }
                continue;
            }
            all_ran = false;
            finished = false;
            match self.starting_for(d, name) {
                Some(place) => states.push(format!("{} starting in {place}", quote(name))),
                None => states.push(format!("{} waits", quote(name))),
            }
        }
        d.st.phase = DRAIN_ADMITTING.into();
        d.st.detail = states.join(", ");
        if all_ran {
            // Every named job has its place; the places take nothing more
            // until those jobs end.
            d.st.phase = DRAIN_RUNNING.into();
        }
        if finished {
            d.st.done = now;
            d.st.phase = DRAIN_DONE.into();
            d.st.detail += "; placement resumed";
            self.log.info(
                "drain done; placement resumes",
                &[("label", &d.req.label), ("places", &d.req.places.join(",")), ("took", &round_secs(now.sub(d.req.queued)))],
            );
        }
        self.save_drain(d);
    }

    fn save_drain(&self, d: &mut Drain) {
        let b = serde_json::to_vec_pretty(&d.st).unwrap();
        if d.saved.trim_ascii() == b.as_slice() {
            return;
        }
        if let Err(e) = config::write_json(&drain_dir(&self.cfg, &d.req.id).join("state.json"), &d.st) {
            self.log.error("could not save a drain's state", &[("label", &d.req.label), ("err", &e)]);
            return;
        }
        d.saved = b;
    }

    /// Records, for each live drain, a named job GitHub gave a runner. In a
    /// runner the drain started, the job counts as run there; in any other
    /// runner, the drain stops waiting for it, since that job will not wait
    /// again. Each runner's place says which runner it runs.
    pub fn drain_started(&self, target: &str, pool: &str, m: &JobMessage) {
        let (live, _) = list_drains(&self.cfg);
        let repo = repo_of(target, m);
        for mut d in live {
            if !d.active() || !d.names(&m.job_display_name, &repo) || !self.drain_serves(&d, pool) {
                continue;
            }
            let mut r = DrainRun { place: String::new(), runner: m.runner_name.clone(), outside: true };
            for p in self.cfg.places() {
                if let Some(run) = read_place_run(&self.cfg, &p.id)
                    && run.runner_name == m.runner_name
                {
                    r.outside = !d.admitted(&p.id, &run);
                    r.place = p.id;
                    break;
                }
            }
            self.log.info(
                "drain: a named job reached a runner",
                &[
                    ("label", &d.req.label),
                    ("job", &m.job_display_name),
                    ("runner", &m.runner_name),
                    ("place", &r.place),
                    ("outside", &r.outside),
                ],
            );
            d.st.ran.insert(m.job_display_name.clone(), r);
            self.save_drain(&mut d);
        }
    }

    /// Whether one of the places the drain holds serves pool.
    fn drain_serves(&self, d: &Drain, pool: &str) -> bool {
        d.req.places.iter().any(|id| self.cfg.place(id).is_some_and(|p| p.pools.iter().any(|x| x == pool)))
    }

    /// Names what a busy place runs, for a drain's caller.
    fn describe_run(&self, place_id: &str) -> String {
        let Some(run) = read_place_run(&self.cfg, place_id) else {
            return "a job".into();
        };
        if !run.borrow_id.is_empty() {
            return format!("borrow {}", quote(&run.job_name));
        }
        format!("{} {}", run.repo, quote(&run.job_name))
    }

    /// Whether place still runs the runner named runner.
    fn runner_runs(&self, place_id: &str, runner: &str) -> bool {
        self.runner_alive(place_id) && read_place_run(&self.cfg, place_id).is_some_and(|run| run.runner_name == runner)
    }

    /// The drain's place whose live runner was started for job name and has
    /// not yet taken a job, if any.
    fn starting_for(&self, d: &Drain, name: &str) -> Option<String> {
        d.req
            .places
            .iter()
            .find(|id| {
                self.runner_alive(id)
                    && read_place_run(&self.cfg, id).is_some_and(|run| {
                        run.borrow_id.is_empty() && !run.started && run.job_name == name && !run.launched_at.before(d.st.drained)
                    })
            })
            .cloned()
    }

    /// The live drain holding place id, the earliest made when two name it.
    pub fn drain_of(&self, id: &str) -> Option<&Drain> {
        self.drains.iter().find(|d| d.active() && d.req.places.iter().any(|p| p == id))
    }

    /// Which drain's places a waiting job may take: "" for any place no
    /// drain holds, a drain's id for that drain's places once it admits the
    /// job, or HELD_FOR_DRAIN while the job waits for a drain that does not
    /// yet admit it. A job counts as named only where a place of the drain
    /// serves its pool.
    pub fn drain_for(&self, j: &Job) -> String {
        if !j.borrow_id.is_empty() {
            return String::new();
        }
        for d in &self.drains {
            if !d.active() || !d.names(&j.name, &j.repo) || !self.drain_serves(d, &j.pool) {
                continue;
            }
            if d.st.drained.is_zero() || self.starting_for(d, &j.name).is_some() {
                return HELD_FOR_DRAIN.into();
            }
            return d.req.id.clone();
        }
        String::new()
    }
}

/// A live drain as `runner-up status` shows it.
pub struct DrainStatus {
    pub label: String,
    pub places: Vec<String>,
    pub jobs: Vec<String>,
    pub repo: String,
    pub queued: Time,
    pub phase: String,
    pub detail: String,
}

/// Lists the drains whose callers still hold them.
pub fn drains(cfg: &Config) -> Vec<DrainStatus> {
    let (live, _) = list_drains(cfg);
    live.into_iter()
        .map(|d| DrainStatus {
            label: d.req.label,
            places: d.req.places,
            jobs: d.req.jobs,
            repo: d.req.repo,
            queued: d.req.queued,
            phase: d.st.phase,
            detail: d.st.detail,
        })
        .collect()
}

/// Pauses new placement on named places until their running jobs finish and
/// the named jobs have run there, then resumes it. It runs on the
/// coordinator's computer, beside a running coordinator.
pub fn drain_main(args: &[String]) -> i32 {
    let mut fs = FlagSet::new("drain");
    fs.string("config", &config::default_config_path(), "setup file");
    fs.list("place", "a place to drain; repeat for more");
    fs.list("job", "the display name of a job to admit once the places are drained; repeat for more");
    fs.string("repo", "", "the repository the named jobs belong to, OWNER/NAME");
    fs.string("label", "", "name shown in status and logs");
    fs.int("minutes", 0, "the longest the drain lasts, in minutes");
    if let Some(code) = cli::parse(&mut fs, args, false) {
        return code;
    }
    let (places, jobs, label, minutes) = (fs.get_list("place"), fs.get_list("job"), fs.get_str("label"), fs.get_int("minutes"));
    if places.is_empty() || jobs.is_empty() || label.is_empty() || minutes <= 0 || minutes > i64::MAX / 60_000_000_000 {
        eprintln!("drain needs --place ID --job NAME --label TEXT --minutes N; repeat --place and --job for more");
        return 2;
    }
    let cfg = match config::load_config(&fs.get_str("config")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let repo = fs.get_str("repo");
    if let Err(e) = check_drain(&cfg, &places, &jobs, &repo) {
        eprintln!("drain: {e}");
        return 2;
    }
    let ctx = signals::notify_stop(&Ctx::background());
    let req = DrainRequest { id: String::new(), label, places, jobs, repo, minutes, queued: Time::default() };
    match run_drain(&ctx, &cfg, req, &mut std::io::stderr()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("drain: {e}");
            1
        }
    }
}

fn check_drain(cfg: &Config, places: &[String], jobs: &[String], repo: &str) -> Result<(), String> {
    for (i, id) in places.iter().enumerate() {
        if cfg.place(id).is_none() {
            return Err(format!("no place {} in the setup", quote(id)));
        }
        if places[..i].contains(id) {
            return Err(format!("place {} is named twice", quote(id)));
        }
    }
    for (i, name) in jobs.iter().enumerate() {
        if name.trim().is_empty() {
            return Err("a job name is empty".into());
        }
        if jobs[..i].contains(name) {
            return Err(format!("job {} is named twice; each name admits one job", quote(name)));
        }
    }
    if !repo.is_empty() {
        let ok = repo.split_once('/').is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'));
        if !ok {
            return Err(format!("--repo {} is not OWNER/NAME", quote(repo)));
        }
    }
    Ok(())
}

/// Publishes the drain, holds it and says where it stands until the
/// coordinator reports it done (0), the time limit passes (124) or the caller
/// is stopped (130). Every way out lets go of the drain.
fn run_drain(ctx: &Ctx, cfg: &Config, mut req: DrainRequest, out: &mut dyn Write) -> Result<i32, String> {
    let (live, _) = list_drains(cfg);
    for d in live.iter().filter(|d| d.active()) {
        if let Some(id) = req.places.iter().find(|id| d.req.places.contains(id)) {
            return Err(format!("place {id} is already drained for {}", quote(&d.req.label)));
        }
    }
    let mut request = open_request(&drains_dir(cfg), "drain-").map_err(|e| format!("could not hold the drain: {e}"))?;
    let code = hold_drain(ctx, &mut request, &mut req, out);
    request.close();
    code
}

fn hold_drain(ctx: &Ctx, request: &mut super::request::CallerRequest, req: &mut DrainRequest, out: &mut dyn Write) -> Result<i32, String> {
    req.id = request.id();
    req.queued = Time::now();
    request.publish(req)?;
    let limit = Instant::now() + Duration::from_secs(req.minutes as u64 * 60);
    let state = request.dir.join("state.json");
    let mut last = String::new();
    loop {
        let st = config::read_json::<DrainState>(&state).unwrap_or_default();
        let mut message = "waiting for the coordinator".to_string();
        if !st.phase.is_empty() {
            message = st.phase.clone();
            if !st.detail.is_empty() {
                message += &format!(": {}", st.detail);
            }
        }
        if message != last {
            let _ = writeln!(out, "drain: {message}");
            last = message;
        }
        if !st.done.is_zero() {
            return Ok(0);
        }
        let wait = Duration::from_secs(1).min(limit.saturating_duration_since(Instant::now()));
        if ctx.sleep(wait) {
            let _ = writeln!(out, "drain: stopped; placement resumes, and running jobs carry on");
            return Ok(130);
        }
        if Instant::now() >= limit {
            let _ = writeln!(out, "drain: {} minutes passed; placement resumes, and running jobs carry on", req.minutes);
            return Ok(124);
        }
    }
}
