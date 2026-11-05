// Intent (README, "What you need" and "Windows machines"): each Windows job of
// an account place runs as a brand-new standard account that is deleted after
// it, whatever happened - passed, failed, stopped, or cut off by a restart; the
// service never deletes an account it did not record; a place whose cleanup
// failed takes no job until a later sweep succeeds; the runner's one-time
// registration reaches the job only on its input, never in a file or a
// program's arguments; and the job ends when the agent's end of the pipe
// closes.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;

use super::*;
use crate::agent::runner::runner_windows_sha256;
use crate::agent::tests::{Buf, REGISTRATION, has_placeholder, tcp_pair};
use crate::go::slog::Logger;

/// Windows, stood in for: accounts by name, and a log of every call.
#[derive(Default)]
struct HostState {
    /// name -> SID
    accounts: HashMap<String, String>,
    calls: Vec<String>,
    files: HashMap<String, HashMap<String, Vec<u8>>>,
    input: Vec<u8>,
    share: Share,
    /// step -> failures left
    fail: HashMap<String, i32>,
    /// The accounts jobs started as.
    jobs: Vec<String>,
    next_sid: i32,
    output: String,
    exit: i32,
    block: bool,
    /// Makes create_account make the account and then fail to read its SID.
    lost_sid: bool,
}

#[derive(Clone)]
struct FakeHost(Arc<Mutex<HostState>>);

fn new_fake_host() -> FakeHost {
    FakeHost(Arc::new(Mutex::new(HostState {
        output: "√ Connected to GitHub\nCurrent runner version: '2.338.1'\nRunning job: build\n".into(),
        ..Default::default()
    })))
}

impl FakeHost {
    fn state(&self) -> MutexGuard<'_, HostState> {
        self.0.lock().unwrap()
    }

    fn call(&self, step: &str, a: &Account) -> Result<(), String> {
        let mut h = self.state();
        h.calls.push(format!("{step} {}", a.name));
        match h.fail.get_mut(step) {
            Some(left) if *left > 0 => {
                *left -= 1;
                Err(format!("{step} failed"))
            }
            _ => Ok(()),
        }
    }

    fn steps(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    fn started(&self) -> usize {
        self.state().jobs.len()
    }

    fn creates(&self) -> usize {
        self.steps().iter().filter(|c| c.starts_with("create ")).count()
    }
}

impl Host for FakeHost {
    fn lookup_account(&self, name: &str) -> Result<String, String> {
        Ok(self.state().accounts.get(name).cloned().unwrap_or_default())
    }

    fn create_account(&self, name: &str, _: &[u16]) -> Result<String, String> {
        let mut h = self.state();
        h.calls.push(format!("create {name}"));
        if h.accounts.contains_key(name) {
            return Err(ERR_ACCOUNT_EXISTS.into());
        }
        h.next_sid += 1;
        let sid = format!("S-1-5-21-1-2-3-{}", 1000 + h.next_sid);
        h.accounts.insert(name.into(), sid.clone());
        if h.lost_sid {
            return Err("made, but not found".into());
        }
        Ok(sid)
    }

    fn prepare(&self, a: &Account, files: &HashMap<String, Vec<u8>>) -> Result<(), String> {
        self.state().files.insert(a.name.clone(), files.clone());
        self.call("prepare", a)
    }

    fn start(&self, a: &Account, _: &[u16], input: &[u8], share: Share) -> Result<Box<dyn Running>, String> {
        self.call("start", a)?;
        let mut h = self.state();
        h.input = input.to_vec();
        h.share = share;
        h.jobs.push(a.name.clone());
        Ok(Box::new(FakeJob {
            out: Mutex::new(Some(h.output.clone())),
            exit: h.exit,
            block: h.block,
            stopped: (Mutex::new(false), Condvar::new()),
            host: self.clone(),
            name: a.name.clone(),
        }))
    }

