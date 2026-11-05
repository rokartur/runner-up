//! An Apple container place runs its job in a fresh Linux guest on a Mac, made
//! with Apple's container tool from the place's image, sized to the job and
//! removed after it. Image prerequisites are documented in README.md under
//! "What you need". runner-up's Linux guest program (hyperv-guest.py) runs in
//! it as root: the agent sends the program on the guest's standard input,
//! then the registration on one more line, so neither the image nor
//! `container inspect` ever holds the registration. Nothing of the Mac is
//! shared with the guest.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use super::hyperv::{linux_guest, linux_guest_launch};
use super::launch::{LINUX_GUEST_DIR, RunnerLaunch};
use super::runner::{runner_arch, runner_sha256};
use super::{AgentRun, Out, Provider, exit_code, first_line, spawn_into, wait_or_stop};
use crate::ctx::Ctx;
use crate::go::quote;

/// Apple container places, run with tool, the container program.
pub struct AppleContainer<'a> {
    pub tool: &'a str,
}

/// The guest's first program, given as an argument: it reads a length line
/// and that many bytes of standard input, unbuffered so the registration
/// after them stays unread, writes them to dir/guest.py and runs that in its
/// place, reading the rest of standard input.
pub fn container_bootstrap(dir: &str) -> String {
    format!(
        r#"import os, sys
def take(n):
    b = b""
    while len(b) < n:
        c = os.read(0, n - len(b))
        if not c:
            sys.exit("runner-up guest: no program on input")
        b += c
    return b
size = b""
while not size.endswith(b"\n"):
    size += take(1)
os.makedirs({q}, exist_ok=True)
path = os.path.join({q}, "guest.py")
with open(path, "wb") as f:
    f.write(take(int(size)))
os.execv(sys.executable, [sys.executable, path, "stdin"])
"#,
        q = quote(dir)
    )
}

pub fn apple_container_name(place: &str) -> String {
    format!("runner-up-{place}")
}

/// The place's guest: no secret is among them.
pub fn apple_container_args(a: &AgentRun) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--interactive", "--rm", "--init", "--name"].map(String::from).to_vec();
    args.extend([
        apple_container_name(&a.place),
        "--label".into(),
        format!("runner-up.place={}", a.place),
        "--cpus".into(),
        a.cores.to_string(),
        "--memory".into(),
        format!("{}M", a.memory_mb),
        "--user".into(),
        "0:0".into(),
        "--entrypoint".into(),
        "/usr/bin/python3".into(),
        a.image.clone(),
        "-c".into(),
        container_bootstrap(LINUX_GUEST_DIR),
    ]);
    args
}

impl Provider for AppleContainer<'_> {
    fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
        if a.image.is_empty() {
            return Err("an Apple container place needs --image, the image its guests start from".into());
        }
        let guest = linux_guest(runner_arch(), &self.launch(a))?;
        let mut cmd = Command::new(self.tool);
        cmd.args(apple_container_args(a)).stdin(Stdio::piped());
        let (mut child, piped) = spawn_into(&mut cmd, out).map_err(|e| format!("container: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let input = format!("{}\n{guest}{jit}\n", guest.len());
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let (place, tool) = (a.place.clone(), self.tool.to_string());
        let stop = move || {
            let c = Ctx::detached_timeout(Duration::from_secs(60));
            let _ = crate::proc::combined_output(&c, Command::new(&tool).args(["delete", "--force", &apple_container_name(&place)]));
        };
        let status = wait_or_stop(ctx, &mut child, &stop, Duration::from_secs(60));
        piped.finish(Duration::from_secs(5));
        if ctx.is_done() {
            return Ok(());
        }
        let code = exit_code(&status);
        if code != 0 {
            // The guest program ends well once its runner ran, so this is the
            // container tool's failure, such as a missing image, or the
            // guest's before the runner started.
            return Err(format!("the guest ran no runner (exit status {code})"));
        }
        Ok(())
    }

    fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String> {
        let (b, res) =
            crate::proc::combined_output(ctx, Command::new(self.tool).args(["delete", "--force", &apple_container_name(&a.place)]));
        let out = String::from_utf8_lossy(&b);
        match res {
            Err(e) if !out.contains("not found") => Err(format!("container delete: {}", first_line(&out, &e))),
            _ => Ok(()),
        }
    }

    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        linux_guest_launch(a)
    }

    /// "ok" when this Mac can run Apple container places: the container tool
    /// is installed and its service answers, and GitHub publishes a Linux
    /// runner for its guests' architecture.
    fn ready(&self) -> String {
        container_ready(self.tool)
    }
}

/// "ok" when Apple's container tool, run as tool, can start containers here.
pub fn container_ready(tool: &str) -> String {
    if !cfg!(target_os = "macos") {
        return "Apple's container tool runs only on macOS".into();
    }
    if runner_sha256(&format!("linux-{}", runner_arch())).is_none() {
        return format!("no Linux runner release for {}", crate::release::goarch());
    }
    if crate::proc::look_path(tool).is_err() {
        return "Apple's container tool is not installed (brew install container)".into();
    }
    let ctx = Ctx::detached_timeout(Duration::from_secs(20));
    let (b, res) = crate::proc::combined_output(&ctx, Command::new(tool).args(["system", "status"]));
    if let Err(e) = res {
        return format!(
            "Apple's container service is not running (container system start): {}",
            first_line(&String::from_utf8_lossy(&b), &e)
        );
    }
    "ok".into()
}
