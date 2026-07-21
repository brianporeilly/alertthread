//! The PostgreSQL backend: opt-in, and what makes more than one replica legal (ADR 001 D4).
//!
//! # Concurrency
//!
//! Every place `sqlite.rs` relies on `BEGIN IMMEDIATE` holding the database's write lock,
//! this file names the rows it needs instead:
//!
//! - the firing claim reads its conflicting row `FOR UPDATE`, so the read-then-write that
//!   classifies a repeat cannot interleave with another replica's;
//! - the lease uses `FOR UPDATE SKIP LOCKED`, the standard multi-consumer queue pattern,
//!   so N replicas draining the outbox hand out disjoint work without coordinating.
//!
//! The statements below are kept in the same order as their SQLite counterparts, and the
//! functions have the same names. Reading the two files side by side is the intended way to
//! check that the dialects have not drifted; the conformance suite is what proves it.

use std::str::FromStr;

use alertthread_core::{
    AlertBatch, ChannelId, ClaimOutcome, ClaimResult, Fingerprint, GroupKey, GroupState, Intent,
    MessageTs, Op, Placement, Plan, ThreadTs, WebhookAlert,
};
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::pool::PoolOptions;
use sqlx::postgres::PgConnectOptions;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Postgres};

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

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/postgres");

// ---------------------------------------------------------------------------
// Statements — same order as sqlite.rs
// ---------------------------------------------------------------------------

const CLAIM_INSERT: &str = "\
INSERT INTO alert_message
    (fingerprint, channel, state, message_ts, thread_parent_ts, group_key,
     first_seen, last_seen, resolved_at, labels, annotations)
VALUES ($1, $2, 'claimed', NULL, NULL, $3, $4, $5, NULL, $6, $7)
ON CONFLICT (fingerprint, channel) DO NOTHING";

/// `FOR UPDATE`, which is what SQLite gets for free from `BEGIN IMMEDIATE`. Without it a
/// second replica could read the same `posted` row and both enqueue a refresh.
const CLAIM_PROBE_LOCKED: &str = "\
SELECT state, last_seen, message_ts FROM alert_message
WHERE fingerprint = $1 AND channel = $2
FOR UPDATE";

/// Unlocked, for the resolve path: the `UPDATE` there has already run and matched nothing,
/// so this only asks whether a row exists at all. Locking would make a resolution wait on
/// an unrelated in-flight claim for the sake of an answer that does not change.
const CLAIM_PROBE: &str = "SELECT state, last_seen, message_ts FROM alert_message WHERE fingerprint = $1 AND channel = $2";

const CLAIM_TOUCH: &str = "\
UPDATE alert_message
SET last_seen = $1, group_key = $2, labels = $3, annotations = $4
WHERE fingerprint = $5 AND channel = $6";

const CLAIM_RECLAIM: &str = "\
UPDATE alert_message
SET state = 'claimed', message_ts = NULL, thread_parent_ts = NULL, resolved_at = NULL,
    group_key = $1, first_seen = $2, last_seen = $3, labels = $4, annotations = $5
WHERE fingerprint = $6 AND channel = $7";

const MARK_RESOLVING: &str = "\
UPDATE alert_message
SET state = 'resolving', resolved_at = $1, last_seen = $2
WHERE fingerprint = $3 AND channel = $4 AND state IN ('claimed', 'posted')
RETURNING message_ts, thread_parent_ts";

const SELECT_ALERT: &str = "\
SELECT fingerprint, channel, state, message_ts, thread_parent_ts, group_key,
       first_seen, last_seen, resolved_at, labels, annotations
FROM alert_message WHERE fingerprint = $1 AND channel = $2";

const SELECT_GROUP: &str = "\
SELECT group_key, channel, message_ts, member_count, created_at
FROM group_message WHERE group_key = $1 AND channel = $2";

