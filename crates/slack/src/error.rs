//! What can go wrong when calling Slack, and — the part that matters — what the outbox
//! should do about it.
//!
//! # The shape of this module is ADR 001 D9
//!
//! D9's failure table is a specification, and Phase 4's worker is the thing that executes
//! it. The worker has exactly four moves available to it, and every one of them is a
//! different [`StateStore`] call:
//!
//! | Move | Store call | Attempt counted? |
//! |---|---|---|
//! | Come back when Slack says so | `defer(Deferral::RateLimited)` | **No** |
//! | Come back with backoff | `defer(Deferral::Backoff)` | Yes |
//! | Stop, loudly | `dead_letter` | — |
//! | The message is gone; post a new one | `complete(OpEffect::MessageLost)` | — |
//!
//! [`Disposition`] is those four moves, and [`SlackError::disposition`] is total over the
//! error type. That is deliberate: the alternative is Phase 4 matching on error *strings*
//! to decide whether an alert gets retried or dead-lettered, and a typo in that match is
//! an alert that never posts. The decision is taken here, once, next to the code that
//! knows what Slack said.
//!
//! [`StateStore`]: https://docs.rs/alertthread-store

use std::time::Duration;

use thiserror::Error;

/// Which of the three Slack Web API methods this relay calls.
///
/// Doubles as the `method` label on `alertthread_slack_calls_total` and
/// `alertthread_slack_call_duration_seconds` (ADR 001 D11), which is why the string form
/// is the API's own spelling rather than a prettier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SlackMethod {
    /// `chat.postMessage` — a new message, or a threaded reply.
    PostMessage,
    /// `chat.update` — an in-place edit (ADR 001 D6, D7).
    UpdateMessage,
    /// `auth.test` — the startup and readiness check (ADR 001 D11).
    AuthTest,
}

impl SlackMethod {
    /// The Web API method name, as it appears in the URL and in the metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PostMessage => "chat.postMessage",
            Self::UpdateMessage => "chat.update",
            Self::AuthTest => "auth.test",
        }
    }
}

impl std::fmt::Display for SlackMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the outbox worker should do about a failed Slack call.
///
/// Four variants because the worker has four moves, and every [`SlackError`] maps onto
/// exactly one of them. Nothing here requires reading an error message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Slack asked us to come back later, and told us when.
    ///
    /// ADR 001 D2 and D9 are explicit that this is **not** a failed attempt: the op is
    /// deferred to `now + retry_after` with its attempt given back. Counting rate limits
    /// would march an alert toward the dead-letter queue for the crime of arriving during
    /// a storm — which is exactly when the alert matters most.
    RateLimited {
        /// How long Slack asked us to wait, already clamped to something sane.
        retry_after: Duration,
    },
    /// Transient. Defer with exponential backoff; this *does* consume an attempt, and
    /// exhausting `max_attempts` dead-letters (ADR 001 D9).
    Retry,
    /// Never going to succeed. Dead-letter now rather than burning attempts on it — D9
    /// says exactly this for `invalid_auth`, and the reasoning generalises to every other
    /// error Slack raises about the *request* rather than about its own health.
    Terminal,
    /// The message this call addressed does not exist any more.
    ///
    /// ADR 001 D7 calls this a free liveness probe on our own correlation state, and D9
    /// specifies the response: clear the stored timestamp and post a fresh message.
    /// ADR 002 §1.3 extends it to storm-collapse group summaries, whose natural
    /// implementation — a silent no-op, because a summary is "just" a rollup — orphans
    /// every threaded child under a parent that is no longer there.
    ///
    /// One disposition serves both message kinds on purpose. The asymmetry between them
    /// was the defect ADR 002 records; a single variant is what stops it recurring.
    MessageGone,
}

impl Disposition {
    /// Whether this outcome should consume one of the op's `max_attempts`.
    ///
    /// Only [`Disposition::Retry`] does. Stated as a method because it is the property
    /// most easily got wrong, and the one whose failure mode is a dead-lettered alert.
    pub const fn counts_as_an_attempt(self) -> bool {
        matches!(self, Self::Retry)
    }
}

