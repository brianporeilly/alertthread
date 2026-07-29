//! Row shapes and the small pieces of logic both backends share.
//!
//! The `FromRow` derive is generic over the row type, so one struct per table serves both
//! SQLite and PostgreSQL. That is worth having: it means a column that exists in one
//! migration and not the other fails to decode on the backend that is missing it, rather
//! than being quietly absent from whichever mapping function forgot it.
//!
//! What is *not* shared is the SQL. The two backends spell placeholders differently and
//! reach for different concurrency primitives (`FOR UPDATE SKIP LOCKED` against
//! `BEGIN IMMEDIATE`), so each keeps its own statements, side by side and in the same
//! order, for reading against each other.

use std::collections::BTreeMap;

use alertthread_core::{
    ChannelId, ClaimResult, Fingerprint, GroupKey, LabelMap, MessageTs, Op, Placement, ThreadTs,
};
use chrono::{DateTime, SubsecRound, Utc};
use sqlx::types::Json;

use crate::error::StoreError;
use crate::model::{
    AlertRecord, AlertState, DeadLetter, GroupMembership, GroupRecord, LeasedOp, OpId, StoreStats,
};
use crate::payload::{OpKind, StoredOp};

/// Rounds a timestamp to what both backends can actually store.
///
/// PostgreSQL's `TIMESTAMPTZ` holds microseconds; SQLite's RFC 3339 text holds up to
/// nanoseconds. Truncating on the way in makes the two behave identically, so a value read
/// back compares equal to the value written on either backend — which is the difference
/// between a conformance suite that proves something and one that has a different meaning
/// per backend.
///
/// Every timestamp this crate writes derives from a `now` parameter, and every public entry
/// point passes it through here exactly once.
pub(crate) fn stamp(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

/// One `alert_message` row.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct AlertRow {
    pub fingerprint: String,
    pub channel: String,
    pub state: String,
    pub message_ts: Option<String>,
    pub thread_parent_ts: Option<String>,
    pub group_key: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub labels: Json<LabelMap>,
    pub annotations: Json<LabelMap>,
}

impl AlertRow {
    /// Converts to the public record, rejecting a `state` the schema does not define.
    pub(crate) fn into_record(self) -> Result<AlertRecord, StoreError> {
        let fingerprint = Fingerprint::new(self.fingerprint);
        let channel = ChannelId::new(self.channel);
        // A `state` outside the five is not something to shrug at: every query in this
        // crate filters on that column, so an unrecognised value means rows are being
        // skipped by predicates that look correct.
        let state =
            AlertState::parse(&self.state).ok_or_else(|| StoreError::UnknownAlertState {
                fingerprint: fingerprint.clone(),
                channel: channel.clone(),
                state: self.state,
            })?;

        Ok(AlertRecord {
            fingerprint,
            channel,
            state,
            message_ts: self.message_ts.map(MessageTs::new),
            thread_parent_ts: self.thread_parent_ts.map(ThreadTs::new),
            group_key: self.group_key.map(GroupKey::new),
            first_seen: self.first_seen,
            last_seen: self.last_seen,
            resolved_at: self.resolved_at,
            labels: self.labels.0,
            annotations: self.annotations.0,
        })
    }
}

/// One `group_message` row.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct GroupRow {
    pub group_key: String,
    pub channel: String,
    pub message_ts: Option<String>,
    pub member_count: i32,
    pub group_labels: Json<LabelMap>,
    pub created_at: DateTime<Utc>,
}

impl GroupRow {
    pub(crate) fn into_record(self) -> GroupRecord {
        GroupRecord {
            group_key: GroupKey::new(self.group_key),
            channel: ChannelId::new(self.channel),
            message_ts: self.message_ts.map(ThreadTs::new),
            member_count: self.member_count,
            group_labels: self.group_labels.0,
            created_at: self.created_at,
        }
    }
}

/// The subset of an `alert_message` row the firing claim needs to classify a conflict.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ClaimProbeRow {
    pub state: String,
    pub last_seen: DateTime<Utc>,
    pub message_ts: Option<String>,
}

