// Intent (README, "When a machine is down"). Before, the coordinator ran tidy
// and place again the moment a place runner ended, and the dead machine,
// counting no busy place, took the oldest waiting job straight back: 50
// refusals made 50 registrations with no time passing, while a younger job took
// a freed healthy place.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::service::{PR184, PUBLISH, SUITE, VERIFY};
use super::*;
use crate::coordinator::reach::REACH_ASK_EVERY;

impl FakeLauncher {
    /// Ends place id's runner as one whose ssh to its machine failed, which
    /// the place runner records in place.json before it exits.
    fn cannot_reach(&self, id: &str) {
        let path = self.st().places.join(id).join("place.json");
        let mut run: PlaceRun = config::read_json(&path).unwrap();
        run.unreachable = true;
        config::write_json(&path, &run).unwrap();
        self.give_up(id);
    }
}

/// Stands in for the far machine's agent answering over SSH.
#[derive(Default)]
struct FarAgent {
    answers: bool,
    asked: usize,
}

fn far_reacher(far: &Arc<Mutex<FarAgent>>) -> crate::coordinator::Reacher {
    let far = far.clone();
    Arc::new(move |_: &Ctx, m: &config::Machine| {
        let mut a = far.lock().unwrap();
        if m.name != "far" {
            return Err(format!("asked {}, which never failed", m.name));
        }
        a.asked += 1;
        if !a.answers {
            return Err("ssh: connect to host far port 22: Connection refused".into());
        }
        Ok(())
    })
}

fn times(far: &Arc<Mutex<FarAgent>>) -> usize {
    far.lock().unwrap().asked
}

/// Adds a machine reached over SSH with one small place, listed last.
fn with_far(mut cfg: Config) -> Config {
    cfg.machines.push(config::Machine {
        name: "far".into(),
        ssh: strings(&["m1@far"]),
        places: vec![Place {
            id: "far-1".into(),
            provider: config::PROVIDER_DOCKER.into(),
            pools: strings(&["ru-linux"]),
            cores: 2,
            memory_mb: 4096,
            ..Default::default()
        }],
        ..Default::default()
    });
    cfg
}

impl Harness {
    /// Counts the runners registered with every scale set.
    fn registrations(&self) -> i64 {
        self.sets.values().map(|s| s.st().next_id).sum()
    }

    /// The serve loop's answer to a place runner ending (serve.rs): tidy and
    /// place at once, then any answer from a resting machine's agent.
    fn round(&self) {
        self.tidy();
        self.place();
        let asking = self.svc.asking.clone();
        wait_for("the resting machines' agents to answer", move || asking.wait());
    }
}

#[test]
fn unreachable_machine_rests_until_its_agent_answers() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let launcher = FakeLauncher::new();
    let h = new_harness(with_far(one_box(dir.path())), launcher.clone(), Sets::new());
    let far = Arc::new(Mutex::new(FarAgent::default()));
    h.svc.lock().reach = Some(far_reacher(&far));
    h.set_now(f.now);
    h.feed(&f);
    // The box's three places are busy, so far, idle, takes PR #184's job.
    h.want(&[("box-1", VERIFY), ("box-2", SUITE), ("box-large", PUBLISH), ("far-1", PR184)]);

    // Far refuses SSH. Its runner ends, unused; then 50 rounds pass with no
    // time passing, as fast as the main loop runs.
    let registered = h.registrations();
    launcher.cannot_reach("far-1");
    for _ in 0..50 {
        h.round();
    }
    h.want(&[("box-1", VERIFY), ("box-2", SUITE), ("box-large", PUBLISH)]);
    assert_eq!(h.registrations() - registered, 0, "made runner registrations while far rests, want none");
    {
        let app = h.set("octo-org/app", "ru-linux");
        assert!(
            app.acquired == [9101, 9102] && app.removed.len() == 1,
            "app acquired {:?} and removed {} runners, want 9101 and 9102 once each and far's runner removed",
            app.acquired,
            app.removed.len()
        );
    }
    assert_eq!(times(&far), 0, "asked far's agent within a minute of the refusal, want none");

    // A healthy place frees: the job far held, older, takes it before PR
    // #186's.
    launcher.finish("box-1");
    h.round();
    assert_eq!(h.run("box-1").request_id, 9102, "box-1 runs another request, want 9102, the job far held");

    // A minute on, far is asked once, still refuses, and keeps resting.
    h.advance(REACH_ASK_EVERY);
    h.round();
    h.round();
    assert!(
        times(&far) == 1 && !h.placed().contains_key("far-1"),
        "asked far {} times and far-1 runs {:?}, want one question and far-1 empty",
        times(&far),
        h.placed().get("far-1")
    );

    // The coordinator restarts while far rests: it still rests, and is asked
    // at once.
    let h2 = new_harness(with_far(one_box(dir.path())), launcher.clone(), h.sets.clone());
    h2.svc.lock().reach = Some(far_reacher(&far));
    h2.set_now(h.now().add(Duration::from_secs(10)));
    let registered = h2.registrations();
    h2.round();
    assert!(
        times(&far) == 2 && !h2.placed().contains_key("far-1") && h2.registrations() == registered,
        "after the restart far was asked {} times in all and far-1 runs {:?}, want two and empty",
        times(&far),
        h2.placed().get("far-1")
    );

    // Far comes back. A minute on its agent answers, and far-1 takes PR
    // #186's job with one new registration.
    far.lock().unwrap().answers = true;
    h2.advance(REACH_ASK_EVERY);
    h2.round();
    h2.place();
    assert!(
        h2.placed().get("far-1").map(String::as_str) == Some(PR184) && h2.registrations() == registered + 1,
        "far-1 runs {:?} after {} new registrations, want PR #186's job after one",
        h2.placed().get("far-1"),
        h2.registrations() - registered
    );
}
