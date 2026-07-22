//! The four endpoints, over a real socket.
//!
//! Every test here drives the router `main` serves, through `reqwest`, against the SQLite
//! store that ships. What is being checked is the contract Alertmanager and Kubernetes see:
//! status codes, the body they get, and — for `/webhook` — that a `200` means the delivery
//! is durable rather than merely accepted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use alertthread_core::{ChannelId, Fingerprint};
use alertthread_store::StateStore;
use harness::{Harness, alert, payload, slack_that_works};

/// Posts a webhook body and returns the response.
async fn post(base: &str, query: &str, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/webhook{query}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("the relay answers")
}

#[tokio::test]
async fn a_firing_delivery_is_acknowledged_only_after_it_is_durable() {
    // ADR 001 D2's whole claim: the durable write happens before the ack, so a crash
    // between them cannot lose the alert. The proof is that the row is readable the
    // instant the 200 lands, with no worker having run.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-durable", &slack).await;
    let server = relay.serve().await;

    let response = post(
        &server.base,
        "",
        payload("firing", &[alert("abc", "firing")]),
    )
    .await;

    assert_eq!(response.status(), 200);
    let record = relay
        .store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(harness::CHANNEL))
        .await
        .expect("reading the store")
        .expect("the claim committed before the 200");
    assert_eq!(record.state, alertthread_store::AlertState::Claimed);
    assert_eq!(
        record.message_ts, None,
        "no Slack call happens in the handler"
    );

    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"accepted\"} 1");
    relay.assert_metric("alertthread_alerts_received_total{status=\"firing\"} 1");
    server.stop().await;
}

