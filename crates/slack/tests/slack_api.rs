//! ADR 001 D9's failure table, as a test matrix against a fake Slack.
//!
//! Every row of D9 that is expressible at this layer is here, plus the two things D9
//! assumes rather than states: that `{"ok": false}` under an HTTP 200 is a failure, and
//! that a rate limit is not a failed attempt.
//!
//! The assertion in almost every case is on [`SlackError::disposition`] rather than on the
//! variant. That is the contract Phase 4 consumes — the worker decides between
//! `defer(RateLimited)`, `defer(Backoff)`, `dead_letter` and `complete(MessageLost)` from
//! the disposition and nothing else — so it is the thing worth pinning.

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

use std::time::Duration;

use alertthread_core::{ChannelId, Fingerprint, LabelMap, MessageTs, ThreadTs};
use alertthread_slack::{
    AlertView, Disposition, MessageBody, PostMessage, RETRY_AFTER_DEFAULT, RETRY_AFTER_MAX,
    RETRY_AFTER_MIN, RenderRequest, Renderer, SlackClient, SlackError, SlackMethod, SlackToken,
    TemplateKind, UpdateMessage, update_group,
};
use serde_json::json;
use wiremock::matchers::{body_json_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn client(server: &MockServer) -> SlackClient {
    SlackClient::builder(SlackToken::new("xoxb-test-token"))
        .base_url(server.uri())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client builds")
}

/// A real rendered body, from a template pinned to a fixed string.
///
/// Built through the renderer rather than by hand, so the bytes these tests assert on are
/// the bytes the production path produces — a hand-assembled body could stay green while
/// the renderer's output drifted away from it.
fn body() -> MessageBody {
    let (renderer, rejected) = Renderer::new([(TemplateKind::Firing, "fixture body".to_owned())]);
    assert!(rejected.is_empty());

    let alert = AlertView {
        fingerprint: Fingerprint::new("a1b2c3"),
        labels: LabelMap::new(),
        annotations: LabelMap::new(),
        starts_at: chrono::DateTime::from_timestamp(1_784_642_520, 0).expect("in range"),
        resolved_at: None,
        generator_url: String::new(),
    };
    renderer
        .render(
            &RenderRequest::Firing(&alert),
            chrono::DateTime::from_timestamp(1_784_642_520, 0).expect("in range"),
        )
        .body
}

/// The JSON `body()` serialises to, as Slack receives it.
fn body_json() -> serde_json::Value {
    json!({
        "text": "fixture body",
        "attachments": [{
            "color": "#d40e0d",
            "fallback": "fixture body",
            "blocks": [{ "type": "section", "text": { "type": "mrkdwn", "text": "fixture body" } }],
        }],
    })
}

fn channel() -> ChannelId {
    ChannelId::new("#alerts")
}

/// Mounts one canned response for one Slack method.
async fn mount(server: &MockServer, api: SlackMethod, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(format!("/{}", api.as_str())))
        .respond_with(response)
        .mount(server)
        .await;
}

/// `chat.postMessage` against a server that answers `response`.
async fn post_against(
    response: ResponseTemplate,
) -> Result<alertthread_slack::PostedMessage, SlackError> {
    let server = MockServer::start().await;
    mount(&server, SlackMethod::PostMessage, response).await;
    let channel = channel();
    let body = body();
    client(&server)
        .post_message(&PostMessage::to_channel(&channel, &body))
        .await
}

/// `chat.update` against a server that answers `response`.
async fn update_against(response: ResponseTemplate) -> Result<(), SlackError> {
    let server = MockServer::start().await;
    mount(&server, SlackMethod::UpdateMessage, response).await;
    let channel = channel();
    let ts = MessageTs::new("1784642520.000100");
    let body = body();
    client(&server)
        .update_message(&UpdateMessage::new(&channel, &ts, &body))
        .await
}

/// Folds the addressing fields onto a rendered body, the way the client does.
fn merge(into: &mut serde_json::Value, from: &serde_json::Value) {
    let (Some(target), Some(source)) = (into.as_object_mut(), from.as_object()) else {
        panic!("both fixtures must be JSON objects");
    };
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

/// An `ok: false` body, served with HTTP 200 exactly as Slack does.
fn api_error(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "ok": false, "error": code }))
}

