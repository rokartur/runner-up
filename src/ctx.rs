//! Go's context, for threads: a cancellation that reaches every context
//! derived from it, and an optional deadline.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Ctx(Arc<Inner>);

struct Inner {
    parent: Option<Ctx>,
    deadline: Option<Instant>,
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    cancelled: bool,
    children: Vec<Weak<Inner>>,
}

impl Ctx {
    /// A context that is never cancelled on its own, like context.Background.
    pub fn background() -> Ctx {
        Ctx::child(None, None)
    }

    fn child(parent: Option<&Ctx>, deadline: Option<Instant>) -> Ctx {
        let deadline = match (parent.and_then(|p| p.deadline()), deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let c = Ctx(Arc::new(Inner { parent: parent.cloned(), deadline, state: Mutex::default(), cv: Condvar::new() }));
        if let Some(p) = parent {
            let mut s = p.0.state.lock().unwrap();
            s.children.retain(|w| w.strong_count() > 0);
            s.children.push(Arc::downgrade(&c.0));
            if s.cancelled {
                drop(s);
                c.cancel();
            }
        }
        c
    }

    /// A context cancelled with this one, or by its own cancel.
    pub fn with_cancel(&self) -> Ctx {
        Ctx::child(Some(self), None)
    }

    /// A context that also ends after d.
    pub fn with_timeout(&self, d: Duration) -> Ctx {
        Ctx::child(Some(self), Some(Instant::now() + d))
    }

    /// context.WithoutCancel(ctx) followed by WithTimeout(d).
    pub fn detached_timeout(d: Duration) -> Ctx {
        Ctx::child(None, Some(Instant::now() + d))
    }

    pub fn cancel(&self) {
        let children = {
            let mut s = self.0.state.lock().unwrap();
            if s.cancelled {
                return;
            }
            s.cancelled = true;
            std::mem::take(&mut s.children)
        };
        self.0.cv.notify_all();
        for c in children.iter().filter_map(Weak::upgrade) {
            Ctx(c).cancel();
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.0.deadline
    }

    /// What is left until the deadline, if there is one.
    pub fn remaining(&self) -> Option<Duration> {
        self.0.deadline.map(|d| d.saturating_duration_since(Instant::now()))
    }

    pub fn is_done(&self) -> bool {
        self.0.state.lock().unwrap().cancelled || self.0.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// The error Go's ctx.Err() would give, when the context has ended.
    pub fn err(&self) -> Option<&'static str> {
        if self.0.state.lock().unwrap().cancelled {
            // A parent's deadline cancels its children through is_done only.
            return Some(if self.deadline_passed() { "context deadline exceeded" } else { "context canceled" });
        }
        if self.deadline_passed() {
            return Some("context deadline exceeded");
        }
        self.0.parent.as_ref().and_then(|p| p.err())
    }

    fn deadline_passed(&self) -> bool {
        self.0.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// Waits for d or until the context ends; true when it ended.
    pub fn sleep(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        let mut s = self.0.state.lock().unwrap();
        loop {
            if s.cancelled || self.deadline_passed() {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            let mut wake = until;
            if let Some(dl) = self.0.deadline {
                wake = wake.min(dl);
            }
            s = self.0.cv.wait_timeout(s, wake - now).unwrap().0;
        }
    }

    /// Waits until the context ends.
    pub fn wait(&self) {
        while !self.sleep(Duration::from_secs(3600)) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_reaches_children_and_deadlines_end() {
        let root = Ctx::background();
        let child = root.with_cancel();
        let grandchild = child.with_timeout(Duration::from_secs(60));
        assert!(!grandchild.is_done());
        root.cancel();
        assert!(grandchild.is_done());
        assert_eq!(grandchild.err(), Some("context canceled"));
        let late = root.with_cancel();
        assert!(late.is_done(), "a child of a cancelled context starts cancelled");

        let short = Ctx::background().with_timeout(Duration::from_millis(20));
        assert!(short.sleep(Duration::from_secs(5)));
        assert_eq!(short.err(), Some("context deadline exceeded"));
        assert!(!Ctx::background().sleep(Duration::from_millis(1)));
    }
}
