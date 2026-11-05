//! A host place runs each job on the machine itself, as the agent's user: the
//! runner is unpacked into a folder made for the job, runs in a process group
//! of its own, and the group and the folder are removed after the job.
//! Nothing else is fresh: the job sees the user's home folder, programs and
//! network, and keeps whatever it changes outside its folder. Its cores and
//! memory are only its share of the machine's budget. Its tool cache is kept
//! between jobs, so a tool an action downloads is downloaded once.

use super::AgentRun;
use super::launch::RunnerLaunch;

pub struct Host;

#[cfg(windows)]
mod windows {
    use std::sync::Arc;

    use super::{Host, RunnerLaunch, launch};
    use crate::agent::{AgentRun, Out, Provider};
    use crate::ctx::Ctx;

    const HOST_ON_WINDOWS: &str = "host places do not run on Windows machines; use account places";

    impl Provider for Host {
        fn run(&self, _: &Ctx, _: &AgentRun, _: &str, _: &Arc<Out>) -> Result<(), String> {
            Err(HOST_ON_WINDOWS.into())
        }
        fn clean(&self, _: &Ctx, _: &AgentRun) -> Result<(), String> {
            Ok(())
        }
        fn launch(&self, a: &AgentRun) -> RunnerLaunch {
            launch(a)
        }
        fn ready(&self) -> String {
            HOST_ON_WINDOWS.into()
        }
    }
}

/// A host place's hook sits in its job folder. The runner runs the hook's
/// path unquoted, which Host::run checks.
fn launch(a: &AgentRun) -> RunnerLaunch {
    let hook = super::launch::host_job_dir(a).join("fork-guard.sh");
    a.launch_with(&hook.to_string_lossy(), &super::fork_guard_hook(&a.self_path))
}

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Host, launch};
    use crate::agent::docker::write_executable;
    use crate::agent::launch::{RunnerLaunch, host_env, host_job_dir};
    use crate::agent::runner::{runner_ready, runner_tarball};
    use crate::agent::usage_measure::{measure_group, write_usage};
    use crate::agent::{AgentRun, Out, Provider, ensure_dir, first_line, spawn_into, wait_or_stop};
    use crate::ctx::Ctx;

    fn pid_file(a: &AgentRun) -> PathBuf {
        a.dir().join("host.pid")
    }

    fn tool_cache(a: &AgentRun) -> PathBuf {
        if a.tool_cache.is_empty() { a.dir().join("tool-cache") } else { PathBuf::from(&a.tool_cache) }
    }

    fn kill_group(pgid: i32, signal: i32) {
        unsafe { libc::kill(-pgid, signal) };
    }

    impl Provider for Host {
        fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
            use std::os::unix::process::CommandExt;
            let tarball = runner_tarball(ctx, &PathBuf::from(&a.state_dir).join("cache"))?;
            let dir = host_job_dir(a);
            let runner = ensure_dir(&dir.join("runner"))?;
            let (b, res) = crate::proc::combined_output(ctx, Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&runner));
            if let Err(e) = res {
                return Err(format!("unpacking the runner: {}", first_line(&String::from_utf8_lossy(&b), &e)));
            }
            let tools = ensure_dir(&tool_cache(a))?;
            let l = self.launch(a);
            if !l.hook.is_empty() {
                // The runner runs its job-started hook's path unquoted.
                if l.hook.contains([' ', '\t', '\n']) {
                    return Err(format!(
                        "the fork guard's path {} has a space, which the runner cannot run: pass the agent a --state-dir without one",
                        crate::go::quote(&l.hook)
                    ));
                }
                write_executable(std::path::Path::new(&l.hook), l.guard.as_bytes())?;
            }
            let agent_env: Vec<String> =
                std::env::vars_os().map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy())).collect();
            let mut cmd = Command::new(runner.join("run.sh"));
            cmd.current_dir(&runner).env_clear().stdin(std::process::Stdio::null()).process_group(0);
            for kv in host_env(&agent_env, &l, jit, &tools.to_string_lossy()) {
                let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                cmd.env(k, v);
            }
            let (mut child, piped) = spawn_into(&mut cmd, out).map_err(|e| format!("starting the runner: {e}"))?;
            let pgid = child.id() as i32;
            let measure = measure_group(pgid as i64);
            let _ = std::fs::write(pid_file(a), format!("{pgid}\n"));
            let _ = wait_or_stop(ctx, &mut child, &|| kill_group(pgid, libc::SIGTERM), Duration::from_secs(30));
            // A process the job leaves running keeps the runner's output open;
            // once the runner has ended, that output is not waited for.
            piped.finish(Duration::from_secs(5));
            // Whatever the job left running in the group goes with it. The
            // group's number cannot be reused while any of it lives.
            kill_group(pgid, libc::SIGKILL);
            write_usage(out, &measure.finish());
            Ok(())
        }

        fn clean(&self, _: &Ctx, a: &AgentRun) -> Result<(), String> {
            if let Ok(b) = std::fs::read_to_string(pid_file(a))
                && let Ok(pgid) = b.trim().parse::<i32>()
                && pgid > 1
            {
                kill_left_group(pgid, &host_job_dir(a).to_string_lossy());
            }
            let _ = std::fs::remove_file(pid_file(a));
            match std::fs::remove_dir_all(host_job_dir(a)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            }
        }

        fn launch(&self, a: &AgentRun) -> RunnerLaunch {
            launch(a)
        }

        fn ready(&self) -> String {
            let msg = runner_ready();
            if msg != "ok" {
                return msg;
            }
            if crate::proc::look_path("tar").is_err() {
                return "tar is not installed".into();
            }
            "ok".into()
        }
    }

    /// Kills process group pgid, left by an agent killed outright, if one of
    /// its processes runs from dir: the group a runner of this place leads,
    /// never another group that reused its number.
    fn kill_left_group(pgid: i32, dir: &str) {
        let Ok(o) = Command::new("ps").args(["-A", "-ww", "-o", "pgid=", "-o", "command="]).output() else {
            return;
        };
        if !o.status.success() {
            return;
        }
        for line in String::from_utf8_lossy(&o.stdout).split('\n') {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 2 {
                continue;
            }
            if f[0].parse::<i32>() == Ok(pgid) && line.contains(dir) {
                kill_group(pgid, libc::SIGKILL);
                return;
            }
        }
    }
}
