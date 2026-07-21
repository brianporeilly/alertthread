-- alertthread initial schema — PostgreSQL dialect.
--
-- ADR 001 D4 specifies this schema in PostgreSQL, so this file is the reference
-- and migrations/sqlite/0001_initial.sql is the translation. D4 accepts the cost
-- of two directories because the type vocabularies genuinely differ (BIGSERIAL
-- vs INTEGER PRIMARY KEY AUTOINCREMENT, JSONB vs TEXT, TIMESTAMPTZ vs TEXT).
--
-- The conformance suite is what keeps them honest: every test runs against both,
-- and `schema_matches_the_other_backend` asserts the column sets and their
-- nullability are identical, so a column added here and forgotten there fails
-- the build rather than surfacing as a decode error in production.

-- One row per (alert, channel) pair. This is the correlation state: it is what
-- turns a `resolved` webhook into an edit of the message the `firing` webhook
-- produced.
CREATE TABLE alert_message (
    fingerprint      TEXT        NOT NULL,
    channel          TEXT        NOT NULL,
    -- claimed | posted | resolving | resolved | failed
    state            TEXT        NOT NULL,
    -- NULL until the post succeeds. `chat.update` addresses a message by
    -- (channel, ts), so this column is what makes update-on-resolve possible.
    message_ts       TEXT,
    -- Non-NULL once a storm-collapsed child knows its parent (ADR 001 D5).
    thread_parent_ts TEXT,
    group_key        TEXT,
    first_seen       TIMESTAMPTZ NOT NULL,
    last_seen        TIMESTAMPTZ NOT NULL,
    resolved_at      TIMESTAMPTZ,
    labels           JSONB       NOT NULL,
    annotations      JSONB       NOT NULL,
    -- The channel is part of the key, not just the fingerprint. If the same
    -- alert is ever routed to two channels a fingerprint-only key silently
    -- loses one of them; this costs nothing and removes the failure mode.
    -- It is also the atomic claim of ADR 001 D3: the conflict target.
    PRIMARY KEY (fingerprint, channel)
);

-- The storm-collapse parent (ADR 001 D5). Its existence is what makes collapse
-- sticky: later alerts joining the group thread under it however small the
-- batch they arrive in.
CREATE TABLE group_message (
    group_key    TEXT        NOT NULL,
    channel      TEXT        NOT NULL,
    message_ts   TEXT,
    member_count INTEGER     NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (group_key, channel)
);

-- The durable outbox (ADR 001 D2). Rows are written in the same transaction as
-- the claim that produced them, which is what makes the durable write happen
-- before the 200 and is the reason a crash cannot lose an alert.
CREATE TABLE outbox (
    id              BIGSERIAL   PRIMARY KEY,
    -- post | post_group | refresh | refresh_group | resolve | post_orphan_resolved
    --
    -- Denormalised from `payload` rather than derived at read time: it is what
    -- `alertthread_outbox_depth{op}` (ADR 001 D11) groups by, and a metric that
    -- has to deserialise every queued row to report a depth is a metric nobody
    -- can afford to scrape.
    op              TEXT        NOT NULL,
    channel         TEXT        NOT NULL,
    fingerprint     TEXT,
    group_key       TEXT,
    payload         JSONB       NOT NULL,
    attempts        INTEGER     NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL,
    leased_by       TEXT,
    leased_until    TIMESTAMPTZ,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL,
    -- Not in ADR 001 D4's schema sketch, and needed by D9: an op that has
    -- exhausted its attempts "dead-letters and alerts". Without a column for it
    -- there is no way to stop the lease query handing the same doomed row out
    -- forever — `attempts` alone records how often it failed, not that it has
    -- stopped being retried.
    dead_lettered_at TIMESTAMPTZ
);

-- The lease query's covering index. Partial, because a leased row is not a
-- candidate and there is no reason to carry it in the index the drain path hits
-- on every poll.
CREATE INDEX outbox_ready ON outbox (next_attempt_at) WHERE leased_until IS NULL;

-- The pruner's sweep over resolved alerts (ADR 001 D4, retention).
CREATE INDEX alert_message_prune ON alert_message (resolved_at);

-- The stale sweep, which is the one that catches alerts that fire and never
-- resolve. Not in D4's sketch: D4 specifies the sweep but indexes only
-- `resolved_at`, which does not serve it.
CREATE INDEX alert_message_stale ON alert_message (last_seen);

-- `complete`, `defer` and the pruner's "does this row still have queued work?"
-- guard all look an outbox row up by what it refers to rather than by id.
CREATE INDEX outbox_subject ON outbox (channel, fingerprint);
CREATE INDEX outbox_group ON outbox (channel, group_key);
