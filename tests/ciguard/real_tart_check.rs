use std::process::Command;
use std::time::Duration;

use crate::{program_in, run};

// Intent (CONTRIBUTING.md, "Before a release"): CI installs no Tart, so the
// tests against real Tart skip there and report passed without running.
// make test-real-tart is the check run before a release instead. It must run
// every test that uses the real-Tart harness and nothing else, and on a
// machine without Tart it must fail each of them rather than skip. The bug it
// catches: a new real-Tart test named outside the check's selection, or a
// check that passes on a machine with no Tart. This test's own name must stay
// outside that selection, or the check would run it again.
#[test]
fn tart_release_check_covers_every_live_tart_test() {
    if std::env::var_os("RUNNER_UP_TEST_SHORT").is_some() {
        return;
    }
    let want = real_tart_tests();
    assert!(!want.is_empty(), "no test in src/cmd/tests calls real_tart_borrow_e2e");

    // A PATH with cargo, rustc and make but no Tart, wherever Tart is installed.
    let Some(cargo) = std::env::var_os("CARGO") else { return };
    let toolchain = std::path::Path::new(&cargo).parent().unwrap().to_path_buf();
    let bin = tempfile::tempdir().unwrap();
    let Some(make) = crate::look_path("make") else { return };
    std::os::unix::fs::symlink(make, bin.path().join("make")).unwrap();
    let dirs = [bin.path().to_path_buf(), toolchain, "/usr/bin".into(), "/bin".into()];
    if dirs.iter().any(|d| program_in(d, "tart").is_some()) {
        eprintln!("Tart is installed beside the toolchain or in /usr/bin or /bin");
        return;
    }
    let mut cmd = Command::new("make");
    cmd.arg("test-real-tart").env_remove("RUNNER_UP_REAL_TART").env("PATH", std::env::join_paths(&dirs).unwrap());
    let (code, out) = run(&mut cmd, Duration::from_secs(900));
    assert_ne!(code, Some(0), "make test-real-tart passed on a machine without Tart:\n{out}");

    let mut ran = Vec::new();
    for line in out.lines() {
        if let Some((name, _)) = line.strip_prefix("test ").and_then(|rest| rest.split_once(" ... ")) {
            ran.push(name.to_string());
        }
    }
    ran.sort();
    let mut failed: Vec<_> = ran.iter().filter(|name| out.lines().any(|l| l == format!("    {name}"))).cloned().collect();
    failed.sort();
    let short = |names: &[String]| names.iter().map(|n| n.rsplit("::").next().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(short(&ran), want, "the check must run every real-Tart test and nothing else\n{out}");
    assert_eq!(short(&failed), want, "without Tart the check must fail every real-Tart test\n{out}");
    assert_eq!(out.matches("Tart is not installed").count(), want.len(), "every test must say Tart is not installed\n{out}");
}

/// The tests in src/cmd/tests that call real_tart_borrow_e2e, sorted.
/// shortcut: reads `#[test]` and `fn` lines at column 0 rather than parsing
/// Rust, enough for the files' rustfmt layout; switch to syn if it misreads.
fn real_tart_tests() -> Vec<String> {
    let mut names = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir("src/cmd/tests").unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let (mut marked, mut current) = (false, None);
        for line in text.lines() {
            if line == "#[test]" {
                marked = true;
            } else if let Some(rest) = line.strip_prefix("fn ") {
                current = marked.then(|| rest.split('(').next().unwrap().to_string());
                marked = false;
            } else if line.contains("real_tart_borrow_e2e(")
                && let Some(name) = current.take()
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}
