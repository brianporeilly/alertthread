//! Optional bearer-token authentication on `POST /webhook` (ADR 001 D11, "Security").
//!
//! # Only the webhook, and only when a token is configured
//!
//! `/healthz`, `/readyz` and `/metrics` are **never** authenticated. A kubelet probe carries
//! no credentials, so a `401` on `/readyz` is a pod that never joins the service, and a
//! `401` on `/metrics` is a relay whose own alerts stop working — both of which are silence
//! arriving from the machinery that exists to prevent it. None of the three reveals the
//! contents of an alert.
//!
//! With no `server.auth_token` set, [`crate::http::router`] does not install the middleware
//! at all rather than installing it in a permissive mode. "Off" is then the absence of code
//! rather than a branch inside it.
//!
//! # What a refusal looks like
//!
//! `401`, with a bare `WWW-Authenticate: Bearer` header and the body `unauthorized`. Every
//! refusal is byte-for-byte identical: a caller cannot learn from the response whether it
//! sent no credential, the wrong credential, or one that is close. The operator's copy of
//! that information is in the log line and in the `outcome` label on
//! `alertthread_webhook_requests_total`, where the two cases *are* separated because the fix
//! differs.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::metrics::Metrics;

/// The bearer token `POST /webhook` requires when one is configured.
///
/// A newtype for the same reason [`alertthread_slack::SlackToken`] is one: its
/// [`std::fmt::Debug`] prints `<redacted>`, so every struct that embeds one inherits the
/// property instead of having to remember it. There is no accessor for the value — the only
/// thing anybody needs to do with it is [`Self::matches`], which is here.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct WebhookToken(String);

impl WebhookToken {
    /// Wraps a configured token.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Whether the configured value is empty or whitespace.
    ///
    /// A chart that renders an unset value produces `""`, which is not a token.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// Whether `presented` is this token.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        secrets_match(presented.as_bytes(), self.0.as_bytes())
    }
}

impl std::fmt::Debug for WebhookToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookToken(<redacted>)")
    }
}

impl From<&str> for WebhookToken {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Compares two secrets in time that does not depend on where they first differ.
///
/// The accumulate-then-compare shape is load-bearing: `==`, `iter().all()` or an early
/// `return` would all short-circuit on the first differing byte and let a caller recover the
/// token one byte at a time. The loop runs over the *configured* length in every case, so
/// the work done is a property of the configuration and not of the request.
fn secrets_match(presented: &[u8], configured: &[u8]) -> bool {
    let mut diff = u8::from(presented.len() != configured.len());
    for (index, expected) in configured.iter().enumerate() {
        diff |= presented.get(index).copied().unwrap_or(0) ^ expected;
    }
    diff == 0
}

/// What `server.auth_token` resolves to.
#[derive(Clone, Debug)]
pub enum WebhookAuth {
    /// Nothing configured: `POST /webhook` accepts unauthenticated requests.
    Open,
    /// A token was configured and is empty. Treated as [`Self::Open`], loudly — see
    /// [`crate::run::start`], which warns at startup rather than leaving an operator to
    /// believe a setting is in effect.
    Blank,
    /// Every `POST /webhook` must present this token.
    Required(WebhookToken),
}

impl WebhookAuth {
    /// The token to enforce, if there is one.
    #[must_use]
    pub const fn token(&self) -> Option<&WebhookToken> {
        match self {
            Self::Open | Self::Blank => None,
            Self::Required(token) => Some(token),
        }
    }

    /// Whether the webhook accepts unauthenticated requests.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.token().is_none()
    }
}

/// Why a delivery was refused.
///
/// Three variants and two metric labels: the log line separates "the header was not a bearer
/// credential" from "there was no header", because a proxy that strips `Authorization` and a
/// receiver with no `authorization:` block are different mistakes, while an operator alerting
/// on the counter only needs to know which side of the credential the problem is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// No `Authorization` header at all.
    Absent,
    /// An `Authorization` header that is not a `Bearer` credential.
    NotBearer,
    /// A `Bearer` credential that is not the configured token.
    Mismatch,
}

