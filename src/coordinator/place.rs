// The place runner is this same program run as `runner-up run-place`. The
// coordinator starts it in its own session, so it outlives a coordinator
// restart, with the place's locked busy.lock as file descriptor 3, the setup
// the coordinator runs (write_setup) and the pool of the job it was started
// for as arguments, and the runner's one-time registration on standard
// input, never in an argument or an environment. It runs the place's
// machine's agent, over SSH or directly on the coordinator's own machine,
// with the pool's image, hands it the registration, and holds the agent's
// input open until the runner ends. The agent makes a fresh place, runs one
// job there and removes the place whatever happened; when its input closes,
// because this process ended or gave up on the runner, it stops the job and
// cleans up.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::reach::SSH_FAILED;
use super::setup::write_setup;
use crate::agent;
use crate::config::{self, Config, Machine, Place, Share};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::go::time::Time;
use crate::{lock, proc, quote};

/// A place's place.json for a job or borrow, written by the coordinator just
/// before launch. For jobs, the place's runner updates it as the job starts.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlaceRun {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub borrow_id: String,
    #[serde(default)]
    pub runner_name: String,
    #[serde(default)]
    pub runner_id: i64,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub request_id: i64,
    #[serde(default)]
    pub job_name: String,
    #[serde(default)]
    pub launched_at: Time,
    #[serde(default)]
    pub started: bool,
    /// The display name of the job that reached the runner, as the runner
    /// printed it. GitHub may give a scale set's runner another of the set's
    /// jobs than job_name, the one it was started for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ran_job: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub pid: i64,
    /// Frozen at launch, so restarts and setup edits cannot shrink a running
    /// job's claim on the machine. Go's omitempty never drops a struct.
    #[serde(default)]
    pub reservation: Share,
    /// The place runner could not reach its machine's agent, or lost it
    /// (reach.rs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreachable: bool,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// Starts a place's runner process for a job of pool. It is handed the
/// place's locked busy.lock and must pass it to the process it starts, which
/// keeps the place busy for as long as that process lives.
pub trait Launcher: Send + Sync {
    fn launch(&self, place: &Place, pool: &str, lock: &File, jit: &str) -> Result<(), String>;
    fn stop(&self, place_id: &str) -> Result<(), String>;
    /// Starts the place runners launched from now on with cfg.
    fn use_setup(&self, _cfg: &Arc<Config>) -> Result<(), String> {
        Ok(())
    }
    /// Starts a borrow's supervisor in place p (borrow.rs); only the real
    /// launcher can.
    #[cfg(unix)]
    fn launch_borrow(&self, _p: &Place, _busy: &File, _id: &str) -> Result<(), String> {
        Err("this launcher starts no borrow".into())
    }
}

/// The real Launcher.
pub struct ProcLauncher {
    self_path: String,
    log: Logger,
    exited: Sender<super::Event>,
    /// The setup place runners are started with: the coordinator's, kept at
    /// its path (write_setup).
    setup: Mutex<(Arc<Config>, String)>,
}

impl ProcLauncher {
    /// Starts place runners with self_path, this program, under cfg; the id
    /// of each place whose runner ends goes to exited.
    pub fn new(cfg: Arc<Config>, self_path: &str, log: Logger, exited: Sender<super::Event>) -> ProcLauncher {
        ProcLauncher { self_path: self_path.into(), log, exited, setup: Mutex::new((cfg, String::new())) }
    }

    fn current_setup(&self) -> Result<(Arc<Config>, String), String> {
        let s = self.setup.lock().unwrap();
        if s.1.is_empty() {
            return Err("no setup kept for place runners".into());
        }
        Ok(s.clone())
    }

