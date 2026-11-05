//! A KVM place runs each job in a new KVM virtual machine. Its disk is a
//! copy-on-write layer over the place's base cloud image, which nothing
//! changes, and is deleted after the job. The machine is set up at boot by
//! cloud-init from a seed the agent serves on the loopback address: a user
//! named runner, with passwordless sudo as on GitHub's own runners, who signs
//! in only with a key the agent makes for this one job. The agent then copies
//! in the runner and starts it over SSH (the machine's OpenSSH client), on a
//! port forwarded from the machine's loopback address.

use std::sync::Arc;

use super::launch::{KVM_GUEST, RunnerLaunch};
use super::{AgentRun, Out, Provider};
use crate::ctx::Ctx;

pub struct Kvm;

fn launch(a: &AgentRun) -> RunnerLaunch {
    a.launch_with(&format!("{KVM_GUEST}/fork-guard.sh"), &super::fork_guard_hook(&format!("{KVM_GUEST}/runner-up")))
}

#[cfg(not(target_os = "linux"))]
const UNSUPPORTED: &str = "KVM places run only on Linux";

#[cfg(not(target_os = "linux"))]
impl Provider for Kvm {
    fn run(&self, _: &Ctx, _: &AgentRun, _: &str, _: &Arc<Out>) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    fn clean(&self, _: &Ctx, _: &AgentRun) -> Result<(), String> {
        Ok(())
    }
    fn launch(&self, a: &AgentRun) -> RunnerLaunch {
        launch(a)
    }
    fn ready(&self) -> String {
        UNSUPPORTED.into()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Kvm, launch};
    use crate::agent::launch::{KVM_GUEST, RunnerLaunch, kvm_script};
    use crate::agent::runner::runner_tarball;
    use crate::agent::{AgentRun, Out, Provider, ensure_dir, first_line, spawn_into};
    use crate::config;
    use crate::ctx::Ctx;

