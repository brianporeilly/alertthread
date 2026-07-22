//! The conformance suite: written once, run against every backend.
//!
//! Each function here is one behaviour of [`StateStore`], expressed generically. The
//! harness in `../conformance.rs` instantiates every one of them against SQLite and against
//! PostgreSQL, so there is exactly one description of what the store does and two proofs
//! that it does it.
//!
//! **Every row of ADR 001 D3's concurrency table is a test in this file**, and the tests
//! that cover a row say so. That table is the reason this project can claim correctness
//! under HA; until it is executable it is a promise rather than a property.
//!
//! Two rules the suite holds itself to:
//!
//! - **The same observable outcome on both backends.** SQLite reaches exclusion through
//!   `BEGIN IMMEDIATE` and PostgreSQL through row locks and `SKIP LOCKED`, and no test here
//!   knows which. A racing test that asserted something weaker on SQLite would be testing
//!   the mechanism instead of the property.
//! - **Nothing asserts that an alert was dropped.** That is not a behaviour this project
//!   has (AGENTS.md), so where a test checks that no op was queued it is always because the
//!   work was already in hand — a duplicate delivery, or a debounced repeat.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a test that unwraps is a test that fails loudly, which is what \
              clippy.toml's allow-*-in-tests settings say for unit tests. Integration \
              tests reach these helpers from outside a #[test] function, where clippy \
              cannot see the context, so the same policy is stated here."
)]

use alertthread_core::{
    AlertBatch, AlertStatus, ChannelId, Fingerprint, GroupKey, LabelMap, MessageTs, Op, Placement,
    Plan, Policy, ResolveTarget, ThreadTs, WebhookAlert, plan,
};
use alertthread_store::{
    AlertState, Deferral, LeasedOp, OpEffect, RetentionPolicy, StateStore, StoreError, WorkerId,
};
use chrono::{DateTime, TimeDelta, Utc};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const CHANNEL: &str = "#alerts";
const OTHER_CHANNEL: &str = "#alerts-critical";
const GROUP: &str = "{}:{alertname=\"KubePodNotReady\"}";
const OTHER_GROUP: &str = "{}:{alertname=\"CephOSDDown\"}";

/// A fixed instant, so every test reads as a timeline rather than as arithmetic.
fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_721_500_000, 0).expect("timestamp is in range")
}

fn secs(n: i64) -> TimeDelta {
    TimeDelta::seconds(n)
}

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

fn group_key() -> GroupKey {
    GroupKey::new(GROUP)
}

fn alert(fingerprint: &str, status: AlertStatus) -> WebhookAlert {
    WebhookAlert {
        status,
        labels: [
            ("alertname".to_owned(), "KubePodNotReady".to_owned()),
            ("severity".to_owned(), "critical".to_owned()),
        ]
        .into_iter()
        .collect(),
        annotations: [("summary".to_owned(), "pod is not ready".to_owned())]
            .into_iter()
            .collect(),
        starts_at: t0() - TimeDelta::hours(1),
        ends_at: DateTime::from_timestamp(0, 0).expect("epoch is in range"),
        generator_url: "http://prometheus/graph".to_owned(),
        fingerprint: Fingerprint::new(fingerprint),
    }
}

fn firing(fingerprint: &str) -> WebhookAlert {
    alert(fingerprint, AlertStatus::Firing)
}

fn resolved(fingerprint: &str) -> WebhookAlert {
    alert(fingerprint, AlertStatus::Resolved)
}

fn batch(alerts: Vec<WebhookAlert>) -> AlertBatch {
    batch_in(CHANNEL, GROUP, alerts)
}

fn batch_in(channel: &str, group: &str, alerts: Vec<WebhookAlert>) -> AlertBatch {
    batch_labelled(channel, group, group_labels(), alerts)
}

/// The `groupLabels` Alertmanager would send for [`GROUP`].
fn group_labels() -> LabelMap {
    [
        ("alertname".to_owned(), "KubePodNotReady".to_owned()),
        ("job".to_owned(), "kube-state-metrics".to_owned()),
    ]
    .into_iter()
    .collect()
}

fn batch_labelled(
    channel: &str,
    group: &str,
    group_labels: LabelMap,
    alerts: Vec<WebhookAlert>,
) -> AlertBatch {
    AlertBatch {
        channel: ChannelId::new(channel),
        group_key: GroupKey::new(group),
        group_labels,
        truncated_alerts: 0,
        alerts,
    }
}

/// Runs one delivery through the store with ADR 001's default policy.
async fn ingest<S: StateStore>(store: &S, batch: &AlertBatch, now: DateTime<Utc>) -> Plan {
    ingest_with(store, batch, now, &Policy::default()).await
}

/// Runs one delivery through the store, with the real planner in the middle.
///
/// The suite never hands `ingest` a stub decision function. The point of the trait's shape
/// is that the claims, [`plan`] and the enqueue happen in one transaction, so the suite
/// exercises that sequence rather than an approximation of it.
async fn ingest_with<S: StateStore>(
    store: &S,
    batch: &AlertBatch,
    now: DateTime<Utc>,
    policy: &Policy,
) -> Plan {
    store
        .ingest(batch, now, |outcomes, group| {
            plan(outcomes, batch, group, policy, now)
        })
        .await
        .expect("ingest succeeds against a healthy store")
}

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// Leases everything that is ready, as a worker would.
async fn lease<S: StateStore>(store: &S, name: &str, now: DateTime<Utc>) -> Vec<LeasedOp> {
    store
        .lease_batch(&worker(name), 100, secs(60), now)
        .await
        .expect("lease succeeds against a healthy store")
}

/// What a worker that successfully delivered this op would report.
///
/// The interesting case is a storm-collapse child whose `parent_ts` is `None`: the plan
/// that queued it was the same plan that queued the parent, so the parent had no timestamp
/// yet. A real worker resolves it from the `group_message` row at send time, and this does
/// the same — because a helper that reported `None` there would quietly stop the suite from
/// ever exercising `thread_parent_ts` at all.
async fn effect_of<S: StateStore>(store: &S, op: &Op, seq: usize) -> OpEffect {
    match op {
        Op::Post {
            placement, channel, ..
        } => {
            let thread_parent_ts = match placement {
                Placement::Thread {
                    group_key,
                    parent_ts,
                } => match parent_ts {
                    Some(parent_ts) => Some(parent_ts.clone()),
                    None => store
                        .group(group_key, channel)
                        .await
                        .expect("reading a group")
                        .and_then(|group| group.message_ts),
                },
                Placement::Channel => None,
            };
            OpEffect::Posted {
                message_ts: MessageTs::new(format!("1721500{seq:03}.000100")),
                thread_parent_ts,
            }
        }
        Op::PostGroup { .. } => OpEffect::GroupPosted {
            message_ts: ThreadTs::new(format!("1721500{seq:03}.000001")),
        },
        Op::Refresh { .. } | Op::RefreshGroup { .. } => OpEffect::Refreshed,
        Op::Resolve { .. } => OpEffect::Resolved,
        Op::PostOrphanResolved { .. } => OpEffect::Standalone,
    }
}

