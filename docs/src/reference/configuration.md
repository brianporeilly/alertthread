# Configuration

Configuration is loaded by `figment`, in three layers. Later layers win:

1. **Built-in defaults** — every value on this page has one except the bot token and the
   default channel.
2. **A YAML file**, named as the first argument to the binary, or by `ALERTTHREAD_CONFIG`.
   A missing file is not an error: configuring a container purely through environment
   variables is the normal case, and requiring a file for it would mean shipping an empty
   one in the image.
3. **Environment variables**, prefixed `ALERTTHREAD_`, with `__` between nested keys —
   `ALERTTHREAD_SLACK__DEFAULT_CHANNEL`, `ALERTTHREAD_STORAGE__RETENTION__RESOLVED`. A
   double underscore, because a single one is legal inside a key name and
   `ALERTTHREAD_SLACK_DEFAULT_CHANNEL` would need the reader to know the schema.

**A new config option is not merged until it appears on this page.** That rule is in
AGENTS.md, and this page is the reason it exists.

⚠️ The two secrets on this page — the Slack bot token and the webhook bearer token — are read
from an environment variable or a file and are **never logged**. Each is held in a newtype
whose `Debug` prints `<redacted>`, so it stays redacted inside every struct that embeds one —
including the whole `Config`, which *is* logged at startup.

## Refusing to start

Three settings make the relay incoherent rather than merely odd, and all three are checked
at startup rather than at the first webhook. By the time a webhook arrives the relay has
already told Alertmanager it would take the alert; refusing then would be too late.

| Condition | Why it is fatal |
|---|---|
| No `slack.token` and no readable `slack.token_file` | There is no degraded mode without one |
| No `slack.default_channel` | [ADR 001 D8](../adr/001-adr.md) resolves the channel as `?channel=` → `slack.default_channel` → refuse to start |
| `resolve.update_in_place` and `resolve.thread_reply` both `false` | [ADR 001 D6](../adr/001-adr.md): a resolve that does nothing is indistinguishable from the bug this relay exists to fix |
| `storage.backend` is not a backend this build has | Named at boot rather than as a connection error against a URL scheme with no driver behind it |
| An unrecognised key anywhere in the file | A misspelled key is a setting an operator believes is in effect |
| `server.auth_token_file` is set and cannot be read | The operator asked for a perimeter; serving without one because a secret failed to mount is the one outcome they would never find out about |

Everything else **degrades rather than refusing**. A template override that will not compile
is dropped and the built-in kept; a file in `templates.dir` that is not one of the four
template names is skipped with a warning. That is [ADR 001 D9](../adr/001-adr.md)'s
reasoning applied one step earlier: a pod that refuses to start over a typo in a `ConfigMap`
is total silence, which is strictly worse than degraded-but-alive.

## Durations

Anywhere this page says *duration*, the value is written the way an operator writes one:
`7d`, `30s`, `250ms`, `15m`, `1h30m`. A **bare number is seconds** — somebody who writes
`timeout: 15` means fifteen seconds. Understood suffixes are `ms`, `s`, `m`, `h`, `d`;
anything else is rejected at startup, by name.

---

## `storage`

Where correlation state and the delivery outbox live.

```yaml
storage:
  backend: sqlite
  url: sqlite:///var/lib/alertthread/state.sqlite
  retention:
    resolved: 7d
    stale: 30d
    interval: 1h
```

### `storage.backend`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_STORAGE__BACKEND` |
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
| Environment variable | `ALERTTHREAD_STORAGE__URL` |
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
| Environment variable | `ALERTTHREAD_STORAGE__RETENTION__RESOLVED` |
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
| Environment variable | `ALERTTHREAD_STORAGE__RETENTION__STALE` |
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

### `storage.retention.interval`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_STORAGE__RETENTION__INTERVAL` |
| Type | duration |
| Default | `1h` |

How often the pruner sweeps. It runs on its own schedule, separate from the outbox worker:
retention runs hourly and delivery runs four times a second, and folding the sweep into the
delivery loop would either run it far too often or make the delivery interval hostage to how
long a `DELETE` takes.

A failed sweep is logged and the loop carries on. A pruner that cannot run costs disk; a
relay that stopped because its pruner failed costs alerts.

---

## `server`

Where the HTTP server listens, and how it behaves.

```yaml
server:
  listen: "0.0.0.0:8080"
  request_timeout: 15s
  shutdown_grace: 20s
  auth_token: ~                # or server.auth_token_file; unset means no auth
```

