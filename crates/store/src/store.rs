//! The [`StateStore`] trait: the one I/O seam between the relay and its correlation state.

use alertthread_core::{
    AlertBatch, ChannelId, ClaimOutcome, Fingerprint, GroupKey, GroupState, Plan,
};
use chrono::{DateTime, TimeDelta, Utc};
use std::future::Future;

use crate::error::StoreError;
use crate::model::{
    AlertRecord, ColumnDef, Deferral, GroupMembership, GroupRecord, LeasedOp, OpEffect, OpId,
    PruneStats, RetentionPolicy, StoreStats, WorkerId,
};

/// Everything the relay does to its correlation and delivery state.
///
/// # Why the futures are spelled out
///
/// Native `async fn` in traits is stable on this toolchain, and this trait deliberately
/// does not use it. Two reasons, in order of importance:
///
/// 1. An `async fn` in a trait returns a future with no `Send` bound, so nothing generic
///    over `StateStore` can be handed to `tokio::spawn`. The outbox worker of Phase 4 is
///    exactly that. Writing `-> impl Future<Output = …> + Send` states the bound once, here,
///    instead of pushing return-type notation onto every call site that spawns.
/// 2. It keeps the trait honest about not being `dyn`-compatible. It is not, and it does
///    not need to be — see [`Store`](crate::Store) for how the backend is selected at
///    runtime without `Arc<dyn StateStore>`, which AGENTS.md names as a design smell.
///
/// Implementations still write plain `async fn`; the bound is checked against them.
pub trait StateStore: Send + Sync {
    /// Applies the migrations this build ships for this backend.
    ///
    /// Idempotent, and safe to call on every start. It is also the only thing that ever
    /// creates the schema: there is no "create tables if missing" path anywhere else, so
    /// the schema under test is always the schema that ships.
    ///
    /// # Errors
    ///
    /// [`StoreError::Migrate`] if a migration fails or if the applied set has diverged from
    /// the shipped set; [`StoreError::Database`] if the store is unreachable.
    fn migrate(&self) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Runs one webhook delivery end to end, in a single transaction.
    ///
    /// This is ADR 001 D2's ingest sequence, and it is one method rather than the separate
    /// `claim_firing` / `mark_resolving` / `enqueue` of D4's trait sketch because the
    /// atomicity is the *whole* point. A crash between committing the claims and committing
    /// the ops would leave a fingerprint marked as owned by a notification that no outbox
    /// row will ever produce — a claimed alert nobody posts, which is silence, which is the
    /// one outcome this project does not accept. Splitting the sequence across three trait
    /// methods makes that crash window a caller's responsibility to avoid; folding it into
    /// one method makes it unrepresentable.
    ///
    /// The order inside the transaction is:
    ///
    /// 1. claim every alert in `batch`, in order, producing one [`ClaimOutcome`] each;
    /// 2. read the storm-collapse parent for this `(group_key, channel)`, if any;
    /// 3. call `decide` — this is where [`plan`](alertthread_core::plan) runs;
    /// 4. persist the resulting ops, and any group row they imply;
    /// 5. commit.
    ///
    /// `decide` is a closure rather than a `Plan` argument because steps 1 and 2 produce
    /// its inputs and step 4 consumes its output, all inside the same transaction. It must
    /// not perform I/O — it is called with a database transaction open, and in SQLite that
    /// transaction holds the write lock.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the transaction cannot be completed;
    /// [`StoreError::ClaimVanished`] in the narrow race described on that variant;
    /// [`StoreError::UnknownAlertState`] if a row holds a `state` outside the schema's five.
    /// Every one of these leaves the transaction rolled back, so the caller returns `503`
    /// and Alertmanager redelivers (ADR 001 D9).
    fn ingest<F>(
        &self,
        batch: &AlertBatch,
        now: DateTime<Utc>,
        decide: F,
    ) -> impl Future<Output = Result<Plan, StoreError>> + Send
    where
        F: FnOnce(&[ClaimOutcome], Option<&GroupState>) -> Plan + Send;

