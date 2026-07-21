# ADR 002: Gaps and corrections found while implementing Phases 1–2

**Status:** Accepted
**Date:** 2026-07-21
**Supersedes:** nothing
**Amends:** [ADR 001](./001-adr.md) — D1, D2, D4, D5, D9

---

## Context

ADR 001 was written before any code existed. Phases 1 and 2 — the pure core and the state
store — implemented D2 through D9 and, in doing so, found cases that ADR 001 does not
answer and one supporting claim that is not true.

None of this reverses a decision in ADR 001. Every decision there stands. This ADR records
the cases the original left open, the answers chosen, and one factual correction. It exists
because those answers were reached by an implementer resolving an ambiguity, and an
ambiguity resolved silently in code is indistinguishable from an oversight six months later.

The gaps cluster in a way worth naming up front: **most of them are silence-shaped.** ADR 001
states repeatedly that the worst failure mode is an alert nobody sees, and its failure
taxonomy (D9) is built around that. The cases it missed are, almost without exception, ones
where the natural implementation is a silent no-op. That is not a coincidence — a table of
failure modes is written by imagining failures, and the ones that go missing are the ones
that do not look like failures from the inside.

---

## Part 1 — Silence-shaped gaps in the failure taxonomy

These amend **D9** (failure semantics) and **D2** (ingest classification). Each is a case
where the obvious implementation drops a notification.

### 1.1 A firing delivery arriving against an already-`resolved` row

**Not covered by D2's classification table.**

An alert fires, resolves, and then fires again. The second firing's `INSERT` conflicts with
the surviving row, whose state is `resolved`. D2's table covers `claimed` and `posted`, not
`resolved`.

**Decision: re-claim the row and post a new message.** The alert is genuinely firing again,
and it is a new incident deserving its own message and its own correlation. Updating the old
resolved message in place would rewrite the history of an incident that already ended.

The natural implementation — treating any conflict as "already handled, no-op" — makes a
re-firing alert silent for as long as the pruner retains the row (default 7 days).

### 1.2 A resolution arriving against a dead-lettered post

**Not covered by D9.**

D9 says an outbox op that exhausts its attempts dead-letters. It does not say what happens
when the resolution for that alert then arrives. The `alert_message` row exists, so the
resolve path finds it and tries to update a `message_ts` that was never obtained.

**Decision: treat it as an orphan resolve** — post a standalone resolved message, per
PRD §5.5. Not a duplicate, because no original was ever posted.

### 1.3 `message_not_found` on a group summary message

**D9 covers this for individual alerts and not for group summaries.**

D9 specifies that `chat.update` returning `message_not_found` clears `message_ts` and posts
fresh. Storm-collapse (D5) introduced a second kind of message — the group parent — and the
taxonomy was never extended to it. The natural implementation is a silent no-op: the summary
is "just" a rollup, so failing to update it looks harmless.

It is not harmless. If the parent is gone, its threaded children are orphaned in the channel
with nothing to attach to, and the count of what is still firing stops being maintained.

**Decision: clear the group's `message_ts` and re-queue the summary**, mirroring the
individual-alert behaviour. Symmetry here is not aesthetic — the asymmetry was the bug.

### 1.4 Orphan resolves are top-level and do not count toward collapse

**D5 does not say where an orphan resolve goes.**

**Decision: orphan resolves post at top level and are excluded from the collapse threshold.**

Burying a resolution inside a *firing* group summary hides the message most likely to be
what a reader needs. An orphan resolve already signals that state was lost; making it
harder to see compounds one failure with another. Excluding them from the threshold also
stops a burst of orphans — the exact signature of a relay that just restarted with an empty
store — from collapsing into a summary that obscures every one of them.

---

## Part 2 — Collapse behaviour (amends D5)

### 2.1 `collapse_threshold: 0` disables stickiness as well

D5 says `0` disables collapse "entirely" without saying whether stickiness — the rule that
a group with an existing parent keeps threading under it — survives.

**Decision: `0` disables stickiness too.** "Entirely" is read literally: with collapse off,
no alert threads under a group parent, including groups that acquired one while collapse was
enabled.

**Accepted cost:** a group parent created before the setting changed will keep a stale
member count, since nothing updates it any more. This is preferred to the alternative, where
`0` produces a lasting mixture of top-level and threaded messages that no setting explains.

