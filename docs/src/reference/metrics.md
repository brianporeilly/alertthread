# Metrics

All metrics are prefixed `alertthread_` and exposed at `GET /metrics` in the OpenMetrics text
format.

**A new metric is not merged until it appears on this page.** That rule is in AGENTS.md.

## What to alert on

Two of these matter more than the rest, and they mean different things.

`alertthread_outbox_oldest_age_seconds` is the metric that actually means **"alerts are not
reaching Slack"**. Alert on it in preference to the error counters, which tell you something
went wrong but not whether delivery is still happening. A healthy relay keeps this in the
low seconds even during a storm; a climbing value with no dead letters means the queue is
draining slower than it is filling.

`alertthread_dead_letter_total` is the other one. **Every increment is an alert nobody was
told about.** Page on it.

`alertthread_slack_auth_valid` dropping to `0` means queued alerts will not be delivered
until the token is replaced — see [why it is a metric and not readiness](#why-a-revoked-token-does-not-make-the-relay-unready).

## Ingest

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `alertthread_alerts_received_total` | counter | `status` | Alerts accepted from Alertmanager |
| `alertthread_webhook_requests_total` | counter | `outcome` | Webhook deliveries, by what the relay did with them |
| `alertthread_orphan_resolves_total` | counter | — | Resolutions with no correlation state behind them |
| `alertthread_alerts_truncated_total` | counter | — | Alerts Alertmanager dropped because `max_alerts` is not `0` |
| `alertthread_storm_collapses_total` | counter | — | Storm-collapse groups opened |

`status` is `firing`, `resolved`, or `other`. An unrecognised status is folded to `other`
rather than passed through: that string comes from outside the relay, and a label value the
sender controls is an unbounded label value. The raw string reaches the log line instead.

`outcome` is one of six values, and three of them mean an alert was lost:

| `outcome` | Status | Lost? |
|---|---|---|
| `accepted` | `200` | No — committed to the store |
| `rejected` | `400` | **Yes** — a body this build cannot parse, and a retry cannot fix it |
| `auth_missing` | `401` | **Yes** — no `Authorization` header, or one that was not a `Bearer` credential |
| `auth_mismatch` | `401` | **Yes** — a bearer credential that is not `server.auth_token` |
| `store_unavailable` | `503` | No — Alertmanager redelivers |
| `misconfigured` | `500` | No — Alertmanager retries; the relay has no channel to post to |

The two `auth_*` values are separate because the fix differs: `auth_missing` is a sender with no
credential configured at all (or a proxy stripping the header), `auth_mismatch` is a sender whose
credential has drifted from the relay's. They are **not** distinguishable from the outside — every
refusal is an identical `401` — which is the point of splitting them here. Both are logged at
ERROR, because Alertmanager does not retry a `401`. See
[Harden a deployment](../how-to/harden-a-deployment.md).

`alertthread_orphan_resolves_total` rising **with no restarts** is the signature of a non-zero
`max_alerts` on the Alertmanager side. That is exactly why `alertthread_alerts_truncated_total`
exists next to it: [ADR 001 D8](../adr/001-adr.md) notes that the symptom of that
misconfiguration "points nowhere near the cause", and the truncation counter *is* the cause,
reported by the sender itself at the moment it happens. Correlate the two.

## Delivery

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `alertthread_slack_calls_total` | counter | `method`, `outcome` | Slack Web API calls |
| `alertthread_slack_call_duration_seconds` | histogram | `method` | Slack Web API latency |
| `alertthread_rate_limited_total` | counter | `method`, `source` | Deliveries deferred by a rate limit |
| `alertthread_fallback_posts_total` | counter | `reason` | Messages built from the hardcoded fallback |
| `alertthread_dead_letter_total` | counter | `reason` | **Operations parked. Page on this.** |
| `alertthread_dead_letter_revived_total` | counter | — | Parked operations returned to the queue |

`method` is Slack's own spelling — `chat.postMessage`, `chat.update`, `auth.test` — so the
metric can be correlated with Slack's documentation without a translation step.

`outcome` is `ok` or one variant name per failure class: `rate_limited`, `message_not_found`,
`invalid_auth`, `channel_unusable`, `bad_request`, `slack_unavailable`, `unrecognised`,
`http_status`, `transport`, `malformed_response`, `incomplete_response`. Slack's error codes
themselves are open-ended and never reach a label; the full code is in the log line and in
the stuck row's `last_error`.

`reason` on `fallback_posts` is `render_failed` or `empty_output`. Either means a template is
broken and the message that went out was the built-in minimal one — degraded, but not silent.

`reason` on `dead_letter` is the same closed set as `outcome`, plus `alert_row_missing`.

### What happens to a parked operation

Parking is not the end of the alert's life, and this is the part the counter alone does not
tell you.

- **It is logged twice, deliberately.** Once at ERROR by the worker at the moment it parks,
  with the payload, and again at ERROR by the background reporter — which announces every
  parked row **once per process**. That second line is the one that matters: the first is
  gone from your log retention by the time anybody is paged, and it does not come back on a
  restart. The reporter's does.
- **It stays in the `outbox` table.** The row is the only record of an alert that never
  reached Slack, and deleting it would erase the evidence of the one failure mode this
  project treats as unacceptable.
- **It is returned to the queue if the bot token starts working again.** `invalid_auth` is a
  reason [ADR 001 D9](../adr/001-adr.md) parks immediately, and the background auth probe
  seeing the token go from rejected to accepted is the event that says the reason has gone.
  Everything parked is revived at that point and `alertthread_dead_letter_revived_total`
  counts it. Those alerts are late — late is the point, because the alternative is never.

An operation parked for a reason the token has nothing to do with (`msg_too_long`,
`channel_unusable`) is revived by the same sweep, fails once more, and re-parks. That costs
one Slack call on an event that only happens when a human has just fixed something, and it
is the direction this project errs in: the cost of not reviving a row that should have been
is an alert nobody hears about.

### `source` on `alertthread_rate_limited_total`

**Not in [ADR 001 D11](../adr/001-adr.md)**, which lists `{method}` alone. It is here because
the relay is rate-limited in two quite different ways and the operator's next action differs:

- `slack` — Slack returned 429 and the outbox is riding it out on the `Retry-After` it gave.
  Consider `slack.rate_limit_divisor`.
- `local` — this process's own token bucket paced itself and no request was made at all. This
  is normal during a storm: Slack allows roughly one `chat.postMessage` per second per
  channel, so a fifteen-alert batch produces fourteen `local` deferrals on its way out.

Without the label those two are one number, and "are we being throttled, or throttling
ourselves?" is unanswerable — which is the only question this counter gets asked.

Neither kind of rate limit consumes one of an op's `max_attempts`. Counting them would march
an alert toward the dead-letter queue for the crime of arriving during a storm, which is
exactly when it matters most.

## The store

These five are **sampled on a background interval** (`worker.sample_interval`, default 15 s)
and served from the last sample. `GET /metrics` never queries the database.

That is deliberate. Prometheus scrapes every 15 seconds, from every replica, for ever:
querying the outbox from the handler would make the monitoring system a load generator
pointed at the queue it is monitoring. Worse, a slow store would make the scrape time out —
and a timed-out scrape loses *every* metric in the response, including the counters that
would have said what was wrong.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `alertthread_outbox_depth` | gauge | `op` | Rows waiting to be delivered |
| `alertthread_outbox_oldest_age_seconds` | gauge | — | **Primary SLO signal** |
| `alertthread_outbox_dead_lettered` | gauge | — | Rows parked and not yet cleared |
| `alertthread_tracked_fingerprints` | gauge | — | Alerts with correlation state |
| `alertthread_store_sample_ok` | gauge | — | Whether the last sample succeeded |

`op` is one of `post`, `post_group`, `refresh`, `refresh_group`, `resolve`,
`post_orphan_resolved`. All six are published on every sample, including the ones with
nothing queued: a gauge that simply stops being reported reads as "no data" in Prometheus
rather than as "nothing pending", and an alert on outbox depth would go stale rather than
clear.

Dead-lettered rows are **excluded** from `outbox_depth` and from `outbox_oldest_age_seconds`,
and counted separately. Nothing will ever lease them again, so including them would peg the
one gauge an operator alerts on at "for ever" from the first parked alert — and the alert
that fired because of it would never clear.

`alertthread_store_sample_ok` is **not in D11**, and it is here because every gauge above it
is a *sample*: one that stopped being refreshed looks identical to one whose value stopped
changing. When a sample fails the gauges keep their previous values rather than being zeroed,
because "the queue is empty" is the single most misleading thing this relay could claim while
its store is unreachable. This is the metric that says the numbers are stale.

## Slack authentication

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `alertthread_slack_auth_valid` | gauge | — | `1` if Slack accepted the bot token at the last check |

**Not in D11.** The relay calls `auth.test` at startup and then re-checks every
`slack.auth_probe_interval` (default 15 minutes). 96 calls a day is negligible.

The startup check splits on *why* it failed rather than on whether it failed. A token Slack
definitively rejects refuses to start; a Slack the relay cannot reach retries for up to
[`slack.auth_startup_grace`](configuration.md#slackauth_startup_grace) and then starts anyway
with this gauge at `0`. The reasoning is the same as the readiness argument below: a relay
that will not start during a Slack outage loses every alert fired during it, from a condition
the outbox exists to survive.

### Why a revoked token does not make the relay unready

This is a **deliberate divergence from ADR 001 D11**, which says `/readyz` checks "store
reachability and Slack auth validity". Only the store half is implemented.

Readiness controls whether the pod receives webhooks. If the bot token is broken the correct
behaviour is to **accept** the webhook, persist it to the outbox and retry — that is what the
outbox is *for*. Going unready makes Alertmanager's POST fail; it retries a few times, gives
up, and **the alert is lost**. That is silence, the one failure mode this project treats as
unacceptable.

It is worse with replicas. Every pod shares one token, so a revocation flips them all unready
at once, and a condition the outbox was designed to ride out becomes a total refusal to
ingest. There is no healthy pod to route to, so shedding traffic fixes nothing.

So the token is watched, and the answer is a metric. Alert on
`alertthread_slack_auth_valid == 0`.

## The shipped alert rules

`deploy/alertthread.rules.yaml` is a Prometheus rules file covering every metric above that
means something is wrong. `promtool check rules` validates it (`just check-rules`, and its own
CI job), and a unit test asserts that every metric and label value it names is one this build
actually exports — a rule on a metric that does not exist never fires and looks exactly like a
healthy relay.

`AlertthreadOutboxNotDraining` is the primary one, on
`alertthread_outbox_oldest_age_seconds`, for the reason at the top of this page.

## ⚠️ The relay cannot alert on itself through itself

If the relay is down, an alert about the relay being down cannot be delivered *by* the relay.
The shipped rules **must** be accompanied by an Alertmanager route sending them to a **direct
Slack receiver**, bypassing the relay entirely — match on `alertname=~"Alertthread.*"`, which
no aggregation can drop, or on `job=alertthread`.

Shipping the rules without that route is worse than shipping no rules at all, because it
creates the appearance of monitoring where there is none. This is the single most important
operational note in the project, and [Alert on the relay](../how-to/alert-on-the-relay.md) is
the page that walks it — including how to prove the bypass works before you need it.
