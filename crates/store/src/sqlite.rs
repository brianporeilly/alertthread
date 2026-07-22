//! The SQLite backend: the default, and exactly one replica by definition (ADR 001 D4).
//!
//! # Concurrency
//!
//! SQLite has no `FOR UPDATE SKIP LOCKED`. ADR 001 D2 records that this is sufficient
//! rather than a compromise, because SQLite mode is single-replica by definition, and that
//! `BEGIN IMMEDIATE` is the equivalent: it takes the database's write lock at the start of
//! the transaction instead of on the first write, so a read-then-write sequence cannot be
//! interleaved with another writer's.
//!
//! Single-*replica* is not single-*task*, though. One process runs many concurrent
//! ingests, and they contend on exactly the same rows two replicas would. So the
//! conformance suite races N tasks against one fingerprint here as well as against
//! PostgreSQL, and asserts the same observable outcome: one `post` op, one `alert_message`
//! row, one outbox row. The mechanism differs; the property does not.
//!
//! Worth being precise about what `BEGIN IMMEDIATE` is currently buying, because it is
//! easy to over-credit: every transaction below happens to *write* in its first statement,
//! and a deferred transaction takes the write lock there anyway. So on today's code the
//! keyword is redundant, and the conformance suite confirms that removing it changes no
//! outcome. It stays because ADR 001 D2 specifies it and because it makes the exclusion a
//! property of the transaction rather than of the order somebody happened to write the
//! statements in — a future refactor that reads before writing would otherwise turn a
//! correct claim into a `SQLITE_BUSY_SNAPSHOT` under load.
//!
//! # Pragmas that are load-bearing
//!
//! sqlx does not set `journal_mode` unless asked. WAL is not a tuning knob here — the
//! default rollback journal blocks readers behind a writer, and with the whole ingest path
//! inside `BEGIN IMMEDIATE` that is the difference between concurrent ingests and a queue.

use std::str::FromStr;

use alertthread_core::{
    AlertBatch, ChannelId, ClaimOutcome, ClaimResult, Fingerprint, GroupKey, GroupState, Intent,
    LabelMap, MessageTs, Op, Placement, Plan, ThreadTs, WebhookAlert,
};
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::types::Json;
use sqlx::{Sqlite, SqliteConnection, SqlitePool};

use crate::error::StoreError;
use crate::model::{
    AlertRecord, AlertState, ColumnDef, Deferral, GroupRecord, LeasedOp, OpEffect, OpId,
    PruneStats, RetentionPolicy, WorkerId,
};
use crate::payload::{OpKind, StoredOp, channel_of, fingerprint_of, group_key_of};
use crate::row::{
    AlertRow, ClaimProbeRow, GroupDelta, GroupRow, OutboxRow, ResolveClaimRow, leased,
    resolve_miss, stamp,
};
use crate::store::StateStore;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/sqlite");

// ---------------------------------------------------------------------------
// Statements
//
// Every one of these is a `&'static str` literal, which is what sqlx 0.9's
// `SqlSafeStr` accepts without an `AssertSqlSafe` escape hatch. Nothing here is
// assembled at runtime; where a query needed to vary, it was split into two
// literals rather than formatted.
//
// They are kept in the same order as their PostgreSQL counterparts in
// `postgres.rs` so the two files can be read side by side. That is the only
// defence against the two dialects drifting apart in ways the conformance
// suite cannot see.
// ---------------------------------------------------------------------------

/// ADR 001 D2's firing claim. The `ON CONFLICT DO NOTHING` is the atomic claim of D3.
const CLAIM_INSERT: &str = "\
INSERT INTO alert_message
    (fingerprint, channel, state, message_ts, thread_parent_ts, group_key,
     first_seen, last_seen, resolved_at, labels, annotations)
VALUES (?, ?, 'claimed', NULL, NULL, ?, ?, ?, NULL, ?, ?)
ON CONFLICT (fingerprint, channel) DO NOTHING";

