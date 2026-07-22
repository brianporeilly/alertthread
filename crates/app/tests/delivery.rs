//! The outbox worker against a `wiremock` Slack: ADR 001 D9's failure table, executed.
//!
//! AGENTS.md's testing table requires a `wiremock` test for each specific Slack failure the
//! relay handles. Every row of D9 that reaches the worker is one test below, and each one
//! asserts on the *state the store was left in* rather than on the code path taken — the
//! question being answered is always "did the alert still get out, and can the next thing
//! that happens to it still work?"

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use alertthread_core::{AlertBatch, ChannelId, Fingerprint, GroupKey, LabelMap, Policy, plan};
use alertthread_store::{AlertState, StateStore, WorkerId};
use chrono::TimeDelta;
use harness::{
    CHANNEL, Harness, alert, payload, slack_error, slack_that_works, slack_with_auth_only, t0,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// Runs one delivery through the store exactly as the handler does.
async fn ingest(relay: &Harness, body: &str, at: chrono::DateTime<chrono::Utc>) {
    let payload: alertthread_core::WebhookPayload = serde_json::from_str(body).unwrap();
    let batch = AlertBatch::from_webhook(payload, ChannelId::new(CHANNEL));
    let policy = Policy::default();
    relay
        .store
        .ingest(&batch, at, |outcomes, group| {
            plan(outcomes, &batch, group, &policy, at)
        })
        .await
        .expect("ingest succeeds against a healthy store");
}

async fn record(relay: &Harness, fingerprint: &str) -> alertthread_store::AlertRecord {
    relay
        .store
        .alert(&Fingerprint::new(fingerprint), &ChannelId::new(CHANNEL))
        .await
        .expect("reading the store")
        .expect("the alert is tracked")
}

#[tokio::test]
async fn a_firing_alert_is_posted_and_its_timestamp_recorded() {
    // The timestamp is the whole basis of the project: without it there is nothing to
    // `chat.update` when the alert resolves.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-post", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    let pass = relay.drain_from(t0(), 5).await;

    assert_eq!(pass.completed, 1, "{pass:?}");
    assert_eq!(pass.dead_lettered, 0);
    let row = record(&relay, "abc").await;
    assert_eq!(row.state, AlertState::Posted);
    assert!(row.message_ts.is_some(), "the post's ts must be recorded");
    relay.assert_metric(
        "alertthread_slack_calls_total{method=\"chat.postMessage\",outcome=\"ok\"} 1",
    );
}

#[tokio::test]
async fn a_resolution_edits_the_message_in_place_and_replies_in_thread() {
    // ADR 001 D6: both, because they solve different problems. `chat.update` does not
    // notify, bump or mark a channel unread, so the edit alone is invisible to anybody
    // watching live; the reply is what generates the unread indicator.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-resolve", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;

    let later = t0() + TimeDelta::minutes(29);
    ingest(
        &relay,
        &payload("resolved", &[alert("abc", "resolved")]),
        later,
    )
    .await;
    let pass = relay.drain_from(later, 5).await;

    assert_eq!(pass.completed, 1, "{pass:?}");
    assert_eq!(record(&relay, "abc").await.state, AlertState::Resolved);

    let calls = slack.received_requests().await.unwrap();
    let updates = calls
        .iter()
        .filter(|r| r.url.path() == "/api/chat.update")
        .count();
    let posts = calls
        .iter()
        .filter(|r| r.url.path() == "/api/chat.postMessage")
        .count();
    assert_eq!(updates, 1, "the in-place edit");
    assert_eq!(posts, 2, "the original message, then the threaded reply");

    // The reply threads under the alert's own message and never broadcasts — broadcasting
    // would put back the channel noise that threading removed.
    let reply: serde_json::Value = calls
        .iter()
        .rfind(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert!(reply.get("thread_ts").is_some(), "{reply}");
    assert_eq!(reply["reply_broadcast"], serde_json::json!(false));
}

#[tokio::test]
async fn disabling_the_thread_reply_leaves_only_the_edit() {
    // ADR 001 D6 makes each half independently disableable, and the flags travel with the
    // op so a config reload cannot change the meaning of work already queued.
    let slack = slack_that_works().await;
    let relay = Harness::with_config(
        "delivery-editonly",
        &slack,
        "resolve:\n  update_in_place: true\n  thread_reply: false\n",
    )
    .await;

    let policy = relay.config.policy();
    let body = payload("firing", &[alert("abc", "firing")]);
    let parsed: alertthread_core::WebhookPayload = serde_json::from_str(&body).unwrap();
    let batch = AlertBatch::from_webhook(parsed, ChannelId::new(CHANNEL));
    relay
        .store
        .ingest(&batch, t0(), |o, g| plan(o, &batch, g, &policy, t0()))
        .await
        .unwrap();
    relay.drain_from(t0(), 5).await;

    let later = t0() + TimeDelta::minutes(29);
    let body = payload("resolved", &[alert("abc", "resolved")]);
    let parsed: alertthread_core::WebhookPayload = serde_json::from_str(&body).unwrap();
    let batch = AlertBatch::from_webhook(parsed, ChannelId::new(CHANNEL));
    relay
        .store
        .ingest(&batch, later, |o, g| plan(o, &batch, g, &policy, later))
        .await
        .unwrap();
    relay.drain_from(later, 5).await;

    let calls = slack.received_requests().await.unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|r| r.url.path() == "/api/chat.postMessage")
            .count(),
        1,
        "only the original message; no threaded reply"
    );
    assert_eq!(record(&relay, "abc").await.state, AlertState::Resolved);
}