    /// Runs this program as command, a place's detached supervisor: a place
    /// runner or a borrow supervisor. It holds place p's locked busy.lock as
    /// file descriptor 3 and reads the setup the coordinator runs, and its
    /// output goes to log in the logs folder.
    pub fn start(&self, p: &Place, busy: &File, log: &str, stdin: &[u8], command: &str, args: &[&str]) -> Result<(), String> {
        let (cfg, setup_path) = self.current_setup()?;
        let log_path = cfg.logs_dir().join(log);
        let out = open_log(&log_path).map_err(|e| format!("open {}: {e}", log_path.display()))?;
        let mut cmd = Command::new(&self.self_path);
        cmd.args([command, "--config", &setup_path, "--place", &p.id]).args(args);
        cmd.stdin(Stdio::piped()).stdout(out.try_clone().map_err(|e| e.to_string())?).stderr(out);
        proc::pass_as_fd3(&mut cmd, busy);
        proc::detach(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        drop(cmd);
        if let Some(mut input) = child.stdin.take() {
            let stdin = stdin.to_vec();
            std::thread::spawn(move || input.write_all(&stdin));
        }
        let pid = child.id() as i64;
        if let Err(e) = update_place_run(&cfg, &p.id, |run| run.pid = pid) {
            self.log
                .warn("could not record the place's supervisor process; stopping it waits for it to end", &[("place", &p.id), ("err", &e)]);
        }
        let (log, exited, id) = (self.log.clone(), self.exited.clone(), p.id.clone());
        std::thread::spawn(move || {
            let status = child.wait();
            log.info("place finished", &[("place", &id), ("exit", &agent::exit_code(&status))]);
            let _ = exited.send(super::Event::Exited);
        });
        Ok(())
    }
}

fn open_log(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

impl Launcher for ProcLauncher {
    /// Keeps cfg for the place runners started from now on to read.
    fn use_setup(&self, cfg: &Arc<Config>) -> Result<(), String> {
        let path = write_setup(cfg).map_err(|e| format!("could not keep the setup for place runners: {e}"))?;
        *self.setup.lock().unwrap() = (cfg.clone(), path);
        Ok(())
    }

    fn launch(&self, p: &Place, pool: &str, lock: &File, jit: &str) -> Result<(), String> {
        let log = format!("{}-{}.log", p.id, chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        self.start(p, lock, &log, format!("{jit}\n").as_bytes(), "run-place", &["--pool", pool])
    }

    #[cfg(unix)]
    fn launch_borrow(&self, p: &Place, busy: &File, id: &str) -> Result<(), String> {
        self.start(p, busy, &format!("borrow-{id}.log"), &[], "run-borrow", &["--borrow", id])
    }

    /// Ends a place's runner; the place runner closes the agent's input and
    /// the agent cleans up. After a coordinator restart the process is no
    /// longer a child, so its pid comes from place.json, and only while the
    /// place's lock is still held.
    fn stop(&self, place_id: &str) -> Result<(), String> {
        let cfg = self.setup.lock().unwrap().0.clone();
        let run: PlaceRun = config::read_json(&cfg.place_dir(place_id).join("place.json")).map_err(|e| e.to_string())?;
        let held = lock::held_by(&cfg.place_dir(place_id).join("busy.lock"));
        if !matches!(held, Ok(true)) || run.pid <= 1 {
            return Err(format!("place {place_id} has no live runner"));
        }
        proc::terminate(run.pid as u32).map_err(|e| e.to_string())
    }
}

/// The command line that runs the agent on machine m with args: the agent
/// itself on the coordinator's own machine, or ssh to it. ssh joins its
/// command's words with spaces for the far side's shell, so every word is
/// quoted for that shell: a POSIX shell, or on Windows cmd or PowerShell,
/// whichever the machine's SSH server starts.
pub fn agent_command(m: &Machine, args: &[String]) -> Vec<String> {
    let mut argv = vec![m.agent_path().to_string()];
    argv.extend_from_slice(args);
    if m.local() {
        return argv;
    }
    let mut out = ssh_args(m);
    out.push(remote_line(m, &argv));
    out
}

/// ssh to the machine, before the remote command. ssh gives up on a machine
/// that does not answer within ten seconds, rather than wait the system's own
/// connect timeout, unless the machine's own options set ConnectTimeout; ssh
/// takes the first value it is given for an option.
pub fn ssh_args(m: &Machine) -> Vec<String> {
    let mut ssh: Vec<String> =
        ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4"].map(String::from).to_vec();
    if !m.ssh.iter().any(|w| w.to_lowercase().contains("connecttimeout")) {
        ssh.extend(["-o".to_string(), "ConnectTimeout=10".to_string()]);
    }
    ssh.extend(m.ssh.iter().cloned());
    ssh
}

/// Joins argv into the command line m's shell reads as argv.
pub fn remote_line(m: &Machine, argv: &[String]) -> String {
    let quote_word = if m.windows() { quote::win_quote } else { quote::sh_quote };
    argv.iter().map(|a| quote_word(a)).collect::<Vec<_>>().join(" ")
}

/// The agent's arguments for running one job of pool in place p, on the
/// pool's image there. None is secret: the registration travels on standard
/// input only.
pub fn run_args(cfg: &Config, p: &Place, pool: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "agent".into(),
        "run".into(),
        "--provider".into(),
        p.provider.clone(),
        "--place".into(),
        p.id.clone(),
        "--cores".into(),
        p.cores.to_string(),
        "--memory".into(),
        p.memory_mb.to_string(),
        "--boot-timeout".into(),
        cfg.boot_timeout_seconds.to_string(),
    ];
    let mut add = |a: &str, b: String| args.extend([a.to_string(), b]);
    let image = p.image_for(pool);
    if !image.is_empty() {
        add("--image", image.to_string());
    }
    if p.disk_gb > 0 {
        add("--disk-gb", p.disk_gb.to_string());
    }
    if p.provider == config::PROVIDER_TART {
        add("--guest-check", cfg.guest_check_seconds.to_string());
    }
    if !p.switch.is_empty() {
        add("--switch", p.switch.clone());
    }
    if !p.tool_cache.is_empty() {
        add("--tool-cache", p.tool_cache.clone());
    }
    if cfg.allow_fork_pull_requests {
        args.push("--allow-forks".into());
    }
    args
}

/// Place p sized for a job of pool as the coordinator reserved it: the size
/// it wrote into place.json before launch, or the pool's size when it wrote
/// none. A concurrent setup edit must not change the agent's limits after the
/// reservation was counted.
pub fn reserved_place(cfg: &Config, p: &Place, pool: &str) -> Place {
    let mut p = cfg.size_place(p, pool);
    if let Ok(run) = config::read_json::<PlaceRun>(&cfg.place_dir(&p.id).join("place.json"))
        && run.reservation.cores > 0
        && run.reservation.memory_mb > 0
    {
        p.cores = run.reservation.cores;
        p.memory_mb = run.reservation.memory_mb;
    }
    p
}

/// The place runner's main, for a job of pool.
pub fn run_place(cfg: &Config, place_id: &str, pool: &str) -> i32 {
    let log = Logger::stdout().with(&[("place", &place_id)]);
    let Some(p) = cfg.place(place_id) else {
        log.error("unknown place", &[]);
        return agent::EXIT_FAILED;
    };
    // The pool picks the job's image, so a pool the place does not serve
    // would run the job on an image chosen for none of its pools.
    if !p.pools.iter().any(|x| x == pool) {
        log.error("the place does not serve the pool", &[("pool", &pool)]);
        return agent::EXIT_FAILED;
    }
    let p = reserved_place(cfg, &p, pool);
    // fd 3 is the place's lock. Keep it open until this process ends, and out
    // of every process started from here.
    let Some(_lock) = proc::take_fd(3) else {
        log.error("no lock handed over", &[]);
        return agent::EXIT_FAILED;
    };
    let mut jit = String::new();
    let read = std::io::stdin().lock().read_line(&mut jit);
    let jit = jit.trim();
    if jit.is_empty() {
        let err = read.err().map(|e| e.to_string()).unwrap_or_else(|| "EOF".into());
        log.error("no runner registration on stdin", &[("err", &err)]);
        return agent::EXIT_FAILED;
    }
    let ctx = crate::signals::notify_stop(&Ctx::background());
    let argv = agent_command(&cfg.machine(&p.machine), &run_args(cfg, &p, pool));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let r = PlaceRunner { cfg, p, pool: pool.into(), log };
    let code = r.watch(&ctx, cmd, jit);
    ctx.cancel();
    code
}

struct PlaceRunner<'a> {
    cfg: &'a Config,
    p: Place,
    pool: String,
    log: Logger,
}

enum Seen {
    Started(String),
    Scanned,
}

impl PlaceRunner<'_> {
    /// Runs the agent, feeding it the registration and holding its input
    /// open, and watches its output for the job starting. A runner that runs
    /// past the job limit, or that the coordinator asks to stop, is stopped
    /// by closing the agent's input; one that does not stop then is killed.
    /// A runner no job reached in time is the coordinator's to stop: it
    /// removes the runner's registration first (Service::give_up), since
    /// GitHub can give the runner a job up to that moment.
    fn watch(&self, ctx: &Ctx, mut cmd: Command, jit: &str) -> i32 {
        let (reader, writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                self.log.error("agent did not start", &[("err", &e)]);
                return agent::EXIT_FAILED;
            }
        };
        let writer2 = match writer.try_clone() {
            Ok(w) => w,
            Err(e) => {
                self.log.error("agent did not start", &[("err", &e)]);
                return agent::EXIT_FAILED;
            }
        };
        cmd.stdin(Stdio::piped()).stdout(writer).stderr(writer2);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.log.error("agent did not start", &[("err", &e)]);
                return agent::EXIT_FAILED;
            }
        };
        // The pipe's write ends live on in cmd until it is dropped, and the
        // scan below ends only once every write end is closed.
        drop(cmd);
        let image = self.p.image_for(&self.pool);
        if image.is_empty() {
            self.log.info("agent started", &[("machine", &self.p.machine), ("provider", &self.p.provider), ("pool", &self.pool)]);
        } else {
            self.log.info(
                "agent started",
                &[("machine", &self.p.machine), ("provider", &self.p.provider), ("pool", &self.pool), ("image", &image)],
            );
        }
        let mut stdin = child.stdin.take();
        if let Some(input) = stdin.as_mut() {
            let _ = input.write_all(format!("{jit}\n").as_bytes());
        }
        let (tx, rx) = mpsc::channel();
        let usage_path = self.cfg.place_dir(&self.p.id).join("usage.json");
        let log = self.log.clone();
        std::thread::spawn(move || scan(reader, &usage_path, &log, &tx));

        let job_deadline = Instant::now() + self.cfg.seconds(self.cfg.max_job_seconds);
        let mut kill_at: Option<Instant> = None;
        let mut ended = false;
        let mut got_job = false;
        let mut scanned = false;
        let mut stop_asked = false;
        let mut end = |why: &str, stdin: &mut Option<std::process::ChildStdin>, kill_at: &mut Option<Instant>| {
            if ended {
                return;
            }
            ended = true;
            self.log.warn(why, &[]);
            *stdin = None;
            *kill_at = Some(Instant::now() + Duration::from_secs(120));
        };
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Seen::Started(job)) => {
                    if !got_job {
                        got_job = true;
                        self.mark_started(&job);
                    }
                    continue;
                }
                Ok(Seen::Scanned) => scanned = true,
                Err(_) => {}
            }
            // Retain the last reading before releasing busy.lock: wait for the
            // scan to end, which it does once the agent has ended.
            if scanned && let Ok(Some(status)) = child.try_wait() {
                let code = agent::exit_code(&Ok(status));
                self.log.info("agent ended", &[("exit", &code), ("job", &got_job)]);
                if code == SSH_FAILED && !self.cfg.machine(&self.p.machine).local() {
                    self.mark_unreachable();
                }
                return if got_job { agent::EXIT_DONE } else { agent::EXIT_UNSTARTED };
            }
            if Instant::now() >= job_deadline {
                end("the job ran past the limit; stopping it", &mut stdin, &mut kill_at);
            }
            if !stop_asked && ctx.is_done() {
                stop_asked = true;
                end("asked to stop; stopping the runner", &mut stdin, &mut kill_at);
            }
            if kill_at.is_some_and(|t| Instant::now() >= t) {
                self.log.warn("the agent did not stop; killing it", &[]);
                let _ = child.kill();
                kill_at = None;
            }
        }
    }

    /// Records that a job reached the runner, and which: the runner prints the
    /// job's display name.
    fn mark_started(&self, job: &str) {
        self.update(|run| {
            run.started = true;
            run.ran_job = job.to_string();
        });
        self.log.info("job reached the runner", &[("job", &job)]);
    }

    /// Records, before this process ends and lets go of the place, that ssh
    /// could not reach the machine's agent or lost it, for the coordinator to
    /// rest the machine (reach.rs).
    fn mark_unreachable(&self) {
        self.update(|run| run.unreachable = true);
        self.log.warn("ssh could not reach the machine's agent, or lost it", &[("machine", &self.p.machine)]);
    }

    fn update(&self, change: impl FnOnce(&mut PlaceRun)) {
        if let Err(e) = update_place_run(self.cfg, &self.p.id, change) {
            self.log.warn("could not record the run in place.json", &[("err", &e)]);
        }
    }
}

