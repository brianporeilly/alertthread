//! The vocabulary the [`StateStore`](crate::StateStore) trait speaks.
//!
//! Everything here is a store-owned type. The *decision* types — [`Op`], [`Plan`],
//! [`ClaimResult`](alertthread_core::ClaimResult) — belong to `alertthread-core` and are
//! used unchanged; what this module adds is the bookkeeping the database needs and the
//! core deliberately does not know about: row identity, lease state, attempt counts and
//! retention.

use alertthread_core::{
    ChannelId, Fingerprint, GroupKey, GroupState, LabelMap, MessageTs, ThreadTs,
};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;

use crate::payload::OpKind;

/// The primary key of an outbox row.
///
/// A newtype for the same reason every identifier in this project is one (AGENTS.md rule
/// 4): `complete(id)` and `defer(id)` take a number, and passing the wrong number acts on
/// somebody else's queued work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(i64);

impl OpId {
    /// Wraps a row id read from the database.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    /// The underlying row id.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for OpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Which replica holds a lease.
///
/// Recorded so a stuck queue can be traced to the process that stopped draining it, which
/// is the first question asked when `alertthread_outbox_oldest_age_seconds` climbs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    /// Wraps a worker identity. In Kubernetes this is the pod name.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The lifecycle of one `alert_message` row.
///
/// The five values ADR 001 D4's schema comment lists, made a type so a typo in a query
/// cannot invent a sixth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AlertState {
    /// The claim won. We own the notification; the post has not landed yet.
    Claimed,
    /// The post landed and `message_ts` is set.
    Posted,
    /// A resolution has been accepted and its op is queued.
    Resolving,
    /// The resolution has been delivered.
    Resolved,
    /// The post was dead-lettered: this alert never reached Slack.
    ///
    /// Distinct from [`Resolved`](Self::Resolved) on purpose. A resolution arriving for a
    /// `failed` row must not be treated as a duplicate — there is no message to edit, so
    /// it becomes an orphan resolve and posts something. Collapsing the two states is how
    /// a failed alert *and* its resolution both end up silent.
    Failed,
}

impl AlertState {
    /// The value stored in the `state` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Posted => "posted",
            Self::Resolving => "resolving",
            Self::Resolved => "resolved",
            Self::Failed => "failed",
        }
    }

    /// Reads a `state` column value, or `None` if it is not one of the five.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claimed" => Some(Self::Claimed),
            "posted" => Some(Self::Posted),
            "resolving" => Some(Self::Resolving),
            "resolved" => Some(Self::Resolved),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One `alert_message` row.
///
/// The relay's correlation state for a single alert in a single channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertRecord {
    /// Alertmanager's identity for the alert.
    pub fingerprint: Fingerprint,
    /// Where its message lives.
    pub channel: ChannelId,
    /// Where it is in its lifecycle.
    pub state: AlertState,
    /// The Slack timestamp of its message, once posted.
    pub message_ts: Option<MessageTs>,
    /// The storm-collapse parent its message hangs under, if it was threaded.
    pub thread_parent_ts: Option<ThreadTs>,
    /// The Alertmanager group it arrived in.
    pub group_key: Option<GroupKey>,
    /// When this fingerprint was first claimed in this channel.
    pub first_seen: DateTime<Utc>,
    /// When a delivery for it was last accepted.
    pub last_seen: DateTime<Utc>,
    /// When its resolution was accepted.
    pub resolved_at: Option<DateTime<Utc>>,
    /// The alert's label set as Alertmanager sent it.
    pub labels: LabelMap,
    /// The alert's annotations as Alertmanager sent them.
    pub annotations: LabelMap,
}

/// One `group_message` row: a storm-collapse parent (ADR 001 D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRecord {
    /// Alertmanager's key for the group.
    pub group_key: GroupKey,
    /// Where the parent lives.
    pub channel: ChannelId,
    /// The parent's Slack timestamp, or `None` while its own post is still queued.
    pub message_ts: Option<ThreadTs>,
    /// How many alerts have joined this group.
    ///
    /// `i32` because the column is `INTEGER`, which is 32 bits in PostgreSQL. Reading it
    /// as `i64` would decode fine on SQLite and fail on PostgreSQL, which is exactly the
    /// class of divergence the conformance suite exists to prevent — so the narrower type
    /// is the honest one.
    pub member_count: i32,
    /// Alertmanager's `groupLabels` for this group: what its `group_by` grouped on.
    ///
    /// Written when the group is opened and never updated. These labels *define* the
    /// group — changing `group_by` changes the group key and therefore produces a different
    /// row — so unlike `commonLabels` they cannot drift while the group exists, which is
    /// what makes write-once correct rather than merely convenient.
    ///
    /// They live on the row rather than on the op because the row is the only thing with
    /// the group's lifetime: `RefreshGroup` is planned later, when a child resolves, and
    /// ADR 001 D4 deletes an outbox row on completion.
    pub group_labels: LabelMap,
    /// When the group was opened.
    pub created_at: DateTime<Utc>,
}

