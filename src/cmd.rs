//! runner-up runs GitHub Actions jobs on your own machines through GitHub's
//! runner scale sets, one fresh container or virtual machine per job, or on
//! the machine itself.
//!
//! One coordinator holds a scale set session per repository or organization
//! and pool, and hears of every waiting job. Jobs wait at GitHub without a
//! runner; the first job waiting only for resources keeps room in the plan.
//! When a place is free, the coordinator picks the job its order rules pick
//! (plan.rs), and only then acquires it and starts a runner for it in that
//! place: it runs the place's machine's agent over SSH (coordinator.rs),
//! which makes the container or virtual machine, runs the one job and removes
//! it (agent.rs). The job's reservation of its machine's cores and memory
//! counts while its runner lives, within the machine's budget less the room
//! kept for its owner.
//!
//! This module parses the command line and starts each of those parts.

mod init;
#[cfg(test)]
pub(crate) mod tests;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crate::agent;
use crate::cli;
use crate::config::{self, Config, Machine, Share};
use crate::coordinator::{self, ClientFactory, GithubApi, ServeOptions};
use crate::ctx::Ctx;
use crate::go::flag::FlagSet;
use crate::go::slog::Logger;
use crate::plan;
use crate::proc;
use crate::signals;
use crate::version::version;

fn usage() -> String {
    format!(
        "usage: runner-up <command> [flags]

Setting up:
  init          start a setup: what to serve and the pool label
  machine add   add a machine, reached over SSH, and its places
  check-config  check the setup and say how each machine is shared

Running:
  serve         run the coordinator
  update        fetch the setup's release for a supervisor to start (program/current)
  status        show each place and the waiting jobs
  usage         show finished jobs' sampled use beside their pool sizes
  borrow        borrow a tart place: --pool LABEL --label TEXT --minutes N -- COMMAND
  drain         pause places until named jobs ran there:
                --place ID --job NAME [--repo OWNER/NAME] --label TEXT --minutes N

On each machine (run by the coordinator):
  agent probe            show the machine's cores, memory and providers
  agent vms              list the macOS virtual machines running here
  agent run|clean        run one job in a place, or clean up after one
  agent measure          run a tart guest's runner and sample its use
  agent service-version  show the version of the Windows service here
  agent update-service   hand the Windows service here a signed release
  fork-guard             the runner's job-started hook
  version                show this program's version

On a Windows machine:
  setup         set it up for places, as an administrator, once
  service       the Windows service that setup installs

Every command but agent, fork-guard, setup, service and version takes --config FILE;
the default is {}
",
        config::default_config_path()
    )
}

fn about_command(name: &str) -> &'static str {
    match name {
        "fork-guard" => "the runner's job-started hook: it stops a job from another repository's pull request",
        "service" => "the Windows service that runner-up setup installs; Windows' service manager starts it",
        _ => "show this program's version",
    }
}