/// A failed Slack call.
///
/// Every variant records the [`SlackMethod`] it came from, because the metric labels in
/// ADR 001 D11 are per-method and because "which call failed" is the first question asked
/// of a log line at 3am.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SlackError {
    /// Slack rate-limited us: HTTP 429, or `{"ok": false, "error": "ratelimited"}`.
    ///
    /// Slack sends both forms. The 200-with-`ratelimited` shape is the reason this crate
    /// inspects the body of a successful HTTP response at all — see [`crate::SlackClient`].
    #[error("{method}: rate-limited by Slack, retry after {}s", retry_after.as_secs())]
    RateLimited {
        /// The call that was limited.
        method: SlackMethod,
        /// `Retry-After`, clamped to [`crate::RETRY_AFTER_MIN`]..=[`crate::RETRY_AFTER_MAX`].
        retry_after: Duration,
    },

    /// `chat.update` addressed a message that is not there any anymore.
    ///
    /// ADR 001 D7's liveness probe firing. Somebody deleted the message, or the stored
    /// timestamp outlived it.
    #[error("{method}: the message addressed no longer exists ({code})")]
    MessageNotFound {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's own error code, preserved for the log line.
        code: String,
    },

    /// The bot token is not accepted: `invalid_auth`, `not_authed`, `account_inactive`,
    /// `token_revoked`, `token_expired`.
    ///
    /// ADR 001 D9: dead-letter immediately, do not burn retries, fire a metric. A token
    /// does not become valid by being tried ten more times, and the ten tries are ten
    /// alerts' worth of worker capacity spent achieving nothing.
    #[error("{method}: Slack rejected the bot token ({code})")]
    InvalidAuth {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's own error code.
        code: String,
    },

    /// The channel cannot be posted to: `channel_not_found`, `not_in_channel`,
    /// `is_archived`, and the `restricted_action*` family.
    ///
    /// Terminal for the same reason as [`SlackError::InvalidAuth`]: this is a
    /// configuration fact, not a transient one. It is separated from `InvalidAuth`
    /// because the operator fixes it somewhere completely different — an Alertmanager
    /// `?channel=` parameter or a Slack channel invite, not a secret.
    #[error("{method}: Slack cannot deliver to that channel ({code})")]
    ChannelUnusable {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's own error code.
        code: String,
    },

    /// Slack rejected the request we built: `msg_too_long`, `invalid_blocks`, `no_text`,
    /// `too_many_attachments`, `cant_update_message`, `edit_window_closed`, …
    ///
    /// Terminal, and the one variant that usually means a bug *here* rather than a
    /// misconfiguration out there. The dead-letter row's payload is the reproduction.
    #[error("{method}: Slack rejected the request we built ({code})")]
    BadRequest {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's own error code.
        code: String,
    },

    /// Slack reported a problem on its own side: `fatal_error`, `internal_error`,
    /// `service_unavailable`, `request_timeout`.
    #[error("{method}: Slack reported a transient failure ({code})")]
    SlackUnavailable {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's own error code.
        code: String,
    },

    /// `ok: false` with an error code this build has never heard of.
    ///
    /// Classified **retryable**, deliberately. Both classifications end in a dead-letter
    /// — an unrecognised error that never resolves exhausts `max_attempts` and parks —
    /// so the only question is whether a transient-but-unfamiliar failure gets a chance
    /// to succeed first. Retrying costs a delay; dead-lettering immediately costs the
    /// alert. AGENTS.md resolves that trade-off in one direction only.
    #[error("{method}: Slack returned an error this build does not recognise ({code})")]
    Unrecognised {
        /// The call that failed.
        method: SlackMethod,
        /// Slack's error code, verbatim — this is the string to search their docs for.
        code: String,
    },

    /// A non-2xx HTTP status, with no usable Slack envelope in the body.
    ///
    /// A proxy, a load balancer, or a wrong `base_url` — not Slack's application layer.
    /// Retryability is decided from the status code by
    /// [`http_status_is_retryable`](SlackError::disposition).
    #[error("{method}: HTTP {status} from Slack ({})", crate::error::snippet(body))]
    HttpStatus {
        /// The call that failed.
        method: SlackMethod,
        /// The status code returned.
        status: u16,
        /// The first part of the body, for the log line.
        body: String,
    },

    /// The request never got an answer: DNS, TCP, TLS, timeout, or a truncated body.
    #[error("{method}: could not reach Slack: {source}")]
    Transport {
        /// The call that failed.
        method: SlackMethod,
        /// What `reqwest` said.
        #[source]
        source: reqwest::Error,
    },

    /// HTTP 200 with a body that is not the JSON envelope Slack documents.
    ///
    /// Retryable: the overwhelmingly likely cause is something between us and Slack —
    /// a captive portal, a proxy error page, a truncated response — rather than Slack
    /// changing its API mid-flight.
    #[error("{method}: Slack's response was not the JSON envelope this build expects: {detail}")]
    MalformedResponse {
        /// The call that failed.
        method: SlackMethod,
        /// What the decoder objected to.
        detail: String,
    },

    /// `ok: true`, but a field this build needs was not in the body.
    ///
    /// Specifically `chat.postMessage` succeeding without returning a `ts`. Retryable
    /// rather than terminal, but note that retrying will post a **second** message —
    /// this is ADR 001 D3's duplicate-over-silence trade-off, reached by a different
    /// road. A post we cannot record the timestamp of is a message we can never update
    /// or resolve, so it is not a success.
    #[error("{method}: Slack reported success but omitted {field}")]
    IncompleteResponse {
        /// The call that failed.
        method: SlackMethod,
        /// The field that was missing.
        field: &'static str,
    },

    /// The bot token cannot be put in an HTTP header.
    ///
    /// Almost always a trailing newline on a token read from a mounted Kubernetes secret
    /// or a `.env` file. Terminal, and worth its own variant because the error `reqwest`
    /// would otherwise produce says nothing about tokens.
    #[error(
        "the bot token contains a character that cannot appear in an HTTP header — a \
         trailing newline from a mounted secret is the usual cause"
    )]
    MalformedToken,

    /// `slack.base_url` is not a URL.
    #[error("{url:?} is not a usable Slack API base URL: {detail}")]
    InvalidBaseUrl {
        /// What was configured.
        url: String,
        /// Why it was rejected.
        detail: String,
    },

    /// The HTTP client itself could not be constructed.
    ///
    /// TLS backend initialisation, or a malformed proxy in the environment. Fatal at
    /// startup; there is no relay without an HTTP client.
    ///
    /// Carries `reqwest`'s message as text rather than as a `#[source]`. The error is
    /// only ever read by a human at startup, and a `String` is constructible in a test —
    /// a `reqwest::Error` is not, which would leave this arm of
    /// [`SlackError::disposition`] and [`SlackError::outcome`] permanently unexercised.
    #[error("the Slack HTTP client could not be built: {detail}")]
    Build {
        /// What `reqwest` said.
        detail: String,
    },
}

