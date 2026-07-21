//! Pure decision logic for `alertthread`.
//!
//! This crate holds every correctness decision in the project and performs no
//! I/O of any kind. It has no async runtime, no database driver and no HTTP
//! client, which is what lets its logic be tested exhaustively with plain
//! function calls and no mocks.
//!
//! The shape this crate grows into is specified in ADR 001 and built in Phase 1
//! of the roadmap: newtyped identifiers, the Alertmanager payload types, and
//! the `plan()` function that decides which operations an incoming batch
//! should produce.
//!
//! Phase 0 is scaffolding only — there is deliberately no relay behaviour here
//! yet.

/// The version of this crate, as recorded in its `Cargo.toml`.
///
/// Exposed so the binary can report the version of the core it was built
/// against, which matters when a deployed image and a source tree disagree.
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
