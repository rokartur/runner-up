use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::github::{ClientFactory, Sessions};
use super::github_api::GithubApi;
use super::place::{Launcher, ProcLauncher, agent_command};
use super::program::{Program, Restart};
use super::setup::{SETUP_POLL, SETUP_QUIET, SetupRead, check_setup, watch_setup};
use super::update::Updater;
use super::vmlimit::read_vms;
use super::{Event, Service};
use crate::config::Config;
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::version::version;
use crate::{agent, lock, plan, proc};

/// Leaves room inside systemd's default 90-second stop allowance. Session
/// acknowledgements and closes each have their own shorter allowance.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// What serve needs beside the setup.
pub struct ServeOptions {
    /// The setup file, applied in place as it changes (setup.rs).
    pub config_path: String,
    /// The program place runners run as.
    pub self_path: String,
    /// Makes each scale set's client; api reads GitHub's API, or is None to
    /// read nothing there.
    pub clients: ClientFactory,
    pub api: Option<Arc<GithubApi>>,
    pub log: Logger,
    /// Called after each pass of the serve loop, so a test can wait for the
    /// passes after an event rather than for a time.
    pub passed: Option<Box<dyn Fn() + Send>>,
}

/// Runs the coordinator under cfg until ctx ends, and returns the program's
/// exit code. It opens a scale set session for each repository or
/// organization and pool, and runs one loop that alone places jobs and tidies
/// places, with the serving context: at every tick, message, ended place
/// runner, setup change and agent update. When the setup names a release, it
/// runs that version of runner-up and copies it to every machine
/// (program.rs, update.rs); it restarts as a new version by replacing itself,
/// and returns only if that fails.
pub fn serve(ctx: &Ctx, cfg: Config, o: ServeOptions) -> i32 {
    let log = o.log.clone();
    let program = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            log.error("could not find runner-up's own program", &[("err", &e)]);
            return 1;
        }
    };
    let (events, rx) = mpsc::channel();
    let up = Arc::new(Updater::new(
        Program::new(std::path::Path::new(&cfg.state_dir), &o.config_path, &program, version(), log.clone()),
        &o.self_path,
        log.clone(),
        events.clone(),
    ));
    // A new version that stopped on trial goes back to the version before.
    if let Some(r) = up.program.begin() {
        return restart(&up.program, &r, &log);
    }
    let mut base = cfg;
    let mut cfg = Arc::new(up.setup(&base));
    let launcher: Arc<dyn Launcher> = Arc::new(ProcLauncher::new(cfg.clone(), &o.self_path, log.clone(), events.clone()));
    let svc = match Service::new(cfg.clone(), log.clone(), launcher, events.clone()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log.error("service", &[("err", &e)]);
            return 1;
        }
    };
    let _ = up.service.set(svc.clone());
    // Each reader is used only while the setup needs it: label and workflow
    // reads while an order rule does, VM reads of the Macs with tart places.
    {
        let mut st = svc.lock();
        if let Some(api) = &o.api {
            let (a, b, c) = (api.clone(), api.clone(), api.clone());
            st.run_reader = Some(Arc::new(move |ctx, repo, id| a.run_closed(ctx, repo, id)));
            st.label_reader = Some(Arc::new(move |ctx, repo| b.pull_labels(ctx, repo)));
            st.workflow_reader = Some(Arc::new(move |ctx, repo, id| c.run_workflow(ctx, repo, id)));
        }
        st.vm_reader = Some(Arc::new(read_vms));
    }
    // serving ends when ctx does, or to restart as a new version.
    let serving = ctx.with_cancel();
    {
        let (serving, events) = (serving.clone(), events.clone());
        std::thread::spawn(move || {
            serving.wait();
            let _ = events.send(Event::Wake);
        });
    }
    clean_leftovers(&serving, &cfg, &log);
    log.info(
        "runner-up starting",
        &[
            ("version", &version()),
            ("targets", &cfg.targets().len()),
            ("pools", &cfg.pools.len()),
            ("machines", &cfg.machines.len()),
            ("places", &cfg.places().len()),
        ],
    );
    let mut sessions = Sessions::new(&serving, svc.clone(), log.clone(), o.clients.clone());
    sessions.match_setup(&cfg);
    {
        let events = events.clone();
        watch_setup(&serving, o.config_path.as_ref(), SETUP_POLL, SETUP_QUIET, move |r| events.send(Event::Setup(r)).is_ok());
    }
    up.tick(&serving, &base);
    let mut next: Option<Restart> = None;
    let mut refused = false;
    let mut tick = cfg.seconds(cfg.tick_seconds);
    let mut next_tick = Instant::now() + tick;
    loop {
        if serving.is_done() {
            // Running jobs keep their places: their place runners are in
            // their own sessions and hold their own locks. Waiting jobs stay
            // at GitHub.
            let Some(r) = next else {
                log.info("runner-up stopping; running jobs carry on", &[]);
                up.program.stopped();
                return wait_for_sessions(&sessions, SHUTDOWN_TIMEOUT, &log);
            };
            log.info("runner-up restarting as a new version; running jobs carry on", &[("version", &r.version)]);
            wait_for_sessions(&sessions, SHUTDOWN_TIMEOUT, &log);
            return restart(&up.program, &r, &log);
        }
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                next_tick = Instant::now() + tick;
                up.program.keep();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("serve holds a sender"),
            Ok(Event::Ready(r)) => {
                next = Some(r);
                serving.cancel();
                continue;
            }
            Ok(Event::Wake) => svc.woken(),
            Ok(Event::Exited(_)) => {}
            Ok(Event::Changed) => {
                // A machine's agent changed: place runners start with it from now.
                let res = plan::parse_order(&base).and_then(|rules| svc.reload(Arc::new(up.setup(&base)), rules));
                if let Err(e) = res {
                    log.error("could not use a machine's new agent", &[("err", &e)]);
                }
                cfg = svc.setup();
            }
            Ok(Event::Setup(read)) => {
                match apply_setup(&svc, &mut sessions, &up, read) {
                    Err(e) => {
                        log.error("the setup file was not applied; keeping the running setup", &[("err", &e)]);
                        refused = true;
                        continue;
                    }
                    Ok(Some(next_base)) => {
                        if next_base.tick_seconds != cfg.tick_seconds {
                            tick = next_base.seconds(next_base.tick_seconds);
                            next_tick = Instant::now() + tick;
                        }
                        base = next_base;
                        cfg = svc.setup();
                    }
                    Ok(None) if refused => log.info("the setup file holds the running setup again", &[]),
                    Ok(None) => {}
                }
                refused = false;
            }
        }
        svc.tidy(&serving);
        svc.place(&serving);
        up.tick(&serving, &base);
        if let Some(passed) = &o.passed {
            passed();
        }
    }
}

