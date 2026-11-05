//! One placement pass's reading of the places: whether each place's runner
//! or borrow supervisor holds its lock, and the place's place.json while it
//! does. A plan asks about a place once for itself and again for every place
//! beside it on its machine; the snapshot reads it once. The pass forgets a
//! place it claims, to read it afresh. Only places the pass claims become
//! busy while it runs, so the rest of a snapshot may at most show a place
//! busy that has just become free, never the reverse. It is used under mu.

use std::collections::HashMap;

use super::place::read_place_run;
use super::{PlaceRun, State};
use crate::config::{Place, Share};
use crate::lock;
use crate::plan::{Headroom, PlaceView};

#[derive(Default)]
pub struct Snapshot {
    places: HashMap<String, PlaceState>,
}

/// One place as a snapshot read it.
#[derive(Clone, Default)]
pub struct PlaceState {
    /// The place's runner or borrow supervisor holds its lock, or the lock
    /// could not be read, which counts alive so a broken reader never starts
    /// a second job in a place.
    pub alive: bool,
    pub unreadable: bool,
    /// The place's place.json, read only while it is alive.
    pub run: Option<PlaceRun>,
}

/// Why a place is busy: one of the reasons below, or what keeps it from
/// fitting on its machine, such as "cores 4 of 6 in use".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusyReason(pub String);

impl BusyReason {
    /// Its lock could not be read.
    pub const UNREADABLE: &str = "unreadable";
    /// A place runner holds it, for a job.
    pub const JOB: &str = "job";
    /// A borrow supervisor holds it.
    pub const BORROW: &str = "borrow";

    /// Says whether a job or a borrow holds the place.
    pub fn runs(&self) -> bool {
        self.0 == Self::JOB || self.0 == Self::BORROW
    }
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn share(p: &Place) -> Share {
    Share { cores: p.cores, memory_mb: p.memory_mb }
}

impl Snapshot {
    pub fn place(&mut self, st: &State, id: &str) -> PlaceState {
        if let Some(ps) = self.places.get(id) {
            return ps.clone();
        }
        let held = lock::held_by(&st.cfg.place_dir(id).join("busy.lock"));
        let unreadable = held.is_err();
        let alive = held.unwrap_or(true);
        let run = if alive { read_place_run(&st.cfg, id) } else { None };
        let ps = PlaceState { alive, unreadable, run };
        self.places.insert(id.into(), ps.clone());
        ps
    }

    /// Drops what the snapshot read of place id.
    pub fn forget(&mut self, id: &str) {
        self.places.remove(id);
    }

    /// Every place's state for the planner. A place cooling down after a
    /// failed launch reads as busy for a minute, and a place on a resting
    /// machine until the machine's agent answers (reach.rs). A place a drain
    /// holds reads as busy until all the drain's places are free, then takes
    /// only the jobs the drain admits (drain.rs).
    pub fn views(&mut self, st: &State) -> Vec<PlaceView> {
        let now = st.now();
        let places = st.cfg.places();
        let mut out = Vec::with_capacity(places.len());
        for p in places {
            // Resource fit depends on the job, not the place's fallback size.
            let (mut busy, _) = self.busy(st, &p, false);
            let cooling = st.cooling.get(&p.id).is_some_and(|t| now.before(*t));
            if !busy && (cooling || st.resting(&p.machine) || st.held.get(&p.id).is_some_and(|w| !w.is_empty())) {
                busy = true;
            }
            let mut view = PlaceView { place: p.clone(), busy, drain: String::new() };
            if let Some(d) = st.drain_of(&p.id) {
                view.drain = d.req.id.clone();
                view.busy = busy || d.st.drained.is_zero();
            }
            out.push(view);
        }
        out
    }

    /// What each counted machine's jobs may still reserve.
    pub fn headroom(&mut self, st: &State) -> HashMap<String, Headroom> {
        let mut out = HashMap::new();
        for m in &st.cfg.machines {
            if m.cores == 0 && m.memory_mb == 0 {
                continue;
            }
            let b = m.budget();
            let used = self.in_use(st, &m.name, "");
            let cores = if m.cores == 0 { i64::MAX } else { b.cores - used.cores };
            out.insert(m.name.clone(), Headroom { cores, memory_mb: b.memory_mb - used.memory_mb, memory: b.memory_mb > 0 });
        }
        out
    }