impl GroupRecord {
    /// The slice of this row the planner needs.
    ///
    /// [`GroupState`] is deliberately smaller than the row: `plan` decides placement and
    /// nothing else, so handing it `member_count` would invite the count to be recomputed
    /// per batch — which is exactly the wrong number, because a batch knows what it added
    /// and not what the group holds.
    pub fn state(&self) -> GroupState {
        GroupState {
            group_key: self.group_key.clone(),
            message_ts: self.message_ts.clone(),
        }
    }
}

/// What a worker did with a leased op.
///
/// Passed to [`StateStore::complete`](crate::StateStore::complete), which applies it to the
/// correlation state and deletes the outbox row in one transaction. ADR 001 D4's retention
/// section is the reason the delete is inline rather than swept: a completed op is not
/// history, it is finished work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpEffect {
    /// An alert's message was posted.
    Posted {
        /// The timestamp Slack returned.
        message_ts: MessageTs,
        /// The storm-collapse parent it was threaded under, if any (ADR 001 D5).
        thread_parent_ts: Option<ThreadTs>,
    },
    /// A storm-collapse parent was posted.
    GroupPosted {
        /// The timestamp Slack returned.
        message_ts: ThreadTs,
    },
    /// An in-place edit succeeded and changed no correlation state (ADR 001 D7).
    Refreshed,
    /// A resolution was delivered.
    Resolved,
    /// `chat.update` reported `message_not_found` (ADR 001 D7, D9).
    ///
    /// The stored timestamp is stale — somebody deleted the message, or the state outlived
    /// it. Clearing it and returning the row to `claimed` is the self-heal D7 describes;
    /// the replacement post is enqueued by the worker, because deciding to re-post is a
    /// decision and decisions do not live in the store.
    MessageLost,
    /// A standalone message went out with nothing to correlate it to.
    ///
    /// The orphan-resolve path of ADR 001 D9 and PRD §5.5. There is no row to update, and
    /// that is the point: this variant exists so "nothing to record" is stated rather than
    /// inferred from a missing branch.
    Standalone,
}

/// Why an op is going back into the queue instead of finishing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deferral {
    /// Slack returned 429 (ADR 001 D2).
    ///
    /// The attempt is given back. A rate limit is Slack telling us to come back later, not
    /// a failure of the op, and counting it would march an alert toward the dead-letter
    /// queue for being popular.
    RateLimited {
        /// `now + Retry-After`.
        until: DateTime<Utc>,
    },
    /// Anything retryable: a 5xx, or a `resolve` whose post has not landed yet.
    Backoff {
        /// When to try again.
        until: DateTime<Utc>,
        /// What went wrong, for the operator reading the row.
        error: String,
    },
}

/// One outbox row handed to a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeasedOp {
    /// The row's identity, used to complete or defer it.
    pub id: OpId,
    /// The work itself, as [`plan`](alertthread_core::plan) decided it.
    pub op: alertthread_core::Op,
    /// How many times this op has been leased, *including* this lease.
    pub attempts: i32,
    /// When this lease expires and the row becomes reclaimable.
    pub leased_until: DateTime<Utc>,
    /// When the op was enqueued. `now - created_at` is
    /// `alertthread_outbox_oldest_age_seconds`, ADR 001 D11's primary SLO signal.
    pub created_at: DateTime<Utc>,
}

