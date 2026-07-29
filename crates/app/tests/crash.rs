//! `kill -9` at every stage of delivery, and what comes back afterwards.
//!
//! Phase 5's exit criterion is one sentence: *killing the process during any phase of
//! delivery never produces silence.* This file is that sentence, executable.
//!
//! # Two kinds of crash, for two different reasons
//!
//! **A real `SIGKILL` on a real process**, for the stages where the interesting state is on
//! disk. `SIGKILL` cannot be caught, so nothing runs on the way down: no drain, no lease
//! release, no flush. What survives is exactly what SQLite had committed, which is precisely
//! the thing under test. These tests spawn `CARGO_BIN_EXE_alertthread`, kill it, and start a
//! second process against the same database.
//!
//! **An in-process stand-in**, for the two windows a signal cannot hit reliably. "Slack
//! accepted the post and the commit had not happened yet" is microseconds wide; racing a
//! `kill` against it would be a test that passes for the wrong reason most of the time. So
//! those run the shipping [`Delivery`] and simply stop where the crash would have, which
//! reproduces the surviving state exactly and does it every time. Each says which it is.
//!
//! # The one window that ends in a duplicate
//!
//! ADR 001 D3's last row: a worker that posts to Slack and dies before recording the
//! timestamp cannot be made atomic against an API with no idempotency key. The recovery is a
//! **second message**, and
//! `a_post_that_reached_slack_before_the_crash_comes_back_as_a_duplicate_not_a_silence`
//! asserts the duplicate rather than pretending it cannot happen. Noise is the accepted
//! cost; silence is the failure.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use alertthread::delivery::{Delivery, Outcome};
use alertthread_core::{AlertBatch, ChannelId, Fingerprint, Policy, WebhookPayload, plan};
use alertthread_slack::Renderer;
use alertthread_store::{AlertState, Backend, OpEffect, StateStore, Store, StoreStats, WorkerId};
use chrono::{TimeDelta, Utc};
use harness::{
    CHANNEL, Harness, alert, payload, slack_that_works, slack_with_auth_only, sqlite_url,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Real processes, killed with SIGKILL
// ---------------------------------------------------------------------------

/// A relay running as its own process, so that killing it means what it says.
struct Process {
    child: Child,
    base: String,
}

impl Process {
    /// Starts the shipping binary against `db` and `slack`, and waits until it serves.
    async fn start(db: &str, slack: &MockServer) -> Self {
        let addr = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_alertthread"))
            .env("ALERTTHREAD_SLACK__TOKEN", "xoxb-test")
            .env("ALERTTHREAD_SLACK__DEFAULT_CHANNEL", CHANNEL)
            .env(
                "ALERTTHREAD_SLACK__BASE_URL",
                format!("{}/api/", slack.uri()),
            )
            // Nothing here is allowed to wait on a wall clock the test does not control.
            .env("ALERTTHREAD_SLACK__AUTH_STARTUP_GRACE", "0s")
            .env("ALERTTHREAD_SLACK__AUTH_PROBE_INTERVAL", "1h")
            .env("ALERTTHREAD_STORAGE__URL", db)
            .env("ALERTTHREAD_STORAGE__RETENTION__INTERVAL", "1h")
            .env("ALERTTHREAD_SERVER__LISTEN", addr.clone())
            .env("ALERTTHREAD_WORKER__IDLE_POLL", "20ms")
            // A short lease is the whole point of these tests: it is how long an alert
            // spends undelivered after the process holding it dies.
            .env("ALERTTHREAD_WORKER__LEASE", "1s")
            .env("ALERTTHREAD_WORKER__BACKOFF_BASE", "100ms")
            .env("ALERTTHREAD_WORKER__BACKOFF_MAX", "200ms")
            .env("ALERTTHREAD_WORKER__SAMPLE_INTERVAL", "100ms")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the relay binary starts");

        let relay = Self {
            child,
            base: format!("http://{addr}"),
        };
        relay.await_ready().await;
        relay
    }

    async fn await_ready(&self) {
        let base = self.base.clone();
        let up = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if reqwest::get(format!("{base}/healthz"))
                    .await
                    .is_ok_and(|response| response.status() == 200)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(up.is_ok(), "the relay never came up at {}", self.base);
    }

    /// Delivers one webhook body and asserts the relay acknowledged it.
    ///
    /// The `200` is the durability boundary: ADR 001 D2 acks only after the ingest
    /// transaction commits, so everything below can crash freely afterwards.
    async fn deliver(&self, body: String) {
        let response = reqwest::Client::new()
            .post(format!("{}/webhook", self.base))
            .body(body)
            .send()
            .await
            .expect("the relay answers");
        assert_eq!(response.status(), 200, "the alert was not durably accepted");
    }

    /// `SIGKILL`. Uncatchable, so nothing at all runs on the way down.
    fn kill_9(mut self) {
        self.child.kill().expect("killing the relay");
        self.child.wait().expect("reaping the relay");
    }

    /// `SIGTERM`, then `SIGKILL` before the drain can finish.
    fn terminate_then_kill_9(mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("sending SIGTERM");
        assert!(status.success(), "SIGTERM was not delivered");
        std::thread::sleep(Duration::from_millis(30));
        self.child.kill().expect("killing the relay mid-drain");
        self.child.wait().expect("reaping the relay");
    }
}

/// An address nothing is listening on, for a child process to bind.
fn free_port() -> String {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    socket
        .local_addr()
        .expect("the socket has an address")
        .to_string()
}

/// Waits until `slack` has seen at least `n` calls to `method_path`.
async fn wait_for_calls(slack: &MockServer, method_path: &str, n: usize) {
    let seen = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if count_calls(slack, method_path).await >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        seen.is_ok(),
        "expected {n} calls to {method_path}, saw {}",
        count_calls(slack, method_path).await
    );
}