/// Drains the outbox the way a healthy worker would: lease everything, deliver it, complete
/// it. Leaves the store with the state a delivered batch produces and an empty queue.
///
/// Ops are completed in lease order, which is id order, which is plan order — so a group
/// parent has its timestamp before its children ask for it, exactly as ADR 001 D5 describes.
async fn deliver<S: StateStore>(store: &S, now: DateTime<Utc>) -> usize {
    let leased = lease(store, "deliverer", now).await;
    for (seq, op) in leased.iter().enumerate() {
        let effect = effect_of(store, &op.op, seq + 1).await;
        store
            .complete(op.id, &effect, now)
            .await
            .expect("completing a leased op succeeds");
    }
    leased.len()
}

fn posts(plan: &Plan) -> usize {
    plan.ops
        .iter()
        .filter(|op| matches!(op, Op::Post { .. }))
        .count()
}

// ---------------------------------------------------------------------------
// Schema: the drift police for ADR 001 D4's two migration directories
// ---------------------------------------------------------------------------

/// The columns each table must have, in every backend, with their nullability.
///
/// This list is the contract. A column added to `migrations/postgres/` and forgotten in
/// `migrations/sqlite/` fails here on SQLite, and vice versa — which is the only mechanism
/// that makes D4's "accepted cost" of two directories actually survivable.
///
/// It is deliberately not derived from either migration file. A check that read the SQL
/// would agree with whatever the SQL said, which is precisely the failure mode.
fn expected_columns(table: &str) -> Vec<(&'static str, bool)> {
    match table {
        "alert_message" => vec![
            ("annotations", false),
            ("channel", false),
            ("fingerprint", false),
            ("first_seen", false),
            ("group_key", true),
            ("labels", false),
            ("last_seen", false),
            ("message_ts", true),
            ("resolved_at", true),
            ("state", false),
            ("thread_parent_ts", true),
        ],
        "group_message" => vec![
            ("channel", false),
            ("created_at", false),
            ("group_key", false),
            ("group_labels", false),
            ("member_count", false),
            ("message_ts", true),
        ],
        "outbox" => vec![
            ("attempts", false),
            ("channel", false),
            ("created_at", false),
            ("dead_lettered_at", true),
            ("fingerprint", true),
            ("group_key", true),
            ("id", false),
            ("last_error", true),
            ("leased_by", true),
            ("leased_until", true),
            ("next_attempt_at", false),
            ("op", false),
            ("payload", false),
        ],
        other => panic!("no expectation recorded for table {other:?}"),
    }
}

pub(crate) async fn the_schema_is_the_one_both_migration_directories_are_supposed_to_build<S>(
    store: &S,
) where
    S: StateStore,
{
    for table in ["alert_message", "group_message", "outbox"] {
        let actual: Vec<(String, bool)> = store
            .describe_table(table)
            .await
            .expect("describing a table that exists")
            .into_iter()
            .map(|column| (column.name, column.nullable))
            .collect();
        let actual: Vec<(&str, bool)> = actual
            .iter()
            .map(|(name, nullable)| (name.as_str(), *nullable))
            .collect();

        assert_eq!(
            actual,
            expected_columns(table),
            "the {table} table this backend built is not the one both migrations are \
             supposed to describe"
        );
    }
}

pub(crate) async fn a_table_that_does_not_exist_describes_as_nothing<S: StateStore>(store: &S) {
    // The assertion that catches a migration forgetting a table outright, rather than
    // forgetting a column in one it did create.
    assert!(
        store
            .describe_table("alert_messages")
            .await
            .expect("describing a missing table is not an error")
            .is_empty()
    );
}

pub(crate) async fn migrating_an_already_migrated_store_is_a_no_op<S: StateStore>(store: &S) {
    // Called on every start. A second run that failed would turn a restart into a crash
    // loop, and a crash loop in the alerting path is silence.
    store.migrate().await.expect("migrations are idempotent");
    store.migrate().await.expect("migrations are idempotent");
    the_schema_is_the_one_both_migration_directories_are_supposed_to_build(store).await;
}

// ---------------------------------------------------------------------------
// ADR 001 D2: ingest classification
// ---------------------------------------------------------------------------

pub(crate) async fn a_new_firing_alert_is_claimed_and_its_post_is_queued<S: StateStore>(store: &S) {
    let batch = batch(vec![firing("abc")]);
    let planned = ingest(store, &batch, t0()).await;

    assert_eq!(
        planned.ops,
        vec![Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            placement: Placement::Channel,
        }]
    );

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("the claim wrote a row");
    assert_eq!(record.state, AlertState::Claimed);
    assert_eq!(record.message_ts, None);
    assert_eq!(record.group_key, Some(group_key()));
    assert_eq!(record.first_seen, t0());
    assert_eq!(record.last_seen, t0());
    assert_eq!(record.resolved_at, None);

    // The durable write happened before the caller could have acked (ADR 001 D2): the op
    // is leasable the moment ingest returns.
    assert_eq!(lease(store, "w1", t0()).await.len(), 1);
}

pub(crate) async fn the_alert_labels_and_annotations_survive_the_round_trip<S: StateStore>(
    store: &S,
) {
    // Phase 3 renders from these columns, not from the webhook body, because the body is
    // long gone by the time the worker drains the op.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("the claim wrote a row");

    let expected: LabelMap = [
        ("alertname".to_owned(), "KubePodNotReady".to_owned()),
        ("severity".to_owned(), "critical".to_owned()),
    ]
    .into_iter()
    .collect();
    assert_eq!(record.labels, expected);
    assert_eq!(
        record.annotations.get("summary").map(String::as_str),
        Some("pod is not ready")
    );
}

pub(crate) async fn timestamps_round_trip_at_microsecond_precision<S: StateStore>(store: &S) {
    // PostgreSQL holds microseconds and SQLite holds nanoseconds. The store truncates on
    // the way in so both backends answer the same question the same way — without that,
    // this assertion would have to be two different assertions.
    let precise = t0() + TimeDelta::nanoseconds(123_456_789);
    ingest(store, &batch(vec![firing("abc")]), precise).await;

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("the claim wrote a row");
    assert_eq!(record.first_seen, t0() + TimeDelta::microseconds(123_456));
}

/// **ADR 001 D3, row 1** — "Alertmanager retries a batch after our timeout".
pub(crate) async fn redelivering_a_batch_does_not_post_it_twice<S: StateStore>(store: &S) {
    let batch = batch(vec![firing("abc")]);

    let first = ingest(store, &batch, t0()).await;
    let second = ingest(store, &batch, t0() + secs(2)).await;

    assert_eq!(posts(&first), 1);
    assert!(second.ops.is_empty(), "{:?}", second.ops);
    assert_eq!(
        lease(store, "w1", t0() + secs(2)).await.len(),
        1,
        "a redelivered batch must not add a second post to the queue"
    );
}

