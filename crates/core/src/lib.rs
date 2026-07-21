//! Pure decision logic for `alertthread`.
//!
//! This crate holds every correctness decision in the project and performs no I/O of any
//! kind. It has no async runtime, no database driver and no HTTP client, and it cannot
//! read a clock — `chrono` is depended on without its `clock` feature, so `Utc::now()`
//! does not exist here. Time arrives as a `now:` parameter. That is what lets this logic
//! be tested exhaustively with plain function calls and no mocks, which in turn is why
//! this crate is the one held to 100% line coverage with no surviving mutants.
//!
//! # The shape
//!
//! ```text
//!   webhook body ──serde──▶ WebhookPayload ──+ channel──▶ AlertBatch
//!                                                              │
//!   shell: atomic claims in one transaction ──▶ [ClaimOutcome] │
//!   shell: look up the group_message row     ──▶ GroupState    │
//!                                                              ▼
//!                                           plan(…) ──▶ Plan { ops, notices }
//!                                                              │
//!   shell: persist ops in the same transaction, commit, 200 ◀───┘
//! ```
//!
//! The claim cannot be pure — its correctness *is* the database's atomicity (ADR 001 D3)
//! — so the shell performs it first and feeds the results in. Everything after that is
//! [`plan`], and everything before it is parsing.
//!
//! # Where the decisions live
//!
//! | ADR 001 | What | Here |
//! |---|---|---|
//! | D2 | Ingest classification | [`ClaimResult`] and [`plan`] |
//! | D3 | Idempotency under concurrency | [`ClaimResult::Claimed`] against [`ClaimResult::AlreadyClaimed`] |
//! | D5 | Storm collapse, and its stickiness | [`GroupState`], [`Placement`] |
//! | D6 | Resolve behaviour | [`Op::Resolve`] |
//! | D7 | Repeat-firing debounce | [`Policy::refresh_debounce`] |
//! | D8 | Truncated payload detection | [`Notice::AlertsTruncated`] |
//! | D9 | Orphan and deferred resolves | [`Op::PostOrphanResolved`], [`ResolveTarget::AwaitingPost`] |

mod domain;
mod ids;
mod plan;
mod policy;
mod webhook;

pub use domain::{
    AlertBatch, ClaimOutcome, ClaimResult, GroupState, Notice, Op, Placement, Plan, ResolveTarget,
};
pub use ids::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};
pub use plan::plan;
pub use policy::{Policy, PolicyError};
pub use webhook::{AlertStatus, Intent, LabelMap, WebhookAlert, WebhookPayload};

/// The version of this crate, as recorded in its `Cargo.toml`.
///
/// Exposed so the binary can report the version of the core it was built against, which
/// matters when a deployed image and a source tree disagree.
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
    fn version_is_not_empty() {
        assert!(!version().is_empty(), "version string must never be empty");
    }
}