/// What the conflicting row held, read under the transaction's write lock.
const CLAIM_PROBE: &str =
    "SELECT state, last_seen, message_ts FROM alert_message WHERE fingerprint = ? AND channel = ?";

/// A delivery for an alert already in flight: record that it is still being sent.
const CLAIM_TOUCH: &str = "\
UPDATE alert_message
SET last_seen = ?, group_key = ?, labels = ?, annotations = ?
WHERE fingerprint = ? AND channel = ?";

/// An alert that finished and has fired again: take the row back over.
const CLAIM_RECLAIM: &str = "\
UPDATE alert_message
SET state = 'claimed', message_ts = NULL, thread_parent_ts = NULL, resolved_at = NULL,
    group_key = ?, first_seen = ?, last_seen = ?, labels = ?, annotations = ?
WHERE fingerprint = ? AND channel = ?";

/// ADR 001 D2's resolve transition.
const MARK_RESOLVING: &str = "\
UPDATE alert_message
SET state = 'resolving', resolved_at = ?, last_seen = ?
WHERE fingerprint = ? AND channel = ? AND state IN ('claimed', 'posted')
RETURNING message_ts, thread_parent_ts";

const SELECT_ALERT: &str = "\
SELECT fingerprint, channel, state, message_ts, thread_parent_ts, group_key,
       first_seen, last_seen, resolved_at, labels, annotations
FROM alert_message WHERE fingerprint = ? AND channel = ?";

const SELECT_GROUP: &str = "\
SELECT group_key, channel, message_ts, member_count, group_labels, created_at
FROM group_message WHERE group_key = ? AND channel = ?";

/// Opens a storm-collapse group. `DO NOTHING` rather than an error: see `persist_group`.
const INSERT_GROUP: &str = "\
INSERT INTO group_message
    (group_key, channel, message_ts, member_count, group_labels, created_at)
VALUES (?, ?, NULL, ?, ?, ?)
ON CONFLICT (group_key, channel) DO NOTHING";

/// A later batch joining an existing group. `group_labels` is *not* in the SET list: see
/// `persist_group`.
const JOIN_GROUP: &str = "\
UPDATE group_message SET member_count = member_count + ? WHERE group_key = ? AND channel = ?";

const INSERT_OUTBOX: &str = "\
INSERT INTO outbox
    (op, channel, fingerprint, group_key, payload, attempts, next_attempt_at,
     leased_by, leased_until, last_error, created_at, dead_lettered_at)
VALUES (?, ?, ?, ?, ?, 0, ?, NULL, NULL, NULL, ?, NULL)";

/// ADR 001 D2's worker lease, without `SKIP LOCKED` — the transaction is `BEGIN IMMEDIATE`,
/// so no other writer is inside this statement at the same time.
const LEASE: &str = "\
UPDATE outbox
SET leased_by = ?, leased_until = ?, attempts = attempts + 1
WHERE id IN (
    SELECT id FROM outbox
    WHERE dead_lettered_at IS NULL
      AND next_attempt_at <= ?
      AND (leased_until IS NULL OR leased_until <= ?)
    ORDER BY id
    LIMIT ?
)
RETURNING id, payload, attempts, leased_until, created_at";

/// ADR 001 D4: "outbox rows completed successfully → deleted inline on completion".
const COMPLETE_DELETE: &str =
    "DELETE FROM outbox WHERE id = ? RETURNING op, channel, fingerprint, group_key";

const APPLY_POSTED: &str = "\
UPDATE alert_message
SET message_ts = ?, thread_parent_ts = ?,
    state = CASE WHEN state = 'claimed' THEN 'posted' ELSE state END
WHERE fingerprint = ? AND channel = ?";

const APPLY_GROUP_POSTED: &str =
    "UPDATE group_message SET message_ts = ? WHERE group_key = ? AND channel = ?";