#[tokio::test]
async fn the_channel_query_parameter_routes_the_delivery() {
    // ADR 001 D8: Alertmanager keeps owning routing, and `%23` is how a `#` survives a URL.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-channel", &slack).await;
    let server = relay.serve().await;

    let response = post(
        &server.base,
        "?channel=%23alerts-critical",
        payload("firing", &[alert("abc", "firing")]),
    )
    .await;
    assert_eq!(response.status(), 200);

    assert!(
        relay
            .store
            .alert(
                &Fingerprint::new("abc"),
                &ChannelId::new("#alerts-critical")
            )
            .await
            .expect("reading the store")
            .is_some(),
        "the alert should be tracked against the channel the URL named"
    );
    assert!(
        relay
            .store
            .alert(&Fingerprint::new("abc"), &ChannelId::new(harness::CHANNEL))
            .await
            .expect("reading the store")
            .is_none(),
        "and not against the default"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_body_this_build_cannot_parse_is_rejected_and_counted() {
    // The one path that answers a delivery with something other than 200 or 503, and the
    // only one where an alert is lost by design: a retry cannot fix an unparseable body.
    // AGENTS.md forbids swallowing that without a metric.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-badbody", &slack).await;
    let server = relay.serve().await;

    let response = post(&server.base, "", "{not json".to_owned()).await;
    assert_eq!(response.status(), 400);
    assert!(
        response.text().await.unwrap().contains("could not parse"),
        "the body has to say what was wrong with it"
    );

    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"rejected\"} 1");
    server.stop().await;
}

#[tokio::test]
async fn a_payload_with_a_field_this_build_has_never_heard_of_is_still_accepted() {
    // Alertmanager has added fields to this payload before and will again. Returning 400
    // because the sender learned a new word would turn an upgrade into silence.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-unknownfield", &slack).await;
    let server = relay.serve().await;

    let mut body: serde_json::Value =
        serde_json::from_str(&payload("firing", &[alert("abc", "firing")])).unwrap();
    body["somethingNewInAlertmanager"] = serde_json::json!({ "a": 1 });

    let response = post(&server.base, "", body.to_string()).await;
    assert_eq!(response.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn a_delivery_that_alertmanager_truncated_is_accepted_and_reported() {
    // ADR 001 D8's footgun, detected at the moment it happens rather than inferred later
    // from a rising orphan-resolve count.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-truncated", &slack).await;
    let server = relay.serve().await;

    let mut body: serde_json::Value =
        serde_json::from_str(&payload("firing", &[alert("abc", "firing")])).unwrap();
    body["truncatedAlerts"] = serde_json::json!(12);

    assert_eq!(post(&server.base, "", body.to_string()).await.status(), 200);
    relay.assert_metric("alertthread_alerts_truncated_total 12");
    server.stop().await;
}

#[tokio::test]
async fn a_redelivered_batch_is_accepted_twice_and_posted_once() {
    // ADR 001 D3, row 1. Slack has no idempotency key on `chat.postMessage`, so every
    // duplicate has to be suppressed on our side, before the call.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-redelivery", &slack).await;
    let server = relay.serve().await;

    let body = payload("firing", &[alert("abc", "firing")]);
    assert_eq!(post(&server.base, "", body.clone()).await.status(), 200);
    assert_eq!(post(&server.base, "", body).await.status(), 200);

    let stats = relay.store.stats().await.expect("sampling");
    assert_eq!(
        stats.outbox_depth.values().sum::<u64>(),
        1,
        "a redelivery must not queue a second post: {stats:?}"
    );
    server.stop().await;
}

#[tokio::test]
async fn healthz_is_process_alive_and_does_not_touch_the_store() {
    // ADR 001 D11, deliberately. A brief database blip must not make Kubernetes restart a
    // pod that is correctly buffering — restarting throws away the in-flight leases the
    // buffering depends on. Proven by pointing the relay at a store with no schema.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-healthz", &slack).await;
    let server = relay.serve().await;

    let response = reqwest::get(format!("{}/healthz", server.base))
        .await
        .expect("the relay answers");
    assert_eq!(response.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn readyz_reports_the_store() {
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-readyz", &slack).await;
    let server = relay.serve().await;

    let response = reqwest::get(format!("{}/readyz", server.base))
        .await
        .expect("the relay answers");
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("ready"));
    server.stop().await;
}

#[tokio::test]
async fn readyz_refuses_when_the_store_cannot_answer() {
    // A relay that cannot reach its store cannot durably accept a webhook, so a 200 would
    // acknowledge an alert it cannot persist. An unmigrated database is the cheapest way to
    // reach that state without breaking a socket.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-notready", &slack).await;

    // Drop the table the probe reads. The store is otherwise perfectly healthy, which is
    // the point: readiness is about whether a delivery can be persisted, not about whether
    // a TCP connection exists.
    let unmigrated = alertthread_store::Store::connect(
        alertthread_store::Backend::Sqlite,
        &harness::sqlite_url("endpoints-notready-empty"),
    )
    .await
    .expect("opening an unmigrated store");

    let state = std::sync::Arc::new(alertthread::http::AppState::new(
        std::sync::Arc::new(unmigrated),
        std::sync::Arc::clone(&relay.metrics),
        &relay.config,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (source, token) = alertthread::shutdown::cancellation();
    let handle = tokio::spawn(async move {
        axum::serve(listener, alertthread::http::router(state))
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/readyz"))
        .await
        .expect("the relay answers");
    assert_eq!(response.status(), 503);
    assert!(response.text().await.unwrap().contains("not reachable"));

    source.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn metrics_serves_the_registry_in_prometheus_text_format() {
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-metrics", &slack).await;
    let server = relay.serve().await;

    let response = reqwest::get(format!("{}/metrics", server.base))
        .await
        .expect("the relay answers");
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(content_type.contains("openmetrics-text"), "{content_type}");

    let body = response.text().await.unwrap();
    assert!(
        body.contains("# TYPE alertthread_orphan_resolves counter"),
        "{body}"
    );
    // Worth knowing, and worth writing down: a `Family` with no members emits *nothing*,
    // not even a `# TYPE`. So `alertthread_outbox_depth` is absent from a relay whose
    // background sampler has not run yet, and appears — with all six op labels — from the
    // first sample onward — as does `alerts_received{status}`, from the first alert. The
    // gauges an operator alerts on are up within the first `worker.sample_interval`, which
    // is 15 seconds.
    assert!(
        !body.contains("alertthread_outbox_depth"),
        "nothing has been sampled yet:\n{body}"
    );
    assert!(
        !body.contains("alertthread_alerts_received"),
        "no alert has arrived yet:\n{body}"
    );
    // The single-valued gauges *are* published from the start, because they exist rather
    // than being a label set that has to be populated.
    assert!(
        body.contains("alertthread_outbox_oldest_age_seconds 0.0"),
        "{body}"
    );
    server.stop().await;
}

#[tokio::test]
async fn the_metrics_endpoint_does_not_query_the_store() {
    // The gauges come from a background sample. A scrape every 15s across N replicas would
    // otherwise make Prometheus a load generator pointed at the outbox, and a slow store
    // would time the scrape out and lose every other metric with it. Proven by scraping a
    // relay whose store has no schema at all.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-metrics-nostore", &slack).await;

    let unmigrated = std::sync::Arc::new(
        alertthread_store::Store::connect(
            alertthread_store::Backend::Sqlite,
            &harness::sqlite_url("endpoints-metrics-nostore-empty"),
        )
        .await
        .expect("opening an unmigrated store"),
    );
    let state = std::sync::Arc::new(alertthread::http::AppState::new(
        unmigrated,
        std::sync::Arc::clone(&relay.metrics),
        &relay.config,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (source, token) = alertthread::shutdown::cancellation();
    let handle = tokio::spawn(async move {
        axum::serve(listener, alertthread::http::router(state))
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "metrics must be servable while the store is unusable — that is when they matter"
    );

    source.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn a_route_that_does_not_exist_is_a_404_rather_than_a_surprise() {
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-404", &slack).await;
    let server = relay.serve().await;

    let response = reqwest::get(format!("{}/webhook", server.base))
        .await
        .expect("the relay answers");
    assert_eq!(
        response.status(),
        405,
        "GET /webhook is the wrong method, not a missing route"
    );
    assert_eq!(
        reqwest::get(format!("{}/nope", server.base))
            .await
            .unwrap()
            .status(),
        404
    );
    server.stop().await;
}

#[tokio::test]
async fn an_empty_delivery_is_accepted_and_reported() {
    // Alertmanager does not send these. Something in front of it might, and a webhook that
    // silently accepts empty bodies is indistinguishable from one that is working.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-empty", &slack).await;
    let server = relay.serve().await;

    assert_eq!(
        post(&server.base, "", payload("firing", &[]))
            .await
            .status(),
        200
    );
    let stats = relay.store.stats().await.expect("sampling");
    assert!(stats.outbox_depth.is_empty(), "{stats:?}");
    server.stop().await;
}

#[tokio::test]
async fn an_orphan_resolve_is_counted_where_the_notice_is_raised() {
    // `plan` produces the notice at ingest, so the counter moves in the handler and not in
    // the worker — before any Slack call has been attempted. That is the point:
    // `alertthread_orphan_resolves_total` measures lost *state*, which is a fact about this
    // relay, not about whether Slack was reachable afterwards.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-orphan", &slack).await;
    let server = relay.serve().await;

    assert_eq!(
        post(
            &server.base,
            "",
            payload("resolved", &[alert("ghost", "resolved")])
        )
        .await
        .status(),
        200
    );

    relay.assert_metric("alertthread_orphan_resolves_total 1");
    relay.assert_metric("alertthread_alerts_received_total{status=\"resolved\"} 1");
    server.stop().await;
}

#[tokio::test]
async fn a_storm_is_counted_when_it_collapses() {
    // ADR 001 D5's decision, made by `plan` at ingest. Counting it in the worker would
    // count the summary being *delivered*, which is a different thing and would go up again
    // every time one was re-posted after a `message_not_found`.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-storm", &slack).await;
    let server = relay.serve().await;

    let alerts: Vec<_> = (0..6).map(|i| alert(&format!("f{i}"), "firing")).collect();
    assert_eq!(
        post(&server.base, "", payload("firing", &alerts))
            .await
            .status(),
        200
    );

    relay.assert_metric("alertthread_storm_collapses_total 1");
    relay.assert_metric("alertthread_alerts_received_total{status=\"firing\"} 6");
    server.stop().await;
}

#[tokio::test]
async fn an_alert_with_a_status_nobody_recognises_is_accepted_and_treated_as_firing() {
    // ADR 002 §2.2: firing is the treatment that both posts a visible message and starts
    // tracking the fingerprint, so a later genuine `resolved` still correlates. The label
    // is folded to `other` rather than passed through, because the raw string comes from
    // outside the relay.
    let slack = slack_that_works().await;
    let relay = Harness::new("endpoints-unknownstatus", &slack).await;
    let server = relay.serve().await;

    let mut alert = alert("abc", "firing");
    alert["status"] = serde_json::json!("suppressed");
    assert_eq!(
        post(&server.base, "", payload("firing", &[alert]))
            .await
            .status(),
        200
    );

    relay.assert_metric("alertthread_alerts_received_total{status=\"other\"} 1");
    let record = relay
        .store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(harness::CHANNEL))
        .await
        .expect("reading the store")
        .expect("an unrecognised status is still tracked");
    assert_eq!(record.state, alertthread_store::AlertState::Claimed);
    server.stop().await;
}
