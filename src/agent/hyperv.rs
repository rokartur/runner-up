//! A Hyper-V place runs its job in a fresh Linux VM on a Windows machine: a
//! generation 2 VM on a differencing disk over the place's base disk, which
//! nothing changes, joined to a Hyper-V switch (the Default Switch, with its
//! own address translation, unless the place names another). cloud-init sets
//! it up at boot from a seed disc the agent writes: a user named runner, with
//! passwordless sudo as on GitHub's own runners, and runner-up's guest program
//! (hyperv-guest.py). The registration and the runner's output travel over the
//! VM's second serial port, which Hyper-V joins to a named pipe on the
//! machine: no network path from the machine into the VM is needed. The VM and
//! its folder are deleted before and after every job.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;

use super::launch::{LINUX_GUEST_DIR, LINUX_GUEST_GUARD, RunnerLaunch, guest_launch, ps_quote};
use super::runner::{runner_sha256, runner_tarball_name, runner_url};
use super::{AgentRun, Conn, Out, Provider};
use crate::config;
use crate::ctx::Ctx;

pub const HYPERV_LIBRARY: &str = include_str!("../../internal/agent/hyperv.ps1");
pub const HYPERV_GUEST: &str = include_str!("../../internal/agent/hyperv-guest.py");

/// The line the guest program says until it has the registration.
pub const GUEST_READY: &str = "runner-up guest: ready";

pub fn hyperv_vm_name(place: &str) -> String {
    format!("runner-up-{place}")
}

pub fn hyperv_pipe(place: &str) -> String {
    format!(r"\\.\pipe\runner-up-{place}")
}

pub struct HyperV;

/// What a Hyper-V place's runner gets; shared with Apple container places,
/// which run the same guest program.
pub fn linux_guest_launch(a: &AgentRun) -> RunnerLaunch {
    a.launch_with(&format!("{LINUX_GUEST_DIR}/fork-guard.sh"), LINUX_GUEST_GUARD)
}

/// The place's cloud-init seed: no secret, since the registration comes later
/// over the serial port.
pub fn hyperv_seed(place: &str, arch: &str, l: &RunnerLaunch, now_nanos: i64) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let guest = linux_guest(arch, l)?;
    let user = format!(
        "#cloud-config\nusers:\n  - name: runner\n    shell: /bin/bash\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    lock_passwd: true\nssh_pwauth: false\ndisable_root: true\nwrite_files:\n  - path: /opt/runner-up/guest.py\n    permissions: \"0755\"\n    encoding: b64\n    content: {}\nruncmd:\n  - [/usr/bin/python3, /opt/runner-up/guest.py]\n",
        base64::engine::general_purpose::STANDARD.encode(guest)
    );
    let meta = format!("instance-id: runner-up-{place}-{now_nanos}\nlocal-hostname: runner-up-{place}\n");
    Ok(BTreeMap::from([("user-data".to_string(), user.into_bytes()), ("meta-data".to_string(), meta.into_bytes())]))
}

/// runner-up's guest program for a Linux guest of arch, filled in for one
/// job: the pinned runner and the runner's launch l. It holds no secret.
pub fn linux_guest(arch: &str, l: &RunnerLaunch) -> Result<String, String> {
    let Some(sum) = runner_sha256(&format!("linux-{arch}")) else {
        return Err(format!("no runner release for {arch}"));
    };
    let fills =
        [("@RUNNER_URL@", runner_url(&runner_tarball_name(arch))), ("@RUNNER_SHA256@", sum.to_string()), ("@LAUNCH@", guest_launch(l))];
    Ok(replace_all(HYPERV_GUEST, &fills))
}

/// strings.NewReplacer(...).Replace(s): one pass, leftmost match first, the
/// earlier pair first at the same position; a replacement is not scanned again.
pub(super) fn replace_all(s: &str, fills: &[(&str, String)]) -> String {
    let mut out = String::new();
    let mut rest = s;
    'scan: while !rest.is_empty() {
        for (i, _) in rest.char_indices() {
            for (from, to) in fills {
                if rest[i..].starts_with(from) {
                    out += &rest[..i];
                    out += to;
                    rest = &rest[i + from.len()..];
                    continue 'scan;
                }
            }
        }
        out += rest;
        break;
    }
    out
}

