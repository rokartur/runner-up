//! The cmd tests, and the test binary run as runner-up (testprog.rs).

use std::process::Command;
use std::sync::{Arc, OnceLock};

use crate::config;
use crate::scaleset::{Client, SystemInfo};
use crate::version::version;

/// The end-to-end tests run the coordinator as this test program started
/// again with SERVE_ENV set: it serves the stand-in for GitHub's Actions
/// service at ACTIONS_ENV, and its place runners are the program at
/// SELF_ENV.
pub const SERVE_ENV: &str = "RUNNER_UP_TEST_SERVE_CONFIG";
pub const ACTIONS_ENV: &str = "RUNNER_UP_TEST_ACTIONS";
pub const SELF_ENV: &str = "RUNNER_UP_TEST_SELF";
/// Names a file the coordinator adds a line to after each pass of its serve
/// loop, for a test to count.
pub const PASSES_ENV: &str = "RUNNER_UP_TEST_PASSES";
/// Runs machine add's ensure_agent in a program of its own, whose HOME and
/// PATH a test sets without touching the other tests'.
const ENSURE_AGENT: &str = "test-ensure-agent";

/// This test program run as runner-up: by a test, by the coordinator
/// checking it before it restarts as it, restarting as it, or on a machine
/// as its agent.
pub fn program(args: &[String]) -> i32 {
    if let Ok(path) = std::env::var(SERVE_ENV)
        && args.first().map(String::as_str) == Some("serve")
    {
        return serve_stand_in(&path);
    }
    if args.first().map(String::as_str) == Some(ENSURE_AGENT) {
        return init::ensure_agent_as_program(&args[1..]);
    }
    super::main(args)
}

/// This test program copied as runner-up, which makes it the program
/// (testprog.rs). The copy sits beside the test binary, in the build folder.
pub fn runner_up() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let me = std::env::current_exe().unwrap();
        let dir = me.with_extension("prog");
        let path = dir.join(if cfg!(windows) { "runner-up.exe" } else { "runner-up" });
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(format!("copy-{}", std::process::id()));
        std::fs::copy(&me, &tmp).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        path.to_string_lossy().into_owned()
    })
}

/// runner-up with args.
pub fn command(args: &[&str]) -> Command {
    let mut c = Command::new(runner_up());
    c.args(args);
    c
}

/// Runs runner-up with args, and returns what it wrote and its exit code.
pub fn cli_run(args: &[&str]) -> (String, String, i32) {
    let (code, out, err) = crate::agent::tests::run_apart(&mut command(args), "");
    (out, err, code)
}

/// A folder removed when the test ends.
pub fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn serve_stand_in(path: &str) -> i32 {
    let cfg = match config::load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let actions = std::env::var(ACTIONS_ENV).unwrap_or_default();
    let clients: crate::coordinator::ClientFactory = Arc::new(move |_, target: &str| {
        let info =
            SystemInfo { system: "runner-up".into(), version: version().into(), subsystem: "coordinator".into(), ..Default::default() };
        Client::with_token(&format!("{actions}/{target}"), "local-only", info).map(Arc::new).map_err(|e| e.to_string())
    });
    let passed: Option<Box<dyn Fn() + Send>> = std::env::var(PASSES_ENV).ok().map(|passes| {
        Box::new(move || {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&passes) {
                let _ = f.write_all(b"pass\n");
            }
        }) as Box<dyn Fn() + Send>
    });
    super::serve_with(cfg, path, &std::env::var(SELF_ENV).unwrap_or_default(), clients, None, passed)
}

#[cfg(not(windows))]
mod borrow;
#[cfg(not(windows))]
mod borrow_live;
mod cli;
#[cfg(not(windows))]
mod drain;
#[cfg(not(windows))]
mod e2e;
mod init;
#[cfg(not(windows))]
mod journeys;
#[cfg(not(windows))]
mod reload;
mod setup;
mod sizing;
#[cfg(not(windows))]
mod sizing_e2e;
#[cfg(not(windows))]
mod status;
#[cfg(not(windows))]
mod unreachable;
#[cfg(not(windows))]
mod update_e2e;
mod usage;
mod windows;