// ---------------------------------------------------------------------------
// The happy paths, so the failure tests are known to be testing failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_post_returns_the_timestamp_that_makes_correlation_possible() {
    let posted = post_against(ResponseTemplate::new(200).set_body_json(json!({
        "ok": true,
        "channel": "C0123456789",
        "ts": "1784642520.000100",
    })))
    .await
    .expect("the post succeeds");

    assert_eq!(posted.ts, MessageTs::new("1784642520.000100"));
    // Slack's canonical ID, not the `#alerts` we sent: it is the more useful of the two
    // and it is what `chat.update` will be addressed with.
    assert_eq!(posted.channel, ChannelId::new("C0123456789"));
    assert_eq!(posted.thread_ts(), ThreadTs::new("1784642520.000100"));
}

#[tokio::test]
async fn a_post_whose_response_omits_the_channel_falls_back_to_the_one_we_addressed() {
    let posted =
        post_against(ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "ts": "1.1" })))
            .await
            .expect("the post succeeds");
    assert_eq!(posted.channel, channel());
}

#[tokio::test]
async fn a_successful_update_reports_nothing_because_there_is_nothing_to_record() {
    update_against(ResponseTemplate::new(200).set_body_json(json!({
        "ok": true, "channel": "C0123456789", "ts": "1784642520.000100",
    })))
    .await
    .expect("the update succeeds");
}

#[tokio::test]
async fn auth_test_reports_the_bot_identity() {
    // ADR 001 D11: called once at startup to fail fast on a bad token and to log who we
    // are, and again from `/readyz`.
    let server = MockServer::start().await;
    mount(
        &server,
        SlackMethod::AuthTest,
        ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "url": "https://example.slack.com/",
            "team": "Example",
            "user": "alertthread",
            "team_id": "T0123",
            "user_id": "U0456",
            "bot_id": "B0789",
        })),
    )
    .await;

    let identity = client(&server)
        .auth_test()
        .await
        .expect("auth.test succeeds");
    assert_eq!(identity.team, "Example");
    assert_eq!(identity.team_id, "T0123");
    assert_eq!(identity.user, "alertthread");
    assert_eq!(identity.user_id, "U0456");
    assert_eq!(identity.bot_id, "B0789");
}

#[tokio::test]
async fn auth_test_tolerates_a_response_carrying_only_the_user_id() {
    let server = MockServer::start().await;
    mount(
        &server,
        SlackMethod::AuthTest,
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "user_id": "U0456" })),
    )
    .await;

    let identity = client(&server)
        .auth_test()
        .await
        .expect("auth.test succeeds");
    assert_eq!(identity.user_id, "U0456");
    assert_eq!(identity.team, "");
}

// ---------------------------------------------------------------------------
// The single most important test in this crate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_200_with_ok_false_is_a_failure_and_not_a_delivered_alert() {
    // Slack answers application errors with HTTP 200. A client that trusts the status
    // line records a post that never happened, marks the alert `posted`, and nobody is
    // ever told — the exact failure AGENTS.md calls the worst possible bug.
    let error = post_against(api_error("channel_not_found"))
        .await
        .expect_err("ok:false under a 200 must not be a success");

    assert!(
        matches!(error, SlackError::ChannelUnusable { .. }),
        "{error:?}"
    );
    assert_eq!(error.disposition(), Disposition::Terminal);
    assert_eq!(error.method(), Some(SlackMethod::PostMessage));
}

#[tokio::test]
async fn a_body_with_no_ok_field_at_all_is_not_a_success_either() {
    // Defaulting `ok` to false is what makes a truncated or proxied response fail loudly
    // rather than look like a delivered alert.
    let error = post_against(ResponseTemplate::new(200).set_body_json(json!({ "ts": "1.1" })))
        .await
        .expect_err("a body with no `ok` must not be a success");
    assert!(
        matches!(error, SlackError::Unrecognised { .. }),
        "{error:?}"
    );
    assert_eq!(error.disposition(), Disposition::Retry);
}

// ---------------------------------------------------------------------------
// D9: "Slack 429 — honour Retry-After, do not count as a failed attempt"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_429_reports_the_delay_slack_asked_for_and_does_not_burn_an_attempt() {
    let error = post_against(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "30")
            .set_body_string("rate limited"),
    )
    .await
    .expect_err("429 is an error");

    assert_eq!(
        error.disposition(),
        Disposition::RateLimited {
            retry_after: Duration::from_secs(30)
        }
    );
    assert!(
        !error.disposition().counts_as_an_attempt(),
        "counting a rate limit would dead-letter alerts during the storm that caused it"
    );
    assert_eq!(error.outcome(), "rate_limited");
}

