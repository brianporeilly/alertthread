//! Two ways a relay stops being able to talk to Slack, and how it comes back from each.
//!
//! # Startup auth is split on the D9 taxonomy, not on "did `auth.test` work"
//!
//! ADR 001 D11 says the relay calls `auth.test` once at startup and fails fast on a bad
//! token, and ROADMAP known open item 12 records what is wrong with reading that literally:
//! a relay restarted during a *Slack outage* never comes back, even though riding out
//! exactly that is what the outbox is for. Worse, it makes container start ordering
//! load-bearing.
//!
//! The split is [`SlackError::disposition`]'s, which is already the one place in the
//! codebase that answers "will this ever succeed?". `Disposition::Terminal` — `invalid_auth`,
//! `account_inactive`, `token_revoked`, a token with a newline in it — still refuses to
//! start, because none of those becomes true by waiting and a container that will not start
//! is visible where a relay that starts and cannot post is not. Everything else retries
//! within `slack.auth_startup_grace` and then **starts anyway** with
//! `alertthread_slack_auth_valid = 0`.
//!
//! # A parked alert is not written off for ever
//!
//! D9 dead-letters an `invalid_auth` post immediately rather than burning ten retries on a
//! token that will not work. That is right, and it leaves the alert permanently undelivered
//! — which is the outcome AGENTS.md says does not merge. The token becoming valid again is
//! the event that says the reason is gone, and the prober is what notices it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use std::sync::Arc;
use std::time::Duration;

use alertthread::config::Config;
use alertthread::metrics::Metrics;
use alertthread::shutdown::cancellation;
use alertthread::worker::{auth_probe_loop, dead_letter_loop};
use alertthread_core::{AlertBatch, ChannelId, Fingerprint, Policy, WebhookPayload, plan};
use alertthread_slack::{SlackClient, SlackToken};
use alertthread_store::{AlertState, StateStore, WorkerId};
use chrono::{TimeDelta, Utc};
use figment::Figment;
use figment::providers::{Format, Serialized, Yaml};
use harness::{
    CHANNEL, Harness, alert, payload, slack_error, slack_that_works, slack_with_auth_only,
    sqlite_url,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A configuration pointed at `slack`, with every interval short enough for a test.
///
/// `grace` is `slack.auth_startup_grace`, which is the setting every test below turns on.
fn config(name: &str, slack_uri: &str, grace: &str) -> Config {
    let yaml = format!(
        "slack:\n  token: \"xoxb-test\"\n  default_channel: \"{CHANNEL}\"\n  base_url: \
         {slack_uri}/api/\n  auth_probe_interval: 1h\n  auth_startup_grace: {grace}\n\
         server:\n  listen: \"127.0.0.1:0\"\n\
         storage:\n  url: {}\n  retention:\n    interval: 1h\nworker:\n  idle_poll: 20ms\n  \
         sample_interval: 50ms\n",
        sqlite_url(name),
    );
    Config::from_figment(
        &Figment::from(Serialized::defaults(Config::default())).merge(Yaml::string(&yaml)),
    )
    .expect("the resilience configuration is valid")
}

/// Mounts an `auth.test` that answers `status` the first `failures` times and then works.
///
/// Mount order matters: wiremock matches in the order mocks were added, so the exhausted
/// failure has to be registered first for the success behind it to ever be reached.
async fn auth_that_recovers(slack: &MockServer, failures: u64, status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(status))
        .up_to_n_times(failures)
        .mount(slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "team": "acme",
            "team_id": "T0123",
            "user": "alertthread",
            "user_id": "U0123",
            "bot_id": "B0123",
        })))
        .mount(slack)
        .await;
}

async fn metrics_of(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scraping")
        .text()
        .await
        .expect("a body")
}

// ---------------------------------------------------------------------------
// Startup auth (ROADMAP known open item 12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_slack_outage_at_startup_does_not_stop_the_relay_coming_back() {
    // The case item 12 is about. `auth.test` answers 503 for ever, which
    // `SlackError::disposition` classifies as retryable, so the relay starts degraded rather
    // than refusing. It has to: Alertmanager delivers to a pod that is *running*, and a
    // relay that will not start during a Slack outage loses every alert fired during it —
    // from a condition the outbox was specifically designed to survive.
    // A bare server, not `slack_with_auth_only`: that helper mounts a working `auth.test`,
    // and wiremock matches in mount order, so a failing one added afterwards is never hit.
    let outage = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&outage)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&outage)
        .await;

    let relay = alertthread::run::start(config("resilience-outage", &outage.uri(), "0s"))
        .await
        .expect("a Slack outage is not a reason to refuse to start");
    let base = format!("http://{}", relay.addr);

    let text = metrics_of(&base).await;
    assert!(
        text.contains("alertthread_slack_auth_valid 0"),
        "the relay has to say it cannot reach Slack, loudly and in the metric operators \
         already alert on:\n{text}"
    );

    // And it still accepts alerts, which is the entire point: they go into the outbox and
    // are delivered when Slack comes back.
    let response = reqwest::Client::new()
        .post(format!("{base}/webhook"))
        .body(payload("firing", &[alert("abc", "firing")]))
        .send()
        .await
        .expect("the relay answers");
    assert_eq!(
        response.status(),
        200,
        "refusing the webhook would lose the alert; queuing it is what the outbox is for"
    );
    assert_eq!(
        reqwest::get(format!("{base}/readyz"))
            .await
            .unwrap()
            .status(),
        200,
        "and it stays ready, per the divergence from D11 recorded in ROADMAP item 8"
    );

    relay.shutdown().await;
}

