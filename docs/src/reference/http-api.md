# HTTP API

*Status: written in Phase 3–4, as endpoints land.*

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
