//! The `alertthread` application: the imperative shell.
//!
//! This crate wires the pure core to the outside world. It owns the axum
//! handlers, the outbox worker loop, the per-channel rate limiter, config
//! loading and the Prometheus metrics registry.
//!
//! The division of labour is the point of the workspace layout (ADR 001, and
//! the roadmap's "shape that makes this work"): the shell runs the atomic
//! claims, hands their outcomes to [`alertthread_core`] to decide, then
//! persists whatever was decided in the same transaction. Handlers execute
//! decisions; they do not make them.
//!
//! # Where to start reading
//!
//! | Module | What |
//! |---|---|
//! | [`config`] | figment layering, and the three settings that refuse to start |
//! | [`auth`] | the optional bearer token on `POST /webhook`, and what it never covers |
//! | [`http`] | the four endpoints, including why `/readyz` diverges from ADR 001 D11 |
//! | [`worker`] | the outbox loop: lease, fan out by channel, drain each serially |
//! | [`delivery`] | one op, one Slack call, and ADR 001 D9's failure table |
//! | [`ratelimit`] | the per-channel token bucket Slack's 1/sec limit requires |
//! | [`metrics`] | ADR 001 D11, and why its gauges are sampled rather than queried |
//! | [`run`] | startup wiring, kept out of `main.rs` so it can be tested |

pub mod auth;
pub mod config;
pub mod delivery;
pub mod http;
pub mod metrics;
pub mod ratelimit;
pub mod run;
pub mod shutdown;
pub mod worker;

/// The version of the running binary, as recorded in its `Cargo.toml`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A one-line build identity, logged once at startup and reported by `/healthz`.
///
/// Having the core, store and slack versions in the same string makes a
/// mismatched build obvious in a log line, which is where such things are
/// usually first noticed.
#[must_use]
pub fn build_identity() -> String {
    format!(
        "alertthread {} (core {}, store {}, slack {})",
        version(),
        alertthread_core::version(),
        alertthread_store::version(),
        alertthread_slack::version(),
    )
}

#[cfg(test)]
mod tests {
    use super::{build_identity, version};

    #[test]
    fn version_is_the_compiled_in_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn build_identity_names_every_workspace_crate() {
        let identity = build_identity();
        for crate_name in ["alertthread", "core", "store", "slack"] {
            assert!(
                identity.contains(crate_name),
                "build identity {identity:?} should mention {crate_name}"
            );
        }
    }

    #[test]
    fn build_identity_reports_one_consistent_version() {
        // All four crates are versioned by the workspace, so a build that
        // reports two different versions is a packaging bug.
        let identity = build_identity();
        let occurrences = identity.matches(version()).count();
        assert_eq!(
            occurrences,
            4,
            "expected all four crates at version {} in {identity:?}",
            version()
        );
    }
}