    fn disable_account(&self, a: &Account) -> Result<(), String> {
        self.call("disable-account", a)
    }
    fn remove_tasks(&self, a: &Account) -> Result<(), String> {
        self.call("remove-tasks", a)
    }
    fn end_processes(&self, a: &Account) -> Result<(), String> {
        self.call("end-processes", a)
    }
    fn remove_shared_files(&self, a: &Account) -> Result<(), String> {
        self.call("remove-shared-files", a)
    }
    fn delete_profile(&self, a: &Account) -> Result<(), String> {
        self.call("delete-profile", a)
    }
    fn remove_folder(&self, a: &Account) -> Result<(), String> {
        self.call("remove-folder", a)
    }

    fn delete_account(&self, a: &Account) -> Result<(), String> {
        self.call("delete-account", a)?;
        let mut h = self.state();
        if h.accounts.get(&a.name) == Some(&a.sid) {
            h.accounts.remove(&a.name);
        }
        Ok(())
    }

    fn verify(&self, a: &Account) -> Result<(), String> {
        self.call("verify", a)?;
        if self.state().accounts.get(&a.name) == Some(&a.sid) {
            return Err("the account is still there".into());
        }
        Ok(())
    }
}

struct FakeJob {
    out: Mutex<Option<String>>,
    exit: i32,
    block: bool,
    stopped: (Mutex<bool>, Condvar),
    host: FakeHost,
    name: String,
}

impl Running for FakeJob {
    fn output(&self) -> Box<dyn Read + Send> {
        Box::new(Cursor::new(self.out.lock().unwrap().take().unwrap_or_default()))
    }

    fn wait(&self) -> Result<i32, String> {
        if !self.block {
            return Ok(self.exit);
        }
        let (stopped, cv) = &self.stopped;
        let _s = cv.wait_while(stopped.lock().unwrap(), |s| !*s).unwrap();
        Ok(1)
    }

    fn stop(&self) {
        let (stopped, cv) = &self.stopped;
        let mut s = stopped.lock().unwrap();
        if !*s {
            *s = true;
            self.host.state().calls.push(format!("stop {}", self.name));
            cv.notify_all();
        }
    }

    fn close(&self) {
        self.host.state().calls.push(format!("close {}", self.name));
    }
}

fn run_request(place: &str) -> JobRequest {
    JobRequest {
        version: JOB_REQUEST_VERSION.into(),
        op: "run".into(),
        place: place.into(),
        cores: 2,
        memory_mb: 4096,
        registration: REGISTRATION.into(),
        ..Default::default()
    }
}

fn sink(w: impl Write + Send + 'static) -> Sink {
    Arc::new(Mutex::new(Box::new(w)))
}

/// Sends req as the agent does, holding its end open until the service has
/// answered, and returns what the service wrote.
fn ask_job_host(s: &Server, req: &JobRequest) -> (String, Result<(), String>) {
    let (r, mut w) = std::io::pipe().unwrap();
    w.write_all(&req.encode()).unwrap();
    let buf = Buf::default();
    let res = s.serve(&Ctx::background(), Box::new(r), &sink(buf.clone()));
    drop(w);
    (buf.text(), res)
}

/// A run request whose agent stays until the returned writer is dropped;
/// the receiver gets what serve returned.
fn hold_job(s: &Arc<Server>, place: &str) -> (std::io::PipeWriter, Receiver<Result<(), String>>) {
    let (r, mut w) = std::io::pipe().unwrap();
    w.write_all(&run_request(place).encode()).unwrap();
    let (tx, done) = channel();
    let s = s.clone();
    std::thread::spawn(move || {
        let _ = tx.send(s.serve(&Ctx::background(), Box::new(r), &sink(std::io::sink())));
    });
    (w, done)
}

