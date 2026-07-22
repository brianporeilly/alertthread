# HTTP API

## Inbound

`alertthread` exposes four endpoints and nothing else. There is no admin API and no
management port.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/webhook` | Alertmanager webhook receiver |
| `GET` | `/healthz` | Liveness |
| `GET` | `/readyz` | Readiness |
| `GET` | `/metrics` | Prometheus exposition |

### `POST /webhook`

Takes an Alertmanager `webhook_config` v4 body. Optional query parameter:

| Parameter | Type | Meaning |
|---|---|---|
| `channel` | string | Where to post. `#alerts` must be percent-encoded as `%23alerts` |

The channel resolves as `?channel=` → `slack.default_channel` → the process would not have
started ([ADR 001 D8](../adr/001-adr.md)). A `?channel=` that is present but blank counts as
absent: a receiver URL rendered from a template with an unset variable produces exactly that,
and posting to a channel named `""` earns `channel_not_found`, which is terminal, which is a
dead-lettered alert.

```
receivers:
  - name: alertthread
    webhook_configs:
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts-critical
        send_resolved: true
```

| Status | Body | When | What Alertmanager should do |
|---|---|---|---|
| `200` | `ok` | The delivery is committed to the store | Nothing |
| `400` | the parse error | The body is not an Alertmanager payload this build can read | Nothing — a retry cannot fix it |
| `503` | `could not persist the delivery; retry` | The store was unreachable | **Retry** |
| `500` | `no channel: …` | No `?channel=` and no default | Retry; the relay is misconfigured |

**`200` means durable, not delivered.** The claim, the plan and the outbox rows are committed
in one transaction before the response is written, and no Slack call happens in the handler
at all — target p99 is 50 ms. A crash the instant after the `200` loses nothing; the worker
picks the rows up.

`503` is the one case where refusing the request is correct
([ADR 001 D9](../adr/001-adr.md)): Alertmanager's own retry is more durable than anything the
relay could do with an unreachable store, and a `200` would acknowledge an alert nothing has
persisted.

`400` is the only status that loses an alert, and it is counted
(`alertthread_webhook_requests_total{outcome="rejected"}`) and logged at `ERROR`. Note that
**unrecognised fields do not cause it**: Alertmanager has added fields to this payload before
and will again, and answering `400` because the sender learned a new word would turn an
upgrade into silence.

⚠️ Two Alertmanager settings break correlation if changed. `send_resolved` must stay `true`,
or the relay never learns about resolutions and every message stays red for ever.
`max_alerts` must stay `0`, or alerts are truncated out of the body, never tracked, and their
resolutions arrive as orphans. See [Troubleshoot](../how-to/troubleshoot.md).

### `GET /healthz`

Liveness. Always `200 ok` while the process is running.

**Deliberately does not check the store.** A brief database blip must not cause Kubernetes to
restart a pod that is correctly buffering alerts — the outbox is exactly the machinery for
riding that out, and restarting throws away the in-flight leases that machinery depends on.

### `GET /readyz`

Readiness. Checks that the state store answers a query.

| Status | When |
|---|---|
| `200 ready` | The store answered |
| `503 the state store is not reachable` | It did not |

The probe is a primary-key lookup for a fingerprint nothing can ever have. It is the cheapest
query that still proves the whole path works — a connection, the `alert_message` table and
the row decoder — where a bare `SELECT 1` would prove only the connection and would keep
answering `200` after a failed migration.

#### It does **not** check Slack auth — a divergence from ADR 001 D11

D11 specifies "store reachability and Slack auth validity". Only the first is implemented,
deliberately.

Readiness controls whether the pod receives webhooks. If the bot token is broken the correct
behaviour is to **accept** the webhook, persist it and retry — that is what the outbox is
*for*. Going unready makes Alertmanager's POST fail; it retries a few times, gives up, and
the alert is lost.

With replicas it is worse: every pod shares one token, so a revocation flips them all unready
at once and there is no healthy pod to route to. Shedding traffic fixes nothing.

The store is different and does belong here: if the store is unreachable the relay cannot
durably accept a webhook, so a `200` would acknowledge an alert it cannot persist.

Token validity is watched by a background prober and reported as
`alertthread_slack_auth_valid` — see [Metrics](metrics.md).

### `GET /metrics`

Prometheus exposition, `Content-Type: application/openmetrics-text; version=1.0.0;
charset=utf-8`.

Serves the in-memory registry and nothing else: **this handler never queries the store.** The
store gauges in it are sampled on a background interval. See [Metrics](metrics.md) for why.

## Shutdown

`SIGTERM` or `SIGINT` stops the listener and drains. The worker finishes the batch it is
holding rather than abandoning its leases — an abandoned lease is not a bug, it expires and
is reclaimed, but waiting a full `worker.lease` is time an alert spends undelivered for no
reason.

## Outbound

`alertthread` calls exactly three Slack Web API methods, and no others. There is no SDK; the
client is written directly on `reqwest` ([ADR 001 D1](../adr/001-adr.md)).

| Method | When | Rate limit |
|---|---|---|
| `chat.postMessage` | A new alert message, a storm-collapse group summary, a threaded reply, an orphan resolve | Special Tier — ~1/sec **per channel**, thread replies included |
| `chat.update` | Resolve-in-place, repeat-firing refresh, group summary count refresh | Tier 3 — ~50/min per workspace |
| `auth.test` | Once at startup, and every `slack.auth_probe_interval` thereafter | Tier 3 |

Requests are `POST`s with a JSON body, `Authorization: Bearer xoxb-…` and
`Content-Type: application/json; charset=utf-8`. Each call is exactly one HTTP round trip:
the client never retries or sleeps internally, including on 429.

The relay paces itself ahead of those limits with a token bucket — per channel for
`chat.postMessage`, per workspace for `chat.update`, matching how Slack scopes each one. A
call the bucket holds back is deferred in the outbox with its attempt returned, not slept on:
a worker that slept through a `Retry-After` longer than its lease would let a second worker
reclaim the row and post it, and then post it again itself.

**Slack answers application errors with HTTP 200 and `{"ok": false, "error": "…"}`.** The
relay treats the `ok` field, not the status code, as the outcome. Every error code and what
the relay does about it is in [Slack errors](slack-errors.md).

## Background tasks

Not endpoints, but part of the running process and worth knowing about when reading its logs.

| Task | Interval | What |
|---|---|---|
| Outbox worker | `worker.idle_poll` | Leases a batch, drains it by channel |
| Metrics sampler | `worker.sample_interval` | Reads the store's queue depth and correlation-state size |
| Retention pruner | `storage.retention.interval` | Deletes finished state ([ADR 001 D4](../adr/001-adr.md)) |
| Auth prober | `slack.auth_probe_interval` | Re-checks the bot token |

All four stop on the same shutdown signal, and none of them waits out its interval first — an
hourly pruner that only checked after sleeping would keep a container alive for up to an hour
past `SIGTERM`, and Kubernetes would `SIGKILL` it instead, mid-delivery.
