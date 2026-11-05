//! The end-to-end harness. Intent (README, "How it works", "Pull requests
//! from forks" and "Running as a service"): the coordinator hears of a
//! waiting job from its scale set, acquires it once a place is free, and runs
//! it there through the machine's agent over SSH, handing the runner its
//! registration on standard input only; every runner starts with the fork
//! guard; a place keeps running through a coordinator restart and a crashed
//! place is cleaned up.
//!
//! The coordinator, its place runners and the agent are the real programs.
//! GitHub's Actions service is a local stand-in speaking the scale set
//! protocol, ssh is a stand-in that runs the agent's command line on this
//! computer, and docker is a stand-in whose "container" is a process that
//! prints what the runner prints when its job starts and ends.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use super::{ACTIONS_ENV, PASSES_ENV, SELF_ENV, SERVE_ENV, runner_up, temp_dir};
use crate::config::{self, Config, Machine, PROVIDER_DOCKER, Place, Pool};
use crate::coordinator::{BusyReason, PlaceRun, ssh_args};
use crate::go::time::Time;
use crate::testserver::{Request, Response, Server};

/// What GitHub's Actions service stand-in has seen and holds.
#[derive(Clone, Default)]
pub struct Seen {
    /// Messages not yet acknowledged, by scale set name: id and body.
    queues: BTreeMap<String, Vec<(i64, String)>>,
    next_msg: i64,
    /// Scale set ids by name.
    sets: BTreeMap<String, i64>,
    /// Sessions opened and closed.
    pub opened: i64,
    pub closed: i64,
    /// Polls since the last session opened.
    pub polls: i64,
    pub acquired: Vec<i64>,
    /// Registrations handed out, in order.
    pub jits: Vec<String>,
    /// Runners removed.
    pub removed: Vec<i64>,
    /// Requests the stand-in does not know.
    pub stray: Vec<String>,
    /// The runners registered, by id; busy are those GitHub gave a job,
    /// which it refuses to remove; refused are the removals it refused, in
    /// order.
    names: BTreeMap<i64, String>,
    busy: BTreeMap<i64, bool>,
    pub refused: Vec<i64>,
    /// At each removal, whether the place's container ran, when
    /// container_up is set.
    pub up: Vec<bool>,
}

/// Answers the scale set protocol as GitHub's Actions service does, for one
/// repository's scale sets, one per pool.
pub struct ActionsStandIn {
    pub url: String,
    seen: Arc<Mutex<Seen>>,
    /// When set, says whether the place's container runs.
    container_up: Arc<Mutex<Option<Box<dyn Fn() -> bool + Send>>>>,
}

const SETS: &str = "/tenant/_apis/runtime/runnerscalesets";
const AGENTS: &str = "/tenant/_apis/distributedtask/pools/0/agents/";

impl ActionsStandIn {
    pub fn new() -> ActionsStandIn {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let container_up: Arc<Mutex<Option<Box<dyn Fn() -> bool + Send>>>> = Arc::default();
        let url = Arc::new(Mutex::new(String::new()));
        let server = {
            let (seen, container_up, url) = (seen.clone(), container_up.clone(), url.clone());
            Server::start(move |r| serve(r, &seen, &container_up, &url.lock().unwrap()))
        };
        *url.lock().unwrap() = server.url.clone();
        ActionsStandIn { url: server.url, seen, container_up }
    }

    /// Queues one message holding jobs, each a scale set job message, for
    /// the ru-linux scale set every setup here serves.
    pub fn send(&self, jobs: &[Value]) {
        self.send_to("ru-linux", jobs)
    }

    /// Queues one message holding jobs for pool's scale set.
    pub fn send_to(&self, pool: &str, jobs: &[Value]) {
        let body = Value::from(jobs.to_vec()).to_string();
        let mut a = self.seen.lock().unwrap();
        a.next_msg += 1;
        let id = a.next_msg;
        a.queues.entry(pool.into()).or_default().push((id, body));
    }

    /// A copy of what the stand-in has seen.
    pub fn read(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }

    /// Has GitHub give runner id a job, so it refuses to remove it.
    pub fn give_job(&self, id: i64) {
        self.seen.lock().unwrap().busy.insert(id, true);
    }