/// **ADR 001 D3, row 2** — "Two replicas receive different batches containing the same
/// fingerprint", run sequentially. The concurrent version is below.
pub(crate) async fn two_different_batches_sharing_a_fingerprint_post_it_once<S: StateStore>(
    store: &S,
) {
    let first = ingest(store, &batch(vec![firing("abc"), firing("def")]), t0()).await;
    let second = ingest(
        store,
        &batch(vec![firing("abc"), firing("ghi")]),
        t0() + secs(1),
    )
    .await;

    assert_eq!(posts(&first), 2);
    assert_eq!(
        posts(&second),
        1,
        "only the fingerprint this batch introduced is new"
    );

    let queued = lease(store, "w1", t0() + secs(1)).await;
    assert_eq!(queued.len(), 3, "abc, def, ghi — abc exactly once");
}

pub(crate) async fn the_same_fingerprint_in_two_channels_is_two_independent_alerts<
    S: StateStore,
>(
    store: &S,
) {
    // ADR 001 D4: the channel is part of the primary key. With a fingerprint-only key the
    // second of these would silently lose its message.
    let here = ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let there = ingest(
        store,
        &batch_in(OTHER_CHANNEL, GROUP, vec![firing("abc")]),
        t0(),
    )
    .await;

    assert_eq!(posts(&here), 1);
    assert_eq!(posts(&there), 1);
    assert!(
        store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(OTHER_CHANNEL))
            .await
            .expect("reading an alert")
            .is_some()
    );
}

// ---------------------------------------------------------------------------
// ADR 001 D7: the repeat-firing debounce
// ---------------------------------------------------------------------------

pub(crate) async fn a_repeat_after_the_debounce_queues_an_in_place_refresh<S: StateStore>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let repeat = ingest(
        store,
        &batch(vec![firing("abc")]),
        t0() + TimeDelta::hours(12),
    )
    .await;

    assert_eq!(
        repeat.ops,
        vec![Op::Refresh {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            message_ts: MessageTs::new("1721500001.000100"),
        }],
        "the refresh has to name the message the post actually produced"
    );
}

pub(crate) async fn a_repeat_inside_the_debounce_is_a_duplicate_delivery<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let repeat = ingest(store, &batch(vec![firing("abc")]), t0() + secs(30)).await;

    assert!(repeat.ops.is_empty(), "{:?}", repeat.ops);
    // Not a drop: the message is already in the channel and already says the right thing.
    // What moved is `last_seen`, which is what keeps the retention sweep honest.
    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.last_seen, t0() + secs(30));
    assert_eq!(record.state, AlertState::Posted);
}

pub(crate) async fn a_repeat_arriving_before_the_post_landed_queues_nothing_new<S: StateStore>(
    store: &S,
) {
    // The first post is still in the outbox. A second post op would be a second message.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let repeat = ingest(
        store,
        &batch(vec![firing("abc")]),
        t0() + TimeDelta::hours(12),
    )
    .await;

    assert!(repeat.ops.is_empty(), "{:?}", repeat.ops);
    assert_eq!(
        lease(store, "w1", t0() + TimeDelta::hours(12)).await.len(),
        1
    );
}

// ---------------------------------------------------------------------------
// ADR 001 D6 / D9: resolve
// ---------------------------------------------------------------------------

pub(crate) async fn resolving_a_posted_alert_targets_the_message_it_posted<S: StateStore>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let planned = ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;

    assert_eq!(
        planned.ops,
        vec![Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::Message {
                message_ts: MessageTs::new("1721500001.000100"),
                thread_parent_ts: None,
            },
            update_in_place: true,
            thread_reply: true,
        }]
    );

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Resolving);
    assert_eq!(record.resolved_at, Some(t0() + secs(300)));
}

pub(crate) async fn resolving_before_the_post_landed_defers_rather_than_dropping<S: StateStore>(
    store: &S,
) {
    // ADR 001 D9, row 4. The op is still queued: the worker self-defers until the post
    // lands, and falls back to a standalone message if it never does.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let planned = ingest(store, &batch(vec![resolved("abc")]), t0() + secs(1)).await;

    assert_eq!(
        planned.ops,
        vec![Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        }]
    );
}

pub(crate) async fn a_duplicate_resolution_is_recognised_as_one<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;
    ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;

    let again = ingest(store, &batch(vec![resolved("abc")]), t0() + secs(301)).await;

    assert!(again.ops.is_empty(), "{:?}", again.ops);
}

pub(crate) async fn a_resolution_for_an_untracked_fingerprint_still_posts_something<
    S: StateStore,
>(
    store: &S,
) {
    // PRD §5.5 and ADR 001 D9. The relay was down when it fired, or `max_alerts` truncated
    // it out of the body. Never silent.
    let planned = ingest(store, &batch(vec![resolved("ghost")]), t0()).await;

    assert_eq!(
        planned.ops,
        vec![Op::PostOrphanResolved {
            fingerprint: Fingerprint::new("ghost"),
            channel: channel(),
        }]
    );
    assert_eq!(lease(store, "w1", t0()).await.len(), 1);
}

pub(crate) async fn an_alert_that_fires_again_after_resolving_is_posted_again<S: StateStore>(
    store: &S,
) {
    // ADR 001 D2's classification table stops at `claimed` and `posted` and does not say
    // what a firing delivery for an already-resolved row means. Treating it as a duplicate
    // would be silence: the message it would duplicate is green. The row is taken back
    // over and the alert is posted afresh.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;
    ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;
    deliver(store, t0() + secs(300)).await;

    let refired = ingest(
        store,
        &batch(vec![firing("abc")]),
        t0() + TimeDelta::hours(6),
    )
    .await;

    assert_eq!(posts(&refired), 1, "{:?}", refired.ops);
    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Claimed);
    assert_eq!(record.message_ts, None, "the green message is not reused");
    assert_eq!(record.resolved_at, None);
    assert_eq!(record.first_seen, t0() + TimeDelta::hours(6));
}

pub(crate) async fn a_resolution_after_a_dead_lettered_post_is_an_orphan_not_a_duplicate<
    S: StateStore,
>(
    store: &S,
) {
    // The alert's own message never reached Slack. If its resolution were then treated as
    // a duplicate resolution, the alert *and* its resolution would both be silent — which
    // is the compound version of the failure this project exists to prevent.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    store
        .dead_letter(leased[0].id, "invalid_auth", t0() + secs(10))
        .await
        .expect("dead-lettering a leased op");

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Failed);

    let planned = ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;
    assert_eq!(
        planned.ops,
        vec![Op::PostOrphanResolved {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
        }]
    );
}

