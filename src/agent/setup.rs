use crate::cli;
use crate::go::flag::FlagSet;

use super::launch::ps_quote;

const SETUP_LIBRARY: &str = include_str!("../../internal/agent/setup.ps1");

#[derive(Default)]
pub struct SetupOptions {
    pub user: String,
    pub key: String,
    pub ssh: bool,
    pub hyperv: bool,
}

/// setup.ps1 after the values it reads.
pub fn setup_script(source: &str, o: &SetupOptions) -> String {
    format!(
        "$source = {}\n$user = {}\n$key = {}\n$ssh = ${}\n$hyperv = ${}\n{SETUP_LIBRARY}",
        ps_quote(source),
        ps_quote(&o.user),
        ps_quote(&o.key),
        o.ssh,
        o.hyperv
    )
}

/// `runner-up setup`, run once on a Windows machine as an administrator.
pub fn setup_main(args: &[String]) -> i32 {
    let mut fs = FlagSet::new("setup");
    fs.string("user", &std::env::var("USERNAME").unwrap_or_default(), "the account the coordinator signs in as over SSH");
    fs.string("ssh-key", "", "a public key file (such as the coordinator's id_ed25519.pub) to let that account sign in with");
    fs.bool("ssh", false, "turn on Windows' OpenSSH server");
    fs.bool("hyperv", false, "turn on Hyper-V and let the account make VMs, for Hyper-V places");
    if let Some(code) = cli::parse(&mut fs, args, false) {
        return code;
    }
    let mut o = SetupOptions { user: fs.get_str("user"), ssh: fs.get_bool("ssh"), hyperv: fs.get_bool("hyperv"), ..Default::default() };
    if o.user.is_empty() {
        eprintln!("name the account the coordinator signs in as with --user");
        return 2;
    }
    let key_file = fs.get_str("ssh-key");
    if !key_file.is_empty() {
        match std::fs::read_to_string(&key_file) {
            Err(e) => {
                eprintln!("open {key_file}: {e}");
                return 2;
            }
            Ok(b) => o.key = b.trim().to_string(),
        }
        if o.key.contains(['\r', '\n']) || !o.key.starts_with("ssh-") && !o.key.starts_with("ecdsa-") {
            eprintln!("{key_file} is not one public key");
            return 2;
        }
    }
    if let Err(e) = run_setup(&o) {
        eprintln!("{e}");
        return 1;
    }
    0
}

#[cfg(not(windows))]
fn run_setup(_: &SetupOptions) -> Result<(), String> {
    Err("runner-up setup is for Windows machines; on Linux and macOS, machine add installs the agent".into())
}

/// Runs the script it reads from standard input. setup.ps1 is too long to
/// hand Windows PowerShell on its command line, which Windows caps at 32,767
/// characters.
#[cfg(windows)]
const SETUP_BOOTSTRAP: &str = "$script = New-Object IO.MemoryStream
[Console]::OpenStandardInput().CopyTo($script)
& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString($script.ToArray())))";

#[cfg(windows)]
fn run_setup(o: &SetupOptions) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if !super::account::elevated() {
        return Err("runner-up setup changes this machine's settings: run it from a terminal opened with Run as administrator".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new("powershell.exe");
    cmd.args(&super::encoded_script(SETUP_BOOTSTRAP)[1..]).stdin(Stdio::piped());
    // PowerShell 7 puts its own modules first in PSModulePath, which Windows
    // PowerShell started from it would then load in place of its own and fail
    // on, so setup run from PowerShell 7 drops it.
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().eq_ignore_ascii_case("PSModulePath") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("runner-up setup could not start Windows PowerShell: {e}"))?;
    let script = setup_script(&exe.to_string_lossy(), o);
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(script.as_bytes());
    });
    let status = child.wait().map_err(|e| format!("runner-up setup could not start Windows PowerShell: {e}"))?;
    let _ = writer.join();
    if !status.success() {
        return Err("runner-up setup stopped; the lines above say why. Running it again is harmless".into());
    }
    Ok(())
}