async fn count_calls(slack: &MockServer, method_path: &str) -> usize {
    slack
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == method_path)
        .count()
}

/// Every `chat.postMessage` body Slack was sent, as JSON.
async fn posted_bodies(slack: &MockServer) -> Vec<serde_json::Value> {
    slack
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/chat.postMessage")
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

/// Opens the database a killed process left behind.
async fn reopen(db: &str) -> Store {
    Store::connect(Backend::Sqlite, db)
        .await
        .expect("reopening the store a killed relay left behind")
}

/// Polls the store until it holds no queued work, or fails the test.
async fn await_drained(store: &Store) -> StoreStats {
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let stats = store.stats().await.expect("sampling the store");
            if stats.outbox_depth.values().all(|depth| *depth == 0) {
                return stats;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    drained.expect("the outbox never drained")
}

#[tokio::test]
async fn killing_the_relay_mid_post_redelivers_the_alert_when_it_comes_back() {
    // Stage: **after the claim, while the lease is held, before the post completes.** A
    // real `SIGKILL` while the worker is blocked inside `chat.postMessage`. Nothing runs on
    // the way down, so the lease is not released — it has to *expire* and be reclaimed,
    // which is ADR 001 D2's whole reason for having one.
    let db = sqlite_url("crash-midpost");

    // A Slack that accepts the connection and never answers. That is what holds the lease.
    let stalled = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_mins(1)))
        .mount(&stalled)
        .await;

    let first = Process::start(&db, &stalled).await;
    first
        .deliver(payload("firing", &[alert("abc", "firing")]))
        .await;
    // The request reaching the stalled Slack proves the row was claimed and leased.
    wait_for_calls(&stalled, "/api/chat.postMessage", 1).await;
    first.kill_9();

    // A second process, against the same database and a Slack that works.
    let working = slack_that_works().await;
    let second = Process::start(&db, &working).await;
    wait_for_calls(&working, "/api/chat.postMessage", 1).await;

    let store = reopen(&db).await;
    let stats = await_drained(&store).await;
    assert_eq!(stats.dead_lettered, 0, "nothing was written off");

    second.kill_9();

    let record = store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
        .await
        .expect("reading the alert")
        .expect("the row survived the kill");
    assert_eq!(
        record.state,
        AlertState::Posted,
        "the alert has to end up posted, not stuck in the state the kill left it in"
    );
    assert!(
        record.message_ts.is_some(),
        "and with the timestamp its resolution will need"
    );
}

