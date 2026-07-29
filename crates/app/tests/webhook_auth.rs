//! The webhook perimeter, over a real socket.
//!
//! What these pin is the contract an operator configures against: which route the bearer
//! token covers, which three routes it must never cover, what a refusal looks like from the
//! outside, and that a refused delivery writes nothing. The router is the one `main` serves
//! and the store is the one that ships, so a test passing here has been through the real
//! middleware stack and the real extractors.
//!
//! The two facts most worth breaking a build over: `/healthz`, `/readyz` and `/metrics` stay
//! open, because a `401` on a probe is a pod that never becomes ready; and every refusal is
//! byte-for-byte identical, because a caller must not be able to tell a wrong credential from
//! a missing one.

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

/// The token every test in this file configures.
const TOKEN: &str = "s3cret-webhook-token";

/// `server.auth_token`, as an operator would write it.
fn with_token() -> String {
    format!("server:\n  auth_token: \"{TOKEN}\"\n")
}

/// Posts a webhook body, optionally carrying an `Authorization` header.
async fn post(base: &str, credential: Option<&str>, body: String) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/webhook"))
        .header("content-type", "application/json");
    if let Some(value) = credential {
        request = request.header("authorization", value);
    }
    request.body(body).send().await.expect("the relay answers")
}

fn firing() -> String {
    payload("firing", &[alert("abc", "firing")])
}

async fn tracked(relay: &Harness) -> bool {
    relay
        .store
        .alert(&Fingerprint::new("abc"), &ChannelId::new(harness::CHANNEL))
        .await
        .expect("reading the store")
        .is_some()
}

#[tokio::test]
async fn the_webhook_is_open_by_default() {
    // ADR 001 D11 makes the token optional, and off is the default: a relay that started
    // requiring a credential on upgrade would 401 every delivery from an Alertmanager nobody
    // had reconfigured yet, which is silence introduced by a security feature.
    let slack = slack_that_works().await;
    let relay = Harness::new("auth-open", &slack).await;
    let server = relay.serve().await;

    assert_eq!(post(&server.base, None, firing()).await.status(), 200);
    assert!(tracked(&relay).await);
    server.stop().await;
}

#[tokio::test]
async fn the_configured_token_is_accepted_and_the_delivery_is_still_durable() {
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-accepted", &slack, &with_token()).await;
    let server = relay.serve().await;

    let response = post(&server.base, Some(&format!("Bearer {TOKEN}")), firing()).await;
    assert_eq!(response.status(), 200);
    assert!(
        tracked(&relay).await,
        "an authenticated delivery is committed before the 200, exactly as an open one is"
    );
    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"accepted\"} 1");
    server.stop().await;
}

#[tokio::test]
async fn a_delivery_with_no_credential_is_refused_and_nothing_is_written() {
    // The alerts in a refused delivery are lost — Alertmanager does not retry a 401 — so the
    // refusal is counted and logged at ERROR rather than being a quiet 401.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-missing", &slack, &with_token()).await;
    let server = relay.serve().await;

    let response = post(&server.base, None, firing()).await;
    assert_eq!(response.status(), 401);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer"),
        "RFC 7235 requires a challenge on a 401, and a bare one says nothing about the token"
    );
    assert_eq!(response.text().await.unwrap(), "unauthorized\n");

    assert!(
        !tracked(&relay).await,
        "a refused delivery must not reach the store"
    );
    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"auth_missing\"} 1");
    server.stop().await;
}

#[tokio::test]
async fn a_wrong_credential_is_refused_identically_and_counted_differently() {
    // Identical on the wire, distinguishable in the metrics. A caller must not be able to
    // probe the token; an operator must be able to tell "Alertmanager sends nothing" from
    // "Alertmanager sends the old secret", because those are different fixes.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-mismatch", &slack, &with_token()).await;
    let server = relay.serve().await;

    let missing = post(&server.base, None, firing()).await;
    let missing_status = missing.status();
    let missing_headers = missing.headers().clone();
    let missing_body = missing.text().await.unwrap();

    let wrong = post(&server.base, Some("Bearer not-the-token"), firing()).await;
    assert_eq!(wrong.status(), missing_status);
    assert_eq!(
        wrong.headers().get("www-authenticate"),
        missing_headers.get("www-authenticate")
    );
    assert_eq!(wrong.text().await.unwrap(), missing_body);

    // A credential that shares a prefix with the real one is no different either.
    let nearly = post(
        &server.base,
        Some(&format!("Bearer {}", &TOKEN[..TOKEN.len() - 1])),
        firing(),
    )
    .await;
    assert_eq!(nearly.status(), 401);

    assert!(!tracked(&relay).await);
    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"auth_missing\"} 1");
    relay.assert_metric("alertthread_webhook_requests_total{outcome=\"auth_mismatch\"} 2");
    server.stop().await;
}

