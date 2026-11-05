// Intent (README, "Linux guests on a Mac"). An Apple container place starts one
// fresh Linux guest per job from its image, sized to the job; the registration
// reaches the guest only on its standard input, after runner-up's guest
// program, so `container inspect` and the image never hold it; the guest runs
// the pinned runner with the fork guard; a stopped place force-deletes its
// guest, and the next job deletes whatever an agent killed outright left.
//
// The stand-in container tool records what it was asked and, for run, the
// guest's standard input, then waits until its guest is deleted.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::hyperv::filled_guest;
use super::{Buf, REGISTRATION, event, python, run, stand_in_runner, wait_for};
use crate::agent::container::{AppleContainer, container_bootstrap};
use crate::agent::hyperv::replace_all;
use crate::agent::runner::{RUNNER_VERSION, runner_arch, runner_sha256};
use crate::agent::{AgentRun, Out, Provider};
use crate::config;
use crate::ctx::Ctx;

/// The stand-in container tool in dir, which fails to start a guest when fail.
fn stand_in_container(dir: &Path, fail: bool) -> String {
    let fail = if fail { "1" } else { "" };
    let script = format!(
        r#"#!/bin/bash
d={}
printf '%s\n' "$1 ${{*: -1}}" >> "$d/calls"
case $1 in
run)
  printf '%s\0' "$@" > "$d/args"
  env > "$d/env"
  cat > "$d/stdin"
  [ -z "{fail}" ] || {{ echo "Error: notFound: image"; exit 1; }}
  echo $$ > "$d/guest.pid"
  echo "runner-up guest: starting the runner"
  until [ -e "$d/deleted" ]; do sleep 0.05; done
  ;;
delete)
  [ -e "$d/guest.pid" ] || {{ echo "Error: notFound: \"container with ID ${{*: -1}} not found\""; exit 1; }}
  touch "$d/deleted"
  rm "$d/guest.pid"
  ;;
esac
"#,
        crate::agent::launch::shell_word(&dir.to_string_lossy())
    );
    let tool = dir.join("container");
    std::fs::write(&tool, script).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    tool.to_string_lossy().into_owned()
}