/// Serves one request on peer, then disconnects it, as the service's pipe
/// handler does; the receiver hears when it has.
fn serve_over(s: &Arc<Server>, peer: TcpStream) -> Receiver<()> {
    let (tx, served) = channel();
    let s = s.clone();
    std::thread::spawn(move || {
        let _ = s.serve(&Ctx::background(), Box::new(peer.try_clone().unwrap()), &sink(peer.try_clone().unwrap()));
        let _ = peer.shutdown(Shutdown::Both);
        let _ = tx.send(());
    });
    served
}

fn test_files(req: &JobRequest) -> Result<HashMap<String, Vec<u8>>, String> {
    Ok(account_job_files(
        r"C:\runner-up\cache\actions-runner-win-x64-2.338.1.zip",
        runner_windows_sha256("x64").unwrap(),
        r"C:\Program Files\runner-up\runner-up.exe",
        req,
    ))
}

fn test_options(dir: &Path) -> Options {
    let mut o = Options::new(dir.join("job-accounts.json"), Box::new(test_files));
    o.clean_wait = Duration::from_secs(1);
    o
}

fn new_test_server(h: &FakeHost, dir: &Path) -> Arc<Server> {
    Arc::new(Server::new(test_options(dir), Box::new(h.clone()), Logger::discard()))
}

const CLEANUP_ORDER: [&str; 8] =
    ["disable-account", "remove-tasks", "end-processes", "remove-shared-files", "delete-profile", "remove-folder", "delete-account", "verify"];

fn steps_of(calls: &[String], name: &str) -> Vec<String> {
    calls.iter().filter_map(|c| c.split_once(' ').filter(|(_, n)| *n == name).map(|(s, _)| s.to_string())).collect()
}

fn job_steps() -> String {
    format!("create prepare start stop close {}", CLEANUP_ORDER.join(" "))
}

/// Waits until n jobs have started on h.
fn wait_started(h: &FakeHost, n: usize) {
    let until = Instant::now() + Duration::from_secs(5);
    while h.started() < n {
        assert!(Instant::now() < until, "{} of {n} jobs started", h.started());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn receive<T>(rx: &Receiver<T>, what: &str) -> T {
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| panic!("waited 5s for {what}"))
}

#[test]
fn each_job_runs_as_a_fresh_account_deleted_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let s = new_test_server(&h, dir.path());
    let mut out = String::new();
    for i in 0..2 {
        let (o, res) = ask_job_host(&s, &run_request("win-pc-1"));
        assert_eq!(res, Ok(()), "job {i}:\n{o}");
        out += &o;
    }
    let names: Vec<String> = h.steps().iter().filter_map(|c| c.strip_prefix("create ").map(String::from)).collect();
    assert!(names.len() == 2 && names[0] != names[1], "two jobs made accounts {names:?}, want two different ones");
    for n in &names {
        let hex = n.strip_prefix("rujob-").unwrap_or_default();
        assert!(
            hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && n.len() <= 20,
            "account {n:?} is not rujob-<8 hex> within Windows' 20 characters"
        );
        // The job object is ended even after a clean exit, so nothing the
        // job started outlives it.
        assert_eq!(steps_of(&h.steps(), n).join(" "), job_steps(), "{n}'s steps");
    }
    assert!(h.state().accounts.is_empty(), "accounts left after the jobs: {:?}", h.state().accounts);
    assert_eq!(s.load(), Ok(vec![]), "records left after the jobs");
    // This test owns the service's running as and start script lines.
    assert!(
        out.contains("runner-up service: running as rujob-")
            && out.contains("Running job: build")
            && out.contains("runner-up service: start script ended with 0"),
        "the job's output and end did not reach the agent:\n{out}"
    );
    assert_eq!(h.state().share, Share { cores: 2, memory_mb: 4096 }, "the job's containment, want the request's 2 cores and 4096 MB");
}

