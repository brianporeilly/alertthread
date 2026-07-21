# Troubleshoot

*Status: written in Phase 5.*

This guide will map symptoms to causes. Two Alertmanager settings account for a
disproportionate share of the failures, and both are worth stating up front because in each
case the symptom points nowhere near the cause.

## Every message stays red; resolutions never arrive

Check `send_resolved` on the webhook receiver. It defaults to `true`, but if it is ever set
to `false`, Alertmanager never tells the relay an alert resolved, so every message stays in
its firing state forever.

## `alertthread_orphan_resolves_total` is climbing with no restarts

Check `max_alerts` on the webhook config. It **must** stay at its default of `0`. Any
non-zero value makes Alertmanager *truncate* alerts out of the webhook body. Truncated
alerts are never tracked, so their eventual resolved notifications arrive as orphans and
surface as standalone messages with no correlation.

A rising orphan count with no corresponding restarts is the signature of this
misconfiguration.

## Alerts are not reaching Slack at all

`alertthread_outbox_oldest_age_seconds` is the metric that actually means this. It matters
more than any error counter — see [reference/metrics.md](../reference/metrics.md).