/// One outbox row that was parked by [`StateStore::dead_letter`](crate::StateStore).
///
/// Every field is something an operator needs in order to answer "which alert did not
/// arrive, and why". The row is never leased again, so this is the only way to read it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadLetter {
    /// The row's identity.
    pub id: OpId,
    /// The work that never happened.
    pub op: alertthread_core::Op,
    /// Where it was addressed. Read from the `channel` column, which is what
    /// [`DeadLetterScope`] filters on.
    pub channel: ChannelId,
    /// The alert it acts on, for the four op kinds that act on one.
    ///
    /// `None` for a storm-collapse parent, which belongs to a group rather than to a
    /// fingerprint. Read from the `fingerprint` column, which is what [`DeadLetterScope`]
    /// filters on.
    pub fingerprint: Option<Fingerprint>,
    /// How many attempts were spent before it was parked.
    pub attempts: i32,
    /// What the last failure said. `None` only for a row parked without a reason recorded.
    pub last_error: Option<String>,
    /// When the alert arrived.
    pub created_at: DateTime<Utc>,
    /// When it was parked.
    pub dead_lettered_at: DateTime<Utc>,
}

/// Which parked rows a dead-letter operation applies to.
///
/// Both filters are `AND`ed, and an unset filter matches every row — so the default,
/// [`DeadLetterScope::ALL`], is the whole dead-letter queue. That is deliberately the
/// value with no fields set: the automatic sweep in the app crate wants it, and asking for
/// it by name at that call site is what keeps ADR 003 §5.1's all-or-nothing decision a
/// written-down choice rather than a property of the only method there was.
///
/// The two axes are the two low-cardinality columns `outbox` already carries. The *reason*
/// a row was parked is not among them: `last_error` holds the verbatim Slack detail, which
/// is free text and not a stable interface to filter on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeadLetterScope {
    channel: Option<ChannelId>,
    fingerprint: Option<Fingerprint>,
}

impl DeadLetterScope {
    /// Every parked row, whatever it is and wherever it was going.
    pub const ALL: Self = Self {
        channel: None,
        fingerprint: None,
    };

    /// Narrows to one channel.
    #[must_use]
    pub fn with_channel(mut self, channel: ChannelId) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Narrows to one alert.
    #[must_use]
    pub fn with_fingerprint(mut self, fingerprint: Fingerprint) -> Self {
        self.fingerprint = Some(fingerprint);
        self
    }

    /// The channel filter, if there is one.
    pub const fn channel(&self) -> Option<&ChannelId> {
        self.channel.as_ref()
    }

    /// The fingerprint filter, if there is one.
    pub const fn fingerprint(&self) -> Option<&Fingerprint> {
        self.fingerprint.as_ref()
    }

    /// Whether this scope narrows nothing.
    ///
    /// The caller that reports what it is about to do needs to distinguish "the whole
    /// queue" from "a filter that happened to match everything".
    pub const fn is_everything(&self) -> bool {
        self.channel.is_none() && self.fingerprint.is_none()
    }
}

/// How long finished state is kept (ADR 001 D4, retention; PRD §5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Delete `resolved` alerts whose `resolved_at` is older than this.
    pub resolved_after: TimeDelta,
    /// Delete alerts in any state whose `last_seen` is older than this.
    ///
    /// Catches alerts that fire and never resolve, which would otherwise pin a row for
    /// ever. This is the sweep that keeps a SQLite deployment's file from growing without
    /// bound on the strength of one misbehaving rule.
    pub stale_after: TimeDelta,
}

impl RetentionPolicy {
    /// ADR 001 D4's default: resolved alerts kept for seven days.
    pub const DEFAULT_RESOLVED_DAYS: i64 = 7;

    /// ADR 001 D4's default: anything at all kept for thirty days.
    pub const DEFAULT_STALE_DAYS: i64 = 30;
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            resolved_after: TimeDelta::days(Self::DEFAULT_RESOLVED_DAYS),
            stale_after: TimeDelta::days(Self::DEFAULT_STALE_DAYS),
        }
    }
}

/// What one pruner pass deleted.
///
/// Returned rather than logged so Phase 4 can turn it into a metric. A pruner that reports
/// nothing is a pruner nobody notices has stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Alerts deleted for having been resolved longer than `resolved_after`.
    pub resolved_alerts: u64,
    /// Alerts deleted for not having been seen within `stale_after`.
    pub stale_alerts: u64,
    /// Storm-collapse parents deleted for having no surviving members.
    pub empty_groups: u64,
}

impl PruneStats {
    /// Whether this pass deleted anything at all.
    pub const fn is_empty(&self) -> bool {
        self.resolved_alerts == 0 && self.stale_alerts == 0 && self.empty_groups == 0
    }
}

