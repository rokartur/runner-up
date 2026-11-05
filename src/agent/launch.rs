//! Every place starts its runner the same way, whichever provider makes it and
//! whatever language its start script is in (Rust, shell, Python or
//! PowerShell). Two things are decided here, once, for all of them:
//!
//! - The registration reaches the place only on standard input or a VM's
//!   serial port, and the runner gets it only in REGISTRATION_VAR, which the
//!   runner reads and removes before any job starts. No launch holds it: a
//!   RunnerLaunch is made without it.
//! - The fork guard is the runner's job-started hook unless the place allows
//!   forks' pull requests (AgentRun::allow_forks).
//!
//! Each provider says only where its place keeps the hook file and what the
//! file runs (its launch method), and renders the RunnerLaunch into its own
//! start: an environment, docker's -e flags, shell exports, the guest
//! program's settings, or the account job's start script.

use std::collections::BTreeMap;
use std::path::Path;

use super::AgentRun;
use crate::quote::sh_quote;

/// The one variable the runner reads its one-time registration from.
pub const REGISTRATION_VAR: &str = "ACTIONS_RUNNER_INPUT_JITCONFIG";
/// The runner's job-started hook, the fork guard.
pub const HOOK_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";

/// What runner-up hands a place's runner besides its registration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunnerLaunch {
    /// runner-up's variables for the runner, NAME=value: the place's size
    /// and, unless forks are allowed, HOOK_VAR naming hook.
    pub env: Vec<String>,
    /// The fork guard's hook file as the runner sees it, and guard that
    /// file's content. Both are empty when forks are allowed.
    ///
    /// A path may start with one variable its start script expands: $HOME/
    /// in a tart clone, whose user the agent does not know, or $dir\ in an
    /// account job, whose folder the service picks. shell_word and ps_word
    /// keep it live.
    pub hook: String,
    pub guard: String,
}

impl AgentRun {
    /// Every place's RunnerLaunch: hook is where the place keeps the fork
    /// guard's hook file, as its runner sees it, and guard that file's content.
    pub fn launch_with(&self, hook: &str, guard: &str) -> RunnerLaunch {
        let mut l = RunnerLaunch::default();
        if !self.allow_forks {
            l.hook = hook.to_string();
            l.guard = guard.to_string();
            l.env.push(format!("{HOOK_VAR}={hook}"));
        }
        l.env.extend(self.size_env());
        l
    }
}

/// Where a container sees runner-up and its hook, mounted read-only from the
/// machine.
pub const DOCKER_GUEST: &str = "/opt/runner-up";

/// Where the agent copies runner-up and its hook into a KVM virtual machine,
/// in the home folder of its user runner.
pub const KVM_GUEST: &str = "/home/runner/.runner-up";

/// Where a tart clone keeps runner-up and its hook: in its user's home folder,
/// not under the shared folder's spaces. The hook runs the runner-up beside
/// it, whatever the runner's home folder is then.
pub const TART_GUEST: &str = "$HOME/.runner-up";
pub const TART_GUARD: &str = "#!/bin/bash\nexec \"$(dirname \"$0\")/runner-up\" fork-guard\n";

/// A Linux guest, a Hyper-V VM or an Apple container guest, cannot run
/// runner-up's own program, so its hook runs the guest program's guard.
pub const LINUX_GUEST_DIR: &str = "/opt/runner-up";
pub const LINUX_GUEST_GUARD: &str = "#!/bin/bash\nexec /usr/bin/python3 /opt/runner-up/guest.py guard\n";

/// Quotes v for a POSIX shell, keeping a leading $HOME/ for the shell to
/// expand.
pub fn shell_word(v: &str) -> String {
    match v.strip_prefix("$HOME/") {
        Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
        None => sh_quote(v),
    }
}

/// path.Dir for slash-separated paths.
fn slash_dir(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => ".",
    }
}

/// l as shell commands: they write the hook file, when there is one, and
/// export runner-up's variables.
pub fn shell_launch(l: &RunnerLaunch) -> String {
    let mut b = String::new();
    if !l.hook.is_empty() {
        b += &format!("mkdir -p {}\n", shell_word(slash_dir(&l.hook)));
        b += &format!("printf '%s' {} > {}\n", sh_quote(&l.guard), shell_word(&l.hook));
        b += &format!("chmod 0755 {}\n", shell_word(&l.hook));
    }
    for kv in &l.env {
        let (name, value) = kv.split_once('=').unwrap_or((kv, ""));
        b += &format!("export {name}={}\n", shell_word(value));
    }
    b
}

/// The host runner's environment: the agent's own, without any runner
/// variable of its own, then this place's. The registration came on the
/// agent's standard input; only the runner's environment holds it, in the
/// variable the runner reads and removes.
pub fn host_env(agent_env: &[String], l: &RunnerLaunch, jit: &str, tools: &str) -> Vec<String> {
    let mut env: Vec<String> = agent_env
        .iter()
        .filter(|kv| {
            !kv.starts_with("ACTIONS_RUNNER_") && !kv.starts_with("RUNNER_TOOL_CACHE=") && !kv.starts_with("RUNNER_UP_")
        })
        .cloned()
        .collect();
    env.extend(l.env.iter().cloned());
    env.push(format!("{REGISTRATION_VAR}={jit}"));
    env.push(format!("RUNNER_TOOL_CACHE={tools}"));
    env
}

/// Starts the runner in a KVM virtual machine, over SSH. The registration goes
/// in on the session's input, never in a command line; the runner removes the
/// variable from its environment before any job starts. Without ICU, .NET
/// runs in its invariant mode.
pub fn kvm_script(l: &RunnerLaunch) -> String {
    format!(
        "read -r {REGISTRATION_VAR}; export {REGISTRATION_VAR}\n{}/sbin/ldconfig -p 2>/dev/null | grep -q libicuuc || export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1\nexec ./actions-runner/run.sh\n",
        shell_launch(l)
    )
}

pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// v as a PowerShell expression, keeping a leading $dir\ for the script to
/// expand.
pub fn ps_word(v: &str) -> String {
    match v.strip_prefix("$dir\\") {
        Some(rest) => format!("(Join-Path $dir {})", ps_quote(rest)),
        None => ps_quote(v),
    }
}

/// l's variables as PowerShell; the service writes the hook file itself.
pub fn ps_launch(l: &RunnerLaunch) -> String {
    let mut lines = Vec::new();
    for kv in &l.env {
        let (name, value) = kv.split_once('=').unwrap_or((kv, ""));
        lines.push(format!("$env:{name} = {}", ps_word(value)));
    }
    lines.join("\n")
}

/// l as the Linux guest program's settings: one JSON object, which is also a
/// Python literal since it holds only strings.
pub fn guest_launch(l: &RunnerLaunch) -> String {
    let mut env = BTreeMap::new();
    for kv in &l.env {
        let (name, value) = kv.split_once('=').unwrap_or((kv, ""));
        env.insert(name, value);
    }
    serde_json::json!({"env": env, "hook": l.hook, "guard": l.guard}).to_string()
}

/// A host place's job folder, removed after the job.
pub fn host_job_dir(a: &AgentRun) -> std::path::PathBuf {
    a.dir().join("host")
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
