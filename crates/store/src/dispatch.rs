//! Runtime backend selection, without `dyn`.
//!
//! # Why an enum and not `Arc<dyn StateStore>`
//!
//! ADR 001 D4 selects the backend at startup from a config value, so the choice genuinely
//! is a runtime one. That does not make dynamic dispatch the answer here, and three things
//! say so:
//!
//! - **There are exactly two implementations and both are known at compile time.** The set
//!   is closed. An enum expresses a closed set; a trait object expresses an open one, and
//!   paying for openness that will never be used is how an abstraction ends up describing
//!   nothing in particular.
//! - **`StateStore` is not `dyn`-compatible and should not be made so.** Its methods return
//!   `impl Future + Send`, which is what gives the Phase 4 worker a spawnable future. Making
//!   the trait object-safe means `async_trait`, which means a `Box<dyn Future>` allocation
//!   on every claim, lease and completion — on the ingest path whose budget is a 50 ms p99.
//! - **AGENTS.md names `Arc<dyn Trait>` as a design smell in this codebase**, and this is
//!   exactly the site it had in mind.
//!
//! The trait still earns its place: it is what lets the conformance suite be written once,
//! generically, and run against both backends. Generic over the trait for testing; an enum
//! for dispatch. Neither job needs `dyn`.

use alertthread_core::{
    AlertBatch, ChannelId, ClaimOutcome, Fingerprint, GroupKey, GroupState, Plan,
};
use chrono::{DateTime, TimeDelta, Utc};

use crate::error::StoreError;
use crate::model::{
    AlertRecord, ColumnDef, Deferral, GroupMembership, GroupRecord, LeasedOp, OpEffect, OpId,
    PruneStats, RetentionPolicy, StoreStats, WorkerId,
};
use crate::store::StateStore;

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!(
    "alertthread-store needs at least one backend feature: `sqlite` (the default) or \
     `postgres`. A store with no backend cannot hold correlation state, and a relay that \
     cannot hold correlation state cannot correlate a resolution to the message it belongs \
     to — which is the entire product."
);

/// Which state store to use, as named by `storage.backend` / `STATE_BACKEND`.
///
/// Parsing is separate from construction so a typo in configuration is rejected at startup
/// with the name it saw, rather than as a connection failure against a URL scheme nobody
/// meant to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Backend {
    /// SQLite. The default, and exactly one replica (ADR 001 D4).
    Sqlite,
    /// PostgreSQL. Opt-in, and what makes N replicas legal.
    Postgres,
}

impl Backend {
    /// The configuration value that selects this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }

    /// Reads a `storage.backend` value.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnknownBackend`] for anything else, carrying the value it was given.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "sqlite" => Ok(Self::Sqlite),
            "postgres" => Ok(Self::Postgres),
            other => Err(StoreError::UnknownBackend(other.to_owned())),
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The state store this process is running against.
///
/// Implements [`StateStore`] by forwarding to whichever backend was selected. Every method
/// is a `match`; there is no vtable and no boxed future.
#[derive(Clone, Debug)]
pub enum Store {
    /// The SQLite backend.
    #[cfg(feature = "sqlite")]
    Sqlite(crate::sqlite::SqliteStore),
    /// The PostgreSQL backend.
    #[cfg(feature = "postgres")]
    Postgres(crate::postgres::PostgresStore),
}

impl Store {
    /// Opens the configured backend.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnknownBackend`] if this binary was built without the feature for the
    /// requested backend — which is a real deployment mistake and deserves to say so at
    /// startup rather than at the first alert. [`StoreError::Database`] if the store cannot
    /// be reached.
    pub async fn connect(backend: Backend, url: &str) -> Result<Self, StoreError> {
        match backend {
            #[cfg(feature = "sqlite")]
            Backend::Sqlite => Ok(Self::Sqlite(
                crate::sqlite::SqliteStore::connect(url).await?,
            )),
            #[cfg(feature = "postgres")]
            Backend::Postgres => Ok(Self::Postgres(
                crate::postgres::PostgresStore::connect(url).await?,
            )),
            #[cfg(not(all(feature = "sqlite", feature = "postgres")))]
            other => Err(StoreError::UnknownBackend(other.as_str().to_owned())),
        }
    }

    /// Which backend this is.
    ///
    /// Reported at startup and carried as a metric label, because "why is correlation state
    /// empty after a restart?" is answered differently for a PVC and for a database.
    pub const fn backend(&self) -> Backend {
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => Backend::Sqlite,
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => Backend::Postgres,
        }
    }
}