### `server.listen`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SERVER__LISTEN` |
| Type | `address:port` |
| Default | `0.0.0.0:8080` |

The address to bind. A port already in use is fatal at startup — otherwise the pod comes up,
passes its liveness probe, and quietly accepts nothing.

### `server.request_timeout`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SERVER__REQUEST_TIMEOUT` |
| Type | duration |
| Default | `15s` |

How long a request may take before the server abandons it. Comfortably above the 50 ms p99
[ADR 001 D2](../adr/001-adr.md) targets for ingest, because what it protects against is a
store that has stopped answering — and in that case the right outcome is a fast `503` that
Alertmanager retries, not a socket held open.

### `server.shutdown_grace`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SERVER__SHUTDOWN_GRACE` |
| Type | duration |
| Default | `20s` |

How long to let in-flight work finish after `SIGTERM` or `SIGINT`. A clean shutdown drains
the batch the worker is holding rather than relying on its leases expiring — an abandoned
lease is not a bug, but waiting the full lease duration is time an alert spends undelivered
for no reason.

### `server.auth_token`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SERVER__AUTH_TOKEN` |
| Type | string |
| Default | unset — **the webhook is unauthenticated** |

A bearer token that `POST /webhook` requires ([ADR 001 D11](../adr/001-adr.md)). When it is
set, a delivery must carry `Authorization: Bearer <token>` or it is answered `401`. Never
logged: see the warning at the top of this page.

Off by default, deliberately. The endpoint is cluster-internal in every deployment shape this
project targets, and a relay that started requiring a credential on upgrade would `401` every
delivery from an Alertmanager nobody had reconfigured yet — silence introduced by a security
feature.

**Covers `POST /webhook` and nothing else.** `/healthz`, `/readyz` and `/metrics` are never
authenticated: probes and scrapes carry no credentials, and a `401` on either of the first two
is a pod that never becomes ready, while a `401` on `/metrics` breaks the relay's own alerting.

| Behaviour | Detail |
|---|---|
| Comparison | Constant-time. A `401` reveals nothing about how close the credential was |
| Refusal | `401`, a bare `WWW-Authenticate: Bearer`, and the body `unauthorized` — identical for every kind of failure |
| Counted as | `alertthread_webhook_requests_total{outcome="auth_missing"}` or `{outcome="auth_mismatch"}` |
| Logged as | ERROR. Alertmanager does not retry a `401`, so a refused delivery is lost |
| Read | Once, at startup. A rotated secret needs a restart |
| Scheme | Case-insensitive, per RFC 7235. `bearer` and `Bearer` both work |

An empty value — which is what a chart renders for a secret that did not resolve — is treated as
**unset**, and warned about at startup:

```
WARN server.auth_token is set to an empty value, so POST /webhook is unauthenticated.
```

Refusing to start over an empty *optional* setting would be an outage caused by a security
feature, and treating it as configured would mean matching every credential against `""`. The
warning is the third option, and it is the only signal that distinguishes this from the default.

The Alertmanager side, and the full setup, is in
[Harden a deployment](../how-to/harden-a-deployment.md).

### `server.auth_token_file`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SERVER__AUTH_TOKEN_FILE` |
| Type | path |
| Default | unset |

Read `server.auth_token` from a file instead — the mounted-secret shape. It wins over an inline
value if both are set, for the same reason `slack.token_file` does.

Trailing whitespace is trimmed, because `kubectl create secret --from-file` keeps the newline.
The inline value is **not** trimmed.

A configured path that cannot be read is **fatal**, unlike a blank inline value. The operator
named a mount, and a relay that served an unauthenticated webhook because the mount failed
would be a perimeter nobody could tell was missing.

⚠️ Alertmanager does not trim what *it* reads from `credentials_file`. A secret written with a
trailing newline sends a credential with a trailing newline, and the relay answers `401` — the
single most common cause of `auth_mismatch`.

---

## `slack`

```yaml
slack:
  token: xoxb-…              # or slack.token_file
  default_channel: "#alerts"
  base_url: https://slack.com/api/
  timeout: 15s
  rate_limit_divisor: 1
  auth_probe_interval: 15m
  auth_startup_grace: 30s
```

### `slack.token`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__TOKEN` |
| Type | string |
| Default | none — **required** |

The bot token, `xoxb-…`. Never logged: see the warning at the top of this page.