#[tokio::test]
async fn killing_the_relay_during_the_shutdown_drain_loses_no_alert() {
    // Stage: **during the drain.** `SIGTERM` puts the relay into
    // `Relay::shutdown`, which stops leasing and finishes the batch in hand; `SIGKILL` 30 ms
    // later takes it out mid-drain. The per-channel rate limit is one message per second, so
    // four alerts guarantee there is still work queued at that moment.
    let db = sqlite_url("crash-drain");
    let slack = slack_that_works().await;

    let fingerprints = ["a1", "a2", "a3", "a4"];
    let alerts: Vec<_> = fingerprints.iter().map(|f| alert(f, "firing")).collect();

    let first = Process::start(&db, &slack).await;
    first.deliver(payload("firing", &alerts)).await;
    wait_for_calls(&slack, "/api/chat.postMessage", 1).await;
    first.terminate_then_kill_9();

    let second = Process::start(&db, &slack).await;
    let store = reopen(&db).await;
    await_drained(&store).await;
    second.kill_9();

    // The property is per-alert, not per-count: a duplicate is ADR 001 D3's accepted cost
    // and must not fail this test, but an alert nobody was told about must.
    let bodies = posted_bodies(&slack).await;
    let rendered = serde_json::to_string(&bodies).expect("bodies serialise");
    for fingerprint in fingerprints {
        assert!(
            rendered.contains(&format!("osd {fingerprint} is down")),
            "{fingerprint} was never posted — that is silence:\n{rendered}"
        );
        let record = store
            .alert(&Fingerprint::new(fingerprint), &ChannelId::new(CHANNEL))
            .await
            .expect("reading the alert")
            .expect("the row survived the kill");
        assert_eq!(record.state, AlertState::Posted, "{fingerprint}");
    }
    assert_eq!(
        store.stats().await.expect("sampling").dead_lettered,
        0,
        "a shutdown kill must not write anything off"
    );
}

// ---------------------------------------------------------------------------
// The windows a signal cannot hit reliably
// ---------------------------------------------------------------------------

/// Ingests one webhook body through the real store and the real planner.
async fn ingest(relay: &Harness, body: &str, now: chrono::DateTime<Utc>) {
    let parsed: WebhookPayload = serde_json::from_str(body).expect("the fixture parses");
    let batch = AlertBatch::from_webhook(parsed, ChannelId::new(CHANNEL));
    let policy = Policy::default();
    relay
        .store
        .ingest(&batch, now, |outcomes, group| {
            plan(outcomes, &batch, group, &policy, now)
        })
        .await
        .expect("ingesting");
}

#[tokio::test]
async fn an_alert_claimed_but_never_attempted_is_delivered_by_the_next_process() {
    // Stage: **after the claim, before any delivery attempt.** The crash is represented by
    // simply not running a worker: `SIGKILL` here would leave exactly this on disk, because
    // the `200` the handler returned means the ingest transaction had already committed.
    let slack = slack_that_works().await;
    let relay = Harness::new("crash-claimed", &slack).await;
    let now = Utc::now();

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), now).await;

    // Whatever the process that accepted it was doing, the row is work for whoever asks
    // next. That worker is a different one, with its own id, exactly as a restart would be.
    let successor = alertthread::worker::Worker::new(
        Arc::clone(&relay.store),
        Arc::clone(&relay.slack),
        Arc::new(Renderer::builtin()),
        Arc::new(alertthread::ratelimit::SlackLimits::default()),
        Arc::clone(&relay.metrics),
        relay.config.worker,
        WorkerId::new("successor"),
    );
    let pass = successor.run_once(now).await.expect("leasing");
    assert_eq!(pass.completed, 1, "{pass:?}");

    let record = relay
        .store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
        .await
        .expect("reading")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Posted);
}

