# Fingerprint correlation

*Why the relay keys on `(fingerprint, channel)`, what that buys, and where it can still
lose track.*

Alertmanager posts one Slack message when an alert starts firing and a completely unrelated
one when it resolves. Nothing links them. This page explains the identity the relay uses to
link them instead, and why that identity is the one it is.

Decisions referenced here are recorded in [ADR 001](../adr/001-adr.md) — D3 (idempotency),
D5 (storm collapse), D8 (routing), D9 (failure semantics).

## The fingerprint is the only stable thing in the payload

Alertmanager computes a `fingerprint` from an alert's full label set and puts it in the
webhook body. It is the same value in the firing notification and in the resolved
notification for that alert, hours or days apart.

Nothing else in the payload has that property:

| Candidate | Why it does not work |
|---|---|
| Message text | Rendered from annotations, which rules and templates change |
| `alertname` plus `severity` | Fifteen `KubePodNotReady` alerts share both |
| Slack message timestamp | Assigned by Slack, and only exists after we have posted |
| `startsAt` | Stable, but not unique across alerts that fire together |
| `groupKey` | Identifies the *group*, not the alert |

The PRD's original framing — correlate via the fingerprint, "not by matching on
alertname/labels text, which is fragile" — is the whole of it. Label-text matching breaks
the first time somebody edits a rule's `summary` annotation, and it breaks *silently*,
which in an alerting relay is the failure that matters.

The one thing worth knowing about fingerprints is that they are derived from the
**complete** label set. Two pods failing the same way get different fingerprints because
`pod` is a label. That is exactly what makes per-alert messaging possible, and it is also
why a fifteen-pod group becomes fifteen messages unless something intervenes — which is
what storm collapse, below, is for.

## Why the channel is part of the key

The PRD sketched the state as `fingerprint -> {channel, message_ts}`. The relay stores
`(fingerprint, channel) -> …` instead.

The difference only matters if the same alert is ever routed to two channels — say a
`severity: critical` rule that goes to both `#alerts-critical` and a team channel. Under a
fingerprint-only key the second route overwrites the first one's stored timestamp, and the
resolution then updates one message and quietly abandons the other. The abandoned message
stays red forever: precisely the bug this project exists to fix, reintroduced by the key.

Adding the channel costs one column and removes the failure mode. It was not found in
production — it was noticed while writing the schema, and the cheap fix was taken.

## Correlation has to survive concurrency, and Slack will not help

Slack's Web API has **no idempotency key** on `chat.postMessage`. Send the same request
twice and you get two messages. There is no header, no client-supplied token, no
deduplication window. Every duplicate therefore has to be suppressed on our side, *before*
the call.

Duplicates arrive from two independent directions at once:

- **Alertmanager retries.** If our handler is slow, Alertmanager times out and re-sends the
  same batch. It has no way to know we already accepted it.
- **Replicas.** In the PostgreSQL configuration there are several relay instances, and
  Alertmanager may deliver overlapping batches to different ones.

The relay does not reason about either. It makes the claim a single atomic statement
against the primary key:

```sql
INSERT INTO alert_message (fingerprint, channel, state, …)
VALUES (?, ?, 'claimed', …)
ON CONFLICT (fingerprint, channel) DO NOTHING
RETURNING id;
```

A returned row means we created it and we own the notification. No row means somebody else
does. The database serialises the conflict, so nothing above it has to. This is why the
correctness argument for concurrency is short: there is one statement to be right about,
and both backends have it.

That claim is also why the decision logic can be pure. It is the part that *must* touch a
database, so it runs first, in the shell; its result is then a fact, and everything
downstream is a function of that fact. See [why an outbox](why-outbox.md) for the rest of
that sequence.

## Storm collapse does not weaken correlation

When one delivery would produce more than `collapse_threshold` new messages in a channel,
the relay posts a single group summary and threads the individual alerts under it (D5). It
would be easy to read that as trading correlation for tidiness. It does not.

Each collapsed alert still gets its own row and its own message, with `thread_parent_ts`
recording where that message lives. A resolution still finds the alert by fingerprint and
still edits *that alert's* message in place. Only the visual placement changed: the message
is a thread reply rather than a top-level post.

Collapse is **sticky** for the same reason. Once a group has a parent message, later alerts
joining that group thread under it even when they arrive one at a time. Without stickiness,
whether an alert appeared in the channel or in the thread would depend on how Alertmanager
happened to batch it — the same alert class rendered two ways on different days, which is
worse than either behaviour applied consistently.

Setting `collapse_threshold: 0` turns all of this off, stickiness included. That is a
deliberate reading of D5's word "entirely": a setting that stops *new* groups forming while
existing ones keep collecting members would leave collapse half-on with nothing in the
config to say so. The visible cost is that a parent posted before the setting changed keeps
whatever count it last displayed. A stale message, not a lost one.

## Where correlation is lost, and how you find out

Correlation depends on the relay having a row for the fingerprint. Three things remove that
row, and all three surface identically: a `resolved` notification arrives with nothing to
correlate to.

The relay never drops such a notification. It posts a standalone resolved-style message and
increments `alertthread_orphan_resolves_total` (D9, PRD §5.5). Degraded, never silent.

The causes, in the order worth checking:

1. **The relay was down when the alert fired.** Expected, self-correcting, and correlated
   with a restart.
2. **State was pruned or lost.** Rows past the retention window are deleted; a SQLite
   deployment that lost its volume starts empty.
3. **Alertmanager truncated the firing notification out of the webhook body.** This is the
   one that does not announce itself.

### `truncatedAlerts`, and why it is modelled rather than only documented

Alertmanager's `webhook_config` has a `max_alerts` setting. Any non-zero value makes it
**drop alerts from the body** rather than splitting the delivery:

```go
if maxAlerts != 0 && uint64(len(alerts)) > maxAlerts {
    return alerts[:maxAlerts], uint64(len(alerts)) - maxAlerts
}
```

The dropped alerts are never delivered, so the relay never tracks them, so their eventual
resolutions arrive as orphans. ADR 001 D8 records the consequence, and why it is nasty: the
symptom — degraded correlation, possibly weeks later — *points nowhere near the cause*,
which is one line of configuration on a different machine. Nobody investigating "why do
resolved messages keep appearing on their own?" starts by suspecting the sender's alert cap.

D8's mitigation was a warning in the troubleshooting docs. That is worth having and it is
not enough, because it only helps somebody who already suspects the right thing.

Alertmanager, it turns out, says so directly. The same function that trims the array sets
`truncatedAlerts` in the payload to the number of alerts it removed. **A non-zero
`truncatedAlerts` is not evidence of the misconfiguration; it is the misconfiguration,
reported by the sender, in the same request.**

So the relay models the field, and `plan()` emits a notice for any non-zero value that the
shell turns into a warning and a metric. Detection is exact and immediate, and needs no
inference from a rising orphan counter. The alerts that *did* arrive are delivered normally
— truncation is reported, never fatal.

The general shape is worth naming, because it recurs: **when the system upstream of you
already knows something went wrong, read what it tells you rather than inferring it from
the damage downstream.**

## What this does not solve

One window stays open, deliberately. A worker can post to Slack and crash before recording
the message timestamp. The Slack call and the local commit cannot be made atomic — Slack
has no two-phase commit and no idempotency key — so on retry that alert is posted twice.

The window is milliseconds wide and the direction of the failure is chosen: **duplicate,
never silence** (D3). A duplicate message is a nuisance somebody notices and ignores. A
dropped alert is an outage nobody hears about.
