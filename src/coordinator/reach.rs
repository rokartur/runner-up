//! A machine whose agent cannot be reached rests. Its place runner records it
//! when ssh to the machine fails, which ssh says by exiting 255: the
//! connection was refused, timed out or dropped. While a machine rests its
//! free places take no job, so no job is acquired and no runner registered
//! for them, and the job the runner was started for, waiting again, goes to
//! another place. Every REACH_ASK_EVERY the coordinator asks the machine's
//! agent its version, which costs no call to GitHub; once it answers, the
//! machine's places take jobs again.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use super::place::agent_command;
use super::{Service, State};
use crate::agent;
use crate::config::Machine;
use crate::ctx::Ctx;
use crate::go::time::td;
use crate::proc;

/// ssh's exit code when it could not reach the machine or lost it. The agent
/// itself exits 0 to 3.
pub const SSH_FAILED: i32 = 255;

/// How often the coordinator asks a resting machine's agent.
pub(super) const REACH_ASK_EVERY: Duration = Duration::from_secs(60);

/// Asks machine m's agent its version.
pub fn reach_agent(ctx: &Ctx, m: &Machine) -> Result<(), String> {
    let c = ctx.with_timeout(Duration::from_secs(30));
    let argv = agent_command(m, &["version".to_string()]);
    let (out, res) = proc::combined_output(&c, Command::new(&argv[0]).args(&argv[1..]));
    res.map_err(|e| agent::first_line(&String::from_utf8_lossy(&out), &e))
}

impl State {
    /// Rests machine name from now, unless it rests already. Its agent is
    /// first asked after REACH_ASK_EVERY.
    pub(super) fn rest(&mut self, name: &str) {
        if self.resting(name) {
            return;
        }
        let now = self.now();
        self.unreachable.insert(name.into(), now);
        self.reach_asked.insert(name.into(), now);
        self.log.warn("the machine's agent could not be reached; its places take no jobs until it answers", &[("machine", &name)]);
        self.save();
    }

    pub(super) fn resting(&self, name: &str) -> bool {
        self.unreachable.contains_key(name)
    }
}

impl Service {
    /// Asks each resting machine's agent, at most every REACH_ASK_EVERY, in
    /// the background: place never waits for an answer, and the first place
    /// after one gives the machine's places jobs again. A machine still
    /// resting after a restart is asked at once. Called with mu held, as st.
    pub(super) fn ask_resting(self: &Arc<Self>, ctx: &Ctx, st: &mut State) {
        let Some(reach) = st.reach.clone() else { return };
        let now = st.now();
        for m in st.cfg.machines.clone() {
            let asked = st.reach_asked.get(&m.name).copied().unwrap_or_default();
            if !st.resting(&m.name) || now.sub(asked) < td(REACH_ASK_EVERY) {
                continue;
            }
            st.reach_asked.insert(m.name.clone(), now);
            let call = self.asking.add();
            let (s, ctx, reach) = (self.clone(), ctx.clone(), reach.clone());
            std::thread::spawn(move || {
                let _call = call;
                let res = reach(&ctx, &m);
                let mut st = s.lock();
                if let Err(e) = res {
                    st.log.warn("the machine's agent still cannot be reached", &[("machine", &m.name), ("err", &e)]);
                    return;
                }
                if st.unreachable.remove(&m.name).is_some() {
                    st.log.info("the machine's agent answers again; its places take jobs", &[("machine", &m.name)]);
                    st.save();
                }
            });
        }
    }
}