#[tokio::test]
async fn a_lease_a_dead_worker_never_released_is_reclaimed_by_the_next_one() {
    // Stage: **mid-lease.** The first worker leases and dies holding it. Nothing releases
    // the lease; it expires, and that expiry is the only thing standing between the alert
    // and permanent silence.
    let slack = slack_that_works().await;
    let relay = Harness::new("crash-lease", &slack).await;
    let now = Utc::now();

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), now).await;

    let held = relay
        .store
        .lease_batch(&WorkerId::new("doomed"), 10, TimeDelta::seconds(60), now)
        .await
        .expect("leasing");
    assert_eq!(held.len(), 1);

    // A successor arriving *inside* the lease finds nothing: the row belongs to a worker it
    // has no way of knowing is dead. This half is what stops two replicas double-posting.
    assert!(
        relay
            .store
            .lease_batch(
                &WorkerId::new("successor"),
                10,
                TimeDelta::seconds(60),
                now + TimeDelta::seconds(30),
            )
            .await
            .expect("leasing")
            .is_empty(),
        "a live lease is not reclaimable"
    );

    // Past the expiry it is work again, and it is delivered.
    let after = now + TimeDelta::seconds(61);
    let pass = relay.worker.run_once(after).await.expect("leasing");
    assert_eq!(pass.completed, 1, "{pass:?}");
    assert_eq!(
        count_calls(&slack, "/api/chat.postMessage").await,
        1,
        "reclaiming must deliver the alert once, not never and not twice"
    );
}

#[tokio::test]
async fn a_post_that_reached_slack_before_the_crash_comes_back_as_a_duplicate_not_a_silence() {
    // Stage: **after the post, before the timestamp commit.** ADR 001 D3's one genuinely
    // unresolvable window — the Slack call and the local commit cannot be made atomic
    // against an API with no idempotency key.
    //
    // Reproduced rather than raced: the shipping `Delivery` runs to completion and its
    // `Outcome` is dropped instead of being applied, which is byte-for-byte the state a
    // `SIGKILL` in that window leaves behind. Racing a signal against a window microseconds
    // wide would pass for the wrong reason most of the time.
    let slack = slack_that_works().await;
    let relay = Harness::new("crash-postcommit", &slack).await;
    let now = Utc::now();

    ingest(&relay, &payload("firing", &[alert("abc", "firing")]), now).await;

    let leased = relay
        .store
        .lease_batch(&WorkerId::new("doomed"), 10, TimeDelta::seconds(60), now)
        .await
        .expect("leasing");
    assert_eq!(leased.len(), 1);

    let delivery = Delivery {
        store: relay.store.as_ref(),
        slack: relay.slack.as_ref(),
        renderer: &Renderer::builtin(),
        limits: &alertthread::ratelimit::SlackLimits::default(),
        metrics: relay.metrics.as_ref(),
        backoff: relay.backoff(),
    };
    let outcome = delivery.run(&leased[0], now).await.expect("delivering");
    assert!(
        matches!(outcome, Outcome::Done(OpEffect::Posted { .. })),
        "Slack accepted the message: {outcome:?}"
    );
    // …and here the process dies. `outcome` is never applied, so nothing is committed.
    drop(outcome);

    assert_eq!(
        count_calls(&slack, "/api/chat.postMessage").await,
        1,
        "one message is in the channel, and the store does not know it"
    );
    assert_eq!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .expect("row exists")
            .message_ts,
        None,
        "the timestamp was never committed — that is the window"
    );

    // The successor reclaims the expired lease and posts again. **A duplicate, not
    // silence.** PRD goal #3 and ADR 001 D3 choose this direction explicitly, and asserting
    // it is the honest thing to do: pretending the window does not exist would be the only
    // way to make this test look tidier.
    let after = now + TimeDelta::seconds(61);
    let pass = relay.worker.run_once(after).await.expect("leasing");
    assert_eq!(pass.completed, 1, "{pass:?}");
    assert_eq!(
        count_calls(&slack, "/api/chat.postMessage").await,
        2,
        "the accepted cost of the D3 window is exactly one extra message"
    );

    let record = relay
        .store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
        .await
        .expect("reading")
        .expect("row exists");
    assert_eq!(record.state, AlertState::Posted);
    assert!(
        record.message_ts.is_some(),
        "and the second message is the one the resolution will edit, so the alert still \
         goes green"
    );
}