/// Makes the place's VM in dir, whose seed disc the agent has written, and
/// starts it.
pub fn hyperv_create_script(a: &AgentRun, dir: &str) -> String {
    let size = if a.disk_gb <= 0 { config::DEFAULT_DISK_GB } else { a.disk_gb };
    let name = ps_quote(&hyperv_vm_name(&a.place));
    let switch = if a.switch.is_empty() { config::DEFAULT_SWITCH } else { &a.switch };
    format!(
        "$dir = {}\nNew-RunnerUpDisk \"$dir\\disk.vhdx\" {} {}\n$null = New-RunnerUpVM -Name {name} -Dir $dir -Vhd \"$dir\\disk.vhdx\" -Dvd \"$dir\\seed.iso\" -MemoryMB {} -Cpu {} -Switch {} -Pipe {}\nStart-RunnerUpVM {name}\n",
        ps_quote(dir),
        ps_quote(&a.image),
        (size as u64) << 30,
        a.memory_mb,
        a.cores,
        ps_quote(switch),
        ps_quote(&hyperv_pipe(&a.place)),
    )
}

/// Turns off and deletes the place's VM.
pub fn hyperv_remove_script(place: &str) -> String {
    format!("Remove-RunnerUpVM {}\n", ps_quote(&hyperv_vm_name(place)))
}

/// The arguments that run script, after the Hyper-V functions, in Windows
/// PowerShell.
pub fn encoded_powershell(script: &str) -> Vec<String> {
    super::encoded_script(&format!("{HYPERV_LIBRARY}\n{script}"))[1..].to_vec()
}

/// Whether err from reading the serial port is its end: the VM turned off and
/// Hyper-V closed the pipe.
fn guest_gone(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(err.kind(), ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe) {
        return true;
    }
    // ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED.
    cfg!(windows) && matches!(err.raw_os_error(), Some(109) | Some(233))
}

#[derive(PartialEq)]
enum Timer {
    Pending,
    Stopped,
    Fired,
}

/// The agent's side of the serial port: it waits up to boot for the guest
/// program to say it is ready, sends it the registration, and copies
/// everything else it says to out until the VM turns off and the pipe ends.
/// It closes conn when ctx is done.
pub fn talk_to_guest(ctx: &Ctx, conn: Conn, jit: &str, out: &Out, boot: Duration) -> Result<(), String> {
    let Conn { reader, mut writer, close } = conn;
    let watch = ctx.with_cancel();
    {
        let (watch, ctx, close) = (watch.clone(), ctx.clone(), close.clone());
        std::thread::spawn(move || {
            watch.wait();
            if ctx.is_done() {
                close();
            }
        });
    }
    let timer = Arc::new(Mutex::new(Timer::Pending));
    {
        let (watch, timer, close) = (watch.clone(), timer.clone(), close.clone());
        std::thread::spawn(move || {
            if watch.sleep(boot) {
                return;
            }
            let mut t = timer.lock().unwrap();
            if *t == Timer::Pending {
                *t = Timer::Fired;
                close();
            }
        });
    }
    let result = read_guest(ctx, reader, &mut writer, &timer, jit, out, boot);
    watch.cancel();
    close();
    result
}

fn read_guest(
    ctx: &Ctx,
    reader: Box<dyn Read + Send>,
    writer: &mut Box<dyn Write + Send>,
    timer: &Mutex<Timer>,
    jit: &str,
    out: &Out,
    boot: Duration,
) -> Result<(), String> {
    let not_started = || format!("the VM did not start runner-up's guest program within {}", crate::go::duration::format(boot));
    let mut r = BufReader::new(reader);
    let mut sent = false;
    loop {
        let mut b = Vec::new();
        let res = r.read_until(b'\n', &mut b);
        let line = String::from_utf8_lossy(&b).into_owned();
        if line.trim_end_matches(['\r', '\n']) == GUEST_READY {
            if !sent {
                {
                    let mut t = timer.lock().unwrap();
                    if *t != Timer::Pending {
                        return Err(not_started());
                    }
                    *t = Timer::Stopped;
                }
                if let Err(e) = writer.write_all(format!("{jit}\n").as_bytes()).and_then(|_| writer.flush()) {
                    return Err(format!("could not send the registration to the VM: {e}"));
                }
                sent = true;
            }
        } else if !line.is_empty() {
            out.write(line.as_bytes());
            if !line.ends_with('\n') {
                out.write(b"\n");
            }
        }
        let err = match res {
            Ok(n) if n > 0 && b.ends_with(b"\n") => continue,
            Ok(_) => std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            Err(e) => e,
        };
        if ctx.is_done() {
            return Ok(());
        }
        if !sent {
            return Err(not_started());
        }
        if guest_gone(&err) {
            return Ok(());
        }
        return Err(format!("the VM's serial port: {err}"));
    }
}