/// The `message_ts` / `thread_parent_ts` pair a successful resolve claim returns.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ResolveClaimRow {
    pub message_ts: Option<String>,
    pub thread_parent_ts: Option<String>,
}

/// One leased `outbox` row.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct OutboxRow {
    pub id: i64,
    // Decoded as an untyped document rather than straight into `StoredOp`, so a payload
    // this build cannot read is reported with the id of the row holding it. `query_as`
    // failing on the decode would produce a driver error naming a column, which is exactly
    // the wrong thing to be told about a stuck queue.
    pub payload: Json<serde_json::Value>,
    pub attempts: i32,
    pub leased_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl OutboxRow {
    /// Converts to the public record, or reports which row would not decode.
    pub(crate) fn into_leased(self, leased_until: DateTime<Utc>) -> Result<LeasedOp, StoreError> {
        let id = OpId::new(self.id);
        let stored: StoredOp = serde_json::from_value(self.payload.0)
            .map_err(|source| StoreError::UndecodableOp { id, source })?;

        Ok(LeasedOp {
            id,
            op: Op::from(stored),
            attempts: self.attempts,
            // The column is nullable because an unleased row has no lease; a row this
            // function is given has just been leased by the statement that produced it, so
            // the value written is authoritative and the read-back is only a fallback.
            leased_until: self.leased_until.unwrap_or(leased_until),
            created_at: self.created_at,
        })
    }
}

/// One parked `outbox` row.
///
/// Separate from [`OutboxRow`] because the two are read for opposite reasons: a leased row
/// is about to be worked, and a parked one never will be. The columns differ accordingly —
/// a lease has no use for `dead_lettered_at`, and a dead letter has no lease to report.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct DeadLetterRow {
    pub id: i64,
    pub payload: Json<serde_json::Value>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub dead_lettered_at: DateTime<Utc>,
}

