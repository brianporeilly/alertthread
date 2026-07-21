//! The Slack Web API client: three methods, hand-rolled on `reqwest`.
//!
//! ADR 001 D1 explains why there is no SDK here. `slack-morphism` is a large dependency
//! surface for `chat.postMessage`, `chat.update` and `auth.test`, and the thing this
//! relay needs most from a Slack client — exact control over 429 and `Retry-After` — is
//! the thing an SDK is most likely to handle for you, in a way the outbox cannot see.
//!
//! # `ok: false` is the whole job
//!
//! **Slack answers HTTP 200 with `{"ok": false, "error": "channel_not_found"}`.** Success
//! at this API is a field in the body, not a status code. A client written the obvious
//! way — `response.error_for_status()?`, then parse — treats every application-level
//! failure as a success, and in this system that means the worker records a post that
//! never happened, marks the alert `posted`, and nobody is ever told. That is the exact
//! failure AGENTS.md calls the worst possible bug.
//!
//! So [`SlackClient::call`] never trusts the status line alone. Its order is:
//!
//! 1. transport failure → [`SlackError::Transport`];
//! 2. HTTP 429 → [`SlackError::RateLimited`], read from the `Retry-After` header;
//! 3. any other non-2xx → [`SlackError::HttpStatus`], because that is a proxy or a wrong
//!    `base_url` talking, not Slack's application layer;
//! 4. body is not the documented JSON envelope → [`SlackError::MalformedResponse`];
//! 5. **`ok: false` → the typed error for that code**, including a *second* rate-limit
//!    path, because Slack also sends `{"ok": false, "error": "ratelimited"}` with a 200;
//! 6. `ok: true` but a field we need is missing → [`SlackError::IncompleteResponse`].
//!
//! Only after all six does a call return `Ok`.
//!
//! # 429 is surfaced, not slept on
//!
//! This client performs exactly one HTTP round trip per call and **never retries or
//! sleeps internally**, including on 429. The delay comes back as
//! [`Disposition::RateLimited`](crate::Disposition::RateLimited) for the outbox to
//! schedule. Three reasons, in increasing order of how badly the alternative fails:
//!
//! - ADR 001 D2 specifies the behaviour as *"set `next_attempt_at = now + Retry-After`,
//!   release the lease"* — that is queue scheduling, not a sleep.
//! - **Backpressure has to be visible.** `alertthread_outbox_oldest_age_seconds` is D11's
//!   primary SLO signal, the one metric that means "alerts are not reaching Slack". A
//!   worker that absorbs a rate limit by sleeping holds its op out of that measurement,
//!   so the queue looks healthy while nothing moves.
//! - **A sleep longer than the lease duplicates the message.** Leases are 60 seconds and
//!   Slack's `Retry-After` is routinely longer during a storm. Sleeping through lease
//!   expiry lets a second worker reclaim the same row and post it, while the first worker
//!   wakes up and posts it too. Releasing the lease is the only safe way to wait, and
//!   only the store can release a lease.

use std::time::Duration;

use alertthread_core::{ChannelId, MessageTs, ThreadTs};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};

use crate::error::{SlackError, SlackMethod};
use crate::message::MessageBody;
use crate::token::SlackToken;

/// Slack's production API root.
pub const DEFAULT_BASE_URL: &str = "https://slack.com/api/";

/// The `Content-Type` every request carries.
const JSON_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/json; charset=utf-8");

/// How long a single Slack call may take before it is a [`SlackError::Transport`].
///
/// Generous by HTTP standards and deliberately so: a slow Slack is not an emergency here,
/// because the outbox is already absorbing the latency. Timing out early would convert a
/// slow success into a retry, and a retry of `chat.postMessage` is a duplicate message.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// What `Retry-After` is assumed to be when Slack omits it or sends something unparseable.
///
/// One second, matching the documented Special Tier limit of one `chat.postMessage` per
/// second per channel — the limit this relay is overwhelmingly most likely to be hitting.
pub const RETRY_AFTER_DEFAULT: Duration = Duration::from_secs(1);

