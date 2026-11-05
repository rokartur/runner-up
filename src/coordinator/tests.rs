// Shared fakes and the harness for the coordinator's tests: a fake scale set
// at GitHub, a launcher that holds each place's lock as its place runner
// would, and recorded scale set messages (testdata/coordinator/messages).

mod assigned;
#[cfg(unix)]
mod borrow;
#[cfg(unix)]
mod closedruns;
#[cfg(unix)]
mod drain;
#[cfg(unix)]
mod placement;
#[cfg(unix)]
mod reach;
#[cfg(unix)]
mod reload;
mod rules;
mod runners;
mod service;
#[cfg(unix)]
mod sizing;
mod ssh;
#[cfg(windows)]
mod update_windows;
#[cfg(unix)]
mod vmlimit;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::config::{self, Config, Place};
use crate::coordinator::{Event, Launcher, PlaceRun, Service, SetClient, set_key};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::go::time::Time;
use crate::lock;
use crate::scaleset::{self, Kind, Message};

#[derive(Deserialize)]
pub struct Fixture {
    pub now: Time,
    pub messages: Vec<FixtureMessage>,
}

#[derive(Deserialize)]
pub struct FixtureMessage {
    /// When set, is when the message arrived.
    #[serde(default)]
    pub at: Time,
    pub repo: String,
    pub label: String,
    pub message: Message,
}

pub fn load_fixture(name: &str) -> Fixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/coordinator/messages").join(name);
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Stands in for one scale set at GitHub.
#[derive(Default)]
pub struct FakeSet(pub Mutex<FakeSetState>);

#[derive(Default)]
pub struct FakeSetState {
    /// Jobs GitHub no longer offers.
    pub gone: HashSet<i64>,
    pub acquired: Vec<i64>,
    pub removed: Vec<i64>,
    pub next_id: i64,
    /// The work folder each runner was registered with.
    pub work_folders: HashMap<String, String>,
    /// How many removals fail before GitHub answers again.
    pub unreachable: usize,
    /// The runners GitHub gave a job, which it refuses to remove.
    pub running: HashSet<i64>,
}

impl FakeSet {
    pub fn gone(ids: &[i64]) -> Arc<FakeSet> {
        let s = FakeSet::default();
        s.0.lock().unwrap().gone = ids.iter().copied().collect();
        Arc::new(s)
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, FakeSetState> {
        self.0.lock().unwrap()
    }
}

impl SetClient for FakeSet {
    fn acquire_jobs(&self, _: &Ctx, ids: &[i64]) -> Result<Vec<i64>, scaleset::Error> {
        let mut f = self.st();
        let mut out = Vec::new();
        for &id in ids {
            if !f.gone.contains(&id) {
                out.push(id);
                f.acquired.push(id);
            }
        }
        Ok(out)
    }

    fn generate_jit(&self, _: &Ctx, name: &str, work_folder: &str) -> Result<(String, i64), scaleset::Error> {
        let mut f = self.st();
        f.work_folders.insert(name.into(), work_folder.into());
        f.next_id += 1;
        Ok((format!("jit-for-{name}"), f.next_id))
    }

    fn remove_runner(&self, _: &Ctx, id: i64) -> Result<(), scaleset::Error> {
        let mut f = self.st();
        if f.unreachable > 0 {
            f.unreachable -= 1;
            return Err(scaleset::Error::new("failed to issue the request: dial tcp: lookup api.github.com: no such host"));
        }
        if f.running.contains(&id) {
            // As GitHub refused a runner it had given a job.
            let mut e = scaleset::Error::of(Kind::JobStillRunning);
            e.msg = format!(
                "bad request: request DELETE https://broker.actions.githubusercontent.com/rest/_apis/distributedtask/pools/0/agents/{id}?api-version=6.0-preview failed(status=\"400 Bad Request\"): {}: Runner is currently running a job and cannot be deleted.",
                e.msg
            );
            return Err(e);
        }
        f.removed.push(id);
        Ok(())
    }
}

/// Keeps each place's lock held, as the place runner process would, until
/// the test finishes that place's job.
#[derive(Default)]
pub struct FakeLauncher(pub Mutex<FakeLauncherState>);

#[derive(Default)]
pub struct FakeLauncherState {
    pub held: HashMap<String, File>,
    /// The folder holding every place's folder, set by new_harness.
    pub places: PathBuf,
    pub jits: HashMap<String, String>,
    pub stopped: Vec<String>,
}

impl FakeLauncher {
    pub fn new() -> Arc<FakeLauncher> {
        Arc::new(FakeLauncher::default())
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, FakeLauncherState> {
        self.0.lock().unwrap()
    }

    /// Ends the job in place id: its runner took a job, as the place runner
    /// records in place.json, then the runner process exits and the kernel
    /// drops the place's lock.
    pub fn finish(&self, id: &str) {
        self.end(id, true)
    }

    /// Ends place id's runner before any job reached it, as a virtual
    /// machine that never boots or a stopped spare runner does.
    pub fn give_up(&self, id: &str) {
        self.end(id, false)
    }

    fn end(&self, id: &str, started: bool) {
        let mut l = self.st();
        assert!(l.held.contains_key(id), "place {id} runs nothing");
        if started {
            let path = l.places.join(id).join("place.json");
            if let Ok(mut run) = config::read_json::<PlaceRun>(&path) {
                run.started = true;
                config::write_json(&path, &run).unwrap();
            }
        }
        l.held.remove(id);
    }
}

impl Launcher for FakeLauncher {
    fn launch(&self, p: &Place, _pool: &str, lock: &File, jit: &str) -> Result<(), String> {
        let held = lock.try_clone().map_err(|e| e.to_string())?;
        let mut l = self.st();
        l.held.insert(p.id.clone(), held);
        l.jits.insert(p.id.clone(), jit.into());
        Ok(())
    }