pub(crate) async fn an_alert_that_fires_again_after_dead_lettering_is_posted_again<
    S: StateStore,
>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    store
        .dead_letter(leased[0].id, "invalid_auth", t0() + secs(10))
        .await
        .expect("dead-lettering a leased op");

    let refired = ingest(
        store,
        &batch(vec![firing("abc")]),
        t0() + TimeDelta::hours(12),
    )
    .await;

    assert_eq!(posts(&refired), 1, "{:?}", refired.ops);
}

pub(crate) async fn an_empty_delivery_writes_nothing<S: StateStore>(store: &S) {
    let planned = ingest(store, &batch(Vec::new()), t0()).await;

    assert!(planned.ops.is_empty());
    assert!(lease(store, "w1", t0()).await.is_empty());
}

// ---------------------------------------------------------------------------
// ADR 001 D3, rows 2 and 3: genuine concurrency
// ---------------------------------------------------------------------------

/// **ADR 001 D3, row 3** — "Two replicas race the identical batch".
///
/// N tasks, released together by a barrier so they are genuinely in flight at the same
/// time, all ingesting the same fingerprint. Exactly one `post` op may result.
///
/// The mechanism differs per backend and the assertion does not: SQLite serialises the
/// transactions on `BEGIN IMMEDIATE` and PostgreSQL on the primary key plus a row lock.
/// Either way, Slack has no idempotency key on `chat.postMessage` (D3), so a second op here
/// is a second message in the channel.
pub(crate) async fn n_tasks_racing_one_fingerprint_produce_exactly_one_post<S>(store: &S)
where
    S: StateStore + Clone + 'static,
{
    const RACERS: usize = 8;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut tasks = Vec::with_capacity(RACERS);

    for _ in 0..RACERS {
        let store = store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let batch = batch(vec![firing("abc")]);
            let planned = ingest(&store, &batch, t0()).await;
            posts(&planned)
        }));
    }

    let mut total = 0;
    for task in tasks {
        total += task.await.expect("no racer panicked");
    }

    assert_eq!(
        total, 1,
        "{RACERS} concurrent ingests of one fingerprint produced {total} post ops"
    );
    assert_eq!(
        lease(store, "w1", t0()).await.len(),
        1,
        "and the queue holds exactly one"
    );
}

/// **ADR 001 D3, row 2** — "Two replicas receive different batches containing the same
/// fingerprint", concurrently.
pub(crate) async fn racing_batches_that_overlap_post_each_fingerprint_once<S>(store: &S)
where
    S: StateStore + Clone + 'static,
{
    const RACERS: usize = 6;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut tasks = Vec::with_capacity(RACERS);

    for n in 0..RACERS {
        let store = store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            // Every batch carries the shared fingerprint plus one of its own.
            let batch = batch(vec![firing("shared"), firing(&format!("own-{n}"))]);
            ingest(&store, &batch, t0()).await;
        }));
    }

    for task in tasks {
        task.await.expect("no racer panicked");
    }

    let queued = lease(store, "w1", t0()).await;
    assert_eq!(
        queued.len(),
        RACERS + 1,
        "one post per distinct fingerprint: {RACERS} private plus the shared one"
    );
}

/// **ADR 001 D7 under HA** — the repeat-firing debounce is a read-then-write, and it has to
/// be one.
///
/// This is the test that holds each backend's exclusion primitive in place. The claim reads
/// `last_seen`, decides whether the delivery is a `repeat_interval` re-send, and writes the
/// new `last_seen`; if two of those interleave, both read the *old* value and both queue a
/// refresh, and the alert's message gets edited twice against a 50-per-minute tier limit.
///
/// The two backends reach exclusion by different routes and this test is the one that
/// checks each of them:
///
/// - **PostgreSQL** needs `FOR UPDATE` on the probe. Under `READ COMMITTED` two replicas
///   otherwise read the same pre-update snapshot and both decide the debounce has elapsed.
///   Deleting those two words was tried against this test, and it fails.
/// - **SQLite** is protected because a transaction that has already written holds the
///   database's write lock, and every ingest's first statement is the claim's `INSERT`.
///   `BEGIN IMMEDIATE` is what ADR 001 D2 specifies and is what makes that independent of
///   statement order rather than a consequence of it — so removing it does *not* fail this
///   test today, and reordering `ingest` to read before writing would.
pub(crate) async fn n_tasks_racing_a_repeat_produce_exactly_one_refresh<S>(store: &S)
where
    S: StateStore + Clone + 'static,
{
    const RACERS: usize = 6;

    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let later = t0() + TimeDelta::hours(12);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut tasks = Vec::with_capacity(RACERS);

    for _ in 0..RACERS {
        let store = store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let batch = batch(vec![firing("abc")]);
            let planned = ingest(&store, &batch, later).await;
            planned
                .ops
                .iter()
                .filter(|op| matches!(op, Op::Refresh { .. }))
                .count()
        }));
    }

    let mut refreshes = 0;
    for task in tasks {
        refreshes += task.await.expect("no racer panicked");
    }

    assert_eq!(
        refreshes, 1,
        "{RACERS} simultaneous repeat deliveries produced {refreshes} refreshes; the \
         debounce read the same stale last_seen more than once"
    );
}

/// **ADR 001 D6 under HA** — one resolution, however many replicas hear about it.
pub(crate) async fn n_tasks_racing_a_resolution_produce_exactly_one_resolve<S>(store: &S)
where
    S: StateStore + Clone + 'static,
{
    const RACERS: usize = 6;

    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let later = t0() + secs(300);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut tasks = Vec::with_capacity(RACERS);

    for _ in 0..RACERS {
        let store = store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let batch = batch(vec![resolved("abc")]);
            let planned = ingest(&store, &batch, later).await;
            planned
                .ops
                .iter()
                .filter(|op| matches!(op, Op::Resolve { .. }))
                .count()
        }));
    }

    let mut resolves = 0;
    for task in tasks {
        resolves += task.await.expect("no racer panicked");
    }

    assert_eq!(
        resolves, 1,
        "a resolution delivered to {RACERS} replicas at once produced {resolves} resolve ops"
    );
}

/// **ADR 001 D5 under HA** — two replicas both deciding to collapse the same group must
/// still produce one summary message.
pub(crate) async fn racing_batches_that_both_collapse_open_one_group<S>(store: &S)
where
    S: StateStore + Clone + 'static,
{
    const RACERS: usize = 4;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut tasks = Vec::with_capacity(RACERS);

    for n in 0..RACERS {
        let store = store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let alerts = (0..6).map(|i| firing(&format!("r{n}-f{i}"))).collect();
            let planned = ingest(&store, &batch(alerts), t0()).await;
            planned
                .ops
                .iter()
                .filter(|op| matches!(op, Op::PostGroup { .. }))
                .count()
        }));
    }

    let mut parents = 0;
    for task in tasks {
        parents += task.await.expect("no racer panicked");
    }

    assert_eq!(
        parents, 1,
        "a storm hitting {RACERS} replicas at once must produce one summary, not {parents}"
    );

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group was opened");
    assert_eq!(
        group.member_count,
        i32::try_from(RACERS * 6).expect("small"),
        "every racer's children joined the one group that was opened"
    );
}

