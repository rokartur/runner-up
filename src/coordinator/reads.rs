//! What the coordinator read from GitHub's API: the order rules' pull
//! request labels and top-level workflows, and which quiet runs GitHub has
//! closed (closedruns.rs). Only placement passes read, one at a time, and
//! never with mu held.

use std::collections::BTreeSet;
use std::time::Duration;

use super::Service;
use crate::ctx::Ctx;
use crate::go::time::{Time, td};
use crate::plan;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkflowRun {
    pub repo: String,
    pub id: i64,
}

#[derive(Clone, Debug, Default)]
pub struct RunWorkflow {
    pub path: String,
    pub attempted: Time,
}

impl Service {
    /// Reads the pull request labels of repositories with waiting pull
    /// request or branch jobs, at most once a minute each. Network reads
    /// never hold the queue lock; an unreadable refresh forgets the
    /// repository's labels rather than keep one that may have been removed.
    pub(super) fn refresh_labels(&self, ctx: &Ctx) {
        let (reader, now, repos) = {
            let mut st = self.lock();
            let Some(reader) = st.label_reader.clone() else { return };
            if !plan::needs(&st.rules).0 {
                // A setup change may have dropped the rules that read labels;
                // labels read before it would be stale once such a rule came
                // back.
                st.labels.clear();
                st.labels_read.clear();
                return;
            }
            let now = st.now();
            let mut repos = BTreeSet::new();
            for j in st.waiting.values() {
                if j.taken_back || j.label_ref().is_empty() {
                    continue;
                }
                let fresh = st.labels_read.get(&j.repo).is_some_and(|last| now.sub(*last) < td(Duration::from_secs(60)));
                if !fresh {
                    repos.insert(j.repo.clone());
                }
            }
            (reader, now, repos)
        };
        for repo in repos {
            let read_ctx = ctx.with_timeout(Duration::from_secs(5));
            let res = reader(&read_ctx, &repo);
            read_ctx.cancel();
            let mut st = self.lock();
            st.labels_read.insert(repo.clone(), now);
            match res {
                Ok(labels) => {
                    st.labels.insert(repo, labels);
                }
                Err(e) => {
                    st.labels.remove(&repo);
                    st.log.warn("pull request labels unreadable; ordering without them", &[("repo", &repo), ("err", &e)]);
                }
            }
        }
    }

    /// Reads each waiting run's top-level workflow once, shared by all its
    /// jobs; a failed read is tried again after a minute. A run no job waits
    /// for any longer is forgotten. The whole pass has five seconds and never
    /// holds the queue lock.
    pub(super) fn refresh_run_workflows(&self, ctx: &Ctx) {
        let (reader, now, reads) = {
            let mut st = self.lock();
            let Some(reader) = st.workflow_reader.clone() else { return };
            if !plan::needs(&st.rules).1 {
                return;
            }
            let now = st.now();
            let mut queued = BTreeSet::new();
            for j in st.waiting.values() {
                if !j.taken_back && j.run_id > 0 {
                    queued.insert(WorkflowRun { repo: j.repo.clone(), id: j.run_id });
                }
            }
            st.run_workflows.retain(|k, _| queued.contains(k));
            let mut reads = Vec::new();
            for key in queued {
                let due = match st.run_workflows.get(&key) {
                    None => true,
                    Some(v) => v.path.is_empty() && now.sub(v.attempted) >= td(Duration::from_secs(60)),
                };
                if due {
                    reads.push(key);
                }
            }
            (reader, now, reads)
        };
        let ctx = ctx.with_timeout(Duration::from_secs(5));
        for key in reads {
            if ctx.is_done() {
                break;
            }
            let res = reader(&ctx, &key.repo, key.id);
            let mut st = self.lock();
            let path = match res {
                Ok(p) => p,
                Err(e) => {
                    st.log
                        .warn("top-level workflow unreadable; using the job's own", &[("repo", &key.repo), ("run", &key.id), ("err", &e)]);
                    String::new()
                }
            };
            st.run_workflows.insert(key, RunWorkflow { path, attempted: now });
        }
        ctx.cancel();
    }
}