    fn launch_borrow(&self, p: &Place, busy: &File, id: &str) -> Result<(), String> {
        self.launch(p, "", busy, &format!("borrow:{id}"))
    }

    fn stop(&self, id: &str) -> Result<(), String> {
        self.st().stopped.push(id.into());
        Ok(())
    }
}

/// A log that keeps what it is told, for tests that read the coordinator's
/// log.
#[derive(Clone, Default)]
pub struct LogBuffer(pub Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

pub type Sets = HashMap<String, Arc<FakeSet>>;

// macOS: a child spawned on another thread briefly holds every open file, so a
// dropped place lock can linger (seen 524 in 20000). So a test that spawns runs
// while no harness lives. Not an RwLock: a test holds two harnesses at once.
// (spawning, harnesses alive)
static SPAWNS: (Mutex<(bool, usize)>, std::sync::Condvar) = (Mutex::new((false, 0)), std::sync::Condvar::new());

pub struct NoSpawns;

pub fn no_spawns() -> NoSpawns {
    let mut s = SPAWNS.1.wait_while(SPAWNS.0.lock().unwrap(), |s| s.0).unwrap();
    s.1 += 1;
    NoSpawns
}

impl Drop for NoSpawns {
    fn drop(&mut self) {
        SPAWNS.0.lock().unwrap().1 -= 1;
        SPAWNS.1.notify_all();
    }
}

pub struct Spawns;

/// Waits until no harness lives, and keeps new ones from starting until it
/// is dropped.
pub fn spawns() -> Spawns {
    let mut s = SPAWNS.1.wait_while(SPAWNS.0.lock().unwrap(), |s| s.0 || s.1 > 0).unwrap();
    s.0 = true;
    Spawns
}

impl Drop for Spawns {
    fn drop(&mut self) {
        SPAWNS.0.lock().unwrap().0 = false;
        SPAWNS.1.notify_all();
    }
}

pub struct Harness {
    pub cfg: Arc<Config>,
    pub svc: Arc<Service>,
    pub launcher: Arc<FakeLauncher>,
    pub sets: Sets,
    pub now: Arc<Mutex<Time>>,
    _events: Receiver<Event>,
    _no_spawns: NoSpawns,
}

/// One machine with two small Docker places for ru-linux jobs and a large
/// KVM place, listed last, for ru-large jobs and, when the small ones are
/// busy, ru-linux jobs.
pub fn one_box(dir: &Path) -> Config {
    let place = |id: &str, provider: &str, pools: &[&str], cores, memory_mb| Place {
        id: id.into(),
        provider: provider.into(),
        pools: pools.iter().map(|s| s.to_string()).collect(),
        cores,
        memory_mb,
        ..Default::default()
    };
    let mut large = place("box-large", config::PROVIDER_KVM, &["ru-large", "ru-linux"], 4, 8192);
    large.image = "/var/lib/runner-up/noble-server-cloudimg-amd64.img".into();
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        github: config::GitHub { repos: strings(&["octo-org/app", "octo-user/tool", "octo-user/wallet"]), ..Default::default() },
        pools: vec![pool("ru-linux"), pool("ru-large")],
        machines: vec![config::Machine {
            name: "box".into(),
            ssh: strings(&["ci@box"]),
            places: vec![
                place("box-1", config::PROVIDER_DOCKER, &["ru-linux"], 2, 4096),
                place("box-2", config::PROVIDER_DOCKER, &["ru-linux"], 2, 4096),
                large,
            ],
            ..Default::default()
        }],
        ..Default::default()
    }
}

pub fn pool(label: &str) -> config::Pool {
    config::Pool { label: label.into(), ..Default::default() }
}

pub fn strings(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}

pub fn new_harness(mut cfg: Config, launcher: Arc<FakeLauncher>, mut sets: Sets) -> Harness {
    let no_spawns = no_spawns();
    cfg.defaults();
    cfg.validate().unwrap();
    let cfg = Arc::new(cfg);
    let (events, rx) = channel();
    let svc = Arc::new(Service::new(cfg.clone(), Logger::discard(), launcher.clone(), events).unwrap());
    launcher.st().places = cfg.place_dir("x").parent().unwrap().to_path_buf();
    let now = Arc::new(Mutex::new(Time::default()));
    let clock = now.clone();
    svc.lock().now = Arc::new(move || *clock.lock().unwrap());
    // The scale sets the coordinator opens (Sessions::match_setup).
    for target in cfg.targets() {
        for label in cfg.labels() {
            let set = sets.entry(set_key(&target, &label)).or_default().clone();
            svc.add_set(&target, &label, set);
        }
    }
    Harness { cfg, svc, launcher, sets, now, _events: rx, _no_spawns: no_spawns }
}

impl Harness {
    pub fn set_now(&self, t: Time) {
        *self.now.lock().unwrap() = t;
    }