    pub fn set_container_up(&self, f: impl Fn() -> bool + Send + 'static) {
        *self.container_up.lock().unwrap() = Some(Box::new(f));
    }
}

/// The shape of the token the service hands a client: a JWT, read for its
/// expiry only.
fn admin_token() -> String {
    let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let exp = (Time::now().0 + chrono::Duration::hours(1)).timestamp();
    format!("{}.{}.{}", enc(br#"{"alg":"HS256","typ":"JWT"}"#), enc(format!(r#"{{"exp":{exp}}}"#).as_bytes()), enc(b"stand-in"))
}

fn set(a: &Seen, name: &str) -> Value {
    json!({"id": a.sets[name], "name": name, "runnerGroupId": 1, "labels": [{"type": "System", "name": name}]})
}

fn serve(r: &Request, seen: &Mutex<Seen>, container_up: &Mutex<Option<Box<dyn Fn() -> bool + Send>>>, url: &str) -> Response {
    let path = r.path.as_str();
    let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    let mut a = seen.lock().unwrap();
    // The set's name and what follows its id, for a set the stand-in made.
    let in_set = path.strip_prefix(&format!("{SETS}/")).and_then(|tail| {
        let (id, rest) = tail.split_once('/').unwrap_or((tail, ""));
        let name = a.sets.iter().find(|(_, v)| v.to_string() == id)?.0.clone();
        Some((name, rest.to_string()))
    });
    let (set_name, set_rest) = in_set.clone().unwrap_or_default();
    let method = r.method.as_str();
    let ok = |v: Value| Response::json(200, &v);
    let none = |code| Response::json(code, &Value::Null);
    if method == "POST" && path.ends_with("/actions/runners/registration-token") {
        return Response::json(201, &json!({"token": "registration-token"}));
    }
    if method == "POST" && path.ends_with("/actions/runner-registration") {
        return ok(json!({"url": format!("{url}/tenant/"), "token": admin_token()}));
    }
    if method == "GET" && path == SETS {
        let name = r.query_value("name").unwrap_or("");
        if !a.sets.contains_key(name) {
            return ok(json!({"count": 0, "value": []}));
        }
        return ok(json!({"count": 1, "value": [set(&a, name)]}));
    }
    if method == "POST" && path == SETS {
        let name = body["name"].as_str().unwrap_or("").to_string();
        let next = 7 + a.sets.len() as i64;
        a.sets.entry(name.clone()).or_insert(next);
        return ok(set(&a, &name));
    }
    if method == "POST" && in_set.is_some() && set_rest == "sessions" {
        a.opened += 1;
        a.polls = 0;
        return ok(json!({
            "sessionId": format!("00000000-0000-4000-8000-{:012}", a.opened), "ownerName": "runner-up",
            "messageQueueUrl": format!("{url}/queue/{set_name}"), "messageQueueAccessToken": "queue-token",
            "statistics": {},
        }));
    }
    if method == "DELETE" && in_set.is_some() && set_rest.starts_with("sessions/") {
        a.closed += 1;
        return none(204);
    }
    if let Some(queue) = path.strip_prefix("/queue/") {
        if method == "GET" {
            a.polls += 1;
            let last: i64 = r.query_value("lastMessageId").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some((id, body)) = a.queues.get(queue).and_then(|q| q.iter().find(|(id, _)| *id > last)) {
                return ok(json!({"messageId": id, "messageType": "RunnerScaleSetJobMessages", "body": body, "statistics": {}}));
            }
            // GitHub holds a poll open until a message comes; a short wait
            // keeps the stand-in's polls cheap.
            drop(a);
            std::thread::sleep(Duration::from_millis(100));
            return none(202);
        }
        if method == "DELETE" {
            let (name, id) = queue.split_once('/').unwrap_or((queue, ""));
            let id: i64 = id.parse().unwrap_or(0);
            a.queues.entry(name.into()).or_default().retain(|(m, _)| *m > id);
            return none(204);
        }
    }
    if method == "POST" && in_set.is_some() && set_rest == "acquirejobs" {
        let ids: Vec<i64> = serde_json::from_value(body).unwrap_or_default();
        a.acquired.extend(&ids);
        return ok(json!({"count": ids.len(), "value": ids}));
    }
    if method == "POST" && in_set.is_some() && set_rest == "generatejitconfig" {
        let jit = format!("stand-in-registration-{}-{}", a.jits.len() + 1, Time::now().unix_nanos());
        a.jits.push(jit.clone());
        let id = a.jits.len() as i64;
        let name = body["name"].as_str().unwrap_or("").to_string();
        a.names.insert(id, name.clone());
        return ok(json!({"runner": {"id": id, "name": name}, "encodedJITConfig": jit}));
    }
    if method == "DELETE"
        && let Some(id) = path.strip_prefix(AGENTS)
    {
        let id: i64 = id.parse().unwrap_or(0);
        if a.busy.get(&id).copied().unwrap_or(false) {
            // GitHub's answer, as it refused a runner it had given a job.
            a.refused.push(id);
            let message = format!("Runner {} is currently running a job and cannot be deleted.", a.names.get(&id).cloned().unwrap_or_default());
            return Response::json(400, &json!({"typeName": "JobStillRunningException", "message": message}));
        }
        a.removed.push(id);
        if let Some(up) = container_up.lock().unwrap().as_ref() {
            a.up.push(up());
        }
        return none(204);
    }
    let query = if r.query.is_empty() { String::new() } else { format!("?{}", r.query) };
    a.stray.push(format!("{method} {path}{query}"));
    none(404)
}

/// The stand-in ssh runs the agent's command line, its last argument, on
/// this computer, in $HOME as ssh does.
pub(super) const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$E2E_DIR/ssh.log"
for last; do :; done
cd "$HOME"
exec sh -c "$last"
"#;

/// The stand-in docker's "container" for a place is the docker run process
/// itself. What a real docker would keep in the container's settings - its
/// arguments and any runner variable in its own environment, which -e NAME
/// copies - goes to settings, and its image to images. It then runs the
/// container's entrypoint and arguments as given, on docker's own standard
/// input, with the stand-in runner run.sh in place of the image's.
const FAKE_DOCKER: &str = r#"#!/bin/bash
d=$E2E_DIR/docker
printf '%s\n' "$*" >> "$d/docker.log"
name=${!#}
case $1 in
run)
  printf '%s %s\n' "$*" "$(env | grep '^ACTIONS_RUNNER_')" >> "$d/settings"
  shift
  entry= image= args=()
  while [ $# -gt 0 ]; do
    case $1 in
    --name) name=$2; shift ;;
    --entrypoint) entry=$2; shift ;;
    --rm|-i) ;;
    -*) shift ;;
    *) image=$1; shift; break ;;
    esac
    shift
  done
  printf '%s\n' "$image" >> "$d/images"
  for a in "$@"; do [ "$a" = /home/runner/run.sh ] && a=$d/run.sh; args+=("$a"); done
  export d name
  exec "$entry" "${args[@]}"
  ;;