/// Copies the agent's output to stdout, keeps its resource readings and
/// reports the job starting, then reports the output's end.
fn scan(reader: impl Read, usage_path: &Path, log: &Logger, tx: &Sender<Seen>) {
    let mut r = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut stdout = std::io::stdout();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        let _ = writeln!(stdout, "{line}");
        if let Some(text) = line.strip_prefix(agent::USAGE_PREFIX)
            && let Some(usage) = agent::parse_usage(text)
            && let Err(e) = config::write_json(usage_path, &usage)
        {
            log.warn("could not keep resource readings", &[("err", &e)]);
        }
        if let Some((_, job)) = line.split_once("Running job:") {
            let _ = tx.send(Seen::Started(job.trim().to_string()));
        }
    }
    let _ = tx.send(Seen::Scanned);
}

// A place's place.json has two writers: the coordinator, which writes it
// before a launch, records the launched process and retires it once the
// place is free, and the place runner, which records its job starting or its
// machine out of reach. Each write takes the place's place.lock first, so one
// writer's read, change and write never loses the other's change.

/// Place id's place.json, if it could be read.
pub fn read_place_run(cfg: &Config, id: &str) -> Option<PlaceRun> {
    config::read_json(&cfg.place_dir(id).join("place.json")).ok()
}