/// Runs the runner-up command line args, the program's arguments after its
/// name, and returns its exit code.
pub fn main(args: &[String]) -> i32 {
    let Some(name) = args.first().map(String::as_str) else {
        eprint!("{}", usage());
        return 2;
    };
    let rest = &args[1..];
    // fork-guard, service and version take nothing: they say what they are
    // on -h and refuse anything else.
    if matches!(name, "fork-guard" | "service" | "version") {
        let mut fs = FlagSet::new(name);
        fs.usage = Some(Box::new(|fs, w| {
            let _ = writeln!(w, "usage: runner-up {}\n{}", fs.name, about_command(&fs.name));
        }));
        if let Some(code) = cli::parse(&mut fs, rest, false) {
            return code;
        }
    }
    match name {
        "agent" => return agent::agent_main(rest),
        "fork-guard" => return agent::run_fork_guard(&mut std::io::stdout(), &mut std::io::stderr()),
        "setup" => return agent::setup_main(rest),
        "service" => {
            return match agent::run_service() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            };
        }
        "version" => {
            println!("{}", version());
            return 0;
        }
        "serve" | "update" | "status" | "usage" | "check-config" | "run-place" | "run-borrow" | "borrow" | "drain" | "init" | "machine" => {
            if let Err(e) = proc::coordinator_runs() {
                eprintln!("{e}");
                return 2;
            }
        }
        "-h" | "--help" | "help" => {
            print!("{}", usage());
            return 0;
        }
        _ => {
            eprintln!("unknown command {}", crate::go::quote(name));
            eprint!("{}", usage());
            return 2;
        }
    }
    match name {
        "borrow" => return coordinator::borrow_main(rest),
        "drain" => return coordinator::drain_main(rest),
        "init" => return init::init_main(rest),
        "machine" => {
            if cli::help(rest) {
                println!("{}", init::MACHINE_ADD_USAGE);
                return 0;
            }
            if rest.first().map(String::as_str) != Some("add") {
                eprintln!("{}", init::MACHINE_ADD_USAGE);
                return 2;
            }
            return init::machine_add_main(&rest[1..]);
        }
        _ => {}
    }
    let mut fs = FlagSet::new(name);
    fs.string("config", &config::default_config_path(), "setup file");
    // Place runners and borrows are started by the coordinator, never by a
    // person; only they take these.
    match name {
        "run-place" => {
            fs.string("place", "", "place to run");
            fs.string("pool", "", "pool of the job the place runs");
        }
        "run-borrow" => {
            fs.string("place", "", "place to run the borrow in");
            fs.string("borrow", "", "borrow request");
        }
        _ => {}
    }
    if let Some(code) = cli::parse(&mut fs, rest, false) {
        return code;
    }
    let config_path = fs.get_str("config");
    let cfg = match config::load_config(&config_path).and_then(|c| plan::parse_order(&c).map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (out, err) = (&mut std::io::stdout(), &mut std::io::stderr());
    match name {
        "serve" => serve(cfg, &config_path),
        "update" => update_program(&cfg, &config_path, out, err),
        "run-borrow" => coordinator::run_borrow(&cfg, &fs.get_str("place"), &fs.get_str("borrow")),
        "run-place" => coordinator::run_place(&cfg, &fs.get_str("place"), &fs.get_str("pool")),
        "status" => coordinator::status(cfg, out),
        "usage" => usage_report(&cfg, out, err),
        _ => check_config(&cfg, out),
    }
}

fn serve(cfg: Config, config_path: &str) -> i32 {
    let self_path = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let token = coordinator::gh_token(cfg.gh());
    let api = Arc::new(GithubApi::new(token.clone()));
    serve_with(cfg, config_path, &self_path, coordinator::gh_clients(token), Some(api), None)
}

/// Runs the coordinator (coordinator::serve) until a stop signal. It lets a
/// test serve a local stand-in for GitHub's Actions service, with no sign-in
/// and no reads of GitHub's API. self_path is the program place runners run
/// as.
fn serve_with(
    cfg: Config,
    config_path: &str,
    self_path: &str,
    clients: ClientFactory,
    api: Option<Arc<GithubApi>>,
    passed: Option<Box<dyn Fn() + Send>>,
) -> i32 {
    let ctx = signals::notify_stop(&Ctx::background());
    let o = ServeOptions { config_path: config_path.into(), self_path: self_path.into(), clients, api, log: Logger::stdout(), passed };
    coordinator::serve(&ctx, cfg, o)
}

/// `runner-up update`: fetches and checks the setup's release and points
/// program/current under the state folder at it, for a supervisor to start,
/// as a running coordinator would.
fn update_program(cfg: &Config, config_path: &str, w: &mut dyn Write, errw: &mut dyn Write) -> i32 {
    let Some(rel) = &cfg.release else {
        let _ = writeln!(errw, "the setup names no release; add one (see the README's Updating runner-up)");
        return 2;
    };
    let self_path = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            let _ = writeln!(errw, "{e}");
            return 1;
        }
    };
    let p = coordinator::Program::new(std::path::Path::new(&cfg.state_dir), config_path, &self_path, version(), Logger::discard());
    match p.ready(&Ctx::detached_timeout(Duration::from_secs(15 * 60)), rel) {
        Ok(current) => {
            let _ = writeln!(
                w,
                "runner-up {} is ready; a supervisor starts it as {} serve --config {config_path}",
                rel.version,
                current.display()
            );
            0
        }
        Err(e) => {
            let _ = writeln!(errw, "{e}");
            1
        }
    }
}

/// Says the setup is valid and how each machine is shared.
fn check_config(cfg: &Config, w: &mut dyn Write) -> i32 {
    let _ = writeln!(
        w,
        "ok: {} targets, {} pools, {} machines, {} places",
        cfg.targets().len(),
        cfg.pools.len(),
        cfg.machines.len(),
        cfg.places().len()
    );
    for m in &cfg.machines {
        let _ = writeln!(w, "{}", machine_line(m));
    }
    for p in &cfg.pools {
        if p.cores > 0 {
            let _ = writeln!(w, "pool {}: each job reserves {} cores, {} MB; overrides place sizes", p.label, p.cores, p.memory_mb);
        }
    }
    0
}

