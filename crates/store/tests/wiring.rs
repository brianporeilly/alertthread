//! Opening a store the way the binary will.
//!
//! The conformance suite is handed a pool by `#[sqlx::test]`, so it never exercises the
//! path a deployed process actually takes: a backend name and a URL out of configuration,
//! turned into a [`Store`]. That path is short, and it is also the first thing that runs on
//! every start — a mistake in it is a container that will not come up.

#![allow(
    clippy::expect_used,
    reason = "test code. clippy.toml turns `expect` back on inside `#[test]` functions, \
              but these helpers are called from outside one, where clippy cannot see the \
              context — so the same policy is stated here."
)]

use alertthread_store::{Backend, StateStore, Store};

/// Names a database file under the directory cargo gives integration tests.
///
/// One file per test, because these run in parallel and a shared file would make them
/// depend on each other's migrations.
#[cfg(feature = "sqlite")]
fn sqlite_url(name: &str) -> String {
    format!("sqlite://{}/{name}.sqlite", env!("CARGO_TARGET_TMPDIR"))
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_sqlite_store_opens_from_a_configured_url() {
    let store = Store::connect(Backend::Sqlite, &sqlite_url("opens-from-url"))
        .await
        .expect("opening a SQLite store at a file URL");

    assert_eq!(store.backend(), Backend::Sqlite);
    store.migrate().await.expect("applying migrations/sqlite");

    // `create_if_missing` is part of the backend's required settings, not an option a
    // caller has to remember: a relay that refused to start because its state file did not
    // exist yet would be a relay that never starts the first time.
    assert!(
        store
            .describe_table("alert_message")
            .await
            .expect("describing a table")
            .len()
            > 1
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_sqlite_store_can_wrap_a_pool_somebody_else_configured() {
    use alertthread_store::SqliteStore;

    let opened = SqliteStore::connect(&sqlite_url("wraps-a-pool"))
        .await
        .expect("opening a SQLite store");
    let wrapped = SqliteStore::from_pool(opened.pool().clone());

    wrapped.migrate().await.expect("applying migrations/sqlite");
    assert!(!wrapped.pool().is_closed());
}

#[cfg(feature = "postgres")]
fn postgres_url() -> String {
    std::env::var("DATABASE_URL").expect("`just test-pg` exports DATABASE_URL")
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_postgres_store_opens_from_a_configured_url() {
    let store = Store::connect(Backend::Postgres, &postgres_url())
        .await
        .expect("opening a PostgreSQL store at the configured URL");

    assert_eq!(store.backend(), Backend::Postgres);
    // Deliberately no `migrate()`: this points at the shared development database, and the
    // conformance suite already proves the migrations apply. What is under test here is
    // that a URL out of configuration produces a usable store.
    assert!(
        store
            .describe_table("information_schema_is_not_a_table_of_ours")
            .await
            .expect("describing a missing table is not an error")
            .is_empty()
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_postgres_store_can_wrap_a_pool_somebody_else_configured() {
    use alertthread_store::PostgresStore;

    let opened = PostgresStore::connect(&postgres_url())
        .await
        .expect("opening a PostgreSQL store");
    let wrapped = PostgresStore::from_pool(opened.pool().clone());

    assert!(!wrapped.pool().is_closed());
}

/// A backend this binary was not built with fails at startup, by name.
///
/// Only meaningful when exactly one backend is compiled in, which is how both gated builds
/// run. An operator who sets `STATE_BACKEND=postgres` against an image built without it
/// gets this at boot rather than a connection error against a URL scheme that never had a
/// driver behind it.
#[cfg(not(all(feature = "sqlite", feature = "postgres")))]
#[tokio::test]
async fn naming_a_backend_this_build_does_not_have_fails_by_name() {
    #[cfg(feature = "sqlite")]
    let (missing, name) = (Backend::Postgres, "postgres");
    #[cfg(feature = "postgres")]
    let (missing, name) = (Backend::Sqlite, "sqlite");

    let error = Store::connect(missing, "does-not-matter")
        .await
        .expect_err("a backend that was not compiled in cannot be opened");
    let rendered = error.to_string();

    assert!(rendered.contains(name), "{rendered}");
    assert!(rendered.contains("cargo feature"), "{rendered}");
}