#[test]
fn registration_reaches_the_job_only_on_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let s = new_test_server(&h, dir.path());
    let (out, res) = ask_job_host(&s, &run_request("win-pc-2"));
    assert_eq!(res, Ok(()), "{out}");
    assert_eq!(h.state().input, format!("{REGISTRATION}\n").into_bytes(), "the start script's input, want the registration alone");
    for (name, files) in &h.state().files {
        for (f, b) in files {
            assert!(!String::from_utf8_lossy(b).contains(REGISTRATION), "{name}'s {f} holds the registration");
        }
        assert!(
            files.get(START_FILE).is_some_and(|b| !b.is_empty()) && files.get(GUARD_FILE).is_some_and(|b| !b.is_empty()),
            "{name}'s folder lacks the start script or the guard: {:?}",
            files.keys()
        );
    }
    let line = job_command_line(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", r"C:\runner-up\jobs\rujob-a1b2c3d4");
    assert_eq!(
        line,
        r#""C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "C:\runner-up\jobs\rujob-a1b2c3d4\start.ps1""#,
        "the start script's command line"
    );
}

#[test]
fn requests_that_could_reach_a_script_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let s = new_test_server(&h, dir.path());
    let req = |version: &str, place: &str, share: bool, registration: &str| JobRequest {
        version: version.into(),
        op: "run".into(),
        place: place.into(),
        cores: if share { 2 } else { 0 },
        memory_mb: if share { 4096 } else { 0 },
        registration: registration.into(),
        ..Default::default()
    };
    for (why, r) in [
        ("a registration that is not base64", req(JOB_REQUEST_VERSION, "win-pc-1", true, "abc\"; Remove-Item C:\\ -Recurse")),
        ("a place that is a path", req(JOB_REQUEST_VERSION, r"..\..\Windows", true, REGISTRATION)),
        ("another version", req("runner-up/0", "win-pc-1", true, REGISTRATION)),
        ("no share", req(JOB_REQUEST_VERSION, "win-pc-1", false, REGISTRATION)),
    ] {
        let (out, res) = ask_job_host(&s, &r);
        assert!(res.is_err() && out.starts_with(SERVICE_REFUSED), "{why}: answered {out:?}, {res:?}; want a refusal");
    }
    // A field the service does not know, such as a start script of the
    // sender's own, is refused too.
    let line = format!(
        r#"{{"version":"runner-up/1","op":"run","place":"win-pc-1","cores":2,"memoryMB":4096,"registration":"{REGISTRATION}","start":"calc.exe"}}"#
    ) + "\n";
    let buf = Buf::default();
    let res = s.serve(&Ctx::background(), Box::new(Cursor::new(line.into_bytes())), &sink(buf.clone()));
    assert!(res.is_err() && buf.text().starts_with(SERVICE_REFUSED), "a request with a start script of its own was answered {:?}, {res:?}", buf.text());
    assert_eq!(h.started(), 0, "a refused request started a job");
}

#[test]
fn leftover_account_is_deleted_before_the_next_job() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let s = new_test_server(&h, dir.path());
    // A power cut during a job: its account and record stay.
    let sid = h.create_account("rujob-0a0b0c0d", &[]).unwrap();
    s.record(&Account { name: "rujob-0a0b0c0d".into(), sid, place: "win-pc-1".into() }, false).unwrap();
    // Cut between recording a name and making the account.
    s.record(&Account { name: "rujob-0d0e0f10".into(), sid: String::new(), place: "win-pc-2".into() }, false).unwrap();
    h.create_account("rujob-0d0e0f10", &[]).unwrap();
    // An account the service never recorded, such as one made by hand.
    h.state().accounts.insert("rujob-ffffffff".into(), "S-1-5-21-9-9-9-1001".into());

    let restarted = new_test_server(&h, dir.path());
    assert_eq!(restarted.sweep(""), Ok(()));
    for n in ["rujob-0a0b0c0d", "rujob-0d0e0f10"] {
        assert!(!h.state().accounts.contains_key(n), "the restart left {n}");
        let got = steps_of(&h.steps(), n);
        assert!(got.ends_with(&CLEANUP_ORDER.map(String::from)), "{n} was cleaned with {got:?}, want {CLEANUP_ORDER:?}");
    }
    assert!(
        h.state().accounts.contains_key("rujob-ffffffff") && steps_of(&h.steps(), "rujob-ffffffff").is_empty(),
        "the sweep touched an account it never recorded: {:?}",
        steps_of(&h.steps(), "rujob-ffffffff")
    );
    assert_eq!(restarted.load(), Ok(vec![]), "records left after the sweep");
}