fn usage_report(cfg: &Config, w: &mut dyn Write, errw: &mut dyn Write) -> i32 {
    let records = match coordinator::read_usage(cfg) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(errw, "could not read resource readings: {e}");
            return 1;
        }
    };
    let _ = writeln!(w, "Sampled peaks, not guaranteed maxima; CPU is cores, memory is MB.");
    let _ = writeln!(w, "process-group: a host job's processes, memory as summed RSS; it can miss detached work.");
    let _ = writeln!(w, "guest: a whole tart guest, read from its kernel.");
    let _ = writeln!(w, "A guest value above the guest's size cannot be its use and is not shown. Allow headroom before changing a size.");
    for p in &cfg.pools {
        if p.cores > 0 {
            let _ = writeln!(w, "pool {}: configured {} cores, {} MB", p.label, p.cores, p.memory_mb);
        } else {
            let _ = writeln!(w, "pool {}: uses each place's size", p.label);
        }
        let mut n = 0;
        for r in records.iter().filter(|r| r.pool == p.label) {
            n += 1;
            let res = r.reservation;
            let _ = write!(
                w,
                "  {} {}/{} reserved {} cores, {} MB: ",
                r.started_at.local_rfc3339(),
                r.machine,
                r.provider,
                res.cores,
                res.memory_mb
            );
            let Some(u) = &r.usage else {
                let _ = writeln!(w, "readings unavailable");
                continue;
            };
            let (mut cores, mut memory) = (format!("{:.2} cores", u.peak_cores), format!("{:.1} MB", u.peak_memory_mb));
            // tart sizes a guest to its reservation, so a guest cannot use
            // more; a reading above it is not a measurement of the guest.
            let mut above = false;
            if u.scope == "guest" {
                if res.cores > 0 && u.peak_cores > res.cores as f64 {
                    (cores, above) = ("cores unavailable".into(), true);
                }
                if res.memory_mb > 0 && u.peak_memory_mb > res.memory_mb as f64 {
                    (memory, above) = ("memory unavailable".into(), true);
                }
            }
            let _ = write!(w, "peak {cores}, {memory} ({}, {} samples, {:.1}s", u.scope, u.samples, u.seconds);
            if u.incomplete {
                let _ = write!(w, ", incomplete");
            }
            if above {
                let _ = write!(w, "; read above the guest's size");
            }
            let _ = writeln!(w, ")");
        }
        if n == 0 {
            let _ = writeln!(w, "  no finished-job readings yet");
        }
    }
    0
}

/// Says how a machine's budget is shared out, and whether all its places fit
/// at once.
pub(super) fn machine_line(m: &Machine) -> String {
    let where_ = if m.local() { "this machine".to_string() } else { m.ssh.join(" ") };
    let mut shares = Vec::new();
    let mut total = Share::default();
    for p in &m.places {
        shares.push(format!("{} {} {} cores {} MB", p.id, p.provider, p.cores, p.memory_mb));
        total.cores += p.cores;
        total.memory_mb += p.memory_mb;
    }
    let line = format!("{} ({where_}): {}", m.name, shares.join(", "));
    if m.cores == 0 && m.memory_mb == 0 {
        return line + "; nothing counted";
    }
    let b = m.budget();
    let mut fit = "all at once".to_string();
    if b.cores > 0 && total.cores > b.cores || b.memory_mb > 0 && total.memory_mb > b.memory_mb {
        fit = "not all at once".into();
    }
    let tart = m.places.iter().filter(|p| p.provider == config::PROVIDER_TART).count();
    if tart > agent::MACOS_VM_LIMIT {
        fit += &format!("; its {tart} tart places take turns, since macOS runs at most {} VMs at once", agent::MACOS_VM_LIMIT);
    }
    let mut counted = Vec::new();
    if m.cores > 0 {
        counted.push(format!("{} of {} cores", b.cores, m.cores));
    }
    if m.memory_mb > 0 {
        counted.push(format!("{} of {} MB", b.memory_mb, m.memory_mb));
    }
    format!("{line}; places may reserve {}, {fit}", counted.join(" and "))
}
