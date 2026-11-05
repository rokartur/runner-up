//! The Windows job host: a SYSTEM service that makes a fresh standard account
//! for each job, runs the job's start script as it in a job object, and
//! deletes the account afterwards. This file is the part every OS builds, so
//! tests run it with a fake Host; account_windows.rs holds the Windows Host.

// Unix builds keep the Windows half so its tests run there; the Windows build checks it for dead code.
#![cfg_attr(not(windows), allow(dead_code))]
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

use super::launch::{ps_launch, ps_quote};
use super::{AgentRun, Conn, EXIT_DONE, EXIT_FAILED};
use crate::config::{self, Share};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::{quote, release, version};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub sid: String,
    pub place: String,
}

pub const START_FILE: &str = "start.ps1";
pub const GUARD_FILE: &str = "fork-guard.ps1";

/// What the service needs of Windows.
pub trait Host: Send + Sync {
    /// The SID of a local account, or "" when it does not exist.
    fn lookup_account(&self, name: &str) -> Result<String, String>;
    /// Makes a standard account and returns its SID. It fails with
    /// ERR_ACCOUNT_EXISTS when the name is taken.
    fn create_account(&self, name: &str, password: &[u16]) -> Result<String, String>;
    /// Puts the account in Users and the job group only, hides it from the
    /// sign-in screen and makes its job folder holding files.
    fn prepare(&self, a: &Account, files: &HashMap<String, Vec<u8>>) -> Result<(), String>;
    /// Runs the job's start script as the account, contained to its share of
    /// the machine, with input on its standard input, from a token that may
    /// not shut the machine down. The command line never carries input.
    fn start(&self, a: &Account, password: &[u16], input: &[u8], share: Share) -> Result<Box<dyn Running>, String>;

    // The cleanup steps, in this order: first nothing may sign in as the
    // account again, so nothing it left can start once its processes end.
    fn disable_account(&self, a: &Account) -> Result<(), String>;
    fn remove_tasks(&self, a: &Account) -> Result<(), String>;
    fn end_processes(&self, a: &Account) -> Result<(), String>;
    fn remove_shared_files(&self, a: &Account) -> Result<(), String>;
    fn delete_profile(&self, a: &Account) -> Result<(), String>;
    fn remove_folder(&self, a: &Account) -> Result<(), String>;
    fn delete_account(&self, a: &Account) -> Result<(), String>;
    /// Checks that nothing of the account is left.
    fn verify(&self, a: &Account) -> Result<(), String>;
}

/// A started job.
pub trait Running: Send + Sync {
    /// The start script's standard output and error together; it ends when
    /// every process holding them has ended. Taken once.
    fn output(&self) -> Box<dyn Read + Send>;
    /// Waits for the start script to end and gives its exit code.
    fn wait(&self) -> Result<i32, String>;
    /// Ends every process in the job.
    fn stop(&self);
    /// Releases the job, its sign-in and its loaded profile.
    fn close(&self);
}

pub const ERR_ACCOUNT_EXISTS: &str = "the account already exists";

/// One request on the service's pipe: one line of JSON. A run request holds
/// the pipe open for the job's whole life.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRequest {
    pub version: String,
    /// run, clean (delete whatever the place's earlier jobs left, waiting for
    /// a job still cleaning up), probe (answer that the service is there, and
    /// its version) or update (replace the service with a signed release).
    pub op: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cores: i64,
    #[serde(default, rename = "memoryMB", skip_serializing_if = "is_zero")]
    pub memory_mb: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_forks: bool,
    /// The runner's one-time registration, for run only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub registration: String,
    // An update request's release, as in ServiceUpdate: its bytes follow the
    // request line. Not flattened, since serde's flatten ignores
    // deny_unknown_fields.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub archive: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checksums: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
}