The relay calls `auth.test` once at startup and **refuses to start** if Slack rejects the
token ([ADR 001 D11](../adr/001-adr.md)). A container that will not start is visible; a
relay that starts and cannot post is not, and the alerts it accepts in the meantime pile up
in an outbox nothing can drain.

### `slack.token_file`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__TOKEN_FILE` |
| Type | path |
| Default | unset |

Read the token from a file instead — the usual Kubernetes mounted-secret shape. It wins over
`slack.token` if both are set: a mount is the more specific answer, and a deployment that
sets both meant the mount.

Trailing whitespace is trimmed. `kubectl create secret --from-file` keeps the newline, and
the error a newline produces further downstream talks about HTTP headers rather than tokens.

### `slack.default_channel`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__DEFAULT_CHANNEL` |
| Type | string |
| Default | none — **required** |

Where to post when the webhook URL carries no `?channel=`. Either form Slack accepts works —
`#alerts` or `C01234567` — and the value is kept verbatim, because normalising it would only
add a way to be wrong.

[ADR 001 D8](../adr/001-adr.md) resolves the channel as `?channel=` → this → refuse to start.
A `?channel=` that is present but blank — which is what a receiver URL rendered from a
template with an unset variable produces — counts as absent, because posting to a channel
named `""` earns `channel_not_found`, which is terminal, which is a dead-lettered alert.

### `slack.base_url`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__BASE_URL` |
| Type | URL |
| Default | `https://slack.com/api/` |

The Slack Web API root. Pointed at `dev/slack-mock` for local development. A trailing slash
is added if it is missing, because `Url::join` on a base without one discards the last path
segment and would silently send every call to the wrong URL.

### `slack.timeout`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__TIMEOUT` |
| Type | duration |
| Default | `15s` |

How long a single Slack call may take. Generous by HTTP standards, deliberately: a slow Slack
is not an emergency because the outbox is already absorbing the latency, and timing out early
would convert a slow success into a retry — and a retry of `chat.postMessage` is a duplicate
message.

### `slack.rate_limit_divisor`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__RATE_LIMIT_DIVISOR` |
| Type | number ≥ 1 |
| Default | `1` |

Divides the relay's own rate limits by this factor. **Set it to the replica count** when
running more than one.

[ADR 001 D2](../adr/001-adr.md) records the honest limitation: each replica holds its own
token bucket, so the aggregate rate is N times the per-process one. This is the stated
mitigation; Slack's 429 with `Retry-After` is the real backstop, and the relay honours it
without counting it as a failed attempt. A value below 1 is treated as 1 — a misconfiguration
must not be able to make the relay post *faster* than Slack allows and then blame the 429s
on itself.

### `slack.auth_probe_interval`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__AUTH_PROBE_INTERVAL` |
| Type | duration |
| Default | `15m` |

How often to re-check that the bot token is still valid, in the background.

Startup refuses to run with a token Slack definitively rejects. What this covers is mid-life
revocation: a token revoked at 2pm with nothing firing until 3am is a silent failure
discovered at the worst possible moment. 96 calls a day is negligible.

The result feeds `alertthread_slack_auth_valid` and **not** `/readyz` — see
[Metrics](metrics.md) and [HTTP API](http-api.md) for why.

The probe also has a second job. If it sees the token go from rejected to accepted, it
returns everything in the dead-letter queue to the outbox, so the alerts that a bad token
cost are delivered rather than written off. `alertthread_dead_letter_revived_total` counts
them.

### `slack.auth_startup_grace`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_SLACK__AUTH_STARTUP_GRACE` |
| Type | duration |
| Default | `30s` |

How long to keep retrying a **transient** startup `auth.test` before starting anyway.

The relay checks the bot token once at startup, and what it does about a failure depends on
which kind it was:

| Startup `auth.test` result | Behaviour |
|---|---|
| Accepted | Start. `alertthread_slack_auth_valid` is `1` |
| `invalid_auth`, `not_authed`, `account_inactive`, `token_revoked`, `token_expired`, `missing_scope`, a malformed token, an unusable `base_url` | **Refuse to start**, immediately, with no retry |
| A transport error, an HTTP 5xx, a 429, or anything else the [Slack error taxonomy](slack-errors.md) calls retryable | Retry with backoff for up to `auth_startup_grace`, then **start anyway** with `alertthread_slack_auth_valid` at `0` |