    /// Claims up to `limit` ready outbox rows for `worker` until `now + lease`.
    ///
    /// ADR 001 D2's worker lease. A row is ready when its `next_attempt_at` has passed, it
    /// has not been dead-lettered, and either nobody holds it or the holder's lease has
    /// expired — which is what makes a crashed worker's rows reclaimable rather than stuck.
    ///
    /// `attempts` is incremented by the lease itself, not by a later failure. A worker that
    /// dies without reporting anything has still consumed an attempt, so a row that kills
    /// its worker every time still reaches the dead-letter queue instead of cycling for ever.
    ///
    /// `now` is a parameter rather than a `now()` call inside the SQL — a divergence from
    /// D2's sketch. It is what makes lease expiry testable without sleeping, and it keeps
    /// the store's notion of time the same as the core's.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`]; [`StoreError::UndecodableOp`] if a leased row holds a
    /// payload this build cannot read.
    fn lease_batch(
        &self,
        worker: &WorkerId,
        limit: u32,
        lease: TimeDelta,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<LeasedOp>, StoreError>> + Send;

    /// Records what a worker did with an op and removes it from the queue.
    ///
    /// The correlation-state update and the delete happen in one transaction, so the
    /// `message_ts` a resolve will need is durable before the row that would have produced
    /// it again is gone.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchOp`] if the row is already gone — a real outcome, not a bug:
    /// see the variant. [`StoreError::Database`] otherwise.
    fn complete(
        &self,
        id: OpId,
        effect: &OpEffect,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Releases an op's lease and schedules it for another attempt.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchOp`] if the row is already gone; [`StoreError::Database`].
    fn defer(
        &self,
        id: OpId,
        deferral: &Deferral,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Parks an op that has run out of attempts (ADR 001 D9).
    ///
    /// The row stops being leasable but is not deleted: its payload is the only record of
    /// an alert that never reached Slack, and deleting it would erase the evidence of the
    /// one failure mode this project treats as unacceptable.
    ///
    /// If the parked op was a post — the alert's *first* message — the alert is marked
    /// [`AlertState::Failed`](crate::AlertState). That is what makes its eventual
    /// resolution arrive as an orphan and post something, instead of being mistaken for a
    /// duplicate resolution of a message that was never sent.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchOp`] if the row is already gone; [`StoreError::Database`].
    fn dead_letter(
        &self,
        id: OpId,
        reason: &str,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Deletes finished state (ADR 001 D4, retention; PRD §5.7).
    ///
    /// Never deletes a row that still has queued work, whatever the policy says. An
    /// `alert_message` deleted while its post op is in flight would be posted and then be
    /// untracked, turning its eventual resolution into an orphan; a `group_message` deleted
    /// while its parent post is queued would leave the parent's timestamp nowhere to land.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    fn prune(
        &self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<PruneStats, StoreError>> + Send;

    /// Reads one alert's correlation state.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`]; [`StoreError::UnknownAlertState`] if the row's `state`
    /// column holds something outside the schema's five values.
    fn alert(
        &self,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
    ) -> impl Future<Output = Result<Option<AlertRecord>, StoreError>> + Send;

    /// Reads one storm-collapse parent.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    fn group(
        &self,
        group_key: &GroupKey,
        channel: &ChannelId,
    ) -> impl Future<Output = Result<Option<GroupRecord>, StoreError>> + Send;

    /// Counts a storm-collapse group's members by whether they have resolved (ADR 001 D5).
    ///
    /// The summary message shows a live firing/resolved count, and this is where it comes
    /// from. It is counted from `alert_message` rather than read off
    /// [`GroupRecord::member_count`], which only ever grows: a parent that kept saying
    /// "9 of 15 firing" over a thread of green replies would be confidently wrong, and the
    /// renderer already treats that as worse than uninformative.
    ///
    /// A group with no surviving members reports zero of each rather than an error. That is
    /// a real state — the pruner deletes resolved alerts before it deletes their parent —
    /// and the caller renders the count it is given.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    fn group_membership(
        &self,
        group_key: &GroupKey,
        channel: &ChannelId,
    ) -> impl Future<Output = Result<GroupMembership, StoreError>> + Send;

    /// Samples everything ADR 001 D11 reports about the queue and the correlation state.
    ///
    /// One call rather than four, because a metrics sample should describe one moment: four
    /// separate queries would let the depth and the oldest age come from different instants,
    /// and the pair "depth 0, oldest age 40 s" is the kind of contradiction that costs
    /// somebody twenty minutes at 3am.
    ///
    /// Called on a background interval, never from `GET /metrics`. A scrape every 15 s
    /// across N replicas would otherwise point Prometheus at the outbox as a load generator,
    /// and a slow store would time the scrape out and hide every other metric with it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    fn stats(&self) -> impl Future<Output = Result<StoreStats, StoreError>> + Send;

    /// Reports the columns of `table`, sorted by name.
    ///
    /// This is on the trait, and not tucked into a test helper, because it is the only
    /// mechanism that polices the cost ADR 001 D4 accepted. Two migration directories will
    /// drift; the conformance suite catches it by asking both backends what they actually
    /// built and asserting the answers match. A helper that lived in the test tree would be
    /// two helpers, one per backend, which is the same drift one layer up.
    ///
    /// Returns an empty vector for a table that does not exist, which is itself the
    /// assertion when a migration forgets a table entirely.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    fn describe_table(
        &self,
        table: &str,
    ) -> impl Future<Output = Result<Vec<ColumnDef>, StoreError>> + Send;
}