/// A release handed to the service, as the agent's update-service command
/// reads it: this, as one line of JSON, then the archive.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceUpdate {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub archive: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: i64,
    /// checksums.txt, as text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checksums: String,
    /// The signature of checksums.txt, in base64.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

pub const JOB_REQUEST_VERSION: &str = "runner-up/1";

/// Bounds a request line, so a broken or hostile sender cannot make the
/// service hold an unbounded one.
pub const MAX_JOB_REQUEST: usize = 128 << 10;

impl JobRequest {
    pub fn check(&self) -> Result<(), String> {
        if self.version != JOB_REQUEST_VERSION {
            return Err(format!("not a {JOB_REQUEST_VERSION} request"));
        }
        match self.op.as_str() {
            "probe" => return Ok(()),
            "update" => {
                let u = self;
                if u.archive.is_empty()
                    || u.archive.contains(['/', '\\', ':'])
                    || u.size <= 0
                    || u.size as u64 > release::MAX_ARCHIVE
                    || u.checksums.is_empty()
                {
                    return Err("an update names a release archive, its size and the release's checksums".into());
                }
                match base64::engine::general_purpose::STANDARD.decode(&u.signature) {
                    Ok(sig) if sig.len() == ed25519_dalek::SIGNATURE_LENGTH => return Ok(()),
                    _ => return Err("an update's signature is not a base64 Ed25519 signature".into()),
                }
            }
            "run" | "clean" => {}
            op => return Err(format!("unknown request {}", crate::go::quote(op))),
        }
        let p = &self.place;
        if p.is_empty() || p.len() > config::MAX_PLACE_ID || !crate::go::only(p, quote::WIN_PLAIN) || p.contains(['\\', '/', ':']) {
            return Err(format!("place {} is not a plain place id", crate::go::quote(p)));
        }
        if self.op == "clean" {
            return Ok(());
        }
        if self.cores < 1 || self.memory_mb < 512 {
            return Err("a job needs at least 1 core and 512 MB".into());
        }
        // A JIT registration is base64; anything else is refused before it can
        // reach a script.
        if self.registration.is_empty()
            || !crate::go::only(&self.registration, "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=")
        {
            return Err("the runner registration is not base64".into());
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = serde_json::to_vec(self).unwrap();
        b.push(b'\n');
        b
    }
}

/// Reads exactly one request line from r.
pub fn read_job_request(r: &mut impl BufRead) -> Result<JobRequest, String> {
    let mut b = Vec::new();
    loop {
        let mut c = [0u8; 1];
        match r.read(&mut c) {
            Ok(0) => return Err("no request: EOF".into()),
            Err(e) => return Err(format!("no request: {e}")),
            Ok(_) => {}
        }
        if c[0] == b'\n' {
            break;
        }
        if b.len() >= MAX_JOB_REQUEST {
            return Err(format!("request longer than {MAX_JOB_REQUEST} bytes"));
        }
        b.push(c[0]);
    }
    let req: JobRequest = serde_json::from_slice(&b).map_err(|e| format!("unreadable request: {e}"))?;
    req.check()?;
    Ok(req)
}

// Lines the service writes back start with SERVICE_PREFIX; a refusal with
// SERVICE_REFUSED.
pub const SERVICE_PREFIX: &str = "runner-up service: ";
pub const SERVICE_REFUSED: &str = "runner-up service: refused: ";
pub const SERVICE_READY: &str = "runner-up service: ready";

// What follows SERVICE_PREFIX on the lines that say where a request stands,
// which the agent reads back in service_answer.
const SERVICE_RUNNING_AS: &str = "running as ";
const SERVICE_SCRIPT_ENDED: &str = "start script ended with ";
pub(crate) const SERVICE_UPDATING_TO: &str = "updating to ";

pub type Files = Box<dyn Fn(&JobRequest) -> Result<HashMap<String, Vec<u8>>, String> + Send + Sync>;
pub type Update = Box<dyn Fn(&str, &[u8]) -> Result<(), String> + Send + Sync>;

/// The service's settings.
pub struct Options {
    pub records: std::path::PathBuf,
    pub max_job: Duration,
    /// Gives a job's start script and fork guard.
    pub files: Files,
    pub new_secret: fn() -> Vec<u16>,
    pub account_name: fn() -> String,
    /// How long a clean request waits for the place's job to finish cleaning
    /// up.
    pub clean_wait: Duration,
    /// Installs version, whose archive for this machine's platform the
    /// release signing keys vouched for, and has the service restart as it.
    /// A service with no update refuses updates.
    pub update: Option<Update>,
    pub platform: String,
    pub keys: Vec<VerifyingKey>,
    /// The version this service runs; an update must be newer.
    pub running: String,
}

impl Options {
    pub fn new(records: std::path::PathBuf, files: Files) -> Options {
        Options {
            records,
            max_job: Duration::from_secs(7 * 3600),
            files,
            new_secret: new_password,
            account_name,
            clean_wait: Duration::from_secs(180),
            update: None,
            platform: release::this_platform(),
            keys: Vec::new(),
            running: version::version().into(),
        }
    }
}

/// Where the service writes: a write that fails means the agent has gone.
pub type Sink = Arc<Mutex<Box<dyn Write + Send>>>;

fn say(out: &Sink, line: &str) -> std::io::Result<()> {
    let mut w = out.lock().unwrap();
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

#[derive(Default)]
struct Places {
    /// The places that run or clean up a job now.
    held: std::collections::HashSet<String>,
    /// The service is replacing itself and takes no job.
    updating: bool,
}

/// Runs jobs, one per place at a time.
pub struct Server {
    opts: Options,
    host: Box<dyn Host>,
    log: Logger,
    places: Mutex<Places>,
    freed: std::sync::Condvar,
    /// Serializes the records file.
    records: Mutex<()>,
}

/// A place's turn, given back when dropped.
struct Held<'a>(&'a Server, String);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.places.lock().unwrap().held.remove(&self.1);
        self.0.freed.notify_all();
    }
}

