use std::sync::mpsc;
use std::time::Duration;

use crate::agent::{AgentRun, lock_place};
use crate::ctx::Ctx;

fn place(dir: &tempfile::TempDir) -> AgentRun {
    AgentRun { place: "ru-e2e-1".into(), state_dir: dir.path().to_string_lossy().into_owned(), ..Default::default() }
}

// Intent (a bug a live Hyper-V check found): a finished place left
// its lock behind. A place leaves nothing once its agent lets go, and
// two agents still never hold one place at once.
#[test]
fn a_finished_place_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let a = place(&dir);
    drop(lock_place(&Ctx::background(), &a).unwrap());
    if a.dir().exists() {
        let entries: Vec<_> = std::fs::read_dir(a.dir()).unwrap().flatten().map(|e| e.file_name()).collect();
        panic!("the finished place left {} with {entries:?}", a.dir().display());
    }
}

// The second agent tries once a second; after 1.5 s it has found the place
// held at least once.
#[test]
fn an_agent_waiting_for_a_place_takes_it_when_the_other_lets_go() {
    let dir = tempfile::tempdir().unwrap();
    let a = place(&dir);
    let first = lock_place(&Ctx::background(), &a).unwrap();
    let (tx, got) = mpsc::channel();
    let b = a.clone();
    std::thread::spawn(move || {
        let _ = tx.send(lock_place(&Ctx::background(), &b).unwrap());
    });
    assert!(got.recv_timeout(Duration::from_millis(1500)).is_err(), "a second agent took a place another holds");
    drop(first);
    let second = got.recv_timeout(Duration::from_secs(10)).expect("the waiting agent did not take the place");
    // The waiting agent holds the lock file now at the place's path: a
    // third finds the place busy.
    let f = crate::lock::busy(&a.dir().join("agent.lock"));
    assert!(matches!(f, Ok(None)), "a third agent could lock the place ({:?})", f.err());
    drop(second);
}
