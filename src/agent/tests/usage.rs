use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::Buf;
use crate::agent::usage_measure::{ProcessUse, group_reader, measure_with, read_process_use, read_process_use_with, write_usage};
use crate::agent::{Out, USAGE_PREFIX, Usage, parse_usage};
use crate::ctx::Ctx;

/// The job the usage tests measure: this test program started again with
/// RUNNER_UP_USAGE_WORKLOAD=1. It uses one CPU and touches 32 MiB until its
/// input closes, so the readings come from actual work, not a fabricated ps
/// fixture or copied limits. Run any other way it does nothing.
#[test]
fn usage_workload() {
    if std::env::var("RUNNER_UP_USAGE_WORKLOAD").as_deref() != Ok("1") {
        return;
    }
    let mut memory = vec![0u8; 32 << 20];
    for (i, b) in memory.iter_mut().enumerate() {
        *b = i as u8;
    }
    let (tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        let _ = tx.send(());
    });
    let mut out = std::io::stdout();
    let _ = writeln!(out, "ready");
    let _ = out.flush();
    while done.try_recv().is_err() {
        for b in memory.iter_mut() {
            *b = b.wrapping_add(1);
        }
        std::hint::black_box(&memory);
    }
    std::process::exit(0);
}

/// A running workload; dropping it kills the workload.
pub struct Workload {
    child: std::process::Child,
    out: Option<BufReader<std::process::ChildStdout>>,
}

impl Workload {
    /// Starts the workload in its own process group when group is set, and
    /// waits for it to say it is ready.
    pub fn start(group: bool) -> Workload {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "agent::tests::usage::usage_workload", "--nocapture", "--test-threads=1"])
            .env("RUNNER_UP_USAGE_WORKLOAD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if group {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, ready) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = out;
            let mut line = String::new();
            while out.read_line(&mut line).is_ok_and(|n| n > 0) {
                if line.trim_end().ends_with("ready") {
                    let _ = tx.send(out);
                    return;
                }
                line.clear();
            }
        });
        let out = ready.recv_timeout(Duration::from_secs(30)).expect("the workload did not start");
        Workload { child, out: Some(out) }
    }

    pub fn pid(&self) -> i64 {
        self.child.id() as i64
    }

    /// Closes the workload's input and waits, for at most 30 seconds, for
    /// it to exit.
    pub fn stop(mut self) {
        drop(self.child.stdin.take());
        let mut out = self.out.take().unwrap();
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        let status = super::wait_timeout(&mut self.child, Duration::from_secs(30)).expect("the workload did not exit");
        assert!(status.success(), "workload: {status}");
    }
}

