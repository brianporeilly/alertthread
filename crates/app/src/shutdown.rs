//! Telling every task to stop, once.
//!
//! # Why this is not `tokio-util`
//!
//! `tokio_util::sync::CancellationToken` does exactly this, and more. It is also a
//! dependency this project does not otherwise carry, and AGENTS.md asks before adding one.
//! What a shutdown actually needs is "has the flag been set?" and "wake me when it is",
//! which is a `tokio::sync::watch` channel — already in the dependency graph, and about
//! forty lines including the two behaviours that are easy to get wrong:
//!
//! - **Cancelling twice is fine.** `SIGTERM` followed by `SIGINT` is an operator being
//!   impatient, not an error, and a shutdown path that panicked on the second signal would
//!   turn impatience into a crash mid-delivery.
//! - **A token whose source is gone reads as cancelled.** Otherwise a background task would
//!   outlive the thing that was supposed to stop it, and the process would never exit —
//!   which Kubernetes resolves with `SIGKILL`, mid-post, which is the one moment worth
//!   avoiding.

use tokio::sync::watch;

/// A flag several tasks watch for shutdown.
#[derive(Clone, Debug)]
pub struct CancelToken {
    rx: watch::Receiver<bool>,
}

/// The other end of a [`CancelToken`].
#[derive(Debug)]
pub struct CancelSource {
    tx: watch::Sender<bool>,
}

impl CancelSource {
    /// Signals every watcher to stop.
    ///
    /// Idempotent, and safe after every watcher has gone: a shutdown that failed
    /// because nothing was listening would be a process that will not exit.
    pub fn cancel(&self) {
        let _outcome: Result<(), watch::error::SendError<bool>> = self.tx.send(true);
    }
}

/// A fresh, uncancelled token and its source.
#[must_use]
pub fn cancellation() -> (CancelSource, CancelToken) {
    let (tx, rx) = watch::channel(false);
    (CancelSource { tx }, CancelToken { rx })
}

impl CancelToken {
    /// Whether shutdown has been signalled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves when shutdown is signalled, immediately if it already has been.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                // Every sender is gone. Treated as cancelled: the alternative is a
                // background task that outlives the thing that was supposed to stop it.
                return;
            }
        }
    }
}