Setting it to `0s` makes one attempt and then starts. There is no setting that makes a
definitively rejected token start the relay, and no setting that makes a Slack outage stop
it starting.

Raise it if your Slack egress is behind something slow to come up, and keep it inside your
`startupProbe` budget: a pod that is still in startup serves neither `/metrics` nor
`/readyz`, so a long grace trades one kind of invisibility for another.

---

## `resolve`

What a resolution does ([ADR 001 D6](../adr/001-adr.md)). Both default on, and they solve
different problems.

```yaml
resolve:
  update_in_place: true
  thread_reply: true
```

### `resolve.update_in_place`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_RESOLVE__UPDATE_IN_PLACE` |
| Type | boolean |
| Default | `true` |

Rewrite the original message: red becomes green, and the channel history is accurate whenever
somebody scrolls it.

### `resolve.thread_reply`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_RESOLVE__THREAD_REPLY` |
| Type | boolean |
| Default | `true` |

Post a threaded reply under the alert's own message.

This is not redundant with the edit. **`chat.update` does not notify, does not bump the
message, and does not mark the channel unread**, so an in-place edit alone is *invisible to
anyone watching the channel live*. The reply is what generates the unread indicator, and with
`reply_broadcast: false` it costs no channel noise.

⚠️ Setting **both** to `false` is a configuration error and the relay refuses to start.

---

## `collapse`

Storm collapse ([ADR 001 D5](../adr/001-adr.md)) and the repeat-firing debounce
([D7](../adr/001-adr.md)).

```yaml
collapse:
  threshold: 5
  refresh_debounce: 60s
```

### `collapse.threshold`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_COLLAPSE__THRESHOLD` |
| Type | integer |
| Default | `5` |

How many **new** messages one delivery may produce for a channel before the batch is
collapsed into a single summary with the individual alerts threaded beneath it. The
comparison is strictly greater-than: five new posts with a threshold of five stay as five
top-level messages.

Collapse is **sticky**. Once a group has a summary, later alerts joining it thread underneath
even when they arrive one at a time — otherwise a group's alerts would be split between
top-level messages and thread replies depending on batch timing, which is worse than either
consistent behaviour.

`0` disables collapse **entirely**, stickiness included: no summary is posted, nothing is
threaded, and an existing summary stops attracting new members and stops having its count
refreshed. That last part is the one visible cost — an operator who turns collapse off after
a storm leaves the existing summary showing the count it had at that moment. That is a stale
message, not a lost one, and it is the price of a setting that means what it says.

> The default of `5` is flagged in ADR 001's own open questions as a guess, to be revisited
> against real alert volume.

### `collapse.refresh_debounce`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_COLLAPSE__REFRESH_DEBOUNCE` |
| Type | duration |
| Default | `60s` |

How long after a message was last seen a repeat delivery counts as a genuine
`repeat_interval` re-send rather than an HTTP retry of the same delivery.

This is the whole mechanism that separates "Alertmanager retried the request" (seconds apart)
from "Alertmanager re-sent on its repeat interval" (12 hours apart) without the relay having
to model either timer. A repeat past the window refreshes the message in place; one inside it
is a duplicate delivery and does nothing.

`0` is legitimate and means "refresh on every repeat delivery" — noisy, but not wrong. A
**negative** value is rejected at startup: it would treat every retried delivery as a repeat.

---

## `worker`

How the outbox is drained ([ADR 001 D2](../adr/001-adr.md)).

```yaml
worker:
  batch_size: 64
  lease: 60s
  idle_poll: 250ms
  max_attempts: 10
  backoff_base: 4s
  backoff_max: 10m
  sample_interval: 15s
```

### `worker.batch_size`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_WORKER__BATCH_SIZE` |
| Type | integer |
| Default | `64` |

How many outbox rows one lease takes. The worker groups them by channel and drains channels
concurrently, ops within a channel serially — so a larger batch buys parallelism across
channels and nothing within one.

### `worker.lease`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_WORKER__LEASE` |
| Type | duration |
| Default | `60s` |

How long a lease is held before the row becomes reclaimable by another worker. This is what
makes a crashed worker's rows retryable rather than stuck; it is also the longest an alert
can be delayed by a worker that died mid-delivery.

### `worker.idle_poll`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_WORKER__IDLE_POLL` |
| Type | duration |
| Default | `250ms` |

How long to wait after a pass that did not fill its batch.