rm)
  if [ -e "$d/$name.pid" ]; then
    read pid agent < "$d/$name.pid"
    kill "$pid"
    rm -f "$d/$name.pid"
    echo "$name"
    exit 0
  fi
  echo "Error response from daemon: No such container: $name" >&2
  exit 1
  ;;
esac
"#;

/// The stand-in runner records the registration it finds in its variable,
/// then its container in NAME.pid (the docker run process it replaced, and
/// the agent), and prints what a runner prints. Its job starts once go.PID
/// exists and ends once done.PID does; the job is named build unless job.PID
/// names it.
const FAKE_RUNNER: &str = r#"#!/bin/sh
printf '%s\n' "$ACTIONS_RUNNER_INPUT_JITCONFIG" >> "$d/registrations"
echo "$$ $PPID" > "$d/$name.pid"
echo "√ Connected to GitHub"
echo "Listening for Jobs"
until [ -e "$d/go.$$" ] || [ -e "$d/stop" ]; do sleep 0.05; done
job=build
[ -e "$d/job.$$" ] && job=$(cat "$d/job.$$")
echo "Running job: $job"
until [ -e "$d/done.$$" ] || [ -e "$d/stop" ]; do sleep 0.05; done
echo "Job $job completed with result: Succeeded"
rm -f "$d/$name.pid"
"#;

