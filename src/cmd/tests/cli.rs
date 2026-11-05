// Intent (the fix that gave each command its own flags, clean -h exits and
// clear errors): every command has its own flags and shows only those on -h,
// exiting 0; a command refuses arguments it does not take; errors go to
// standard error; a missing setup file points at runner-up init; and the usage
// names every agent command.

use super::{cli_run, temp_dir};
use crate::agent::AGENT_COMMANDS;
use crate::config::{self, Config, Pool};

/// Writes the setup runner-up init writes, before any machine add.
pub fn cli_setup(dir: &std::path::Path) -> String {
    let path = dir.join("config.json").to_string_lossy().into_owned();
    let mut cfg = Config {
        state_dir: dir.join("state").to_string_lossy().into_owned(),
        pools: vec![Pool { label: "runner-up".into(), ..Default::default() }],
        ..Default::default()
    };
    cfg.github.repos = vec!["example/app".into()];
    config::save_config(&path, &cfg).unwrap();
    path
}

/// Whether a command is the coordinator's, which runner-up refuses on Windows
/// before reading its flags.
fn coordinator_only(name: &str) -> bool {
    !["agent", "fork-guard", "setup", "service", "version"].contains(&name.split_whitespace().next().unwrap())
}

/// The commands `runner-up help` lists, each agent command on its own.
fn usage_commands() -> Vec<String> {
    let (help, _, code) = cli_run(&["help"]);
    assert_eq!(code, 0, "help exited {code}");
    let mut names = Vec::new();
    for line in help.lines() {
        let Some(rest) = line.strip_prefix("  ").filter(|r| r.starts_with(|c: char| c.is_ascii_lowercase())) else {
            continue;
        };
        let word =
            |s: &str, extra: &str| s.chars().take_while(|c| c.is_ascii_lowercase() || *c == '-' || extra.contains(*c)).collect::<String>();
        let first = word(rest, "");
        let after = &rest[first.len()..];
        let second = after.strip_prefix(' ').map(|s| word(s, "|")).unwrap_or_default();
        match first.as_str() {
            "agent" => names.extend(second.split('|').map(|sub| format!("agent {sub}"))),
            "machine" => names.push("machine add".into()),
            _ => names.push(first),
        }
    }
    // The help must list the commands runner-up runs, the agent's among
    // them; reading none would let every test over them pass having run
    // nothing.
    for sub in AGENT_COMMANDS {
        assert!(names.contains(&format!("agent {sub}")), "read commands {names:?} from runner-up help, which lacks agent {sub}:\n{help}");
    }
    assert!(names.len() > AGENT_COMMANDS.len(), "read only {names:?} from runner-up help:\n{help}");
    names
}

#[test]
fn usage_names_every_agent_command() {
    let listed: Vec<String> = usage_commands().iter().filter_map(|n| n.strip_prefix("agent ").map(String::from)).collect();
    assert_eq!(listed, AGENT_COMMANDS, "runner-up help lists agent commands {listed:?}; the agent takes {AGENT_COMMANDS:?}");
    let (out, _, code) = cli_run(&["agent", "-h"]);
    assert!(
        code == 0 && out == format!("usage: runner-up agent {}\n", AGENT_COMMANDS.join("|")),
        "runner-up agent -h exited {code} and said {out:?}"
    );
}

#[test]
fn every_commands_help_exits_zero() {
    for name in usage_commands() {
        if cfg!(windows) && coordinator_only(&name) {
            continue;
        }
        let mut args: Vec<&str> = name.split_whitespace().collect();
        args.push("-h");
        let (stdout, stderr, code) = cli_run(&args);
        assert!(
            code == 0 && !(stdout.clone() + &stderr).trim().is_empty(),
            "runner-up {name} -h exited {code} and said {stdout:?} {stderr:?}"
        );
    }
}

#[cfg(not(windows))]
#[test]
fn commands_show_only_their_own_flags() {
    // Only the coordinator's place and borrow runners take these.
    for name in ["serve", "update", "status", "usage", "check-config"] {
        let (_, stderr, code) = cli_run(&[name, "-h"]);
        assert!(code == 0 && stderr.contains("-config"), "runner-up {name} -h exited {code} and said {stderr:?}");
        for flag in ["-borrow", "-place", "-pool"] {
            assert!(!stderr.contains(flag), "runner-up {name} -h lists {flag}, which it does not take: {stderr:?}");
        }
    }
}

#[test]
fn commands_refuse_arguments_they_do_not_take() {
    let dir = temp_dir();
    let setup = cli_setup(dir.path());
    let new = dir.path().join("new.json").to_string_lossy().into_owned();
    let mut cases: Vec<Vec<&str>> = vec![
        vec!["serve", "--config", &setup],
        vec!["update", "--config", &setup],
        vec!["status", "--config", &setup],
        vec!["usage", "--config", &setup],
        vec!["check-config", "--config", &setup],
        vec!["init", "--config", &new, "--repo", "example/app"],
        vec!["machine", "add", "box", "--config", &setup],
        vec!["drain", "--config", &setup, "--place", "box-1", "--job", "build", "--label", "upgrade", "--minutes", "5"],
        vec!["setup", "--user", "ci"],
        vec!["fork-guard"],
        vec!["service"],
        vec!["version"],
        vec!["agent", "run", "--provider", "docker", "--place", "box-1"],
        vec!["agent", "clean", "--provider", "docker", "--place", "box-1"],
    ];
    for sub in AGENT_COMMANDS {
        if !["run", "clean", "measure"].contains(&sub) {
            cases.push(vec!["agent", sub]);
        }
    }
    for mut args in cases {
        if cfg!(windows) && coordinator_only(args[0]) {
            continue;
        }
        args.push("stray");
        let (stdout, stderr, code) = cli_run(&args);
        assert!(
            code == 2 && stdout.is_empty() && stderr.contains("stray"),
            "runner-up {} exited {code}, wrote {stdout:?} and said {stderr:?}",
            args.join(" ")
        );
    }
}

#[cfg(not(windows))]
#[test]
fn missing_setup_points_at_init() {
    let dir = temp_dir();
    let missing = dir.path().join("config.json").to_string_lossy().into_owned();
    let m = missing.as_str();
    for args in [
        vec!["serve", "--config", m],
        vec!["status", "--config", m],
        vec!["check-config", "--config", m],
        vec!["machine", "add", "box", "--config", m],
        vec!["drain", "--config", m, "--place", "box-1", "--job", "build", "--label", "upgrade", "--minutes", "5"],
        vec!["borrow", "--config", m, "--pool", "runner-up", "--label", "try", "--minutes", "5", "--", "true"],
    ] {
        let (stdout, stderr, code) = cli_run(&args);
        assert!(
            code == 2 && stdout.is_empty() && stderr.contains("runner-up init"),
            "runner-up {} exited {code}, wrote {stdout:?} and said {stderr:?}",
            args[0]
        );
    }
}

#[cfg(not(windows))]
#[test]
fn update_says_its_error_on_standard_error() {
    let dir = temp_dir();
    let (stdout, stderr, code) = cli_run(&["update", "--config", &cli_setup(dir.path())]);
    assert!(
        code == 2 && stdout.is_empty() && stderr.contains("names no release"),
        "runner-up update with no release exited {code}, wrote {stdout:?} and said {stderr:?}"
    );
}
