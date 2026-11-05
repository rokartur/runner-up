use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use super::launch::{DOCKER_GUEST, REGISTRATION_VAR, RunnerLaunch};
use super::runner::runner_version;
use super::{AgentRun, Out, Provider, first_line, spawn_into, wait_or_stop};
use crate::ctx::Ctx;

/// GitHub's own runner image, pinned to the runner version every place uses.
pub const DEFAULT_DOCKER_IMAGE: &str = concat!("ghcr.io/actions/actions-runner:", runner_version!());

/// Runs each job in a new container, removed after the job. The container is
/// the place: one runner, one job, nothing kept.
pub struct Docker;

pub fn container_name(place: &str) -> String {
    format!("runner-up-{place}")
}

/// Starts the runner inside the container: it reads the registration from
/// standard input and gives it only to the runner, in the one variable the
/// runner reads it from. The runner removes it from its environment before
/// any job starts.
fn docker_entry() -> String {
    format!("read -r {REGISTRATION_VAR} && export {REGISTRATION_VAR} && exec \"$@\"")
}

impl Provider for Docker {
    fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
        let l = self.launch(a);
        let hook = a.dir().join("fork-guard.sh");
        if !l.hook.is_empty() {
            write_executable(&hook, l.guard.as_bytes())?;
        }
        let mut cmd = Command::new("docker");
        cmd.args(docker_args(a, &l, &hook.to_string_lossy())).stdin(Stdio::piped());
        let (mut child, piped) = spawn_into(&mut cmd, out).map_err(|e| format!("docker: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let line = format!("{jit}\n");
        std::thread::spawn(move || {
            let _ = stdin.write_all(line.as_bytes());
        });
        let place = a.place.clone();
        let stop = move || {
            let c = Ctx::detached_timeout(Duration::from_secs(60));
            let _ = crate::proc::combined_output(&c, Command::new("docker").args(["rm", "-f", &container_name(&place)]));
        };
        let status = wait_or_stop(ctx, &mut child, &stop, Duration::from_secs(60));
        piped.finish(Duration::from_secs(5));
        if ctx.is_done() {
            return Ok(());
        }
        if super::exit_code(&status) == 125 {
            // docker's own failure, such as a missing image: no runner ran.
            return Err("docker could not start the container".into());
        }
        Ok(())
    }

    fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String> {
        let (b, res) = crate::proc::combined_output(ctx, Command::new("docker").args(["rm", "-f", &container_name(&a.place)]));
        let out = String::from_utf8_lossy(&b);
        match res {
            Err(e) if !out.contains("No such container") => Err(format!("docker rm: {}", first_line(&out, &e))),
            _ => Ok(()),
        }
    }

    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        a.launch_with(&format!("{DOCKER_GUEST}/fork-guard.sh"), &super::fork_guard_hook(&format!("{DOCKER_GUEST}/runner-up")))
    }

    fn ready(&self) -> String {
        let ctx = Ctx::detached_timeout(Duration::from_secs(20));
        let (b, res) = crate::proc::combined_output(&ctx, Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]));
        match res {
            Err(e) => first_line(&String::from_utf8_lossy(&b), &e),
            Ok(()) => "ok".into(),
        }
    }
}

/// The place's container, with the fork guard's hook file, written on the
/// machine at hook, mounted where l names it. -i keeps the container's
/// standard input open: the registration arrives there, never in docker's
/// arguments or environment, so the container's settings (docker inspect)
/// never hold it.
pub fn docker_args(a: &AgentRun, l: &RunnerLaunch, hook: &str) -> Vec<String> {
    let image = if a.image.is_empty() { DEFAULT_DOCKER_IMAGE } else { &a.image };
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        "--name".into(),
        container_name(&a.place),
        "--label".into(),
        format!("runner-up.place={}", a.place),
        "--cpus".into(),
        a.cores.to_string(),
        "--memory".into(),
        format!("{}m", a.memory_mb),
    ];
    if !l.hook.is_empty() {
        args.extend(["-v".into(), format!("{}:{DOCKER_GUEST}/runner-up:ro", a.self_path), "-v".into(), format!("{hook}:{}:ro", l.hook)]);
    }
    for env in &l.env {
        args.extend(["-e".into(), env.clone()]);
    }
    let image = image.to_string();
    args.extend(["--entrypoint".into(), "/bin/sh".into(), image, "-c".into(), docker_entry(), "runner-up".into(), "/home/runner/run.sh".into()]);
    args
}

/// os.WriteFile(path, b, 0o755).
pub fn write_executable(path: &std::path::Path, b: &[u8]) -> Result<(), String> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o755);
    o.open(path).and_then(|mut f| f.write_all(b)).map_err(|e| e.to_string())
}
