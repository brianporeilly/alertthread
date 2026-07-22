//! The branches a healthy relay never takes.
//!
//! Every case here is a state the outbox can genuinely reach — a row pruned out from under
//! an op, a storm-collapse parent that never posted, an edit that fails halfway through a
//! resolve — and every one of them is checked for the same property: **the alert still
//! gets out, or something says loudly that it did not.**
//!
//! These drive [`Delivery`] directly with hand-built [`LeasedOp`]s rather than going
//! through the worker. That is deliberate: several of these states cannot be produced
//! through the trait at all (nothing deletes an `alert_message` row with queued work), and
//! a test that contorted the store into reaching them would be a test of the contortion.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use alertthread_core::{
    AlertBatch, ChannelId, Fingerprint, GroupKey, MessageTs, Op, Placement, Policy, ResolveTarget,
    ThreadTs, plan,
};
use alertthread_slack::Renderer;
use alertthread_store::{LeasedOp, OpEffect, OpId, StateStore};
use chrono::TimeDelta;
use harness::{
    CHANNEL, Harness, alert, payload, slack_error, slack_that_works, slack_with_auth_only, t0,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A leased op with the attempt count a test wants.
fn leased(op: Op, attempts: i32) -> LeasedOp {
    LeasedOp {
        id: OpId::new(1),
        op,
        attempts,
        leased_until: t0() + TimeDelta::seconds(60),
        created_at: t0(),
    }
}

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

fn group_key() -> GroupKey {
    GroupKey::new("{}:{alertname=\"CephOSDDown\"}")
}

/// Runs one op against the relay's real store, client and renderer.
async fn deliver(relay: &Harness, op: Op, attempts: i32) -> alertthread::delivery::Outcome {
    let renderer = Renderer::builtin();
    let limits = alertthread::ratelimit::SlackLimits::default();
    let delivery = alertthread::delivery::Delivery {
        store: relay.store.as_ref(),
        slack: relay.slack.as_ref(),
        renderer: &renderer,
        limits: &limits,
        metrics: relay.metrics.as_ref(),
        backoff: relay.backoff(),
    };
    delivery
        .run(&leased(op, attempts), t0())
        .await
        .expect("the store is healthy")
}

/// Puts a delivery through the store, so there is correlation state to render from.
async fn ingest(relay: &Harness, body: &str, at: chrono::DateTime<chrono::Utc>) {
    let parsed: alertthread_core::WebhookPayload = serde_json::from_str(body).unwrap();
    let batch = AlertBatch::from_webhook(parsed, channel());
    let policy = Policy::default();
    relay
        .store
        .ingest(&batch, at, |o, g| plan(o, &batch, g, &policy, at))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_post_whose_alert_row_is_gone_is_parked_loudly() {
    // Nothing in the relay produces this: the pruner refuses to delete a row that still has
    // queued work. But the row is what a message renders *from*, so if it ever does go
    // there is nothing to send — and silently completing the op would be an alert nobody
    // was told about and nothing to show for it.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-postnorow", &slack).await;

    let outcome = deliver(
        &relay,
        Op::Post {
            fingerprint: Fingerprint::new("vanished"),
            channel: channel(),
            placement: Placement::Channel,
        },
        1,
    )
    .await;

    let alertthread::delivery::Outcome::DeadLetter { reason, detail } = outcome else {
        panic!("a post with nothing to render must not be quietly completed: {outcome:?}");
    };
    assert_eq!(reason, "alert_row_missing");
    assert!(detail.contains("vanished"), "{detail}");
}

#[tokio::test]
async fn a_refresh_whose_alert_row_is_gone_is_completed_rather_than_parked() {
    // The opposite call from a post, and for a reason: a refresh only ever *edits* a
    // message that is already in the channel. There is no work left and nothing was lost,
    // so raising a dead-letter alarm would page somebody about a message sitting there
    // being read.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-refreshnorow", &slack).await;

    let outcome = deliver(
        &relay,
        Op::Refresh {
            fingerprint: Fingerprint::new("vanished"),
            channel: channel(),
            message_ts: MessageTs::new("1784642520.000001"),
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Refreshed)
    );
    assert!(
        slack.received_requests().await.unwrap().is_empty(),
        "and no Slack call is made for a message we cannot render"
    );
}

#[tokio::test]
async fn a_resolve_whose_alert_row_is_gone_is_parked_loudly() {
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolvenorow", &slack).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("vanished"),
            channel: channel(),
            target: ResolveTarget::Message {
                message_ts: MessageTs::new("1784642520.000001"),
                thread_parent_ts: None,
            },
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    assert!(
        matches!(
            outcome,
            alertthread::delivery::Outcome::DeadLetter {
                reason: "alert_row_missing",
                ..
            }
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_resolve_with_no_message_and_no_row_is_parked_rather_than_waiting_for_ever() {
    // There is nothing to wait for and nothing to render. Deferring would be an op that
    // comes back ten times and then dead-letters anyway, ten backoffs later.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolvenothing", &slack).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("vanished"),
            channel: channel(),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    assert!(
        matches!(
            outcome,
            alertthread::delivery::Outcome::DeadLetter {
                reason: "alert_row_missing",
                ..
            }
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_resolve_whose_post_has_not_landed_waits_before_it_gives_up() {
    // ADR 001 D9, first half: "self-defer with backoff".
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolvewaits", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    let alertthread::delivery::Outcome::Retry { error, .. } = outcome else {
        panic!("a resolve whose post has not landed should wait: {outcome:?}");
    };
    assert!(error.contains("post to land"), "{error}");
    assert!(
        slack.received_requests().await.unwrap().is_empty(),
        "nothing is sent while it waits"
    );
}

#[tokio::test]
async fn a_resolve_that_has_waited_long_enough_posts_a_standalone_message() {
    // ADR 001 D9, second half: "on timeout, post standalone". The alternative is a
    // resolution nobody hears about, for an alert nobody heard about.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolvestandalone", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        },
        relay.config.worker.max_attempts,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Resolved)
    );
    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("a standalone message went out");
    assert!(
        posted["attachments"][0]["color"] == "#2eb886",
        "and it is green: {posted}"
    );
    assert!(
        posted.get("thread_ts").is_none(),
        "standalone means top level: {posted}"
    );
}

#[tokio::test]
async fn a_resolve_for_an_alert_whose_post_was_parked_does_not_wait_at_all() {
    // A post that has already dead-lettered will never produce a timestamp. Waiting the
    // full ten attempts for it would delay the only message this alert is ever going to
    // get, by half an hour.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolveafterfailed", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let queued = relay
        .store
        .lease_batch(
            &alertthread_store::WorkerId::new("t"),
            10,
            TimeDelta::seconds(60),
            t0(),
        )
        .await
        .unwrap();
    relay
        .store
        .dead_letter(queued[0].id, "invalid_auth", t0())
        .await
        .unwrap();
    assert_eq!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &channel())
            .await
            .unwrap()
            .unwrap()
            .state,
        alertthread_store::AlertState::Failed
    );

    // Attempt 1, nowhere near exhaustion — and it still posts immediately.
    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Resolved)
    );
}

#[tokio::test]
async fn a_resolve_with_both_behaviours_off_still_completes() {
    // Configuration validation rejects this pairing at startup (ADR 001 D6), so it can only
    // reach the worker on an op planned before a config change. Completing it is right:
    // the alert *has* resolved, and leaving the row in `resolving` for ever would make its
    // next firing look like a duplicate.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-resolvenoop", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::Message {
                message_ts: MessageTs::new("1784642520.000001"),
                thread_parent_ts: None,
            },
            update_in_place: false,
            thread_reply: false,
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Resolved)
    );
}