/// What the relay looks like from the outside, in one sample.
///
/// Every field here is a metric from ADR 001 D11, and the whole struct is read on a
/// background interval rather than inside `GET /metrics`. That is deliberate: a Prometheus
/// scraping every 15 seconds across N replicas would otherwise be a load generator pointed
/// at the outbox, and a slow store would make the scrape time out and take *every* other
/// metric with it — including the ones that would have said why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// How many rows are queued and leasable, per kind: `alertthread_outbox_depth{op}`.
    ///
    /// Kinds with nothing queued are absent rather than zero. The sampler holds the label
    /// set steady itself, because a gauge that stops being reported reads as "no data" and
    /// not as "nothing pending".
    pub outbox_depth: BTreeMap<OpKind, u64>,
    /// How many rows have been parked by [`StateStore::dead_letter`](crate::StateStore).
    ///
    /// A level, not the `alertthread_dead_letter_total` counter: the counter says how many
    /// alerts stopped being delivered, this says how many are still sitting there unread.
    pub dead_lettered: u64,
    /// When the oldest queued row was enqueued, if anything is queued.
    ///
    /// `now - this` is `alertthread_outbox_oldest_age_seconds`, D11's primary SLO signal —
    /// the one metric that means "alerts are not reaching Slack".
    pub oldest_queued_at: Option<DateTime<Utc>>,
    /// How many `alert_message` rows exist: `alertthread_tracked_fingerprints`.
    pub tracked_fingerprints: u64,
}

/// How a storm-collapse group's members are currently split (ADR 001 D5).
///
/// Counted from `alert_message` rather than read off `group_message.member_count`, because
/// the summary shows a **live** firing/resolved count and `member_count` only ever grows.
/// A parent that said "9 of 15 firing" for ever, over a thread of green replies, is
/// confidently wrong — which the renderer already treats as worse than uninformative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupMembership {
    /// Members that have not resolved.
    pub firing: usize,
    /// Members whose resolution has been accepted or delivered.
    pub resolved: usize,
}

impl GroupMembership {
    /// Every member, however it is doing.
    pub const fn total(&self) -> usize {
        self.firing + self.resolved
    }
}

/// One column of one table, as the database reports it.
///
/// Used by [`StateStore::describe_table`](crate::StateStore::describe_table) to police the
/// two migration directories against each other. See that method for why this is part of
/// the trait rather than a test helper.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColumnDef {
    /// The column's name.
    pub name: String,
    /// Whether the column accepts `NULL`.
    pub nullable: bool,
}

#[cfg(test)]
mod tests {
    use super::{AlertState, ColumnDef, OpId, PruneStats, RetentionPolicy, WorkerId};
    use alertthread_core::{ChannelId, GroupKey, ThreadTs};
    use chrono::{DateTime, TimeDelta, Utc};

    #[test]
    fn an_op_id_carries_its_row_number() {
        let id = OpId::new(42);
        assert_eq!(id.get(), 42);
        assert_eq!(id.to_string(), "42");
        assert_eq!(format!("{id:?}"), "OpId(42)");
    }

    #[test]
    fn a_worker_id_carries_its_identity() {
        let worker = WorkerId::new("alertthread-7d9f-abc");
        assert_eq!(worker.as_str(), "alertthread-7d9f-abc");
        assert_eq!(worker.to_string(), "alertthread-7d9f-abc");
        assert!(format!("{worker:?}").contains("alertthread-7d9f-abc"));
    }

    #[test]
    fn every_alert_state_round_trips_through_its_column_value() {
        // The `state` column is compared against string literals in five queries. A
        // spelling that does not round-trip here is a query that silently matches nothing.
        for state in [
            AlertState::Claimed,
            AlertState::Posted,
            AlertState::Resolving,
            AlertState::Resolved,
            AlertState::Failed,
        ] {
            assert_eq!(AlertState::parse(state.as_str()), Some(state));
        }
    }

    #[test]
    fn the_five_state_spellings_are_the_ones_the_schema_documents() {
        assert_eq!(AlertState::Claimed.as_str(), "claimed");
        assert_eq!(AlertState::Posted.as_str(), "posted");
        assert_eq!(AlertState::Resolving.as_str(), "resolving");
        assert_eq!(AlertState::Resolved.as_str(), "resolved");
        assert_eq!(AlertState::Failed.as_str(), "failed");
    }

    #[test]
    fn an_unrecognised_state_does_not_parse() {
        assert_eq!(AlertState::parse("posting"), None);
        assert_eq!(AlertState::parse(""), None);
        assert_eq!(AlertState::parse("Claimed"), None);
    }

    #[test]
    fn alert_state_debug_names_the_variant() {
        assert_eq!(format!("{:?}", AlertState::Failed), "Failed");
    }