#[tokio::test]
async fn a_429_with_no_body_at_all_is_still_a_rate_limit() {
    // Slack's 429 does not always carry a JSON envelope. Reading the body first and
    // failing to decode it would classify a rate limit as a retry — which burns an
    // attempt, which is what D2 says must not happen.
    let error = post_against(ResponseTemplate::new(429).insert_header("retry-after", "3"))
        .await
        .expect_err("429 is an error");
    assert_eq!(
        error.disposition(),
        Disposition::RateLimited {
            retry_after: Duration::from_secs(3)
        }
    );
}

#[tokio::test]
async fn a_429_without_a_retry_after_header_falls_back_to_the_documented_tier() {
    let error = post_against(ResponseTemplate::new(429))
        .await
        .expect_err("429 is an error");
    assert_eq!(
        error.disposition(),
        Disposition::RateLimited {
            retry_after: RETRY_AFTER_DEFAULT
        }
    );
}

#[tokio::test]
async fn an_absurd_retry_after_is_clamped_at_both_ends() {
    for (header_value, expected) in [("0", RETRY_AFTER_MIN), ("86400", RETRY_AFTER_MAX)] {
        let error =
            post_against(ResponseTemplate::new(429).insert_header("retry-after", header_value))
                .await
                .expect_err("429 is an error");
        assert_eq!(
            error.disposition(),
            Disposition::RateLimited {
                retry_after: expected
            },
            "retry-after: {header_value}"
        );
    }
}

#[tokio::test]
async fn slack_also_rate_limits_with_an_http_200_and_that_is_treated_identically() {
    // The shape that catches clients out: `{"ok": false, "error": "ratelimited"}` under a
    // 200, with the delay still in the header. Classifying it as an ordinary application
    // error would burn an attempt per rate limit.
    let error = post_against(
        ResponseTemplate::new(200)
            .insert_header("retry-after", "12")
            .set_body_json(json!({ "ok": false, "error": "ratelimited" })),
    )
    .await
    .expect_err("a rate limit is an error whatever status carries it");

    assert_eq!(
        error.disposition(),
        Disposition::RateLimited {
            retry_after: Duration::from_secs(12)
        }
    );
}

// ---------------------------------------------------------------------------
// D9: `chat.update` -> message_not_found, and ADR 002 §1.3 for group summaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn message_not_found_on_an_alert_message_asks_for_a_fresh_post() {
    // D9: clear `message_ts`, post a fresh message. D7 calls this a free liveness probe
    // on our own correlation state.
    let error = update_against(api_error("message_not_found"))
        .await
        .expect_err("message_not_found is an error");

    assert_eq!(error.disposition(), Disposition::MessageGone);
    assert!(
        !error.disposition().counts_as_an_attempt(),
        "self-healing is not a retry"
    );
    assert_eq!(error.method(), Some(SlackMethod::UpdateMessage));
}

#[tokio::test]
async fn message_not_found_on_a_group_summary_asks_for_exactly_the_same_thing() {
    // ADR 002 §1.3. The natural implementation is a silent no-op — a summary is "just" a
    // rollup — and that orphans every threaded child under a parent that is gone. The
    // symmetry is the invariant, so this test exists to break if it is lost.
    let server = MockServer::start().await;
    mount(
        &server,
        SlackMethod::UpdateMessage,
        api_error("message_not_found"),
    )
    .await;

    let channel = channel();
    let parent = ThreadTs::new("1784642520.000001");
    let body = body();
    let owned = update_group(&channel, &parent, &body);
    let error = client(&server)
        .update_message(&owned.as_request())
        .await
        .expect_err("message_not_found is an error");

    assert_eq!(error.disposition(), Disposition::MessageGone);
}

#[tokio::test]
async fn any_other_update_error_is_retried_with_backoff() {
    // D9: "chat.update → any other error → retry with backoff; on exhaustion, post a
    // standalone message." The exhaustion half belongs to Phase 4's worker; the half this
    // layer owns is classifying it as retryable at all.
    let error = update_against(api_error("internal_error"))
        .await
        .expect_err("internal_error is an error");

    assert_eq!(error.disposition(), Disposition::Retry);
    assert!(error.disposition().counts_as_an_attempt());
}