impl Server {
    pub fn new(opts: Options, host: Box<dyn Host>, log: Logger) -> Server {
        Server { opts, host, log, places: Mutex::default(), freed: Default::default(), records: Mutex::new(()) }
    }

    /// Takes place's turn, waiting up to wait for a job still running or
    /// cleaning up there. None when the place stayed busy or an update began.
    fn hold(&self, place: &str, wait: Duration) -> Option<Held<'_>> {
        let p = self.places.lock().unwrap();
        let (mut p, _) = self.freed.wait_timeout_while(p, wait, |p| p.held.contains(place)).unwrap();
        if p.held.contains(place) || p.updating {
            return None;
        }
        p.held.insert(place.to_string());
        Some(Held(self, place.to_string()))
    }

    /// Stops the service taking jobs, unless one runs or cleans up now.
    fn pause(&self) -> Result<(), String> {
        let mut p = self.places.lock().unwrap();
        if p.updating {
            return Err("the service is updating already".into());
        }
        if !p.held.is_empty() {
            return Err(format!("{} places run or clean up a job; update once they end", p.held.len()));
        }
        p.updating = true;
        Ok(())
    }

    fn resume(&self) {
        self.places.lock().unwrap().updating = false;
    }

    // Records: every account the service made and has not yet deleted, so a
    // restart finds what a cut-off job left. Only SYSTEM and Administrators
    // may read the file (protect_file).
    fn load(&self) -> Result<Vec<Account>, String> {
        let path = &self.opts.records;
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display())),
        }
    }

    fn save(&self, accounts: &[Account]) -> Result<(), String> {
        let path = &self.opts.records;
        let mut b = serde_json::to_vec_pretty(accounts).unwrap();
        b.push(b'\n');
        config::create_dir_all_mode(path.parent().unwrap_or(Path::new(".")), 0o700).map_err(|e| e.to_string())?;
        let mut tmp = path.clone().into_os_string();
        tmp.push(".new");
        let tmp = std::path::PathBuf::from(tmp);
        // Never write into a file someone else left.
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
            _ => {}
        }
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(&tmp).and_then(|mut f| f.write_all(&b)).map_err(|e| e.to_string())?;
        if let Err(e) = super::account::protect_file(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Adds or updates a, or removes it when drop is set.
    fn record(&self, a: &Account, drop: bool) -> Result<(), String> {
        let _g = self.records.lock().unwrap();
        let mut accounts = self.load()?;
        accounts.retain(|x| x.name != a.name);
        if !drop {
            accounts.push(a.clone());
        }
        self.save(&accounts)
    }

    /// Deletes every recorded account of place, or of every place when place
    /// is empty, and fails if anything of place is left. Every job request
    /// sweeps its place first, so a place whose cleanup failed takes no job
    /// until a later sweep succeeds. Accounts the service did not record are
    /// never touched.
    pub fn sweep(&self, place: &str) -> Result<(), String> {
        let accounts = {
            let _g = self.records.lock().unwrap();
            self.load()?
        };
        let mut failed = Ok(());
        for a in accounts {
            if !place.is_empty() && a.place != place {
                continue;
            }
            if let Err(e) = self.clean(a)
                && failed.is_ok()
            {
                failed = Err(e);
            }
        }
        failed
    }

    /// Runs every cleanup step in order, each even after an earlier one
    /// failed, then verifies, and forgets the account only when nothing is
    /// left.
    fn clean(&self, mut a: Account) -> Result<(), String> {
        let log = self.log.with(&[("account", &a.name)]);
        if a.sid.is_empty() {
            // Recorded before the account was made: it is ours if it exists.
            let sid = self.host.lookup_account(&a.name)?;
            if sid.is_empty() {
                return self.record(&a, true);
            }
            a.sid = sid;
            self.record(&a, false)?;
        }
        let h = &self.host;
        let steps: [(&str, &dyn Fn(&Account) -> Result<(), String>); 8] = [
            ("disable account", &|a| h.disable_account(a)),
            ("remove scheduled tasks", &|a| h.remove_tasks(a)),
            ("end processes", &|a| h.end_processes(a)),
            ("remove files in shared folders", &|a| h.remove_shared_files(a)),
            ("delete profile", &|a| h.delete_profile(a)),
            ("remove job folder", &|a| h.remove_folder(a)),
            ("delete account", &|a| h.delete_account(a)),
            ("verify", &|a| h.verify(a)),
        ];
        let mut first = None;
        for (name, step) in steps {
            if let Err(e) = step(&a) {
                log.error("cleanup step failed", &[("step", &name), ("err", &e)]);
                first.get_or_insert(format!("{name}: {e}"));
            }
        }
        if let Some(e) = first {
            return Err(e);
        }
        log.info("job account deleted", &[]);
        self.record(&a, true)
    }

    /// Answers one request read from input and writes what it has to say, and
    /// a job's output, to out. A run request's job ends when input ends: the
    /// agent stopped the job, or the agent or its connection went.
    pub fn serve(&self, ctx: &Ctx, input: Box<dyn Read + Send>, out: &Sink) -> Result<(), String> {
        let mut br = BufReader::new(input);
        let req = match read_job_request(&mut br) {
            Ok(r) => r,
            Err(e) => return self.refuse(out, e),
        };
        match req.op.as_str() {
            "probe" => {
                let _ = say(out, &format!("{SERVICE_READY} {}", version::version()));
                return Ok(());
            }
            "update" => return self.update(&req, &mut br, out),
            "clean" => {
                let Some(_held) = self.hold(&req.place, self.opts.clean_wait) else {
                    return self.refuse(out, format!("place {} is still running a job, or the service is updating", req.place));
                };
                if let Err(e) = self.sweep(&req.place) {
                    return self.refuse(out, format!("place {} is not clean: {e}", req.place));
                }
                let _ = say(out, &format!("{SERVICE_PREFIX}place {} is clean", req.place));
                return Ok(());
            }
            _ => {}
        }
        let Some(_held) = self.hold(&req.place, Duration::ZERO) else {
            return self.refuse(out, format!("place {} already runs a job, or the service is updating", req.place));
        };
        if let Err(e) = self.sweep(&req.place) {
            return self.refuse(out, format!("place {} is not clean: {e}", req.place));
        }
        let (gone_tx, gone) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut br, &mut std::io::sink());
            drop(gone_tx);
        });
        let res = self.run(ctx, &req, out, &gone);
        let _ = match &res {
            Err(e) => say(out, &format!("{SERVICE_PREFIX}{e}")),
            Ok(code) => say(out, &format!("{SERVICE_PREFIX}{SERVICE_SCRIPT_ENDED}{code}")),
        };
        res.map(|_| ())
    }

    /// Reads a release from input, after the request line, and installs it
    /// once the release signing key vouches for it and it is newer than the
    /// service. No job runs meanwhile, and none starts once the service is to
    /// restart as the new version.
    fn update(&self, req: &JobRequest, input: &mut impl Read, out: &Sink) -> Result<(), String> {
        let Some(install) = &self.opts.update else {
            return self.refuse(out, "this service does not update itself".into());
        };
        let mut archive = vec![0u8; req.size as usize];
        if let Err(e) = input.read_exact(&mut archive) {
            return self.refuse(out, format!("the archive ended early: {e}"));
        }
        let signed = release::Signed {
            name: req.archive.clone(),
            archive,
            checksums: req.checksums.clone().into_bytes(),
            signature: base64::engine::general_purpose::STANDARD.decode(&req.signature).unwrap_or_default(),
        };
        let v = match signed.verify(&self.opts.platform, &self.opts.keys) {
            Ok(v) => v,
            Err(e) => return self.refuse(out, e),
        };
        if release::compare(&v, &self.opts.running).is_le() {
            return self.refuse(
                out,
                format!(
                    "the service runs {} and installs only a newer release, not {v}; an administrator's runner-up setup installs any",
                    self.opts.running
                ),
            );
        }
        if let Err(e) = self.pause() {
            return self.refuse(out, e);
        }
        if let Err(e) = install(&v, &signed.archive) {
            self.resume();
            return self.refuse(out, e);
        }
        self.log.info("installed a signed release; restarting as it", &[("version", &v)]);
        let _ = say(out, &format!("{SERVICE_PREFIX}{SERVICE_UPDATING_TO}{v}"));
        Ok(())
    }

    fn refuse(&self, out: &Sink, err: String) -> Result<(), String> {
        self.log.warn("refused a request", &[("err", &err)]);
        let _ = say(out, &format!("{SERVICE_REFUSED}{err}"));
        Err(err)
    }

    fn run(&self, ctx: &Ctx, req: &JobRequest, out: &Sink, gone: &mpsc::Receiver<()>) -> Result<i32, String> {
        let files = (self.opts.files)(req)?;
        let name = (self.opts.account_name)();
        let mut a = Account { name: name.clone(), sid: String::new(), place: req.place.clone() };
        let log = self.log.with(&[("account", &name), ("place", &req.place)]);
        // Record the name before the account exists, so a power cut at any
        // later moment leaves a record the next sweep acts on.
        self.record(&a, false)?;
        let mut password = (self.opts.new_secret)();
        let made = self.host.create_account(&name, &password);
        a.sid = match made {
            Ok(sid) => sid,
            Err(e) => {
                password.fill(0);
                if e == ERR_ACCOUNT_EXISTS {
                    // Never delete an account this call did not make.
                    let _ = self.record(&a, true);
                }
                // Otherwise it may exist all the same; the record stays for
                // the next sweep.
                return Err(format!("could not make {name}: {e}"));
            }
        };
        if let Err(e) = self.record(&a, false) {
            password.fill(0);
            let _ = self.clean(a);
            return Err(e);
        }
        let res = self.run_as(ctx, req, &a, &files, &mut password, out, gone, &log);
        password.fill(0);
        let cleaned = self.clean(a);
        match (res, cleaned) {
            (Ok(_), Err(e)) => Err(e),
            (res, _) => res,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_as(
        &self,
        ctx: &Ctx,
        req: &JobRequest,
        a: &Account,
        files: &HashMap<String, Vec<u8>>,
        password: &mut [u16],
        out: &Sink,
        gone: &mpsc::Receiver<()>,
        log: &Logger,
    ) -> Result<i32, String> {
        log.info("job account made", &[("sid", &a.sid)]);
        self.host.prepare(a, files).map_err(|e| format!("could not prepare {}: {e}", a.name))?;
        let job = self.host.start(
            a,
            password,
            format!("{}\n", req.registration).as_bytes(),
            Share { cores: req.cores, memory_mb: req.memory_mb },
        );
        password.fill(0);
        let job: Arc<dyn Running> = Arc::from(job.map_err(|e| format!("could not start the job as {}: {e}", a.name))?);
        let _ = say(out, &format!("{SERVICE_PREFIX}{SERVICE_RUNNING_AS}{}", a.name));
        let (relayed_tx, relayed) = mpsc::channel::<()>();
        {
            let (job, out) = (job.clone(), out.clone());
            let mut output = job.output();
            std::thread::spawn(move || {
                let mut b = [0u8; 8192];
                loop {
                    let n = match output.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut w = out.lock().unwrap();
                    // A write that fails means the agent has gone.
                    if w.write_all(&b[..n]).and_then(|_| w.flush()).is_err() {
                        job.stop();
                        break;
                    }
                }
                drop(relayed_tx);
            });
        }
        let (waited_tx, waited) = mpsc::channel();
        {
            let job = job.clone();
            std::thread::spawn(move || {
                let _ = waited_tx.send(job.wait());
            });
        }
        let limit = std::time::Instant::now() + self.opts.max_job;
        let result = loop {
            if let Ok(r) = waited.recv_timeout(Duration::from_millis(50)) {
                break r;
            }
            let why = if matches!(gone.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                "the agent went away; stopping the job"
            } else if ctx.is_done() {
                "the service is stopping; stopping the job"
            } else if std::time::Instant::now() >= limit {
                "the job ran past its limit; stopping it"
            } else {
                continue;
            };
            log.warn(why, &[]);
            job.stop();
            break waited.recv().unwrap_or_else(|_| Err("the job's wait ended".into()));
        };
        // The start script has ended; whatever it left in the job ends too,
        // so the output reaches its end.
        job.stop();
        let _ = relayed.recv_timeout(Duration::from_secs(30));
        job.close();
        match &result {
            Ok(code) => log.info("job ended", &[("exit", code)]),
            Err(e) => log.info("job ended", &[("err", e)]),
        }
        result
    }
}

/// rujob-<8 hex>: 14 characters, inside the 20 a local account name may
/// have.
pub fn account_name() -> String {
    format!("rujob-{}", crate::random::hex(4))
}

/// 48 random bytes as base64 plus one character of each class Windows'
/// complexity rule asks for, as UTF-16 so it can be wiped after use.
pub fn new_password() -> Vec<u16> {
    let mut b = [0u8; 48];
    getrandom::getrandom(&mut b).expect("no randomness");
    const ENC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut p = Vec::with_capacity(68);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        for shift in [18, 12, 6, 0] {
            p.push(ENC[(n >> shift & 63) as usize] as u16);
        }
    }
    b.fill(0);
    p.extend("aA1!".encode_utf16());
    p
}