### 2.2 An unknown `status` value is treated as firing

Alertmanager sends `firing` or `resolved`. ADR 001 does not say what to do with a third value.

**Decision: treat it as firing — post *and* track it.** Tracking matters as much as posting:
a later genuine `resolved` for the same fingerprint then correlates normally, so one
unrecognised value does not orphan every subsequent notification for that alert. The raw
string is surfaced as a notice for logging.

Ignoring an unknown status would drop the alert silently, which is the one outcome this
project does not permit.

---

## Part 3 — Interface refinements

These amend **D2** and **D4**, whose sketches predate any working code.

### 3.1 `plan()` takes group state and returns notices

ADR 001 and ROADMAP sketch `plan(outcomes, batch, policy, now) -> Vec<Op>`. As built:

```rust
plan(outcomes, batch, group: Option<&GroupState>, policy, now) -> Plan
```

- **`group`** — sticky collapse cannot be implemented without knowing whether the group
  already has a parent message. The sketch had nowhere for that state to live.
- **`Plan { ops, notices }`** — truncation, empty batches, unknown statuses and orphan
  resolves all produce perfectly ordinary ops, so they are invisible in an op list. Returning
  the classification alongside stops the shell re-deriving it, which would be a second place
  to be wrong.

### 3.2 Ingest is one store method, not three

D4 sketches `claim_firing`, `mark_resolving` and `enqueue` as separate trait methods.

**Decision: a single `ingest` method** that takes `plan()` as a closure and runs claims,
planning and op persistence in one transaction.

D3's correctness argument depends on the claim and the resulting op being committed
together. Three methods leave that to the caller — which makes the crash window between them
a real state the system can occupy, and one that no test would naturally cover. One method
makes it unrepresentable.

### 3.3 `outbox.dead_lettered_at`

D9 requires dead-lettering; D4's schema has no way to express it. A parked op must be
distinguishable from a merely-deferred one, or the lease query will keep handing it out
forever. Added, with the indexes its queries need.

### 3.4 `truncatedAlerts` is surfaced, not just documented

ADR 001 D8 treats a non-zero Alertmanager `max_alerts` as a documentation problem: it warns
that truncation causes orphaned resolves and that the symptom points nowhere near the cause.

The webhook payload carries a `truncatedAlerts` count. **Decision: model it and emit a
notice**, so Phase 4 can turn it into a log line and a metric.

The general principle, worth stating because it will recur: **when the upstream system
already tells you something broke, read what it says rather than inferring it from
downstream damage.**

---

## Part 4 — Correction to D1

**D1's claim that `sqlx` gives compile-time-checked queries against both backends from one
codebase is not attainable.**

D1 lists it among the reasons to choose Rust. In practice `query!` validates against one
live database and one `.sqlx` cache per crate. A crate implementing both SQLite and
PostgreSQL cannot have both checked without being split in two — which would fragment the
conformance suite, and that suite is a stronger guarantee than the macro provides.

**The decision to use Rust is unaffected** and stands on its other stated grounds: the small
static binary, one codebase across both backends, and a type system that makes "update a
message we have not posted yet" a compile error. Only this one supporting claim is withdrawn.

**Compensating control:** runtime-checked queries, with the dual-backend conformance suite
as the mechanism that actually catches SQL errors. It has already proven stronger than the
macro would have been — it caught a `RETURNING`-clause ordering bug in PostgreSQL that
compile-time checking would not have looked for, and that SQLite hid entirely.

---

## Consequences

- The failure taxonomy in D9 now covers group summaries as well as individual alerts. The
  asymmetry between them was the defect, and symmetry is the invariant to preserve when
  future message kinds are added.
- Four more paths that would have been silent now post something. Every one was found by
  implementing the case, not by re-reading the ADR — which is an argument for phases that end
  in working code rather than in design documents.
- D4's trait is smaller and harder to misuse than its sketch.
- One claim in D1 is withdrawn without disturbing the decision it supported.

## Open questions unchanged by this ADR

ADR 001's three open items still stand: the `collapse_threshold` default of 5, whether group
summaries should list members inline, and whether to publish `alertthread-core` to crates.io.
