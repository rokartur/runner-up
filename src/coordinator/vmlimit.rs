//! Apple's Virtualization framework runs at most two macOS virtual machines
//! at once on a Mac (agent::MACOS_VM_LIMIT). A VM runner-up did not start,
//! such as one its owner runs by hand, takes one of those two as much as a
//! place's clone does, and tart then refuses the next clone. So a free tart
//! place counts as busy while its Mac already runs as many macOS VMs as the
//! framework allows: its other tart places whose runners or borrow
//! supervisors are alive, each of which reserves its named clone, and the
//! macOS VMs the Mac's agent reports that are no clone of those places.

use std::collections::{BTreeSet, HashMap};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use super::place::agent_command;
use super::snapshot::Snapshot;
use super::{Service, State};
use crate::agent;
use crate::config::{Machine, PROVIDER_TART, Place};
use crate::ctx::Ctx;
use crate::go::time::td;
use crate::proc;

/// How often the coordinator asks a Mac with a free tart place which macOS
/// VMs it runs. A VM started since is met by the agent, which waits for a
/// free one before it clones.
pub(super) const VM_READ_EVERY: Duration = Duration::from_secs(30);

/// How long one placement pass waits for the Macs it reads. A Mac that has
/// not answered by then counts as a failed reading.
pub(super) static VM_READ_BUDGET_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30_000);

/// Asks machine m's agent which macOS VMs it runs.
pub fn read_vms(ctx: &Ctx, m: &Machine) -> Result<Vec<String>, String> {
    let c = ctx.with_timeout(Duration::from_secs(30));
    let argv = agent_command(m, &["agent".to_string(), "vms".to_string()]);
    let out = proc::output(&c, Command::new(&argv[0]).args(&argv[1..]), Vec::new());
    if let Err(e) = out.status {
        return Err(format!("{e} {}", agent::first_line(&String::from_utf8_lossy(&out.stderr), "")));
    }
    serde_json::from_slice::<Option<Vec<String>>>(&out.stdout)
        .map(Option::unwrap_or_default)
        .map_err(|e| format!("unreadable answer: {e}"))
}

impl Service {
    /// Reads the running macOS VMs of every Mac with a free tart place, at
    /// most every VM_READ_EVERY each, outside the queue lock. It reads the
    /// Macs at once, within the read budget for them all, so one Mac slow to
    /// answer never delays placement by more than that. A Mac whose reading
    /// fails counts no VM beyond its places' own until a reading succeeds. A
    /// resting Mac is not read: its places take no job (reach.rs).
    pub(super) fn refresh_vms(&self, ctx: &Ctx) {
        let (reader, now, due) = {
            let st = self.lock();
            let Some(reader) = st.vm_reader.clone() else { return };
            let now = st.now();
            let mut due = Vec::new();
            for m in &st.cfg.machines {
                let read = st.vms_read.get(&m.name).copied().unwrap_or_default();
                if now.sub(read) < td(VM_READ_EVERY) || st.resting(&m.name) {
                    continue;
                }
                if m.places.iter().any(|p| p.provider == PROVIDER_TART && !st.runner_alive(&p.id)) {
                    due.push(m.clone());
                }
            }
            (reader, now, due)
        };
        if due.is_empty() {
            return;
        }
        let budget = Duration::from_millis(VM_READ_BUDGET_MS.load(std::sync::atomic::Ordering::Relaxed));
        let ctx = ctx.with_timeout(budget);
        // The channel never blocks a reader that answers after the budget.
        let (tx, rx) = mpsc::channel();
        for m in &due {
            let (tx, ctx, reader, m) = (tx.clone(), ctx.clone(), reader.clone(), m.clone());
            std::thread::spawn(move || {
                let r = reader(&ctx, &m);
                let _ = tx.send((m.name, r));
            });
        }
        let mut got = HashMap::new();
        while got.len() < due.len() && !ctx.is_done() {
            let wait = ctx.remaining().unwrap_or(budget).min(Duration::from_millis(50));
            if let Ok((name, r)) = rx.recv_timeout(wait) {
                got.insert(name, r);
            }
        }
        ctx.cancel();
        let mut st = self.lock();
        for m in &due {
            st.vms_read.insert(m.name.clone(), now);
            let r = got.remove(&m.name).unwrap_or_else(|| Err(format!("no answer within {}", crate::go::duration::format(budget))));
            match r {
                Ok(vms) => {
                    st.vms.insert(m.name.clone(), vms);
                }
                Err(e) => {
                    st.vms.remove(&m.name);
                    st.log.warn(
                        "could not read the Mac's running macOS VMs; counting only its places' clones",
                        &[("machine", &m.name), ("err", &e)],
                    );
                }
            }
        }
    }
}

impl State {
    /// Why tart place p cannot start a clone now, or None when it can.
    pub(super) fn vm_limit_full(&self, v: &mut Snapshot, p: &Place) -> Option<String> {
        if p.provider != PROVIDER_TART {
            return None;
        }
        let mut own = BTreeSet::from([agent::clone_name(&p.id)]);
        let mut clones = 0;
        for q in &self.cfg.machine(&p.machine).places {
            if q.provider != PROVIDER_TART || q.id == p.id || !v.place(self, &q.id).alive {
                continue;
            }
            own.insert(agent::clone_name(&q.id));
            clones += 1;
        }
        let others: Vec<&str> =
            self.vms.get(&p.machine).into_iter().flatten().filter(|vm| !own.contains(*vm)).map(String::as_str).collect();
        if clones + others.len() < agent::MACOS_VM_LIMIT {
            return None;
        }
        let mut why = format!("macOS VM limit: {} of {} running", clones + others.len(), agent::MACOS_VM_LIMIT);
        if !others.is_empty() {
            why += &format!(", outside runner-up: {}", others.join(", "));
        }
        Some(why)
    }
}