// ---------------------------------------------------------------------------
// ADR 001 D5: storm collapse
// ---------------------------------------------------------------------------

pub(crate) async fn a_batch_above_the_threshold_opens_a_group_and_threads_its_children<
    S: StateStore,
>(
    store: &S,
) {
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    let planned = ingest(store, &batch(alerts), t0()).await;

    assert!(matches!(planned.ops.first(), Some(Op::PostGroup { .. })));
    assert_eq!(posts(&planned), 6);

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group was opened");
    assert_eq!(group.member_count, 6);
    assert_eq!(
        group.message_ts, None,
        "the parent's own post is still queued"
    );
    assert_eq!(group.created_at, t0());
}

pub(crate) async fn a_late_alert_sticks_to_a_group_that_already_exists<S: StateStore>(store: &S) {
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    let late = ingest(store, &batch(vec![firing("late")]), t0() + secs(60)).await;

    let threaded = late.ops.iter().any(|op| {
        matches!(
            op,
            Op::Post {
                placement: Placement::Thread { .. },
                ..
            }
        )
    });
    assert!(threaded, "{:?}", late.ops);

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group exists");
    assert_eq!(group.member_count, 7, "the late alert joined");
    assert!(group.message_ts.is_some(), "the parent has been posted");
}

pub(crate) async fn a_groups_labels_are_stored_when_it_is_opened<S: StateStore>(store: &S) {
    // The whole point of the column. Without it a summary can only name its group by
    // string-parsing Alertmanager's `groupKey`, which is that project's internal
    // serialisation and yields nothing when `alertname` is not in `group_by`.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group was opened");

    assert_eq!(group.group_labels, group_labels());
}

pub(crate) async fn a_group_opened_with_no_group_labels_stores_an_empty_map<S: StateStore>(
    store: &S,
) {
    // `group_by: []` is a legitimate Alertmanager configuration. The column is NOT NULL, so
    // the empty case has to be an empty map — a group that could not be written is a
    // storm-collapse parent that never posts, which is silence.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    let batch = batch_labelled(CHANNEL, GROUP, LabelMap::new(), alerts);
    ingest(store, &batch, t0()).await;

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group was opened");

    assert_eq!(group.group_labels, LabelMap::new());
    assert_eq!(group.member_count, 6, "the group is otherwise intact");
}

pub(crate) async fn a_later_batch_joining_a_group_does_not_rewrite_its_labels<S: StateStore>(
    store: &S,
) {
    // Write-once, asserted rather than assumed. The labels are what *defines* the group —
    // a different `group_by` is a different group key and so a different row — so the join
    // has nothing correct to say about them, and a join that rewrote them could only ever
    // replace them with something wrong.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    // Deliberately different labels, which a real Alertmanager could not send for this
    // group key. That is exactly what makes the assertion below observable.
    let late = batch_labelled(
        CHANNEL,
        GROUP,
        [("alertname".to_owned(), "SomethingElse".to_owned())]
            .into_iter()
            .collect(),
        vec![firing("late")],
    );
    ingest(store, &late, t0() + secs(60)).await;

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group exists");

    assert_eq!(group.member_count, 7, "the late alert still joined");
    assert_eq!(
        group.group_labels,
        group_labels(),
        "a rejoin must not overwrite the labels the group was opened with"
    );
}

pub(crate) async fn resolving_a_collapsed_child_edits_the_childs_own_message<S: StateStore>(
    store: &S,
) {
    // ADR 001 D5's correctness claim: collapse changes visual placement only, and per-alert
    // resolve still updates the right message in place.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    let record = store
        .alert(&Fingerprint::new("f0"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Posted);
    let own_message = record.message_ts.clone().expect("the child was posted");
    assert!(
        record.thread_parent_ts.is_some(),
        "a collapsed child records the parent it hangs under"
    );

    let planned = ingest(store, &batch(vec![resolved("f0")]), t0() + secs(300)).await;
    let resolve = planned
        .ops
        .iter()
        .find(|op| matches!(op, Op::Resolve { .. }))
        .expect("the child resolves");

    match resolve {
        Op::Resolve {
            target: ResolveTarget::Message { message_ts, .. },
            ..
        } => assert_eq!(message_ts, &own_message),
        other => panic!("expected a resolve against the child's own message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The worker lease (ADR 001 D2) and D3's rows 4 and 5
// ---------------------------------------------------------------------------

pub(crate) async fn work_is_handed_to_one_worker_at_a_time<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;

    let first = lease(store, "w1", t0()).await;
    let second = lease(store, "w2", t0()).await;

    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempts, 1, "the lease itself counts the attempt");
    assert!(
        second.is_empty(),
        "a leased row is not a candidate: {second:?}"
    );
}

pub(crate) async fn a_lease_hands_back_the_op_that_was_planned<S: StateStore>(store: &S) {
    let planned = ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;

    assert_eq!(leased.len(), 1);
    assert_eq!(
        leased[0].op, planned.ops[0],
        "an op that does not survive the outbox is an alert that arrives as something else"
    );
    assert_eq!(leased[0].created_at, t0());
    assert_eq!(leased[0].leased_until, t0() + secs(60));
}

/// **ADR 001 D3, row 4** — "Worker crashes mid-post: lease expires, row reclaimed, retried".
pub(crate) async fn a_dead_workers_lease_expires_and_the_row_is_reclaimed<S: StateStore>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;

    let first = lease(store, "doomed", t0()).await;
    assert_eq!(first.len(), 1);

    // The worker dies here. It never completes, never defers, never dead-letters.
    assert!(
        lease(store, "healthy", t0() + secs(30)).await.is_empty(),
        "the lease is still live at half its duration"
    );

    let reclaimed = lease(store, "healthy", t0() + secs(61)).await;
    assert_eq!(reclaimed.len(), 1, "the row must not be stuck for ever");
    assert_eq!(reclaimed[0].id, first[0].id, "it is the same work");
    assert_eq!(
        reclaimed[0].attempts, 2,
        "the dead worker's attempt still counted, so a row that kills its worker every \
         time still reaches the dead-letter queue"
    );
}

pub(crate) async fn a_lease_expires_at_the_instant_it_says_it_does<S: StateStore>(store: &S) {
    // The boundary is decided here rather than left to whichever comparison someone typed.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    lease(store, "doomed", t0()).await;

    assert!(
        lease(store, "healthy", t0() + secs(59)).await.is_empty(),
        "one second before expiry the lease still holds"
    );
    assert_eq!(
        lease(store, "healthy", t0() + secs(60)).await.len(),
        1,
        "at the expiry instant the row is reclaimable"
    );
}

/// **ADR 001 D3, row 5** — "Worker posts to Slack then crashes before writing `message_ts`:
/// duplicate message".
///
/// This is the one genuinely unresolvable case in the design, and the test exists to pin
/// the *direction* the store fails in rather than to pretend it is solved. The op comes
/// back, so the message is posted a second time. What must never happen is the op not
/// coming back.
pub(crate) async fn a_worker_that_posts_then_dies_has_its_work_redelivered<S: StateStore>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let first = lease(store, "doomed", t0()).await;

    // Slack has the message. We do not have its timestamp, and never will.
    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.message_ts, None);
    assert_eq!(record.state, AlertState::Claimed);

    let redelivered = lease(store, "healthy", t0() + secs(61)).await;
    assert_eq!(
        redelivered.len(),
        1,
        "duplicate, never silence — the op has to come back"
    );
    assert_eq!(redelivered[0].id, first[0].id);
}

