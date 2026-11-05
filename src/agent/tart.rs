//! A tart place runs each job in a new tart clone of the place's macOS image,
//! deleted after the job. The clone sees one folder of its Mac, read-only: the
//! runner's release archive and this program for readings and the fork guard.
//! Its guest agent runs the runner with `tart exec`, which hands it the
//! one-time registration on its input, never in a command line. tart is the
//! program its makers publish; runner-up only runs it.
//!
//! Apple's Virtualization framework runs at most two macOS virtual machines at
//! once on a Mac. The coordinator counts its own clones and the macOS VMs it
//! did not start (coordinator::vmlimit), and the agent waits for a free one
//! before it clones, in case a VM started since the coordinator last looked.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::launch::{RunnerLaunch, TART_GUARD, TART_GUEST, shell_launch, shell_word};
use super::runner::runner_tarball;
use super::{AgentRun, Out, Provider, ensure_dir, first_line, spawn_into, wait_or_stop};
use crate::ctx::Ctx;

pub struct Tart;

/// The Virtualization framework's limit on macOS virtual machines running at
/// once on one Mac.
pub const MACOS_VM_LIMIT: usize = 2;

/// Names a tart place's clone.
pub fn clone_name(place: &str) -> String {
    format!("runner-up-{place}")
}

/// The folder of the Mac the clone sees, by the name tart mounts it under in
/// the guest.
const TART_SHARE_NAME: &str = "runner-up";
const TART_GUEST_SHARE: &str = "/Volumes/My Shared Files/runner-up";

/// Runs in the clone as its guest agent's user. It leaves the registration on
/// stdin for the resource reader and copies the runner and this program out
/// of the shared folder, which macOS mounts once the clone's user has signed
/// in. It starts the runner under the resource reader, with l's variables
/// and, unless forks are allowed, the fork guard as its job-started hook. The
/// runner runs that hook's path unquoted, so it lives in the home folder, not
/// under the share's spaces. Its arguments are the share and the tool cache.
pub fn tart_script(l: &RunnerLaunch) -> String {
    let guest = shell_word(TART_GUEST);
    let program = shell_word(&format!("{TART_GUEST}/runner-up"));
    format!(
        r#"set -eu
share=$1
if [ -n "${{2:-}}" ]; then export RUNNER_TOOL_CACHE="$2"; fi
i=0
until [ -r "$share/runner.tar.gz" ]; do
  i=$((i+1)); [ "$i" -le 120 ] || {{ echo "the shared folder never appeared in the clone" >&2; exit 1; }}
  sleep 1
done
rm -rf "$HOME/actions-runner" {guest}
mkdir -p "$HOME/actions-runner" {guest}
tar xzf "$share/runner.tar.gz" -C "$HOME/actions-runner"
cp "$share/runner-up" {program}
chmod 0755 {program}
{}cd "$HOME/actions-runner"
exec {program} agent measure -- ./run.sh
"#,
        shell_launch(l)
    )
}

fn share_dir(a: &AgentRun) -> PathBuf {
    a.dir().join("share")
}

/// Runs tart with args, bounded by two minutes, and returns its output.
fn tart(ctx: &Ctx, args: &[&str]) -> Result<String, String> {
    let c = ctx.with_timeout(Duration::from_secs(120));
    let (b, res) = crate::proc::combined_output(&c, Command::new("tart").args(args));
    let out = String::from_utf8_lossy(&b).into_owned();
    res.map(|_| out.clone()).map_err(|e| first_line(&out, &e))
}

/// A child another thread may signal while one waits for it.
struct Shared(Arc<Mutex<Child>>);

impl Shared {
    fn signal(&self, sig: i32) {
        let pid = self.0.lock().unwrap().id() as i32;
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, sig)
        };
        #[cfg(not(unix))]
        let _ = (pid, sig);
    }
}

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;

