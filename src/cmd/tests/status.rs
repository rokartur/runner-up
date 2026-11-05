// Intent (README.md, "Setting up": "runner-up status shows each place, its
// running reservation and the waiting work"; "When a machine is down":
// "runner-up status shows a resting machine's places as resting"): status
// gives each place one line saying whether it is free, which job or borrow
// holds it, or that its machine rests, then each waiting job and borrow and
// each job acquired for a place. This test owns those lines' words; journey
// tests read the state status reads (place.json, busy.lock, state.json)
// instead of typing them. drain_command_admits_the_pair_only_once_its_places_are_quiet
// owns a drain's lines, which need a drain a coordinator moves on.

use chrono::TimeZone;
use serde_json::json;

use super::setup::initial_setup;
use crate::config::{self, Machine, PROVIDER_DOCKER, Place, Share};
use crate::coordinator::{PlaceRun, status};
use crate::go::time::Time;
use crate::plan::Job;

#[test]
fn status_says_each_place_and_job_in_one_line() {
    let (_dir, mut cfg) = initial_setup();
    let docker = |id: &str| Place {
        id: id.into(),
        provider: PROVIDER_DOCKER.into(),
        pools: vec!["ru-linux".into()],
        cores: 2,
        memory_mb: 4096,
        ..Default::default()
    };
    cfg.machines = vec![
        Machine { name: "box".into(), places: ["box-1", "box-2", "box-3", "box-4"].map(docker).to_vec(), ..Default::default() },
        Machine { name: "far".into(), places: vec![docker("far-1")], ..Default::default() },
    ];
    cfg.defaults();
    let at = Time(chrono::Utc.with_ymd_and_hms(2026, 11, 3, 9, 5, 0).unwrap());
    let since = at.local_rfc3339();
    let mut held = Vec::new();
    let mut hold = |id: &str, mut run: PlaceRun| {
        std::fs::create_dir_all(cfg.place_dir(id)).unwrap();
        let lock = crate::lock::busy(&cfg.place_dir(id).join("busy.lock")).unwrap();
        held.push(lock.unwrap_or_else(|| panic!("lock {id}: held")));
        run.reservation = Share { cores: 2, memory_mb: 4096 };
        config::write_json(&cfg.place_dir(id).join("place.json"), &run).unwrap();
    };
    hold("box-2", PlaceRun { repo: "octo-org/app".into(), job_name: "verify / linux".into(), ..Default::default() });
    hold("box-3", PlaceRun { repo: "octo-org/app".into(), job_name: "affected-journeys".into(), started: true, ..Default::default() });
    hold("box-4", PlaceRun { borrow_id: "request-check".into(), job_name: "native check".into(), launched_at: at, ..Default::default() });
    // state.json as the coordinator saves it; an acquired job's type is the
    // coordinator's own, so it is written as JSON.
    let job = |repo: &str, borrow_id: &str, name: &str, queue_time| Job {
        repo: repo.into(),
        borrow_id: borrow_id.into(),
        pool: "ru-linux".into(),
        name: name.into(),
        queue_time,
        ..Default::default()
    };
    let state = json!({
        "waiting": [job("octo-org/app", "", "suite", at), job("", "request-two", "second check", at)],
        "acquired": [{"job": job("octo-org/app", "", "build", Time::default()), "place": "box-1", "at": at}],
        "unreachable": {"far": at},
    });
    config::write_json(&std::path::Path::new(&cfg.state_dir).join("state.json"), &state).unwrap();

    let mut out = Vec::new();
    let code = status(cfg, &mut out);
    assert_eq!(code, 0, "status exited {code}");
    let want = [
        "place box-1 on box (docker, 2 cores, 4096 MB) free".to_string(),
        r#"place box-2 on box (docker, 2 cores, 4096 MB) busy (job): octo-org/app "verify / linux" waiting for its job"#.into(),
        r#"place box-3 on box (docker, 2 cores, 4096 MB) busy (job): octo-org/app "affected-journeys" running"#.into(),
        format!(r#"place box-4 on box (docker, 2 cores, 4096 MB) busy (borrow): borrow "native check" since {since}"#),
        format!("place far-1 on far (docker, 2 cores, 4096 MB) resting (agent unreachable since {since})"),
        format!(r#"waiting octo-org/app ru-linux "suite" since {since}"#),
        format!(r#"waiting borrow ru-linux "second check" since {since}"#),
        r#"starting octo-org/app ru-linux "build" in box-1"#.into(),
    ]
    .join("\n")
        + "\n";
    let got = String::from_utf8(out).unwrap();
    assert_eq!(got, want, "status said\n{got}\nwant\n{want}");
    drop(held);
}