#[tokio::test]
async fn a_transient_failure_that_clears_inside_the_grace_starts_fully_authenticated() {
    // The retry is not decoration: a DNS blip or a proxy 503 during a rolling restart
    // clears in a second, and starting degraded would leave the auth gauge at zero until
    // the fifteen-minute prober got round to it.
    let slack = MockServer::start().await;
    auth_that_recovers(&slack, 2, 503).await;

    let relay = alertthread::run::start(config("resilience-recovers", &slack.uri(), "20s"))
        .await
        .expect("the relay starts");

    let text = metrics_of(&format!("http://{}", relay.addr)).await;
    assert!(
        text.contains("alertthread_slack_auth_valid 1"),
        "the retry succeeded, so the relay is not degraded:\n{text}"
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn a_token_slack_definitively_rejects_still_refuses_to_start() {
    // The half of D11 that survives item 12 unchanged. These are the codes
    // `SlackError::from_api_code` maps to `InvalidAuth`, and no amount of waiting makes any
    // of them true — a relay that started on one would accept alerts it can never deliver.
    for code in ["invalid_auth", "account_inactive", "token_revoked"] {
        let slack = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth.test"))
            .respond_with(slack_error(code))
            .mount(&slack)
            .await;

        let error = alertthread::run::start(config(
            &format!("resilience-terminal-{code}"),
            &slack.uri(),
            // A generous grace, to prove the refusal is about the *classification* and not
            // about running out of time.
            "1h",
        ))
        .await
        .expect_err("{code} is definitive");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("bot token"), "{code}: {rendered}");
        assert!(rendered.contains(code), "{code}: {rendered}");
    }
}

#[tokio::test]
async fn a_definitive_rejection_is_not_retried_even_once() {
    // A grace of an hour must not mean an hour of `auth.test` calls against a token Slack
    // has already refused. One call, one refusal, done.
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(slack_error("invalid_auth"))
        .mount(&slack)
        .await;

    alertthread::run::start(config("resilience-noretry", &slack.uri(), "1h"))
        .await
        .expect_err("a rejected token is fatal");

    let calls = slack
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/auth.test")
        .count();
    assert_eq!(calls, 1, "D9: do not burn retries on a terminal failure");
}

// ---------------------------------------------------------------------------
// Dead-letter handling and recovery
// ---------------------------------------------------------------------------

/// Ingests one firing alert, drains it against a Slack that rejects the token, and returns
/// the parked op that leaves behind.
async fn park_an_alert(relay: &Harness) {
    let body = payload("firing", &[alert("abc", "firing")]);
    let parsed: WebhookPayload = serde_json::from_str(&body).expect("the fixture parses");
    let batch = AlertBatch::from_webhook(parsed, ChannelId::new(CHANNEL));
    let policy = Policy::default();
    let now = Utc::now();
    relay
        .store
        .ingest(&batch, now, |outcomes, group| {
            plan(outcomes, &batch, group, &policy, now)
        })
        .await
        .expect("ingesting");

    let pass = relay.worker.run_once(now).await.expect("draining");
    assert_eq!(pass.dead_lettered, 1, "the token was refused: {pass:?}");
}

#[tokio::test]
async fn an_alert_parked_by_a_bad_token_is_readable_with_the_reason_it_was_parked() {
    // `alertthread_outbox_dead_lettered` is a number. This is the route from that number to
    // the alert behind it — which is the difference between an operator who knows three
    // alerts were lost and one who knows *which* three.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("invalid_auth"))
        .mount(&slack)
        .await;
    let relay = Harness::new("resilience-parked", &slack).await;
    park_an_alert(&relay).await;

    relay.assert_metric("alertthread_dead_letter_total{reason=\"invalid_auth\"} 1");

    let parked = relay.store.dead_letters(100).await.expect("listing");
    assert_eq!(parked.len(), 1);
    assert!(
        parked[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("invalid_auth"),
        "the reason has to survive into the row: {:?}",
        parked[0]
    );
    assert!(
        format!("{:?}", parked[0].op).contains("abc"),
        "and so does the alert it was for: {:?}",
        parked[0]
    );

    // The reporter reads it back and shuts down cleanly, which is what the process does on
    // every restart for as long as the row is there.
    let (source, token) = cancellation();
    let reporting = tokio::spawn(dead_letter_loop(
        Arc::clone(&relay.store),
        TimeDelta::milliseconds(10),
        token,
    ));
    tokio::time::sleep(Duration::from_millis(40)).await;
    source.cancel();
    tokio::time::timeout(Duration::from_secs(5), reporting)
        .await
        .expect("the reporter stops promptly")
        .expect("the reporter task finishes");
}

#[tokio::test]
async fn a_token_that_starts_working_again_returns_the_alerts_it_cost() {
    // The recovery path. Without it, replacing a revoked token delivers every *future*
    // alert and silently writes off every one that arrived while it was broken — and those
    // are the ones from the window somebody is about to ask about.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("invalid_auth"))
        .mount(&slack)
        .await;
    let relay = Harness::new("resilience-revive", &slack).await;
    park_an_alert(&relay).await;

    assert_eq!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .expect("row exists")
            .state,
        AlertState::Failed
    );

    // A second Slack, standing in for the operator having replaced the token. The prober is
    // told the relay started degraded, which is what makes the next success a *transition*.
    let fixed = slack_that_works().await;
    let client = Arc::new(
        SlackClient::builder(SlackToken::new("xoxb-fixed"))
            .base_url(format!("{}/api/", fixed.uri()))
            .build()
            .unwrap(),
    );
    let (source, token) = cancellation();
    let probing = tokio::spawn(auth_probe_loop(
        client,
        Arc::clone(&relay.store),
        Arc::clone(&relay.metrics),
        TimeDelta::milliseconds(10),
        false,
        token,
    ));

    let revived = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if relay
                .store
                .dead_letters(100)
                .await
                .expect("listing")
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(revived.is_ok(), "the parked alert was never returned");
    source.cancel();
    probing.await.expect("the prober stops");

    relay.assert_metric("alertthread_dead_letter_revived_total 1");
    assert_eq!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .expect("row exists")
            .state,
        AlertState::Claimed,
        "and the alert is claimed again, so its resolution correlates instead of orphaning"
    );

    // The proof that matters: a worker pointed at the fixed Slack now delivers it.
    let worker = alertthread::worker::Worker::new(
        Arc::clone(&relay.store),
        Arc::new(
            SlackClient::builder(SlackToken::new("xoxb-fixed"))
                .base_url(format!("{}/api/", fixed.uri()))
                .build()
                .unwrap(),
        ),
        Arc::new(alertthread_slack::Renderer::builtin()),
        Arc::new(alertthread::ratelimit::SlackLimits::default()),
        Arc::clone(&relay.metrics),
        relay.config.worker,
        WorkerId::new("after-the-fix"),
    );
    let pass = worker.run_once(Utc::now()).await.expect("draining");
    assert_eq!(pass.completed, 1, "the late alert is delivered: {pass:?}");
    assert_eq!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(CHANNEL))
            .await
            .expect("reading")
            .expect("row exists")
            .state,
        AlertState::Posted
    );
}