const APPLY_RESOLVED: &str = "\
UPDATE alert_message
SET state = 'resolved', resolved_at = COALESCE(resolved_at, ?)
WHERE fingerprint = ? AND channel = ? AND state = 'resolving'";

const APPLY_MESSAGE_LOST: &str = "\
UPDATE alert_message
SET message_ts = NULL, thread_parent_ts = NULL, state = 'claimed'
WHERE fingerprint = ? AND channel = ?";

/// The parent's live count, so a re-posted summary does not come back saying zero.
const SELECT_GROUP_MEMBERS: &str =
    "SELECT member_count FROM group_message WHERE group_key = ? AND channel = ?";

const APPLY_GROUP_MESSAGE_LOST: &str =
    "UPDATE group_message SET message_ts = NULL WHERE group_key = ? AND channel = ?";

const DEFER_RATE_LIMITED: &str = "\
UPDATE outbox
SET leased_by = NULL, leased_until = NULL, next_attempt_at = ?,
    attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END
WHERE id = ?";

const DEFER_BACKOFF: &str = "\
UPDATE outbox
SET leased_by = NULL, leased_until = NULL, next_attempt_at = ?, last_error = ?
WHERE id = ?";

const DEAD_LETTER: &str = "\
UPDATE outbox
SET dead_lettered_at = ?, leased_by = NULL, leased_until = NULL, last_error = ?
WHERE id = ?
RETURNING op, channel, fingerprint";

const MARK_ALERT_FAILED: &str = "\
UPDATE alert_message SET state = 'failed'
WHERE fingerprint = ? AND channel = ? AND state IN ('claimed', 'posted')";

const PRUNE_RESOLVED: &str = "\
DELETE FROM alert_message
WHERE state = 'resolved' AND resolved_at IS NOT NULL AND resolved_at < ?
  AND NOT EXISTS (
      SELECT 1 FROM outbox o
      WHERE o.channel = alert_message.channel AND o.fingerprint = alert_message.fingerprint)";

const PRUNE_STALE: &str = "\
DELETE FROM alert_message
WHERE last_seen < ?
  AND NOT EXISTS (
      SELECT 1 FROM outbox o
      WHERE o.channel = alert_message.channel AND o.fingerprint = alert_message.fingerprint)";

const PRUNE_GROUPS: &str = "\
DELETE FROM group_message
WHERE NOT EXISTS (
      SELECT 1 FROM alert_message a
      WHERE a.channel = group_message.channel AND a.group_key = group_message.group_key)
  AND NOT EXISTS (
      SELECT 1 FROM outbox o
      WHERE o.channel = group_message.channel AND o.group_key = group_message.group_key)";

const DESCRIBE_TABLE: &str = "SELECT name, \"notnull\" FROM pragma_table_info(?)";

/// SQLite-backed [`StateStore`].
#[derive(Clone, Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens a store at a `sqlite:` URL, creating the database if it is not there.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the URL will not parse or the database cannot be opened.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str(url)?;
        Self::connect_with(PoolOptions::<Sqlite>::new(), options).await
    }

    /// Opens a store from explicit pool and connection options.
    ///
    /// The connection options are passed through [`Self::tune`] first, so a caller cannot
    /// accidentally get a store without WAL — including the conformance suite, which is
    /// handed raw options by `#[sqlx::test]` and would otherwise be testing a
    /// differently-configured database from the one that ships.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the database cannot be opened.
    pub async fn connect_with(
        pool: PoolOptions<Sqlite>,
        options: SqliteConnectOptions,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            pool: pool.connect_with(Self::tune(options)).await?,
        })
    }

    /// Wraps a pool that has already been configured.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for the health check in Phase 4.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The connection settings this backend requires.
    ///
    /// - **WAL.** sqlx leaves `journal_mode` alone by default. Without WAL a reader blocks
    ///   behind the writer, and every ingest here is a writer from its first statement.
    /// - **`synchronous = NORMAL`.** Safe under WAL: a crash cannot corrupt the database,
    ///   it can only lose the most recent commits — and a lost commit is an alert
    ///   Alertmanager has not been acknowledged for, so it is redelivered.
    /// - **A generous busy timeout.** `BEGIN IMMEDIATE` contends with every other ingest
    ///   in the process. Failing fast here would surface as a `503` under exactly the load
    ///   that produced the alerts.
    #[must_use]
    pub fn tune(options: SqliteConnectOptions) -> SqliteConnectOptions {
        options
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30))
            .foreign_keys(true)
    }
}