#[test]
fn name_taken_by_another_account_is_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let mut opts = test_options(dir.path());
    opts.account_name = || "rujob-12345678".into();
    let s = Server::new(opts, Box::new(h.clone()), Logger::discard());
    h.state().accounts.insert("rujob-12345678".into(), "S-1-5-21-7-7-7-1001".into());
    let (_, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert!(res.is_err(), "a job ran under a name another account holds");
    assert_eq!(h.state().accounts["rujob-12345678"], "S-1-5-21-7-7-7-1001", "the service deleted an account it did not make");
    for c in h.steps() {
        assert!(c.starts_with("create "), "the service went on to {c:?} after the name was taken");
    }
    assert_eq!(s.load(), Ok(vec![]), "the service kept a record of an account it did not make");
}

// An account made whose SID could not be read must not drop out of the
// record, or nothing would ever delete it.
#[test]
fn account_made_but_not_read_is_deleted_by_the_next_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    h.state().lost_sid = true;
    let s = new_test_server(&h, dir.path());
    let (_, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert!(res.is_err(), "a job ran without its account's SID");
    assert_eq!(h.state().accounts.len(), 1, "accounts {:?}, want the one made", h.state().accounts);
    h.state().lost_sid = false;
    assert_eq!(s.sweep("win-pc-1"), Ok(()));
    assert!(h.state().accounts.is_empty(), "the sweep left {:?}", h.state().accounts);
}

#[test]
fn place_whose_cleanup_failed_takes_no_job_until_a_sweep_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    h.state().fail.insert("delete-profile".into(), 2);
    let s = new_test_server(&h, dir.path());
    let (_, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert!(res.is_err(), "a job whose cleanup failed reported success");
    // The next request's sweep fails again: no new account is made.
    let (out, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert!(res.is_err() && h.creates() == 1, "a dirty place took a job ({} accounts made): {out}", h.creates());
    assert!(out.contains("refused: place win-pc-1 is not clean"), "the refusal did not say why: {out}");
    // The other place is unaffected.
    let (out, res) = ask_job_host(&s, &run_request("win-pc-2"));
    assert_eq!(res, Ok(()), "place win-pc-2 refused a job: {out}");
    // Once the sweep succeeds, the place runs again.
    let (out, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert_eq!(res, Ok(()), "place win-pc-1 still refused after its leftovers were deleted: {out}");
    assert!(h.state().accounts.is_empty(), "accounts {:?} after a clean sweep", h.state().accounts);
}

#[test]
fn job_is_stopped_and_deleted_when_the_agent_goes() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    h.state().block = true;
    let s = new_test_server(&h, dir.path());
    let (agent, done) = hold_job(&s, "win-pc-1");
    wait_started(&h, 1);
    // The agent's end of the pipe closes.
    drop(agent);
    let _ = receive(&done, "the job to end once the agent went");
    let name = h.state().jobs[0].clone();
    assert_eq!(steps_of(&h.steps(), &name).join(" "), job_steps(), "{name}'s steps");
}

#[test]
fn refuses_a_second_job_in_a_busy_place() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    h.state().block = true;
    let s = new_test_server(&h, dir.path());
    let (agent, done) = hold_job(&s, "win-pc-1");
    wait_started(&h, 1);
    let (out, res) = ask_job_host(&s, &run_request("win-pc-1"));
    assert!(res.is_err() && out.contains("already runs a job"), "a second job in a busy place was not refused: {res:?} {out}");
    // A clean request waits for the place's turn, then gives up.
    let clean = JobRequest { version: JOB_REQUEST_VERSION.into(), op: "clean".into(), place: "win-pc-1".into(), ..Default::default() };
    let (out, res) = ask_job_host(&s, &clean);
    assert!(res.is_err() && out.contains("still running a job"), "a clean of a busy place did not wait and refuse: {res:?} {out}");
    drop(agent);
    let _ = receive(&done, "the service to end once the agent went");
}

