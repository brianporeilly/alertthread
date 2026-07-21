//! Runs the conformance suite against every compiled backend.
//!
//! The suite itself is in `conformance/suite.rs` and knows nothing about SQLite or
//! PostgreSQL. This file is the only place that does: it builds a store, migrates it with
//! the migrations that actually ship for that backend, and hands it to every test in the
//! suite. One description of the behaviour, one proof per backend.
//!
//! # Why the suite runs against [`Store`] rather than the concrete backends
//!
//! [`Store`] is the enum the binary will hold, so running the suite through it means the
//! dispatcher is exercised by every test rather than by a token one — and a forwarding arm
//! wired to the wrong method is exactly the kind of mistake that a token test misses and
//! production does not.
//!
//! # Why `migrations = false`
//!
//! `#[sqlx::test]` will apply migrations itself, but then the schema under test would be
//! whatever this file pointed it at rather than whatever `StateStore::migrate` ships.
//! Disabling it and calling `migrate()` means the suite tests the migration runner and both
//! migration directories, not just the tables they happen to produce today.
//!
//! # The two backends' different needs
//!
//! `#[sqlx::test]` requires `DATABASE_URL` for PostgreSQL and requires nothing for SQLite,
//! where it puts a database per test under `target/sqlx/test-dbs/`. That difference is
//! absorbed here, by the feature gates: `just test` compiles only the SQLite arm and
//! `just test-pg` compiles only the PostgreSQL arm. Neither arm is ever compiled into a
//! build that cannot run it.

// `conformance.rs` is a test-target root, so a bare `mod suite;` would look for
// `tests/suite.rs`. The suite lives in a subdirectory instead, because cargo turns every
// `tests/*.rs` into its own test binary and a second root here would compile the whole
// suite twice for nothing.
#[path = "conformance/suite.rs"]
mod suite;

/// Generates one `#[sqlx::test]` per backend for every named suite function.
///
/// The arms are identical apart from the four backend-specific lines at the top of each.
/// Anything shared between them lives in the suite, where it is written once.
macro_rules! conformance {
    ($($name:ident),+ $(,)?) => {
        #[cfg(feature = "sqlite")]
        mod sqlite {
            use alertthread_store::{SqliteStore, StateStore, Store};

            /// Removes a previous run's write-ahead log.
            ///
            /// `#[sqlx::test]` deletes the database file between runs but knows nothing
            /// about WAL, and a leftover `-wal` alongside a freshly created database is a
            /// migration that fails with "table already exists" on the second run and not
            /// the first. Cheaper to delete than to debug.
            fn clear_write_ahead_log(path: &std::path::Path) {
                for suffix in ["-wal", "-shm"] {
                    let mut sidecar = path.as_os_str().to_owned();
                    sidecar.push(suffix);
                    match std::fs::remove_file(std::path::PathBuf::from(sidecar)) {
                        Ok(()) => {}
                        Err(error) => assert_eq!(
                            error.kind(),
                            std::io::ErrorKind::NotFound,
                            "could not clear a stale SQLite sidecar file"
                        ),
                    }
                }
            }

            $(
                #[sqlx::test(migrations = false)]
                async fn $name(
                    pool: sqlx::pool::PoolOptions<sqlx::Sqlite>,
                    options: sqlx::sqlite::SqliteConnectOptions,
                ) {
                    clear_write_ahead_log(options.get_filename());
                    let store = Store::Sqlite(
                        SqliteStore::connect_with(pool, options)
                            .await
                            .expect("opening the SQLite test database"),
                    );
                    store.migrate().await.expect("applying migrations/sqlite");
                    super::suite::$name(&store).await;
                }
            )+
        }

        #[cfg(feature = "postgres")]
        mod postgres {
            use alertthread_store::{PostgresStore, StateStore, Store};

            $(
                #[sqlx::test(migrations = false)]
                async fn $name(
                    pool: sqlx::pool::PoolOptions<sqlx::Postgres>,
                    options: sqlx::postgres::PgConnectOptions,
                ) {
                    let store = Store::Postgres(
                        PostgresStore::connect_with(pool, options)
                            .await
                            .expect("opening the PostgreSQL test database"),
                    );
                    store.migrate().await.expect("applying migrations/postgres");
                    super::suite::$name(&store).await;
                }
            )+
        }
    };
}

