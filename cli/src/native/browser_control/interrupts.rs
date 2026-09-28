//! Interruptions of the agent's work that a person, the daemon or a caller
//! raises.
//!
//! A person taking control registers one before waiting for command custody:
//! the agent's paced input stops at its next pointer sample or key, a running
//! Playwright program stops at once, and neither starts while one is pending.
//! Raising or reading one never waits for the gate paced input holds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterruptReason {
    HumanControl,
    CallerDisconnected,
    Shutdown,
}

/// The daemon's pending interruptions, shared by every owner of agent work.
#[derive(Clone)]
pub(crate) struct Interrupts(Arc<Pending>);

struct Pending {
    reasons: Mutex<Vec<InterruptReason>>,
    /// Counts raises, so a sleeper wakes the moment one arrives.
    raised: watch::Sender<u64>,
    /// Counts a person's takeovers. A takeover leaves the pending list once
    /// its request holds command custody, so work that waits without custody
    /// learns of one from this count instead.
    takeovers: AtomicU64,
}

impl Default for Interrupts {
    fn default() -> Self {
        Self(Arc::new(Pending {
            reasons: Mutex::new(Vec::new()),
            raised: watch::channel(0).0,
            takeovers: AtomicU64::new(0),
        }))
    }
}

/// Keeps one interruption pending until dropped.
pub(crate) struct Interruption {
    interrupts: Interrupts,
    reason: InterruptReason,
}

impl Drop for Interruption {
    fn drop(&mut self) {
        let mut reasons = self.interrupts.reasons();
        if let Some(index) = reasons.iter().position(|reason| *reason == self.reason) {
            reasons.swap_remove(index);
        }
    }
}

impl Interrupts {
    fn reasons(&self) -> std::sync::MutexGuard<'_, Vec<InterruptReason>> {
        self.0
            .reasons
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn raise(&self, reason: InterruptReason) -> Interruption {
        self.reasons().push(reason);
        if reason == InterruptReason::HumanControl {
            self.0.takeovers.fetch_add(1, Ordering::SeqCst);
        }
        self.0
            .raised
            .send_modify(|count| *count = count.wrapping_add(1));
        Interruption {
            interrupts: self.clone(),
            reason,
        }
    }

    /// The pending interruption agent work obeys: a person's takeover before
    /// any other, so work it stops reports that takeover.
    pub(crate) fn pending(&self) -> Option<InterruptReason> {
        let reasons = self.reasons();
        reasons
            .iter()
            .copied()
            .find(|reason| *reason == InterruptReason::HumanControl)
            .or_else(|| reasons.first().copied())
    }

    /// Whether a person is taking control: from this moment custody is
    /// theirs, before their request holds command custody.
    pub(crate) fn takeover(&self) -> bool {
        self.pending() == Some(InterruptReason::HumanControl)
    }

    /// Wakes whoever waits on it when an interruption is raised.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.raised.subscribe()
    }

    /// How many takeovers were ever raised. Work that waits without command
    /// custody compares it with the count it began with.
    pub(crate) fn takeovers(&self) -> u64 {
        self.0.takeovers.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_takeover_outranks_other_interruptions_until_its_guard_drops() {
        let interrupts = Interrupts::default();
        let mut raised = interrupts.subscribe();
        assert_eq!(interrupts.pending(), None);
        let shutdown = interrupts.raise(InterruptReason::Shutdown);
        raised.changed().await.unwrap();
        assert_eq!(interrupts.pending(), Some(InterruptReason::Shutdown));
        assert!(!interrupts.takeover());
        let takeover = interrupts.clone().raise(InterruptReason::HumanControl);
        raised.changed().await.unwrap();
        assert_eq!(interrupts.pending(), Some(InterruptReason::HumanControl));
        assert!(interrupts.takeover());
        drop(takeover);
        assert_eq!(interrupts.pending(), Some(InterruptReason::Shutdown));
        drop(shutdown);
        assert_eq!(interrupts.pending(), None);
    }

    /// Two takeovers pending at once each keep custody the person's until
    /// both guards drop.
    #[test]
    fn each_guard_removes_only_its_own_interruption() {
        let interrupts = Interrupts::default();
        let first = interrupts.raise(InterruptReason::HumanControl);
        let second = interrupts.raise(InterruptReason::HumanControl);
        drop(first);
        assert!(interrupts.takeover());
        drop(second);
        assert!(!interrupts.takeover());
    }

    /// A takeover's request leaves the pending list once it holds command
    /// custody; the count still says it happened.
    #[test]
    fn a_takeover_is_counted_after_its_guard_drops() {
        let interrupts = Interrupts::default();
        let before = interrupts.takeovers();
        drop(interrupts.raise(InterruptReason::Shutdown));
        assert_eq!(interrupts.takeovers(), before);
        drop(interrupts.raise(InterruptReason::HumanControl));
        assert!(!interrupts.takeover());
        assert_eq!(interrupts.takeovers(), before + 1);
    }
}