// The agent's side against the service itself, as over the pipe: a job runs
// to its end, and closing the agent ends the job and deletes its account.
#[test]
fn agent_and_service_over_the_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let s = new_test_server(&h, dir.path());
    let (agent, peer) = tcp_pair();
    serve_over(&s, peer);
    let mut out = Vec::new();
    let res = ask_service(&Ctx::background(), agent, &run_request("win-pc-1"), None, &mut out);
    let out = String::from_utf8_lossy(&out);
    assert_eq!(res, Ok(()), "a job that ran to its end failed:\n{out}");
    assert!(out.contains("Running job: build"), "the job's output did not reach the agent:\n{out}");

    h.state().block = true;
    let (agent, peer) = tcp_pair();
    let served = serve_over(&s, peer);
    let ctx = Ctx::background().with_cancel();
    let (asked_tx, asked) = channel();
    let c = ctx.clone();
    std::thread::spawn(move || {
        let _ = asked_tx.send(ask_service(&c, agent, &run_request("win-pc-1"), None, &mut std::io::sink()));
    });
    wait_started(&h, 2);
    // The coordinator stopped the place: the agent's input closed.
    ctx.cancel();
    assert_eq!(receive(&asked, "the agent to end once its place stopped"), Ok(()), "a stopped job failed the agent");
    receive(&served, "the service to let go of the job after the agent closed");
    assert!(h.state().accounts.is_empty(), "accounts left after the agent stopped the job: {:?}", h.state().accounts);

    // A refusal is the agent's error.
    h.state().fail.insert("delete-profile".into(), 1);
    h.state().accounts.insert("rujob-0a0b0c0d".into(), "S-1-5-21-1-2-3-9999".into());
    s.record(&Account { name: "rujob-0a0b0c0d".into(), sid: "S-1-5-21-1-2-3-9999".into(), place: "win-pc-3".into() }, false).unwrap();
    let (agent, peer) = tcp_pair();
    serve_over(&s, peer);
    let clean = JobRequest { version: JOB_REQUEST_VERSION.into(), op: "clean".into(), place: "win-pc-3".into(), ..Default::default() };
    let res = ask_service(&Ctx::background(), agent, &clean, None, &mut std::io::sink());
    assert!(res.as_ref().is_err_and(|e| e.contains("place win-pc-3 is not clean")), "a failed clean gave the agent {res:?}");
}

// Intent (the audit found that a probe, clean or update whose context ended
// before the service answered counted as success, so a timed-out probe
// reported the service ready): only a run, which the agent stops on purpose
// as every place does, ends well when its context ends; any other request
// without the service's answer is an error.
#[test]
fn only_a_stopped_run_ends_well_without_the_services_answer() {
    for op in ["probe", "clean", "update", "run"] {
        let (agent, mut peer) = tcp_pair();
        // A service that takes the request and never answers.
        let closer = peer.try_clone().unwrap();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut peer, &mut std::io::sink());
        });
        let ctx = Ctx::background().with_timeout(Duration::from_millis(50));
        let mut req = JobRequest { version: JOB_REQUEST_VERSION.into(), op: op.into(), place: "win-pc-1".into(), ..Default::default() };
        let mut zip = Cursor::new(b"zip!".to_vec());
        let mut body: Option<&mut dyn Read> = None;
        if op == "update" {
            req.archive = "runner-up_9.0.2_windows_amd64.zip".into();
            req.size = 4;
            body = Some(&mut zip);
        }
        let res = ask_service(&ctx, agent, &req, body, &mut std::io::sink());
        ctx.cancel();
        let _ = closer.shutdown(Shutdown::Both);
        if op == "run" {
            assert_eq!(res, Ok(()), "a run the agent stopped failed");
        } else {
            assert!(res.as_ref().is_err_and(|e| e.contains("context deadline exceeded")), "a {op} the service never answered gave {res:?}, want a timeout");
        }
    }
}