    /// Sums what machine's places reserve while their runner or borrow
    /// supervisor is alive, leaving out the place except. A runner waiting
    /// for its job keeps its share, since the job will run there. A departed
    /// place counts on the machine its runner runs on, not where the setup
    /// has it now (reload.rs).
    pub fn in_use(&mut self, st: &State, machine: &str, except: &str) -> Share {
        let mut used = Share::default();
        let mut places: Vec<Place> = st.cfg.places().into_iter().filter(|q| !st.departed.contains_key(&q.id)).collect();
        places.extend(st.departed.values().cloned());
        for q in places {
            if q.id != except && q.machine == machine && self.place(st, &q.id).alive {
                let sh = self.reservation(st, &q);
                used.cores += sh.cores;
                used.memory_mb += sh.memory_mb;
            }
        }
        used
    }

    /// The size held by the running place, frozen in its place.json at
    /// launch. An unknown live reservation must not be mistaken for a smaller
    /// place default: until it can be read, the place holds its machine's
    /// whole counted budget.
    pub fn reservation(&mut self, st: &State, p: &Place) -> Share {
        if let Some(run) = &self.place(st, &p.id).run
            && run.reservation.cores > 0
            && run.reservation.memory_mb > 0
        {
            return run.reservation;
        }
        let b = st.cfg.machine(&p.machine).budget();
        Share { cores: b.cores.max(p.cores), memory_mb: b.memory_mb.max(p.memory_mb) }
    }

    pub fn busy(&mut self, st: &State, p: &Place, resources: bool) -> (bool, Option<BusyReason>) {
        let ps = self.place(st, &p.id);
        if ps.unreadable {
            return (true, Some(BusyReason(BusyReason::UNREADABLE.into())));
        }
        if ps.alive {
            if ps.run.is_some_and(|r| !r.borrow_id.is_empty()) {
                return (true, Some(BusyReason(BusyReason::BORROW.into())));
            }
            return (true, Some(BusyReason(BusyReason::JOB.into())));
        }
        if resources && let Some(why) = self.tight(st, p) {
            return (true, Some(BusyReason(why)));
        }
        if let Some(why) = st.vm_limit_full(self, p) {
            return (true, Some(BusyReason(why)));
        }
        (false, None)
    }

    /// Why place p's job size does not fit beside its machine's other runs,
    /// or None when it fits. A machine without a budget always fits.
    fn tight(&mut self, st: &State, p: &Place) -> Option<String> {
        let m = st.cfg.machine(&p.machine);
        if m.cores == 0 && m.memory_mb == 0 {
            return None;
        }
        let (b, used, sh) = (m.budget(), self.in_use(st, &p.machine, &p.id), share(p));
        let room = |n: i64, unit: &str| if n == 0 { String::new() } else { format!(", {n}{unit} kept for the owner") };
        if b.cores > 0 && used.cores + sh.cores > b.cores {
            return Some(format!("cores {} of {} in use{}", used.cores, b.cores, room(m.room_cores, "")));
        }
        if b.memory_mb > 0 && used.memory_mb + sh.memory_mb > b.memory_mb {
            return Some(format!("memory {} of {} MB in use{}", used.memory_mb, b.memory_mb, room(m.room_memory_mb, " MB")));
        }
        None
    }
}

impl State {
    /// Whether a place's runner or borrow supervisor is alive, from its lock
    /// alone. A lock that cannot be read counts alive.
    pub fn runner_alive(&self, place_id: &str) -> bool {
        lock::held_by(&self.cfg.place_dir(place_id).join("busy.lock")).unwrap_or(true)
    }
}

#[cfg(test)]
impl super::Service {
    /// Snapshot::in_use read afresh.
    #[cfg(unix)]
    pub fn in_use(&self, machine: &str, except: &str) -> Share {
        let st = self.lock();
        Snapshot::default().in_use(&st, machine, except)
    }

    /// Reads one place, sized for the pool when checking an assignment. Its
    /// own lock says whether its runner or borrow supervisor is alive. A
    /// place is also busy while its size does not fit beside the other runs,
    /// within the machine's budget less the owner's room. A tart place is
    /// also busy while its Mac runs as many macOS VMs as it may (vmlimit.rs).
    /// A lock that cannot be read counts busy, so a broken reader never
    /// starts a second job in a place.
    pub fn place_busy(&self, p: &Place) -> (bool, Option<BusyReason>) {
        let st = self.lock();
        Snapshot::default().busy(&st, p, true)
    }
}
