//! Persistence for `alertthread`.
//!
//! This crate owns the [`StateStore`] trait (ADR 001 D4) and its two backends: SQLite by
//! default, PostgreSQL opt-in for horizontal scaling. Both are exercised by a single shared
//! conformance suite in `tests/conformance.rs`, so the HA path is continuously verified
//! rather than theoretical — which is the reason ADR 001 gives for building both now:
//! *"a store abstraction with only one implementation is an abstraction that is wrong in
//! ways nobody has discovered yet."*
//!
//! # Where the atomicity lives
//!
//! The claim on `(fingerprint, channel)` is here rather than in `alertthread-core` because
//! its correctness *is* the database's atomicity and cannot be made pure. What that means
//! in practice is [`StateStore::ingest`]: one transaction that claims every alert in a
//! delivery, hands the outcomes to [`plan`](alertthread_core::plan), persists what it
//! decided, and commits. ADR 001 D2's durable-write-before-ack is that single commit.
//!
//! ```text
//!          ┌───────────────────────── one transaction ─────────────────────────┐
//! batch ──▶│ claim each alert ──▶ read group ──▶ plan(…) ──▶ enqueue ops       │──▶ commit
//!          └───────────────────────────────────────────────────────────────────┘
//!                                        ▲
//!                          the only pure step, and the only
//!                          one that makes a decision
//! ```
//!
//! # Choosing a backend
//!
//! [`Store`] is an enum over the two concrete backends, not `Arc<dyn StateStore>`. See its
//! module documentation for why; the short version is that the set of backends is closed,
//! `async fn` in traits is not `dyn`-compatible, and AGENTS.md names `Arc<dyn Trait>` as a
//! design smell in this codebase.
//!
//! # What is *not* here
//!
//! No decisions. The store executes ADR 001's classification rules and returns facts; every
//! question of the form "given this state, what should we do?" is answered by
//! [`plan`](alertthread_core::plan). The two places this crate comes closest to that line —
//! re-claiming a row for an alert that has fired again, and enqueuing the replacement post
//! after `message_not_found` — are both cases where ADR 001 already took the decision and
//! only its *atomicity* belongs to the store. Both say so at the call site.

mod dispatch;
mod error;
mod model;
mod payload;
mod row;
mod store;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use dispatch::{Backend, Store};
pub use error::StoreError;
pub use model::{
    AlertRecord, AlertState, ColumnDef, DeadLetter, Deferral, GroupMembership, GroupRecord,
    LeasedOp, OpEffect, OpId, PruneStats, RetentionPolicy, StoreStats, WorkerId,
};
pub use payload::OpKind;
pub use store::StateStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

/// The version of this crate, as recorded in its `Cargo.toml`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_the_compiled_in_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn core_is_linked_and_reports_the_same_workspace_version() {
        // The workspace versions every crate together; a mismatch here means a
        // crate was published or bumped out of step with the rest.
        assert_eq!(version(), alertthread_core::version());
    }
}