impl Drop for Workload {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The usage line write_usage says for u.
pub fn usage_line(u: &Usage) -> String {
    let buf = Buf::default();
    write_usage(&Out::new(Box::new(buf.clone())), u);
    buf.text()
}

/// Waits, for at most limit each, for n readings from sampled.
pub fn readings(sampled: &mpsc::Receiver<()>, n: usize, limit: Duration) {
    for _ in 0..n {
        sampled.recv_timeout(limit).expect("actual readings did not arrive");
    }
}

// Intent (README, "Job sizes and readings"): sizes must be informed by
// readings of actual work, not copied limits. The sampler reads this job's
// group, captures nonzero CPU and resident memory, and emits a record the
// coordinator can consume. Stopping readings completes without leaving a
// sampler behind.
#[test]
fn usage_observes_actual_job_processes() {
    let work = Workload::start(true);
    let (tx, sampled) = mpsc::channel();
    let measure = measure_with(
        "process-group",
        group_reader(
            work.pid(),
            Box::new(move |ctx: &Ctx| {
                let rows = read_process_use(ctx);
                if rows.is_ok() {
                    let _ = tx.send(());
                }
                rows
            }),
        ),
    );
    readings(&sampled, 3, Duration::from_secs(15));
    let u = measure.finish();
    work.stop();
    assert!(
        u.samples >= 2 && u.peak_cores > 0.0 && u.peak_memory_mb >= 32.0 && u.scope == "process-group" && !u.incomplete,
        "readings: {u:?}"
    );
    let line = usage_line(&u);
    let parsed = parse_usage(line.trim_start_matches(USAGE_PREFIX).trim());
    assert_eq!(parsed.as_ref(), Some(&u), "round-trip readings: {line}");
}

// Intent (README, "Job sizes and readings"): absent and too-short readings must
// not look like measured zero use. Exercise the actual reader with ps absent,
// then with one real reading; the barrier models a job ending after that read.
#[test]
fn usage_reports_unavailable_and_short_readings() {
    let group = unsafe { libc::getpgid(0) } as i64;
    for unavailable in [true, false] {
        let (tx, read) = mpsc::channel();
        let measure = measure_with(
            "process-group",
            group_reader(
                group,
                Box::new(move |ctx: &Ctx| -> Result<Vec<ProcessUse>, String> {
                    let rows = if unavailable { read_process_use_with(ctx, "/nonexistent/ps") } else { read_process_use(ctx) };
                    let _ = tx.send(());
                    ctx.wait();
                    rows
                }),
            ),
        );
        read.recv_timeout(Duration::from_secs(15)).expect("the first reading did not arrive");
        let u = measure.finish();
        let output = usage_line(&u);
        if unavailable {
            assert!(
                u.samples == 0 && output.contains("readings unavailable") && !output.contains(USAGE_PREFIX),
                "unavailable reading: {u:?}, {output:?}"
            );
        } else {
            let parsed = parse_usage(output.trim_start_matches(USAGE_PREFIX).trim());
            assert!(parsed.is_some_and(|p| p.samples == 1 && p.incomplete && p.peak_memory_mb > 0.0), "short reading: {u:?}, {output:?}");
        }
    }
}

// Intent (README, "Job sizes and readings"): a tart guest's reading stays
// within the guest's own CPUs and memory, even when its readings return late as
// they do in a busy guest. Adding up processes, a ps returning 1.9 s late on
// every second reading read 70-93 cores on a 14-CPU Mac, and summed RSS read
// 41-97 GB in 10-16 GB guests. This reads the real machine with macOS's own
// tools, as a guest does, while a child keeps one core and 32 MiB busy; every
// second tool returns 1.9 s after it read.
#[cfg(target_os = "macos")]
#[test]
fn guest_reading_stays_within_the_guest() {
    use crate::agent::usage_measure::Sample;
    use crate::agent::usage_measure::guest::{guest_reader_with, guest_size, guest_tool};
    let mut tool: crate::agent::usage_measure::guest::Tool = Box::new(guest_tool);
    let (cpus, memory_mb) = guest_size(&Ctx::background(), &mut tool).unwrap();
    let work = Workload::start(false);
    let mut calls = 0;
    let late = Box::new(move |ctx: &Ctx, name: &str, args: &[&str]| {
        let b = guest_tool(ctx, name, args);
        calls += 1;
        if calls % 2 == 0 {
            ctx.sleep(Duration::from_millis(1900));
        }
        b
    });
    let mut read = guest_reader_with(late);
    let (tx, sampled) = mpsc::channel();
    let measure = measure_with(
        "guest",
        Box::new(move |ctx: &Ctx| -> Result<Sample, String> {
            let s = read(ctx);
            if s.is_ok() {
                let _ = tx.send(());
            }
            s
        }),
    );
    readings(&sampled, 4, Duration::from_secs(30));
    let u = measure.finish();
    work.stop();
    assert!(
        u.scope == "guest"
            && u.samples >= 4
            && !u.incomplete
            && u.peak_cores >= 0.5
            && u.peak_cores <= cpus as f64
            && u.peak_memory_mb >= 32.0
            && u.peak_memory_mb <= memory_mb,
        "readings of a {cpus}-CPU, {memory_mb:.0} MB machine: {u:?}"
    );
    let line = usage_line(&u);
    assert_eq!(parse_usage(line.trim_start_matches(USAGE_PREFIX).trim()).as_ref(), Some(&u), "round-trip readings: {line}");
}