#[tokio::test]
async fn a_storm_collapses_into_a_summary_with_its_alerts_threaded_under_it() {
    // ADR 001 D5. The parent posts first so the summary lands within a second while the
    // children fill in behind it at one per second — which is exactly what the loop below
    // simulates by advancing the clock a second per pass.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-storm", &slack).await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("f{i}"), "firing")).collect();
    ingest(&relay, &payload("firing", &alerts), t0()).await;
    let pass = relay.drain_from(t0(), 20).await;

    assert_eq!(pass.completed, 7, "one summary plus six children: {pass:?}");
    assert_eq!(pass.dead_lettered, 0);

    let group = relay
        .store
        .group(
            &GroupKey::new("{}:{alertname=\"CephOSDDown\"}"),
            &ChannelId::new(CHANNEL),
        )
        .await
        .expect("reading the group")
        .expect("a group was opened");
    assert!(group.message_ts.is_some(), "the summary posted");
    assert_eq!(group.member_count, 6);

    // Every child records the parent it hangs under, which is what makes per-alert resolve
    // still edit the right message in place (D5's correctness claim).
    for i in 0..6 {
        let child = record(&relay, &format!("f{i}")).await;
        assert_eq!(
            child.thread_parent_ts, group.message_ts,
            "child f{i} must thread under the summary"
        );
    }
}