// ---------------------------------------------------------------------------
// Claims (ADR 001 D2, D3)
// ---------------------------------------------------------------------------

/// The firing claim: ADR 001 D2's `INSERT ... ON CONFLICT DO NOTHING`, plus the
/// classification of what a conflict means.
async fn claim_firing(
    conn: &mut SqliteConnection,
    alert: &WebhookAlert,
    channel: &ChannelId,
    group_key: &GroupKey,
    now: DateTime<Utc>,
) -> Result<ClaimResult, StoreError> {
    let inserted = sqlx::query(CLAIM_INSERT)
        .bind(alert.fingerprint.as_str())
        .bind(channel.as_str())
        .bind(group_key.as_str())
        .bind(now)
        .bind(now)
        .bind(Json(&alert.labels))
        .bind(Json(&alert.annotations))
        .execute(&mut *conn)
        .await?;

    if inserted.rows_affected() > 0 {
        return Ok(ClaimResult::Claimed);
    }

    // The insert conflicted, so a row exists. Read it under the write lock this
    // transaction already holds: nothing else can be between the two statements.
    let existing: Option<ClaimProbeRow> = sqlx::query_as(CLAIM_PROBE)
        .bind(alert.fingerprint.as_str())
        .bind(channel.as_str())
        .fetch_optional(&mut *conn)
        .await?;

    // Not reachable through the trait, and deliberately not deleted: it needs the pruner
    // to delete this exact row in the microseconds between the two statements above, and
    // the pruner refuses to touch a row with queued work. Left in, and left uncovered,
    // because the alternative is a `match` arm that assumes a row it has not seen.
    let Some(existing) = existing else {
        return Err(StoreError::ClaimVanished {
            fingerprint: alert.fingerprint.clone(),
            channel: channel.clone(),
        });
    };

    // Also unreachable through the trait — nothing writes a sixth state — and also kept.
    // Every query in this crate filters on this column, so a value outside the five means
    // rows are being skipped by predicates that look correct, and that has to be loud.
    let state =
        AlertState::parse(&existing.state).ok_or_else(|| StoreError::UnknownAlertState {
            fingerprint: alert.fingerprint.clone(),
            channel: channel.clone(),
            state: existing.state.clone(),
        })?;

    match state {
        // ADR 001 D2 stops here, and this is the case it does not cover: an alert that
        // resolved, or whose post dead-lettered, and is now firing again. Treating it as a
        // duplicate would be silence — the message it would be a duplicate of is either
        // green or was never sent. So the row is taken back over and the alert is posted
        // afresh, which is the same direction D3 resolves its unresolvable case in.
        AlertState::Resolved | AlertState::Failed => {
            sqlx::query(CLAIM_RECLAIM)
                .bind(group_key.as_str())
                .bind(now)
                .bind(now)
                .bind(Json(&alert.labels))
                .bind(Json(&alert.annotations))
                .bind(alert.fingerprint.as_str())
                .bind(channel.as_str())
                .execute(&mut *conn)
                .await?;
            Ok(ClaimResult::Claimed)
        }

        // ADR 001 D7's repeat: hand the planner the `last_seen` from *before* this
        // delivery, which is what the debounce compares against.
        AlertState::Posted => {
            let last_seen = existing.last_seen;
            let message_ts = existing.message_ts.map(MessageTs::new);
            touch(conn, alert, channel, group_key, now).await?;

            match message_ts {
                Some(message_ts) => Ok(ClaimResult::AlreadyPosted {
                    last_seen,
                    message_ts,
                }),
                // `posted` without a timestamp is a state the store cannot produce, so
                // rather than invent a refresh target this is treated as still in flight.
                // The alert has a message; nothing is dropped.
                None => Ok(ClaimResult::AlreadyClaimed),
            }
        }

        // Another replica, or a retried delivery, is already posting it. Also a firing
        // arriving while a resolution is in flight: the resolve op is already queued and
        // will finish, and the *next* delivery re-claims the row via the branch above.
        AlertState::Claimed | AlertState::Resolving => {
            touch(conn, alert, channel, group_key, now).await?;
            Ok(ClaimResult::AlreadyClaimed)
        }
    }
}

