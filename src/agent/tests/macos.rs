// Intent (README, "Macs", "Pull requests from forks" and "Trade-offs"). The
// agent runs one job in a fresh tart clone of the place's image, sized to the
// place and seeing only a read-only folder with the runner and the fork guard;
// the runner gets its registration on its input only and the guard as its
// job-started hook; a new boot session in the clone ends the job; the clone is
// deleted whatever happened; the agent waits for a free macOS VM before it
// clones. On the machine itself, the agent runs the runner in a folder of its
// own, ends the runner and everything the job started when its input closes,
// and cleans up what an agent killed outright left.
//
// Intent (a bug moving a fleet to runner-up found: runner-up's predecessor
// kept tools such as Node in a host place's tool cache between jobs, and jobs
// relied on it). A host place keeps its runner's tool cache between jobs, so
// a tool one job downloads is there for the next; a tart place points the
// runner at the tool cache its image was set up with.
//
// The agent is the real program, run with a stand-in runner whose run.sh
// prints what GitHub's runner prints and waits for the test. tart is a
// stand-in that keeps its virtual machines as folders, runs a "VM" as a
// process, and runs `tart exec` here with the guest's home folder and shared
// folder mapped to folders of the test.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{event, run, wait_timeout};
use crate::agent::tart::clone_name;
use crate::agent::{EXIT_DONE, EXIT_FAILED, exit_code};

/// The stand-in runner's run.sh. It records what the runner would read from
/// its environment, then waits for go, starts a job that leaves a process
/// running, and waits for done.
pub(super) const FAKE_RUN_SH: &str = r#"#!/bin/bash
d=$MAC_DIR
printf '%s\n' "$ACTIONS_RUNNER_INPUT_JITCONFIG" >> "$d/registrations"
printf '%s\n' "${ACTIONS_RUNNER_HOOK_JOB_STARTED:-none}" >> "$d/hooks"
# What the tool cache held when the job began; then the job caches a tool
# there, as setup-node does after a download.
printf '%s %s\n' "${RUNNER_TOOL_CACHE:-none}" "$(ls "${RUNNER_TOOL_CACHE:-/nonexistent}" 2>/dev/null | tr '\n' ,)" >> "$d/toolcaches"
[ -z "${RUNNER_TOOL_CACHE:-}" ] || { mkdir -p "$RUNNER_TOOL_CACHE/node/24.22.1"; touch "$RUNNER_TOOL_CACHE/node/24.22.1/arm64.complete"; }
echo $$ > "$d/runner.pid"
echo "√ Connected to GitHub"
echo "Listening for Jobs"
until [ -e "$d/go" ]; do sleep 0.05; done
echo "Running job: build"
sleep 300 &
echo $! > "$d/job.pid"
until [ -e "$d/done" ]; do
  # It answers each ping, to show it still runs.
  [ -e "$d/ping" ] && rm -f "$d/ping" && echo >> "$d/pongs"
  sleep 0.05
done
echo "Job build completed with result: Succeeded"
"#;

