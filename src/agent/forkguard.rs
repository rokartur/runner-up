//! The fork guard is every runner's job-started hook, so a job from another
//! repository's pull request runs nothing: the runner fails the job at "Set
//! up runner", before any of its steps. runner-up runs it on every provider
//! unless allowForkPullRequests is set. It reads only the job's event file,
//! which the runner writes from GitHub's payload just before the hook runs; a
//! workflow cannot point the hook at another file, since the hook's
//! environment is the runner's, not the job's.
//!
//! Refused:
//! - an event whose pull_request head repository is not its base repository,
//!   or has no id (a deleted fork); pull_request, pull_request_target,
//!   pull_request_review and pull_request_review_comment events all carry
//!   pull_request;
//! - a workflow_run event whose run came from another repository;
//! - no event file, or one that is not a single JSON object.
//!
//! Every other job is allowed. A job started by a comment names no head
//! repository and is allowed.

use std::io::Write;

use serde_json::Value;

use crate::quote::sh_quote;

/// The runner runs a hook by its file extension, so the hook it is given is a
/// two-line script that runs this program's fork-guard command.
pub fn fork_guard_hook(agent: &str) -> String {
    format!("#!/bin/bash\nexec {} fork-guard\n", sh_quote(agent))
}

const UNREADABLE_EVENT: &str = "unreadable event file";

/// Decides on the event file at path: Ok allows the job, an error refuses it
/// and says why.
pub fn fork_guard(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("no event file".into());
    }
    let b = std::fs::read(path).map_err(|_| "no event file".to_string())?;
    // serde_json rejects anything after the one value but whitespace, as
    // Go's decoder followed by a check for EOF does.
    let event: serde_json::Map<String, Value> = serde_json::from_slice(&b).map_err(|_| UNREADABLE_EVENT.to_string())?;
    if let Some(pr) = event.get("pull_request").filter(|v| !v.is_null())
        && !same_repo(pr.pointer("/head/repo/id"), pr.pointer("/base/repo/id"))
    {
        return Err(format!("pull request from {}", name_or(pr.pointer("/head/repo/full_name"), "a deleted fork")));
    }
    if let Some(run) = event.get("workflow_run").filter(|v| !v.is_null())
        && !same_repo(run.pointer("/head_repository/id"), run.pointer("/repository/id"))
    {
        return Err(format!("follow-on run of {}", name_or(run.pointer("/head_repository/full_name"), "a deleted fork")));
    }
    Ok(())
}

/// Whether head and base are the same positive repository id.
fn same_repo(head: Option<&Value>, base: Option<&Value>) -> bool {
    let id = |v: Option<&Value>| v.and_then(Value::as_i64);
    match (id(head), id(base)) {
        (Some(h), Some(b)) => h > 0 && h == b,
        _ => false,
    }
}

fn name_or(v: Option<&Value>, fallback: &str) -> String {
    match v.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => fallback.to_string(),
    }
}

/// The fork-guard command, as the runner's hook runs it.
pub fn run_fork_guard(stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let var = |name| std::env::var(name).unwrap_or_default();
    fork_guard_say(&var("GITHUB_EVENT_PATH"), &var("GITHUB_EVENT_NAME"), stdout, stderr)
}

/// The fork guard's verdict on the event file at path, named name, as the
/// command says it.
pub(crate) fn fork_guard_say(path: &str, name: &str, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    if let Err(e) = fork_guard(path) {
        let _ = writeln!(
            stderr,
            "runner-up refuses this job: {e}. Jobs from another repository's pull request never run on these runners."
        );
        return 1;
    }
    let name = if name.is_empty() { "event" } else { name };
    let _ = writeln!(stdout, "runner-up: {name} job from this repository; allowed");
    0
}

#[cfg(test)]
#[path = "tests/forkguard.rs"]
mod tests;