#[tokio::test]
async fn a_resolve_whose_edit_fails_does_not_go_on_to_post_the_reply() {
    // The edit and the reply are two halves of one resolution. Posting "resolved after 29m"
    // into a thread under a message that is still red would be worse than either failing:
    // the channel would contradict itself.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(slack_error("internal_error"))
        .mount(&slack)
        .await;
    let relay = Harness::new("degradation-editfails", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::Message {
                message_ts: MessageTs::new("1784642520.000001"),
                thread_parent_ts: None,
            },
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    assert!(
        matches!(outcome, alertthread::delivery::Outcome::Retry { .. }),
        "{outcome:?}"
    );
    assert!(
        !slack
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/api/chat.postMessage"),
        "the reply must not go out while the message is still red"
    );
}

#[tokio::test]
async fn a_collapsed_child_waits_for_its_parent_and_then_gives_up_and_posts_anyway() {
    // ADR 001 D2's self-deferral, and the limit on it. A threaded message with no thread is
    // not a possible outcome; an unthreaded message is merely untidy, and untidy beats
    // absent every time.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-orphanedchild", &slack).await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("f{i}"), "firing")).collect();
    ingest(&relay, &payload("firing", &alerts), t0()).await;
    // The group row exists with no `message_ts`: the summary's own post has not completed.

    let child = Op::Post {
        fingerprint: Fingerprint::new("f0"),
        channel: channel(),
        placement: Placement::Thread {
            group_key: group_key(),
            parent_ts: None,
        },
    };

    let waiting = deliver(&relay, child.clone(), 1).await;
    assert!(
        matches!(waiting, alertthread::delivery::Outcome::Retry { .. }),
        "it should wait for the summary first: {waiting:?}"
    );
    assert!(slack.received_requests().await.unwrap().is_empty());

    let exhausted = deliver(&relay, child, relay.config.worker.max_attempts).await;
    let alertthread::delivery::Outcome::Done(OpEffect::Posted {
        thread_parent_ts, ..
    }) = exhausted
    else {
        panic!("an alert that waited long enough must still post: {exhausted:?}");
    };
    assert_eq!(
        thread_parent_ts, None,
        "at top level, because there is no thread to put it in"
    );

    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert!(posted.get("thread_ts").is_none(), "{posted}");
}

#[tokio::test]
async fn a_summary_refresh_renders_the_live_count_from_the_store() {
    // `Op::RefreshGroup` deliberately carries no counts: membership is a property of the
    // store, not of the batch that planned the refresh. Inventing a number here would put a
    // wrong count on the most-read message of a storm.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-refreshgroup", &slack).await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("f{i}"), "firing")).collect();
    ingest(&relay, &payload("firing", &alerts), t0()).await;
    relay.drain_from(t0(), 20).await;

    let outcome = deliver(
        &relay,
        Op::RefreshGroup {
            group_key: group_key(),
            channel: channel(),
            message_ts: ThreadTs::new("1784642520.000001"),
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Refreshed)
    );
    let edit: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .rev()
        .filter(|r| r.url.path() == "/api/chat.update")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .next()
        .expect("the summary was edited");
    assert!(
        edit.to_string().contains("6 of 6"),
        "the edit carries the store's count: {edit}"
    );
}

#[tokio::test]
async fn a_summary_for_a_group_row_that_is_gone_still_renders_something() {
    // The pruner deletes resolved alerts before it deletes their parent, so a summary can
    // outlive every member it counts. A blank heading over a thread of replies is the
    // outcome the renderer's fallback chain exists to prevent.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-groupgone", &slack).await;

    let outcome = deliver(
        &relay,
        Op::PostGroup {
            group_key: GroupKey::new("never-existed"),
            channel: channel(),
            initial_members: 3,
        },
        1,
    )
    .await;

    assert!(
        matches!(
            outcome,
            alertthread::delivery::Outcome::Done(OpEffect::GroupPosted { .. })
        ),
        "{outcome:?}"
    );
    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    let text = posted.to_string();
    assert!(text.contains("never-existed"), "titled by its key: {text}");
    assert!(
        text.contains("3 of 3"),
        "and never claims fewer members than the batch that opened it: {text}"
    );
}

#[tokio::test]
async fn a_post_for_an_alert_that_has_already_resolved_goes_out_green() {
    // The post and its resolve can be planned in one batch, and the post is drained first.
    // Rendering it red would put a firing message in the channel for an alert that has
    // already cleared — and the resolve behind it would turn it green a moment later, which
    // is two notifications for one event.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-postresolved", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    ingest(
        &relay,
        &payload("resolved", &[alert("abc", "resolved")]),
        t0() + TimeDelta::seconds(1),
    )
    .await;

    let outcome = deliver(
        &relay,
        Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            placement: Placement::Channel,
        },
        1,
    )
    .await;

    assert!(
        matches!(
            outcome,
            alertthread::delivery::Outcome::Done(OpEffect::Posted { .. })
        ),
        "{outcome:?}"
    );
    let posted: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert_eq!(
        posted["attachments"][0]["color"], "#2eb886",
        "green, not red: {posted}"
    );
}