/// The start script's command line: Windows PowerShell, which every Windows
/// has, running the script in the job's folder. It is made from the folder
/// alone, so no request field can reach it.
pub fn job_command_line(powershell: &str, folder: &str) -> String {
    format!(
        "\"{powershell}\" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\\{START_FILE}\"",
        folder.trim_end_matches('\\')
    )
}

const ACCOUNT_JOB_SCRIPT: &str = include_str!("../../internal/agent/account-job.ps1");

/// A job's start script, which unpacks the runner archive zip only when its
/// SHA-256 is sum, and, unless req allows forks, the fork guard, which runs
/// program's fork-guard command.
pub fn account_job_files(zip: &str, sum: &str, program: &str, req: &JobRequest) -> HashMap<String, Vec<u8>> {
    let a = AgentRun {
        cores: req.cores,
        memory_mb: req.memory_mb,
        allow_forks: req.allow_forks,
        self_path: program.to_string(),
        ..Default::default()
    };
    let l = super::account::launch(&a);
    let mut files = HashMap::new();
    if !l.hook.is_empty() {
        files.insert(GUARD_FILE.to_string(), l.guard.clone().into_bytes());
    }
    let script = super::hyperv::replace_all(
        ACCOUNT_JOB_SCRIPT,
        &[("@RUNNER_ZIP@", ps_quote(zip)), ("@RUNNER_SHA256@", ps_quote(&sum.to_uppercase())), ("@LAUNCH@", ps_launch(&l))],
    );
    files.insert(START_FILE.to_string(), script.into_bytes());
    files
}