impl SlackError {
    /// Which call produced this, where there was one.
    ///
    /// `None` for the construction-time failures, which happen before any method is
    /// chosen. Phase 4 uses it for the `method` label on `alertthread_slack_calls_total`.
    pub const fn method(&self) -> Option<SlackMethod> {
        match self {
            Self::RateLimited { method, .. }
            | Self::MessageNotFound { method, .. }
            | Self::InvalidAuth { method, .. }
            | Self::ChannelUnusable { method, .. }
            | Self::BadRequest { method, .. }
            | Self::SlackUnavailable { method, .. }
            | Self::Unrecognised { method, .. }
            | Self::HttpStatus { method, .. }
            | Self::Transport { method, .. }
            | Self::MalformedResponse { method, .. }
            | Self::IncompleteResponse { method, .. } => Some(*method),
            Self::MalformedToken | Self::InvalidBaseUrl { .. } | Self::Build { .. } => None,
        }
    }

    /// What the outbox worker should do about this.
    ///
    /// Total over the enum, and the only place the retryable/terminal question is
    /// answered. See [`Disposition`].
    pub const fn disposition(&self) -> Disposition {
        match self {
            Self::RateLimited { retry_after, .. } => Disposition::RateLimited {
                retry_after: *retry_after,
            },

            Self::MessageNotFound { .. } => Disposition::MessageGone,

            // ADR 001 D9, verbatim for the first of these: "dead-letter immediately, do
            // not burn retries, fire a metric". None of these becomes true by waiting.
            Self::InvalidAuth { .. }
            | Self::ChannelUnusable { .. }
            | Self::BadRequest { .. }
            | Self::MalformedToken
            | Self::InvalidBaseUrl { .. }
            | Self::Build { .. } => Disposition::Terminal,

            // D9's "Slack 5xx" row, plus everything else that might be different in
            // thirty seconds.
            Self::SlackUnavailable { .. }
            | Self::Unrecognised { .. }
            | Self::Transport { .. }
            | Self::MalformedResponse { .. }
            | Self::IncompleteResponse { .. } => Disposition::Retry,

            Self::HttpStatus { status, .. } => {
                if http_status_is_retryable(*status) {
                    Disposition::Retry
                } else {
                    Disposition::Terminal
                }
            }
        }
    }

