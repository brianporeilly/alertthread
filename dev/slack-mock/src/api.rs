//! The Slack Web API face: the three methods `alertthread-slack` calls, plus
//! `/api/state` for the end-to-end assertions.
//!
//! Every answer is HTTP 200 with `ok` in the body, including the failures. That
//! is not a shortcut — it is the single most important thing about Slack's API
//! and the reason `crates/slack/src/client.rs` is written the way it is. A mock
//! that signalled failure with a status code would let a client that ignores
//! `ok` pass.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::messages::{PostRequest, UpdateRequest, Workspace};

/// The bot the fake workspace reports from `auth.test`.
pub(crate) const BOT_USER: &str = "alertthread";

/// Dispatches one API call and returns the body to serialise.
///
/// Split out from the axum handlers so the request shapes can be tested against
/// the JSON `crates/slack` actually sends, without a socket.
pub(crate) fn dispatch(
    method: &str,
    body: &[u8],
    workspace: &mut Workspace,
    now: DateTime<Utc>,
) -> Value {
    match method {
        "chat.postMessage" => match parse::<PostRequest>(body) {
            Err(error) => error,
            Ok(request) => match workspace.post(&request, now) {
                Ok(posted) => json!({
                    "ok": true,
                    "channel": posted.channel,
                    "ts": posted.ts,
                    "message": { "text": request.text },
                }),
                Err(code) => failure(code),
            },
        },
        "chat.update" => match parse::<UpdateRequest>(body) {
            Err(error) => error,
            Ok(request) => match workspace.update(&request, now) {
                Ok(posted) => json!({
                    "ok": true,
                    "channel": posted.channel,
                    "ts": posted.ts,
                    "text": request.text,
                }),
                Err(code) => failure(code),
            },
        },
        "auth.test" => json!({
            "ok": true,
            "url": "http://slack-mock:8081/",
            "team": "alertthread-dev",
            "team_id": "T0MOCK0001",
            "user": BOT_USER,
            "user_id": "U0MOCK0001",
            "bot_id": "B0MOCK0001",
        }),
        _ => failure("unknown_method"),
    }
}

/// An `ok: false` body, in Slack's shape.
pub(crate) fn failure(code: &str) -> Value {
    json!({ "ok": false, "error": code })
}

/// Parses a request body, or produces the `ok: false` Slack answers with.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Value> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!(%error, "a request body did not parse");
        failure("invalid_arguments")
    })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::dispatch;
    use crate::messages::Workspace;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_784_642_520, 0).unwrap()
    }

    /// The exact body `PostMessage::to_channel` serialises to.
    ///
    /// Copied from `crates/slack/tests/slack_api.rs`, which pins it against the
    /// real renderer. If that fixture changes, this must too.
    fn wire_post(thread_ts: Option<&str>) -> Vec<u8> {
        let mut body = json!({
            "channel": "#alerts",
            "reply_broadcast": false,
            "text": "fixture body",
            "attachments": [{
                "color": "#d40e0d",
                "fallback": "fixture body",
                "blocks": [{ "type": "section", "text": { "type": "mrkdwn", "text": "fixture body" } }],
            }],
        });
        if let Some(parent) = thread_ts {
            body["thread_ts"] = json!(parent);
        }
        serde_json::to_vec(&body).unwrap()
    }

    #[test]
    fn a_post_is_answered_with_the_envelope_the_client_reads() {
        let mut workspace = Workspace::default();
        let answer = dispatch("chat.postMessage", &wire_post(None), &mut workspace, now());

        assert_eq!(answer["ok"], json!(true));
        assert!(answer["channel"].as_str().is_some_and(|id| id.len() == 11));
        // `seconds.microseconds`, which is what the relay stores and re-addresses.
        assert!(
            answer["ts"]
                .as_str()
                .is_some_and(|ts| ts.contains('.') && ts.len() > 8)
        );
    }

    #[test]
    fn an_update_rewrites_the_message_the_post_created() {
        let mut workspace = Workspace::default();
        let posted = dispatch("chat.postMessage", &wire_post(None), &mut workspace, now());
        let update = serde_json::to_vec(&json!({
            "channel": posted["channel"],
            "ts": posted["ts"],
            "text": "resolved",
            "attachments": [{ "color": "#2eb886", "fallback": "resolved", "blocks": [] }],
        }))
        .unwrap();

        let answer = dispatch("chat.update", &update, &mut workspace, now());
        assert_eq!(answer["ok"], json!(true));

        let view = workspace.view();
        assert_eq!(view.channels[0].messages[0].color, "#2eb886");
        assert!(view.channels[0].messages[0].edited);
    }

    #[test]
    fn auth_test_reports_a_user_id_because_the_client_refuses_to_start_without_one() {
        let mut workspace = Workspace::default();
        let answer = dispatch("auth.test", b"{}", &mut workspace, now());
        assert_eq!(answer["ok"], json!(true));
        assert!(answer["user_id"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn a_failure_is_an_http_200_with_ok_false() {
        let mut workspace = Workspace::default();
        let answer = dispatch("chat.frobnicate", b"{}", &mut workspace, now());
        assert_eq!(answer, json!({ "ok": false, "error": "unknown_method" }));
    }

    #[test]
    fn a_body_that_is_not_json_is_an_api_error_and_not_a_panic() {
        let mut workspace = Workspace::default();
        for method in ["chat.postMessage", "chat.update"] {
            let answer = dispatch(method, b"not json", &mut workspace, now());
            assert_eq!(answer, json!({ "ok": false, "error": "invalid_arguments" }));
        }
    }

    #[test]
    fn a_threaded_post_is_accepted_with_the_parent_the_relay_names() {
        let mut workspace = Workspace::default();
        let parent = dispatch("chat.postMessage", &wire_post(None), &mut workspace, now());
        let ts = parent["ts"].as_str().unwrap().to_owned();

        let answer = dispatch(
            "chat.postMessage",
            &wire_post(Some(&ts)),
            &mut workspace,
            now(),
        );
        assert_eq!(answer["ok"], json!(true));

        let view = workspace.view();
        assert_eq!(view.channels[0].messages.len(), 1);
        assert_eq!(view.channels[0].messages[0].replies.len(), 1);
    }
}
