//! Samples a job's use: a host runner's process group, or a whole tart guest.

use super::{Out, USAGE_PREFIX, Usage};
#[cfg(unix)]
pub use unix::*;

pub fn write_usage(out: &Out, u: &Usage) {
    if u.samples == 0 {
        out.line("runner-up agent: resource readings unavailable");
        return;
    }
    out.line(&format!("{USAGE_PREFIX}{}", serde_json::to_string(u).unwrap()));
}

/// The measurement wrapper runs only inside a macOS guest. Windows agents
/// still honor each pool's configured reservation and provider limits.
#[cfg(windows)]
pub fn measure_main(args: &[String]) -> i32 {
    eprintln!("runner-up agent measure runs only inside a macOS guest");
    if crate::cli::help(args) { 0 } else { 2 }
}

/// One reading: the memory in use now and, once the reader can tell, the
/// cores used since its previous reading.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub cores: Option<f64>,
    pub memory_mb: f64,
}

pub type Reader = Box<dyn FnMut(&crate::ctx::Ctx) -> Result<Sample, String> + Send>;

/// A measurement running on its own thread until finish stops it.
pub struct Measure {
    ctx: crate::ctx::Ctx,
    done: std::sync::mpsc::Receiver<Usage>,
}

impl Measure {
    pub fn finish(self) -> Usage {
        self.ctx.cancel();
        self.done.recv().unwrap()
    }
}

/// Keeps the peaks of read's samples, taken once a second until finish stops
/// it.
pub fn measure_with(scope: &str, mut read: Reader) -> Measure {
    let ctx = crate::ctx::Ctx::background().with_cancel();
    let (tx, done) = std::sync::mpsc::channel();
    let c = ctx.clone();
    let scope = scope.to_string();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut u = Usage { scope, ..Default::default() };
        let mut take = |u: &mut Usage| match read(&c) {
            Err(_) => {
                if !c.is_done() {
                    u.incomplete = true;
                }
            }
            Ok(s) => {
                if let Some(cores) = s.cores {
                    u.peak_cores = u.peak_cores.max(cores);
                }
                u.peak_memory_mb = u.peak_memory_mb.max(s.memory_mb);
                u.samples += 1;
            }
        };
        take(&mut u);
        let mut next = start + std::time::Duration::from_secs(1);
        loop {
            let wait = next.saturating_duration_since(std::time::Instant::now());
            if c.sleep(wait) {
                break;
            }
            // A Go ticker drops the ticks a slow reading missed.
            next = (next + std::time::Duration::from_secs(1)).max(std::time::Instant::now());
            take(&mut u);
        }
        u.seconds = start.elapsed().as_secs_f64();
        if u.samples < 2 {
            u.incomplete = true;
        }
        let _ = tx.send(u);
    });
    Measure { ctx, done }
}