    pub fn now(&self) -> Time {
        *self.now.lock().unwrap()
    }

    pub fn advance(&self, d: Duration) {
        let t = self.now().add(d);
        self.set_now(t);
    }

    pub fn set(&self, target: &str, pool: &str) -> std::sync::MutexGuard<'_, FakeSetState> {
        self.sets[&set_key(target, pool)].st()
    }

    pub fn waiting(&self) -> usize {
        self.svc.lock().waiting.len()
    }

    pub fn feed(&self, f: &Fixture) {
        self.feed_range(f, 0, f.messages.len())
    }

    /// Hands the service messages [from, to) of f, each at its own arrival
    /// time where the fixture has one.
    pub fn feed_range(&self, f: &Fixture, from: usize, to: usize) {
        for m in &f.messages[from..to] {
            if !m.at.is_zero() {
                self.set_now(m.at);
            }
            self.handle(&m.repo, &m.label, &m.message);
        }
    }

    /// Hands the service one message from a scale set's session, then gives
    /// up on the spare runners a cancellation left and places what now fits,
    /// as the serve loop does when handle wakes it.
    pub fn handle(&self, target: &str, pool: &str, msg: &Message) {
        let ctx = Ctx::background();
        self.svc.handle(target, pool, Some(msg));
        {
            let _pass = self.svc.pass.lock().unwrap();
            self.svc.give_up_idle(&ctx, true);
        }
        self.svc.place(&ctx);
    }

    pub fn tidy(&self) {
        self.svc.tidy(&Ctx::background());
    }

    pub fn place(&self) {
        self.svc.place(&Ctx::background());
    }

    pub fn run(&self, id: &str) -> PlaceRun {
        config::read_json(&self.cfg.place_dir(id).join("place.json")).unwrap()
    }

    /// Which job each busy place runs, by place.json.
    pub fn placed(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for p in self.cfg.places() {
            if !lock::held_by(&self.cfg.place_dir(&p.id).join("busy.lock")).unwrap_or(false) {
                continue;
            }
            let run = self.run(&p.id);
            out.insert(p.id.clone(), format!("{} {}", run.repo, run.job_name));
        }
        out
    }

    pub fn want(&self, want: &[(&str, &str)]) {
        let got = self.placed();
        let want: HashMap<String, String> = want.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(got, want, "places run (left), want (right)");
    }

    /// Ends a place's job the way GitHub reports it and the service then
    /// places what fits.
    pub fn finish_and_place(&self, id: &str) {
        self.launcher.finish(id);
        self.advance(Duration::from_secs(60));
        self.tidy();
        self.place();
    }
}

/// Holds a place's lock, as another job's runner would.
pub fn occupy(cfg: &Config, id: &str) -> File {
    std::fs::create_dir_all(cfg.place_dir(id)).unwrap();
    let f = lock::busy(&cfg.place_dir(id).join("busy.lock")).unwrap().expect("could not occupy");
    let run = PlaceRun { repo: "octo-user/tool".into(), job_name: "suite".into(), started: true, ..Default::default() };
    config::write_json(&cfg.place_dir(id).join("place.json"), &run).unwrap();
    f
}

/// Runs f on its own thread and fails the test if it has not returned within
/// a minute, naming what it waited for.
pub fn wait_for(what: &str, f: impl FnOnce() + Send + 'static) {
    let (done, rx) = channel();
    std::thread::spawn(move || {
        f();
        let _ = done.send(());
    });
    rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

pub fn mark_started(cfg: &Config, id: &str) {
    let path = cfg.place_dir(id).join("place.json");
    let mut run: PlaceRun = config::read_json(&path).unwrap();
    run.started = true;
    config::write_json(&path, &run).unwrap();
}