const INSERT_GROUP: &str = "\
INSERT INTO group_message (group_key, channel, message_ts, member_count, created_at)
VALUES ($1, $2, NULL, $3, $4)
ON CONFLICT (group_key, channel) DO NOTHING";

const JOIN_GROUP: &str = "\
UPDATE group_message SET member_count = member_count + $1 WHERE group_key = $2 AND channel = $3";

const INSERT_OUTBOX: &str = "\
INSERT INTO outbox
    (op, channel, fingerprint, group_key, payload, attempts, next_attempt_at,
     leased_by, leased_until, last_error, created_at, dead_lettered_at)
VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, NULL, NULL, $7, NULL)";

/// ADR 001 D2's lease, verbatim in intent: `FOR UPDATE SKIP LOCKED` is the whole reason
/// this backend scales past one replica.
const LEASE: &str = "\
UPDATE outbox
SET leased_by = $1, leased_until = $2, attempts = attempts + 1
WHERE id IN (
    SELECT id FROM outbox
    WHERE dead_lettered_at IS NULL
      AND next_attempt_at <= $3
      AND (leased_until IS NULL OR leased_until <= $4)
    ORDER BY id
    LIMIT $5
    FOR UPDATE SKIP LOCKED
)
RETURNING id, payload, attempts, leased_until, created_at";

const COMPLETE_DELETE: &str =
    "DELETE FROM outbox WHERE id = $1 RETURNING op, channel, fingerprint, group_key";

const APPLY_POSTED: &str = "\
UPDATE alert_message
SET message_ts = $1, thread_parent_ts = $2,
    state = CASE WHEN state = 'claimed' THEN 'posted' ELSE state END
WHERE fingerprint = $3 AND channel = $4";

const APPLY_GROUP_POSTED: &str =
    "UPDATE group_message SET message_ts = $1 WHERE group_key = $2 AND channel = $3";

const APPLY_RESOLVED: &str = "\
UPDATE alert_message
SET state = 'resolved', resolved_at = COALESCE(resolved_at, $1)
WHERE fingerprint = $2 AND channel = $3 AND state = 'resolving'";

const APPLY_MESSAGE_LOST: &str = "\
UPDATE alert_message
SET message_ts = NULL, thread_parent_ts = NULL, state = 'claimed'
WHERE fingerprint = $1 AND channel = $2";

/// The parent's live count, so a re-posted summary does not come back saying zero.
const SELECT_GROUP_MEMBERS: &str =
    "SELECT member_count FROM group_message WHERE group_key = $1 AND channel = $2";

const APPLY_GROUP_MESSAGE_LOST: &str =
    "UPDATE group_message SET message_ts = NULL WHERE group_key = $1 AND channel = $2";

const DEFER_RATE_LIMITED: &str = "\
UPDATE outbox
SET leased_by = NULL, leased_until = NULL, next_attempt_at = $1,
    attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END
WHERE id = $2";

const DEFER_BACKOFF: &str = "\
UPDATE outbox
SET leased_by = NULL, leased_until = NULL, next_attempt_at = $1, last_error = $2
WHERE id = $3";

const DEAD_LETTER: &str = "\
UPDATE outbox
SET dead_lettered_at = $1, leased_by = NULL, leased_until = NULL, last_error = $2
WHERE id = $3
RETURNING op, channel, fingerprint";

const MARK_ALERT_FAILED: &str = "\
UPDATE alert_message SET state = 'failed'
WHERE fingerprint = $1 AND channel = $2 AND state IN ('claimed', 'posted')";

const PRUNE_RESOLVED: &str = "\
DELETE FROM alert_message
WHERE state = 'resolved' AND resolved_at IS NOT NULL AND resolved_at < $1
  AND NOT EXISTS (
      SELECT 1 FROM outbox o
      WHERE o.channel = alert_message.channel AND o.fingerprint = alert_message.fingerprint)";

const PRUNE_STALE: &str = "\
DELETE FROM alert_message
WHERE last_seen < $1
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