/// The floor applied to `Retry-After`.
///
/// `Retry-After: 0` would put the op straight back on the ready queue and produce a hot
/// loop against an API that has just asked us to stop.
pub const RETRY_AFTER_MIN: Duration = Duration::from_secs(1);

/// The ceiling applied to `Retry-After`.
///
/// Nothing Slack legitimately sends comes near this. The clamp exists so that a hostile
/// or misconfigured proxy cannot park an alert for hours with one header; if the wait was
/// genuinely needed, the next attempt gets another 429 and waits again, which costs one
/// request and cannot lose the alert.
pub const RETRY_AFTER_MAX: Duration = Duration::from_mins(15);

/// A Slack Web API client.
///
/// Cheap to clone — `reqwest::Client` is an `Arc` internally — and safe to share across
/// every outbox worker.
#[derive(Clone, Debug)]
pub struct SlackClient {
    http: reqwest::Client,
    /// The three endpoint URLs, resolved once at construction.
    ///
    /// Resolved here rather than per call so that a `base_url` which parses but cannot
    /// have a path joined onto it — `mailto:`, `data:`, anything "cannot-be-a-base" — is
    /// rejected at startup instead of on the first alert. A relay that starts cleanly and
    /// then cannot post is the failure mode this project is least willing to accept.
    endpoints: Endpoints,
}

#[derive(Clone, Debug)]
struct Endpoints {
    post_message: reqwest::Url,
    update_message: reqwest::Url,
    auth_test: reqwest::Url,
}

impl Endpoints {
    fn url(&self, method: SlackMethod) -> &reqwest::Url {
        match method {
            SlackMethod::PostMessage => &self.post_message,
            SlackMethod::UpdateMessage => &self.update_message,
            SlackMethod::AuthTest => &self.auth_test,
        }
    }
}

/// Builds a [`SlackClient`].
#[derive(Debug)]
pub struct SlackClientBuilder {
    token: SlackToken,
    base_url: String,
    timeout: Duration,
}

impl SlackClientBuilder {
    /// Points the client at something other than Slack.
    ///
    /// Used by the `wiremock` suite and by `dev/slack-mock`. A trailing slash is added if
    /// it is missing, because `Url::join` on a base without one discards the last path
    /// segment — a footgun that would silently send every call to the wrong URL.
    #[must_use]
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Overrides [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builds the client.
    ///
    /// # Errors
    ///
    /// [`SlackError::MalformedToken`] if the token cannot go in a header — a trailing
    /// newline on a mounted secret is the usual cause; [`SlackError::InvalidBaseUrl`] if
    /// the base URL will not parse; [`SlackError::Build`] if `reqwest` cannot construct a
    /// client at all.
    pub fn build(self) -> Result<SlackClient, SlackError> {
        let normalised = if self.base_url.ends_with('/') {
            self.base_url.clone()
        } else {
            format!("{}/", self.base_url)
        };
        let invalid = |detail: String| SlackError::InvalidBaseUrl {
            url: self.base_url.clone(),
            detail,
        };
        let base_url =
            reqwest::Url::parse(&normalised).map_err(|error| invalid(error.to_string()))?;
        let endpoint = |method: SlackMethod| {
            base_url
                .join(method.as_str())
                .map_err(|error| invalid(error.to_string()))
        };
        let endpoints = Endpoints {
            post_message: endpoint(SlackMethod::PostMessage)?,
            update_message: endpoint(SlackMethod::UpdateMessage)?,
            auth_test: endpoint(SlackMethod::AuthTest)?,
        };

        // Built by hand rather than with `RequestBuilder::bearer_auth` so that a token
        // carrying a stray newline produces `MalformedToken` — which names the problem —
        // instead of a `reqwest` error about header values, which sends the reader
        // looking somewhere there is nothing wrong.
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.token.expose()))
            .map_err(|_| SlackError::MalformedToken)?;
        // Keeps the token out of `reqwest`'s own debug output as well as ours.
        authorization.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);

        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(concat!("alertthread/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .build()
            .map_err(|error| SlackError::Build {
                detail: error.to_string(),
            })?;

        Ok(SlackClient { http, endpoints })
    }
}

