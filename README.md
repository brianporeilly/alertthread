# alertthread

**Alertmanager → Slack relay with fingerprint-keyed threading and update-on-resolve.**

Alertmanager's Slack receiver posts an independent message for every notification. A firing
alert and its later resolution arrive as two unrelated messages, often hours and hundreds of
lines apart, and nothing in the channel connects them. `alertthread` sits between
Alertmanager and Slack and correlates them by alert fingerprint, so a resolution **updates
the original message in place and threads a reply under it** rather than posting somewhere
new.

```
┌──────────────────────────────────┐
│ ✅ RESOLVED   CephOSDDown        │   ← the original message, edited red → green
│ osd.3 · ceph-node-2              │
│ fired 14:02 → resolved 14:31     │
│ duration 29m                     │
└──────────────────────────────────┘
  └─ 💬 1 reply
     ✅ Resolved after 29m          ← the reply is what makes it visible live
```

Both halves are needed. `chat.update` does not bump the message, mark the channel unread, or
notify — so an in-place edit alone is invisible to anyone watching. The threaded reply
generates the unread indicator without re-posting to the channel.

> ⚠️ **Status: under construction.** Phase 0 (foundations) is complete — workspace, gates,
> CI, and a validated `scratch` container image. The relay itself is not implemented yet.
> See [`ROADMAP.md`](ROADMAP.md) for the phased plan.

## Why this exists

There is sustained upstream demand for this behaviour
([alertmanager#2165](https://github.com/prometheus/alertmanager/issues/2165),
[#3221](https://github.com/prometheus/alertmanager/issues/3221) — 100+ 👍 between them) and
no maintained tool in the niche.

**This is alerting infrastructure. The worst possible bug is silence.** A duplicate message
is a nuisance; a dropped alert is an outage nobody hears about. Every trade-off in this
codebase resolves in that direction — including one known, deliberately accepted case where
a crash at exactly the wrong moment produces a duplicate rather than risking a loss.

## How it works

The webhook handler persists intent and returns `200` in under 50 ms. Background workers
drain a durable outbox and make the Slack calls.

That indirection is not architectural taste; it falls out of two facts:

- **Slack allows roughly one `chat.postMessage` per second per channel**, thread replies
  included. Posting synchronously means a 15-alert group takes a 15-second handler,
  Alertmanager times out, and the same batch ends up in flight twice.
- **A single Alertmanager group can carry a dozen-plus alerts.** Naïve per-fingerprint
  messaging turns one message into fifteen, which is a regression against the problem this
  project exists to solve. Above a threshold, alerts collapse into one summary message with
  the individuals threaded beneath it — each still correlated, so per-alert resolve still
  updates the right child.

Full reasoning is in [ADR 001](docs/src/adr/001-adr.md).

## Configuration sketch

The channel comes from the webhook URL, so Alertmanager keeps owning routing:

```yaml
receivers:
  - name: slack-critical
    webhook_configs:
      - url: http://alertthread.observability.svc.cluster.local:8080/webhook?channel=%23alerts-critical
        send_resolved: true
```

⚠️ **`send_resolved` must be `true`** or the relay never learns about resolutions, and every
message stays red forever.

⚠️ **`max_alerts` must stay `0`.** Any non-zero value makes Alertmanager *truncate* alerts
out of the webhook body. Truncated alerts are never tracked, so their resolutions arrive as
orphans with no correlation — and the symptom points nowhere near the cause.

## ⚠️ The relay cannot alert on itself through itself

If the relay is down, an alert about the relay being down cannot be delivered *by* the
relay. Any `PrometheusRule` for `alertthread` **must** be paired with an Alertmanager route
sending `job=alertthread` alerts to a **direct Slack receiver**, bypassing the relay.

Shipping the rule without that route is worse than shipping no rule, because it creates the
appearance of monitoring where there is none. This is the most important operational note in
the project.

## Storage

SQLite by default, with no external dependency — exactly one replica, enforced at startup.
PostgreSQL is opt-in and enables horizontal scaling. Both are exercised by one shared
conformance suite, so the HA path is continuously verified rather than theoretical.

## Documentation

Written with [Diátaxis](https://diataxis.fr/) and rendered by mdBook:

```
just docs      # build and link-check
```

| | |
|---|---|
| [Tutorials](docs/src/tutorials/) | Teach me this, I'm new |
| [How-to guides](docs/src/how-to/) | I have a specific goal |
| [Reference](docs/src/reference/) | What are the exact options? |
| [Explanation](docs/src/explanation/) | Why is it built this way? |

Start with [build and packaging](docs/src/explanation/build-and-packaging.md), which records
what the container image actually measures and why.

## Development

```
just            # list recipes
just test-fast  # inner loop
just ci         # everything CI runs, including the coverage gate
just up/down    # compose dev stack (podman or docker)
```

Requires a Rust toolchain (pinned in `rust-toolchain.toml`) and either podman or
docker. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the full setup, and
[`AGENTS.md`](AGENTS.md) for the constraints this codebase holds itself to.

## Licence

Dual-licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.