/// Replaces the coordinator with r's program. It returns only when that could
/// not be done, to stop, and let the coordinator's supervisor start it again.
fn restart(p: &Program, r: &Restart, log: &Logger) -> i32 {
    log.info("runner-up runs another version", &[("version", &r.version), ("path", &r.path.display())]);
    let err = p.exec(r);
    log.error(
        "could not run the other version; runner-up stops for its supervisor to start it again",
        &[("version", &r.version), ("err", &err)],
    );
    1
}

/// Checks what the setup file holds, as check-config does, and applies it in
/// place, with each machine's agent the release's (Updater). It returns the
/// setup file's setup when it applied it, None when the file holds the
/// running setup, or why it kept the running setup.
fn apply_setup(svc: &Service, sessions: &mut Sessions, up: &Updater, read: SetupRead) -> Result<Option<Config>, String> {
    let (next, rules) = check_setup(&read?)?;
    if !svc.reload(Arc::new(up.setup(&next)), rules)? {
        return Ok(None);
    }
    sessions.match_setup(&next);
    Ok(Some(next))
}

/// Joins the scale set sessions, including their closes, but never lets an
/// unreachable endpoint hold shutdown indefinitely.
fn wait_for_sessions(sessions: &Sessions, allowance: Duration, log: &Logger) -> i32 {
    if sessions.wait_done(allowance) {
        log.info("scale set listeners stopped", &[]);
        return 0;
    }
    log.error(
        "timed out waiting for scale set listeners; sessions may remain active",
        &[("allowance", &crate::go::duration::format(allowance))],
    );
    1
}

/// Asks each free place's agent to remove anything left of the place: a
/// place runner or agent killed outright, or a machine that went down
/// mid-job, cannot clean up after itself. Busy places keep theirs. Every
/// agent's own run cleans first too, so a machine unreachable now is cleaned
/// at its next job.
fn clean_leftovers(ctx: &Ctx, cfg: &Config, log: &Logger) {
    std::thread::scope(|s| {
        for p in cfg.places() {
            if lock::held_by(&cfg.place_dir(&p.id).join("busy.lock")).unwrap_or(true) {
                continue;
            }
            s.spawn(move || {
                let c = ctx.with_timeout(Duration::from_secs(60));
                let args: Vec<String> = ["agent", "clean", "--provider", &p.provider, "--place", &p.id].map(String::from).into();
                let argv = agent_command(&cfg.machine(&p.machine), &args);
                let (out, res) = proc::combined_output(&c, Command::new(&argv[0]).args(&argv[1..]));
                if let Err(e) = res {
                    log.warn(
                        "could not clean a free place; its next job cleans it",
                        &[("place", &p.id), ("machine", &p.machine), ("err", &agent::first_line(&String::from_utf8_lossy(&out), &e))],
                    );
                }
            });
        }
    });
}