pub(crate) async fn a_rate_limited_op_gives_its_attempt_back<S: StateStore>(store: &S) {
    // ADR 001 D2: a 429 is Slack telling us to come back later, not a failure of the op.
    // Counting it would march a popular alert toward the dead-letter queue.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    assert_eq!(leased[0].attempts, 1);

    store
        .defer(
            leased[0].id,
            &Deferral::RateLimited {
                until: t0() + secs(30),
            },
        )
        .await
        .expect("deferring a leased op");

    assert!(
        lease(store, "w1", t0() + secs(29)).await.is_empty(),
        "Retry-After is honoured"
    );
    let again = lease(store, "w1", t0() + secs(30)).await;
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].attempts, 1,
        "the rate-limited lease gave its attempt back, so this is still attempt one"
    );
}

pub(crate) async fn a_backed_off_op_keeps_its_attempt<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;

    store
        .defer(
            leased[0].id,
            &Deferral::Backoff {
                until: t0() + secs(30),
                error: "slack 503".to_owned(),
            },
        )
        .await
        .expect("deferring a leased op");

    assert!(lease(store, "w1", t0() + secs(29)).await.is_empty());
    let again = lease(store, "w1", t0() + secs(30)).await;
    assert_eq!(again[0].attempts, 2, "a real failure counts");
}

pub(crate) async fn deferring_an_op_that_is_already_gone_says_so<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    store
        .complete(
            leased[0].id,
            &OpEffect::Posted {
                message_ts: MessageTs::new("1.1"),
                thread_parent_ts: None,
            },
            t0(),
        )
        .await
        .expect("completing a leased op");

    let error = store
        .defer(
            leased[0].id,
            &Deferral::Backoff {
                until: t0() + secs(30),
                error: "too late".to_owned(),
            },
        )
        .await
        .expect_err("the row is gone");

    // Loud rather than a silent no-op: this is a lease that outlived its row, and Phase 4
    // counts it rather than guessing at it.
    assert!(matches!(error, StoreError::NoSuchOp(id) if id == leased[0].id));
}

pub(crate) async fn completing_an_op_twice_says_so<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    let effect = OpEffect::Posted {
        message_ts: MessageTs::new("1721500001.000100"),
        thread_parent_ts: None,
    };

    store
        .complete(leased[0].id, &effect, t0())
        .await
        .expect("the first completion");
    let error = store
        .complete(leased[0].id, &effect, t0())
        .await
        .expect_err("the second completion has nothing to complete");

    assert!(matches!(error, StoreError::NoSuchOp(_)), "{error}");
}

pub(crate) async fn a_dead_lettered_op_is_never_leased_again<S: StateStore>(store: &S) {
    // ADR 001 D9: an op that has exhausted its attempts stops being retried. Without a
    // column for that, `attempts` alone would let the lease hand the same doomed row out
    // for ever, starving everything behind it.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;

    store
        .dead_letter(leased[0].id, "attempts exhausted", t0() + secs(10))
        .await
        .expect("dead-lettering a leased op");

    assert!(
        lease(store, "w1", t0() + TimeDelta::days(1))
            .await
            .is_empty(),
        "a parked row is not work"
    );
}

pub(crate) async fn dead_lettering_an_op_that_is_gone_says_so<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    store
        .complete(leased[0].id, &OpEffect::Refreshed, t0())
        .await
        .expect("completing a leased op");

    let error = store
        .dead_letter(leased[0].id, "too late", t0())
        .await
        .expect_err("the row is gone");
    assert!(matches!(error, StoreError::NoSuchOp(_)), "{error}");
}

pub(crate) async fn dead_lettering_a_resolve_leaves_the_alert_alone<S: StateStore>(store: &S) {
    // Only a *post* failing means the alert never reached Slack. A resolve dead-lettering
    // leaves a message in the channel that is merely stale, and marking the alert `failed`
    // would make its next firing delivery re-post an alert that is already visible.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;
    ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;
    let leased = lease(store, "w1", t0() + secs(300)).await;

    store
        .dead_letter(leased[0].id, "message_not_found", t0() + secs(400))
        .await
        .expect("dead-lettering a leased op");

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Resolving);
}

pub(crate) async fn the_lease_honours_its_limit_and_takes_the_oldest_work_first<S: StateStore>(
    store: &S,
) {
    for i in 0..5 {
        ingest(
            store,
            &batch_in(CHANNEL, GROUP, vec![firing(&format!("f{i}"))]),
            t0() + secs(i),
        )
        .await;
    }

    let first = store
        .lease_batch(&worker("w1"), 2, secs(60), t0() + secs(10))
        .await
        .expect("leasing");
    assert_eq!(first.len(), 2);

    let second = store
        .lease_batch(&worker("w2"), 2, secs(60), t0() + secs(10))
        .await
        .expect("leasing");
    assert_eq!(second.len(), 2);
    assert!(
        first[0].id < second[0].id,
        "the queue is drained oldest first, or a storm starves its own tail"
    );
}

pub(crate) async fn a_lease_hands_out_its_batch_oldest_first<S: StateStore>(store: &S) {
    // ADR 001 D5: "the parent posts immediately; children fill in at 1/sec". That only
    // holds if the worker is handed the parent first, and both backends' lease statements
    // order the rows they *select* without saying anything about the order `RETURNING`
    // hands them back in. PostgreSQL does not preserve it; SQLite happened to. The suite
    // found that, and this is what stops it coming back.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;

    let leased = lease(store, "w1", t0()).await;

    assert!(
        matches!(leased.first().map(|op| &op.op), Some(Op::PostGroup { .. })),
        "the storm-collapse parent has to come out first: {:?}",
        leased.iter().map(|op| &op.op).collect::<Vec<_>>()
    );
    let ids: Vec<_> = leased.iter().map(|op| op.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "a lease is handed out oldest first");
}