/// Converts parked rows into the public record, oldest first.
///
/// # Errors
///
/// [`StoreError::UndecodableOp`] naming the row whose payload this build cannot read. A
/// parked row that will not decode is reported rather than skipped: it is still an alert
/// that never reached Slack, and a listing that quietly omitted it would understate exactly
/// the number this project treats as unacceptable.
pub(crate) fn dead_letters(rows: Vec<DeadLetterRow>) -> Result<Vec<DeadLetter>, StoreError> {
    let mut parked = rows
        .into_iter()
        .map(|row| {
            let id = OpId::new(row.id);
            let stored: StoredOp = serde_json::from_value(row.payload.0)
                .map_err(|source| StoreError::UndecodableOp { id, source })?;
            Ok(DeadLetter {
                id,
                op: Op::from(stored),
                attempts: row.attempts,
                last_error: row.last_error,
                created_at: row.created_at,
                dead_lettered_at: row.dead_lettered_at,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    parked.sort_by_key(|row| row.id);
    Ok(parked)
}

/// Converts a lease's returned rows into ops, oldest first.
///
/// The sort is not decoration. Both backends' lease statements say `ORDER BY id` in the
/// subquery that *chooses* the rows, and neither guarantees anything about the order
/// `RETURNING` hands them back in — PostgreSQL demonstrably does not preserve it. That
/// matters here because ADR 001 D5 has the storm-collapse parent posting before its
/// children: a worker handed the children first would post them, find no parent timestamp,
/// and defer every one of them. The conformance suite caught exactly this, on PostgreSQL
/// and not on SQLite, which is the divergence two backends exist to expose.
///
/// # Errors
///
/// [`StoreError::UndecodableOp`] naming the row whose payload this build cannot read.
pub(crate) fn leased(
    rows: Vec<OutboxRow>,
    leased_until: DateTime<Utc>,
) -> Result<Vec<LeasedOp>, StoreError> {
    let mut ops = rows
        .into_iter()
        .map(|row| row.into_leased(leased_until))
        .collect::<Result<Vec<_>, _>>()?;
    ops.sort_by_key(|op| op.id);
    Ok(ops)
}

/// Classifies a resolve whose `UPDATE ... WHERE state IN ('claimed','posted')` matched
/// nothing.
///
/// Shared by both backends. The SQL that produced `existing` differs — one holds the
/// database's write lock, the other does not need to — but what the answer *means* is a
/// decision from ADR 001 D9, and it should not be possible for the two backends to disagree
/// about it.
///
/// # Errors
///
/// [`StoreError::UnknownAlertState`] if the row holds a `state` outside the schema's five.
pub(crate) fn resolve_miss(
    existing: Option<ClaimProbeRow>,
    fingerprint: &Fingerprint,
    channel: &ChannelId,
) -> Result<ClaimResult, StoreError> {
    let Some(existing) = existing else {
        // PRD §5.5 and ADR 001 D9: nothing to correlate to. Never silent — this becomes a
        // standalone resolved message.
        return Ok(ClaimResult::Orphan);
    };

    let state =
        AlertState::parse(&existing.state).ok_or_else(|| StoreError::UnknownAlertState {
            fingerprint: fingerprint.clone(),
            channel: channel.clone(),
            state: existing.state,
        })?;

    match state {
        // The post dead-lettered, so the message this resolution would have edited was
        // never sent. Calling it a duplicate resolution would mean the alert *and* its
        // resolution both went unmentioned; an orphan resolve posts something.
        AlertState::Failed => Ok(ClaimResult::Orphan),
        AlertState::Claimed | AlertState::Posted | AlertState::Resolving | AlertState::Resolved => {
            Ok(ClaimResult::AlreadyResolving)
        }
    }
}

/// Turns the two numbers the membership query returns into the split a summary renders.
///
/// Shared rather than written twice because it is arithmetic on a `SUM` that is `NULL` for
/// an empty group on both backends, and "the group has no members" must not be able to
/// become "the group is entirely resolved" on one of them.
pub(crate) fn membership(total: i64, resolved: Option<i64>) -> GroupMembership {
    // `usize::try_from` rather than `as`: a negative count is not something either backend
    // produces, and saturating to zero is the reading that cannot make a summary claim more
    // members than it has.
    let total = usize::try_from(total).unwrap_or(0);
    let resolved = usize::try_from(resolved.unwrap_or(0))
        .unwrap_or(0)
        .min(total);
    GroupMembership {
        firing: total - resolved,
        resolved,
    }
}

/// Assembles one metrics sample from the rows both backends' queries return.
///
/// # Errors
///
/// [`StoreError::UnknownOpKind`] if a queued row holds an `op` value this build cannot
/// classify. Reported rather than skipped: the row is still there, and a depth gauge that
/// silently omitted it would say the queue was shorter than it is.
pub(crate) fn stats(
    depth: Vec<(String, i64, Option<DateTime<Utc>>)>,
    dead_lettered: i64,
    tracked: i64,
) -> Result<StoreStats, StoreError> {
    let mut outbox_depth = BTreeMap::new();
    let mut oldest_queued_at: Option<DateTime<Utc>> = None;

    for (op, count, oldest) in depth {
        let kind = OpKind::parse(&op).ok_or(StoreError::UnknownOpKind(op))?;
        outbox_depth.insert(kind, u64::try_from(count).unwrap_or(0));
        if let Some(at) = oldest {
            oldest_queued_at = Some(oldest_queued_at.map_or(at, |seen| seen.min(at)));
        }
    }

    Ok(StoreStats {
        outbox_depth,
        dead_lettered: u64::try_from(dead_lettered).unwrap_or(0),
        oldest_queued_at,
        tracked_fingerprints: u64::try_from(tracked).unwrap_or(0),
    })
}

/// What one plan does to a storm-collapse group's membership.
///
/// Computed from the plan rather than counted in SQL because the plan is where the decision
/// was made: `Op::PostGroup` means "this batch opened the group", and a threaded `Op::Post`
/// with no accompanying `PostGroup` means "this alert joined a group that already existed"
/// (ADR 001 D5's stickiness).
///
/// `members` counts the threaded posts rather than trusting `PostGroup`'s
/// `initial_members`, because those posts are the rows that will actually exist. The two
/// agree today; if they ever stopped agreeing, the count on the summary message an operator
/// reads first during a storm should be the one derived from real members.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupDelta<'a> {
    /// The group these threaded posts belong to, if this plan threads anything at all.
    pub group: Option<&'a GroupKey>,
    /// Whether this plan opens that group rather than joining an existing one.
    pub opens: bool,
    /// How many alerts this plan threads under it.
    pub members: i32,
}

impl<'a> GroupDelta<'a> {
    /// Reads the membership change out of a plan's ops.
    pub(crate) fn of(ops: &'a [Op]) -> Self {
        let mut delta = Self::default();

        for op in ops {
            match op {
                Op::PostGroup { group_key, .. } => {
                    delta.group = Some(group_key);
                    delta.opens = true;
                }
                Op::Post {
                    placement: Placement::Thread { group_key, .. },
                    ..
                } => {
                    delta.group.get_or_insert(group_key);
                    delta.members = delta.members.saturating_add(1);
                }
                _ => {}
            }
        }

        delta
    }
}

#[cfg(test)]
mod tests {
    use super::{AlertRow, GroupDelta, stamp};
    use alertthread_core::{ChannelId, Fingerprint, GroupKey, Op, Placement};
    use chrono::{DateTime, TimeDelta, Utc};
    use sqlx::types::Json;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    #[test]
    fn stamping_drops_precision_postgres_cannot_hold() {
        // 123_456_789 ns -> 123_456 us. Without this, a value written to SQLite reads back
        // identical and the same value written to PostgreSQL does not, and the conformance
        // suite would have to say two different things about one behaviour.
        let precise = at(1_721_500_000) + TimeDelta::nanoseconds(123_456_789);
        assert_eq!(
            stamp(precise),
            at(1_721_500_000) + TimeDelta::microseconds(123_456)
        );
    }

    #[test]
    fn stamping_a_whole_second_changes_nothing() {
        assert_eq!(stamp(at(1_721_500_000)), at(1_721_500_000));
    }

    #[test]
    fn stamping_is_idempotent() {
        // Entry points stamp once; anything that stamped twice would still be correct, and
        // this is what says so rather than leaving it to be reasoned about.
        let once = stamp(at(1_721_500_000) + TimeDelta::nanoseconds(999_999_999));
        assert_eq!(stamp(once), once);
    }

    #[test]
    fn an_unknown_state_column_names_the_alert_it_came_from() {
        // The operator reading this error has a row they cannot interpret; the message has
        // to say which row.
        let row = AlertRow {
            fingerprint: "abc".to_owned(),
            channel: "#alerts".to_owned(),
            state: "posting".to_owned(),
            message_ts: None,
            thread_parent_ts: None,
            group_key: None,
            first_seen: at(1),
            last_seen: at(1),
            resolved_at: None,
            labels: Json(alertthread_core::LabelMap::new()),
            annotations: Json(alertthread_core::LabelMap::new()),
        };

        let error = row
            .into_record()
            .expect_err("a state outside the schema's five must not decode");
        let rendered = error.to_string();
        assert!(rendered.contains("abc"), "{rendered}");
        assert!(rendered.contains("#alerts"), "{rendered}");
        assert!(rendered.contains("posting"), "{rendered}");
    }

    #[test]
    fn a_well_formed_row_decodes_to_its_record() {
        let row = AlertRow {
            fingerprint: "abc".to_owned(),
            channel: "#alerts".to_owned(),
            state: "posted".to_owned(),
            message_ts: Some("1.1".to_owned()),
            thread_parent_ts: Some("1.2".to_owned()),
            group_key: Some("gk".to_owned()),
            first_seen: at(1),
            last_seen: at(2),
            resolved_at: Some(at(3)),
            labels: Json([("a".to_owned(), "b".to_owned())].into_iter().collect()),
            annotations: Json(alertthread_core::LabelMap::new()),
        };

        let record = row.into_record().expect("a documented state decodes");
        assert_eq!(record.fingerprint, Fingerprint::new("abc"));
        assert_eq!(record.state, crate::AlertState::Posted);
        assert_eq!(record.group_key, Some(GroupKey::new("gk")));
        assert_eq!(record.resolved_at, Some(at(3)));
        assert_eq!(record.labels.get("a").map(String::as_str), Some("b"));
        assert!(format!("{record:?}").contains("abc"));
    }

    fn probe(state: &str) -> super::ClaimProbeRow {
        super::ClaimProbeRow {
            state: state.to_owned(),
            last_seen: at(1),
            message_ts: None,
        }
    }

    // `resolve_miss` is unit-tested here rather than only through the conformance suite
    // because two of its branches describe rows the store will not produce on demand: a
    // `state` outside the schema's five cannot be written through the trait at all.
    // Reaching them from a database test would mean corrupting a row on purpose, which
    // would be a test of the corruption rather than of the classification.

    #[test]
    fn a_resolve_for_a_fingerprint_with_no_row_is_an_orphan() {
        // PRD §5.5: the relay was down when it fired, or `max_alerts` truncated it out of
        // the body. Posting something is the whole point.
        assert_eq!(
            super::resolve_miss(None, &Fingerprint::new("abc"), &ChannelId::new("#alerts"))
                .expect("a missing row classifies"),
            alertthread_core::ClaimResult::Orphan
        );
    }

    #[test]
    fn a_resolve_for_a_dead_lettered_alert_is_an_orphan_too() {
        // The message this resolution would have edited never reached Slack.
        assert_eq!(
            super::resolve_miss(
                Some(probe("failed")),
                &Fingerprint::new("abc"),
                &ChannelId::new("#alerts")
            )
            .expect("a failed row classifies"),
            alertthread_core::ClaimResult::Orphan
        );
    }

    #[test]
    fn a_resolve_for_an_alert_already_going_green_is_a_duplicate() {
        for state in ["claimed", "posted", "resolving", "resolved"] {
            assert_eq!(
                super::resolve_miss(
                    Some(probe(state)),
                    &Fingerprint::new("abc"),
                    &ChannelId::new("#alerts")
                )
                .expect("a documented state classifies"),
                alertthread_core::ClaimResult::AlreadyResolving,
                "state {state}"
            );
        }
    }

    #[test]
    fn a_resolve_against_an_undocumented_state_is_reported_rather_than_guessed_at() {
        let error = super::resolve_miss(
            Some(probe("posting")),
            &Fingerprint::new("abc"),
            &ChannelId::new("#alerts"),
        )
        .expect_err("a state outside the schema's five must not classify");

        let rendered = error.to_string();
        assert!(rendered.contains("posting"), "{rendered}");
        assert!(rendered.contains("abc"), "{rendered}");
    }

    #[test]
    fn a_group_with_no_members_is_not_reported_as_entirely_resolved() {
        // `SUM` over no rows is `NULL` on both backends. Reading that as "0 resolved of 0"
        // is correct; reading it as "all resolved" would turn a summary green the instant
        // its children were pruned.
        let empty = super::membership(0, None);
        assert_eq!(empty.firing, 0);
        assert_eq!(empty.resolved, 0);
        assert_eq!(empty.total(), 0);
        assert!(format!("{empty:?}").contains("firing"));
    }

    #[test]
    fn membership_splits_a_group_into_what_is_still_firing_and_what_is_not() {
        let split = super::membership(15, Some(6));
        assert_eq!(split.firing, 9);
        assert_eq!(split.resolved, 6);
        assert_eq!(split.total(), 15);
    }

    #[test]
    fn a_fully_resolved_group_reports_nothing_firing() {
        assert_eq!(super::membership(4, Some(4)).firing, 0);
    }

    #[test]
    fn membership_cannot_report_more_resolved_members_than_it_has() {
        // Neither backend produces this; the clamp is here because the subtraction below it
        // is on `usize`, and a summary is not the place to find out that it underflowed.
        let clamped = super::membership(2, Some(5));
        assert_eq!(clamped.resolved, 2);
        assert_eq!(clamped.firing, 0);

        let negative = super::membership(-1, Some(-3));
        assert_eq!(negative.total(), 0);
    }

    #[test]
    fn a_sample_reports_depth_per_kind_and_the_single_oldest_row() {
        // The oldest age is across *all* kinds, not per kind: "alerts are not reaching
        // Slack" is one question, and asking it per op would let a stuck `post` hide behind
        // a healthy `refresh`.
        let sample = super::stats(
            vec![
                ("post".to_owned(), 3, Some(at(1_000))),
                ("resolve".to_owned(), 1, Some(at(500))),
                ("refresh".to_owned(), 2, None),
            ],
            7,
            42,
        )
        .expect("known op kinds classify");

        assert_eq!(sample.outbox_depth.get(&super::OpKind::Post), Some(&3));
        assert_eq!(sample.outbox_depth.get(&super::OpKind::Resolve), Some(&1));
        assert_eq!(sample.outbox_depth.get(&super::OpKind::Refresh), Some(&2));
        assert_eq!(sample.oldest_queued_at, Some(at(500)));
        assert_eq!(sample.dead_lettered, 7);
        assert_eq!(sample.tracked_fingerprints, 42);
    }

    #[test]
    fn an_empty_queue_has_no_oldest_row_rather_than_an_age_of_zero() {
        // Zero would mean "the oldest queued alert is brand new", which is what a healthy
        // busy relay looks like. `None` is what an idle one looks like, and the sampler
        // renders the two differently.
        let sample = super::stats(Vec::new(), 0, 0).expect("an empty queue samples");
        assert_eq!(sample.oldest_queued_at, None);
        assert!(sample.outbox_depth.is_empty());
        assert_eq!(sample, crate::StoreStats::default());
    }

    #[test]
    fn an_op_kind_this_build_cannot_classify_is_reported_rather_than_dropped() {
        // The row is still queued. A depth gauge that silently omitted it would say the
        // queue was shorter than it is, which is the one direction this metric must not be
        // wrong in.
        let error = super::stats(vec![("send_carrier_pigeon".to_owned(), 1, None)], 0, 0)
            .expect_err("an unknown op kind must not be folded into a neighbour");
        assert!(error.to_string().contains("send_carrier_pigeon"), "{error}");
    }

    fn post(placement: Placement) -> Op {
        Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: ChannelId::new("#alerts"),
            placement,
        }
    }

    fn threaded() -> Op {
        post(Placement::Thread {
            group_key: GroupKey::new("gk"),
            parent_ts: None,
        })
    }

    #[test]
    fn a_plan_that_opens_a_group_counts_the_children_it_opens_with() {
        let ops = vec![
            Op::PostGroup {
                group_key: GroupKey::new("gk"),
                channel: ChannelId::new("#alerts"),
                initial_members: 2,
            },
            threaded(),
            threaded(),
        ];
        let delta = GroupDelta::of(&ops);

        assert_eq!(delta.group, Some(&GroupKey::new("gk")));
        assert!(delta.opens);
        assert_eq!(delta.members, 2);
        assert!(format!("{delta:?}").contains("members"));
    }

    #[test]
    fn a_sticky_join_names_its_group_without_opening_one() {
        // ADR 001 D5: one late alert threading under a group that already exists. The
        // parent's member count has to move even though no PostGroup was planned — and the
        // group has to be nameable from the children alone, because there is no PostGroup
        // op to read it off.
        let ops = vec![threaded()];
        let delta = GroupDelta::of(&ops);

        assert_eq!(delta.group, Some(&GroupKey::new("gk")));
        assert!(!delta.opens);
        assert_eq!(delta.members, 1);
    }

    #[test]
    fn a_top_level_plan_touches_no_group() {
        let ops = vec![post(Placement::Channel), post(Placement::Channel)];
        assert_eq!(GroupDelta::of(&ops), GroupDelta::default());
    }

    #[test]
    fn ops_that_are_not_posts_do_not_move_the_membership() {
        // A resolve or a refresh changes what a member's message says, not how many
        // members there are. Counting one would put a wrong number on the summary message
        // an operator reads first during a storm.
        let ops = vec![
            Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: ChannelId::new("#alerts"),
                message_ts: alertthread_core::MessageTs::new("1.1"),
            },
            Op::PostOrphanResolved {
                fingerprint: Fingerprint::new("def"),
                channel: ChannelId::new("#alerts"),
            },
        ];
        assert_eq!(GroupDelta::of(&ops), GroupDelta::default());
    }
}