impl Provider for Tart {
    fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String> {
        let vm = clone_name(&a.place);
        for v in tart_vms(ctx)? {
            if v.name != vm || v.source != "local" {
                continue;
            }
            if v.running {
                let _ = tart(ctx, &["stop", "--timeout", "10", &vm]);
            }
            tart(ctx, &["delete", &vm]).map_err(|e| format!("tart delete {vm}: {e}"))?;
        }
        match std::fs::remove_dir_all(share_dir(a)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
        let msg = self.ready();
        if msg != "ok" {
            return Err(msg);
        }
        let tarball = runner_tarball(ctx, &Path::new(&a.state_dir).join("cache"))?;
        let share = ensure_dir(&share_dir(a))?;
        link_or_copy(&tarball, &share.join("runner.tar.gz")).map_err(|e| e.to_string())?;
        // The guest uses this program for readings even when the fork guard is off.
        link_or_copy(Path::new(&a.self_path), &share.join("runner-up")).map_err(|e| e.to_string())?;
        let vm = clone_name(&a.place);
        wait_for_vm_slot(ctx, &vm, a.boot_timeout, out)?;
        tart(ctx, &["clone", &a.image, &vm]).map_err(|e| format!("tart clone {}: {e}", a.image))?;
        let (cpu, memory, disk) = (a.cores.to_string(), a.memory_mb.to_string(), a.disk_gb.to_string());
        let mut set = vec!["set", &vm, "--cpu", &cpu, "--memory", &memory];
        if a.disk_gb > 0 {
            set.extend(["--disk-size", &disk]);
        }
        tart(ctx, &set).map_err(|e| format!("tart set: {e}"))?;

        let mut boot_cmd = Command::new("tart");
        boot_cmd.args(["run", "--no-graphics", &format!("--dir={TART_SHARE_NAME}:{}:ro", share.display()), &vm]).stdin(Stdio::null());
        let (boot, boot_piped) = spawn_into(&mut boot_cmd, out).map_err(|e| format!("tart run: {e}"))?;
        let boot = Shared(Arc::new(Mutex::new(boot)));
        let vm_done = Ctx::background().with_cancel();
        {
            let (child, done) = (boot.0.clone(), vm_done.clone());
            std::thread::spawn(move || {
                loop {
                    if !matches!(child.lock().unwrap().try_wait(), Ok(None)) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                done.cancel();
            });
        }
        let result = run_in_clone(ctx, a, jit, out, &vm, &vm_done, self.launch(a));
        boot.signal(SIGTERM);
        if !vm_done.sleep(Duration::from_secs(30)) {
            let _ = boot.0.lock().unwrap().kill();
            vm_done.wait();
        }
        boot_piped.finish(Duration::from_secs(5));
        result
    }

    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        a.launch_with(&format!("{TART_GUEST}/fork-guard.sh"), TART_GUARD)
    }

    /// "ok" when this machine can run tart places, or why not.
    fn ready(&self) -> String {
        if !(cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")) {
            return "tart places run only on Macs with Apple silicon".into();
        }
        if crate::proc::look_path("tart").is_err() {
            return "tart is not installed (brew install cirruslabs/cli/tart)".into();
        }
        "ok".into()
    }
}

/// The part of a tart run while its clone boots and runs the job; the caller
/// stops the clone after it.
fn run_in_clone(ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>, vm: &str, vm_done: &Ctx, l: RunnerLaunch) -> Result<(), String> {
    wait_for_guest(ctx, vm, a.boot_timeout, vm_done)?;
    out.say(&format!("clone {vm} of {} is up", a.image));

    // A kernel panic restarts the clone's macOS and ends the runner with it,
    // but its tart exec can go on waiting. So the job ends once the clone
    // reads a new boot session, or the clone stops.
    let job = ctx.with_cancel();
    let restarted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (job, restarted, vm, every) = (job.clone(), restarted.clone(), vm.to_string(), a.guest_check);
        std::thread::spawn(move || {
            if watch_boot_session(&job, &mut |c| boot_session(c, &vm), every) {
                restarted.store(true, std::sync::atomic::Ordering::SeqCst);
                job.cancel();
            }
        });
    }
    {
        let (job, vm_done) = (job.clone(), vm_done.clone());
        std::thread::spawn(move || {
            while !job.is_done() {
                if vm_done.is_done() {
                    job.cancel();
                    return;
                }
                job.sleep(Duration::from_millis(100));
            }
        });
    }
    let script = tart_script(&l);
    let mut cmd = Command::new("tart");
    cmd.args(["exec", "-i", vm, "/bin/bash", "-c", &script, "runner-up", TART_GUEST_SHARE, &a.tool_cache]).stdin(Stdio::piped());
    let started = spawn_into(&mut cmd, out);
    let (mut child, piped) = match started {
        Ok(c) => c,
        Err(e) => {
            job.cancel();
            return Err(format!("tart exec: {e}"));
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    let line = format!("{jit}\n");
    std::thread::spawn(move || {
        let _ = stdin.write_all(line.as_bytes());
    });
    let pid = child.id();
    let _ = wait_or_stop(
        &job,
        &mut child,
        &|| {
            let _ = crate::proc::terminate(pid);
        },
        Duration::from_secs(30),
    );
    // Once tart exec has ended, its output is not waited for.
    piped.finish(Duration::from_secs(5));
    job.cancel();
    if restarted.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("the clone's macOS restarted during the job".into());
    }
    if vm_done.is_done() && !ctx.is_done() {
        return Err("the clone stopped while the runner ran".into());
    }
    Ok(())
}

#[derive(Deserialize, Debug, Default)]
struct TartVm {
    #[serde(default, alias = "name")]
    #[serde(rename = "Name")]
    name: String,
    #[serde(default, alias = "source")]
    #[serde(rename = "Source")]
    source: String,
    #[serde(default, alias = "running")]
    #[serde(rename = "Running")]
    running: bool,
}

fn tart_vms(ctx: &Ctx) -> Result<Vec<TartVm>, String> {
    let c = ctx.with_timeout(Duration::from_secs(20));
    let o = crate::proc::output(&c, Command::new("tart").args(["list", "--format", "json"]), Vec::new());
    o.status.map_err(|e| format!("tart list: {e}"))?;
    serde_json::from_slice::<Option<Vec<TartVm>>>(&o.stdout).map(Option::unwrap_or_default).map_err(|e| format!("tart list: {e}"))
}

/// Names the macOS virtual machines tart runs on this Mac: only those count
/// toward the framework's limit. A running VM whose system cannot be read
/// counts, since it may be one.
pub fn running_mac_vms(ctx: &Ctx) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Got {
        #[serde(default, rename = "OS", alias = "os", alias = "Os")]
        os: String,
    }
    let mut running = Vec::new();
    for vm in tart_vms(ctx)? {
        if vm.source != "local" || !vm.running {
            continue;
        }
        let c = ctx.with_timeout(Duration::from_secs(20));
        let o = crate::proc::output(&c, Command::new("tart").args(["get", &vm.name, "--format", "json"]), Vec::new());
        if o.status.is_ok()
            && let Ok(got) = serde_json::from_slice::<Got>(&o.stdout)
            && !got.os.is_empty()
            && got.os != "darwin"
        {
            continue;
        }
        running.push(vm.name);
    }
    Ok(running)
}

