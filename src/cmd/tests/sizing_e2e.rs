use serde_json::json;

use super::e2e::{E2e, job_message, verify_job};
use super::journeys::skip_if_short;
use crate::config::{Machine, PROVIDER_HOST, Place, Pool, Share};
use crate::coordinator::read_usage;

// Intent (README, "Job sizes and readings"): a size selected for a pool reaches
// a real host agent and the job environment, its actual readings survive
// retirement and a coordinator restart, and `usage` shows readings beside the
// configured size. The real coordinator picks the size and writes it into
// place.json, so a wrong pick fails here: the place's own size differs from
// the pool's. Only GitHub's service and runner are stand-ins; no sign-in or
// real fleet is used.
#[test]
fn sized_host_job_reports_its_use() {
    skip_if_short!();
    let mut e = E2e::new();
    let Some(python) = super::unreachable::look_path("python3") else {
        eprintln!("python3 is needed for the isolated workload");
        return;
    };
    // This deliberately bounded workload, rather than a sleep, exercises the
    // actual CPU/RSS reader over more than one sampling interval.
    let script = r#"#!/bin/sh
set -eu
printf '%s %s\n' "$RUNNER_UP_CORES" "$RUNNER_UP_MEMORY_MB" > "$SIZE_PROOF/env"
echo 'Running job: sized host check'
"$SIZE_PYTHON" - <<'PY'
import time
memory=bytearray(16*1024*1024)
start=time.monotonic()
while time.monotonic()-start < 3:
    memory[0]=(memory[0]+1)%256
PY
echo 'Job sized host check completed with result: Succeeded'
"#;
    let archive = e.dir.join("runner.tar.gz");
    let gz = flate2::write::GzEncoder::new(std::fs::File::create(&archive).unwrap(), flate2::Compression::default());
    let mut tw = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    tw.append_data(&mut header, "run.sh", script.as_bytes()).unwrap();
    tw.into_inner().unwrap().finish().unwrap();

    e.cfg.pools = vec![Pool { label: "ru-linux".into(), cores: 2, memory_mb: 4096, ..Default::default() }];
    e.cfg.machines = vec![Machine {
        name: "mac".into(),
        ssh: vec!["ci@mac".into()],
        agent: super::runner_up().to_string(),
        cores: 14,
        memory_mb: 49152,
        places: vec![Place {
            id: "host-1".into(),
            provider: PROVIDER_HOST.into(),
            pools: vec!["ru-linux".into()],
            cores: 6,
            memory_mb: 16384,
            ..Default::default()
        }],
        ..Default::default()
    }];
    e.save();
    let (pool, place) = (e.cfg.pools[0].clone(), e.cfg.places()[0].clone());
    assert!(
        pool.size() != (Share { cores: place.cores, memory_mb: place.memory_mb }),
        "the pool's size must differ from its place's for this test to tell them apart"
    );
    // The agent's own environment says 999: the job must see the pool's size,
    // which the agent sets, not what it inherited.
    let dir = e.dir.to_string_lossy().into_owned();
    for (k, v) in [
        ("HOME", dir.as_str()),
        ("SIZE_PROOF", &dir),
        ("SIZE_PYTHON", &python),
        ("RUNNER_UP_CORES", "999"),
        ("RUNNER_UP_MEMORY_MB", "999"),
        ("RUNNER_UP_TEST_RUNNER_ARCHIVE", &archive.to_string_lossy()),
    ] {
        e.env.push((k.into(), v.into()));
    }
    e.actions.send(&[job_message(&verify_job(), "JobAvailable", json!({}))]);
    e.serve();

    e.eventually("the coordinator keeps the finished job's readings", || read_usage(&e.cfg).is_ok_and(|r| r.len() == 1));
    let env = std::fs::read_to_string(e.dir.join("env")).unwrap_or_default();
    assert_eq!(env, format!("{} {}\n", pool.cores, pool.memory_mb), "job size\n{}", e.logs());
    let r = read_usage(&e.cfg).unwrap().remove(0);
    assert!(
        r.pool == "ru-linux" && r.place == "host-1" && r.reservation == pool.size() && r.usage.as_ref().is_some_and(|u| u.samples >= 2),
        "reading: {r:?}\n{}",
        e.logs()
    );

    // A coordinator restart cannot count the completed job twice.
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    e.serve();
    e.eventually("the restarted coordinator polls its new session twice", || {
        let seen = e.actions.read();
        seen.opened == 2 && seen.polls >= 2
    });
    let records = read_usage(&e.cfg).unwrap_or_default();
    assert_eq!(records.len(), 1, "restart duplicated readings: {records:?}");
    let code = e.stop();
    assert_eq!(code, 0, "the coordinator exited {code} on SIGTERM\n{}", e.logs());
    let (code, result) = crate::agent::tests::run(&mut e.command(&["usage", "--config", &e.config]), "");
    assert!(
        code == 0
            && result.contains(&format!("configured {} cores, {} MB", pool.cores, pool.memory_mb))
            && result.contains("process-group")
            && !result.contains("unavailable"),
        "usage: {code}\n{result}"
    );
}
