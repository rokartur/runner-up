//! A setup change applies in place (setup.rs). Running jobs run on: each
//! place runner runs as the setup it started with, and its job's reservation,
//! frozen in place.json at launch, keeps counting against its machine. A
//! place the change removes, or moves to another machine, while its runner
//! runs is departed: it takes no job, it counts against the machine its
//! runner runs on, and tidy looks after its runner as before, until the
//! runner ends. Waiting jobs keep their place in line. A job of a pool the
//! change removes waits with no session to place it, until the pool comes
//! back or the job expires.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{Service, State};
use crate::config::{self, Config, Machine, Place, Pool};
use crate::plan::Rule;

/// Makes the folders cfg's places and logs need.
pub fn prepare(cfg: &Config) -> Result<(), String> {
    for p in cfg.places() {
        let dir = cfg.place_dir(&p.id);
        config::create_dir_all_mode(&dir, 0o700).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    let dir = cfg.logs_dir();
    config::create_dir_all_mode(&dir, 0o700).map_err(|e| format!("mkdir {}: {e}", dir.display()))
}

impl State {
    /// Every place the coordinator looks after: the setup's, and the departed
    /// places the setup no longer has.
    pub(super) fn places(&self) -> Vec<Place> {
        let mut places = self.cfg.places();
        for (id, p) in &self.departed {
            if self.cfg.place(id).is_none() {
                places.push(p.clone());
            }
        }
        places
    }
}

impl Service {
    /// Replaces the setup the coordinator runs with next, checked as
    /// check-config checks it, and reports whether anything changed. Nothing
    /// running stops: the caller opens and closes scale set sessions to match
    /// (Sessions::match_setup). A setup that moves the state folder is
    /// refused, and the running setup stays, since the running places hold
    /// their locks there.
    pub fn reload(&self, next: Arc<Config>, rules: Vec<Rule>) -> Result<bool, String> {
        let mut st = self.lock();
        let old = st.cfg.clone();
        if *old == *next {
            return Ok(false);
        }
        if next.state_dir != old.state_dir {
            return Err(format!(
                "stateDir changes only when runner-up serve restarts: running places hold their locks in {}",
                old.state_dir
            ));
        }
        prepare(&next)?;
        self.launcher.use_setup(&next)?;
        for p in old.places() {
            if next.place(&p.id).is_some_and(|q| q.machine == p.machine) {
                continue;
            }
            // A place departed before is counted where its runner runs already.
            if st.departed.contains_key(&p.id) || !st.runner_alive(&p.id) {
                continue;
            }
            st.log.info(
                "the setup no longer has this place on this machine; its runner runs on, and its share counts until it ends",
                &[("place", &p.id), ("machine", &p.machine)],
            );
            st.departed.insert(p.id.clone(), p);
        }
        st.departed.retain(|id, p| next.place(id).is_none_or(|q| q.machine != p.machine));
        let attrs = describe_change(&old, &next);
        st.cfg = next.clone();
        st.rules = rules;
        let gone: Vec<String> =
            st.unreachable.keys().filter(|name| !next.machines.iter().any(|m| &m.name == *name)).cloned().collect();
        for name in gone {
            st.unreachable.remove(&name);
            st.reach_asked.remove(&name);
        }
        st.save();
        let attrs: Vec<(&str, &dyn std::fmt::Display)> = attrs.iter().map(|(k, v)| (*k, v as &dyn std::fmt::Display)).collect();
        st.log.info("applied the changed setup", &attrs);
        Ok(true)
    }
}

/// Names what changed from old to next, for the log: each repository or
/// organization, pool, machine and place added (+), removed (-) or changed
/// (~), the new order, and the other settings changed.
pub fn describe_change(old: &Config, next: &Config) -> Vec<(&'static str, String)> {
    let mut attrs = Vec::new();
    let mut add = |name: &'static str, change: String| {
        if !change.is_empty() {
            attrs.push((name, change));
        }
    };
    let targets = |c: &Config| -> BTreeMap<String, bool> { c.targets().into_iter().map(|t| (t, true)).collect() };
    let pools = |c: &Config| -> BTreeMap<String, Pool> { c.pools.iter().map(|p| (p.label.clone(), p.clone())).collect() };
    let machines = |c: &Config| -> BTreeMap<String, Machine> {
        c.machines.iter().map(|m| (m.name.clone(), Machine { places: Vec::new(), ..m.clone() })).collect()
    };
    let places = |c: &Config| -> BTreeMap<String, Place> { c.places().into_iter().map(|p| (p.id.clone(), p)).collect() };
    let settings = |c: &Config| -> BTreeMap<String, serde_json::Value> {
        let mut rest = c.clone();
        rest.github.repos.clear();
        rest.github.orgs.clear();
        rest.pools.clear();
        rest.machines.clear();
        rest.order.clear();
        match serde_json::to_value(&rest) {
            Ok(serde_json::Value::Object(m)) => m.into_iter().collect(),
            _ => BTreeMap::new(),
        }
    };
    add("targets", changes(&targets(old), &targets(next)));
    add("pools", changes(&pools(old), &pools(next)));
    add("machines", changes(&machines(old), &machines(next)));
    add("places", changes(&places(old), &places(next)));
    if old.order != next.order {
        let order = if next.order.is_empty() { "default".to_string() } else { next.order.join(" ") };
        add("order", order);
    }
    add("settings", changes(&settings(old), &settings(next)));
    attrs
}

/// Lists the keys after adds (+), changes (~) and drops (-) from before, in
/// order.
fn changes<T: PartialEq>(before: &BTreeMap<String, T>, after: &BTreeMap<String, T>) -> String {
    let mut out = Vec::new();
    for (k, v) in after {
        match before.get(k) {
            None => out.push(format!("+{k}")),
            Some(b) if b != v => out.push(format!("~{k}")),
            _ => {}
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            out.push(format!("-{k}"));
        }
    }
    out.join(" ")
}
