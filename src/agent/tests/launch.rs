use crate::agent::jobhost::{JobRequest, START_FILE, account_job_files};
use crate::agent::launch::{HOOK_VAR, RunnerLaunch, host_env, kvm_script};
use crate::agent::runner::runner_windows_sha256;
use crate::agent::{AgentRun, docker, hyperv, provider_for, tart};
use crate::config;

fn linux_guest_handed(_: &AgentRun, l: &RunnerLaunch) -> String {
    hyperv::linux_guest("x64", l).unwrap_or_else(|e| e)
}

fn handed(name: &str, a: &AgentRun, l: &RunnerLaunch) -> Option<String> {
    let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    Some(match name {
        config::PROVIDER_DOCKER => docker::docker_args(a, l, &a.dir().join("fork-guard.sh").to_string_lossy()).join("\n"),
        config::PROVIDER_HOST => host_env(&env, l, "", &a.dir().join("tool-cache").to_string_lossy()).join("\n"),
        config::PROVIDER_KVM => kvm_script(l),
        config::PROVIDER_TART => tart::tart_script(l),
        config::PROVIDER_ACCOUNT => {
            let req = JobRequest { cores: a.cores, memory_mb: a.memory_mb, allow_forks: a.allow_forks, ..Default::default() };
            let files = account_job_files(
                r"C:\runner-up\cache\actions-runner-win-x64-2.338.1.zip",
                runner_windows_sha256("x64").unwrap(),
                &a.self_path,
                &req,
            );
            String::from_utf8(files[START_FILE].clone()).unwrap()
        }
        config::PROVIDER_HYPERV | config::PROVIDER_CONTAINER => linux_guest_handed(a, l),
        _ => return None,
    })
}

// Intent (AGENTS.md: "Every runner starts with the fork guard as its
// job-started hook unless allowForkPullRequests is set"; the audit before
// sharing runner-up found this wiring written six times in four languages
// and asked for one launch path with one test over every place kind): for
// every provider, what its place is handed - docker's arguments, the host
// runner's environment, the shell a VM or clone runs, the Linux guest
// program, the account job's start script - sets the fork guard as the hook
// exactly when forks are not allowed, and carries the place's size.
#[test]
fn every_place_starts_its_runner_with_the_fork_guard_unless_forks_are_allowed() {
    for name in config::PROVIDERS {
        let p = provider_for(name).unwrap();
        for allow in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let a = AgentRun {
                provider: name.into(),
                place: "mini-1".into(),
                cores: 3,
                memory_mb: 6144,
                allow_forks: allow,
                state_dir: dir.path().to_string_lossy().into_owned(),
                self_path: "/usr/local/bin/runner-up".into(),
                ..Default::default()
            };
            let l = p.launch(&a);
            let Some(text) = handed(name, &a, &l) else {
                panic!("provider {name} has no row: say what its place is handed");
            };
            assert_ne!(text.contains(HOOK_VAR), allow, "{name}, forks allowed {allow}: the hook in what the place is handed:\n{text}");
            assert_ne!(
                l.env.contains(&format!("{HOOK_VAR}={}", l.hook)),
                allow,
                "{name}, forks allowed {allow}: the runner's variables {:?}",
                l.env
            );
            if allow {
                assert!(
                    l.hook.is_empty() && l.guard.is_empty(),
                    "{name} allows forks but still has a fork guard at {:?}: {:?}",
                    l.hook,
                    l.guard
                );
            } else {
                // The hook file runs runner-up's fork guard, or the Linux
                // guest program's, which decides as it does.
                assert!(
                    l.guard.contains(" fork-guard") || l.guard.contains("guest.py guard"),
                    "{name}'s hook at {:?} runs no fork guard: {:?}",
                    l.hook,
                    l.guard
                );
                assert!(text.contains("fork-guard."), "{name}'s place is not handed its hook file:\n{text}");
            }
            for want in ["RUNNER_UP_CORES", "RUNNER_UP_MEMORY_MB", "6144"] {
                assert!(text.contains(want), "{name}'s place is not handed its size ({want}):\n{text}");
            }
        }
    }
}

// Intent (the audit's finding that a new provider meant edits in five or
// more spots, Apple container places among them with no readiness check):
// `agent probe` answers for every provider this machine can run, and a Mac
// answers for Apple container places too, from the container tool's own
// status. The fake tool is named by its path, since PATH is shared by every
// test in this process.
#[cfg(unix)]
#[test]
fn probe_answers_for_every_provider_of_its_system() {
    use crate::agent::{container, probe};
    use std::os::unix::fs::PermissionsExt;
    let pr = probe();
    for name in config::PROVIDERS {
        let got = pr.ready(name);
        assert_eq!(config::WINDOWS_PROVIDERS.contains(&name), got.is_empty(), "this machine answered {got:?} for {name}");
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("container");
    let tool_s = tool.to_string_lossy().into_owned();
    for (status, want) in [
        ("#!/bin/sh\necho 'status running'\n", "ok"),
        (
            "#!/bin/sh\necho 'Error: apiserver is not running' >&2\nexit 1\n",
            "Apple's container service is not running (container system start): Error: apiserver is not running",
        ),
    ] {
        std::fs::write(&tool, status).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let want = if cfg!(target_os = "macos") { want } else { "Apple's container tool runs only on macOS" };
        assert_eq!(container::container_ready(&tool_s), want, "with container system status {status:?}");
    }
    std::fs::remove_file(&tool).unwrap();
    let got = container::container_ready(&tool_s);
    if cfg!(target_os = "macos") {
        assert!(got.starts_with("Apple's container tool is not installed"), "without the container tool the probe said {got:?}");
    }
}