/// The SQLite-specific hazard, asserted identically on both backends.
///
/// SQLite stores `TIMESTAMPTZ` as RFC 3339 text and compares it byte by byte, and sqlx
/// writes a variable number of sub-second digits — none, three, six or nine — depending on
/// the value. If that encoding were not order-preserving, `next_attempt_at <= now` would
/// quietly match the wrong rows, and the symptom would be an alert that is never leased.
///
/// The timestamps below are inside one second, chosen so that a naive byte comparison of
/// unequal-length fractions is the thing under test. On PostgreSQL the assertion is
/// trivially true, which is the point: it is the same assertion.
pub(crate) async fn lease_ordering_survives_variable_sub_second_precision<S: StateStore>(
    store: &S,
) {
    let moments = [
        t0(),                                    // no fractional digits at all
        t0() + TimeDelta::milliseconds(500),     // three
        t0() + TimeDelta::microseconds(500_001), // six
        t0() + TimeDelta::microseconds(999_999), // six, and later than `now` below
    ];
    for (i, at) in moments.iter().enumerate() {
        ingest(store, &batch(vec![firing(&format!("f{i}"))]), *at).await;
    }

    let ready = lease(store, "w1", t0() + TimeDelta::microseconds(500_001)).await;

    assert_eq!(
        ready.len(),
        3,
        "everything at or before the lease instant is ready, and nothing after it is"
    );
}

// ---------------------------------------------------------------------------
// Completion effects
// ---------------------------------------------------------------------------

pub(crate) async fn completing_a_post_records_its_message_and_empties_the_queue<S: StateStore>(
    store: &S,
) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;

    store
        .complete(
            leased[0].id,
            &OpEffect::Posted {
                message_ts: MessageTs::new("1721500001.000100"),
                thread_parent_ts: None,
            },
            t0() + secs(1),
        )
        .await
        .expect("completing a leased op");

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Posted);
    assert_eq!(record.message_ts, Some(MessageTs::new("1721500001.000100")));

    // ADR 001 D4: completed rows are deleted inline, not swept later.
    assert!(
        lease(store, "w1", t0() + TimeDelta::days(1))
            .await
            .is_empty()
    );
}

pub(crate) async fn a_post_that_lands_after_its_resolve_records_the_timestamp_without_reviving_it<
    S,
>(
    store: &S,
) where
    S: StateStore,
{
    // The resolve arrived while the post was still queued (ADR 001 D9, row 4). When the
    // post finally lands, the alert must keep its `resolving` state — otherwise the
    // resolution would be lost — but it must still record `message_ts`, because that is
    // the handle the deferred resolve is waiting for.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;
    ingest(store, &batch(vec![resolved("abc")]), t0() + secs(1)).await;

    store
        .complete(
            leased[0].id,
            &OpEffect::Posted {
                message_ts: MessageTs::new("1721500001.000100"),
                thread_parent_ts: None,
            },
            t0() + secs(2),
        )
        .await
        .expect("completing a leased op");

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Resolving);
    assert_eq!(record.message_ts, Some(MessageTs::new("1721500001.000100")));
}

pub(crate) async fn completing_a_group_post_gives_the_parent_its_timestamp<S: StateStore>(
    store: &S,
) {
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;

    let leased = lease(store, "w1", t0()).await;
    let parent = leased
        .iter()
        .find(|op| matches!(op.op, Op::PostGroup { .. }))
        .expect("the parent is queued");

    store
        .complete(
            parent.id,
            &OpEffect::GroupPosted {
                message_ts: ThreadTs::new("1721500000.000001"),
            },
            t0(),
        )
        .await
        .expect("completing a leased op");

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group exists");
    assert_eq!(group.message_ts, Some(ThreadTs::new("1721500000.000001")));
}

pub(crate) async fn completing_a_resolve_marks_the_alert_resolved<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;
    ingest(store, &batch(vec![resolved("abc")]), t0() + secs(300)).await;
    deliver(store, t0() + secs(301)).await;

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Resolved);
    assert_eq!(
        record.resolved_at,
        Some(t0() + secs(300)),
        "the resolution is dated when Alertmanager told us, not when Slack was updated"
    );
}

pub(crate) async fn a_lost_message_is_forgotten_and_replaced_in_the_same_transaction<
    S: StateStore,
>(
    store: &S,
) {
    // ADR 001 D7 and D9: `chat.update` returning `message_not_found` is a free liveness
    // probe on our own correlation state. Clearing the stale timestamp without queuing the
    // replacement would leave the relay with no message and no plan to make one — which is
    // the shape of a silent alert. The two happen together or not at all.
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;
    let repeat = ingest(
        store,
        &batch(vec![firing("abc")]),
        t0() + TimeDelta::hours(12),
    )
    .await;
    assert!(matches!(repeat.ops.first(), Some(Op::Refresh { .. })));

    let leased = lease(store, "w1", t0() + TimeDelta::hours(12)).await;
    store
        .complete(
            leased[0].id,
            &OpEffect::MessageLost,
            t0() + TimeDelta::hours(12),
        )
        .await
        .expect("completing a leased op");

    let record = store
        .alert(&Fingerprint::new("abc"), &channel())
        .await
        .expect("reading an alert")
        .expect("row exists");
    assert_eq!(record.message_ts, None, "the stale timestamp is forgotten");
    assert_eq!(record.state, AlertState::Claimed);

    let replacement = lease(store, "w1", t0() + TimeDelta::hours(12)).await;
    assert_eq!(replacement.len(), 1, "a fresh post is queued");
    assert_eq!(
        replacement[0].op,
        Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            placement: Placement::Channel,
        },
        "top level: `message_not_found` says our state is stale, and any thread parent we \
         remember came from the same state"
    );
}

pub(crate) async fn a_lost_group_summary_is_forgotten_and_replaced_too<S: StateStore>(store: &S) {
    // The same self-heal as above, for the storm-collapse parent. Somebody deleted the
    // summary message; the children still have their own messages and are fine, but the
    // group's `message_ts` now points at nothing — so without this, every later
    // `RefreshGroup` would fail the same way for as long as the group lives.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    // A late alert joins, which is what queues a RefreshGroup against the parent.
    ingest(store, &batch(vec![firing("late")]), t0() + secs(60)).await;
    let leased = lease(store, "w1", t0() + secs(60)).await;
    let refresh = leased
        .iter()
        .find(|op| matches!(op.op, Op::RefreshGroup { .. }))
        .expect("a joining member refreshes the parent's count");

    store
        .complete(refresh.id, &OpEffect::MessageLost, t0() + secs(60))
        .await
        .expect("completing a leased op");

    let group = store
        .group(&group_key(), &channel())
        .await
        .expect("reading a group")
        .expect("the group still exists");
    assert_eq!(group.message_ts, None, "the stale timestamp is forgotten");

    let replacement = lease(store, "w2", t0() + secs(120)).await;
    let reposted = replacement
        .iter()
        .find(|op| matches!(op.op, Op::PostGroup { .. }))
        .expect("a fresh summary is queued");
    assert_eq!(
        reposted.op,
        Op::PostGroup {
            group_key: group_key(),
            channel: channel(),
            initial_members: 7,
        },
        "the replacement summary carries the members the group actually has, not zero"
    );
}