impl SlackClient {
    /// A client for Slack itself.
    ///
    /// # Errors
    ///
    /// As [`SlackClientBuilder::build`].
    pub fn new(token: SlackToken) -> Result<Self, SlackError> {
        Self::builder(token).build()
    }

    /// Starts building a client.
    #[must_use]
    pub fn builder(token: SlackToken) -> SlackClientBuilder {
        SlackClientBuilder {
            token,
            base_url: DEFAULT_BASE_URL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// `chat.postMessage`.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]. Consult [`SlackError::disposition`] rather than matching on
    /// the variant: that is where ADR 001 D9's table is encoded.
    pub async fn post_message(
        &self,
        request: &PostMessage<'_>,
    ) -> Result<PostedMessage, SlackError> {
        let method = SlackMethod::PostMessage;
        let response: PostResponse = self.call(method, &request.wire()).await?;

        let ts = response.ts.ok_or(SlackError::IncompleteResponse {
            method,
            field: "ts",
        })?;

        Ok(PostedMessage {
            // Slack echoes the canonical channel ID, which is more useful than the
            // `#name` we sent — but a mock or a future API version might not, so the
            // request's own channel is the fallback rather than a hard failure.
            channel: response
                .channel
                .map_or_else(|| request.channel.clone(), ChannelId::new),
            ts: MessageTs::new(ts),
        })
    }

    /// `chat.update`.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]. In particular [`SlackError::MessageNotFound`], whose
    /// disposition is [`Disposition::MessageGone`](crate::Disposition::MessageGone) —
    /// ADR 001 D7's liveness probe firing, for an alert message or a group summary alike
    /// (ADR 002 §1.3).
    pub async fn update_message(&self, request: &UpdateMessage<'_>) -> Result<(), SlackError> {
        let _: UpdateResponse = self
            .call(SlackMethod::UpdateMessage, &request.wire())
            .await?;
        Ok(())
    }

    /// `auth.test`.
    ///
    /// ADR 001 D11 calls this at startup to fail fast on a bad token and to log the
    /// resolved bot identity, and again from `/readyz`.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]; [`SlackError::InvalidAuth`] is the one that means the token is
    /// wrong rather than Slack being unreachable.
    pub async fn auth_test(&self) -> Result<AuthIdentity, SlackError> {
        let method = SlackMethod::AuthTest;
        let response: AuthResponse = self.call(method, &serde_json::json!({})).await?;

        Ok(AuthIdentity {
            team: response.team.unwrap_or_default(),
            team_id: response.team_id.unwrap_or_default(),
            user: response.user.unwrap_or_default(),
            user_id: response.user_id.ok_or(SlackError::IncompleteResponse {
                method,
                field: "user_id",
            })?,
            bot_id: response.bot_id.unwrap_or_default(),
        })
    }

    /// One round trip, with the six-step check described in the module documentation.
    async fn call<R>(&self, method: SlackMethod, payload: &impl Serialize) -> Result<R, SlackError>
    where
        R: serde::de::DeserializeOwned + AsEnvelope,
    {
        let response = self
            .http
            .post(self.endpoints.url(method).clone())
            // Set before `.json()`, which only supplies a content type when there is not
            // one already. Slack's documentation asks for the charset explicitly, and
            // alert annotations routinely carry non-ASCII.
            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
            .json(payload)
            .send()
            .await
            .map_err(|source| SlackError::Transport { method, source })?;

        let status = response.status();
        let retry_after = retry_after_of(response.headers());

        // (2) Rate limits are read from the status line, before the body: Slack's 429
        // does not always carry a JSON envelope, and treating an empty body as malformed
        // would classify a rate limit as a retry — which burns an attempt, which is what
        // D2 says must not happen.
        if status.as_u16() == 429 {
            return Err(SlackError::RateLimited {
                method,
                retry_after,
            });
        }

        let body = response
            .text()
            .await
            .map_err(|source| SlackError::Transport { method, source })?;

        // (3) Anything else non-2xx is a proxy, a load balancer or a wrong base URL.
        if !status.is_success() {
            return Err(SlackError::HttpStatus {
                method,
                status: status.as_u16(),
                body,
            });
        }

        // (4) 200, but is it Slack?
        let decoded: R =
            serde_json::from_str(&body).map_err(|error| SlackError::MalformedResponse {
                method,
                detail: error.to_string(),
            })?;

        // (5) The step everything else in this module exists to protect.
        let envelope = decoded.envelope();
        if !envelope.ok {
            let code = envelope.error.as_deref().unwrap_or("");
            return Err(SlackError::from_api_code(method, code, retry_after));
        }

        Ok(decoded)
    }
}

/// Reads `Retry-After`, clamped to [`RETRY_AFTER_MIN`]..=[`RETRY_AFTER_MAX`].
///
/// Only the delta-seconds form is understood. The HTTP-date form is legal and Slack does
/// not send it; parsing it would need a date parser reachable from an untrusted header
/// for no benefit, and falling back to [`RETRY_AFTER_DEFAULT`] is safe in a way that
/// guessing is not.
fn retry_after_of(headers: &HeaderMap) -> Duration {
    let parsed = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(RETRY_AFTER_DEFAULT, Duration::from_secs);

    parsed.clamp(RETRY_AFTER_MIN, RETRY_AFTER_MAX)
}

/// The `ok`/`error` envelope every Slack Web API response carries.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Envelope {
    /// **The field that decides whether the call worked.** Defaulted to `false`: a body
    /// with no `ok` at all is not a success, and defaulting the other way would make a
    /// truncated or proxied response look like a delivered alert.
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Implemented by every response type so [`SlackClient::call`] can check `ok` generically.
trait AsEnvelope {
    fn envelope(&self) -> &Envelope;
}

macro_rules! as_envelope {
    ($ty:ty) => {
        impl AsEnvelope for $ty {
            fn envelope(&self) -> &Envelope {
                &self.envelope
            }
        }
    };
}

#[derive(Debug, Deserialize)]
struct PostResponse {
    #[serde(flatten)]
    envelope: Envelope,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}
as_envelope!(PostResponse);

#[derive(Debug, Deserialize)]
struct UpdateResponse {
    #[serde(flatten)]
    envelope: Envelope,
}
as_envelope!(UpdateResponse);

#[derive(Debug, Deserialize)]
struct AuthResponse {
    #[serde(flatten)]
    envelope: Envelope,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
}
as_envelope!(AuthResponse);

/// A `chat.postMessage` call.
///
/// Built through one of the three constructors rather than by hand. Each names the
/// situation it is for, and between them they are the only three ways this relay posts a
/// message — which is what keeps the two timestamp newtypes from having to be converted
/// at a call site (AGENTS.md rule 4).
#[derive(Clone, Debug)]
pub struct PostMessage<'a> {
    /// Where to post. A `#name` or a `C…` ID; ADR 001 D8 keeps whatever the query
    /// parameter said.
    pub channel: &'a ChannelId,
    /// What to post.
    pub body: &'a MessageBody,
    /// The message to thread under, if this is a reply.
    pub thread_ts: Option<ThreadTs>,
    /// Whether a threaded reply is also echoed into the channel.
    ///
    /// `false` everywhere in this relay. D6 is explicit: the reply exists to generate an
    /// unread indicator on the parent, and broadcasting it would put the channel noise
    /// back that threading removed.
    pub reply_broadcast: bool,
}

impl<'a> PostMessage<'a> {
    /// A top-level message.
    ///
    /// A newly claimed alert, a storm-collapse parent, or an orphan resolve — ADR 002 §1.4
    /// puts orphan resolves at top level deliberately, because burying a resolution inside
    /// a firing summary hides the message most likely to be what a reader needs.
    #[must_use]
    pub const fn to_channel(channel: &'a ChannelId, body: &'a MessageBody) -> Self {
        Self {
            channel,
            body,
            thread_ts: None,
            reply_broadcast: false,
        }
    }