    /// The `outcome` label for `alertthread_slack_calls_total` (ADR 001 D11).
    ///
    /// Deliberately low-cardinality: one value per variant, never the error text. Slack's
    /// error codes are open-ended and putting them in a label is how a Prometheus falls
    /// over.
    pub const fn outcome(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::MessageNotFound { .. } => "message_not_found",
            Self::InvalidAuth { .. } => "invalid_auth",
            Self::ChannelUnusable { .. } => "channel_unusable",
            Self::BadRequest { .. } => "bad_request",
            Self::SlackUnavailable { .. } => "slack_unavailable",
            Self::Unrecognised { .. } => "unrecognised",
            Self::HttpStatus { .. } => "http_status",
            Self::Transport { .. } => "transport",
            Self::MalformedResponse { .. } => "malformed_response",
            Self::IncompleteResponse { .. } => "incomplete_response",
            Self::MalformedToken => "malformed_token",
            Self::InvalidBaseUrl { .. } => "invalid_base_url",
            Self::Build { .. } => "build",
        }
    }

    /// Builds the right variant for one of Slack's `ok: false` error codes.
    ///
    /// This match **is** the D9 table for the application layer, and grouping the codes
    /// by what an operator would do about them is the point: an unfamiliar code lands in
    /// [`SlackError::Unrecognised`] rather than being silently treated as whichever
    /// neighbour it was listed next to.
    ///
    /// `retry_after` is supplied by the caller because Slack's 200-with-`ratelimited`
    /// shape carries its delay in a header, not in the body.
    pub(crate) fn from_api_code(method: SlackMethod, code: &str, retry_after: Duration) -> Self {
        let owned = || code.to_owned();
        match code {
            // Slack spells this without an underscore in the body and with one in some
            // of its documentation. Both are accepted rather than picking a favourite.
            "ratelimited" | "rate_limited" => Self::RateLimited {
                method,
                retry_after,
            },

            "message_not_found" => Self::MessageNotFound {
                method,
                code: owned(),
            },

            "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked"
            | "token_expired" | "no_permission" | "missing_scope" | "ekm_access_denied" => {
                Self::InvalidAuth {
                    method,
                    code: owned(),
                }
            }

            "channel_not_found"
            | "not_in_channel"
            | "is_archived"
            | "restricted_action"
            | "restricted_action_read_only_channel"
            | "restricted_action_thread_only_channel"
            | "restricted_action_non_threadable_channel" => Self::ChannelUnusable {
                method,
                code: owned(),
            },

            "msg_too_long"
            | "no_text"
            | "too_many_attachments"
            | "invalid_blocks"
            | "invalid_blocks_format"
            | "invalid_arguments"
            | "invalid_arg_name"
            | "cant_update_message"
            | "edit_window_closed"
            | "cant_broadcast_message"
            | "as_user_not_supported"
            | "invalid_form_data"
            | "invalid_post_type"
            | "missing_post_type" => Self::BadRequest {
                method,
                code: owned(),
            },

            "fatal_error"
            | "internal_error"
            | "service_unavailable"
            | "request_timeout"
            | "server_error" => Self::SlackUnavailable {
                method,
                code: owned(),
            },

            _ => Self::Unrecognised {
                method,
                code: owned(),
            },
        }
    }
}