    fn vm_dir(a: &AgentRun) -> PathBuf {
        a.dir().join("vm")
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    impl Provider for Kvm {
        fn clean(&self, _: &Ctx, a: &AgentRun) -> Result<(), String> {
            let dir = vm_dir(a);
            if let Ok(b) = std::fs::read_to_string(dir.join("qemu.pid")) {
                let pid: i32 = b.trim().parse().unwrap_or(0);
                // Only a QEMU started for this place's folder: the pid may be reused.
                if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline"))
                    && pid > 1
                    && contains(&cmdline, dir.to_string_lossy().as_bytes())
                {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    for _ in 0..100 {
                        if !alive(pid) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            match std::fs::remove_dir_all(&dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            }
        }

        fn run(&self, ctx: &Ctx, a: &AgentRun, jit: &str, out: &Arc<Out>) -> Result<(), String> {
            let msg = self.ready();
            if msg != "ok" {
                return Err(msg);
            }
            let dir = ensure_dir(&vm_dir(a))?;
            let tarball = runner_tarball(ctx, &Path::new(&a.state_dir).join("cache"))?;
            let disk = overlay(ctx, a, &dir)?;
            let key = make_key(ctx, &dir)?;
            let seed = serve_seed(&a.place, &key.public)?;
            let port = free_port()?;
            let qemu_log = std::fs::File::create(dir.join("qemu.log")).map_err(|e| e.to_string())?;
            let mut cmd = Command::new(qemu_binary());
            cmd.args(qemu_args(a, &dir, &disk, port, seed.port))
                .stdin(Stdio::null())
                .stdout(qemu_log.try_clone().map_err(|e| e.to_string())?)
                .stderr(qemu_log);
            // QEMU dies with the agent; clean removes it by its pid file otherwise.
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
            let qemu = cmd.spawn().map_err(|e| format!("qemu: {e}"))?;
            let qemu = Qemu::watch(qemu);
            let result = (|| {
                let ssh = dial_vm(ctx, port, &key.path, a.boot_timeout, &qemu.done)
                    .map_err(|e| format!("{e}; the console log ends: {}", tail(&dir.join("console.log"), 400)))?;
                out.say(&format!("virtual machine for place {} is up", a.place));

                let f = std::fs::File::open(&tarball).map_err(|e| e.to_string())?;
                ssh.run(ctx, f, "mkdir -p actions-runner && tar xzf - -C actions-runner")
                    .map_err(|e| format!("copying the runner in: {e}"))?;
                let l = self.launch(a);
                if !l.hook.is_empty() {
                    let f = std::fs::File::open(&a.self_path).map_err(|e| e.to_string())?;
                    ssh.run(ctx, f, &format!("mkdir -p {KVM_GUEST} && cat > {KVM_GUEST}/runner-up && chmod 0755 {KVM_GUEST}/runner-up"))
                        .map_err(|e| format!("copying the fork guard in: {e}"))?;
                }
                let mut cmd = ssh.command(&kvm_script(&l));
                cmd.stdin(Stdio::piped());
                let (mut session, piped) = spawn_into(&mut cmd, out).map_err(|e| e.to_string())?;
                let mut stdin = session.stdin.take().unwrap();
                let line = format!("{jit}\n");
                std::thread::spawn(move || {
                    let _ = stdin.write_all(line.as_bytes());
                });
                let mut stopped = Ok(());
                loop {
                    if !matches!(session.try_wait(), Ok(None)) {
                        break;
                    }
                    if qemu.done.load(Ordering::SeqCst) {
                        stopped = Err("the virtual machine stopped while the runner ran".to_string());
                        break;
                    }
                    if ctx.sleep(Duration::from_millis(100)) {
                        break;
                    }
                }
                let _ = session.kill();
                let _ = session.wait();
                piped.finish(Duration::from_secs(5));
                stopped
            })();
            drop(seed);
            qemu.kill();
            result
        }

        fn launch(&self, a: &AgentRun) -> RunnerLaunch {
            launch(a)
        }

        /// "ok" when this machine can run KVM places, or why not.
        fn ready(&self) -> String {
            if let Err(e) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
                return format!("no usable /dev/kvm: open /dev/kvm: {e}");
            }
            for bin in [qemu_binary(), "qemu-img", "ssh", "ssh-keygen"] {
                if crate::proc::look_path(bin).is_err() {
                    return format!("{bin} is not installed");
                }
            }
            if cfg!(target_arch = "aarch64") {
                if ARM_FIRMWARE.iter().any(|fw| Path::new(fw).exists()) {
                    return "ok".into();
                }
                return "no arm64 UEFI firmware (install qemu-efi-aarch64)".into();
            }
            "ok".into()
        }
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// QEMU running, with a flag set once it has ended.
    struct Qemu {
        pid: i32,
        done: Arc<AtomicBool>,
        reaper: std::thread::JoinHandle<()>,
    }

    impl Qemu {
        fn watch(mut child: Child) -> Qemu {
            let done = Arc::new(AtomicBool::new(false));
            let pid = child.id() as i32;
            let d = done.clone();
            let reaper = std::thread::spawn(move || {
                let _ = child.wait();
                d.store(true, Ordering::SeqCst);
            });
            Qemu { pid, done, reaper }
        }

        fn kill(self) {
            if !self.done.load(Ordering::SeqCst) {
                unsafe { libc::kill(self.pid, libc::SIGKILL) };
            }
            let _ = self.reaper.join();
        }
    }

    /// Makes the place's disk: a copy-on-write layer over the base image.
    fn overlay(ctx: &Ctx, a: &AgentRun, dir: &Path) -> Result<PathBuf, String> {
        let o = crate::proc::output(ctx, Command::new("qemu-img").args(["info", "--output=json", &a.image]), Vec::new());
        o.status.map_err(|e| format!("base image {} unreadable: {e}", a.image))?;
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(default)]
            format: String,
        }
        let format = match serde_json::from_slice::<Info>(&o.stdout) {
            Ok(i) if !i.format.is_empty() => i.format,
            _ => return Err(format!("base image {} has no format qemu-img knows", a.image)),
        };
        let size = if a.disk_gb <= 0 { config::DEFAULT_DISK_GB } else { a.disk_gb };
        let disk = dir.join("disk.qcow2");
        let (b, res) = crate::proc::combined_output(
            ctx,
            Command::new("qemu-img")
                .args(["create", "-q", "-f", "qcow2", "-F", &format, "-b", &a.image])
                .arg(&disk)
                .arg(format!("{size}G")),
        );
        if let Err(e) = res {
            return Err(format!("qemu-img create: {}", first_line(&String::from_utf8_lossy(&b), &e)));
        }
        Ok(disk)
    }

    fn qemu_binary() -> &'static str {
        if cfg!(target_arch = "aarch64") { "qemu-system-aarch64" } else { "qemu-system-x86_64" }
    }

    /// Where distributions put the UEFI firmware an arm64 virtual machine
    /// boots from.
    const ARM_FIRMWARE: [&str; 3] =
        ["/usr/share/qemu-efi-aarch64/QEMU_EFI.fd", "/usr/share/edk2/aarch64/QEMU_EFI.fd", "/usr/share/AAVMF/AAVMF_CODE.fd"];

    pub fn qemu_args(a: &AgentRun, dir: &Path, disk: &Path, ssh_port: u16, seed_port: u16) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-name".into(),
            format!("runner-up-{}", a.place),
            "-nodefaults".into(),
            "-display".into(),
            "none".into(),
            "-pidfile".into(),
            dir.join("qemu.pid").to_string_lossy().into_owned(),
            "-cpu".into(),
            "host".into(),
            "-smp".into(),
            a.cores.to_string(),
            "-m".into(),
            a.memory_mb.to_string(),
            "-drive".into(),
            format!("if=virtio,format=qcow2,file={}", disk.to_string_lossy().replace(',', ",,")),
            "-netdev".into(),
            format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
            "-device".into(),
            "virtio-net-pci,netdev=net0".into(),
            "-serial".into(),
            format!("file:{}", dir.join("console.log").display()),
            // cloud-init reads its seed from the agent: 10.0.2.2 is the
            // machine's loopback address as the virtual machine sees it.
            "-smbios".into(),
            format!("type=1,serial=ds=nocloud;s=http://10.0.2.2:{seed_port}/"),
        ];
        if cfg!(target_arch = "aarch64") {
            args.extend(["-machine".into(), "virt,accel=kvm,gic-version=host".into()]);
            if let Some(fw) = ARM_FIRMWARE.iter().find(|fw| Path::new(fw).exists()) {
                args.extend(["-bios".into(), fw.to_string()]);
            }
        } else {
            args.extend(["-machine".into(), "q35,accel=kvm".into()]);
        }
        args
    }

    /// The job's SSH key: its private half in the VM's folder, which only
    /// the agent's user reads and clean removes, and its public half.
    struct Key {
        path: PathBuf,
        public: String,
    }

    fn make_key(ctx: &Ctx, dir: &Path) -> Result<Key, String> {
        let path = dir.join("ssh-key");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("pub"));
        let (b, res) = crate::proc::combined_output(
            ctx,
            Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "runner-up", "-f"]).arg(&path),
        );
        if let Err(e) = res {
            return Err(format!("ssh-keygen: {}", first_line(&String::from_utf8_lossy(&b), &e)));
        }
        let public = std::fs::read_to_string(path.with_extension("pub")).map_err(|e| e.to_string())?;
        Ok(Key { path, public: public.trim().to_string() })
    }

    /// cloud-init's NoCloud seed served on a loopback port until dropped. It
    /// holds nothing secret: the key it hands out is the public half.
    struct Seed {
        port: u16,
        stop: Arc<AtomicBool>,
    }

    impl Drop for Seed {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wakes the accept.
            let _ = TcpStream::connect(("127.0.0.1", self.port));
        }
    }

    fn serve_seed(place: &str, key: &str) -> Result<Seed, String> {
        let ln = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = ln.local_addr().map_err(|e| e.to_string())?.port();
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let meta = format!("instance-id: runner-up-{place}-{nanos}\nlocal-hostname: runner-up-{place}\n");
        let user = format!(
            "#cloud-config\nusers:\n  - name: runner\n    shell: /bin/bash\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    lock_passwd: true\n    ssh_authorized_keys:\n      - {key}\nssh_pwauth: false\ndisable_root: true\n"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            for conn in ln.incoming() {
                if s.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (meta, user) = (meta.clone(), user.clone());
                std::thread::spawn(move || serve_one(conn, &meta, &user));
            }
        });
        Ok(Seed { port, stop })
    }

    /// Answers one HTTP request for the seed's files.
    fn serve_one(mut conn: TcpStream, meta: &str, user: &str) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
        let mut req = Vec::new();
        let mut b = [0u8; 1024];
        while !contains(&req, b"\r\n\r\n") && req.len() < 16 << 10 {
            match conn.read(&mut b) {
                Ok(0) | Err(_) => return,
                Ok(n) => req.extend_from_slice(&b[..n]),
            }
        }
        let head = String::from_utf8_lossy(&req);
        let path = head.split_whitespace().nth(1).unwrap_or("");
        let path = path.split('?').next().unwrap_or("");
        let (status, body) = match path {
            "/meta-data" => ("200 OK", meta),
            "/user-data" => ("200 OK", user),
            "/vendor-data" => ("200 OK", ""),
            _ => ("404 Not Found", "404 page not found\n"),
        };
        let _ = write!(
            conn,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn free_port() -> Result<u16, String> {
        let ln = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        Ok(ln.local_addr().map_err(|e| e.to_string())?.port())
    }

    /// How the agent reaches the VM: the machine's ssh, with the job's key.
    struct Ssh {
        port: u16,
        key: PathBuf,
    }

    impl Ssh {
        /// The port is QEMU's: QEMU stops at once if it cannot take it, and
        /// every attempt first checks QEMU still runs, so no other program on
        /// the machine can answer in its place. That is why any host key the
        /// machine made at boot is accepted.
        fn command(&self, remote: &str) -> Command {
            let mut c = Command::new("ssh");
            c.args(["-F", "/dev/null", "-i"]).arg(&self.key).args([
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=10",
                "-p",
                &self.port.to_string(),
                "runner@127.0.0.1",
                remote,
            ]);
            c
        }

        /// Runs command with input on its stdin; an error names the first
        /// line it wrote to stderr.
        fn run(&self, ctx: &Ctx, mut input: std::fs::File, command: &str) -> Result<(), String> {
            let mut cmd = self.command(command);
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            let mut stdin = child.stdin.take().unwrap();
            let writer = std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut stdin);
            });
            let mut stderr = child.stderr.take().unwrap();
            let reader = std::thread::spawn(move || {
                let mut s = String::new();
                let _ = stderr.read_to_string(&mut s);
                s
            });
            let status = crate::proc::wait_ctx(ctx, &mut child);
            let _ = writer.join();
            let stderr = reader.join().unwrap_or_default();
            status.map_err(|e| format!("{e}: {}", first_line(&stderr, "")))
        }
    }

    /// Signs in to the virtual machine once it has booted.
    fn dial_vm(ctx: &Ctx, port: u16, key: &Path, timeout: Duration, qemu_done: &AtomicBool) -> Result<Ssh, String> {
        let ssh = Ssh { port, key: key.to_path_buf() };
        let deadline = Instant::now() + timeout;
        loop {
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                if qemu_done.load(Ordering::SeqCst) {
                    return Err("the virtual machine stopped while booting".into());
                }
                if ctx.sleep(Duration::from_millis(100)) {
                    return Err(ctx.err().unwrap_or("context canceled").to_string());
                }
            }
            // Bounds the whole handshake: QEMU accepts the connection before
            // the machine's SSH server is up.
            let c = ctx.with_timeout(Duration::from_secs(10));
            let mut cmd = ssh.command("true");
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            if crate::proc::combined_output(&c, &mut cmd).1.is_ok() {
                return Ok(ssh);
            }
            if Instant::now() > deadline {
                return Err(format!("the virtual machine did not boot within {}", crate::go::duration::format(timeout)));
            }
        }
    }

    /// The last n bytes of a file, on one line.
    fn tail(path: &Path, n: u64) -> String {
        let Ok(mut f) = std::fs::File::open(path) else {
            return "(none)".into();
        };
        if f.metadata().is_ok_and(|m| m.len() > n) {
            let _ = f.seek(SeekFrom::End(-(n as i64)));
        }
        let mut b = Vec::new();
        let _ = f.read_to_end(&mut b);
        String::from_utf8_lossy(&b).split_whitespace().collect::<Vec<_>>().join(" ")
    }
}