impl Denial {
    /// The `outcome` label for `alertthread_webhook_requests_total`.
    #[must_use]
    pub const fn outcome(self) -> &'static str {
        match self {
            Self::Absent | Self::NotBearer => "auth_missing",
            Self::Mismatch => "auth_mismatch",
        }
    }

    /// What the log line says happened.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Absent => "no Authorization header",
            Self::NotBearer => "the Authorization header is not a Bearer credential",
            Self::Mismatch => "the Bearer credential is not server.auth_token",
        }
    }
}

/// Checks one `Authorization` header value against the configured token.
///
/// # Errors
///
/// [`Denial`], which the caller turns into an identical `401` whichever variant it is.
pub fn authorize(header: Option<&str>, expected: &WebhookToken) -> Result<(), Denial> {
    let Some(value) = header else {
        return Err(Denial::Absent);
    };
    let Some(presented) = bearer(value) else {
        return Err(Denial::NotBearer);
    };
    if expected.matches(presented) {
        Ok(())
    } else {
        Err(Denial::Mismatch)
    }
}

/// The credential out of a `Bearer <credential>` header value.
///
/// The scheme is compared case-insensitively because RFC 7235 says it is case-insensitive,
/// and a relay that accepted only the one capitalisation Alertmanager happens to send would
/// reject every other correctly-configured sender.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, credential) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim())
}

/// Everything the webhook's authentication layer holds.
///
/// Deliberately not the whole [`crate::http::AppState`]: this layer has no business reaching
/// the store, and a guard that cannot see it cannot grow a database call on the perimeter.
#[derive(Clone, Debug)]
pub struct Guard {
    token: WebhookToken,
    metrics: Arc<Metrics>,
}

impl Guard {
    /// Builds a guard enforcing `token`.
    #[must_use]
    pub fn new(token: WebhookToken, metrics: Arc<Metrics>) -> Self {
        Self { token, metrics }
    }
}

/// Refuses a `POST /webhook` that does not carry the configured bearer token.
pub async fn require_bearer(State(guard): State<Guard>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        // A header that is not valid UTF-8 cannot be the configured token, and treating it
        // as absent keeps the byte string out of everything downstream.
        .and_then(|value| value.to_str().ok());

    match authorize(presented, &guard.token) {
        Ok(()) => next.run(request).await,
        Err(denial) => {
            guard.metrics.webhook(denial.outcome());
            tracing::error!(
                reason = denial.detail(),
                "refused a webhook delivery: it did not carry server.auth_token. Nothing was \
                 persisted, and Alertmanager does not retry a 401 — any alerts in that \
                 delivery are lost"
            );
            unauthorized()
        }
    }
}

