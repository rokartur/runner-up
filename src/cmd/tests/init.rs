// Intent (README, "Setting up"), and the bug a live run found: adding a machine
// a second time, to another setup, failed. machine add had installed runner-up
// in ~/.local/bin, which a command run over ssh does not search, and then
// refused to install it again from a build for another system.

use super::{ENSURE_AGENT, command, runner_up, temp_dir};
use crate::cmd::init::{INSTALLED_AGENT, ensure_agent};
use crate::config::Machine;

/// The stand-in ssh runs its command, the last argument, in $HOME, as ssh
/// does on the machine.
const SSH_IN_HOME: &str = "#!/bin/sh\nfor last; do :; done\ncd \"$HOME\" && exec sh -c \"$last\"\n";

/// ensure_agent for the machine args name, reached at the ssh destination
/// after it, in this program: it prints the agent machine add chose, then
/// what it said.
pub fn ensure_agent_as_program(args: &[String]) -> i32 {
    let mut m = Machine { name: args[0].clone(), ssh: vec![args[1].clone()], ..Default::default() };
    let mut out = Vec::new();
    if let Err(e) = ensure_agent(&mut m, "", &mut out) {
        eprintln!("{e}");
        return 1;
    }
    print!("{}\n{}", m.agent, String::from_utf8_lossy(&out));
    0
}

/// ensure_agent for machine name at destination ssh, with the stand-in ssh
/// first on PATH, under home: its exit code, the agent and what it said.
#[cfg(unix)]
fn ensure_agent_over_stand_in_ssh(home: &std::path::Path, bin: &std::path::Path, name: &str, ssh: &str) -> (i32, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin.join("ssh");
    std::fs::write(&path, SSH_IN_HOME).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = command(&[ENSURE_AGENT, name, ssh]);
    c.env("HOME", home).env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let (code, out, err) = crate::agent::tests::run_apart(&mut c, "");
    assert_eq!(code, 0, "machine add did not find or install runner-up: {err}");
    let (agent, said) = out.split_once('\n').unwrap();
    (code, agent.into(), said.into())
}

#[cfg(unix)]
#[test]
fn machine_add_finds_the_agent_an_earlier_machine_add_installed() {
    let dir = temp_dir();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let installed = home.join(INSTALLED_AGENT);
    std::os::unix::fs::symlink(runner_up(), &installed).unwrap();

    let (_, agent, said) = ensure_agent_over_stand_in_ssh(&home, &bin, "lima", "ci@lima");
    assert!(agent == INSTALLED_AGENT && said == "lima: runner-up dev\n", "machine add chose agent {agent:?} and said {said:?}");
    let target = std::fs::read_link(&installed);
    assert!(
        matches!(&target, Ok(t) if t.to_str() == Some(runner_up())),
        "machine add replaced the installed runner-up: {target:?}"
    );
}

// Intent (README, "Setting up"), and the bug building it found: machine add
// compared a Mac's uname, which says arm64, with Linux's name for it, aarch64,
// and refused to install itself on a Mac like the one it ran on.
#[cfg(unix)]
#[test]
fn machine_add_installs_itself_on_a_machine_like_its_own() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    // The machine is this computer, reached over the stand-in ssh, with no
    // runner-up yet: its real uname answers.
    let (_, agent, said) = ensure_agent_over_stand_in_ssh(&home, &bin, "mac", "you@mac");
    assert!(
        agent == INSTALLED_AGENT && said == "mac: installed runner-up in ~/.local/bin\n",
        "machine add chose agent {agent:?} and said {said:?}"
    );
    let info = std::fs::metadata(home.join(INSTALLED_AGENT));
    assert!(matches!(&info, Ok(i) if i.permissions().mode() & 0o111 != 0), "no runnable runner-up was installed: {info:?}");
}