conformance!(
    // Schema — the drift police for ADR 001 D4's two migration directories.
    the_schema_is_the_one_both_migration_directories_are_supposed_to_build,
    a_table_that_does_not_exist_describes_as_nothing,
    migrating_an_already_migrated_store_is_a_no_op,
    // ADR 001 D2 — ingest classification.
    a_new_firing_alert_is_claimed_and_its_post_is_queued,
    the_alert_labels_and_annotations_survive_the_round_trip,
    timestamps_round_trip_at_microsecond_precision,
    redelivering_a_batch_does_not_post_it_twice,
    two_different_batches_sharing_a_fingerprint_post_it_once,
    the_same_fingerprint_in_two_channels_is_two_independent_alerts,
    // ADR 001 D7 — the repeat-firing debounce.
    a_repeat_after_the_debounce_queues_an_in_place_refresh,
    a_repeat_inside_the_debounce_is_a_duplicate_delivery,
    a_repeat_arriving_before_the_post_landed_queues_nothing_new,
    // ADR 001 D6 and D9 — resolve, orphans, and the states D2 left unspecified.
    resolving_a_posted_alert_targets_the_message_it_posted,
    resolving_before_the_post_landed_defers_rather_than_dropping,
    a_duplicate_resolution_is_recognised_as_one,
    a_resolution_for_an_untracked_fingerprint_still_posts_something,
    an_alert_that_fires_again_after_resolving_is_posted_again,
    a_resolution_after_a_dead_lettered_post_is_an_orphan_not_a_duplicate,
    an_alert_that_fires_again_after_dead_lettering_is_posted_again,
    an_empty_delivery_writes_nothing,
    // ADR 001 D3 — the concurrency table, run as real concurrency.
    n_tasks_racing_one_fingerprint_produce_exactly_one_post,
    n_tasks_racing_a_repeat_produce_exactly_one_refresh,
    n_tasks_racing_a_resolution_produce_exactly_one_resolve,
    racing_batches_that_overlap_post_each_fingerprint_once,
    racing_batches_that_both_collapse_open_one_group,
    // ADR 001 D5 — storm collapse.
    a_batch_above_the_threshold_opens_a_group_and_threads_its_children,
    a_late_alert_sticks_to_a_group_that_already_exists,
    a_groups_labels_are_stored_when_it_is_opened,
    a_group_opened_with_no_group_labels_stores_an_empty_map,
    a_later_batch_joining_a_group_does_not_rewrite_its_labels,
    resolving_a_collapsed_child_edits_the_childs_own_message,
    // ADR 001 D2 — the worker lease, and D3's crash rows.
    work_is_handed_to_one_worker_at_a_time,
    a_lease_hands_back_the_op_that_was_planned,
    a_dead_workers_lease_expires_and_the_row_is_reclaimed,
    a_lease_expires_at_the_instant_it_says_it_does,
    a_worker_that_posts_then_dies_has_its_work_redelivered,
    a_rate_limited_op_gives_its_attempt_back,
    a_backed_off_op_keeps_its_attempt,
    deferring_an_op_that_is_already_gone_says_so,
    completing_an_op_twice_says_so,
    a_dead_lettered_op_is_never_leased_again,
    dead_lettering_an_op_that_is_gone_says_so,
    dead_lettering_a_resolve_leaves_the_alert_alone,
    the_lease_honours_its_limit_and_takes_the_oldest_work_first,
    a_lease_hands_out_its_batch_oldest_first,
    lease_ordering_survives_variable_sub_second_precision,
    // Completion effects.
    completing_a_post_records_its_message_and_empties_the_queue,
    a_post_that_lands_after_its_resolve_records_the_timestamp_without_reviving_it,
    completing_a_group_post_gives_the_parent_its_timestamp,
    completing_a_resolve_marks_the_alert_resolved,
    a_lost_message_is_forgotten_and_replaced_in_the_same_transaction,
    a_lost_group_summary_is_forgotten_and_replaced_too,
    completing_an_orphan_post_leaves_no_correlation_state_behind,
    // Retention (ADR 001 D4; PRD §5.7).
    resolved_alerts_older_than_the_policy_are_deleted,
    a_resolved_alert_inside_the_retention_window_survives,
    an_alert_that_fires_and_never_resolves_is_eventually_deleted,
    an_alert_with_queued_work_is_never_pruned,
    a_group_with_no_surviving_members_is_deleted,
    a_group_whose_members_survive_is_not_deleted,
    a_group_whose_parent_post_is_still_queued_is_not_deleted,
    pruning_a_healthy_store_deletes_nothing,
    pruning_leaves_other_groups_alone,
);