// ---------------------------------------------------------------------------
// D9: invalid_auth dead-letters immediately; 5xx backs off
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_auth_dead_letters_immediately_rather_than_burning_retries() {
    // D9, verbatim. Ten retries of a bad token is ten alerts' worth of worker capacity
    // spent achieving nothing, and the token does not become valid in the meantime.
    for code in [
        "invalid_auth",
        "not_authed",
        "account_inactive",
        "token_revoked",
    ] {
        let error = post_against(api_error(code)).await.expect_err("must fail");
        assert_eq!(error.disposition(), Disposition::Terminal, "{code}");
        assert_eq!(error.outcome(), "invalid_auth", "{code}");
    }
}

#[tokio::test]
async fn a_slack_5xx_is_retryable() {
    for status in [500_u16, 502, 503, 504] {
        let error = post_against(ResponseTemplate::new(status).set_body_string("upstream down"))
            .await
            .expect_err("5xx is an error");
        assert_eq!(error.disposition(), Disposition::Retry, "{status}");
        assert!(
            error.to_string().contains("upstream down"),
            "the body is the diagnosis: {error}"
        );
    }
}

#[tokio::test]
async fn a_4xx_that_is_not_a_rate_limit_is_terminal() {
    // A 404 or a 403 here is a wrong `base_url` or a proxy refusing us — nothing that
    // changes by being tried again in thirty seconds.
    for status in [400_u16, 403, 404] {
        let error = post_against(ResponseTemplate::new(status).set_body_string("nope"))
            .await
            .expect_err("4xx is an error");
        assert_eq!(error.disposition(), Disposition::Terminal, "{status}");
    }
}

#[tokio::test]
async fn a_message_slack_rejects_as_malformed_is_terminal_and_says_which_code() {
    // The path the block-limit truncation exists to keep us off. If it is ever reached,
    // the dead-lettered row's `last_error` has to name the reason.
    let error = post_against(api_error("invalid_blocks"))
        .await
        .expect_err("invalid_blocks is an error");
    assert_eq!(error.disposition(), Disposition::Terminal);
    assert!(error.to_string().contains("invalid_blocks"), "{error}");
}

// ---------------------------------------------------------------------------
// Everything between us and Slack
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_that_is_not_json_is_retryable_rather_than_a_delivered_alert() {
    // A captive portal or a proxy error page, served with a 200. Treating it as success
    // would be silence; treating it as terminal would dead-letter an alert over something
    // that is usually transient.
    let error = post_against(
        ResponseTemplate::new(200).set_body_string("<html>proxy authentication required</html>"),
    )
    .await
    .expect_err("html is not a Slack envelope");

    assert!(
        matches!(error, SlackError::MalformedResponse { .. }),
        "{error:?}"
    );
    assert_eq!(error.disposition(), Disposition::Retry);
}