#[tokio::test]
async fn a_healthy_prober_does_not_keep_reviving_the_dead_letter_queue() {
    // The revival is keyed on the invalid→valid *transition*, not on the token being valid.
    // A prober that revived on every tick would put an op Slack genuinely refuses back in
    // the queue every fifteen minutes, for ever, at the cost of a Slack call each time.
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("msg_too_long"))
        .mount(&slack)
        .await;
    let relay = Harness::new("resilience-no-churn", &slack).await;
    park_an_alert(&relay).await;

    let client = Arc::new(
        SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url(format!("{}/api/", slack.uri()))
            .build()
            .unwrap(),
    );
    let (source, token) = cancellation();
    let probing = tokio::spawn(auth_probe_loop(
        client,
        Arc::clone(&relay.store),
        Arc::clone(&relay.metrics),
        TimeDelta::milliseconds(5),
        true,
        token,
    ));
    tokio::time::sleep(Duration::from_millis(60)).await;
    source.cancel();
    probing.await.expect("the prober stops");

    assert_eq!(
        relay.store.dead_letters(100).await.expect("listing").len(),
        1,
        "the token never stopped working, so nothing was recovered"
    );
    assert!(
        !relay
            .metrics_text()
            .contains("alertthread_dead_letter_revived_total 1"),
        "and nothing was counted as recovered either"
    );
}

/// Compile-time reminder that the metric an operator pages on is still registered.
#[test]
fn the_dead_letter_gauges_are_still_in_the_exposition() {
    let metrics = Metrics::new();
    metrics.dead_lettered("invalid_auth");
    metrics.dead_letters_revived(3);
    let text = metrics.render().expect("the registry encodes");
    assert!(text.contains("alertthread_dead_letter_total"), "{text}");
    assert!(
        text.contains("alertthread_dead_letter_revived_total 3"),
        "{text}"
    );
    assert!(text.contains("alertthread_outbox_dead_lettered"), "{text}");
}