/// Waits, up to timeout, until fewer than MACOS_VM_LIMIT macOS VMs other than
/// vm run on this Mac.
fn wait_for_vm_slot(ctx: &Ctx, vm: &str, timeout: Duration, out: &Out) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut said = false;
    loop {
        let others: Vec<String> = running_mac_vms(ctx)?.into_iter().filter(|v| v != vm).collect();
        if others.len() < MACOS_VM_LIMIT {
            return Ok(());
        }
        if !said {
            out.say(&format!("waiting for a macOS VM to stop: macOS runs at most {MACOS_VM_LIMIT} at once, and {} run", others.join(", ")));
            said = true;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "macOS already ran {} VMs ({}) for {}",
                others.len(),
                others.join(", "),
                crate::go::duration::format(timeout)
            ));
        }
        if ctx.sleep(Duration::from_secs(5)) {
            return Err(ctx.err().unwrap_or("context canceled").to_string());
        }
    }
}

/// Waits until the clone's guest agent answers, up to timeout.
fn wait_for_guest(ctx: &Ctx, vm: &str, timeout: Duration, vm_done: &Ctx) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let c = ctx.with_timeout(Duration::from_secs(20));
        let o = crate::proc::output(&c, Command::new("tart").args(["exec", vm, "/usr/bin/true"]), Vec::new());
        if o.status.is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("the clone did not come up within {}", crate::go::duration::format(timeout)));
        }
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if vm_done.is_done() {
                return Err("the clone stopped while booting".into());
            }
            if ctx.sleep(Duration::from_millis(100)) {
                return Err(ctx.err().unwrap_or("context canceled").to_string());
            }
        }
    }
}

/// How macOS prints kern.bootsessionuuid: a UUID the kernel makes afresh at
/// each boot. kern.boottime would not do: macOS moves it whenever the clock is
/// set, as a new clone's clock is soon after boot.
pub fn is_boot_session_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5 && parts.iter().zip(lens).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Reads the clone's boot session, or "" when it cannot.
fn boot_session(ctx: &Ctx, vm: &str) -> String {
    let c = ctx.with_timeout(Duration::from_secs(20));
    let o = crate::proc::output(&c, Command::new("tart").args(["exec", vm, "/usr/sbin/sysctl", "-n", "kern.bootsessionuuid"]), Vec::new());
    let now = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.is_ok() && is_boot_session_uuid(&now) { now } else { String::new() }
}

/// Reads the clone's boot session every interval until ctx ends, and reports
/// true once a reading differs from the first: the clone's macOS restarted,
/// and its runner went with it. A reading that fails, or is no UUID, counts
/// for nothing, so a clone too busy to answer is never taken for a restarted
/// one.
pub fn watch_boot_session(ctx: &Ctx, read: &mut dyn FnMut(&Ctx) -> String, interval: Duration) -> bool {
    let mut first = String::new();
    loop {
        let now = read(ctx);
        if is_boot_session_uuid(&now) {
            if first.is_empty() {
                first = now;
            } else if now != first {
                return true;
            }
        }
        if ctx.sleep(interval) {
            return false;
        }
    }
}

/// Puts src at dst, as a hard link where it can.
pub fn link_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dst);
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    let mut input = std::fs::File::open(src)?;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o755);
    let mut output = o.open(dst)?;
    std::io::copy(&mut input, &mut output)?;
    Ok(())
}