/// Keeps each VM as a folder of $MAC_DIR/tart/vms. A running VM is its tart
/// run process, recorded in running; a VM's os says what it runs.
const FAKE_TART: &str = r#"#!/bin/bash
t=$MAC_DIR/tart
mkdir -p "$t/vms"
printf '%s\n' "$*" >> "$t/tart.log"
cmd=$1; shift
case $cmd in
list)
  sep=""; printf '['
  for v in "$t"/vms/*/; do
    [ -d "$v" ] || continue
    r=false; [ -e "$v/running" ] && r=true
    printf '%s{"Name":"%s","Source":"local","Running":%s,"State":"x"}' "$sep" "$(basename "$v")" "$r"; sep=,
  done
  printf ']\n';;
get) printf '{"OS":"%s"}\n' "$(cat "$t/vms/$1/os")";;
clone) mkdir -p "$t/vms/$2"; echo darwin > "$t/vms/$2/os"; echo "$1" > "$t/vms/$2/image";;
set) n=$1; shift; echo "$*" > "$t/vms/$n/size";;
run)
  for n; do :; done; v=$t/vms/$n
  for a; do case $a in --dir=runner-up:*) d=${a#--dir=runner-up:}; echo "${d%:ro}" > "$v/share";; esac; done
  uuidgen > "$v/boot"
  echo $$ > "$v/running"
  trap 'rm -f "$v/running"; exit 0' TERM INT
  while [ -e "$v/running" ]; do sleep 0.05; done;;
exec)
  [ "$1" = -i ] && shift
  v=$t/vms/$1; shift
  [ -e "$v/running" ] || { echo "the VM is not running" >&2; exit 1; }
  case $1 in
  /usr/bin/true) exit 0;;
  /usr/sbin/sysctl) cat "$v/boot";;
  *)
    mkdir -p "$v/home"
    args=("$@")
    for i in "${!args[@]}"; do
      [ "${args[$i]}" = "/Volumes/My Shared Files/runner-up" ] && args[$i]=$(cat "$v/share")
    done
    HOME=$v/home exec "${args[@]}";;
  esac;;
stop)
  v=$t/vms/${@: -1}
  p=$(cat "$v/running" 2>/dev/null); [ "${p:-0}" -gt 1 ] && kill "$p"
  rm -f "$v/running";;
delete) [ -d "$t/vms/$1" ] || { echo "the specified VM \"$1\" does not exist" >&2; exit 1; }; rm -rf "$t/vms/$1";;
esac
"#;

pub(super) fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub(super) fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// One machine for the agent: a folder holding the agent's state, the
/// stand-in tart and what the stand-in runner records.
struct Mac {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    state: PathBuf,
    env: Vec<(String, String)>,
}

impl Mac {
    fn new() -> Option<Mac> {
        if std::env::var_os("RUNNER_UP_TEST_SHORT").is_some() {
            eprintln!("runs the agent as a program");
            return None;
        }
        // The runner runs its hook's path unquoted, so the agent's folder must
        // have no space.
        let tmp = crate::cmd::tests::temp_dir();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        write_exec(&dir.join("bin/tart"), FAKE_TART);
        let (archive, _) = super::stand_in_runner(FAKE_RUN_SH);
        std::fs::write(dir.join("fake-runner.tar.gz"), archive).unwrap();
        let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let env = vec![
            ("MAC_DIR".into(), dir.to_string_lossy().into_owned()),
            ("PATH".into(), path),
            ("RUNNER_UP_TEST_RUNNER_ARCHIVE".into(), dir.join("fake-runner.tar.gz").to_string_lossy().into_owned()),
        ];
        Some(Mac { state: dir.join("state"), _tmp: tmp, dir, env })
    }

    /// A Mac for a tart place, which the agent refuses anywhere but a Mac
    /// with Apple silicon.
    fn tart() -> Option<Mac> {
        if !(cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")) {
            eprintln!("tart places run only on Macs with Apple silicon");
            return None;
        }
        Mac::new()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = crate::cmd::tests::command(args);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        c
    }

    fn run(&self, jit: &str, args: &[&str]) -> Agent {
        let state = self.state.to_string_lossy().into_owned();
        let mut all = vec!["agent", "run", "--state-dir", &state];
        all.extend(args);
        let out = self.dir.join(format!("agent-{}.out", jit));
        let f = std::fs::File::create(&out).unwrap();
        let mut child = self.command(&all).stdin(Stdio::piped()).stdout(f.try_clone().unwrap()).stderr(f).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{jit}").unwrap();
        Agent { child, input: Some(input), out }
    }

    fn eventually(&self, what: &str, a: Option<&Agent>, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ok() {
            if Instant::now() > deadline {
                let out = a.map(Agent::output).unwrap_or_default();
                let log = std::fs::read_to_string(self.dir.join("tart/tart.log")).unwrap_or_default();
                panic!("after 30 seconds: {what}\nagent:\n{out}\ntart:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid(&self, name: &str) -> i32 {
        std::fs::read_to_string(self.dir.join(name)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    fn lines(&self, name: &str) -> Vec<String> {
        let s = std::fs::read_to_string(self.dir.join(name)).unwrap_or_default();
        s.trim().split('\n').map(String::from).collect()
    }

    /// Counts the tart commands run so far that begin with prefix.
    fn tart_runs(&self, prefix: &str) -> usize {
        self.lines("tart/tart.log").iter().filter(|l| l.starts_with(prefix)).count()
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }

    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.join(name));
    }

    fn vm(&self, name: &str) -> PathBuf {
        self.dir.join("tart/vms").join(name)
    }

    /// A VM that runner-up did not start, running system.
    fn running_vm(&self, name: &str, system: &str) {
        std::fs::create_dir_all(self.vm(name)).unwrap();
        std::fs::write(self.vm(name).join("os"), format!("{system}\n")).unwrap();
        std::fs::write(self.vm(name).join("running"), "0\n").unwrap();
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        for name in ["runner.pid", "job.pid"] {
            let pid = self.pid(name);
            if pid > 1 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        for vm in std::fs::read_dir(self.dir.join("tart/vms")).into_iter().flatten().flatten() {
            let pid: i32 = std::fs::read_to_string(vm.path().join("running")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            if pid > 1 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// One `agent run` on the mac, its input held open.
struct Agent {
    child: Child,
    input: Option<std::process::ChildStdin>,
    out: PathBuf,
}

impl Agent {
    fn output(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    /// Waits for the agent to end and returns its exit code.
    fn exit(&mut self) -> i32 {
        let status = wait_timeout(&mut self.child, Duration::from_secs(60));
        let status = status.unwrap_or_else(|| panic!("the agent did not end within a minute:\n{}", self.output()));
        exit_code(&Ok(status))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs a job-started hook with a saved event as the runner would, and says
/// whether it allowed the job.
fn guard(hook: &Path, name: &str) -> bool {
    let mut c = Command::new("bash");
    c.arg(hook).env("GITHUB_EVENT_PATH", event(name));
    run(&mut c, "").0 == 0
}

const SEQUOIA: &str = "ghcr.io/cirruslabs/macos-sequoia-base:latest";

#[test]
fn tart_place_runs_the_job_in_a_fresh_clone_and_deletes_it() {
    let Some(m) = Mac::tart() else { return };
    let jit = "eyJydW5uZXIiOiJydW5uZXItdXAtbWFjLTEifQ-mac-1";
    let mut a = m.run(
        jit,
        &["--provider", "tart", "--place", "mac-1", "--image", SEQUOIA, "--cores", "4", "--memory", "8192", "--boot-timeout", "30", "--guest-check", "1"],
    );
    let vm = m.vm(&clone_name("mac-1"));
    m.eventually("the runner starts in the clone", Some(&a), || m.pid("runner.pid") > 0);

    // The clone is of the place's image, sized to the place, and sees only
    // the agent's read-only folder for the place.
    let image = std::fs::read_to_string(vm.join("image")).unwrap_or_default();
    assert_eq!(image.trim(), SEQUOIA, "the clone's image");
    let size = std::fs::read_to_string(vm.join("size")).unwrap_or_default();
    assert_eq!(size.trim(), "--cpu 4 --memory 8192", "the clone's size");
    let share = m.state.join("places/mac-1/share");
    let log = m.lines("tart/tart.log");
    let want_run = format!("run --no-graphics --dir=runner-up:{}:ro {}", share.display(), clone_name("mac-1"));
    assert!(log.contains(&want_run), "tart ran {log:?}, want {want_run:?} among them");
    let mut names: Vec<String> = std::fs::read_dir(&share).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names.join(" "), "runner-up runner.tar.gz", "the clone sees");

    // The runner got its registration on its input, and the fork guard as its
    // hook, from the clone's home folder; the guard refuses a fork's pull
    // request there and allows the repository's own.
    assert_eq!(m.lines("registrations"), [jit], "the runner's registrations");
    let hook = vm.join("home/.runner-up/fork-guard.sh");
    assert_eq!(m.lines("hooks"), [hook.to_string_lossy()], "the runner's hook");
    assert!(
        !guard(&hook, "pull-request-521-fork.json") && guard(&hook, "pull-request-527-same-repo.json"),
        "the clone's fork guard does not decide as the guard does"
    );
    for line in &log {
        assert!(!line.contains(jit), "the registration appeared in a tart command line: {line:?}");
    }

    // The job runs and ends; the clone is stopped and deleted.
    m.touch("go");
    m.eventually("the job starts", Some(&a), || a.output().contains("Running job: build"));
    m.touch("done");
    let code = a.exit();
    assert_eq!(code, EXIT_DONE, "the agent exited {code}:\n{}", a.output());
    assert!(!vm.exists() && !share.exists(), "the clone or its folder outlived the job: {} {}", vm.exists(), share.exists());
    assert!(a.output().contains("runner-up agent: place mac-1 finished"), "the agent said:\n{}", a.output());
}

#[test]
fn tart_place_ends_the_job_when_the_clones_macos_restarts() {
    let Some(m) = Mac::tart() else { return };
    let mut a = m.run("jit-mac-2", &["--provider", "tart", "--place", "mac-2", "--image", SEQUOIA, "--boot-timeout", "30", "--guest-check", "1"]);
    let vm = m.vm(&clone_name("mac-2"));
    m.eventually("the runner starts in the clone", Some(&a), || m.pid("runner.pid") > 0);
    m.touch("go");
    m.eventually("the job starts", Some(&a), || a.output().contains("Running job: build"));
    // The agent has read the clone's first boot session once a second reading
    // begins. The clone then panics and restarts, with a new boot session. The
    // runner's tart exec goes on waiting, as it did on the predecessor's Mac.
    let sysctl = format!("exec {} /usr/sbin/sysctl", clone_name("mac-2"));
    m.eventually("the agent reads the clone's boot session twice", Some(&a), || m.tart_runs(&sysctl) >= 2);
    std::fs::write(vm.join("boot"), "0F9A2B4C-7D1E-4C2A-8B5F-3E6D7A8B9C0D\n").unwrap();
    let code = a.exit();
    assert_eq!(code, EXIT_FAILED, "the agent exited {code}, want {EXIT_FAILED}:\n{}", a.output());
    assert!(a.output().contains("the clone's macOS restarted during the job"), "the agent said:\n{}", a.output());
    assert!(!vm.exists(), "the restarted clone was not deleted");
    assert!(!alive(m.pid("runner.pid")), "the runner's tart exec outlived the restart");
}

#[test]
fn tart_place_waits_for_a_free_macos_vm() {
    let Some(m) = Mac::tart() else { return };
    // Two VMs run that runner-up did not start: a macOS proof clone and a
    // Linux VM, which does not count. A leftover clone of this same place is
    // cleaned up first and does not count either.
    for (name, system) in [("sshhip-xcode271-proof", "darwin"), ("ubuntu", "linux"), (&*clone_name("mac-1"), "darwin")] {
        m.running_vm(name, system);
    }
    let mut a = m.run("jit-mac-1", &["--provider", "tart", "--place", "mac-1", "--image", SEQUOIA, "--boot-timeout", "30"]);
    m.eventually("the runner starts beside one other macOS VM", Some(&a), || m.pid("runner.pid") > 0);
    m.touch("go");
    m.touch("done");
    let code = a.exit();
    assert_eq!(code, EXIT_DONE, "the agent exited {code}:\n{}", a.output());
    for name in ["runner.pid", "go", "done"] {
        m.remove(name);
    }

    // A second macOS VM now runs outside runner-up: the agent waits rather
    // than have tart refuse the clone, and clones once one stops.
    m.running_vm("xcode-26-test", "darwin");
    let mut a = m.run("jit-mac-1-again", &["--provider", "tart", "--place", "mac-1", "--image", SEQUOIA, "--boot-timeout", "60"]);
    m.eventually("the agent says it waits", Some(&a), || a.output().contains("waiting for a macOS VM to stop"));
    // The agent keeps looking: once it has begun two more lists of the VMs,
    // the look between them found macOS still running two, and the agent
    // cloned nothing.
    let lists = m.tart_runs("list");
    m.eventually("the agent lists the VMs twice more", Some(&a), || m.tart_runs("list") >= lists + 2 || m.tart_runs("clone") > 1);
    assert!(!m.vm(&clone_name("mac-1")).exists() && m.tart_runs("clone") == 1, "the agent cloned while macOS ran two VMs");
    let _ = std::fs::remove_file(m.vm("xcode-26-test").join("running"));
    m.eventually("the runner starts once a VM stopped", Some(&a), || m.pid("runner.pid") > 0);
    m.touch("go");
    m.touch("done");
    let code = a.exit();
    assert_eq!(code, EXIT_DONE, "the agent exited {code}:\n{}", a.output());
}

#[test]
fn host_place_runs_on_the_machine_and_ends_everything_the_job_started() {
    let Some(m) = Mac::new() else { return };
    let jit = "eyJydW5uZXIiOiJydW5uZXItdXAtbWFjLWhvc3QifQ-host";
    let mut a = m.run(jit, &["--provider", "host", "--place", "mac-host"]);
    m.eventually("the runner starts", Some(&a), || m.pid("runner.pid") > 0);
    let runner = m.state.join("places/mac-host/host");
    assert_eq!(m.lines("registrations"), [jit], "the runner's registrations");
    let hook = runner.join("fork-guard.sh");
    assert_eq!(m.lines("hooks"), [hook.to_string_lossy()], "the runner's hook");
    assert!(
        !guard(&hook, "pull-request-521-fork.json") && guard(&hook, "push-main.json"),
        "the host place's fork guard does not decide as the guard does"
    );
    // AGENTS.md: the registration never travels on a command line. The runner
    // got it above; no process on this computer, the runner and the processes
    // the agent started included, has it among its arguments.
    let runner_pid = m.pid("runner.pid");
    let (code, all) = run(Command::new("ps").args(["-A", "-ww", "-o", "pid=,args="]), "");
    let listed = all.lines().any(|l| l.split_whitespace().next() == Some(&runner_pid.to_string()));
    assert!(code == 0 && listed, "ps did not list the runner {runner_pid}");
    for line in all.lines() {
        assert!(!line.contains(jit), "a process's command line holds the registration: {line:?}");
    }

    // The job starts a process of its own; then the coordinator lets go of
    // the agent's input, as when it stops the runner or loses the machine.
    m.touch("go");
    m.eventually("the job starts its process", Some(&a), || m.pid("job.pid") > 0);
    let job_pid = m.pid("job.pid");
    drop(a.input.take());
    let code = a.exit();
    assert_eq!(code, EXIT_DONE, "the agent exited {code}:\n{}", a.output());
    assert!(
        !alive(runner_pid) && !alive(job_pid),
        "the runner ({}) or the job's process ({}) outlived the place",
        alive(runner_pid),
        alive(job_pid)
    );
    assert!(!runner.exists(), "the runner's folder outlived the job");
}

#[test]
fn host_place_left_by_a_killed_agent_is_cleaned_up() {
    let Some(m) = Mac::new() else { return };
    let mut a = m.run("jit-host", &["--provider", "host", "--place", "mac-host"]);
    m.eventually("the runner starts", Some(&a), || m.pid("runner.pid") > 0);
    m.touch("go");
    m.eventually("the job starts its process", Some(&a), || m.pid("job.pid") > 0);
    let (runner_pid, job_pid) = (m.pid("runner.pid"), m.pid("job.pid"));
    // The agent is killed outright: its runner, in a group of its own, runs on
    // until the coordinator's clean, or the place's next agent, removes it.
    a.child.kill().unwrap();
    a.exit();
    m.touch("ping");
    m.eventually("the runner left behind answers a ping after the agent ended", None, || m.dir.join("pongs").exists());
    let state = m.state.to_string_lossy().into_owned();
    let (code, out) = run(&mut m.command(&["agent", "clean", "--provider", "host", "--place", "mac-host", "--state-dir", &state]), "");
    assert_eq!(code, 0, "agent clean: {out}");
    m.eventually("the runner and the job's process are gone", None, || !alive(runner_pid) && !alive(job_pid));
    assert!(!m.state.join("places/mac-host/host").exists(), "the runner's folder outlived the clean");
}

#[test]
fn host_place_keeps_its_tool_cache_between_jobs() {
    let Some(m) = Mac::new() else { return };
    let tools = m.state.join("places/mac-host/tool-cache").to_string_lossy().into_owned();
    for job in 1..=2 {
        m.remove("go");
        m.remove("done");
        let mut a = m.run("jit-host", &["--provider", "host", "--place", "mac-host"]);
        m.eventually("the runner starts", Some(&a), || {
            let seen = m.lines("toolcaches");
            seen.len() == job && !seen[0].is_empty()
        });
        m.touch("go");
        m.touch("done");
        let code = a.exit();
        assert_eq!(code, EXIT_DONE, "job {job}: the agent exited {code}:\n{}", a.output());
    }
    // The first job began with an empty cache and cached Node; the second
    // began with Node there.
    assert_eq!(m.lines("toolcaches"), [format!("{tools} "), format!("{tools} node,")], "the jobs saw tool caches");
    assert!(Path::new(&tools).join("node/24.22.1/arm64.complete").exists(), "the tool cache did not outlive the jobs");
}

#[test]
fn tart_place_runner_uses_the_images_tool_cache() {
    let Some(m) = Mac::tart() else { return };
    // The fake tart runs the guest's commands here, so a folder of the test
    // stands in for the image's tool cache.
    let tools = m.dir.join("image-tool-cache").to_string_lossy().into_owned();
    let mut a = m.run(
        "jit-mac-3",
        &["--provider", "tart", "--place", "mac-3", "--image", SEQUOIA, "--boot-timeout", "30", "--guest-check", "1", "--tool-cache", &tools],
    );
    m.eventually("the runner starts in the clone", Some(&a), || m.pid("runner.pid") > 0);
    m.touch("go");
    m.touch("done");
    let code = a.exit();
    assert_eq!(code, EXIT_DONE, "the agent exited {code}:\n{}", a.output());
    assert_eq!(m.lines("toolcaches"), [tools], "the runner saw tool caches");
}