/// Records that a delivery for an alert already in flight arrived.
///
/// `last_seen` moving is what keeps the retention sweep honest about long-running alerts,
/// and the labels are refreshed because Alertmanager may have re-evaluated them.
async fn touch(
    conn: &mut SqliteConnection,
    alert: &WebhookAlert,
    channel: &ChannelId,
    group_key: &GroupKey,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    sqlx::query(CLAIM_TOUCH)
        .bind(now)
        .bind(group_key.as_str())
        .bind(Json(&alert.labels))
        .bind(Json(&alert.annotations))
        .bind(alert.fingerprint.as_str())
        .bind(channel.as_str())
        .execute(conn)
        .await?;
    Ok(())
}

/// The resolve transition: ADR 001 D2's `UPDATE ... RETURNING`, plus the orphan and
/// duplicate classifications of D9.
async fn mark_resolving(
    conn: &mut SqliteConnection,
    alert: &WebhookAlert,
    channel: &ChannelId,
    now: DateTime<Utc>,
) -> Result<ClaimResult, StoreError> {
    let claimed: Option<ResolveClaimRow> = sqlx::query_as(MARK_RESOLVING)
        .bind(now)
        .bind(now)
        .bind(alert.fingerprint.as_str())
        .bind(channel.as_str())
        .fetch_optional(&mut *conn)
        .await?;

    if let Some(row) = claimed {
        return Ok(ClaimResult::Resolving {
            message_ts: row.message_ts.map(MessageTs::new),
            thread_parent_ts: row.thread_parent_ts.map(ThreadTs::new),
        });
    }

    let existing: Option<ClaimProbeRow> = sqlx::query_as(CLAIM_PROBE)
        .bind(alert.fingerprint.as_str())
        .bind(channel.as_str())
        .fetch_optional(&mut *conn)
        .await?;

    resolve_miss(existing, &alert.fingerprint, channel)
}

// ---------------------------------------------------------------------------
// Persistence of a plan
// ---------------------------------------------------------------------------

