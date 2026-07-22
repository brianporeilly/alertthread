//! What can go wrong in the store, as a typed enum.
//!
//! Two rules from AGENTS.md shape this list. Library crates use `thiserror` with a typed
//! enum, and an error is never swallowed. The second is why several variants here describe
//! situations that "cannot happen": an outbox row whose payload will not decode, a
//! completion for an op that is no longer there. Each of those is a state the relay could
//! reach after a downgrade or a lost lease, and each one is reported rather than treated as
//! a no-op — a store call that quietly does nothing is how an alert goes missing.

use alertthread_core::{ChannelId, Fingerprint};
use thiserror::Error;

use crate::model::OpId;

/// A failure from the state store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The database driver failed: connection lost, constraint violated, disk full.
    ///
    /// ADR 001 D9 makes this the one case where refusing the request is correct. The
    /// handler returns `503` and Alertmanager retries, because Alertmanager's own retry is
    /// more durable than anything the relay could do while its store is unreachable.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A migration failed to apply, or the applied set does not match the shipped set.
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// An outbox row holds a payload this build cannot decode.
    ///
    /// Reported rather than skipped. A skipped row is an alert that never reaches Slack,
    /// and it would be skipped silently on every poll forever; an error is loud, and the
    /// row stays in the queue for a build that understands it.
    #[error("outbox row {id} holds a payload this build cannot decode: {source}")]
    UndecodableOp {
        /// The row that could not be decoded.
        id: OpId,
        /// What the decoder objected to.
        #[source]
        source: serde_json::Error,
    },

    /// An `alert_message` row holds a `state` value that is not one of the five the schema
    /// documents.
    #[error("alert_message row for {fingerprint} in {channel} holds unknown state {state:?}")]
    UnknownAlertState {
        /// The alert whose row is unreadable.
        fingerprint: Fingerprint,
        /// The channel it was routed to.
        channel: ChannelId,
        /// The value found in the column.
        state: String,
    },

    /// A completion, deferral or dead-letter names an outbox row that is no longer there.
    ///
    /// Reachable without a bug: a worker whose lease expired mid-post finishes, by which
    /// time another worker has already leased and completed the row. ADR 001 D3 enumerates
    /// that window and chooses duplicate-over-silence for it. Surfacing it as an error is
    /// what lets Phase 4 count it instead of guessing at it.
    #[error("outbox row {0} is no longer present")]
    NoSuchOp(OpId),

    /// The claim for a fingerprint conflicted with an existing row, and that row had been
    /// deleted by the time it was read back.
    ///
    /// Only the pruner deletes `alert_message` rows, and only rows with no queued work, so
    /// this needs a resolved alert to be pruned in the microseconds between an ingest's
    /// insert and its read-back. The claim cannot be retried safely from here — the
    /// transaction is already open — so it fails, the handler returns `503`, and
    /// Alertmanager redelivers. That is D9's store-unreachable row, reused for a store
    /// that is reachable but momentarily lying.
    #[error("the claim for {fingerprint} in {channel} conflicted with a row that then vanished")]
    ClaimVanished {
        /// The alert being claimed.
        fingerprint: Fingerprint,
        /// The channel it was routed to.
        channel: ChannelId,
    },

    /// An `outbox` row holds an `op` value that is not one of the six this build knows.
    ///
    /// Reported rather than folded into a neighbouring label. `alertthread_outbox_depth{op}`
    /// is how an operator sees what is stuck, and a gauge that quietly attributes work it
    /// cannot classify to `post` is worse than one that admits it does not know — the row
    /// would still be there, and the metric would say it was something else.
    #[error("outbox row holds unknown op kind {0:?}")]
    UnknownOpKind(String),

    /// `STATE_BACKEND` (or `storage.backend`) named something that is not a backend, or
    /// named one this binary was not compiled with.
    #[error(
        "unknown storage backend {0:?}: expected \"sqlite\" or \"postgres\" (and the \
         matching cargo feature must be enabled)"
    )]
    UnknownBackend(String),
}
