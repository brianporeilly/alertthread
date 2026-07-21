//! Persistence for `alertthread`.
//!
//! This crate owns the `StateStore` trait (ADR 001 D4) and its two backends:
//! SQLite by default, PostgreSQL opt-in for horizontal scaling. Both are
//! exercised by a single shared conformance suite, so the HA path is
//! continuously verified rather than theoretical.
//!
//! The atomic claim on `(fingerprint, channel)` lives here rather than in the
//! core because its correctness *is* the database's atomicity — it cannot be
//! made pure. The shell runs claims first and feeds their outcomes into
//! [`alertthread_core`] for the actual decision.
//!
//! Phase 0 is scaffolding only — the trait and backends arrive in Phase 2.

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
        // The workspace versions every crate together; a mismatch here means a
        // crate was published or bumped out of step with the rest.
        assert_eq!(version(), alertthread_core::version());
    }
}