/// Whether a transport-level HTTP status is worth trying again.
///
/// 5xx and the two 4xx codes that mean "not now" rather than "not ever". Everything else
/// in the 4xx range describes the request, and the request will be identical next time.
const fn http_status_is_retryable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || status >= 500
}

/// The first 200 characters of a body, on one line, for an error message.
///
/// Bodies at this point are usually a proxy's HTML error page. The whole thing in a log
/// line is noise; the first line of it is usually the entire diagnosis.
fn snippet(body: &str) -> String {
    const LIMIT: usize = 200;
    let flattened: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(LIMIT)
        .collect();
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        "empty body".to_owned()
    } else if body.chars().count() > LIMIT {
        format!("{trimmed}…")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Disposition, SlackError, SlackMethod, http_status_is_retryable, snippet};

    fn rate_limited(seconds: u64) -> SlackError {
        SlackError::RateLimited {
            method: SlackMethod::PostMessage,
            retry_after: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn method_names_are_the_api_spellings() {
        // These reach a metric label and a URL path. A prettier name here would be a
        // metric nobody can correlate with Slack's own documentation.
        assert_eq!(SlackMethod::PostMessage.as_str(), "chat.postMessage");
        assert_eq!(SlackMethod::UpdateMessage.as_str(), "chat.update");
        assert_eq!(SlackMethod::AuthTest.as_str(), "auth.test");
        assert_eq!(SlackMethod::AuthTest.to_string(), "auth.test");
        assert_eq!(format!("{:?}", SlackMethod::PostMessage), "PostMessage");
    }

    #[test]
    fn only_a_plain_retry_consumes_an_attempt() {
        // ADR 001 D2 and D9: a 429 is Slack scheduling us, not the op failing. If this
        // ever returns true for RateLimited, an alert storm dead-letters its own alerts.
        assert!(Disposition::Retry.counts_as_an_attempt());
        assert!(
            !Disposition::RateLimited {
                retry_after: Duration::from_secs(30)
            }
            .counts_as_an_attempt()
        );
        assert!(!Disposition::Terminal.counts_as_an_attempt());
        assert!(!Disposition::MessageGone.counts_as_an_attempt());
    }

    #[test]
    fn a_rate_limit_carries_its_delay_through_to_the_disposition() {
        assert_eq!(
            rate_limited(42).disposition(),
            Disposition::RateLimited {
                retry_after: Duration::from_secs(42)
            }
        );
    }

    #[test]
    fn message_not_found_asks_for_a_fresh_post_rather_than_a_retry() {
        // ADR 001 D9 for an alert message, ADR 002 §1.3 for a group summary — the same
        // disposition, which is the whole point of that amendment.
        let error = SlackError::MessageNotFound {
            method: SlackMethod::UpdateMessage,
            code: "message_not_found".to_owned(),
        };
        assert_eq!(error.disposition(), Disposition::MessageGone);
    }

    #[test]
    fn auth_failures_are_terminal_rather_than_retried() {
        // D9: "dead-letter immediately, do not burn retries, fire a metric."
        for code in [
            "invalid_auth",
            "not_authed",
            "account_inactive",
            "token_revoked",
            "token_expired",
            "missing_scope",
        ] {
            let error =
                SlackError::from_api_code(SlackMethod::PostMessage, code, Duration::from_secs(1));
            assert!(
                matches!(error, SlackError::InvalidAuth { .. }),
                "{code} classified as {error:?}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal, "{code}");
        }
    }

    #[test]
    fn channel_problems_are_terminal_and_distinguishable_from_auth_problems() {
        // Separated because the operator fixes them somewhere entirely different.
        for code in [
            "channel_not_found",
            "not_in_channel",
            "is_archived",
            "restricted_action",
            "restricted_action_read_only_channel",
        ] {
            let error =
                SlackError::from_api_code(SlackMethod::PostMessage, code, Duration::from_secs(1));
            assert!(
                matches!(error, SlackError::ChannelUnusable { .. }),
                "{code} classified as {error:?}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal, "{code}");
        }
    }

    #[test]
    fn requests_slack_rejects_are_terminal() {
        for code in [
            "msg_too_long",
            "invalid_blocks",
            "no_text",
            "too_many_attachments",
            "cant_update_message",
            "edit_window_closed",
            "invalid_arguments",
        ] {
            let error =
                SlackError::from_api_code(SlackMethod::PostMessage, code, Duration::from_secs(1));
            assert!(
                matches!(error, SlackError::BadRequest { .. }),
                "{code} classified as {error:?}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal, "{code}");
        }
    }

    #[test]
    fn slacks_own_failures_are_retryable() {
        for code in [
            "fatal_error",
            "internal_error",
            "service_unavailable",
            "request_timeout",
        ] {
            let error =
                SlackError::from_api_code(SlackMethod::PostMessage, code, Duration::from_secs(1));
            assert!(
                matches!(error, SlackError::SlackUnavailable { .. }),
                "{code} classified as {error:?}"
            );
            assert_eq!(error.disposition(), Disposition::Retry, "{code}");
        }
    }

    #[test]
    fn both_spellings_of_the_rate_limit_code_are_accepted() {
        // Slack's body says `ratelimited`; parts of its documentation say `rate_limited`.
        // Picking one and being wrong means a rate limit counted as a failed attempt.
        for code in ["ratelimited", "rate_limited"] {
            let error =
                SlackError::from_api_code(SlackMethod::PostMessage, code, Duration::from_secs(7));
            assert_eq!(
                error.disposition(),
                Disposition::RateLimited {
                    retry_after: Duration::from_secs(7)
                },
                "{code}"
            );
        }
    }

    #[test]
    fn an_unfamiliar_error_code_is_retried_and_kept_verbatim() {
        // Deliberate direction: both classifications end in a dead-letter, so the only
        // question is whether a transient-but-unfamiliar failure gets a chance first.
        let error = SlackError::from_api_code(
            SlackMethod::PostMessage,
            "some_future_slack_error",
            Duration::from_secs(1),
        );
        assert_eq!(error.disposition(), Disposition::Retry);
        assert!(
            error.to_string().contains("some_future_slack_error"),
            "the code must survive into the log line: {error}"
        );
    }

    #[test]
    fn http_statuses_split_into_wait_and_give_up() {
        for status in [408, 425, 429, 500, 502, 503, 504, 599] {
            assert!(http_status_is_retryable(status), "{status}");
        }
        for status in [400, 401, 403, 404, 405, 410, 418, 451] {
            assert!(!http_status_is_retryable(status), "{status}");
        }
    }

    #[test]
    fn an_http_status_takes_its_disposition_from_the_code() {
        let server = SlackError::HttpStatus {
            method: SlackMethod::PostMessage,
            status: 503,
            body: "upstream unavailable".to_owned(),
        };
        assert_eq!(server.disposition(), Disposition::Retry);

        let client = SlackError::HttpStatus {
            method: SlackMethod::PostMessage,
            status: 404,
            body: "not found".to_owned(),
        };
        assert_eq!(client.disposition(), Disposition::Terminal);
    }

    #[test]
    fn construction_failures_have_no_method_and_are_terminal() {
        for error in [
            SlackError::MalformedToken,
            SlackError::InvalidBaseUrl {
                url: "not a url".to_owned(),
                detail: "relative URL without a base".to_owned(),
            },
            SlackError::Build {
                detail: "TLS backend unavailable".to_owned(),
            },
        ] {
            assert_eq!(error.method(), None, "{error:?}");
            assert_eq!(error.disposition(), Disposition::Terminal, "{error:?}");
        }
    }

    #[test]
    fn every_call_failure_reports_the_method_it_came_from() {
        let errors = [
            rate_limited(1),
            SlackError::MessageNotFound {
                method: SlackMethod::PostMessage,
                code: "message_not_found".to_owned(),
            },
            SlackError::InvalidAuth {
                method: SlackMethod::PostMessage,
                code: "invalid_auth".to_owned(),
            },
            SlackError::ChannelUnusable {
                method: SlackMethod::PostMessage,
                code: "is_archived".to_owned(),
            },
            SlackError::BadRequest {
                method: SlackMethod::PostMessage,
                code: "msg_too_long".to_owned(),
            },
            SlackError::SlackUnavailable {
                method: SlackMethod::PostMessage,
                code: "internal_error".to_owned(),
            },
            SlackError::Unrecognised {
                method: SlackMethod::PostMessage,
                code: "who_knows".to_owned(),
            },
            SlackError::HttpStatus {
                method: SlackMethod::PostMessage,
                status: 500,
                body: String::new(),
            },
            SlackError::MalformedResponse {
                method: SlackMethod::PostMessage,
                detail: "expected value".to_owned(),
            },
            SlackError::IncompleteResponse {
                method: SlackMethod::PostMessage,
                field: "ts",
            },
        ];
        for error in &errors {
            assert_eq!(
                error.method(),
                Some(SlackMethod::PostMessage),
                "{error:?} lost its method"
            );
        }
    }

    #[test]
    fn every_outcome_label_is_distinct_and_low_cardinality() {
        // These become a Prometheus label value. Two variants sharing one would merge two
        // different failures in the one place an operator looks to tell them apart.
        let errors = [
            rate_limited(1),
            SlackError::MessageNotFound {
                method: SlackMethod::UpdateMessage,
                code: "message_not_found".to_owned(),
            },
            SlackError::InvalidAuth {
                method: SlackMethod::AuthTest,
                code: "invalid_auth".to_owned(),
            },
            SlackError::ChannelUnusable {
                method: SlackMethod::PostMessage,
                code: "is_archived".to_owned(),
            },
            SlackError::BadRequest {
                method: SlackMethod::PostMessage,
                code: "msg_too_long".to_owned(),
            },
            SlackError::SlackUnavailable {
                method: SlackMethod::PostMessage,
                code: "internal_error".to_owned(),
            },
            SlackError::Unrecognised {
                method: SlackMethod::PostMessage,
                code: "who_knows".to_owned(),
            },
            SlackError::HttpStatus {
                method: SlackMethod::PostMessage,
                status: 500,
                body: String::new(),
            },
            SlackError::MalformedResponse {
                method: SlackMethod::PostMessage,
                detail: "x".to_owned(),
            },
            SlackError::IncompleteResponse {
                method: SlackMethod::PostMessage,
                field: "ts",
            },
            SlackError::MalformedToken,
            SlackError::InvalidBaseUrl {
                url: String::new(),
                detail: String::new(),
            },
            SlackError::Build {
                detail: "TLS backend unavailable".to_owned(),
            },
        ];
        let mut labels: Vec<&str> = errors.iter().map(SlackError::outcome).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), errors.len(), "two errors share an outcome");
        for label in labels {
            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "{label} is not a usable metric label value"
            );
        }
    }

    #[test]
    fn error_messages_name_the_method_and_the_code() {
        // Every one of these reaches an operator through `last_error` on a stuck outbox
        // row, which is often the only evidence of what happened.
        let error = SlackError::BadRequest {
            method: SlackMethod::UpdateMessage,
            code: "msg_too_long".to_owned(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("chat.update"), "{rendered}");
        assert!(rendered.contains("msg_too_long"), "{rendered}");

        assert!(rate_limited(30).to_string().contains("30"));
    }

    #[test]
    fn a_malformed_token_error_says_what_to_look_for() {
        // The reqwest error this replaces mentions header values and not tokens, which
        // sends the reader looking in the wrong place.
        let rendered = SlackError::MalformedToken.to_string();
        assert!(rendered.contains("newline"), "{rendered}");
        assert!(rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn an_http_error_quotes_the_start_of_the_body_on_one_line() {
        let error = SlackError::HttpStatus {
            method: SlackMethod::PostMessage,
            status: 502,
            body: "<html>\n<head><title>502 Bad Gateway</title></head>\n".to_owned(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("502 Bad Gateway"), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
    }

    #[test]
    fn a_snippet_flattens_trims_and_elides() {
        assert_eq!(snippet("  hello\nworld  "), "hello world");
        assert_eq!(snippet(""), "empty body");
        assert_eq!(snippet("   \n  "), "empty body");

        let long = "a".repeat(500);
        let cut = snippet(&long);
        assert!(cut.ends_with('…'), "{cut}");
        assert_eq!(cut.chars().count(), 201);
    }

    #[test]
    fn a_snippet_exactly_at_the_limit_is_not_elided() {
        let exact = "b".repeat(200);
        assert_eq!(snippet(&exact), exact);
    }
}
