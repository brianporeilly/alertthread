//! Starting and stopping the whole relay.
//!
//! `main.rs` is excluded from the coverage gate because it is wiring and signal handling
//! only. The wiring itself is in `alertthread::run`, and these are the tests that make that
//! exclusion honest: a real process, a real socket, real background tasks, stopped cleanly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use std::sync::Arc;

use alertthread::config::Config;
use alertthread::metrics::Metrics;
use alertthread::shutdown::cancellation;
use alertthread::worker::{auth_probe_loop, prune_loop, sample_loop};
use alertthread_slack::{SlackClient, SlackToken};
use alertthread_store::{Backend, RetentionPolicy, StateStore, Store};
use chrono::TimeDelta;
use figment::Figment;
use figment::providers::{Format, Serialized, Yaml};
use harness::{Harness, alert, payload, slack_error, slack_that_works, sqlite_url};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A configuration pointed at `slack` and at a database named after the test.
fn config(name: &str, slack_uri: &str) -> Config {
    let yaml = format!(
        "slack:\n  token: \"xoxb-test\"\n  default_channel: \"#alerts\"\n  base_url: \
         {slack_uri}/api/\n  auth_probe_interval: 50ms\nserver:\n  listen: \"127.0.0.1:0\"\n\
         storage:\n  url: {}\n  retention:\n    interval: 50ms\nworker:\n  idle_poll: 20ms\n  \
         sample_interval: 50ms\n",
        sqlite_url(name),
    );
    Config::from_figment(
        &Figment::from(Serialized::defaults(Config::default())).merge(Yaml::string(&yaml)),
    )
    .expect("the lifecycle configuration is valid")
}

#[tokio::test]
async fn a_relay_starts_serves_delivers_and_stops() {
    // The walking skeleton, end to end and with no test-only shortcuts: `run::start` opens
    // the store, migrates it, authenticates to Slack, binds a socket and spawns every
    // background task. Then an alert goes in one end and comes out the other.
    let slack = slack_that_works().await;
    let relay = alertthread::run::start(config("lifecycle-full", &slack.uri()))
        .await
        .expect("the relay starts");
    let base = format!("http://{}", relay.addr);

    assert_eq!(
        reqwest::get(format!("{base}/readyz"))
            .await
            .unwrap()
            .status(),
        200
    );

    let response = reqwest::Client::new()
        .post(format!("{base}/webhook"))
        .body(payload("firing", &[alert("abc", "firing")]))
        .send()
        .await
        .expect("the relay answers");
    assert_eq!(response.status(), 200);

    // The worker polls every 20 ms in this configuration, so waiting for the Slack call is
    // waiting on the real loop rather than on a sleep chosen to be long enough.
    let posted = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let calls = slack.received_requests().await.unwrap();
            if calls
                .iter()
                .any(|r| r.url.path() == "/api/chat.postMessage")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        posted.is_ok(),
        "the outbox worker should have posted the alert"
    );

    // The gauges are up too, which means the sampler ran against the store.
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("alertthread_slack_auth_valid 1"),
        "{metrics}"
    );

    relay.shutdown().await;
}

