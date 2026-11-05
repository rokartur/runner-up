// Intent (README, "Job sizes and readings"): `runner-up usage` never shows a
// tart guest using more than its own CPUs or memory; such a value reads as
// unavailable. tart sizes a guest to its reservation. A host job is not
// confined to its reservation, so its reading stays. The records are real
// readings from a Mac fleet, renamed: they read 147.70 cores and 83 GB in a
// 4-core, 16 GB guest.

use std::path::Path;

use super::setup::initial_setup;
use crate::cmd::usage_report;
use crate::config::{PROVIDER_TART, Pool};
use crate::coordinator::read_usage;

#[test]
fn usage_withholds_guest_readings_above_the_guests_size() {
    let (_dir, mut cfg) = initial_setup();
    let records_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/cmd/usage");
    let usage = Path::new(&cfg.state_dir).join("usage");
    std::fs::create_dir_all(&usage).unwrap();
    let mut files = 0;
    for f in std::fs::read_dir(&records_dir).unwrap() {
        let f = f.unwrap().path();
        if f.extension().is_some_and(|x| x == "json") {
            std::fs::copy(&f, usage.join(f.file_name().unwrap())).unwrap();
            files += 1;
        }
    }
    assert!(files > 0, "no records in {}", records_dir.display());
    let records = read_usage(&cfg).unwrap();
    assert_eq!(records.len(), files, "records");
    cfg.pools =
        records.iter().map(|r| Pool { label: r.pool.clone(), cores: r.reservation.cores, memory_mb: r.reservation.memory_mb }).collect();
    let mut out = Vec::new();
    let code = usage_report(&cfg, &mut out, &mut std::io::sink());
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "{out}");
    let mut withheld = 0;
    for r in &records {
        let started = r.started_at.0.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S").to_string();
        let line = out.lines().rfind(|l| l.contains(&started)).unwrap_or("");
        let u = r.usage.as_ref().unwrap();
        let guest = r.provider == PROVIDER_TART;
        let (cores, memory) = (format!("{:.2} cores", u.peak_cores), format!("{:.1} MB", u.peak_memory_mb));
        let over_cores = guest && u.peak_cores > r.reservation.cores as f64;
        let over_memory = guest && u.peak_memory_mb > r.reservation.memory_mb as f64;
        assert!(
            line.contains(&cores) != over_cores
                && line.contains("cores unavailable") == over_cores
                && line.contains(&memory) != over_memory
                && line.contains("memory unavailable") == over_memory,
            "{} record {u:?}\nread: {line:?}",
            r.provider
        );
        if over_cores || over_memory {
            withheld += 1;
        }
    }
    assert!(
        withheld != 0 && withheld != records.len(),
        "the records must hold readings above and within their size: {withheld} of {} above",
        records.len()
    );
}