#[tokio::test]
async fn a_refresh_for_an_alert_that_has_resolved_renders_it_resolved() {
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-refreshresolved", &slack).await;

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;
    ingest(
        &relay,
        &payload("resolved", &[alert("abc", "resolved")]),
        t0() + TimeDelta::minutes(29),
    )
    .await;

    let outcome = deliver(
        &relay,
        Op::Refresh {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            message_ts: MessageTs::new("1784642520.000001"),
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::Refreshed)
    );
    let edit: serde_json::Value = slack
        .received_requests()
        .await
        .unwrap()
        .iter()
        .rev()
        .filter(|r| r.url.path() == "/api/chat.update")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .next()
        .unwrap();
    assert_eq!(edit["attachments"][0]["color"], "#2eb886", "{edit}");
}

#[tokio::test]
async fn a_message_slack_reports_as_gone_during_a_resolve_is_healed_not_parked() {
    // ADR 001 D7's liveness probe, on the resolve path rather than the refresh path. The
    // store clears the stale timestamp and enqueues a fresh post in the same transaction,
    // and that post renders green because the row's `resolved_at` is already set.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(slack_error("message_not_found"))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "channel": "C1", "ts": "1784642520.000009"
        })))
        .mount(&slack)
        .await;
    let relay = Harness::new("degradation-resolvegone", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;

    let outcome = deliver(
        &relay,
        Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            target: ResolveTarget::Message {
                message_ts: MessageTs::new("1784642520.000001"),
                thread_parent_ts: None,
            },
            update_in_place: true,
            thread_reply: true,
        },
        1,
    )
    .await;

    assert_eq!(
        outcome,
        alertthread::delivery::Outcome::Done(OpEffect::MessageLost),
        "a lost message is replaced, not retried and not parked"
    );
}