pub(crate) async fn completing_an_orphan_post_leaves_no_correlation_state_behind<S: StateStore>(
    store: &S,
) {
    // There is nothing to correlate an orphan to — that is what makes it an orphan. The
    // variant exists so "nothing to record" is stated rather than inferred.
    ingest(store, &batch(vec![resolved("ghost")]), t0()).await;
    let leased = lease(store, "w1", t0()).await;

    store
        .complete(leased[0].id, &OpEffect::Standalone, t0())
        .await
        .expect("completing a leased op");

    assert!(
        store
            .alert(&Fingerprint::new("ghost"), &channel())
            .await
            .expect("reading an alert")
            .is_none()
    );
    assert!(
        lease(store, "w1", t0() + TimeDelta::days(1))
            .await
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Retention (ADR 001 D4; PRD §5.7)
// ---------------------------------------------------------------------------

/// Drives one alert all the way to `resolved`, with an empty queue behind it.
async fn resolve_fully<S: StateStore>(store: &S, fingerprint: &str, at: DateTime<Utc>) {
    ingest(
        store,
        &batch(vec![firing(fingerprint)]),
        at - TimeDelta::hours(1),
    )
    .await;
    deliver(store, at - TimeDelta::hours(1)).await;
    ingest(store, &batch(vec![resolved(fingerprint)]), at).await;
    deliver(store, at).await;
}

pub(crate) async fn resolved_alerts_older_than_the_policy_are_deleted<S: StateStore>(store: &S) {
    resolve_fully(store, "old", t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(8))
        .await
        .expect("pruning");

    assert_eq!(stats.resolved_alerts, 1);
    assert_eq!(stats.stale_alerts, 0);
    assert!(
        store
            .alert(&Fingerprint::new("old"), &channel())
            .await
            .expect("reading an alert")
            .is_none()
    );
}

pub(crate) async fn a_resolved_alert_inside_the_retention_window_survives<S: StateStore>(
    store: &S,
) {
    resolve_fully(store, "recent", t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(6))
        .await
        .expect("pruning");

    assert!(stats.is_empty(), "{stats:?}");
    assert!(
        store
            .alert(&Fingerprint::new("recent"), &channel())
            .await
            .expect("reading an alert")
            .is_some()
    );
}

pub(crate) async fn an_alert_that_fires_and_never_resolves_is_eventually_deleted<S: StateStore>(
    store: &S,
) {
    // ADR 001 D4's stale sweep. Without it a single misbehaving rule pins a row for ever,
    // which on a SQLite deployment means a PVC that grows without bound.
    ingest(store, &batch(vec![firing("forever")]), t0()).await;
    deliver(store, t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(31))
        .await
        .expect("pruning");

    assert_eq!(stats.stale_alerts, 1);
    assert_eq!(stats.resolved_alerts, 0, "it was never resolved");
    assert!(
        store
            .alert(&Fingerprint::new("forever"), &channel())
            .await
            .expect("reading an alert")
            .is_none()
    );
}

pub(crate) async fn an_alert_with_queued_work_is_never_pruned<S: StateStore>(store: &S) {
    // Deleting the row while its post is in flight would leave a message in Slack that
    // nothing is tracking, and its eventual resolution would surface as an orphan. The
    // guard holds however far past the policy the row is.
    ingest(store, &batch(vec![firing("busy")]), t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(365))
        .await
        .expect("pruning");

    assert!(stats.is_empty(), "{stats:?}");
    assert!(
        store
            .alert(&Fingerprint::new("busy"), &channel())
            .await
            .expect("reading an alert")
            .is_some()
    );
}

pub(crate) async fn a_group_with_no_surviving_members_is_deleted<S: StateStore>(store: &S) {
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(31))
        .await
        .expect("pruning");

    assert_eq!(stats.stale_alerts, 6);
    assert_eq!(stats.empty_groups, 1);
    assert!(
        store
            .group(&group_key(), &channel())
            .await
            .expect("reading a group")
            .is_none()
    );
}

pub(crate) async fn a_group_whose_members_survive_is_not_deleted<S: StateStore>(store: &S) {
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;
    deliver(store, t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(1))
        .await
        .expect("pruning");

    assert!(stats.is_empty(), "{stats:?}");
    assert!(
        store
            .group(&group_key(), &channel())
            .await
            .expect("reading a group")
            .is_some()
    );
}

pub(crate) async fn a_group_whose_parent_post_is_still_queued_is_not_deleted<S: StateStore>(
    store: &S,
) {
    // The parent's own post has not been delivered, so there is nowhere for its timestamp
    // to land if the row is deleted out from under it.
    let alerts = (0..6).map(|i| firing(&format!("f{i}"))).collect();
    ingest(store, &batch(alerts), t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(365))
        .await
        .expect("pruning");

    assert_eq!(stats.empty_groups, 0, "{stats:?}");
    assert!(
        store
            .group(&group_key(), &channel())
            .await
            .expect("reading a group")
            .is_some()
    );
}

pub(crate) async fn pruning_a_healthy_store_deletes_nothing<S: StateStore>(store: &S) {
    ingest(store, &batch(vec![firing("abc")]), t0()).await;
    deliver(store, t0()).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + secs(60))
        .await
        .expect("pruning");

    assert!(stats.is_empty(), "{stats:?}");
    assert!(
        store
            .prune(&RetentionPolicy::default(), t0() + secs(60))
            .await
            .expect("pruning")
            .is_empty()
    );
}

pub(crate) async fn pruning_leaves_other_groups_alone<S: StateStore>(store: &S) {
    // Two groups in one channel; only the one whose members are gone should go.
    let alerts = (0..6).map(|i| firing(&format!("a{i}"))).collect();
    ingest(store, &batch_in(CHANNEL, GROUP, alerts), t0()).await;
    let others = (0..6).map(|i| firing(&format!("b{i}"))).collect();
    ingest(
        store,
        &batch_in(CHANNEL, OTHER_GROUP, others),
        t0() + TimeDelta::days(30),
    )
    .await;
    deliver(store, t0() + TimeDelta::days(30)).await;

    let stats = store
        .prune(&RetentionPolicy::default(), t0() + TimeDelta::days(31))
        .await
        .expect("pruning");

    assert_eq!(stats.stale_alerts, 6, "only the first group's members");
    assert_eq!(stats.empty_groups, 1);
    assert!(
        store
            .group(&GroupKey::new(OTHER_GROUP), &channel())
            .await
            .expect("reading a group")
            .is_some()
    );
}