#[tokio::test]
async fn the_scheme_is_matched_case_insensitively_and_other_schemes_are_refused() {
    // RFC 7235 says the scheme is case-insensitive, and a relay that accepted only the
    // capitalisation Alertmanager happens to send would refuse every other correct sender.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-scheme", &slack, &with_token()).await;
    let server = relay.serve().await;

    for accepted in [
        format!("Bearer {TOKEN}"),
        format!("bearer {TOKEN}"),
        format!("BEARER {TOKEN}"),
    ] {
        assert_eq!(
            post(&server.base, Some(&accepted), firing()).await.status(),
            200,
            "{accepted:?}"
        );
    }

    for refused in [
        format!("Basic {TOKEN}"),
        TOKEN.to_owned(),
        "Bearer".to_owned(),
        String::new(),
    ] {
        assert_eq!(
            post(&server.base, Some(&refused), firing()).await.status(),
            401,
            "{refused:?}"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn liveness_readiness_and_metrics_stay_open_when_the_webhook_does_not() {
    // The single most important property in this file. A kubelet probe carries no credential,
    // so a 401 on `/readyz` is a pod that never joins the service — and a 401 on `/metrics`
    // is a relay whose own alerting stops, which is the failure this project exists to avoid.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-open-endpoints", &slack, &with_token()).await;
    let server = relay.serve().await;

    for path in ["healthz", "readyz", "metrics"] {
        let response = reqwest::get(format!("{}/{path}", server.base))
            .await
            .expect("the relay answers");
        assert_eq!(response.status(), 200, "/{path} must not require a token");
    }
    // And the webhook on the same router is closed, so this is not a relay that failed to
    // install the layer at all.
    assert_eq!(post(&server.base, None, firing()).await.status(), 401);
    server.stop().await;
}

#[tokio::test]
async fn the_wrong_method_is_still_a_405_rather_than_a_401() {
    // `route_layer`, not `layer`: the wrong method is not a secret, and answering it with a
    // 401 would make a misconfigured receiver harder to diagnose without making anything
    // safer.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-method", &slack, &with_token()).await;
    let server = relay.serve().await;

    assert_eq!(
        reqwest::get(format!("{}/webhook", server.base))
            .await
            .unwrap()
            .status(),
        405
    );
    server.stop().await;
}

#[tokio::test]
async fn a_blank_token_leaves_the_webhook_open() {
    // What a chart renders for a secret that did not resolve. It behaves as the default and
    // says so at startup (`run::report_webhook_auth`); refusing to start over an optional
    // security setting would be silence.
    let slack = slack_that_works().await;
    let relay = Harness::with_config("auth-blank", &slack, "server:\n  auth_token: \"\"\n").await;
    let server = relay.serve().await;

    assert_eq!(post(&server.base, None, firing()).await.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn a_token_read_from_a_file_is_the_one_enforced() {
    // The Kubernetes mounted-secret shape, including the trailing newline `kubectl create
    // secret --from-file` leaves behind — which would otherwise never match the header
    // Alertmanager sends.
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("auth-token-file");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("webhook-token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("writing the token");

    let slack = slack_that_works().await;
    let relay = Harness::with_config(
        "auth-token-file",
        &slack,
        &format!("server:\n  auth_token_file: {}\n", path.display()),
    )
    .await;
    let server = relay.serve().await;

    assert_eq!(
        post(&server.base, Some(&format!("Bearer {TOKEN}")), firing())
            .await
            .status(),
        200
    );
    // The 200 above is the whole assertion: an untrimmed configured value would be
    // `"s3cret-webhook-token\n"`, which no header Alertmanager can send ever matches.
    assert_eq!(
        post(&server.base, Some("Bearer wrong"), firing())
            .await
            .status(),
        401,
        "and the file did not simply disable the check"
    );
    server.stop().await;
    std::fs::remove_dir_all(&dir).expect("cleaning up");
}
