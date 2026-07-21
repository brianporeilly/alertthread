//! Slack Web API client and message rendering for `alertthread`.
//!
//! This crate calls exactly three Slack methods — `chat.postMessage`,
//! `chat.update` and `auth.test` — hand-rolled on `reqwest` rather than through
//! an SDK (ADR 001 D1). That is roughly 200 lines and buys exact control over
//! 429 and `Retry-After` handling, which the outbox in D2 depends on.
//!
//! Rendering produces Block Kit blocks wrapped in a legacy attachment for the
//! colour bar (D10), through a MiniJinja template users may override. Every
//! render is wrapped in the D9 fallback: a broken user template degrades to a
//! hardcoded plain message and never to silence.
//!
//! Phase 0 is scaffolding only — the client and templates arrive in Phase 3.

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