#[tokio::test]
async fn a_slack_rate_limit_defers_the_op_without_spending_an_attempt() {
    // ADR 001 D2 and D9, and the single most important row of the table: if a 429 consumed
    // an attempt, an alert storm would dead-letter its own alerts.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&slack)
        .await;
    let relay = Harness::new("delivery-429", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    let pass = relay
        .worker
        .run_once(t0())
        .await
        .expect("the worker leases");

    assert_eq!(pass.deferred, 1, "{pass:?}");
    assert_eq!(pass.dead_lettered, 0);
    relay.assert_metric(
        "alertthread_rate_limited_total{method=\"chat.postMessage\",source=\"slack\"} 1",
    );

    // The attempt was given back, so the op is exactly where it started rather than one
    // step closer to the dead-letter queue.
    let leased = relay
        .store
        .lease_batch(
            &WorkerId::new("probe"),
            10,
            TimeDelta::seconds(60),
            t0() + TimeDelta::seconds(31),
        )
        .await
        .expect("leasing");
    assert_eq!(leased.len(), 1, "the op comes back after Retry-After");
    assert_eq!(
        leased[0].attempts, 1,
        "the 429 must not have counted: this is the second lease, so 1 means one was refunded"
    );
    assert_eq!(record(&relay, "abc").await.state, AlertState::Claimed);
}

#[tokio::test]
async fn an_invalid_token_dead_letters_immediately_rather_than_burning_retries() {
    // ADR 001 D9, verbatim: "dead-letter immediately, do not burn retries, fire a metric".
    // A token does not become valid by being tried ten more times, and the ten tries are
    // ten alerts' worth of worker capacity spent achieving nothing.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("invalid_auth"))
        .mount(&slack)
        .await;
    let relay = Harness::new("delivery-invalidauth", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    let pass = relay
        .worker
        .run_once(t0())
        .await
        .expect("the worker leases");

    assert_eq!(pass.dead_lettered, 1, "{pass:?}");
    relay.assert_metric("alertthread_dead_letter_total{reason=\"invalid_auth\"} 1");

    // The alert is marked failed, not resolved. That is what makes its eventual resolution
    // arrive as an orphan and post something, rather than being mistaken for a duplicate
    // resolution of a message that was never sent.
    assert_eq!(record(&relay, "abc").await.state, AlertState::Failed);

    // And the row is not leasable again: retrying for ever would be a queue that never
    // drains, which stalls every alert behind it.
    let leased = relay
        .store
        .lease_batch(
            &WorkerId::new("probe"),
            10,
            TimeDelta::seconds(60),
            t0() + TimeDelta::hours(1),
        )
        .await
        .expect("leasing");
    assert!(leased.is_empty(), "a parked row is never leased again");
}

#[tokio::test]
async fn a_transient_slack_failure_is_retried_and_eventually_parked() {
    // D9's "Slack 5xx" row. Both ends matter: it must not give up on the first failure, and
    // it must not retry for ever — a row that never stops retrying is a queue that never
    // drains.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("internal_error"))
        .mount(&slack)
        .await;
    let relay = Harness::new("delivery-5xx", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let mut now = t0();
    let mut dead = 0;
    for _ in 0..relay.config.worker.max_attempts + 2 {
        let pass = relay.worker.run_once(now).await.expect("the worker leases");
        dead += pass.dead_lettered;
        if dead > 0 {
            break;
        }
        // Far enough forward that the backoff has always elapsed.
        now += TimeDelta::minutes(30);
    }

    assert_eq!(dead, 1, "a permanently failing op is parked eventually");
    relay.assert_metric("alertthread_dead_letter_total{reason=\"slack_unavailable\"} 1");
    assert_eq!(record(&relay, "abc").await.state, AlertState::Failed);
}

#[tokio::test]
async fn a_message_slack_no_longer_has_is_forgotten_and_re_posted() {
    // ADR 001 D7's free liveness probe on our own correlation state, firing. The
    // replacement post is enqueued in the same transaction that cleared the timestamp, so a
    // crash between the two cannot leave a message nobody replaces.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "channel": "C1", "ts": "1784642520.000001"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(slack_error("message_not_found"))
        .mount(&slack)
        .await;
    let relay = Harness::new("delivery-messagegone", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;
    assert!(record(&relay, "abc").await.message_ts.is_some());

    // A repeat, twelve hours later, refreshes in place — and the edit finds nothing there.
    let later = t0() + TimeDelta::hours(12);
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), later).await;
    let pass = relay.drain_from(later, 5).await;

    assert_eq!(
        pass.dead_lettered, 0,
        "a lost message is healed, not parked"
    );
    let row = record(&relay, "abc").await;
    assert_eq!(
        row.state,
        AlertState::Posted,
        "the replacement post ran in the same drain and landed"
    );
    assert!(row.message_ts.is_some(), "and recorded a fresh timestamp");
}

#[tokio::test]
async fn a_resolution_for_an_alert_nobody_told_us_about_still_posts_something() {
    // PRD §5.5 and ADR 001 D9. The relay was down when it fired, or `max_alerts` truncated
    // it out of the firing notification. Never silent.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-orphan", &slack).await;

    ingest(
        &relay,
        &payload("resolved", &[alert("ghost", "resolved")]),
        t0(),
    )
    .await;
    let pass = relay.drain_from(t0(), 5).await;

    assert_eq!(pass.completed, 1, "{pass:?}");
    // The counter is the handler's job, not the worker's — the notice is produced by
    // `plan` at ingest. `endpoints::an_orphan_resolve_is_counted_where_the_notice_is_raised`
    // is where that half is checked.

    // ⚠️ The op carries a fingerprint and a channel and nothing else, so this is everything
    // the message can say. Recorded in the PR as a gap: `Op::PostOrphanResolved` has
    // nowhere to get the alert's labels from, because by definition there is no row.
    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("something was posted");
    let text = posted.to_string();
    assert!(
        text.contains("ghost"),
        "the fingerprint has to be in it: {text}"
    );
    assert!(
        text.contains("no record"),
        "and it has to say what it does not know: {text}"
    );

    // Nothing to correlate to, so nothing is left behind pretending there is.
    assert!(
        relay
            .store
            .alert(&Fingerprint::new("ghost"), &ChannelId::new(CHANNEL))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_resolution_that_overtakes_its_own_post_waits_for_it() {
    // ADR 001 D9's "resolve arrives while `message_ts` is NULL". Both ops are planned in
    // one batch; the resolve self-defers until the post's timestamp lands.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-overtake", &slack).await;

    let body = payload(
        "firing",
        &[alert("abc", "firing"), alert("abc", "resolved")],
    );
    ingest(&relay, &body, t0()).await;

    // One pass: the post goes out, the resolve is deferred by the rate limiter (the post
    // took this second's token) rather than being sent against a message that had no
    // timestamp when it was planned.
    let first = relay
        .worker
        .run_once(t0())
        .await
        .expect("the worker leases");
    assert_eq!(first.leased, 2, "{first:?}");
    assert_eq!(first.completed, 1);
    assert_eq!(first.deferred, 1);

    let after = relay.drain_from(t0() + TimeDelta::seconds(2), 10).await;
    assert_eq!(after.dead_lettered, 0, "{after:?}");
    assert_eq!(record(&relay, "abc").await.state, AlertState::Resolved);
}

#[tokio::test]
async fn a_second_alert_in_the_same_channel_waits_its_turn_rather_than_racing_slack() {
    // Slack allows roughly one `chat.postMessage` per second per channel. Pacing ourselves
    // costs one deferral; not pacing costs a 429, a round trip and an attempt's worth of
    // delay on an alert somebody is waiting for.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-paced", &slack).await;

    ingest(
        &relay,
        &payload("firing", &[alert("a", "firing"), alert("b", "firing")]),
        t0(),
    )
    .await;

    let pass = relay
        .worker
        .run_once(t0())
        .await
        .expect("the worker leases");
    assert_eq!(pass.leased, 2);
    assert_eq!(pass.completed, 1, "one message per second per channel");
    assert_eq!(pass.deferred, 1);
    relay.assert_metric(
        "alertthread_rate_limited_total{method=\"chat.postMessage\",source=\"local\"} 1",
    );

    // A second later the other one goes.
    let pass = relay
        .worker
        .run_once(t0() + TimeDelta::seconds(1))
        .await
        .expect("the worker leases");
    assert_eq!(pass.completed, 1, "{pass:?}");
}

#[tokio::test]
async fn two_channels_do_not_wait_for_each_other() {
    // The reason the worker fans out by channel instead of draining serially: Slack's limit
    // is per channel, so a single serial drain would let one busy channel starve every
    // other one for as long as its storm lasted.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-fanout", &slack).await;

    for channel in ["#alerts", "#alerts-critical", "#database"] {
        let body = payload("firing", &[alert(&format!("in-{channel}"), "firing")]);
        let parsed: alertthread_core::WebhookPayload = serde_json::from_str(&body).unwrap();
        let batch = AlertBatch::from_webhook(parsed, ChannelId::new(channel));
        let policy = Policy::default();
        relay
            .store
            .ingest(&batch, t0(), |o, g| plan(o, &batch, g, &policy, t0()))
            .await
            .unwrap();
    }

    let pass = relay
        .worker
        .run_once(t0())
        .await
        .expect("the worker leases");
    assert_eq!(
        pass.completed, 3,
        "three channels, three messages, one second: {pass:?}"
    );
    assert_eq!(pass.deferred, 0);
}

#[tokio::test]
async fn a_repeat_after_the_debounce_refreshes_the_message_rather_than_reposting() {
    // ADR 001 D7. With `repeatInterval: 12h`, a thread reply would add a notification twice
    // a day per long-running alert — working against the entire point of the project.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-repeat", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;

    let later = t0() + TimeDelta::hours(12);
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), later).await;
    let pass = relay.drain_from(later, 5).await;

    assert_eq!(pass.completed, 1, "{pass:?}");
    let calls = slack.received_requests().await.unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|r| r.url.path() == "/api/chat.postMessage")
            .count(),
        1,
        "the message stays where it is in history"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| r.url.path() == "/api/chat.update")
            .count(),
        1,
        "and is refreshed in place"
    );
}

