//! Tests that guard the repository's CI, release and install files. Cargo runs
//! integration tests from the package root, so they read those files by
//! repository-relative path.

mod install_script;
// The real-Tart tests build only on Linux and macOS.
#[cfg(unix)]
mod real_tart_check;
mod release_ci_exclusions;

use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_yaml::Value;

/// ci.yml's job that gathers lint and the test matrix into one stable name, the
/// context a branch rule on main can require.
pub const CI_CHECK_CONTEXT: &str = "CI passed";
pub const RELEASE_WORKFLOW_PATH: &str = ".github/workflows/release.yml";

pub type Fields = BTreeMap<String, Value>;

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct WorkflowStep {
    pub name: String,
    pub uses: String,
    pub run: String,
    pub env: Fields,
    pub with: Fields,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct WorkflowJob {
    pub name: String,
    #[serde(rename = "if")]
    pub condition: Value,
    pub permissions: Option<BTreeMap<String, String>>,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct WorkflowFile {
    pub permissions: Option<BTreeMap<String, String>>,
    pub jobs: BTreeMap<String, WorkflowJob>,
}

pub fn read_workflow(path: &str) -> WorkflowFile {
    let data = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yaml::from_str(&data).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// A YAML scalar as the text GitHub reads it; anything else is empty.
pub fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Runs cmd to completion with stdin closed and stdout and stderr captured
/// together, as Go's CombinedOutput did. It kills cmd and fails the test once
/// limit passes, so a hang reports its cause. The exit code is None when a
/// signal ended cmd.
pub fn run(cmd: &mut Command, limit: Duration) -> (Option<i32>, String) {
    let mut out = tempfile::tempfile().unwrap();
    cmd.stdin(Stdio::null()).stdout(out.try_clone().unwrap()).stderr(out.try_clone().unwrap());
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("start {cmd:?}: {e}"));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{cmd:?} did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    out.rewind().unwrap();
    let mut bytes = Vec::new();
    out.read_to_end(&mut bytes).unwrap();
    (status.code(), String::from_utf8_lossy(&bytes).into_owned())
}

/// The first program named name on PATH, searched in PATH's order as a shell
/// would. Command::new on Windows searches System32 first, which would find
/// WSL's bash ahead of Git's.
pub fn look_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| program_in(&dir, name))
}

pub fn program_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidates = if cfg!(windows) { vec![format!("{name}.exe"), name.to_string()] } else { vec![name.to_string()] };
    candidates.into_iter().map(|c| dir.join(c)).find(|p| p.is_file())
}

#[cfg(unix)]
pub fn write_programs(dir: &Path, programs: &[(&str, &str)]) {
    use std::os::unix::fs::PermissionsExt;
    for (name, text) in programs {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
