// Intent (README, "Macs"). Apple's Virtualization framework runs at most two
// macOS VMs at once on a Mac, so a Mac's tart places never start a third clone,
// and a macOS VM someone else runs there takes one of the two, as a proof clone
// did to runner-up's predecessor (it refused every clone of a place until the
// boot timeout). The clone's boot watch is owned by the agent.

use std::sync::{Arc, Mutex};

use super::*;
use crate::agent;
use crate::coordinator::snapshot::BusyReason;
use crate::coordinator::vmlimit::VM_READ_EVERY;

/// A Mac with Apple silicon, 10 cores and 24 GB, with three small tart places
/// and a host place for ru-mac jobs: its budget fits all four at once, so
/// only the VM limit holds a tart place back.
pub(super) fn one_mac(dir: &Path) -> Config {
    let tart = |id: &str| Place {
        id: id.into(),
        provider: config::PROVIDER_TART.into(),
        pools: strings(&["ru-mac"]),
        cores: 2,
        memory_mb: 4096,
        image: "ghcr.io/cirruslabs/macos-sequoia-base:latest".into(),
        ..Default::default()
    };
    let host = Place {
        id: "mac-host".into(),
        provider: config::PROVIDER_HOST.into(),
        pools: strings(&["ru-mac"]),
        cores: 2,
        memory_mb: 4096,
        ..Default::default()
    };
    Config {
        state_dir: dir.to_string_lossy().into_owned(),
        github: config::GitHub { repos: strings(&["octo-org/app", "octo-user/tool", "octo-user/wallet"]), ..Default::default() },
        pools: vec![pool("ru-mac")],
        machines: vec![config::Machine {
            name: "mac".into(),
            ssh: strings(&["you@mac"]),
            cores: 10,
            memory_mb: 24576,
            room_cores: 1,
            room_memory_mb: 1024,
            places: vec![tart("mac-1"), tart("mac-2"), tart("mac-3"), host],
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl Harness {
    /// Hands the service the recorded waiting jobs as if each had asked for
    /// ru-mac.
    pub(super) fn feed_as_mac(&self, f: &Fixture) {
        for m in &f.messages {
            let mut msg = m.message.clone();
            for j in &mut msg.job_available {
                j.request_labels = strings(&["ru-mac"]);
            }
            self.handle(&m.repo, "ru-mac", &msg);
        }
    }

    fn busy_tart(&self) -> Vec<String> {
        let mut out: Vec<String> =
            self.placed().into_keys().filter(|id| id.starts_with("mac-") && id != "mac-host").collect();
        out.sort();
        out
    }
}

fn why(h: &Harness, p: &Place) -> (bool, String) {
    let (busy, why) = h.svc.place_busy(p);
    (busy, why.map(|BusyReason(s)| s).unwrap_or_default())
}

#[test]
fn mac_runs_at_most_two_clones_at_once() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_mac(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    h.feed_as_mac(&f);
    // Six jobs wait. Two tart places start clones, and the host place, which
    // starts none, takes a job too; the third tart place waits.
    assert_eq!(h.busy_tart(), ["mac-1", "mac-2"], "tart places run jobs, want mac-1 and mac-2");
    assert!(h.placed().contains_key("mac-host"), "the host place was held back by the VM limit");
    let mac3 = h.cfg.machines[0].places[2].clone();
    assert_eq!(why(&h, &mac3), (true, "macOS VM limit: 2 of 2 running".into()), "want mac-3 held by the VM limit");

    // A clone's job ends: one tart place takes the next job, never two.
    h.finish_and_place("mac-1");
    assert_eq!(h.busy_tart().len(), 2, "tart places {:?} run jobs after one finished, want two", h.busy_tart());
    h.finish_and_place("mac-2");
    assert_eq!(h.busy_tart().len(), 2, "tart places {:?} run jobs after another finished, want two", h.busy_tart());
}

/// Stands in for the Mac's agent answering `agent vms`.
#[derive(Default)]
struct VmsOnMac {
    vms: Vec<String>,
    reads: usize,
}

#[test]
fn macos_vm_outside_runner_up_takes_one_of_the_two() {
    let f = load_fixture("waiting-jobs.json");
    let dir = tempfile::tempdir().unwrap();
    let h = new_harness(one_mac(dir.path()), FakeLauncher::new(), Sets::new());
    h.set_now(f.now);
    // A proof clone someone started by hand runs on the Mac, beside a Linux
    // VM, which the agent leaves out of its answer.
    let mac = Arc::new(Mutex::new(VmsOnMac { vms: strings(&["sshhip-xcode271-proof"]), reads: 0 }));
    let reader = mac.clone();
    h.svc.lock().vm_reader = Some(Arc::new(move |_: &Ctx, _: &config::Machine| {
        let mut m = reader.lock().unwrap();
        m.reads += 1;
        Ok(m.vms.clone())
    }));
    let reads = || mac.lock().unwrap().reads;
    let set_vms = |vms: &[&str]| mac.lock().unwrap().vms = strings(vms);
    h.feed_as_mac(&f);
    assert_eq!(h.busy_tart(), ["mac-1"], "tart places run jobs beside the proof clone, want only mac-1");
    let mac2 = h.cfg.machines[0].places[1].clone();
    assert_eq!(why(&h, &mac2).1, "macOS VM limit: 2 of 2 running, outside runner-up: sshhip-xcode271-proof");
    // Every message placed again, but the Mac was asked once.
    assert_eq!(reads(), 1, "the coordinator asked the Mac more than once within {VM_READ_EVERY:?}");

    // mac-1's clone now shows in the Mac's answer too; it counts once.
    set_vms(&["sshhip-xcode271-proof", &agent::clone_name("mac-1")]);
    h.advance(VM_READ_EVERY);
    h.place();
    assert_eq!(h.busy_tart(), ["mac-1"], "tart places run jobs, want only mac-1");

    // The proof clone stops: the next reading frees the second VM.
    set_vms(&[&agent::clone_name("mac-1")]);
    h.advance(VM_READ_EVERY);
    h.place();
    assert_eq!(h.busy_tart(), ["mac-1", "mac-2"], "tart places run jobs once the proof clone stopped, want mac-1 and mac-2");
    assert_eq!(reads(), 3, "the coordinator asked the Mac this many times, want 3");
}