// Intent (README, "Windows machines"): a job account's password is long,
// never repeats, fits LogonUser and meets Windows's complexity rule. Windows
// asks for characters from three of the classes upper case, lower case, digit
// and symbol; a random draw can miss a class, so every password must carry
// all four.
#[test]
fn passwords_are_long_random_and_meet_windows_complexity() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2000 {
        let p = new_password();
        assert!((64..=127).contains(&p.len()), "password {} long; want at least 64, and LogonUser takes at most 127", p.len());
        let s = String::from_utf16(&p).unwrap();
        assert!(seen.insert(s.clone()), "a password repeated after {}", seen.len());
        let (mut upper, mut lower, mut digit, mut symbol) = (false, false, false, false);
        for c in s.chars() {
            assert!(c > ' ' && c <= '~', "password holds {c:?}, which is not printable ASCII");
            match c {
                'A'..='Z' => upper = true,
                'a'..='z' => lower = true,
                '0'..='9' => digit = true,
                _ => symbol = true,
            }
        }
        assert!(upper && lower && digit && symbol, "password lacks a class: upper {upper}, lower {lower}, digit {digit}, symbol {symbol}");
    }
}

// A job's files fill every value of the start script, and the fork guard
// runs runner-up's own fork-guard command. What the start script does when
// it runs - the pin, the registration and the hook - is
// the account job start script test's, on Windows.
#[test]
fn account_job_files_fill_the_start_script_and_guard() {
    let files = test_files(&JobRequest::default()).unwrap();
    let start = String::from_utf8_lossy(&files[START_FILE]);
    assert!(!has_placeholder(&start), "the start script has a value left unfilled:\n{start}");
    assert_eq!(
        String::from_utf8_lossy(&files[GUARD_FILE]),
        "& 'C:\\Program Files\\runner-up\\runner-up.exe' fork-guard\nexit $LASTEXITCODE\n",
        "the fork guard"
    );
    let forks = test_files(&JobRequest { allow_forks: true, ..Default::default() }).unwrap();
    assert!(
        !forks.contains_key(GUARD_FILE) && !String::from_utf8_lossy(&forks[START_FILE]).contains("ACTIONS_RUNNER_HOOK_JOB_STARTED"),
        "a place that allows forks still has the fork guard"
    );
}

/// release's test fixture: checksums.txt for runner-up v9.0.2, signed by a
/// throwaway key whose public half is test-key.pub, and the archives it lists.
fn signed_release() -> (release::PublicKey, Vec<u8>, Vec<u8>) {
    let key = base64::engine::general_purpose::STANDARD.decode(read_release("test-key.pub").trim_ascii()).unwrap();
    let key = release::PublicKey::from_bytes(&key.try_into().unwrap()).unwrap();
    (key, read_release("checksums.txt"), read_release("checksums.txt.sig"))
}

