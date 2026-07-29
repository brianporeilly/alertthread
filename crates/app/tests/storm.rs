//! ADR 001 D5's storm collapse, driven through the whole stack under concurrent load.
//!
//! The collapse *decision* is a pure function and is exhaustively unit-tested in
//! `alertthread-core`. What those tests cannot say is whether the decision survives contact
//! with the parts around it: a real axum handler, a real transaction, a real lease, a real
//! rate limiter and a real Slack on the wire — with several deliveries of the same storm
//! racing each other, which is exactly what Alertmanager does when it times out and retries.
//!
//! Two failure modes are being hunted here, and both are invisible to a sequential test:
//!
//! - **More than one summary.** Two deliveries of a storm that each open their own group
//!   would put two parents in the channel, split the thread, and leave both counts wrong.
//! - **A child that lost its parent.** A `Placement::Thread` whose parent timestamp never
//!   landed must still post — at top level if it has to (ADR 001 D9) — because a threaded
//!   message with no thread is not an outcome this project has.
//!
//! Everything asserted below is a number ADR 001 D5 states: *more than* `collapse_threshold`
//! new posts in one batch collapses, the group is sticky, and each child keeps its own
//! message so per-alert resolve still edits the right one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use std::collections::BTreeSet;

use alertthread_core::{ChannelId, Fingerprint, GroupKey};
use alertthread_store::StateStore;
use harness::{CHANNEL, Harness, alert, payload, slack_that_works};
use wiremock::MockServer;

/// Alertmanager's `groupKey` for the fixtures in `harness`.
const GROUP: &str = "{}:{alertname=\"CephOSDDown\"}";

/// One `chat.postMessage` as Slack received it.
struct Posted {
    thread_ts: Option<String>,
}

async fn posts(slack: &MockServer) -> Vec<Posted> {
    slack
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/chat.postMessage")
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("the relay sends JSON");
            Posted {
                thread_ts: body
                    .get("thread_ts")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
            }
        })
        .collect()
}

/// Delivers `bodies` to a running relay all at once, and asserts every one was accepted.
///
/// Concurrent rather than sequential on purpose: ADR 001 D3's guarantee is about racing
/// deliveries, and a loop that waits for each `200` before sending the next never produces
/// the race it is supposed to be testing.
async fn storm(base: &str, bodies: Vec<String>) {
    let client = reqwest::Client::new();
    let mut sending = tokio::task::JoinSet::new();
    for body in bodies {
        let client = client.clone();
        let url = format!("{base}/webhook");
        sending.spawn(async move { client.post(url).body(body).send().await });
    }
    while let Some(finished) = sending.join_next().await {
        let response = finished
            .expect("the delivery task finishes")
            .expect("the relay answers");
        assert_eq!(
            response.status(),
            200,
            "every delivery has to be durably accepted, or Alertmanager gives up on it"
        );
    }
}

fn alerts(range: std::ops::Range<usize>) -> Vec<serde_json::Value> {
    range
        .map(|i| alert(&format!("f{i:02}"), "firing"))
        .collect()
}

