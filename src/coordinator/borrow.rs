// The caller owns a request lock while waiting and a private socket while
// running. The coordinator starts a detached supervisor with busy.lock, just
// as it does for a job. Command arguments, environment and stdio go directly
// from caller to supervisor, never into queue files or logs. A coordinator
// restart therefore neither loses the reservation nor stops the command.

#[cfg(unix)]
pub use unix::{borrow_main, run_borrow};

#[cfg(windows)]
mod windows {
    use std::fs::File;

    use crate::config::{Config, Place};
    use crate::coordinator::{Service, State};
    use crate::plan::Job;

    pub fn borrow_main(_: &[String]) -> i32 {
        2
    }

    pub fn run_borrow(_: &Config, _: &str, _: &str) -> i32 {
        2
    }

    impl State {
        pub fn refresh_borrows(&mut self) {}
    }

    impl Service {
        pub(in crate::coordinator) fn assign_borrow(&self, _: &mut State, _: &Place, _: &Job, _: &File) -> bool {
            false
        }
    }
}

#[cfg(windows)]
pub use windows::{borrow_main, run_borrow};

#[cfg(unix)]
pub(crate) mod unix {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use serde::{Deserialize, Serialize};

    use crate::config::{self, Config, PROVIDER_TART, Place};
    use crate::coordinator::place::{PlaceRun, agent_command, reserved_place, write_place_run};
    use crate::coordinator::request::{open_request, request_held, scan_requests};
    use crate::coordinator::snapshot::{Snapshot, share};
    use crate::coordinator::{Service, State};
    use crate::ctx::Ctx;
    use crate::go::flag::FlagSet;
    use crate::go::quote;
    use crate::go::time::{Time, round_secs};
    use crate::plan::{self, Job};
    use crate::{agent, cli, lock, proc, signals};

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub(crate) struct BorrowRequest {
        #[serde(default)]
        pub(crate) id: String,
        #[serde(default)]
        pub(crate) pool: String,
        #[serde(default)]
        pub(crate) label: String,
        #[serde(default)]
        pub(crate) minutes: i64,
        #[serde(default)]
        pub(crate) queued: Time,
        #[serde(default)]
        socket: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        place: String,
        #[serde(default)]
        granted: Time,
        #[serde(default)]
        position: usize,
        #[serde(default)]
        reason: String,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    pub(crate) struct BorrowCommand {
        #[serde(default, deserialize_with = "config::null_as_default")]
        pub(crate) args: Vec<String>,
        #[serde(default, deserialize_with = "config::null_as_default")]
        pub(crate) env: Vec<String>,
        #[serde(default)]
        pub(crate) dir: String,
    }

    fn borrows_dir(cfg: &Config) -> PathBuf {
        Path::new(&cfg.state_dir).join("borrows")
    }

    pub(crate) fn borrow_dir(cfg: &Config, id: &str) -> PathBuf {
        borrows_dir(cfg).join(id)
    }

    fn valid_borrow_id(id: &str) -> bool {
        !id.is_empty() && id != "." && id != ".." && !id.contains('/')
    }

    pub(crate) fn borrow_limit(minutes: i64) -> Duration {
        Duration::from_secs(3600).max(Duration::from_secs(minutes as u64 * 60))
    }

    /// Runs a command with a tart place reserved for its VM. It runs on the
    /// coordinator's computer; commands addressing a remote Mac must reach it
    /// themselves, as they would without a borrow.
    pub fn borrow_main(args: &[String]) -> i32 {
        let mut fs = FlagSet::new("borrow");
        fs.string("config", &config::default_config_path(), "setup file");
        fs.string("pool", "", "pool to borrow a tart place from");
        fs.string("label", "", "name shown in the queue and status");
        fs.int("minutes", 0, "declared runtime in minutes");
        if let Some(code) = cli::parse(&mut fs, args, true) {
            return code;
        }
        let (pool, label, minutes) = (fs.get_str("pool"), fs.get_str("label"), fs.get_int("minutes"));
        if pool.is_empty() || label.is_empty() || minutes <= 0 || minutes > i64::MAX / 60_000_000_000 || fs.args().is_empty() {
            eprintln!("borrow needs --pool LABEL --label TEXT --minutes N -- COMMAND");
            return 2;
        }
        let cfg = match config::load_config(&fs.get_str("config")) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        if !cfg.places().iter().any(|p| p.provider == PROVIDER_TART && p.pools.contains(&pool)) {
            eprintln!("pool has no tart place");
            return 2;
        }
        let ctx = signals::notify_stop(&Ctx::background());
        match borrow(&ctx, &cfg, &pool, &label, minutes, fs.args()) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("borrow: {e}");
                1
            }
        }
    }

    fn borrow(ctx: &Ctx, cfg: &Config, pool: &str, label: &str, minutes: i64, args: &[String]) -> Result<i32, String> {
        // Resolve with the caller's PATH, not the coordinator service's PATH.
        let executable = std::path::absolute(proc::look_path(&args[0])?).map_err(|e| e.to_string())?;
        let mut args = args.to_vec();
        args[0] = executable.to_string_lossy().into_owned();
        let mut req = open_request(&borrows_dir(cfg), "request-")?;
        // macOS limits Unix socket addresses to 104 bytes; the state folder can
        // be much longer. This private temporary folder holds only the socket.
        let sockets = std::env::temp_dir().join(format!("ru-b-{}", config::random_digits()));
        let res = (|| {
            std::fs::DirBuilder::new().mode(0o700).create(&sockets).map_err(|e| e.to_string())?;
            let socket = sockets.join("s");
            let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
            let r = BorrowRequest {
                id: req.id(),
                pool: pool.into(),
                label: label.into(),
                minutes,
                queued: Time::now(),
                socket: socket.to_string_lossy().into_owned(),
                ..Default::default()
            };
            req.publish(&r)?;
            serve_borrow(ctx, cfg, &req.dir, &listener, args)
        })();
        let _ = std::fs::remove_dir_all(&sockets);
        req.close();
        res
    }

    use std::os::unix::fs::DirBuilderExt;

    /// Waits for the supervisor's connection, saying where the request
    /// stands, then hands it the caller's stdio and command and waits for its
    /// exit code.
    fn serve_borrow(ctx: &Ctx, cfg: &Config, dir: &Path, listener: &UnixListener, args: Vec<String>) -> Result<i32, String> {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut last = String::new();
        let conn = loop {
            match listener.accept() {
                Ok((c, _)) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
            if let Ok(granted) = config::read_json::<BorrowRequest>(&dir.join("granted.json"))
                && !granted.place.is_empty()
                && lock::held_by(&cfg.place_dir(&granted.place).join("busy.lock")).is_ok_and(|held| !held)
            {
                return Err("supervisor ended before connecting".into());
            }
            let mut message = "waiting for coordinator".to_string();
            if let Ok(current) = config::read_json::<BorrowRequest>(&dir.join("request.json"))
                && current.position > 0
            {
                message = format!("waiting at position {}: {}", current.position, current.reason);
            }
            if message != last {
                eprintln!("borrow: {message}");
                last = message;
            }
            if ctx.sleep(Duration::from_millis(200)) {
                return Ok(130);
            }
        };
        conn.set_nonblocking(false).map_err(|e| e.to_string())?;
        let done = ctx.with_cancel();
        {
            let (done, ctx, conn) = (done.clone(), ctx.clone(), conn.try_clone().map_err(|e| e.to_string())?);
            std::thread::spawn(move || {
                done.wait();
                if ctx.is_done() {
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                }
            });
        }
        let res = hand_over(ctx, conn, args);
        done.cancel();
        res
    }

    fn hand_over(ctx: &Ctx, mut conn: UnixStream, args: Vec<String>) -> Result<i32, String> {
        // Send exactly one byte with the three descriptors, then the command
        // on the same stream. No command or environment is persisted.
        send_fds(&conn, &[0, 1, 2]).map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let env = std::env::vars_os().map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy())).collect();
        let command = BorrowCommand { args, env, dir: cwd.to_string_lossy().into_owned() };
        let mut line = serde_json::to_vec(&command).unwrap();
        line.push(b'\n');
        conn.write_all(&line).map_err(|e| e.to_string())?;
        let mut answer = String::new();
        let read = conn.read_to_string(&mut answer);
        match read.map_err(|e| e.to_string()).and_then(|_| answer.trim().parse::<i32>().map_err(|e| e.to_string())) {
            Ok(code) => Ok(code),
            Err(_) if ctx.is_done() => Ok(130),
            Err(e) => Err(format!("supervisor disconnected: {e}")),
        }
    }

    pub(crate) fn send_fds(conn: &UnixStream, fds: &[RawFd]) -> std::io::Result<()> {
        let byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_ptr() as *mut _, iov_len: 1 };
        let size = std::mem::size_of_val(fds) as u32;
        // SAFETY: CMSG_SPACE only computes a size.
        let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(size) } as usize];
        // SAFETY: msg points at buffers that outlive the sendmsg call, and the
        // control header is written inside control, sized by CMSG_SPACE.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len() as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
            if libc::sendmsg(conn.as_raw_fd(), &msg, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Receives one byte with exactly three descriptors, owned by the
    /// returned files and closed on exec.
    fn receive_fds(conn: &UnixStream) -> Option<[File; 3]> {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        // SAFETY: CMSG_SPACE only computes a size.
        let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(3 * 4) } as usize];
        let mut fds = Vec::new();
        // SAFETY: msg points at buffers that outlive the recvmsg call; each
        // control message is read within the length recvmsg reported.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len() as _;
            if libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) < 0 {
                return None;
            }
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let n = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..n {
                        fds.push(std::ptr::read_unaligned(data.add(i)));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            let truncated = msg.msg_flags & libc::MSG_CTRUNC != 0;
            let files: Vec<File> = fds.into_iter().map(|fd| File::from_raw_fd(fd)).collect();
            if truncated || files.len() != 3 {
                return None;
            }
            for f in &files {
                libc::fcntl(f.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            files.try_into().ok()
        }
    }

    impl State {
        pub fn refresh_borrows(&mut self) {
            // Local requests are reread rather than saved as GitHub jobs.
            // Their live locks discard abandoned requests even when the
            // coordinator was stopped.
            self.waiting.retain(|_, j| j.borrow_id.is_empty());
            // A granted request has left the queue, but its folder is removed
            // only once its caller lets go.
            let (live, gone) = scan_requests(&borrows_dir(&self.cfg), &["request.json", "granted.json"]);
            for id in gone {
                let _ = std::fs::remove_dir_all(borrow_dir(&self.cfg, &id));
            }
            for id in live {
                let Ok(r) = config::read_json::<BorrowRequest>(&borrow_dir(&self.cfg, &id).join("request.json")) else {
                    continue;
                };
                if r.id != id || r.minutes <= 0 {
                    continue;
                }
                let j = Job {
                    borrow_id: r.id,
                    target: "borrow".into(),
                    pool: r.pool,
                    name: r.label,
                    queue_time: r.queued,
                    seen_at: r.queued,
                    estimate: Duration::from_secs(r.minutes as u64 * 60),
                    ..Default::default()
                };
                self.waiting.insert(j.key(), j);
            }
            // Rank against the very same job views and rules, not a separate
            // FIFO queue.
            let ranked = plan::rank(&self.waiting_jobs(), &self.rules, self.now());
            for (i, j) in ranked.iter().enumerate() {
                if j.borrow_id.is_empty() {
                    continue;
                }
                let path = borrow_dir(&self.cfg, &j.borrow_id).join("request.json");
                let Ok(mut r) = config::read_json::<BorrowRequest>(&path) else {
                    continue;
                };
                r.position = i + 1;
                r.reason = "waiting behind higher-ranked work".into();
                for p in self.cfg.places() {
                    if p.provider != PROVIDER_TART || !p.pools.contains(&j.pool) {
                        continue;
                    }
                    if let Some(d) = self.drain_of(&p.id) {
                        r.reason = format!("{}: drained for {}", p.id, quote(&d.req.label));
                        continue;
                    }
                    if let (true, Some(why)) = Snapshot::default().busy(self, &p, true) {
                        r.reason = format!("{}: {}", p.id, why.0);
                        continue;
                    }
                    r.reason = "waiting for placement".into();
                    break;
                }
                let _ = config::write_json(&path, &r);
            }
        }
    }

    impl Service {
        pub(in crate::coordinator) fn assign_borrow(&self, st: &mut State, p: &Place, j: &Job, busy: &File) -> bool {
            let dir = borrow_dir(&st.cfg, &j.borrow_id);
            if !request_held(&dir).unwrap_or(false) {
                st.waiting.remove(&j.key());
                return true;
            }
            let Ok(mut request) = config::read_json::<BorrowRequest>(&dir.join("request.json")) else {
                return false;
            };
            request.place = p.id.clone();
            request.granted = st.now();
            if config::write_json(&dir.join("request.json"), &request).is_err() {
                return false;
            }
            // Rename before launching: a restarted coordinator must not grant
            // the same request to a second place while the detached
            // supervisor connects.
            if std::fs::rename(dir.join("request.json"), dir.join("granted.json")).is_err() {
                return false;
            }
            let run = PlaceRun {
                borrow_id: j.borrow_id.clone(),
                pool: j.pool.clone(),
                job_name: j.name.clone(),
                started: true,
                launched_at: st.now(),
                reservation: share(p),
                ..Default::default()
            };
            let launched = write_place_run(&st.cfg, &p.id, &run).and_then(|_| self.launcher.launch_borrow(p, busy, &j.borrow_id));
            if let Err(e) = launched {
                let _ = std::fs::rename(dir.join("granted.json"), dir.join("request.json"));
                st.log.error("borrow launch failed", &[("place", &p.id), ("err", &e)]);
                let until = st.now().add(Duration::from_secs(60));
                st.cooling.insert(p.id.clone(), until);
                return false;
            }
            st.waiting.remove(&j.key());
            st.log.info("borrow placed", &[("place", &p.id), ("label", &j.name), ("waited", &round_secs(st.now().sub(j.queue_time)))]);
            true
        }
    }

    /// Holds the place while the command and cleanup run. Only the
    /// coordinator launches it, with busy.lock as descriptor 3.
    pub fn run_borrow(cfg: &Config, place_id: &str, id: &str) -> i32 {
        if !valid_borrow_id(id) {
            return 1;
        }
        let Some(_busy) = proc::take_fd(3) else {
            return 1;
        };
        let Some(p) = cfg.place(place_id).filter(|p| p.provider == PROVIDER_TART) else {
            return 1;
        };
        let Ok(r) = config::read_json::<BorrowRequest>(&borrow_dir(cfg, id).join("granted.json")) else {
            return 1;
        };
        let p = reserved_place(cfg, &p, &r.pool);
        let Ok(mut conn) = UnixStream::connect(&r.socket) else {
            return 1;
        };
        let mut started = r.granted;
        if started.is_zero() {
            started = Time::now();
        }
        let code = supervise_borrow(cfg, &p, &r, &conn, started);
        // Printed on every way out, after cleanup.
        let minutes = Time::now().sub(started).num_milliseconds() as f64 / 60_000.0;
        println!("borrow completed place={} label={} borrowedMinutes={minutes:.2}", p.id, quote(&r.label));
        let _ = conn.write_all(format!("{code}\n").as_bytes());
        code
    }

    fn supervise_borrow(cfg: &Config, p: &Place, r: &BorrowRequest, conn: &UnixStream, started: Time) -> i32 {
        // Cleanup happens before busy.lock is released, including on
        // cancellation. A supervisor killed outright is covered by the next
        // job's agent cleanup.
        let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
        let Some([input, out, stderr]) = receive_fds(conn) else {
            return 1;
        };
        let mut line = Vec::new();
        let Ok(reader) = conn.try_clone() else { return 1 };
        if BufReader::new(reader.take(4 << 20)).read_until(b'\n', &mut line).is_err() {
            return 1;
        }
        let command: BorrowCommand = match serde_json::from_slice(&line) {
            Ok(c) => c,
            Err(_) => return 1,
        };
        if command.args.is_empty() {
            return 1;
        }
        let _ = conn.set_read_timeout(None);
        let stop = signals::notify_stop(&Ctx::background());
        let left = started.add(borrow_limit(r.minutes)).sub(Time::now()).to_std().unwrap_or_default();
        let ctx = stop.with_timeout(left);
        {
            // The caller closing its end stops the command.
            let (ctx, mut conn) = (ctx.clone(), conn.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = conn.read(&mut [0u8; 1]);
                ctx.cancel();
            });
        }
        let clean = || {
            let c = Ctx::background().with_timeout(Duration::from_secs(60));
            let args: Vec<String> = ["agent", "clean", "--provider", PROVIDER_TART, "--place", &p.id].map(String::from).into();
            let argv = agent_command(&cfg.machine(&p.machine), &args);
            let (b, res) = proc::combined_output(&c, Command::new(&argv[0]).args(&argv[1..]));
            if let Err(e) = &res {
                let _ = writeln!(&stderr, "borrow cleanup: {}", agent::first_line(&String::from_utf8_lossy(&b), e));
            }
            res.is_ok()
        };
        if !clean() {
            return 1;
        }
        let limit = crate::go::duration::format(borrow_limit(r.minutes));
        let _ = writeln!(
            &stderr,
            "borrow: granted {} ({} cores, {} MB), VM {}; limit {limit}",
            p.id,
            p.cores,
            p.memory_mb,
            agent::clone_name(&p.id)
        );
        let code = run_borrow_command(&ctx, p, command, input, out, &stderr);
        if !clean() {
            return 1;
        }
        code
    }

    fn stop_code(ctx: &Ctx) -> i32 {
        if ctx.err() == Some("context deadline exceeded") { 124 } else { 130 }
    }

    /// Owns a process group so cancellation, deadlines and stray children
    /// cannot carry on using a place after its command finishes.
    pub(crate) fn run_borrow_command(ctx: &Ctx, p: &Place, command: BorrowCommand, input: File, out: File, stderr: &File) -> i32 {
        if let Some(e) = ctx.err() {
            let _ = writeln!(&*stderr, "borrow: stopping command: {e}");
            return stop_code(ctx);
        }
        let mut cmd = Command::new(&command.args[0]);
        cmd.args(&command.args[1..]).env_clear();
        if !command.dir.is_empty() {
            cmd.current_dir(&command.dir);
        }
        for kv in &command.env {
            if let Some((k, v)) = kv.split_once('=') {
                cmd.env(k, v);
            }
        }
        cmd.env("BORROWED_PLACE", &p.id)
            .env("BORROWED_VM", agent::clone_name(&p.id))
            .env("BORROWED_CPUS", p.cores.to_string())
            .env("BORROWED_MEMORY_MB", p.memory_mb.to_string());
        let Ok(err) = stderr.try_clone() else { return 1 };
        cmd.stdin(Stdio::from(input)).stdout(Stdio::from(out)).stderr(Stdio::from(err)).process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(&*stderr, "borrow: {e}");
                return 1;
            }
        };
        let group = -(child.id() as i32);
        let code = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            }
            if ctx.is_done() {
                let _ = writeln!(&*stderr, "borrow: stopping command: {}", ctx.err().unwrap_or("context canceled"));
                // SAFETY: kill only signals the command's own process group.
                unsafe { libc::kill(group, libc::SIGTERM) };
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                // SAFETY: as above.
                unsafe { libc::kill(group, libc::SIGKILL) };
                let _ = child.wait();
                break stop_code(ctx);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // A command may leave descendants behind; they must not keep using
        // the borrowed place after its leader has exited.
        // SAFETY: as above.
        unsafe { libc::kill(group, libc::SIGKILL) };
        code
    }
}