#[cfg(not(windows))]
const WINDOWS_ONLY: &str = "runs only on Windows";

#[cfg(not(windows))]
impl Provider for HyperV {
    fn run(&self, _: &Ctx, _: &AgentRun, _: &str, _: &Arc<Out>) -> Result<(), String> {
        Err(WINDOWS_ONLY.into())
    }
    fn clean(&self, _: &Ctx, _: &AgentRun) -> Result<(), String> {
        Ok(())
    }
    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        linux_guest_launch(a)
    }
    fn ready(&self) -> String {
        WINDOWS_ONLY.into()
    }
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;

    use super::*;
    use crate::agent::{Conn, ensure_dir, first_line, winpipe};

    fn vm_dir(a: &AgentRun) -> PathBuf {
        a.dir().join("vm")
    }

    /// Runs script after the Hyper-V functions and returns the first line of
    /// what went wrong.
    pub fn powershell(ctx: &Ctx, script: &str) -> Result<(), String> {
        let (b, res) = crate::proc::combined_output(ctx, Command::new("powershell.exe").args(encoded_powershell(script)));
        res.map_err(|e| first_line(&String::from_utf8_lossy(&b), &e))
    }

    impl Provider for HyperV {
        fn clean(&self, ctx: &Ctx, a: &AgentRun) -> Result<(), String> {
            powershell(ctx, &hyperv_remove_script(&a.place)).map_err(|e| format!("could not delete the VM: {e}"))?;
            // Hyper-V may hold the disk for a moment after deleting the VM.
            let mut err = String::new();
            for _ in 0..30 {
                match std::fs::remove_dir_all(vm_dir(a)) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => err = e.to_string(),
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(err)
        }

        fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
            let msg = self.ready();
            if msg != "ok" {
                return Err(msg);
            }
            let dir = ensure_dir(&vm_dir(a))?;
            let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
            let seed = hyperv_seed(&a.place, crate::agent::runner::runner_arch(), &self.launch(a), nanos)?;
            let iso = crate::agent::iso::seed_iso(&seed)?;
            std::fs::write(dir.join("seed.iso"), iso).map_err(|e| e.to_string())?;
            powershell(ctx, &hyperv_create_script(a, &dir.to_string_lossy())).map_err(|e| format!("could not make the VM: {e}"))?;
            out.say(&format!("virtual machine for place {} is starting", a.place));
            let conn = dial_guest(ctx, &hyperv_pipe(&a.place))?;
            talk_to_guest(ctx, conn, jit, out, a.boot_timeout)
        }

        fn launch(&self, a: &AgentRun) -> RunnerLaunch {
            linux_guest_launch(a)
        }

        /// "ok" when this account may make Hyper-V VMs here.
        fn ready(&self) -> String {
            let ctx = Ctx::detached_timeout(Duration::from_secs(30));
            match powershell(&ctx, "$null = Get-RunnerUpVsms\n") {
                Err(e) => format!("no Hyper-V for this account (run runner-up setup --hyperv): {e}"),
                Ok(()) => "ok".into(),
            }
        }
    }

    /// Opens the VM's serial port pipe, which Hyper-V makes as the VM starts.
    /// Before sending the registration it checks Hyper-V made it: another
    /// account could make a pipe of the same name first. Hyper-V's own pipes
    /// belong to the VM's account (NT VIRTUAL MACHINE), SYSTEM or
    /// Administrators, which a standard account cannot give a pipe.
    fn dial_guest(ctx: &Ctx, pipe: &str) -> Result<Conn, String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let h = loop {
            let err = match winpipe::open(pipe, GENERIC_READ | GENERIC_WRITE | READ_CONTROL) {
                Ok(h) => break h,
                Err(e) => e,
            };
            let code = err.raw_os_error().unwrap_or(0) as u32;
            if code != ERROR_FILE_NOT_FOUND && code != ERROR_PIPE_BUSY || Instant::now() > deadline {
                return Err(format!("could not open the VM's serial port {pipe}: {err}"));
            }
            if ctx.sleep(Duration::from_millis(200)) {
                return Err(ctx.err().unwrap_or("context canceled").to_string());
            }
        };
        let owned = winpipe::owner(h).and_then(|s| {
            if s == "S-1-5-18" || s == "S-1-5-32-544" || s.starts_with("S-1-5-83-") {
                Ok(())
            } else {
                Err(format!("the pipe belongs to {s}, not Hyper-V"))
            }
        });
        if let Err(e) = owned {
            winpipe::close(h);
            return Err(format!("{pipe}: {e}"));
        }
        winpipe::conn(h).map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
pub use windows::powershell;