#[tokio::test]
async fn a_chat_update_rate_limit_is_paced_by_the_workspace_bucket() {
    // Tier 3 is 50/min per *workspace*, not per channel. Keying it per channel would permit
    // 50/min per channel and silently multiply the real budget by however many channels an
    // operator routes to.
    let slack = slack_that_works().await;
    let relay = Harness::new("degradation-updatepacing", &slack).await;
    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), t0()).await;
    relay.drain_from(t0(), 5).await;

    let renderer = Renderer::builtin();
    let limits = alertthread::ratelimit::SlackLimits::default();
    let delivery = alertthread::delivery::Delivery {
        store: relay.store.as_ref(),
        slack: relay.slack.as_ref(),
        renderer: &renderer,
        limits: &limits,
        metrics: relay.metrics.as_ref(),
        backoff: relay.backoff(),
    };

    let op = leased(
        Op::Refresh {
            fingerprint: Fingerprint::new("abc"),
            channel: channel(),
            message_ts: MessageTs::new("1784642520.000001"),
        },
        1,
    );

    // Fifty edits in one instant is the whole minute's budget; the fifty-first waits.
    for _ in 0..50 {
        assert_eq!(
            delivery.run(&op, t0()).await.unwrap(),
            alertthread::delivery::Outcome::Done(OpEffect::Refreshed)
        );
    }
    assert!(
        matches!(
            delivery.run(&op, t0()).await.unwrap(),
            alertthread::delivery::Outcome::Wait { .. }
        ),
        "the fifty-first edit in one second has to wait"
    );
    relay
        .assert_metric("alertthread_rate_limited_total{method=\"chat.update\",source=\"local\"} 1");
}
