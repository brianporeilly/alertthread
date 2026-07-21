//! Pure decision logic for `alertthread`.
//!
//! This crate holds every correctness decision in the project and performs no I/O of any
//! kind. It has no async runtime, no database driver and no HTTP client, and it cannot
//! read a clock — `chrono` is depended on without its `clock` feature, so `Utc::now()`
//! does not exist here. Time arrives as a `now:` parameter. That is what lets this logic
//! be tested exhaustively with plain function calls and no mocks, which in turn is why
//! this crate is the one held to 100% line coverage with no surviving mutants.

mod ids;
mod webhook;

pub use ids::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};
pub use webhook::{AlertStatus, Intent, LabelMap, WebhookAlert, WebhookPayload};

/// The version of this crate, as recorded in its `Cargo.toml`.
///
/// Exposed so the binary can report the version of the core it was built against, which
/// matters when a deployed image and a source tree disagree.
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
    fn version_is_not_empty() {
        assert!(!version().is_empty(), "version string must never be empty");
    }
}