/// The fork guard as the runner runs a hook: a PowerShell script that runs
/// this program's fork-guard command.
pub fn account_guard_script(program: &str) -> String {
    format!("& {} fork-guard\nexit $LASTEXITCODE\n", ps_quote(program))
}

// The agent's side of an account place. The account provider sends one
// request to the service and relays what comes back; on Windows the
// connection is the service's pipe.

enum Answer {
    Done,
    Pending,
    Failed(String),
}

/// Sends req on conn and copies the service's answer to out until the
/// service ends it. It closes conn when ctx is done, which is how the
/// service learns the agent has gone and stops the job. It returns Ok once
/// the service said the request was done, or when ctx stopped a run - the
/// agent's own choice, as with every other place. A refusal, any other end,
/// or ctx ending a probe, clean or update before its answer is an error.
pub fn ask_service(ctx: &Ctx, conn: Conn, req: &JobRequest, body: Option<&mut dyn Read>, out: &mut dyn Write) -> Result<(), String> {
    let Conn { reader, mut writer, close } = conn;
    let finished = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (ctx, close, finished) = (ctx.clone(), close.clone(), finished.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if ctx.sleep(Duration::from_millis(50)) {
                    close();
                    return;
                }
            }
        })
    };
    let result = talk(&mut *writer, reader, req, body, out);
    finished.store(true, Ordering::SeqCst);
    close();
    let _ = watcher.join();
    match result {
        Ok(Answer::Done) => Ok(()),
        _ if ctx.is_done() && req.op == "run" => Ok(()),
        _ if ctx.is_done() => Err(format!("runner-up's service gave no answer to {}: {}", req.op, ctx.err().unwrap_or("context canceled"))),
        Ok(Answer::Pending) => Err("runner-up's service ended without an answer".into()),
        Ok(Answer::Failed(e)) | Err(e) => Err(e),
    }
}

