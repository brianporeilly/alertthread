-- alertthread initial schema — SQLite dialect.
--
-- The translation of migrations/postgres/0001_initial.sql. Read that file for
-- what each table is for; this one records only what had to change and why.
--
-- Three differences, all forced by SQLite's type system:
--
--   * BIGSERIAL           -> INTEGER PRIMARY KEY AUTOINCREMENT
--   * JSONB               -> TEXT. SQLite has JSON *functions* but no JSON
--                            column type. The relay writes and reads these
--                            columns whole, so nothing is lost.
--   * TIMESTAMPTZ         -> TEXT. sqlx encodes `DateTime<Utc>` as RFC 3339
--                            with a `+00:00` offset, which orders correctly
--                            under SQLite's lexicographic TEXT comparison —
--                            see the note below, and the conformance test
--                            `lease_ordering_survives_sub_second_precision`
--                            which exists precisely to hold that property.
--
-- ⚠️ Every timestamp in this database MUST be written by binding a
-- `DateTime<Utc>` from Rust. Never `datetime('now')`, never a hand-formatted
-- string: the lease and prune queries compare these columns with `<` and `<=`,
-- and SQLite compares TEXT byte by byte. A second timestamp format in these
-- columns would not fail — it would silently sort wrong, which in this system
-- means an alert that is never leased.

CREATE TABLE alert_message (
    fingerprint      TEXT NOT NULL,
    channel          TEXT NOT NULL,
    -- claimed | posted | resolving | resolved | failed
    state            TEXT NOT NULL,
    message_ts       TEXT,
    thread_parent_ts TEXT,
    group_key        TEXT,
    first_seen       TEXT NOT NULL,
    last_seen        TEXT NOT NULL,
    resolved_at      TEXT,
    labels           TEXT NOT NULL,
    annotations      TEXT NOT NULL,
    PRIMARY KEY (fingerprint, channel)
);

CREATE TABLE group_message (
    group_key    TEXT    NOT NULL,
    channel      TEXT    NOT NULL,
    message_ts   TEXT,
    member_count INTEGER NOT NULL DEFAULT 0,
    -- JSONB in the PostgreSQL dialect; TEXT here, per the JSONB -> TEXT note
    -- at the top of this file. Read the PostgreSQL migration for what the
    -- column is for and why it is written once.
    group_labels TEXT    NOT NULL,
    created_at   TEXT    NOT NULL,
    PRIMARY KEY (group_key, channel)
);

CREATE TABLE outbox (
    -- AUTOINCREMENT, not a bare INTEGER PRIMARY KEY. Without it SQLite reuses
    -- the rowid of a deleted row, and outbox rows are deleted on completion —
    -- so ids would be recycled while a crashed worker still holds a lease
    -- naming one of them. `complete(id)` would then act on somebody else's op.
    --
    -- The explicit NOT NULL is not redundant. SQLite reports an INTEGER PRIMARY
    -- KEY as nullable, because it historically allows an explicit NULL to mean
    -- "assign one"; PostgreSQL's BIGSERIAL is NOT NULL. Without this the two
    -- backends genuinely differ, and the conformance suite's schema check said
    -- so — which is the whole reason it exists.
    id               INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    op               TEXT    NOT NULL,
    channel          TEXT    NOT NULL,
    fingerprint      TEXT,
    group_key        TEXT,
    payload          TEXT    NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  TEXT    NOT NULL,
    leased_by        TEXT,
    leased_until     TEXT,
    last_error       TEXT,
    created_at       TEXT    NOT NULL,
    dead_lettered_at TEXT
);

CREATE INDEX outbox_ready ON outbox (next_attempt_at) WHERE leased_until IS NULL;
CREATE INDEX alert_message_prune ON alert_message (resolved_at);
CREATE INDEX alert_message_stale ON alert_message (last_seen);
CREATE INDEX outbox_subject ON outbox (channel, fingerprint);
CREATE INDEX outbox_group ON outbox (channel, group_key);