fn container_place(state: &Path) -> AgentRun {
    AgentRun {
        provider: config::PROVIDER_CONTAINER.into(),
        place: "mini-linux1".into(),
        cores: 2,
        memory_mb: 8192,
        image: "runner-up-linux:2026-11-03".into(),
        state_dir: state.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn out() -> Arc<Out> {
    Arc::new(Out::new(Box::new(Buf::default())))
}

/// A started container place: the arguments and the standard input its guest
/// got, and how to stop it.
struct Guest {
    args: Vec<String>,
    stdin: String,
    ctx: Ctx,
    done: std::sync::mpsc::Receiver<Result<(), String>>,
}

impl Guest {
    /// Stops the place and returns what it reported.
    fn stop(self) -> Result<(), String> {
        self.ctx.cancel();
        self.done.recv_timeout(Duration::from_secs(10)).expect("the place did not stop")
    }
}

fn start_container_guest(dir: &Path, tool: &str, a: &AgentRun) -> Guest {
    let ctx = Ctx::background().with_cancel();
    let (tx, done) = std::sync::mpsc::channel();
    let (c, a, tool) = (ctx.clone(), a.clone(), tool.to_string());
    std::thread::spawn(move || {
        let _ = tx.send(AppleContainer { tool: &tool }.run(&c, &a, REGISTRATION, &out()));
    });
    wait_for(&dir.join("guest.pid"));
    let b = std::fs::read_to_string(dir.join("args")).unwrap();
    let args = b.trim_end_matches('\0').split('\0').map(String::from).collect();
    Guest { args, stdin: std::fs::read_to_string(dir.join("stdin")).unwrap(), ctx, done }
}

#[test]
fn container_guest_gets_the_registration_only_on_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let tool = stand_in_container(dir.path(), false);
    let a = container_place(state.path());
    let guest = start_container_guest(dir.path(), &tool, &a);
    let args = guest.args.clone();

    let b = std::fs::read_to_string(dir.path().join("args")).unwrap();
    let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
    assert!(
        !b.contains(REGISTRATION) && !env.contains(REGISTRATION),
        "the registration is in the guest's arguments or the container tool's environment"
    );
    assert!(!b.contains("\0-e\0") && !b.contains("--env"), "the guest gets an environment variable: {args:?}");
    let values: HashMap<&str, &str> = args.windows(2).map(|w| (w[0].as_str(), w[1].as_str())).collect();
    for (flag, want) in [
        ("--name", "runner-up-mini-linux1"),
        ("--cpus", "2"),
        ("--memory", "8192M"),
        ("--label", "runner-up.place=mini-linux1"),
        ("--user", "0:0"),
        ("--entrypoint", "/usr/bin/python3"),
    ] {
        assert_eq!(values.get(flag).copied(), Some(want), "container run {flag} ({args:?})");
    }
    let n = args.len();
    assert!(n >= 3 && args[n - 3] == a.image && args[n - 2] == "-c", "the guest does not start a program from the place's image: {args:?}");

    // Its input is a length line and that many bytes of program, then the
    // registration on one line.
    let (size, rest) = guest.stdin.split_once('\n').unwrap_or((&guest.stdin, ""));
    let n: usize = size.parse().ok().filter(|&n| n <= rest.len()).unwrap_or_else(|| panic!("the guest's input starts {size:?}"));
    assert!(!rest[..n].contains(REGISTRATION), "the guest's program holds the registration");
    assert_eq!(rest[n..], format!("{REGISTRATION}\n"), "after the program the guest reads this, want the registration");

    // Stopping the place deletes its guest.
    assert_eq!(guest.stop(), Ok(()), "a stopped place reported an error");
    let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
    assert!(calls.contains("delete runner-up-mini-linux1"), "stopping the place did not delete its guest: {calls}");
}

// guestHarness runs a container guest's bootstrap, as given to python3, on
// its standard input, as the guest would, with the guest's filesystem under
// a directory and its other edges stood in for: GitHub's download serves a
// file and records what was asked, chown records its arguments, and the
// runner runs as this user and records the user it was to run as.
const GUEST_HARNESS: &str = r#"import builtins, importlib.util, json, os, shutil, subprocess, sys, urllib.request
root, served, bootstrap = sys.argv[1:4]
real_open = builtins.open
def record(**kw):
    with real_open(os.path.join(root, "record"), "a") as f:
        f.write(json.dumps(kw) + "\n")
def moved(p):
    if isinstance(p, str) and p.startswith(("/opt/runner-up", "/home/runner", "/etc/environment")):
        return root + p
    return p
def wrap(fn):
    return lambda p, *a, **k: fn(moved(p), *a, **k)
builtins.open = wrap(real_open)
os.makedirs, os.chmod, os.remove, shutil.rmtree = map(wrap, (os.makedirs, os.chmod, os.remove, shutil.rmtree))
def urlopen(url, timeout=None):
    record(download=url)
    return real_open(served, "rb")
urllib.request.urlopen = urlopen
real_run = subprocess.run
def run(args, **kw):
    args = [moved(a) for a in args]
    if args[0] == "chown":
        record(chown=args)
        return subprocess.CompletedProcess(args, 0)
    if "user" in kw:
        record(user=kw.pop("user"), group=kw.pop("group"), extra_groups=kw.pop("extra_groups"))
    kw["cwd"] = moved(kw.get("cwd"))
    return real_run(args, **kw)
subprocess.run = run
def execv(path, argv):
    spec = importlib.util.spec_from_file_location("guest", moved(argv[1]))
    g = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(g)
    sys.argv = argv[1:]
    sys.exit(g.main())
os.execv = execv
os.makedirs(os.path.join(root, "home/runner"))
exec(bootstrap, {"__name__": "__main__"})
"#;

// Intent (README, "Linux guests on a Mac" and "Pull requests from forks"): the
// guest runs GitHub's runner, from GitHub's release of the pinned version, only
// when its download matches the pin; as the user runner, with the registration
// and the job's size in its environment and the fork guard, which refuses
// another repository's pull request, as its job-started hook. The pin is the
// stand-in release's own sum, so a download of anything else must be refused.
#[test]
fn container_guest_runs_the_pinned_runner_with_the_fork_guard() {
    let Some(py) = python() else { return };
    if crate::proc::look_path("tar").is_err() {
        eprintln!("skipped: no tar");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let release = work.path().join("release.tar.gz");
    let (tarball, sum) = stand_in_runner("#!/bin/sh\nenv > runner.env\necho 'Running job: build'\n");
    std::fs::write(&release, &tarball).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let tool = stand_in_container(dir.path(), false);
    let guest = start_container_guest(dir.path(), &tool, &container_place(state.path()));
    let bootstrap = guest.args.last().unwrap().clone();
    // The guest program pins the stand-in release in place of GitHub's.
    let input = guest.stdin.replace(runner_sha256(&format!("linux-{}", runner_arch())).unwrap(), &sum);
    let _ = guest.stop();

    let run_guest = |served: &Path| {
        let root = tempfile::tempdir().unwrap();
        let (code, out) = run(Command::new(&py).args(["-I", "-c", GUEST_HARNESS]).arg(root.path()).arg(served).arg(&bootstrap), &input);
        (root, out, code)
    };

    let (root, out, code) = run_guest(&release);
    assert!(
        code == 0 && out.contains("Running job: build\nrunner-up guest: the runner ended with 0\n"),
        "the guest ended with {code}:\n{out}"
    );
    assert!(!out.contains(REGISTRATION), "the guest's output holds the registration:\n{out}");
    let root_s = root.path().to_string_lossy();
    let recorded = std::fs::read_to_string(root.path().join("record")).unwrap();
    let asked: Vec<Value> = recorded.trim().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let want_url = format!(
        "https://github.com/actions/runner/releases/download/v{RUNNER_VERSION}/actions-runner-linux-{}-{RUNNER_VERSION}.tar.gz",
        runner_arch()
    );
    let want = vec![
        json!({"download": want_url}),
        json!({"chown": ["chown", "-R", "runner:runner", format!("{root_s}/home/runner/actions-runner")]}),
        json!({"user": "runner", "group": "runner", "extra_groups": []}),
    ];
    assert_eq!(asked, want, "what the guest asked");

    let env_file = std::fs::read_to_string(root.path().join("home/runner/actions-runner/runner.env")).expect("the runner did not run");
    let env: HashMap<&str, &str> = env_file.lines().filter_map(|l| l.split_once('=')).collect();
    let hook = "/opt/runner-up/fork-guard.sh";
    for (k, v) in [
        ("ACTIONS_RUNNER_INPUT_JITCONFIG", REGISTRATION),
        ("ACTIONS_RUNNER_HOOK_JOB_STARTED", hook),
        ("RUNNER_UP_CORES", "2"),
        ("RUNNER_UP_MEMORY_MB", "8192"),
        ("HOME", "/home/runner"),
        ("USER", "runner"),
    ] {
        assert_eq!(env.get(k).copied(), Some(v), "the runner got {k}");
    }

    // The hook, run with the guest's filesystem under root, refuses a pull
    // request from a fork and allows one from the repository.
    let installed = format!("{root_s}{hook}");
    let script = std::fs::read_to_string(&installed).unwrap_or_default();
    let runnable = std::fs::metadata(&installed).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    assert!(runnable, "the guest installed no runnable hook at {hook}");
    let mapped = replace_all(&script, &[("/usr/bin/python3", format!("{py} -I")), ("/opt/runner-up", format!("{root_s}/opt/runner-up"))]);
    for (name, want_code) in [("pull-request-521-fork.json", 1), ("pull-request-527-same-repo.json", 0)] {
        let (code, out) = run(
            Command::new("bash").args(["-c", &mapped]).env("GITHUB_EVENT_PATH", event(name)).env("GITHUB_EVENT_NAME", "pull_request"),
            "",
        );
        assert_eq!(code, want_code, "the hook on {name}: {out}");
    }

    // A download that does not match the pin runs no runner.
    let other = work.path().join("other.tar.gz");
    std::fs::write(&other, [b"x".as_slice(), &tarball].concat()).unwrap();
    let (root, out, code) = run_guest(&other);
    assert!(
        code == 1 && out.contains("runner-up guest: failed: the runner's download does not match its pin"),
        "a guest given a download that does not match its pin ended with {code}:\n{out}"
    );
    assert!(
        !root.path().join("home/runner/actions-runner/runner.env").exists(),
        "the guest ran a runner whose download does not match its pin"
    );
}

// A guest the container tool could not start ran no runner: the place says
// so, rather than finishing as though a job ran.
#[test]
fn container_guest_that_never_starts_fails_the_place() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let tool = stand_in_container(dir.path(), true);
    let res = AppleContainer { tool: &tool }.run(&Ctx::background(), &container_place(state.path()), REGISTRATION, &out());
    assert!(res.as_ref().is_err_and(|e| e.contains("no runner")), "a guest that never started reported {res:?}");
}

// Cleaning a place deletes a guest an agent killed outright left, and cleaning
// a place with no guest is no failure.
#[test]
fn container_clean_deletes_a_leftover_guest() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let tool = stand_in_container(dir.path(), false);
    let a = container_place(state.path());
    let p = AppleContainer { tool: &tool };
    assert_eq!(p.clean(&Ctx::background(), &a), Ok(()), "cleaning a place with no guest");
    std::fs::write(dir.path().join("guest.pid"), "1").unwrap();
    assert_eq!(p.clean(&Ctx::background(), &a), Ok(()));
    assert!(dir.path().join("deleted").exists(), "the leftover guest was not deleted");
}