const DESCRIBE_TABLE: &str = "\
SELECT column_name, is_nullable = 'YES'
FROM information_schema.columns
WHERE table_schema = current_schema() AND table_name = $1";

/// PostgreSQL-backed [`StateStore`].
#[derive(Clone, Debug)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Opens a store at a `postgres:` URL.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the URL will not parse or the server is unreachable.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(url)?;
        Self::connect_with(PoolOptions::<Postgres>::new(), options).await
    }

    /// Opens a store from explicit pool and connection options.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the server is unreachable.
    pub async fn connect_with(
        pool: PoolOptions<Postgres>,
        options: PgConnectOptions,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            pool: pool.connect_with(options).await?,
        })
    }

    /// Wraps a pool that has already been configured.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for the health check in Phase 4.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ---------------------------------------------------------------------------
// Claims (ADR 001 D2, D3)
// ---------------------------------------------------------------------------

async fn claim_firing(
    conn: &mut PgConnection,
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

    let existing: Option<ClaimProbeRow> = sqlx::query_as(CLAIM_PROBE_LOCKED)
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

        AlertState::Posted => {
            let last_seen = existing.last_seen;
            let message_ts = existing.message_ts.map(MessageTs::new);
            touch(conn, alert, channel, group_key, now).await?;

            match message_ts {
                Some(message_ts) => Ok(ClaimResult::AlreadyPosted {
                    last_seen,
                    message_ts,
                }),
                None => Ok(ClaimResult::AlreadyClaimed),
            }
        }

        AlertState::Claimed | AlertState::Resolving => {
            touch(conn, alert, channel, group_key, now).await?;
            Ok(ClaimResult::AlreadyClaimed)
        }
    }
}

async fn touch(
    conn: &mut PgConnection,
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

async fn mark_resolving(
    conn: &mut PgConnection,
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

async fn persist_group(
    conn: &mut PgConnection,
    delta: &GroupDelta<'_>,
    channel: &ChannelId,
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
            .bind(now)
            .execute(&mut *conn)
            .await?;

        if inserted.rows_affected() > 0 {
            return Ok(true);
        }
    }

    sqlx::query(JOIN_GROUP)
        .bind(delta.members)
        .bind(group_key.as_str())
        .bind(channel.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(false)
}

async fn enqueue(conn: &mut PgConnection, op: &Op, now: DateTime<Utc>) -> Result<(), StoreError> {
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

impl StateStore for PostgresStore {
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
        let mut txn = self.pool.begin().await?;

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
        let opened = persist_group(&mut txn, &delta, &batch.channel, now).await?;

        let mut persisted = Vec::with_capacity(plan.ops.len());
        for op in plan.ops {
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

        let rows: Vec<OutboxRow> = sqlx::query_as(LEASE)
            .bind(worker.as_str())
            .bind(until)
            .bind(now)
            .bind(now)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?;

        leased(rows, until)
    }

    async fn complete(
        &self,
        id: OpId,
        effect: &OpEffect,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let now = stamp(now);
        let mut txn = self.pool.begin().await?;

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
        let mut txn = self.pool.begin().await?;

        let parked: Option<(String, String, Option<String>)> = sqlx::query_as(DEAD_LETTER)
            .bind(now)
            .bind(reason)
            .bind(id.get())
            .fetch_optional(&mut *txn)
            .await?;
        let Some((kind, channel, fingerprint)) = parked else {
            return Err(StoreError::NoSuchOp(id));
        };

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
        let mut txn = self.pool.begin().await?;

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
        let rows: Vec<(String, bool)> = sqlx::query_as(DESCRIBE_TABLE)
            .bind(table)
            .fetch_all(&self.pool)
            .await?;

        let mut columns: Vec<ColumnDef> = rows
            .into_iter()
            .map(|(name, nullable)| ColumnDef { name, nullable })
            .collect();
        columns.sort();
        Ok(columns)
    }
}