#[tokio::test]
async fn a_broken_template_degrades_to_the_built_in_message_and_is_counted() {
    // ADR 001 D9's headline: a user-supplied template is the most likely thing to break in
    // production, and it must not be able to take alerting down.
    let slack = slack_that_works().await;
    let dir = std::env::temp_dir().join(format!("alertthread-badtpl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("firing.j2"), "{{ alert.labels | int }}").unwrap();

    let relay = Harness::with_config(
        "delivery-badtemplate",
        &slack,
        &format!("templates:\n  dir: {}\n", dir.display()),
    )
    .await;
    let (overrides, _) = relay.config.templates().unwrap();
    let (renderer, rejected) = alertthread_slack::Renderer::new(overrides);
    assert!(
        rejected.is_empty(),
        "this template compiles and fails at render"
    );

    let worker = alertthread::worker::Worker::new(
        std::sync::Arc::clone(&relay.store),
        std::sync::Arc::clone(&relay.slack),
        std::sync::Arc::new(renderer),
        std::sync::Arc::new(alertthread::ratelimit::SlackLimits::default()),
        std::sync::Arc::clone(&relay.metrics),
        relay.config.worker,
        WorkerId::new("degraded"),
    );

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    let pass = worker.run_once(t0()).await.expect("the worker leases");

    assert_eq!(pass.completed, 1, "the alert still posts: {pass:?}");
    relay.assert_metric("alertthread_fallback_posts_total{reason=\"render_failed\"} 1");

    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert!(
        posted.to_string().contains("CephOSDDown"),
        "the fallback still names the alert: {posted}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_store_sample_reports_what_the_queue_holds() {
    // ADR 001 D11's gauges, end to end: ingest, sample, and read the exposition.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-sample", &slack).await;

    ingest(
        &relay,
        &payload("firing", &[alert("a", "firing"), alert("b", "firing")]),
        t0(),
    )
    .await;

    let stats = relay.store.stats().await.expect("sampling");
    relay.metrics.publish(&stats, t0() + TimeDelta::seconds(40));

    relay.assert_metric("alertthread_outbox_depth{op=\"post\"} 2");
    relay.assert_metric("alertthread_outbox_depth{op=\"resolve\"} 0");
    relay.assert_metric("alertthread_outbox_oldest_age_seconds 40.0");
    relay.assert_metric("alertthread_tracked_fingerprints 2");
    relay.assert_metric("alertthread_store_sample_ok 1");
}

#[tokio::test]
async fn the_pruner_deletes_finished_state_and_leaves_queued_work_alone() {
    // ADR 001 D4's retention, on its own schedule. An `alert_message` deleted while its
    // post is in flight would be posted and then be untracked, turning its eventual
    // resolution into an orphan.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-prune", &slack).await;

    ingest(&relay, &payload("firing", &[alert("old", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;
    let resolved_at = t0() + TimeDelta::minutes(1);
    ingest(
        &relay,
        &payload("resolved", &[alert("old", "resolved")]),
        resolved_at,
    )
    .await;
    relay.drain_from(resolved_at, 5).await;

    // Something still queued, which the sweep must not touch.
    ingest(
        &relay,
        &payload("firing", &[alert("queued", "firing")]),
        resolved_at,
    )
    .await;

    let policy = relay.config.storage.retention.policy();
    let stats = relay
        .store
        .prune(&policy, t0() + TimeDelta::days(8))
        .await
        .expect("pruning");

    assert_eq!(stats.resolved_alerts, 1, "{stats:?}");
    assert!(
        relay
            .store
            .alert(&Fingerprint::new("queued"), &ChannelId::new(CHANNEL))
            .await
            .unwrap()
            .is_some(),
        "an alert with queued work is never pruned"
    );
}

#[tokio::test]
async fn a_group_summary_names_itself_from_the_labels_stored_when_it_opened() {
    // The `group_labels` column that landed before Phase 4 exists precisely so a
    // `RefreshGroup` planned hours later has something to render from: the op payload that
    // opened the group was deleted on completion.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-grouplabels", &slack).await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("g{i}"), "firing")).collect();
    ingest(&relay, &payload("firing", &alerts), t0()).await;
    relay.drain_from(t0(), 20).await;

    let group = relay
        .store
        .group(
            &GroupKey::new("{}:{alertname=\"CephOSDDown\"}"),
            &ChannelId::new(CHANNEL),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        group.group_labels,
        [("alertname".to_owned(), "CephOSDDown".to_owned())]
            .into_iter()
            .collect::<LabelMap>()
    );

    let summary: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert!(
        summary.to_string().contains("CephOSDDown"),
        "the summary has to name its group: {summary}"
    );
}

#[tokio::test]
async fn resolving_a_collapsed_child_refreshes_the_summarys_live_count() {
    // ADR 001 D5: the parent shows a live firing/resolved count. A summary still saying
    // "6 of 6 firing" over a thread of green replies is confidently wrong, which is worse
    // than uninformative.
    let slack = slack_that_works().await;
    let relay = Harness::new("delivery-groupcount", &slack).await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("c{i}"), "firing")).collect();
    ingest(&relay, &payload("firing", &alerts), t0()).await;
    relay.drain_from(t0(), 20).await;

    let later = t0() + TimeDelta::minutes(10);
    ingest(
        &relay,
        &payload("resolved", &[alert("c0", "resolved")]),
        later,
    )
    .await;
    relay.drain_from(later, 20).await;

    let membership = relay
        .store
        .group_membership(
            &GroupKey::new("{}:{alertname=\"CephOSDDown\"}"),
            &ChannelId::new(CHANNEL),
        )
        .await
        .expect("counting the group");
    assert_eq!(membership.firing, 5);
    assert_eq!(membership.resolved, 1);

    // The refreshed summary carries the new split.
    let updates: Vec<serde_json::Value> = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/chat.update")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(
        updates.iter().any(|u| u.to_string().contains("5 of 6")),
        "one of the edits should be the summary's new count: {updates:?}"
    );
}
