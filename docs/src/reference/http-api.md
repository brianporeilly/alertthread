# HTTP API

*Status: the inbound endpoints are written in Phase 4, as they land. The outbound calls
below are complete.*

## Inbound

`alertthread` exposes four endpoints.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/webhook` | Alertmanager webhook receiver |
| `GET` | `/healthz` | Liveness |
| `GET` | `/readyz` | Readiness |
| `GET` | `/metrics` | Prometheus exposition |

This page will document request and response shapes, status codes and the `?channel=` query
parameter in full.

Two behaviours are worth stating here because they are load-bearing rather than incidental:

- **`/healthz` deliberately does not check the store.** A brief database blip must not cause
  Kubernetes to restart a pod that is correctly buffering alerts. `/readyz` is where store
  reachability and Slack auth are checked.
- **`POST /webhook` returns `200` before anything is sent to Slack.** The durable write
  happens before the ack, so nothing is lost; the ack happens before the Slack call, so
  nothing blocks. A `503` means the store was unreachable and Alertmanager should retry —
  the one case where refusing the request is the correct behaviour.

Background is in [ADR 001 D2](../adr/001-adr.md) and
[explanation/why-outbox.md](../explanation/why-outbox.md).

## Outbound

`alertthread` calls exactly three Slack Web API methods, and no others. There is no SDK;
the client is written directly on `reqwest` (ADR 001 D1).

| Method | When | Rate limit |
|---|---|---|
| `chat.postMessage` | A new alert message, a storm-collapse group summary, a threaded reply, an orphan resolve | Special Tier — ~1/sec/channel, thread replies included |
| `chat.update` | Resolve-in-place, repeat-firing refresh, group summary count refresh | Tier 3 — ~50/min |
| `auth.test` | Once at startup, and on every `GET /readyz` | Tier 3 |

Requests are `POST`s with a JSON body, `Authorization: Bearer xoxb-…` and
`Content-Type: application/json; charset=utf-8`. Each call is exactly one HTTP round trip:
the client never retries or sleeps internally, including on 429.

**Slack answers application errors with HTTP 200 and `{"ok": false, "error": "…"}`.** The
relay treats the `ok` field, not the status code, as the outcome. Every error code and what
the relay does about it is in [reference/slack-errors.md](slack-errors.md).

### What `/readyz` checks against Slack

`auth.test`. Ready requires an `ok: true` response carrying a `user_id`; `invalid_auth` and
the rest of the token-rejection family make the process not-ready rather than restarting it.
The same call runs once at startup and logs the resolved bot identity, so a bad token fails
before any alert arrives (ADR 001 D11).