    #[test]
    fn the_retention_defaults_are_the_ones_adr_001_specifies() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.resolved_after, TimeDelta::days(7));
        assert_eq!(policy.stale_after, TimeDelta::days(30));
        assert!(format!("{policy:?}").contains("resolved_after"));
    }

    #[test]
    fn an_untouched_prune_pass_reports_nothing() {
        let stats = PruneStats::default();
        assert!(stats.is_empty());
        assert_eq!(stats.resolved_alerts, 0);
        assert_eq!(stats.stale_alerts, 0);
        assert_eq!(stats.empty_groups, 0);
    }

    #[test]
    fn a_prune_pass_that_deleted_anything_at_all_is_not_empty() {
        // Each counter is checked separately: a hand-written `is_empty` that forgot one
        // field would make a pruner that only ever deletes groups look idle.
        for stats in [
            PruneStats {
                resolved_alerts: 1,
                ..PruneStats::default()
            },
            PruneStats {
                stale_alerts: 1,
                ..PruneStats::default()
            },
            PruneStats {
                empty_groups: 1,
                ..PruneStats::default()
            },
        ] {
            assert!(!stats.is_empty(), "{stats:?}");
        }
    }

    #[test]
    fn a_group_record_narrows_to_the_state_the_planner_takes() {
        let record = super::GroupRecord {
            group_key: GroupKey::new("gk"),
            channel: ChannelId::new("#alerts"),
            message_ts: Some(ThreadTs::new("1.2")),
            member_count: 9,
            group_labels: [("alertname".to_owned(), "KubePodNotReady".to_owned())]
                .into_iter()
                .collect(),
            created_at: DateTime::<Utc>::from_timestamp(1_721_500_000, 0)
                .expect("timestamp is in range"),
        };
        let state = record.state();

        assert_eq!(state.group_key, GroupKey::new("gk"));
        assert_eq!(state.message_ts, Some(ThreadTs::new("1.2")));
        // The labels are presentation, not placement: `plan` has no use for them, so
        // narrowing to `GroupState` deliberately leaves them behind.
        assert_eq!(
            record.group_labels.get("alertname").map(String::as_str),
            Some("KubePodNotReady")
        );
    }

    #[test]
    fn the_default_dead_letter_scope_is_the_whole_queue() {
        // `ALL` and `default()` have to agree: one of them is what the automatic sweep
        // passes and the other is what a caller building a scope up starts from.
        assert_eq!(
            super::DeadLetterScope::default(),
            super::DeadLetterScope::ALL
        );
        assert!(super::DeadLetterScope::ALL.is_everything());
        assert_eq!(super::DeadLetterScope::ALL.channel(), None);
        assert_eq!(super::DeadLetterScope::ALL.fingerprint(), None);
    }

    #[test]
    fn a_dead_letter_scope_carries_each_filter_it_was_given() {
        let channel_only = super::DeadLetterScope::ALL.with_channel(ChannelId::new("#alerts"));
        assert!(!channel_only.is_everything());
        assert_eq!(channel_only.channel(), Some(&ChannelId::new("#alerts")));
        assert_eq!(channel_only.fingerprint(), None);

        let fingerprint_only =
            super::DeadLetterScope::ALL.with_fingerprint(alertthread_core::Fingerprint::new("abc"));
        assert!(!fingerprint_only.is_everything());
        assert_eq!(fingerprint_only.channel(), None);
        assert_eq!(
            fingerprint_only.fingerprint(),
            Some(&alertthread_core::Fingerprint::new("abc"))
        );

        // Both at once, because the two filters are ANDed rather than alternatives.
        let both = super::DeadLetterScope::ALL
            .with_channel(ChannelId::new("#alerts"))
            .with_fingerprint(alertthread_core::Fingerprint::new("abc"));
        assert_eq!(both.channel(), Some(&ChannelId::new("#alerts")));
        assert_eq!(
            both.fingerprint(),
            Some(&alertthread_core::Fingerprint::new("abc"))
        );
        assert!(format!("{both:?}").contains("#alerts"));
    }

    #[test]
    fn column_definitions_sort_by_name() {
        let mut columns = [
            ColumnDef {
                name: "state".to_owned(),
                nullable: false,
            },
            ColumnDef {
                name: "channel".to_owned(),
                nullable: false,
            },
        ];
        columns.sort();
        assert_eq!(columns[0].name, "channel");
        assert!(format!("{:?}", columns[0]).contains("channel"));
    }
}
