# Configuration

*Status: populated per-phase, as options are added.*

Configuration is loaded by `figment`: a YAML file, layered with environment-variable
overrides.

**A new config option is not merged until it appears on this page.** That rule is in
AGENTS.md, and this page is the reason it exists.

The tables below will document, for every option: its key, its environment-variable
equivalent, its type, its default, and what it does.

Planned sections, in the phase that fills them:

| Section | Phase |
|---|---|
| `slack.*` — token, default channel, rate limiting | 3 |
| `templates.*` — overrides | 3 |
| `server.*` — bind address, timeouts, optional bearer token | 4 |
| `resolve.*` — `update_in_place`, `thread_reply` | 4 |
| `collapse.*` — `collapse_threshold` | 4 |

⚠️ The bot token is read from an environment variable or a file and is never logged. The
config type carries a redacting `Debug` implementation.

---

## `storage`

Where correlation state and the delivery outbox live.

> **Status.** The store implements all of this — backend selection, both URL dialects,
> migrations and the retention pruner — as of Phase 2. The `figment` layer that reads these
> keys out of a YAML file and its environment overrides lands in Phase 4 along with the rest
> of the binary's configuration, so the key and variable names below are what Phase 4 will
> bind to, not what a running binary accepts today.

```yaml
storage:
  backend: sqlite
  url: sqlite:///var/lib/alertthread/state.sqlite
  retention:
    resolved: 7d
    stale: 30d
```

### `storage.backend`

| | |
|---|---|
| Environment variable | `STATE_BACKEND` |
| Type | `sqlite` \| `postgres` |
| Default | `sqlite` |

Selects the state store. Matched exactly: `SQLite`, `postgresql` and a leading space are all
rejected at startup, by name.

The value must also be compiled into the binary. Published images carry both backends; a
binary built with `--no-default-features --features postgres` refuses `sqlite` and says so.

| | `sqlite` | `postgres` |
|---|---|---|
| Replicas | Exactly 1 | N |
| Deploy strategy | `Recreate`, RWO PVC | `RollingUpdate`, no PVC |
| External dependency | None (bundled) | An existing PostgreSQL |

To move from one to the other, see [Enable HA with PostgreSQL](../how-to/enable-ha-postgres.md).

### `storage.url`

| | |
|---|---|
| Environment variable | `DATABASE_URL` |
| Type | string |
| Default | none — required |

The connection string, in the dialect of the selected backend.

| Backend | Form | Example |
|---|---|---|
| `sqlite` | `sqlite://<path>` | `sqlite:///var/lib/alertthread/state.sqlite` |
| `postgres` | `postgres://<user>:<password>@<host>/<database>` | `postgres://alertthread:…@pg.observability.svc/alertthread` |

The SQLite file is **created if it does not exist**, along with its schema. The directory
must exist and be writable.

Three SQLite connection settings are applied by the relay and are not configurable:
`journal_mode=WAL`, `synchronous=NORMAL`, and a 30-second busy timeout. They are properties
the store depends on rather than tuning: without WAL a reader blocks behind the writer, and
a short busy timeout turns contention into `503`s under exactly the load that produced the
alerts.

### Migrations

Applied automatically at startup, every time, from the migrations compiled into the binary.
There is no separate migration step and no flag to skip it. Running the same version twice
changes nothing.

Each backend has its own migration set — the type vocabularies differ — and the store's
conformance suite runs against both, asserting that the schemas they build are identical
column for column.

### `storage.retention.resolved`

| | |
|---|---|
| Environment variable | `STORAGE_RETENTION_RESOLVED` |
| Type | duration |
| Default | `7d` |

How long a resolved alert's correlation state is kept before the pruner deletes it.

Long enough that a resolution arriving late still correlates; short enough that the table
does not grow without bound. Below roughly an hour, a `resolved` webhook that Alertmanager
re-sends may find its state already gone and surface as an orphan (see
`alertthread_orphan_resolves_total`).

### `storage.retention.stale`

| | |
|---|---|
| Environment variable | `STORAGE_RETENTION_STALE` |
| Type | duration |
| Default | `30d` |

How long an alert is kept after it was last seen, **whatever state it is in**.

This is the sweep that catches alerts which fire and never resolve; without it one
misbehaving rule pins a row for ever, which on a SQLite deployment is a PVC that grows
without bound. It must be comfortably longer than the longest alert you expect to stay
firing — a value shorter than that deletes the state of an alert that is still active, and
its eventual resolution then arrives as an orphan.

### What the pruner will not delete

Neither sweep ever deletes a row that still has queued work, however far past the retention
window it is. An `alert_message` deleted while its post was in flight would leave a message
in Slack that nothing is tracking; a `group_message` deleted while its summary post was
queued would leave that message's timestamp nowhere to land.

Storm-collapse parents are deleted separately, once they have no surviving members and no
queued work.