/// Forwards one method to whichever backend is selected.
///
/// A macro rather than nine hand-written matches: the arms are mechanical, and a
/// hand-written set is where one method quietly ends up calling the wrong thing.
macro_rules! forward {
    ($self:expr, $store:ident => $call:expr) => {
        match $self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite($store) => $call.await,
            #[cfg(feature = "postgres")]
            Self::Postgres($store) => $call.await,
        }
    };
}

impl StateStore for Store {
    async fn migrate(&self) -> Result<(), StoreError> {
        forward!(self, s => s.migrate())
    }

    async fn ingest<F>(
        &self,
        batch: &AlertBatch,
        now: DateTime<Utc>,
        decide: F,
    ) -> Result<Plan, StoreError>
    where
        F: FnOnce(&[ClaimOutcome], Option<&GroupState>) -> Plan + Send,
    {
        forward!(self, s => s.ingest(batch, now, decide))
    }

    async fn lease_batch(
        &self,
        worker: &WorkerId,
        limit: u32,
        lease: TimeDelta,
        now: DateTime<Utc>,
    ) -> Result<Vec<LeasedOp>, StoreError> {
        forward!(self, s => s.lease_batch(worker, limit, lease, now))
    }

    async fn complete(
        &self,
        id: OpId,
        effect: &OpEffect,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        forward!(self, s => s.complete(id, effect, now))
    }

    async fn defer(&self, id: OpId, deferral: &Deferral) -> Result<(), StoreError> {
        forward!(self, s => s.defer(id, deferral))
    }

    async fn dead_letter(
        &self,
        id: OpId,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        forward!(self, s => s.dead_letter(id, reason, now))
    }

    async fn prune(
        &self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
    ) -> Result<PruneStats, StoreError> {
        forward!(self, s => s.prune(policy, now))
    }

    async fn alert(
        &self,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
    ) -> Result<Option<AlertRecord>, StoreError> {
        forward!(self, s => s.alert(fingerprint, channel))
    }

    async fn group(
        &self,
        group_key: &GroupKey,
        channel: &ChannelId,
    ) -> Result<Option<GroupRecord>, StoreError> {
        forward!(self, s => s.group(group_key, channel))
    }

    async fn group_membership(
        &self,
        group_key: &GroupKey,
        channel: &ChannelId,
    ) -> Result<GroupMembership, StoreError> {
        forward!(self, s => s.group_membership(group_key, channel))
    }

    async fn stats(&self) -> Result<StoreStats, StoreError> {
        forward!(self, s => s.stats())
    }

    async fn describe_table(&self, table: &str) -> Result<Vec<ColumnDef>, StoreError> {
        forward!(self, s => s.describe_table(table))
    }
}

#[cfg(test)]
mod tests {
    use super::Backend;
    use crate::StoreError;

    #[test]
    fn the_two_configuration_values_parse_to_their_backends() {
        assert_eq!(
            Backend::parse("sqlite").expect("\"sqlite\" is a backend"),
            Backend::Sqlite
        );
        assert_eq!(
            Backend::parse("postgres").expect("\"postgres\" is a backend"),
            Backend::Postgres
        );
    }

    #[test]
    fn a_backend_round_trips_through_its_configuration_value() {
        for backend in [Backend::Sqlite, Backend::Postgres] {
            assert_eq!(
                Backend::parse(backend.as_str()).expect("a backend's own name parses"),
                backend
            );
            assert_eq!(backend.to_string(), backend.as_str());
        }
    }

    #[test]
    fn an_unknown_backend_is_rejected_and_says_what_it_saw() {
        // This message reaches an operator whose container has failed to start. "unknown
        // storage backend" without the value is a message that sends them to the source.
        let error = Backend::parse("postgresql").expect_err("only two values are backends");
        assert!(matches!(error, StoreError::UnknownBackend(ref got) if got == "postgresql"));
        let rendered = error.to_string();
        assert!(rendered.contains("postgresql"), "{rendered}");
        assert!(rendered.contains("sqlite"), "{rendered}");
    }

    #[test]
    fn backend_names_are_case_sensitive() {
        // Configuration is matched exactly rather than normalised: quietly accepting
        // "SQLite" would mean two spellings that have to keep agreeing for ever.
        assert!(Backend::parse("SQLite").is_err());
        assert!(Backend::parse(" sqlite").is_err());
    }

    #[test]
    fn backend_debug_names_the_variant() {
        assert_eq!(format!("{:?}", Backend::Postgres), "Postgres");
    }
}