Short, because it is also how long a self-deferred op waits past its `next_attempt_at` — and
the per-channel rate limiter defers roughly one op per channel per pass, so a long poll would
turn "one message per second" into "one message per poll".

The relay polls rather than being woken. `LISTEN`/`NOTIFY` would be PostgreSQL-only, and the
SQLite deployment would then need a second mechanism doing the same job — exactly the
divergence two backends behind one trait exist to avoid.

### `worker.max_attempts`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_WORKER__MAX_ATTEMPTS` |
| Type | integer |
| Default | `10` |

How many attempts an op gets before it is dead-lettered ([ADR 001 D9](../adr/001-adr.md)).

An attempt is consumed by *leasing*, not by failing — so a row that kills its worker every
time still reaches the dead-letter queue instead of cycling for ever. A Slack 429 and the
relay's own pacing both give the attempt back: neither is the op failing, and counting them
would march an alert toward the dead-letter queue for arriving during a storm.

**Every dead-letter is an alert nobody was told about.** Page on
`alertthread_dead_letter_total`.

### `worker.backoff_base` and `worker.backoff_max`

| | |
|---|---|
| Environment variables | `ALERTTHREAD_WORKER__BACKOFF_BASE`, `ALERTTHREAD_WORKER__BACKOFF_MAX` |
| Type | duration |
| Defaults | `4s`, `10m` |

The first retry delay and the ceiling. The delay doubles per attempt and carries a
deterministic ±12.5% spread keyed on the attempt number, so a hundred ops deferred by one
outage do not all come back in the same millisecond.

With the defaults, the nine waits an op spends before it is parked add up to roughly the half
hour ADR 001 D9 quotes.

### `worker.sample_interval`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_WORKER__SAMPLE_INTERVAL` |
| Type | duration |
| Default | `15s` |

How often the store is sampled for the gauges in [Metrics](metrics.md).

The gauges are **sampled in the background and served from the last sample**, never queried
inside `GET /metrics`. A scrape every 15 seconds across N replicas would otherwise make
Prometheus a load generator pointed at the outbox, and a slow store would time the scrape out
and take every other metric with it.

Until the first sample lands, `alertthread_outbox_depth` is absent from the exposition rather
than zero — a Prometheus `Family` with no members emits nothing at all.

---

## `templates`

Message template overrides ([ADR 001 D10](../adr/001-adr.md)).

```yaml
templates:
  dir: /etc/alertthread/templates
```

### `templates.dir`

| | |
|---|---|
| Environment variable | `ALERTTHREAD_TEMPLATES__DIR` |
| Type | path |
| Default | unset — the four built-in templates are used |

A directory of overrides, one file per template, named after it: `firing`, `resolved`,
`group_summary`, `thread_reply`. A `.j2`, `.jinja` or `.txt` extension is accepted and
stripped, because a `ConfigMap` of templates is a directory of files and people name files
with extensions.

Files whose names are not one of the four are **ignored with a warning**, not rejected: a
`ConfigMap` mount brings `..data` and dotted symlinks with it, and a relay that refused to
start over one could not run in the place it is designed for.

An override that does not compile is **dropped and its built-in kept**, with an error logged
naming the template and the line. That error is the only signal an operator gets that the
`ConfigMap` they just applied is not in effect.

A directory that cannot be listed at all *is* fatal — that is an operator naming a path that
is not mounted.

See [Customize message templates](../how-to/customize-templates.md) for what a template can
see.

---

## Logging

Two environment variables, and no config-file equivalent: logging has to be configurable
before the configuration file has been read.

| Variable | Default | What |
|---|---|---|
| `ALERTTHREAD_LOG` | `info,alertthread=info` | A `tracing` env-filter directive |
| `ALERTTHREAD_LOG_FORMAT` | human-readable | Set to `json` for structured output |
| `ALERTTHREAD_CONFIG` | unset | Path to the YAML file, if not given as the first argument |

⚠️ **All three currently make the relay refuse to start**, with
`unknown field: found \`log\`` or similar. The `ALERTTHREAD_` environment layer reads a name
with no `__` in it as a *top-level* configuration key, and an unrecognised key is fatal — see
ROADMAP known open item 22. Until that is fixed, pass the configuration file as the first
argument instead, and there is no way to change the log filter (`RUST_LOG` is not read).
Nested names such as `ALERTTHREAD_STORAGE__URL` are unaffected.

JSON is not the default because the first thing anybody does with this binary is run it in a
terminal, and a wall of JSON there is structured without being clear.
