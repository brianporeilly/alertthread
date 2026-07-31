# Troubleshoot

Symptom first, cause second. Every heading below is something you can observe from Slack, a
dashboard or `kubectl`; the fix is under it.

If you have one metric to look at, it is `alertthread_outbox_oldest_age_seconds`. It is the
only one that means **"alerts are not reaching Slack"** — every other number here is a cause
that may or may not be affecting delivery right now.

```bash
kubectl -n observability port-forward deploy/alertthread 8080:8080
curl -s localhost:8080/metrics | grep -E 'oldest_age|dead_letter|auth_valid|sample_ok'
```

`/metrics` needs no credentials even when the webhook does, so this works on a hardened
deployment.

---

## Nothing has appeared in Slack at all

Work down this list; it is ordered by how often each one is the answer.

1. **Is Alertmanager sending?** Its `/#/status` page shows the loaded config. Check the route
   actually reaches the `alertthread` receiver, and that the receiver's `url` has the
   `?channel=` you expect.
2. **Is the relay refusing the delivery?**
   `alertthread_webhook_requests_total{outcome="auth_missing"}` or `{outcome="auth_mismatch"}`
   climbing means it is answering `401` — see
   [below](#the-relay-is-answering-401-to-alertmanager).
3. **Is the queue draining?** `alertthread_outbox_oldest_age_seconds` climbing means the
   deliveries arrived and are stuck. Carry on to the next section.
4. **Is the token good?** `alertthread_slack_auth_valid` is `0` if Slack is rejecting it.
5. **Is the bot in the channel?** Slack answers `not_in_channel` for a channel the bot was
   never invited to. That is terminal, so those operations park immediately — look for
   `alertthread_dead_letter_total{reason="channel_unusable"}`.
6. **Is the channel name right?** `?channel=%23alerts`, percent-encoded. A literal `#` in a
   URL is a fragment and never reaches the relay, which then uses
   `slack.default_channel` — so alerts appear, in the wrong channel.

## `alertthread_outbox_oldest_age_seconds` is climbing

Deliveries are being accepted and not delivered. The relay is doing its job as far as the
outbox and no further, and this is the one condition worth paging on.

| Also true | Cause | Fix |
|---|---|---|
| `alertthread_slack_auth_valid` is `0` | The bot token is rejected or Slack is unreachable | Replace the token; parked work is revived automatically when it starts working |
| `alertthread_slack_calls_total{outcome="rate_limited"}` rising | Slack is throttling | Expected during a storm. If you run replicas, set `slack.rate_limit_divisor` to the replica count |
| `alertthread_rate_limited_total{source="local"}` rising, `{source="slack"}` flat | The relay is pacing *itself* | Also expected: Slack allows ~1 `chat.postMessage` per second per channel, so a 15-alert batch takes 15 seconds to come out. Nothing is wrong |
| `alertthread_slack_calls_total{outcome="transport"}` rising | Egress is blocked | Check the NetworkPolicy and DNS; `slack.base_url` must be reachable from the pod |
| `alertthread_store_sample_ok` is `0` | The numbers you are reading are **stale** | The gauges are sampled in the background and keep their last value on failure. Fix the store first, then re-read |
| Nothing else | The queue is filling faster than one message per second per channel can drain | Split across more channels, or accept the latency |

A sustained climb with no dead letters means the queue is draining slower than it fills. A
climb that stops rising but does not fall means the oldest row is stuck behind something that
keeps deferring.

## Every message stays red; resolutions never arrive

Check `send_resolved` on the webhook receiver. It defaults to `true`, but if it is ever set to
`false`, Alertmanager never tells the relay an alert resolved, so every message stays in its
firing state for ever.

```yaml
receivers:
  - name: alertthread
    webhook_configs:
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts
        send_resolved: true      # ← mandatory
```

There is no symptom in the relay's own metrics for this, which is what makes it nasty: from
the relay's side, nothing happened. `alertthread_alerts_received_total{status="resolved"}`
staying at zero while `firing` climbs is the closest thing to a signature.

## `alertthread_orphan_resolves_total` is climbing with no restarts

Check `max_alerts` on the webhook config. It **must** stay at its default of `0`.

```yaml
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts
        send_resolved: true
        max_alerts: 0            # ← any other value truncates the body
```

Any non-zero value makes Alertmanager *truncate* alerts out of the webhook body. Truncated
alerts are never tracked, so their eventual resolved notifications arrive as orphans and
surface as standalone messages with no correlation. The symptom — degraded correlation — points
nowhere near the cause, which is why there is a second metric next to it:
**`alertthread_alerts_truncated_total` is the cause**, reported by Alertmanager itself at the
moment it happens. Correlate the two.

Orphan resolves are also normal in three cases that are not this one:

- after a restart that raced a resolution,
- for an alert whose state was pruned (`storage.retention.resolved`, default 7 days),
- for an alert that fired before the relay was deployed.

A rising count with none of those and no restarts is the `max_alerts` signature.

## The relay is answering 401 to Alertmanager

`alertthread_webhook_requests_total{outcome="auth_missing"}` or `{outcome="auth_mismatch"}` is
climbing, and the relay's log has `refused a webhook delivery` at ERROR.

**These alerts are lost.** Alertmanager does not retry a `401`. Fix it as an incident, not as
housekeeping.

| Outcome label | What it means | Fix |
|---|---|---|
| `auth_missing` | The delivery carried no `Authorization` header, or one that was not a `Bearer` credential | Add `http_config.authorization` to the receiver. If it is already there, something between Alertmanager and the relay is stripping the header |
| `auth_mismatch` | It carried a bearer credential that is not the configured one | The two secrets have drifted — usually a rotated `Secret` with a pod that has not restarted, or a trailing newline in one of them |

Check both sides:

```bash
# What the relay expects — never logged, so read the Secret, not the logs.
kubectl -n observability get secret alertthread-webhook -o jsonpath='{.data.token}' | base64 -d | xxd | tail -2
# What Alertmanager sends.
kubectl -n monitoring get secret alertmanager-config -o jsonpath='{.data.alertmanager\.yaml}' | base64 -d | grep -A3 authorization
```

`xxd` rather than `echo`, deliberately: a trailing `0a` is the single most common cause of
`auth_mismatch`. The relay trims whitespace from a token read via `server.auth_token_file`,
but Alertmanager does not trim what it reads from `credentials_file`.

To confirm the perimeter is behaving from outside the cluster:

```bash
curl -si -X POST localhost:8080/webhook -d '{}' | head -1              # 401
curl -si localhost:8080/healthz localhost:8080/readyz | grep HTTP      # both 200
```

If `/healthz` or `/readyz` returns `401`, that is a bug in this relay, not a configuration
mistake: those endpoints are never authenticated. See
[Harden a deployment](harden-a-deployment.md).

## `alertthread_dead_letter_total` went up

An alert was accepted and will not be delivered. This is the counter to page on, and every
increment is an alert nobody was told about.

Two log lines carry the detail. One at the moment the operation parked, with the full payload,
and one from the background reporter that announces **every** parked row once per process —
including on a restart hours later, which is the line most people actually find.

```bash
kubectl -n observability logs deploy/alertthread | grep -i 'dead' | tail -20
```

The `reason` label says which failure it was; [Slack errors](../reference/slack-errors.md) maps
each one to what it means. The common ones:

| `reason` | What happened | What to do |
|---|---|---|
| `invalid_auth` | The bot token is revoked, expired, or missing a scope | Replace it. Everything parked is revived automatically on the next successful probe |
| `channel_unusable` | The bot is not in the channel, or the channel is archived | Invite the bot. **Nothing revives these automatically** (ROADMAP item 14) — the row survives and can be replayed by hand |
| `bad_request` | Slack rejected the message itself — `msg_too_long`, `invalid_blocks`, `no_text` | Almost always a template override producing something enormous or malformed. The verbatim Slack code is in the log line and in the row's `last_error` |
| `slack_unavailable`, `transport`, `http_status` | Retries ran out while Slack was unreachable | The outbox tried ten times over ~30 minutes. Check egress; consider `worker.max_attempts` if your Slack is routinely unavailable for longer |
| `alert_row_missing` | The correlation row was gone when the operation ran | Should not happen; the pruner refuses to delete rows with queued work. Worth an issue |

The label values are a closed set — the same one as `outcome` on
`alertthread_slack_calls_total`, plus `alert_row_missing`. Slack's own error codes are
open-ended and never become label values; they are in the log line and in the parked row's
`last_error` column.

`alertthread_outbox_dead_lettered` is the count of rows still parked. It stays where it is
until something clears them, which makes it the better dashboard panel of the two: the counter
tells you it happened, the gauge tells you it is still true.

## Alerts arrived, but as fifteen separate messages

Storm collapse did not trigger. The threshold is on **new messages produced by one delivery**,
compared strictly greater-than, so `collapse.threshold: 5` leaves a batch of exactly five as
five top-level messages.

More often, the batch was never one batch: Alertmanager's `group_by` decides what arrives
together. Fifteen alerts in fifteen separate groups are fifteen separate webhook deliveries,
and no relay-side setting can collapse them.

```yaml
route:
  group_by: ["alertname", "namespace"]   # what arrives in one delivery
  group_wait: 30s                        # how long Alertmanager waits to batch
```

`alertthread_storm_collapses_total` is how you tell the two apart: if it is not moving, the
relay never saw a batch over the threshold.

## One message, and the rest are missing

The opposite mistake, and it is usually correct behaviour being read wrong. Above the
threshold the relay posts **one summary message** with the individual alerts *threaded
underneath it*. Slack collapses threads by default; click the reply count.

Each threaded child is still correlated on its own fingerprint, so a per-alert resolution
edits that child rather than the summary.

## The relay will not start

It refuses to start for exactly five reasons, and each one names itself in the container's
log. `kubectl logs` before anything else.

| Message names | Cause |
|---|---|
| `no Slack bot token` | Neither `slack.token` nor a readable `slack.token_file` |
| `no default channel` | No `slack.default_channel`, so a delivery with no `?channel=` would have nowhere to go |
| `resolve` / `PolicyError` | `resolve.update_in_place` and `resolve.thread_reply` both `false` |
| `configuration is not valid` with a key name | A misspelled or unknown key in the YAML |
| `Slack rejected the bot token at startup` | `auth.test` returned a definitive rejection — `invalid_auth`, `account_inactive`, `token_revoked` |

That last one is the only startup failure that involves the network, and it is deliberately
narrow: a Slack the relay merely cannot *reach* retries for `slack.auth_startup_grace` and then
starts anyway with `alertthread_slack_auth_valid` at `0`. A relay that refused to start through
a Slack outage would lose every alert fired during it, which is the outcome the outbox exists
to prevent.

If the process starts and then exits, check `server.listen`: a port already in use is fatal by
design, because the alternative is a pod that passes its liveness probe and accepts nothing.

## The relay crash-loops on a read-only filesystem

`readOnlyRootFilesystem: true` with nowhere writable for SQLite. The database, its `-wal` and
its `-shm` all live next to `storage.url`, and that path must be on a writable mount.

```
could not open the sqlite state store: unable to open database file
```

The fix is a declared writable volume, never relaxing the flag —
[Harden a deployment](harden-a-deployment.md) has the manifest. On PostgreSQL there is nothing
to mount: the relay writes to the network and to stdout only.

## Slack messages are ugly, or missing their labels

`alertthread_fallback_posts_total{reason}` tells you whether a template is broken:

- `render_failed` — the override does not render. The ERROR log names the template and the
  line.
- `empty_output` — it rendered nothing, so the hardcoded message was posted instead.

Either way the alert *was* delivered, from the built-in minimal message. A template override
that does not compile at startup is dropped with an ERROR and the built-in kept; the
`ConfigMap` you applied is not in effect. See
[Customize message templates](customize-templates.md).

## Everything looks healthy and you do not trust it

Two checks that do not depend on the relay's own reporting.

**The relay's own alerts.** `deploy/alertthread.rules.yaml` must be loaded *and* routed to a
Slack receiver that bypasses the relay, or the notification that says "the relay is down"
cannot be delivered. See [Alert on the relay](alert-on-the-relay.md); this is the single most
important operational note in the project.

**A watchdog through the relay.** `kube-prometheus-stack` ships an always-firing `Watchdog`
alert. Routing it through the relay to a quiet channel makes the whole path — Prometheus,
Alertmanager, the webhook, the outbox, Slack — self-testing: if the message stops arriving,
something in that chain is broken, and no metric had to be right for you to notice.