    /// A child alert, threaded under its storm-collapse parent (ADR 001 D5).
    #[must_use]
    pub fn in_thread(channel: &'a ChannelId, body: &'a MessageBody, parent: &ThreadTs) -> Self {
        Self {
            channel,
            body,
            thread_ts: Some(parent.clone()),
            reply_broadcast: false,
        }
    }

    /// The resolve reply, threaded under the alert's *own* message (ADR 001 D6).
    ///
    /// This is the one place a [`MessageTs`] legitimately becomes a [`ThreadTs`]: an
    /// alert's message is the parent of its own resolve reply. Named, so the crossing is a
    /// decision somebody took here rather than a conversion available everywhere — which
    /// is the whole reason the two are separate types.
    ///
    /// `chat.update` does not notify, bump, or mark a channel unread, so the in-place edit
    /// alone is invisible to anybody watching live. This reply is what generates the
    /// unread indicator, and `reply_broadcast` stays `false` so it costs no channel noise.
    #[must_use]
    pub fn in_reply_to(channel: &'a ChannelId, body: &'a MessageBody, message: &MessageTs) -> Self {
        Self {
            channel,
            body,
            thread_ts: Some(ThreadTs::new(message.as_str())),
            reply_broadcast: false,
        }
    }

    fn wire(&self) -> WirePost<'_> {
        WirePost {
            channel: self.channel.as_str(),
            thread_ts: self.thread_ts.as_ref().map(ThreadTs::as_str),
            reply_broadcast: self.reply_broadcast,
            body: self.body,
        }
    }
}