fn read_release(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/testdata/release/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

// Intent (README, "Updating runner-up"): handed a release the way the
// coordinator hands it to `runner-up agent update-service`, the service
// installs it only when the signing key vouches for that very archive for its
// own platform, only when it is newer than the service, and never while a place
// runs a job; once it installs one, it takes no job until it restarts as it.
#[test]
fn the_service_installs_only_a_newer_release_the_signing_key_signed() {
    let (key, checksums, sig) = signed_release();
    let zip = read_release("runner-up-v9.0.2-windows-amd64.zip");
    let installed = Arc::new(Mutex::new(Vec::<String>::new()));
    let dir = tempfile::tempdir().unwrap();
    let h = new_fake_host();
    let server = |running: &str| {
        let mut o = test_options(dir.path());
        o.platform = "windows-amd64".into();
        o.keys = vec![key];
        o.running = running.into();
        let (installed, zip) = (installed.clone(), zip.clone());
        o.update = Some(Box::new(move |v: &str, b: &[u8]| {
            installed.lock().unwrap().push(if b == zip { v.to_string() } else { format!("{} bytes that are not the signed archive", b.len()) });
            Ok(())
        }));
        Arc::new(Server::new(o, Box::new(h.clone()), Logger::discard()))
    };
    let s = server(crate::version::version());
    // What the coordinator writes to the agent: a line describing the
    // release, then the archive.
    let hand = |s: &Arc<Server>, name: &str, body: &[u8], sig: &[u8]| {
        let u = ServiceUpdate {
            archive: name.into(),
            size: body.len() as i64,
            checksums: String::from_utf8(checksums.clone()).unwrap(),
            signature: base64::engine::general_purpose::STANDARD.encode(sig),
        };
        let input = [serde_json::to_vec(&u).unwrap(), b"\n".to_vec(), body.to_vec()].concat();
        let mut out = Vec::new();
        let ask = |ctx: &Ctx, u: &ServiceUpdate, body: &mut dyn Read, out: &mut dyn Write| {
            let (agent, peer) = tcp_pair();
            serve_over(s, peer);
            let req = JobRequest {
                version: JOB_REQUEST_VERSION.into(),
                op: "update".into(),
                archive: u.archive.clone(),
                size: u.size,
                checksums: u.checksums.clone(),
                signature: u.signature.clone(),
                ..Default::default()
            };
            ask_service(ctx, agent, &req, Some(body), out)
        };
        let code = update_service_main(&mut input.as_slice(), &mut out, &ask);
        (String::from_utf8_lossy(&out).into_owned(), code)
    };
    let refused = |what: &str, s: &Arc<Server>, name: &str, body: &[u8], sig: &[u8], why: &str| {
        let (out, code) = hand(s, name, body, sig);
        assert!(code != EXIT_DONE && out.contains(why), "{what}: the agent exited {code}, want a refusal saying {why:?}:\n{out}");
        assert!(installed.lock().unwrap().is_empty(), "{what}: the service installed {:?}", installed.lock().unwrap());
    };
    let zip_name = "runner-up-v9.0.2-windows-amd64.zip";
    let mut tampered = zip.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 1;
    refused("an archive changed after signing", &s, zip_name, &tampered, &sig, "the signed checksums list");
    let mut forged = sig.clone();
    forged[0] ^= 1;
    refused("checksums the key did not sign", &s, zip_name, &zip, &forged, "did not sign these checksums");
    let linux = "runner-up-v9.0.2-linux-amd64.tar.gz";
    refused("another platform's signed archive", &s, linux, &read_release(linux), &sig, "windows-amd64");
    refused("the version the service runs", &server("v9.0.2"), zip_name, &zip, &sig, "installs only a newer release");

    // A place running a job keeps the service as it is.
    h.state().block = true;
    let (agent, done) = hold_job(&s, "win-pc-1");
    wait_started(&h, 1);
    refused("a release while a job runs", &s, zip_name, &zip, &sig, "1 places run or clean up a job");
    drop(agent);
    let _ = receive(&done, "the service to end once the agent went");

    let (out, code) = hand(&s, zip_name, &zip, &sig);
    assert!(
        code == EXIT_DONE && out.contains("updating to v9.0.2") && *installed.lock().unwrap() == ["v9.0.2"],
        "a newer signed release: the agent exited {code} and the service installed {:?}:\n{out}",
        installed.lock().unwrap()
    );
    let (out, res) = ask_job_host(&s, &run_request("win-pc-2"));
    assert!(res.is_err() && out.contains("the service is updating"), "the service took a job after installing a release: {res:?} {out}");
}

#[cfg(windows)]
#[path = "jobhost_windows.rs"]
mod windows;