fn talk(
    writer: &mut dyn Write,
    reader: Box<dyn Read + Send>,
    req: &JobRequest,
    body: Option<&mut dyn Read>,
    out: &mut dyn Write,
) -> Result<Answer, String> {
    writer.write_all(&req.encode()).map_err(|e| format!("could not reach runner-up's service: {e}"))?;
    if let Some(body) = body {
        std::io::copy(body, writer).map_err(|e| format!("could not send runner-up's service the release: {e}"))?;
    }
    let _ = writer.flush();
    let mut r = BufReader::new(reader);
    let mut result = Answer::Pending;
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line);
        if !line.is_empty() {
            let _ = out.write_all(&line);
            if !line.ends_with(b"\n") {
                let _ = out.write_all(b"\n");
            }
            let text = String::from_utf8_lossy(&line);
            if let Some(l) = text.trim_end_matches(['\r', '\n']).strip_prefix(SERVICE_PREFIX) {
                result = service_answer(&req.op, l);
            }
        }
        if !matches!(n, Ok(k) if k > 0 && line.ends_with(b"\n")) {
            break;
        }
    }
    Ok(result)
}

/// The version in the service's answer to a probe.
pub fn ready_version(answer: &str) -> Result<String, String> {
    for line in answer.split('\n') {
        if let Some(v) = line.trim().strip_prefix(&format!("{SERVICE_READY} "))
            && !v.is_empty()
        {
            return Ok(v.to_string());
        }
    }
    Err("runner-up's service answered with no version".into())
}