fn write_program(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Whether the tests that run the coordinator, place runners and agent as
/// programs are to be skipped, as go test -short skipped them.
pub fn short() -> bool {
    std::env::var_os("RUNNER_UP_TEST_SHORT").is_some()
}

/// One coordinator setup: a machine reached over (stand-in) SSH with one
/// Docker place, serving octo-org/app's ru-linux pool.
pub struct E2e {
    _tmp: tempfile::TempDir,
    pub dir: PathBuf,
    pub config: String,
    pub cfg: Config,
    pub actions: ActionsStandIn,
    pub env: Vec<(String, String)>,
    serving: Option<Child>,
    served: i32,
}

impl E2e {
    pub fn new() -> E2e {
        let tmp = temp_dir();
        let dir = tmp.path().to_path_buf();
        for sub in ["bin", "docker", "state", "machine"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        write_program(&dir.join("bin/ssh"), FAKE_SSH);
        write_program(&dir.join("bin/docker"), FAKE_DOCKER);
        write_program(&dir.join("docker/run.sh"), FAKE_RUNNER);
        let cfg = Config {
            state_dir: dir.join("state").to_string_lossy().into_owned(),
            pools: vec![Pool { label: "ru-linux".into(), ..Default::default() }],
            tick_seconds: 1,
            machines: vec![Machine {
                name: "box".into(),
                ssh: vec!["ci@box".into()],
                agent: runner_up().into(),
                cores: 4,
                memory_mb: 8192,
                room_cores: 1,
                room_memory_mb: 1024,
                places: vec![Place {
                    id: "box-1".into(),
                    provider: PROVIDER_DOCKER.into(),
                    pools: vec!["ru-linux".into()],
                    cores: 2,
                    memory_mb: 4096,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let path = std::env::var("PATH").unwrap_or_default();
        let env = vec![
            ("PATH".into(), format!("{}:{path}", dir.join("bin").display())),
            ("E2E_DIR".into(), dir.to_string_lossy().into_owned()),
            // The agent's own folder on the machine.
            ("XDG_STATE_HOME".into(), dir.join("machine").to_string_lossy().into_owned()),
        ];
        let mut e = E2e {
            config: dir.join("config.json").to_string_lossy().into_owned(),
            _tmp: tmp,
            dir,
            cfg,
            actions: ActionsStandIn::new(),
            env,
            serving: None,
            served: 0,
        };
        e.cfg.github.repos = vec!["octo-org/app".into()];
        e.save();
        e
    }

    /// Writes the setup file and reads it back, as the coordinator will.
    pub fn save(&mut self) {
        config::save_config(&self.config, &self.cfg).unwrap();
        self.cfg = config::load_config(&self.config).unwrap();
    }

    /// runner-up with args, in this setup's environment.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = super::command(args);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        c
    }

    /// Starts the coordinator; its output goes to coordinator-N.log.
    pub fn serve(&mut self) {
        self.serve_as(Path::new(runner_up()));
    }

    /// Starts program as the coordinator, as its supervisor does with
    /// program/current; its output goes to coordinator-N.log.
    pub fn serve_as(&mut self, program: &Path) {
        self.served += 1;
        let out = std::fs::File::create(self.dir.join(format!("coordinator-{}.log", self.served))).unwrap();
        let mut c = Command::new(program);
        c.arg("serve").envs(self.env.iter().map(|(k, v)| (k, v)));
        c.env(SERVE_ENV, &self.config)
            .env(ACTIONS_ENV, &self.actions.url)
            .env(SELF_ENV, runner_up())
            .env(PASSES_ENV, self.dir.join("passes"))
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out);
        self.serving = Some(c.spawn().unwrap());
    }

    /// Says whether the running coordinator's log holds text.
    pub fn logged(&self, text: &str) -> bool {
        let log = std::fs::read_to_string(self.dir.join(format!("coordinator-{}.log", self.served)));
        log.is_ok_and(|l| l.contains(text))
    }

    /// Waits for the coordinator to stop on its own and returns its exit code.
    pub fn exited(&mut self) -> i32 {
        let mut child = self.serving.take().expect("the coordinator runs");
        let Some(status) = crate::agent::tests::wait_timeout(&mut child, Duration::from_secs(60)) else {
            let _ = child.kill();
            panic!("the coordinator did not stop within 60 seconds\n{}", self.logs());
        };
        crate::agent::exit_code(&Ok(status))
    }

    /// Stops the coordinator as systemd does, and returns its exit code.
    pub fn stop(&mut self) -> i32 {
        let mut child = self.serving.take().expect("the coordinator runs");
        signal(child.id() as i32, libc::SIGTERM);
        let Some(status) = crate::agent::tests::wait_timeout(&mut child, Duration::from_secs(30)) else {
            let _ = child.kill();
            panic!("the coordinator did not stop within 30 seconds");
        };
        crate::agent::exit_code(&Ok(status))
    }

    /// Ends everything a test left running.
    fn stop_all(&mut self) {
        if let Some(mut c) = self.serving.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::write(self.dir.join("docker/stop"), "");
        for p in self.cfg.places() {
            for name in ["place.json", "last-place.json"] {
                if let Ok(run) = config::read_json::<PlaceRun>(&self.cfg.place_dir(&p.id).join(name))
                    && run.pid > 1
                {
                    // A place runner leads its own process group.
                    signal(-(run.pid as i32), libc::SIGKILL);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    pub fn eventually(&self, what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ok() {
            assert!(Instant::now() < deadline, "after 30 seconds: {what}\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the coordinator has made n whole passes of its serve loop
    /// since the call, each tidying and placing, so whatever those passes
    /// would do is done. A pass under way at the call may have begun before
    /// it, so it counts for none.
    pub fn passes(&self, n: usize) {
        let count = || std::fs::read_to_string(self.dir.join("passes")).unwrap_or_default().matches('\n').count();
        let want = count() + n + 1;
        self.eventually(&format!("the coordinator makes {n} more passes"), || count() >= want);
    }

    /// Every log the run wrote, for a failure message.
    pub fn logs(&self) -> String {
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut glob = |dir: &Path, prefix: &str| {
            let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().unwrap().to_string_lossy();
                    name.starts_with(prefix) && name.ends_with(".log")
                })
                .collect();
            found.sort();
            paths.extend(found);
        };
        glob(&self.dir, "coordinator-");
        glob(&self.cfg.logs_dir(), "");
        paths.push(self.dir.join("ssh.log"));
        paths.push(self.dir.join("docker/docker.log"));
        let mut b = String::new();
        for p in paths {
            let data = std::fs::read_to_string(&p).unwrap_or_default();
            b += &format!("--- {}\n{data}", p.file_name().unwrap().to_string_lossy());
        }
        b
    }

    /// box-1's running stand-in container: its pid and its agent's, or
    /// zeros when none runs.
    pub fn container(&self) -> (i32, i32) {
        self.container_in("box-1")
    }

    /// Place id's running stand-in container.
    pub fn container_in(&self, id: &str) -> (i32, i32) {
        container_in(&self.dir, id)
    }

    /// Has the Actions stand-in record, at each removal, whether box-1's
    /// container runs.
    pub fn watch_container(&self) {
        let dir = self.dir.clone();
        self.actions.set_container_up(move || alive(container_in(&dir, "box-1").0));
    }

    /// What each container found in its runner's variable.
    pub fn registrations(&self) -> Vec<String> {
        let data = std::fs::read_to_string(self.dir.join("docker/registrations")).unwrap_or_default();
        data.split_whitespace().map(String::from).collect()
    }

    pub fn start_job(&self, pid: i32) {
        std::fs::write(self.dir.join(format!("docker/go.{pid}")), "").unwrap();
    }

    /// Starts the job a container's runner took, named name as GitHub names
    /// it.
    pub fn start_job_named(&self, pid: i32, name: &str) {
        std::fs::write(self.dir.join(format!("docker/job.{pid}")), name).unwrap();
        self.start_job(pid);
    }

    pub fn end_job(&self, pid: i32) {
        std::fs::write(self.dir.join(format!("docker/done.{pid}")), "").unwrap();
    }

    pub fn place_run(&self) -> Option<PlaceRun> {
        self.run_in("box-1")
    }

    pub fn run_in(&self, id: &str) -> Option<PlaceRun> {
        config::read_json(&self.cfg.place_dir(id).join("place.json")).ok()
    }

    pub fn busy(&self) -> bool {
        crate::lock::held_by(&self.cfg.place_dir("box-1").join("busy.lock")).unwrap_or(true)
    }

    /// Why place id is busy, as status reads it: None when free.
    pub fn held_by(&self, id: &str) -> Option<BusyReason> {
        crate::coordinator::held_by(&self.cfg, id)
    }

    /// The state the coordinator last saved, which status reads.
    pub fn saved(&self) -> Value {
        config::read_json(&Path::new(&self.cfg.state_dir).join("state.json")).unwrap_or(Value::Null)
    }

    /// The jobs the coordinator saved as waiting.
    pub fn waiting(&self) -> Vec<String> {
        let saved = self.saved();
        let jobs = saved["waiting"].as_array().cloned().unwrap_or_default();
        jobs.iter().map(|j| j["name"].as_str().unwrap_or("").to_string()).collect()
    }

    /// runner-up status for this setup.
    pub fn status(&self) -> String {
        let (code, out) = crate::agent::tests::run(&mut self.command(&["status", "--config", &self.config]), "");
        assert_eq!(code, 0, "status failed:\n{out}");
        out
    }

    /// Runs runner-up status beside the coordinator and checks it says what
    /// status says of the same setup and state in this process;
    /// status_says_each_place_and_job_in_one_line owns what that is.
    pub fn status_as_read(&self) -> String {
        let out = self.status();
        let mut here = Vec::new();
        crate::coordinator::status(self.cfg.clone(), &mut here);
        let here = String::from_utf8(here).unwrap();
        assert_eq!(out, here, "runner-up status said\n{out}\nwant what its setup and state say\n{here}");
        out
    }

    pub fn lines(&self, path: &Path) -> Vec<String> {
        let data = std::fs::read_to_string(path).unwrap_or_default();
        data.trim().split('\n').map(String::from).collect()
    }

    /// Checks no registration appears in any command line or log.
    pub fn no_secrets(&self, jits: &[String]) {
        let logs = self.logs();
        for jit in jits {
            assert!(!logs.contains(jit.as_str()), "a runner registration appeared in a command line or log:\n{logs}");
        }
    }
}

impl Drop for E2e {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn container_in(dir: &Path, id: &str) -> (i32, i32) {
    let Ok(data) = std::fs::read_to_string(dir.join(format!("docker/runner-up-{id}.pid"))) else {
        return (0, 0);
    };
    let mut words = data.split_whitespace().map(|w| w.parse().unwrap_or(0));
    (words.next().unwrap_or(0), words.next().unwrap_or(0))
}

fn signal(pid: i32, sig: i32) {
    unsafe { libc::kill(pid, sig) };
}

pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// How ssh to m starts, as a stand-in ssh logs its arguments:
/// coordinator's ssh_args_are_batch_kept_alive_and_timed owns its words.
pub fn ssh_to(m: &Machine) -> String {
    ssh_args(m)[1..].join(" ")
}

/// The first job of a recorded waiting-jobs message: octo-org/app's "verify
/// / linux" in the ru-linux pool, request 9101, queued a minute ago.
pub fn verify_job() -> Value {
    waiting_job(0)
}

/// The second job of the same message: octo-org/app's "affected-journeys" in
/// the ru-linux pool, request 9102, queued a minute ago.
pub fn journeys_job() -> Value {
    waiting_job(1)
}

/// Job i of the first recorded waiting-jobs message, queued a minute ago.
pub fn waiting_job(i: usize) -> Value {
    recorded_job(0, i)
}

/// Job n of message m of the recorded waiting-jobs messages, queued a minute
/// ago.
pub fn recorded_job(m: usize, n: usize) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/coordinator/messages/waiting-jobs.json");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut job = f["messages"][m]["message"]["JobAvailableMessages"][n].clone();
    job["queueTime"] = json!(Time::now().sub_dur(Duration::from_secs(60)).rfc3339());
    job
}

/// job as a message of kind, with fields added.
pub fn job_message(job: &Value, kind: &str, fields: Value) -> Value {
    let mut m = job.clone();
    m["messageType"] = json!(kind);
    for (k, v) in fields.as_object().unwrap() {
        m[k] = v.clone();
    }
    m
}

/// GitHub's message that job was cancelled before a runner took it, as it
/// recorded one: no runner, and a finish time.
pub fn cancelled(job: &Value) -> Value {
    job_message(job, "JobCompleted", json!({"result": "canceled", "finishTime": Time::now().rfc3339()}))
}