/// Applies a plan's effect on `group_message` and reports whether this transaction is the
/// one that opened the group.
///
/// The return value matters under HA. Two replicas can each read no group, each plan a
/// `PostGroup`, and each try to open it; the loser's `PostGroup` is dropped rather than
/// enqueued, so a storm produces one summary message and not one per replica. Its children
/// are unaffected — they carry no parent timestamp and resolve it from the row the winner
/// wrote.
///
/// `group_labels` is bound on the insert and never on the join. They are what *defines* the
/// group and cannot change while it exists — a different `group_by` is a different group
/// key and therefore a different row — so rewriting them on every join would be work whose
/// only possible effect is to replace them with something wrong.
async fn persist_group(
    conn: &mut SqliteConnection,
    delta: &GroupDelta<'_>,
    channel: &ChannelId,
    group_labels: &LabelMap,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let Some(group_key) = delta.group else {
        return Ok(false);
    };

    if delta.opens {
        let inserted = sqlx::query(INSERT_GROUP)
            .bind(group_key.as_str())
            .bind(channel.as_str())
            .bind(delta.members)
            .bind(Json(group_labels))
            .bind(now)
            .execute(&mut *conn)
            .await?;

        if inserted.rows_affected() > 0 {
            return Ok(true);
        }
        // Somebody else opened it between our read and our write. Fall through and join
        // theirs; our `PostGroup` op is dropped by the caller.
        //
        // Unreachable on this backend, and correct to keep: `BEGIN IMMEDIATE` serialises
        // the two ingests, so the second one reads the group and never plans a `PostGroup`
        // in the first place. The PostgreSQL backend does reach it, and the conformance
        // suite covers it there — which is why the two backends are gated separately.
    }

    sqlx::query(JOIN_GROUP)
        .bind(delta.members)
        .bind(group_key.as_str())
        .bind(channel.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(false)
}

/// Writes one op to the outbox, ready to be leased immediately.
async fn enqueue(
    conn: &mut SqliteConnection,
    op: &Op,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let payload = StoredOp::from(op);
    sqlx::query(INSERT_OUTBOX)
        .bind(OpKind::of(op).as_str())
        .bind(channel_of(op).as_str())
        .bind(fingerprint_of(op).map(Fingerprint::as_str))
        .bind(group_key_of(op).map(GroupKey::as_str))
        .bind(Json(&payload))
        .bind(now)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

impl StateStore for SqliteStore {
    async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    async fn ingest<F>(
        &self,
        batch: &AlertBatch,
        now: DateTime<Utc>,
        decide: F,
    ) -> Result<Plan, StoreError>
    where
        F: FnOnce(&[ClaimOutcome], Option<&GroupState>) -> Plan + Send,
    {
        let now = stamp(now);
        // `BEGIN IMMEDIATE`, not `BEGIN`. A deferred transaction takes its write lock on
        // the first write, which here is *after* the claim's read — leaving room for
        // another task to claim the same fingerprint in between. This one word is ADR 001
        // D2's stated equivalent of `FOR UPDATE SKIP LOCKED`.
        let mut txn = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let mut outcomes = Vec::with_capacity(batch.alerts.len());
        for alert in &batch.alerts {
            let result = match alert.status.intent() {
                Intent::Firing => {
                    claim_firing(&mut txn, alert, &batch.channel, &batch.group_key, now).await?
                }
                Intent::Resolved => mark_resolving(&mut txn, alert, &batch.channel, now).await?,
            };
            outcomes.push(ClaimOutcome::new(alert.clone(), result));
        }

        let group: Option<GroupRow> = sqlx::query_as(SELECT_GROUP)
            .bind(batch.group_key.as_str())
            .bind(batch.channel.as_str())
            .fetch_optional(&mut *txn)
            .await?;
        let group = group.map(GroupRow::into_record);

        let plan = decide(&outcomes, group.as_ref().map(GroupRecord::state).as_ref());

        let delta = GroupDelta::of(&plan.ops);
        let opened =
            persist_group(&mut txn, &delta, &batch.channel, &batch.group_labels, now).await?;

        let mut persisted = Vec::with_capacity(plan.ops.len());
        for op in plan.ops {
            // Dropping a `PostGroup` we lost the race to open. Same as above: only the
            // PostgreSQL backend can get here, and it is covered there.
            if matches!(op, Op::PostGroup { .. }) && !opened {
                continue;
            }
            enqueue(&mut txn, &op, now).await?;
            persisted.push(op);
        }

        txn.commit().await?;

        Ok(Plan {
            ops: persisted,
            notices: plan.notices,
        })
    }

    async fn lease_batch(
        &self,
        worker: &WorkerId,
        limit: u32,
        lease: TimeDelta,
        now: DateTime<Utc>,
    ) -> Result<Vec<LeasedOp>, StoreError> {
        let now = stamp(now);
        let until = stamp(now + lease);

        let mut txn = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<OutboxRow> = sqlx::query_as(LEASE)
            .bind(worker.as_str())
            .bind(until)
            .bind(now)
            .bind(now)
            .bind(i64::from(limit))
            .fetch_all(&mut *txn)
            .await?;
        txn.commit().await?;

        leased(rows, until)
    }

    async fn complete(
        &self,
        id: OpId,
        effect: &OpEffect,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let now = stamp(now);
        let mut txn = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let subject: Option<(String, String, Option<String>, Option<String>)> =
            sqlx::query_as(COMPLETE_DELETE)
                .bind(id.get())
                .fetch_optional(&mut *txn)
                .await?;
        let Some((_, channel, fingerprint, group_key)) = subject else {
            return Err(StoreError::NoSuchOp(id));
        };

        match effect {
            OpEffect::Posted {
                message_ts,
                thread_parent_ts,
            } => {
                sqlx::query(APPLY_POSTED)
                    .bind(message_ts.as_str())
                    .bind(thread_parent_ts.as_ref().map(ThreadTs::as_str))
                    .bind(fingerprint)
                    .bind(&channel)
                    .execute(&mut *txn)
                    .await?;
            }
            OpEffect::GroupPosted { message_ts } => {
                sqlx::query(APPLY_GROUP_POSTED)
                    .bind(message_ts.as_str())
                    .bind(group_key)
                    .bind(&channel)
                    .execute(&mut *txn)
                    .await?;
            }
            OpEffect::Resolved => {
                sqlx::query(APPLY_RESOLVED)
                    .bind(now)
                    .bind(fingerprint)
                    .bind(&channel)
                    .execute(&mut *txn)
                    .await?;
            }
            OpEffect::MessageLost => {
                // ADR 001 D9: "clear `message_ts`, post a fresh message". The replacement
                // is enqueued *here*, in the same transaction that cleared the timestamp,
                // rather than by the caller afterwards. Two steps would leave a window in
                // which the relay has forgotten the message and has not queued another —
                // a crash there is a message nobody replaces. The decision was ADR 001's;
                // only its atomicity is the store's.
                //
                // Which row is being healed depends on what the op was addressed to. Every
                // op names either an alert or a group, so there is no third case.
                if let Some(fingerprint) = fingerprint {
                    sqlx::query(APPLY_MESSAGE_LOST)
                        .bind(&fingerprint)
                        .bind(&channel)
                        .execute(&mut *txn)
                        .await?;
                    // Top level, not back into the thread: `message_not_found` says our
                    // correlation state is stale, and any parent timestamp we remember
                    // came from the same state. A visible message beats a reply into a
                    // thread that may also be gone.
                    enqueue(
                        &mut txn,
                        &Op::Post {
                            fingerprint: Fingerprint::new(fingerprint),
                            channel: ChannelId::new(channel.clone()),
                            placement: Placement::Channel,
                        },
                        now,
                    )
                    .await?;
                } else if let Some(group_key) = group_key {
                    // The storm-collapse parent was deleted. Its children keep their own
                    // messages and are unaffected; what is gone is the summary, and doing
                    // nothing here would leave a group whose `message_ts` points at a
                    // message that no longer exists — so every later `RefreshGroup` would
                    // fail the same way, for ever.
                    let members: Option<(i32,)> = sqlx::query_as(SELECT_GROUP_MEMBERS)
                        .bind(&group_key)
                        .bind(&channel)
                        .fetch_optional(&mut *txn)
                        .await?;
                    sqlx::query(APPLY_GROUP_MESSAGE_LOST)
                        .bind(&group_key)
                        .bind(&channel)
                        .execute(&mut *txn)
                        .await?;
                    enqueue(
                        &mut txn,
                        &Op::PostGroup {
                            group_key: GroupKey::new(group_key),
                            channel: ChannelId::new(channel.clone()),
                            initial_members: members
                                .map_or(0, |(count,)| usize::try_from(count).unwrap_or(0)),
                        },
                        now,
                    )
                    .await?;
                }
            }
            OpEffect::Refreshed | OpEffect::Standalone => {}
        }

        txn.commit().await?;
        Ok(())
    }

    async fn defer(&self, id: OpId, deferral: &Deferral) -> Result<(), StoreError> {
        let affected = match deferral {
            Deferral::RateLimited { until } => sqlx::query(DEFER_RATE_LIMITED)
                .bind(stamp(*until))
                .bind(id.get())
                .execute(&self.pool)
                .await?
                .rows_affected(),
            Deferral::Backoff { until, error } => sqlx::query(DEFER_BACKOFF)
                .bind(stamp(*until))
                .bind(error.as_str())
                .bind(id.get())
                .execute(&self.pool)
                .await?
                .rows_affected(),
        };

        if affected == 0 {
            return Err(StoreError::NoSuchOp(id));
        }
        Ok(())
    }

    async fn dead_letter(
        &self,
        id: OpId,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let now = stamp(now);
        let mut txn = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let parked: Option<(String, String, Option<String>)> = sqlx::query_as(DEAD_LETTER)
            .bind(now)
            .bind(reason)
            .bind(id.get())
            .fetch_optional(&mut *txn)
            .await?;
        let Some((kind, channel, fingerprint)) = parked else {
            return Err(StoreError::NoSuchOp(id));
        };

        // Only a post failing means the alert never reached Slack at all. A refresh or a
        // resolve dead-lettering leaves a message in the channel that is merely stale.
        if kind == OpKind::Post.as_str()
            && let Some(fingerprint) = fingerprint
        {
            sqlx::query(MARK_ALERT_FAILED)
                .bind(fingerprint)
                .bind(&channel)
                .execute(&mut *txn)
                .await?;
        }

        txn.commit().await?;
        Ok(())
    }

    async fn prune(
        &self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
    ) -> Result<PruneStats, StoreError> {
        let now = stamp(now);
        let mut txn = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let resolved_alerts = sqlx::query(PRUNE_RESOLVED)
            .bind(now - policy.resolved_after)
            .execute(&mut *txn)
            .await?
            .rows_affected();

        let stale_alerts = sqlx::query(PRUNE_STALE)
            .bind(now - policy.stale_after)
            .execute(&mut *txn)
            .await?
            .rows_affected();

        let empty_groups = sqlx::query(PRUNE_GROUPS)
            .execute(&mut *txn)
            .await?
            .rows_affected();

        txn.commit().await?;

        Ok(PruneStats {
            resolved_alerts,
            stale_alerts,
            empty_groups,
        })
    }

    async fn alert(
        &self,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
    ) -> Result<Option<AlertRecord>, StoreError> {
        let row: Option<AlertRow> = sqlx::query_as(SELECT_ALERT)
            .bind(fingerprint.as_str())
            .bind(channel.as_str())
            .fetch_optional(&self.pool)
            .await?;

        row.map(AlertRow::into_record).transpose()
    }

    async fn group(
        &self,
        group_key: &GroupKey,
        channel: &ChannelId,
    ) -> Result<Option<GroupRecord>, StoreError> {
        let row: Option<GroupRow> = sqlx::query_as(SELECT_GROUP)
            .bind(group_key.as_str())
            .bind(channel.as_str())
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(GroupRow::into_record))
    }

    async fn describe_table(&self, table: &str) -> Result<Vec<ColumnDef>, StoreError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(DESCRIBE_TABLE)
            .bind(table)
            .fetch_all(&self.pool)
            .await?;

        let mut columns: Vec<ColumnDef> = rows
            .into_iter()
            .map(|(name, not_null)| ColumnDef {
                name,
                nullable: not_null == 0,
            })
            .collect();
        columns.sort();
        Ok(columns)
    }
}