/// A `chat.update` call.
#[derive(Clone, Debug)]
pub struct UpdateMessage<'a> {
    /// The channel the message lives in.
    pub channel: &'a ChannelId,
    /// The message to rewrite.
    pub ts: MessageTs,
    /// Its replacement content.
    pub body: &'a MessageBody,
}

impl<'a> UpdateMessage<'a> {
    /// An in-place edit of an alert's own message (ADR 001 D6, D7).
    #[must_use]
    pub fn new(channel: &'a ChannelId, ts: &MessageTs, body: &'a MessageBody) -> Self {
        Self {
            channel,
            ts: ts.clone(),
            body,
        }
    }

    /// An in-place edit of a storm-collapse parent (ADR 001 D5, ADR 002 §1.3).
    ///
    /// The counterpart to [`PostMessage::in_reply_to`], and the other place the two
    /// timestamp types legitimately cross: a group parent is addressed by `chat.update`
    /// exactly like any other message, which is precisely the symmetry ADR 002 §1.3 says
    /// was missing.
    #[must_use]
    pub fn group(channel: &'a ChannelId, parent: &ThreadTs, body: &'a MessageBody) -> Self {
        Self {
            channel,
            ts: MessageTs::new(parent.as_str()),
            body,
        }
    }

    fn wire(&self) -> WireUpdate<'_> {
        WireUpdate {
            channel: self.channel.as_str(),
            ts: self.ts.as_str(),
            body: self.body,
        }
    }
}

#[derive(Serialize)]
struct WirePost<'a> {
    channel: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
    reply_broadcast: bool,
    #[serde(flatten)]
    body: &'a MessageBody,
}

#[derive(Serialize)]
struct WireUpdate<'a> {
    channel: &'a str,
    ts: &'a str,
    #[serde(flatten)]
    body: &'a MessageBody,
}

/// What Slack said about a message it accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostedMessage {
    /// The canonical channel ID Slack resolved.
    pub channel: ChannelId,
    /// The timestamp that makes update-on-resolve possible at all.
    pub ts: MessageTs,
}

impl PostedMessage {
    /// This message's timestamp, viewed as a thread parent.
    ///
    /// The one legitimate crossing between the two timestamp newtypes: a storm-collapse
    /// parent has just been posted, and its own `ts` is what its children will thread
    /// under (ADR 001 D5).
    #[must_use]
    pub fn thread_ts(&self) -> ThreadTs {
        ThreadTs::new(self.ts.as_str())
    }
}