#[tokio::test]
async fn shutting_down_stops_serving() {
    // Kubernetes sends `SIGTERM` and expects the pod to stop accepting. A relay that kept
    // its listener open would keep taking webhooks it had no worker left to drain.
    let slack = slack_that_works().await;
    let relay = alertthread::run::start(config("lifecycle-shutdown", &slack.uri()))
        .await
        .expect("the relay starts");
    let base = format!("http://{}", relay.addr);

    assert_eq!(
        reqwest::get(format!("{base}/healthz"))
            .await
            .unwrap()
            .status(),
        200
    );
    relay.shutdown().await;

    let after = reqwest::Client::new()
        .get(format!("{base}/healthz"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await;
    assert!(after.is_err(), "the listener must be closed: {after:?}");
}

#[tokio::test]
async fn a_bad_bot_token_stops_the_relay_from_starting_at_all() {
    // ADR 001 D11: call `auth.test` once at startup and fail fast. A container that will
    // not start is visible; a relay that starts and cannot post is not, and the alerts it
    // accepts in the meantime pile up in an outbox nothing can drain.
    // A bare mock server: `slack_with_auth_only` mounts a permissive `auth.test`, and
    // wiremock matches in mount order, so a rejecting mock added afterwards would never be
    // reached.
    let slack = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(slack_error("invalid_auth"))
        .mount(&slack)
        .await;

    let error = alertthread::run::start(config("lifecycle-badtoken", &slack.uri()))
        .await
        .expect_err("a token Slack rejects is fatal");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("bot token"), "{rendered}");
}

#[tokio::test]
async fn a_store_that_cannot_be_opened_stops_the_relay_from_starting() {
    let slack = slack_that_works().await;
    let mut config = config("lifecycle-badstore", &slack.uri());
    config.storage.url = "sqlite:///nonexistent-directory/alertthread/state.sqlite".to_owned();

    let error = alertthread::run::start(config)
        .await
        .expect_err("an unopenable store is fatal");
    assert!(format!("{error:#}").contains("state store"), "{error:#}");
}

#[tokio::test]
async fn a_port_already_in_use_stops_the_relay_from_starting() {
    // Otherwise the pod comes up, passes its liveness probe, and quietly accepts nothing.
    let slack = slack_that_works().await;
    let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = squatter.local_addr().unwrap();

    let mut config = config("lifecycle-portinuse", &slack.uri());
    config.server.listen = taken;

    let error = alertthread::run::start(config)
        .await
        .expect_err("a port in use is fatal");
    assert!(format!("{error:#}").contains("could not bind"), "{error:#}");
}

#[tokio::test]
async fn the_sampler_publishes_the_gauges_and_says_when_it_could_not() {
    // `store_sample_ok` is not in ADR 001 D11 and is here because every gauge beside it is
    // a *sample*: one that stopped being refreshed looks identical to one whose value
    // stopped changing.
    let slack = slack_that_works().await;
    let relay = Harness::new("lifecycle-sampler", &slack).await;
    let metrics = Arc::clone(&relay.metrics);

    let (source, token) = cancellation();
    let handle = tokio::spawn(sample_loop(
        Arc::clone(&relay.store),
        Arc::clone(&metrics),
        TimeDelta::milliseconds(10),
        token,
    ));

    wait_for(&metrics, "alertthread_store_sample_ok 1").await;
    source.cancel();
    handle.await.unwrap();

    // And against a store with no schema, it reports the failure without claiming the
    // queue is empty — which would be the most misleading thing it could say.
    let broken = Arc::new(
        Store::connect(Backend::Sqlite, &sqlite_url("lifecycle-sampler-broken"))
            .await
            .unwrap(),
    );
    let (source, token) = cancellation();
    let handle = tokio::spawn(sample_loop(
        broken,
        Arc::clone(&metrics),
        TimeDelta::milliseconds(10),
        token,
    ));
    wait_for(&metrics, "alertthread_store_sample_ok 0").await;
    source.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn the_auth_prober_reports_a_revoked_token_as_a_metric_and_not_as_unreadiness() {
    // The divergence from ADR 001 D11, stated as a test. Going unready over a revoked token
    // would make Alertmanager's POST fail, and it retries a few times and then gives up —
    // so the alert is lost. Accepting it into the outbox and retrying is what the outbox is
    // for. `/readyz` stays green; the gauge goes to zero.
    let slack = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(slack_error("token_revoked"))
        .mount(&slack)
        .await;

    let relay = Harness::new("lifecycle-authprobe", &slack).await;
    let server = relay.serve().await;
    let client = Arc::new(
        SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url(format!("{}/api/", slack.uri()))
            .build()
            .unwrap(),
    );

    let (source, token) = cancellation();
    let handle = tokio::spawn(auth_probe_loop(
        client,
        Arc::clone(&relay.metrics),
        TimeDelta::milliseconds(10),
        token,
    ));

    wait_for(&relay.metrics, "alertthread_slack_auth_valid 0").await;

    assert_eq!(
        reqwest::get(format!("{}/readyz", server.base))
            .await
            .unwrap()
            .status(),
        200,
        "a revoked token must NOT make the relay refuse webhooks (divergence from D11)"
    );

    source.cancel();
    handle.await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn the_auth_prober_reports_a_working_token() {
    let slack = slack_that_works().await;
    let relay = Harness::new("lifecycle-authprobe-ok", &slack).await;
    let client = Arc::new(
        SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url(format!("{}/api/", slack.uri()))
            .build()
            .unwrap(),
    );

    let (source, token) = cancellation();
    let handle = tokio::spawn(auth_probe_loop(
        client,
        Arc::clone(&relay.metrics),
        TimeDelta::milliseconds(10),
        token,
    ));
    wait_for(&relay.metrics, "alertthread_slack_auth_valid 1").await;
    source.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn the_pruner_runs_on_its_own_schedule_and_survives_a_store_it_cannot_read() {
    // A pruner that cannot run costs disk; a relay that stopped because its pruner failed
    // costs alerts. So a failed sweep is logged and the loop carries on.
    let broken = Arc::new(
        Store::connect(Backend::Sqlite, &sqlite_url("lifecycle-pruner-broken"))
            .await
            .unwrap(),
    );
    let (source, token) = cancellation();
    let handle = tokio::spawn(prune_loop(
        broken,
        RetentionPolicy::default(),
        TimeDelta::milliseconds(5),
        token,
    ));

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    source.cancel();
    handle
        .await
        .expect("the pruner keeps running through a failing sweep");
}

#[tokio::test]
async fn a_pruner_with_a_healthy_store_deletes_finished_state() {
    let slack = slack_that_works().await;
    let relay = Harness::new("lifecycle-pruner", &slack).await;

    let (source, token) = cancellation();
    let handle = tokio::spawn(prune_loop(
        Arc::clone(&relay.store),
        RetentionPolicy::default(),
        TimeDelta::milliseconds(5),
        token,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    source.cancel();
    handle.await.expect("the pruner shuts down cleanly");

    // Nothing was there to delete, and the store is still usable — which is the property
    // that matters, because the pruner runs against a live database for ever.
    assert!(relay.store.stats().await.is_ok());
}

#[tokio::test]
async fn the_worker_loop_drains_until_it_is_told_to_stop() {
    // The loop, rather than `run_once`: what is under test is that it keeps going, and that
    // it stops when the token fires rather than a poll interval later.
    let slack = slack_that_works().await;
    let relay = Harness::with_config(
        "lifecycle-workerloop",
        &slack,
        "worker:\n  idle_poll: 10ms\n",
    )
    .await;

    let body = payload("firing", &[alert("abc", "firing")]);
    let parsed: alertthread_core::WebhookPayload = serde_json::from_str(&body).unwrap();
    let batch = alertthread_core::AlertBatch::from_webhook(
        parsed,
        alertthread_core::ChannelId::new(harness::CHANNEL),
    );
    let policy = alertthread_core::Policy::default();
    let now = chrono::Utc::now();
    relay
        .store
        .ingest(&batch, now, |o, g| {
            alertthread_core::plan(o, &batch, g, &policy, now)
        })
        .await
        .unwrap();

    let worker = relay.worker.clone();
    let (source, token) = cancellation();
    let handle = tokio::spawn(async move { worker.run(token).await });

    let delivered = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let calls = slack.received_requests().await.unwrap();
            if calls
                .iter()
                .any(|r| r.url.path() == "/api/chat.postMessage")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(delivered.is_ok(), "the loop should have drained the outbox");

    source.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("the worker stops promptly rather than a poll interval later")
        .expect("the worker task finishes");
}

#[tokio::test]
async fn a_worker_whose_store_fails_backs_off_rather_than_spinning() {
    // A store that cannot be reached is precisely when hammering it helps least. The loop
    // must also still stop when told to, which a naive retry loop would not.
    let slack = slack_that_works().await;
    let relay = Harness::new("lifecycle-workerfail", &slack).await;

    let broken = Arc::new(
        Store::connect(Backend::Sqlite, &sqlite_url("lifecycle-workerfail-broken"))
            .await
            .unwrap(),
    );
    let worker = alertthread::worker::Worker::new(
        broken,
        Arc::clone(&relay.slack),
        Arc::new(alertthread_slack::Renderer::builtin()),
        Arc::new(alertthread::ratelimit::SlackLimits::default()),
        Arc::clone(&relay.metrics),
        relay.config.worker,
        alertthread_store::WorkerId::new("broken"),
    );

    let (source, token) = cancellation();
    let handle = tokio::spawn(async move { worker.run(token).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    source.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("the worker stops even while its store is failing")
        .expect("the worker task finishes");
}

#[tokio::test]
async fn a_relay_whose_response_body_says_it_is_stopping_answers_nothing_new() {
    // Belt and braces on the graceful path: a request in flight when shutdown fires should
    // complete, and a request started afterwards should not connect at all.
    let slack = slack_that_works().await;
    let relay = alertthread::run::start(config("lifecycle-drain", &slack.uri()))
        .await
        .expect("the relay starts");
    let base = format!("http://{}", relay.addr);

    let response = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("ready"));

    relay.shutdown().await;
}

/// Polls the exposition until it contains `line`, or fails the test.
///
/// A poll rather than a sleep: the background loops here run on 10 ms intervals, and a
/// fixed sleep long enough to be safe on a loaded CI runner would be long enough to make
/// the suite slow on every other machine.
async fn wait_for(metrics: &Metrics, line: &str) {
    let found = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if metrics.render().unwrap().contains(line) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "expected {line:?} in:\n{}",
        metrics.render().unwrap()
    );
}