/// The one response every refusal gets, whatever the [`Denial`] was.
///
/// A bare `Bearer` challenge rather than RFC 6750's `error="invalid_token"` parameter: the
/// parameter's whole purpose is to tell the client which mistake it made, and that is exactly
/// what this endpoint declines to do.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{Denial, WebhookAuth, WebhookToken, authorize, bearer, secrets_match};

    fn token() -> WebhookToken {
        WebhookToken::new("s3cret-webhook-token")
    }

    #[test]
    fn debug_never_shows_the_token() {
        #[derive(Debug)]
        struct Embedded {
            #[expect(dead_code, reason = "read only by the derived Debug this test checks")]
            token: WebhookToken,
        }

        // The whole reason this is a newtype. A token in a log line is a burned token.
        let rendered = format!("{:?}", token());
        assert_eq!(rendered, "WebhookToken(<redacted>)");
        assert!(!rendered.contains("s3cret"), "{rendered}");

        let embedded = format!("{:?}", Embedded { token: token() });
        assert!(!embedded.contains("s3cret"), "{embedded}");
    }

    #[test]
    fn the_configured_token_is_the_only_one_that_matches() {
        assert!(token().matches("s3cret-webhook-token"));
        assert!(!token().matches("s3cret-webhook-toke"));
        assert!(!token().matches("s3cret-webhook-tokenn"));
        assert!(!token().matches("S3cret-webhook-token"));
        assert!(!token().matches(""));
    }

    #[test]
    fn the_comparison_looks_at_every_byte_of_the_configured_token() {
        // A short-circuiting comparison leaks the token one byte at a time. This asserts the
        // outcomes rather than the timing — timing is not assertable in a unit test — but the
        // cases it covers are the ones a `==` would answer early on.
        assert!(secrets_match(b"abc", b"abc"));
        assert!(!secrets_match(b"abd", b"abc"));
        assert!(!secrets_match(b"xbc", b"abc"));
        assert!(!secrets_match(b"ab", b"abc"));
        assert!(!secrets_match(b"abcd", b"abc"));
        assert!(!secrets_match(b"", b"abc"));
        // A zero byte where the presented value has run out must not read as a match: the
        // padding is arithmetic, not a value.
        assert!(!secrets_match(b"ab", b"ab\0"));
        assert!(secrets_match(b"", b""));
    }

    #[test]
    fn a_blank_configured_token_is_not_a_token() {
        // A chart that renders an unset value produces exactly this.
        for blank in ["", " ", "\n", "\t "] {
            assert!(WebhookToken::new(blank).is_blank(), "{blank:?}");
        }
        assert!(!token().is_blank());
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively_and_the_credential_is_trimmed() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        assert_eq!(bearer("BEARER  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("abc"), None);
        assert_eq!(bearer(""), None);
    }

    #[test]
    fn every_way_of_getting_it_wrong_has_its_own_reason_and_one_of_two_labels() {
        assert_eq!(authorize(None, &token()), Err(Denial::Absent));
        assert_eq!(
            authorize(Some("Basic dXNlcjpwYXNz"), &token()),
            Err(Denial::NotBearer)
        );
        assert_eq!(
            authorize(Some("Bearer wrong"), &token()),
            Err(Denial::Mismatch)
        );
        assert_eq!(
            authorize(Some("Bearer s3cret-webhook-token"), &token()),
            Ok(())
        );

        assert_eq!(Denial::Absent.outcome(), "auth_missing");
        assert_eq!(Denial::NotBearer.outcome(), "auth_missing");
        assert_eq!(Denial::Mismatch.outcome(), "auth_mismatch");

        // The two `auth_missing` variants share a metric label, so the log line is the *only*
        // place they are distinguishable — and telling "the receiver sends no credential" from
        // "something stripped the header" apart is the whole reason both exist. Each detail
        // has to name its own case, and no two may read the same.
        assert!(Denial::Absent.detail().contains("no Authorization header"));
        assert!(Denial::NotBearer.detail().contains("not a Bearer"));
        assert!(Denial::Mismatch.detail().contains("server.auth_token"));
        let details = [
            Denial::Absent.detail(),
            Denial::NotBearer.detail(),
            Denial::Mismatch.detail(),
        ];
        for (index, detail) in details.iter().enumerate() {
            assert!(
                !details[index + 1..].contains(detail),
                "two denials read identically in the log: {detail}"
            );
        }
    }

    #[test]
    fn only_a_configured_non_blank_token_closes_the_webhook() {
        assert!(WebhookAuth::Open.is_open());
        assert!(WebhookAuth::Open.token().is_none());
        assert!(WebhookAuth::Blank.is_open());
        assert!(WebhookAuth::Blank.token().is_none());

        let required = WebhookAuth::Required(token());
        assert!(!required.is_open());
        assert_eq!(required.token(), Some(&token()));
        // And the enum itself must not print the token either.
        assert!(!format!("{required:?}").contains("s3cret"));
    }

    #[test]
    fn a_token_can_be_built_from_a_str() {
        assert_eq!(WebhookToken::from("abc"), WebhookToken::new("abc"));
    }
}