/// Runs f holding place id's place.lock, with the path of its place.json.
fn with_place_run(cfg: &Config, id: &str, f: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let dir = cfg.place_dir(id);
    let _l = lock::wait(&dir.join("place.lock")).map_err(|e| e.to_string())?;
    f(&dir.join("place.json"))
}

/// Replaces place id's place.json with run.
pub fn write_place_run(cfg: &Config, id: &str, run: &PlaceRun) -> Result<(), String> {
    with_place_run(cfg, id, |path| config::write_json(path, run))
}

/// Applies change to place id's place.json.
pub fn update_place_run(cfg: &Config, id: &str, change: impl FnOnce(&mut PlaceRun)) -> Result<(), String> {
    with_place_run(cfg, id, |path| {
        let mut run: PlaceRun = config::read_json(path).map_err(|e| e.to_string())?;
        change(&mut run);
        config::write_json(path, &run)
    })
}

/// Keeps place id's place.json as its last-place.json. A missing place.json
/// is an io::ErrorKind::NotFound.
pub fn retire_place_run(cfg: &Config, id: &str) -> std::io::Result<()> {
    let dir = cfg.place_dir(id);
    let _l = lock::wait(&dir.join("place.lock"))?;
    std::fs::rename(dir.join("place.json"), dir.join("last-place.json"))
}