#[tokio::test]
async fn a_post_slack_accepted_without_returning_a_ts_is_not_a_success() {
    // A message we cannot record the timestamp of can never be updated or resolved, so
    // treating it as posted would leave a permanently red message in the channel and a
    // resolution that has nothing to edit.
    let error = post_against(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .await
        .expect_err("a post with no ts is not a post");

    assert!(
        matches!(error, SlackError::IncompleteResponse { field: "ts", .. }),
        "{error:?}"
    );
    assert_eq!(error.disposition(), Disposition::Retry);
}

#[tokio::test]
async fn auth_test_without_a_user_id_is_not_a_valid_identity() {
    let server = MockServer::start().await;
    mount(
        &server,
        SlackMethod::AuthTest,
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "team": "Example" })),
    )
    .await;

    let error = client(&server)
        .auth_test()
        .await
        .expect_err("an identity with no user id is not an identity");
    assert!(
        matches!(
            error,
            SlackError::IncompleteResponse {
                field: "user_id",
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_connection_that_is_refused_is_a_retryable_transport_error() {
    let channel = channel();
    let body = body();
    let error = SlackClient::builder(SlackToken::new("xoxb-test"))
        // Port 1 is reserved and nothing listens on it, so the connection is refused
        // immediately rather than after a timeout.
        .base_url("http://127.0.0.1:1/api")
        .build()
        .expect("client builds")
        .post_message(&PostMessage::to_channel(&channel, &body))
        .await
        .expect_err("nothing is listening");

    assert!(matches!(error, SlackError::Transport { .. }), "{error:?}");
    assert_eq!(error.disposition(), Disposition::Retry);
    assert_eq!(error.outcome(), "transport");
    assert_eq!(error.method(), Some(SlackMethod::PostMessage));
}

#[tokio::test]
async fn a_slack_that_answers_too_slowly_is_a_retryable_transport_error() {
    // Deliberately *not* terminal: a slow Slack is a slow Slack. Note the direction this
    // trades in — a retry of `chat.postMessage` can duplicate a message, which is why
    // `DEFAULT_TIMEOUT` is generous rather than tight (ADR 001 D3).
    let server = MockServer::start().await;
    mount(
        &server,
        SlackMethod::PostMessage,
        ResponseTemplate::new(200)
            .set_delay(Duration::from_secs(2))
            .set_body_json(json!({ "ok": true, "ts": "1.1" })),
    )
    .await;

    let channel = channel();
    let body = body();
    let error = SlackClient::builder(SlackToken::new("xoxb-test"))
        .base_url(server.uri())
        .timeout(Duration::from_millis(50))
        .build()
        .expect("client builds")
        .post_message(&PostMessage::to_channel(&channel, &body))
        .await
        .expect_err("the client timed out");

    assert!(matches!(error, SlackError::Transport { .. }), "{error:?}");
    assert_eq!(error.disposition(), Disposition::Retry);
}

// ---------------------------------------------------------------------------
// What actually goes on the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_request_carries_the_bearer_token_and_a_json_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-test-token"))
        .and(header("content-type", "application/json; charset=utf-8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "ts": "1.1" })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = channel();
    let body = body();
    client(&server)
        .post_message(&PostMessage::to_channel(&channel, &body))
        .await
        .expect("the post succeeds");
    // `expect(1)` above is verified when the server drops.
}

#[tokio::test]
async fn a_threaded_reply_sends_its_parent_and_does_not_broadcast() {
    // ADR 001 D5 and D6. `reply_broadcast: true` would echo every resolve back into the
    // channel and undo the noise reduction the whole project is for.
    let server = MockServer::start().await;
    let mut expected_value = body_json();
    merge(
        &mut expected_value,
        &json!({
            "channel": "#alerts",
            "thread_ts": "1784642520.000001",
            "reply_broadcast": false,
        }),
    );
    let expected = serde_json::to_string(&expected_value).expect("fixture serialises");

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .and(body_json_string(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "ts": "1.2" })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = channel();
    let body = body();
    let parent = ThreadTs::new("1784642520.000001");
    client(&server)
        .post_message(&PostMessage::in_thread(&channel, &body, &parent))
        .await
        .expect("the reply posts");
}

#[tokio::test]
async fn an_update_addresses_the_message_by_channel_and_timestamp() {
    // `chat.update(channel, ts)` takes two strings and swapping them compiles fine — the
    // newtypes in `alertthread-core` are why it cannot happen, and this pins the mapping
    // those newtypes are enforcing.
    let server = MockServer::start().await;
    let mut expected_value = body_json();
    merge(
        &mut expected_value,
        &json!({ "channel": "#alerts", "ts": "1784642520.000100" }),
    );
    let expected = serde_json::to_string(&expected_value).expect("fixture serialises");

    Mock::given(method("POST"))
        .and(path("/chat.update"))
        .and(body_json_string(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = channel();
    let ts = MessageTs::new("1784642520.000100");
    let body = body();
    client(&server)
        .update_message(&UpdateMessage::new(&channel, &ts, &body))
        .await
        .expect("the update succeeds");
}

#[tokio::test]
async fn one_call_makes_exactly_one_request_even_when_it_is_rate_limited() {
    // The client does not retry internally, on 429 or on anything else. Scheduling
    // belongs to the outbox: a sleep here would hold the lease past its expiry, let a
    // second worker reclaim the row, and post the message twice (ADR 001 D2).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .expect(1)
        .mount(&server)
        .await;

    let channel = channel();
    let body = body();
    client(&server)
        .post_message(&PostMessage::to_channel(&channel, &body))
        .await
        .expect_err("429 is an error");

    assert_eq!(
        server.received_requests().await.map(|r| r.len()),
        Some(1),
        "the client must not retry internally"
    );
}