#[tokio::test]
async fn a_storm_delivered_many_times_at_once_collapses_exactly_once() {
    // Twelve alerts, one groupKey, eight racing deliveries of the identical batch — the
    // shape Alertmanager produces when the relay is slow enough to hit its timeout and it
    // retries. D5 says one summary with twelve threaded children, and D3 says the racing
    // does not change that.
    let slack = slack_that_works().await;
    let relay = Harness::new("storm-concurrent", &slack).await;
    let server = relay.serve().await;

    let body = payload("firing", &alerts(0..12));
    storm(&server.base, vec![body; 8]).await;
    server.stop().await;

    // 1 summary + 12 children, each paced a second apart by the per-channel rate limit.
    let pass = relay.drain_from(chrono::Utc::now(), 60).await;
    assert_eq!(
        pass.completed, 13,
        "one summary plus twelve children: {pass:?}"
    );
    assert_eq!(pass.dead_lettered, 0, "nothing was written off: {pass:?}");

    relay.assert_metric("alertthread_storm_collapses_total 1");

    let group = relay
        .store
        .group(&GroupKey::new(GROUP), &ChannelId::new(CHANNEL))
        .await
        .expect("reading the group")
        .expect("exactly one group was opened");
    let parent = group
        .message_ts
        .clone()
        .expect("the summary posted before its children");
    assert_eq!(
        group.member_count, 12,
        "eight deliveries of twelve alerts is twelve members, not ninety-six"
    );

    let sent = posts(&slack).await;
    assert_eq!(
        sent.len(),
        13,
        "one message per fingerprint plus one summary — a duplicate here is eight copies \
         of a storm in somebody's channel"
    );
    let parents: Vec<_> = sent
        .iter()
        .filter(|post| post.thread_ts.is_none())
        .collect();
    assert_eq!(
        parents.len(),
        1,
        "a second top-level message would be a second summary, and the thread would split"
    );
    for post in sent.iter().filter(|post| post.thread_ts.is_some()) {
        assert_eq!(
            post.thread_ts.as_deref(),
            Some(parent.as_str()),
            "every child threads under the one summary"
        );
    }

    // D5's correctness claim: collapse changes visual placement only. Each child still has
    // its own message, so its resolution still edits the right one.
    let mut seen = BTreeSet::new();
    for i in 0..12 {
        let fingerprint = format!("f{i:02}");
        let child = relay
            .store
            .alert(&Fingerprint::new(&fingerprint), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .unwrap_or_else(|| panic!("{fingerprint} has no correlation state"));
        assert_eq!(
            child.thread_parent_ts.as_ref(),
            Some(&parent),
            "{fingerprint} must hang under the summary"
        );
        let message_ts = child
            .message_ts
            .unwrap_or_else(|| panic!("{fingerprint} never got its own message"));
        assert!(
            seen.insert(message_ts.as_str().to_owned()),
            "{fingerprint} shares a message with another alert"
        );
    }
}

#[tokio::test]
async fn concurrent_batches_that_are_each_below_the_threshold_do_not_collapse() {
    // The other half of D5, and the reason the first test is not enough on its own: the
    // trigger is "more than `collapse_threshold` new posts **in one batch**", not "more than
    // that many alerts in flight". Twelve alerts arriving as four racing batches of three
    // stay at top level, because collapsing them would be the relay inventing a summary
    // nothing asked for.
    let slack = slack_that_works().await;
    let relay = Harness::new("storm-small-batches", &slack).await;
    let server = relay.serve().await;

    let bodies: Vec<_> = (0..4)
        .map(|batch| payload("firing", &alerts(batch * 3..batch * 3 + 3)))
        .collect();
    storm(&server.base, bodies).await;
    server.stop().await;

    let pass = relay.drain_from(chrono::Utc::now(), 40).await;
    assert_eq!(pass.completed, 12, "twelve top-level messages: {pass:?}");
    assert_eq!(pass.dead_lettered, 0);

    assert!(
        !relay
            .metrics_text()
            .contains("alertthread_storm_collapses_total 1"),
        "no batch crossed the threshold, so no group should have been opened"
    );
    assert!(
        relay
            .store
            .group(&GroupKey::new(GROUP), &ChannelId::new(CHANNEL))
            .await
            .expect("reading the group")
            .is_none(),
        "no group row either"
    );

    let sent = posts(&slack).await;
    assert_eq!(sent.len(), 12);
    assert!(
        sent.iter().all(|post| post.thread_ts.is_none()),
        "nothing threads when nothing collapsed"
    );
}

#[tokio::test]
async fn late_alerts_racing_into_an_open_group_all_thread_under_it() {
    // D5's stickiness, under load. Once a group has a parent, later alerts join it even in
    // batches far below the threshold — otherwise a group's alerts would be split between
    // top-level messages and thread replies depending on batch timing, which is worse than
    // either consistent behaviour.
    let slack = slack_that_works().await;
    let relay = Harness::new("storm-sticky", &slack).await;
    let server = relay.serve().await;

    // Open the group, and land the summary, so the stickiness under test is against a
    // parent that really exists rather than one still queued.
    storm(&server.base, vec![payload("firing", &alerts(0..6))]).await;
    let opening = relay.drain_from(chrono::Utc::now(), 30).await;
    assert_eq!(opening.completed, 7, "{opening:?}");

    let parent = relay
        .store
        .group(&GroupKey::new(GROUP), &ChannelId::new(CHANNEL))
        .await
        .expect("reading the group")
        .expect("a group was opened")
        .message_ts
        .expect("the summary posted");

    // Six more alerts, as three racing batches of two. Each is far below the threshold.
    let late: Vec<_> = (0..3)
        .map(|batch| payload("firing", &alerts(6 + batch * 2..8 + batch * 2)))
        .collect();
    storm(&server.base, late).await;
    server.stop().await;

    let joining = relay.drain_from(chrono::Utc::now(), 40).await;
    assert_eq!(joining.dead_lettered, 0, "{joining:?}");

    for i in 6..12 {
        let fingerprint = format!("f{i:02}");
        let child = relay
            .store
            .alert(&Fingerprint::new(&fingerprint), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .unwrap_or_else(|| panic!("{fingerprint} has no correlation state"));
        assert_eq!(
            child.thread_parent_ts.as_ref(),
            Some(&parent),
            "{fingerprint} arrived in a batch of two and still has to join the open group"
        );
    }

    let sent = posts(&slack).await;
    let top_level = sent.iter().filter(|post| post.thread_ts.is_none()).count();
    assert_eq!(
        top_level, 1,
        "the summary is the only thing at top level; a late alert that posted there would \
         be the split D5 exists to prevent"
    );
    assert_eq!(sent.len(), 13, "one summary and twelve children, once each");
}