pub type AskUpdate<'a> = &'a dyn Fn(&Ctx, &ServiceUpdate, &mut dyn Read, &mut dyn Write) -> Result<(), String>;

/// `agent update-service`: hands the service the release on input - a
/// ServiceUpdate line, then the archive - and relays its answer to out.
pub fn update_service_main(input: &mut dyn Read, out: &mut dyn Write, ask: AskUpdate<'_>) -> i32 {
    let mut r = BufReader::with_capacity(64 << 10, input);
    let mut head = Vec::new();
    loop {
        let mut b = [0u8; 1];
        match r.read(&mut b) {
            Ok(0) => {
                if head.is_empty() {
                    let _ = writeln!(out, "runner-up agent: no release on input: EOF");
                    return EXIT_FAILED;
                }
                break;
            }
            Err(e) => {
                let _ = writeln!(out, "runner-up agent: no release on input: {e}");
                return EXIT_FAILED;
            }
            Ok(_) if b[0] == b'\n' => break,
            Ok(_) => head.push(b[0]),
        }
        if head.len() > MAX_JOB_REQUEST {
            let _ = writeln!(out, "runner-up agent: the release's description is too long");
            return EXIT_FAILED;
        }
    }
    if head.ends_with(b"\r") {
        head.pop();
    }
    let u: ServiceUpdate = match serde_json::from_slice(&head) {
        Ok(u) => u,
        Err(e) => {
            let _ = writeln!(out, "runner-up agent: unreadable release description: {e}");
            return EXIT_FAILED;
        }
    };
    let ctx = Ctx::detached_timeout(Duration::from_secs(600));
    let mut body = r.take(u.size.max(0) as u64);
    if let Err(e) = ask(&ctx, &u, &mut body, out) {
        let _ = writeln!(out, "runner-up agent: {e}");
        return EXIT_FAILED;
    }
    EXIT_DONE
}

/// Reads one of the service's own lines: Done when it ends the request well,
/// Pending for a line that ends nothing, or the error.
fn service_answer(op: &str, line: &str) -> Answer {
    if let Some(e) = line.strip_prefix("refused: ") {
        return Answer::Failed(e.to_string());
    }
    let done = match op {
        "probe" => line.starts_with("ready"),
        "update" => line.starts_with(SERVICE_UPDATING_TO),
        "clean" => line.ends_with(" is clean"),
        "run" => line.starts_with(SERVICE_SCRIPT_ENDED),
        _ => false,
    };
    if done {
        return Answer::Done;
    }
    if op == "run" && line.starts_with(SERVICE_RUNNING_AS) {
        return Answer::Failed("runner-up's service stopped answering during the job".into());
    }
    Answer::Failed(line.to_string())
}

#[cfg(test)]
#[path = "tests/jobhost.rs"]
mod tests;