#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Measure, Reader, Sample, measure_with, write_usage};
    use crate::agent::launch::REGISTRATION_VAR;
    use crate::agent::{Out, wait_or_stop};
    use crate::ctx::Ctx;
    use crate::go::flag::FlagSet;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct ProcessUse {
        pub pid: i64,
        pub group: i64,
        pub cpu: f64,
        pub rss: f64,
    }

    /// Reads numbers only, never another process's arguments or environment.
    /// ps is available on macOS and Linux hosts.
    pub fn read_process_use(ctx: &Ctx) -> Result<Vec<ProcessUse>, String> {
        read_process_use_with(ctx, "ps")
    }

    pub fn read_process_use_with(ctx: &Ctx, ps: &str) -> Result<Vec<ProcessUse>, String> {
        let c = ctx.with_timeout(Duration::from_secs(2));
        let o = crate::proc::output(&c, Command::new(ps).args(["-axo", "pid=,pgid=,time=,rss="]), Vec::new());
        o.status?;
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() != 4 {
                continue;
            }
            let (Ok(pid), Ok(group), Ok(cpu), Ok(rss)) =
                (f[0].parse::<i64>(), f[1].parse::<i64>(), cpu_seconds(f[2]), f[3].parse::<f64>())
            else {
                return Err("unreadable process usage".into());
            };
            out.push(ProcessUse { pid, group, cpu, rss: rss / 1024.0 });
        }
        if out.is_empty() {
            return Err("no process readings".into());
        }
        Ok(out)
    }

    pub fn cpu_seconds(s: &str) -> Result<f64, String> {
        let bad = |_| format!("bad cpu time {s:?}");
        let (days, rest) = match s.split_once('-') {
            Some((d, rest)) => (d.parse::<i64>().map_err(bad)?, rest),
            None => (0, s),
        };
        let mut seconds = 0.0;
        for part in rest.split(':') {
            seconds = seconds * 60.0 + part.parse::<f64>().map_err(|_| format!("bad cpu time {s:?}"))?;
        }
        Ok(days as f64 * 86400.0 + seconds)
    }

    /// Samples one host runner's process group. It stops before the caller
    /// releases the place. A process that exits between samples, or escapes
    /// the group, can be missed: the report deliberately calls these sampled
    /// peaks.
    pub fn measure_group(group: i64) -> Measure {
        measure_with("process-group", group_reader(group, Box::new(read_process_use)))
    }

    pub type ProcessReader = Box<dyn FnMut(&Ctx) -> Result<Vec<ProcessUse>, String> + Send>;

    /// Adds up the group's processes: the CPU time they used since the
    /// previous reading over the time between the two, and their RSS.
    pub fn group_reader(group: i64, mut read: ProcessReader) -> Reader {
        let mut previous: Option<HashMap<i64, f64>> = None;
        let mut last = Instant::now();
        Box::new(move |ctx| {
            let rows = read(ctx)?;
            let now = Instant::now();
            let mut current = HashMap::new();
            let mut s = Sample::default();
            let mut cpu = 0.0;
            for p in rows.iter().filter(|p| p.group == group) {
                current.insert(p.pid, p.cpu);
                s.memory_mb += p.rss;
                if let Some(old) = previous.as_ref().and_then(|m| m.get(&p.pid))
                    && p.cpu >= *old
                {
                    cpu += p.cpu - old;
                }
            }
            let elapsed = now.duration_since(last).as_secs_f64();
            if previous.is_some() && elapsed > 0.0 {
                s.cores = Some(cpu / elapsed);
            }
            previous = Some(current);
            last = now;
            Ok(s)
        })
    }

    /// Samples the whole macOS guest a tart job runs in, until finish stops it.
    pub fn measure_guest() -> Measure {
        measure_with("guest", guest::guest_reader())
    }

    /// Runs inside a disposable tart guest. The whole guest, including
    /// simulators started by launchd, belongs to this job, so it reads the
    /// guest rather than the runner's processes. The wrapper takes the
    /// registration on stdin and gives only the runner its required input
    /// variable; the wrapper's environment never holds it.
    pub fn measure_main(args: &[String]) -> i32 {
        let mut fs = FlagSet::new("agent measure");
        if let Some(code) = crate::cli::parse(&mut fs, args, true) {
            return code;
        }
        let Some((program, rest)) = fs.args().split_first() else {
            eprintln!("usage: runner-up agent measure -- COMMAND");
            return 2;
        };
        let mut input = std::io::stdin().lock();
        let mut line = String::new();
        let _ = input.read_line(&mut line);
        let registration = line.trim();
        if registration.is_empty() {
            eprintln!("no runner registration on stdin");
            return 1;
        }
        let ctx = crate::signals::notify_stop(&Ctx::background());
        let mut child = match Command::new(program)
            .args(rest)
            .env(REGISTRATION_VAR, registration)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        // The rest of stdin, with what the line read buffered, is the runner's.
        let mut to_runner = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_runner);
        });
        drop(input);
        let measure = measure_guest();
        let pid = child.id();
        let status = wait_or_stop(&ctx, &mut child, &|| {
            let _ = crate::proc::terminate(pid);
        }, Duration::from_secs(30));
        write_usage(&Out::new(Box::new(std::io::stdout())), &measure.finish());
        let code = crate::agent::exit_code(&status);
        if code < 0 { 1 } else { code }
    }

    pub mod guest {
        use super::super::{Reader, Sample};
        use crate::ctx::Ctx;

        /// Runs one of macOS's reading tools and returns its output.
        pub type Tool = Box<dyn FnMut(&Ctx, &str, &[&str]) -> Result<String, String> + Send>;

        /// Reads the whole macOS guest from its kernel's own counters rather
        /// than adding up its processes. Adding them up reads a guest above
        /// its size: one ps that returned late shrank the time the CPU it
        /// read was divided by, and pages shared by many processes counted
        /// once per process.
        ///
        /// CPU is the busy share of all the guest's CPUs over one second,
        /// which iostat works out from the kernel's CPU ticks, so a reading
        /// that returns late cannot raise it. Memory is what the guest has in
        /// use, as Activity Monitor counts it, so a shared page counts once.
        /// Neither can exceed the guest's own CPUs and memory; a reading that
        /// would is refused. The tools are macOS's own, in the guest image at
        /// fixed paths.
        #[cfg(target_os = "macos")]
        pub fn guest_reader() -> Reader {
            guest_reader_with(Box::new(guest_tool))
        }

        /// Guest readings come from macOS's own tools: only tart guests are
        /// read.
        #[cfg(not(target_os = "macos"))]
        pub fn guest_reader() -> Reader {
            Box::new(|_| Err("guest readings need macOS".into()))
        }

        pub fn guest_reader_with(mut run: Tool) -> Reader {
            let mut cpus = 0i64;
            let mut memory_mb = 0.0;
            Box::new(move |ctx| {
                if cpus == 0 {
                    (cpus, memory_mb) = guest_size(ctx, &mut run)?;
                }
                let busy = guest_busy(ctx, &mut run)?;
                let used = guest_memory_mb(ctx, &mut run)?;
                if used > memory_mb {
                    return Err(format!("guest memory reading above the guest's {memory_mb:.0} MB"));
                }
                Ok(Sample { cores: Some(busy * cpus as f64), memory_mb: used })
            })
        }

        /// Reads the guest's logical CPUs and memory. tart sets both to the
        /// job's reservation.
        pub fn guest_size(ctx: &Ctx, run: &mut Tool) -> Result<(i64, f64), String> {
            let b = run(ctx, "/usr/sbin/sysctl", &["-n", "hw.logicalcpu", "hw.memsize"])?;
            let f: Vec<&str> = b.split_whitespace().collect();
            let bad = || "unreadable guest size".to_string();
            if f.len() != 2 {
                return Err(bad());
            }
            let (Ok(cpus), Ok(memory)) = (f[0].parse::<i64>(), f[1].parse::<f64>()) else {
                return Err(bad());
            };
            if cpus <= 0 || memory <= 0.0 {
                return Err(bad());
            }
            Ok((cpus, memory / (1u64 << 20) as f64))
        }

        /// The share of the guest's CPU time that was not idle over one
        /// second. iostat's first line covers the time since boot; its second,
        /// the interval.
        fn guest_busy(ctx: &Ctx, run: &mut Tool) -> Result<f64, String> {
            let b = run(ctx, "/usr/sbin/iostat", &["-n", "0", "-c", "2", "-w", "1"])?;
            let mut idle = Vec::new();
            for line in b.split('\n') {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 3 {
                    continue;
                }
                let nums: Vec<Option<f64>> = f[..3].iter().map(|s| s.parse().ok()).collect();
                if let [Some(_), Some(_), Some(i)] = nums[..] {
                    idle.push(i);
                }
            }
            if idle.len() != 2 || idle[1] < 0.0 || idle[1] > 100.0 {
                return Err("unreadable guest CPU reading".into());
            }
            Ok((100.0 - idle[1]) / 100.0)
        }

        /// The guest's memory in use from vm_stat: app memory (anonymous
        /// pages less purgeable ones), wired pages and the pages the
        /// compressor occupies, as Activity Monitor's Memory Used counts them.
        /// File cache the guest can drop is not in use.
        fn guest_memory_mb(ctx: &Ctx, run: &mut Tool) -> Result<f64, String> {
            let b = run(ctx, "/usr/bin/vm_stat", &[])?;
            let lines: Vec<&str> = b.split('\n').collect();
            let mut page_size = 0.0;
            if let Some((_, after)) = lines[0].split_once("page size of ") {
                page_size = trim_suffix(after.trim(), " bytes)").parse().unwrap_or(0.0);
            }
            let mut pages = std::collections::HashMap::new();
            for line in &lines[1..] {
                let Some((name, value)) = line.split_once(':') else { continue };
                if let Ok(n) = trim_suffix(value.trim(), ".").parse::<f64>() {
                    pages.insert(name.trim(), n);
                }
            }
            const ANONYMOUS: &str = "Anonymous pages";
            const PURGEABLE: &str = "Pages purgeable";
            const WIRED: &str = "Pages wired down";
            const COMPRESSOR: &str = "Pages occupied by compressor";
            for name in [ANONYMOUS, PURGEABLE, WIRED, COMPRESSOR] {
                if !pages.contains_key(name) || page_size <= 0.0 {
                    return Err("unreadable guest memory reading".into());
                }
            }
            let app = (pages[ANONYMOUS] - pages[PURGEABLE]).max(0.0);
            Ok((app + pages[WIRED] + pages[COMPRESSOR]) * page_size / (1u64 << 20) as f64)
        }

        fn trim_suffix<'a>(s: &'a str, suffix: &str) -> &'a str {
            s.strip_suffix(suffix).unwrap_or(s)
        }

        /// Runs one of macOS's reading tools. Its readings stay true however
        /// late it returns, so it may take a while in a busy guest.
        #[cfg(target_os = "macos")]
        pub fn guest_tool(ctx: &Ctx, name: &str, args: &[&str]) -> Result<String, String> {
            let c = ctx.with_timeout(std::time::Duration::from_secs(10));
            let o = crate::proc::output(&c, std::process::Command::new(name).args(args), Vec::new());
            o.status?;
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        }
    }
}
