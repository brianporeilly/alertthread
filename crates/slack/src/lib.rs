//! Slack Web API client and message rendering for `alertthread`.
//!
//! This crate calls exactly three Slack methods — `chat.postMessage`, `chat.update` and
//! `auth.test` — hand-rolled on `reqwest` rather than through an SDK (ADR 001 D1). That
//! is roughly 200 lines and buys exact control over 429 and `Retry-After` handling, which
//! the outbox in D2 depends on.
//!
//! Rendering produces Block Kit blocks wrapped in a legacy attachment for the colour bar
//! (D10), through a MiniJinja template users may override. Every render is wrapped in the
//! D9 fallback: a broken user template degrades to a hardcoded plain message and never to
//! silence.
//!
//! # The two things to know before using this
//!
//! **Slack answers HTTP 200 with `{"ok": false, "error": "…"}`.** Success is a field in
//! the body, not a status code. [`SlackClient`] never trusts the status line alone; see
//! its module documentation for the full order of checks. Nothing downstream of this
//! crate should have to think about it, and nothing downstream should have to parse an
//! error string either — that is what [`SlackError::disposition`] is for.
//!
//! **[`Renderer::render`] cannot fail.** There is no `Result`, because D9 does not permit
//! one: a template error degrades to a hardcoded message and is reported through
//! [`Rendered::degraded`], never by declining to produce a message.
//!
//! # Shape
//!
//! ```text
//!   AlertView / GroupView ──▶ Renderer::render ──▶ MessageBody ──▶ SlackClient
//!                                    │                                  │
//!                          Rendered { degraded, truncated }      Result<_, SlackError>
//!                                    │                                  │
//!                          fallback + truncation metrics       Disposition ──▶ outbox
//! ```
//!
//! The renderer decides *what to say*, the client decides *what happened*, and neither
//! decides what to do about it. That belongs to Phase 4's worker, which is why the two
//! things it needs — [`Disposition`] and [`Rendered::degraded`] — are the parts of this
//! API designed hardest.

mod error;
mod message;
mod render;
mod token;

pub use error::{Disposition, SlackError, SlackMethod};
pub use message::{
    Attachment, Block, Colour, MAX_BLOCKS, MAX_NOTIFICATION_CHARS, MAX_SECTION_CHARS, MessageBody,
    Text,
};
pub use render::{
    AlertView, Degradation, FallbackReason, GroupView, RejectedOverride, RenderRequest, Rendered,
    Renderer, TemplateKind, Truncation,
};
pub use token::SlackToken;

/// The version of this crate, as recorded in its `Cargo.toml`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_the_compiled_in_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn core_is_linked_and_reports_the_same_workspace_version() {
        assert_eq!(version(), alertthread_core::version());
    }
}