/// The bot identity `auth.test` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthIdentity {
    /// The workspace name.
    pub team: String,
    /// The workspace ID.
    pub team_id: String,
    /// The bot's display name.
    pub user: String,
    /// The bot's user ID.
    pub user_id: String,
    /// The bot ID, when Slack reports one.
    pub bot_id: String,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alertthread_core::{ChannelId, MessageTs, ThreadTs};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::{
        DEFAULT_BASE_URL, DEFAULT_TIMEOUT, PostMessage, PostedMessage, RETRY_AFTER_DEFAULT,
        RETRY_AFTER_MAX, RETRY_AFTER_MIN, SlackClient, UpdateMessage, retry_after_of,
    };
    use crate::error::SlackError;
    use crate::message::{Colour, MessageBody};
    use crate::token::SlackToken;

    fn body() -> MessageBody {
        MessageBody::new(Colour::Firing, "FIRING".to_owned(), Vec::new())
    }

    fn headers(retry_after: Option<&str>) -> HeaderMap {
        let mut map = HeaderMap::new();
        if let Some(value) = retry_after {
            map.insert(
                RETRY_AFTER,
                HeaderValue::from_str(value).expect("test header is valid"),
            );
        }
        map
    }

    #[test]
    fn a_retry_after_header_is_honoured() {
        assert_eq!(
            retry_after_of(&headers(Some("30"))),
            Duration::from_secs(30)
        );
        assert_eq!(
            retry_after_of(&headers(Some(" 7 "))),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn a_missing_or_unparseable_retry_after_falls_back_to_the_documented_tier() {
        // Guessing longer would delay an alert; guessing shorter would hammer an API that
        // just asked us to stop. One second is the Special Tier limit this relay is
        // overwhelmingly most likely to be hitting.
        for value in [None, Some("soon"), Some("Wed, 21 Oct 2026 07:28:00 GMT")] {
            assert_eq!(
                retry_after_of(&headers(value)),
                RETRY_AFTER_DEFAULT,
                "{value:?}"
            );
        }
    }

    #[test]
    fn retry_after_is_clamped_at_both_ends() {
        // `Retry-After: 0` would put the op straight back on the ready queue and hot-loop
        // against an API that has just asked us to stop.
        assert_eq!(retry_after_of(&headers(Some("0"))), RETRY_AFTER_MIN);
        assert_eq!(retry_after_of(&headers(Some("999999999"))), RETRY_AFTER_MAX);
        assert_eq!(RETRY_AFTER_MIN, Duration::from_secs(1));
        assert_eq!(RETRY_AFTER_MAX, Duration::from_mins(15));
    }

    #[test]
    fn a_client_can_be_built_for_slack_itself() {
        let client = SlackClient::new(SlackToken::new("xoxb-test")).expect("client builds");
        assert!(format!("{client:?}").contains("slack.com"));
        assert_eq!(DEFAULT_BASE_URL, "https://slack.com/api/");
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn a_base_url_without_a_trailing_slash_still_addresses_the_right_path() {
        // `Url::join` discards the last path segment of a base with no trailing slash, so
        // `http://mock/api` would send every call to `http://mock/chat.postMessage`.
        // Silently wrong, and only visible as 404s from a mock nobody suspects.
        let client = SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url("http://localhost:1/api")
            .build()
            .expect("client builds");
        assert!(format!("{client:?}").contains("/api/"), "{client:?}");
    }

    #[test]
    fn a_token_that_cannot_go_in_a_header_is_named_as_such() {
        // The classic: `kubectl create secret --from-file` keeps the trailing newline.
        let error = SlackClient::new(SlackToken::new("xoxb-test\n")).expect_err("must fail");
        assert!(matches!(error, SlackError::MalformedToken), "{error:?}");
    }

    #[test]
    fn a_base_url_that_is_not_a_url_is_rejected_with_the_value_that_was_configured() {
        let error = SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url("not a url")
            .build()
            .expect_err("must fail");
        assert!(
            matches!(error, SlackError::InvalidBaseUrl { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("not a url"), "{error}");
    }

    #[test]
    fn a_base_url_that_cannot_carry_a_path_is_rejected_at_construction() {
        // `mailto:` parses as a URL and is "cannot-be-a-base", so joining `chat.update`
        // onto it fails. Catching it here rather than on the first call is the point: a
        // relay that starts cleanly and then cannot post is worse than one that refuses
        // to start.
        let error = SlackClient::builder(SlackToken::new("xoxb-test"))
            .base_url("mailto:relay@example.com")
            .build()
            .expect_err("must fail");
        assert!(
            matches!(error, SlackError::InvalidBaseUrl { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_timeout_can_be_overridden() {
        let client = SlackClient::builder(SlackToken::new("xoxb-test"))
            .timeout(Duration::from_millis(50))
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn the_builder_never_shows_the_token() {
        let builder = SlackClient::builder(SlackToken::new("xoxb-secret"));
        assert!(!format!("{builder:?}").contains("xoxb-secret"));
    }

    #[test]
    fn a_top_level_post_serialises_without_a_thread_ts() {
        // `thread_ts: null` is not the same as an absent field to Slack: it rejects the
        // former. A message that will not post is a silent alert.
        let channel = ChannelId::new("#alerts");
        let body = body();
        let json = serde_json::to_value(PostMessage::to_channel(&channel, &body).wire())
            .expect("request serialises");
        assert_eq!(
            json.get("channel").and_then(|v| v.as_str()),
            Some("#alerts")
        );
        assert!(json.get("thread_ts").is_none(), "{json}");
        assert_eq!(
            json.get("reply_broadcast")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(json.get("attachments").is_some(), "{json}");
        assert!(json.get("text").is_some(), "{json}");
    }

    #[test]
    fn a_threaded_post_carries_its_parent_and_never_broadcasts() {
        // ADR 001 D6: broadcasting would put back the channel noise that threading
        // removed.
        let channel = ChannelId::new("#alerts");
        let body = body();
        let parent = ThreadTs::new("1721570520.000100");
        let request = PostMessage::in_thread(&channel, &body, &parent);
        assert!(!request.reply_broadcast);

        let json = serde_json::to_value(request.wire()).expect("request serialises");
        assert_eq!(
            json.get("thread_ts").and_then(|v| v.as_str()),
            Some("1721570520.000100")
        );
        assert_eq!(
            json.get("reply_broadcast")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn an_update_addresses_a_channel_and_a_timestamp() {
        let channel = ChannelId::new("#alerts");
        let ts = MessageTs::new("1721570520.000200");
        let body = body();
        let json = serde_json::to_value(UpdateMessage::new(&channel, &ts, &body).wire())
            .expect("request serialises");
        assert_eq!(
            json.get("channel").and_then(|v| v.as_str()),
            Some("#alerts")
        );
        assert_eq!(
            json.get("ts").and_then(|v| v.as_str()),
            Some("1721570520.000200")
        );
        assert!(json.get("attachments").is_some(), "{json}");
    }

    #[test]
    fn a_group_summary_update_crosses_the_two_timestamp_types_in_exactly_one_place() {
        // ADR 002 §1.3 needs the group parent updated the same way an alert message is.
        // The conversion is a named constructor so that the crossing is a decision
        // somebody took, not something a `From` impl made available everywhere.
        let channel = ChannelId::new("#alerts");
        let parent = ThreadTs::new("1721570520.000300");
        let body = body();
        let request = UpdateMessage::group(&channel, &parent, &body);
        assert_eq!(request.ts.as_str(), "1721570520.000300");
        assert_eq!(request.channel, &channel);
    }

    #[test]
    fn a_resolve_reply_threads_under_the_alerts_own_message() {
        // ADR 001 D6's other half. The parent of a resolve reply is the alert's own
        // message, which is the one legitimate MessageTs -> ThreadTs crossing — and
        // without a named constructor for it, every caller would do the conversion inline.
        let channel = ChannelId::new("#alerts");
        let body = body();
        let own = MessageTs::new("1721570520.000100");
        let request = PostMessage::in_reply_to(&channel, &body, &own);

        assert_eq!(request.thread_ts, Some(ThreadTs::new("1721570520.000100")));
        assert!(!request.reply_broadcast);

        let json = serde_json::to_value(request.wire()).expect("request serialises");
        assert_eq!(
            json.get("thread_ts").and_then(|v| v.as_str()),
            Some("1721570520.000100")
        );
    }

    #[test]
    fn a_posted_message_can_be_read_as_a_thread_parent() {
        let posted = PostedMessage {
            channel: ChannelId::new("C0123"),
            ts: MessageTs::new("1721570520.000400"),
        };
        assert_eq!(posted.thread_ts(), ThreadTs::new("1721570520.000400"));
        assert!(format!("{posted:?}").contains("C0123"));
    }
}