// The bootstrap takes exactly the program from its input, so the program,
// run in its place, reads the registration as its own next line; the guest
// program, run that way, takes its registration from standard input and,
// without one, fails so the agent knows no runner ran.
#[test]
fn container_bootstrap_leaves_the_registration_for_the_guest() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let program = "import os, sys\nline = b\"\"\nwhile not line.endswith(b\"\\n\"):\n    line += os.read(0, 1)\nprint(sys.argv[1:], line.decode().strip())\n";
    let bootstrap = container_bootstrap(&dir.path().join("opt").to_string_lossy());
    let (code, out) = run(Command::new(&py).args(["-I", "-c", &bootstrap]), &format!("{}\n{program}{REGISTRATION}\n", program.len()));
    assert!(code == 0 && out.trim() == format!("['stdin'] {REGISTRATION}"), "the program the bootstrap ran said {out:?} ({code})");

    let guest = dir.path().join("guest.py");
    std::fs::write(&guest, filled_guest()).unwrap();
    let script = "\nimport importlib.util, os, sys\nspec = importlib.util.spec_from_file_location(\"guest\", sys.argv[1])\ng = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)\nprint(g.registration(0, announce=False))\n";
    let (code, out) = run(Command::new(&py).args(["-I", "-c", script]).arg(&guest), &format!("{REGISTRATION}\n"));
    assert!(code == 0 && out.trim() == REGISTRATION, "the guest took {out:?} ({code}), want the registration");

    let (code, out) = run(Command::new(&py).arg("-I").arg(&guest).arg("stdin"), "");
    assert!(
        code == 1 && out.contains("runner-up guest: failed: the agent closed its input"),
        "a guest with no registration said {out:?} ({code})"
    );
}
