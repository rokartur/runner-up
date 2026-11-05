use std::process::Command;

use crate::agent::tart::{Tart, link_or_copy, tart_script};
use crate::agent::{AgentRun, Provider};

// Intent (README, "Setup file" and "Macs"): the tart guest's executed startup
// script must expose the configured cores and memory, including when forks are
// allowed, without retaining the registration in the reader's process
// environment. The shared folder and runner archive follow the existing Mac
// fixture; this executes the actual script, not an assertion about source
// strings.
#[test]
fn tart_guest_receives_its_size() {
    if std::env::var_os("RUNNER_UP_TEST_SHORT").is_some() {
        eprintln!("runs the guest startup and measurement programs");
        return;
    }
    let binary = crate::cmd::tests::runner_up();
    for allow in [false, true] {
        let tmp = crate::cmd::tests::temp_dir();
        let dir = tmp.path();
        let share = dir.join("share");
        std::fs::create_dir_all(&share).unwrap();
        const REGISTRATION: &str = "stand-in-registration";
        let script = format!(
            r#"#!/bin/sh
set -eu
[ "$ACTIONS_RUNNER_INPUT_JITCONFIG" = "{REGISTRATION}" ]
parent=$(ps eww -p "$PPID" -o command=)
case "$parent" in
  *"{REGISTRATION}"*) echo 'the resource reader retained the registration' >&2; exit 1 ;;
esac
printf 'job-size=%s,%s\n' "$RUNNER_UP_CORES" "$RUNNER_UP_MEMORY_MB"
"#
        );
        std::fs::write(share.join("runner.tar.gz"), super::stand_in_runner(&script).0).unwrap();
        link_or_copy(binary.as_ref(), &share.join("runner-up")).unwrap();
        let a = AgentRun { cores: 4, memory_mb: 16384, allow_forks: allow, ..Default::default() };
        let mut cmd = Command::new("bash");
        cmd.args(["-c", &tart_script(&Tart.launch(&a)), "runner-up"]).arg(&share).arg("");
        // Only disposable environment values can enter the process reading.
        cmd.env_clear().envs([
            ("HOME", dir.to_str().unwrap()),
            ("PATH", &std::env::var("PATH").unwrap()),
            ("LANG", "C"),
            ("ACTIONS_RUNNER_HOOK_JOB_STARTED", ""),
        ]);
        let (code, out) = super::run(&mut cmd, format!("{REGISTRATION}\n"));
        let want = format!("job-size={},{}", a.cores, a.memory_mb);
        assert!(code == 0 && out.contains(&want), "allowForks={allow}: guest startup exited {code}\n{out}");
        assert_ne!(dir.join(".runner-up/fork-guard.sh").exists(), allow, "fork guard state did not follow allowForks={allow}");
    }
}
