# Failure semantics

*Why every degradation path in this system terminates in "post a plain message".*

The governing rule is that **silence is never a valid outcome.** A duplicate message is a
nuisance. A dropped alert is an outage nobody hears about. Wherever the two trade off, this
codebase chooses noise.

That sentence is easy to agree with and hard to hold, because the natural implementation of
almost every failure is a silent no-op. A resolution arrives for something we have no record
of — return early. A template throws — skip the message. An operation runs out of retries —
delete the row. Each of those reads as defensive programming and each of them is an alert
nobody sees. So the failure table in [ADR 001 D9](../adr/001-adr.md) is written the other
way round: every row ends in something appearing in the channel, and the question this page
answers is *why each row terminates where it does*.

## The table, walked

### A resolve arrives for a fingerprint we have never seen

**Post a standalone resolved message.** `alertthread_orphan_resolves_total++`.

There is nothing to update and nothing to thread under, so the only choices are a
context-free green message or nothing at all. The relay posts. Somebody was told an alert
fired — by this relay before it lost state, by a previous relay, or by Alertmanager directly —
and the resolution is the half of the pair that lets them stop looking.

This is also the path that makes the loss of correlation state *survivable*. Retention
deletes resolved state after seven days, a restart can race a resolution, and a non-zero
`max_alerts` truncates alerts out of the webhook body so they were never tracked at all. In
every one of those cases the resolution still lands in the channel; what is lost is the
threading, not the notification. That is the whole design in one row: **degrade the
presentation, never the delivery.**

### `chat.update` returns `message_not_found`

**Forget `message_ts` and post a fresh message.**

The message we were going to edit is gone — deleted by a human, or from a channel the bot was
removed from and re-added to. Retrying cannot help: the timestamp will never exist again.
Treating it as terminal and stopping would leave a resolved alert with no resolution visible
anywhere, so the row's `message_ts` is cleared in the same transaction that queues a
replacement post. The clearing matters as much as the posting: a `message_ts` left behind
would make every future operation on that alert fail the same way, for ever.

### `chat.update` returns anything else

**Retry with backoff; on exhaustion, post a standalone message.**

Everything that is not "the message is gone" might succeed later, so it backs off. The
interesting half is the exhaustion case: after ten attempts the relay stops trying to edit
and posts instead. A message that says "resolved" next to one that still says "firing" is
confusing. A message that says "firing" for ever, about something that resolved an hour ago,
is worse — it is the exact bug this project exists to fix, arrived at by a different route.

### A resolve arrives while `message_ts` is still `NULL`

**Self-defer with backoff; on timeout, post standalone.**

The firing message is queued but has not been sent yet, so there is no timestamp to edit.
Deferring is right because the timestamp is *about to* exist — usually within a second — and
the resolution will then edit the message that is already in the channel, which is the
behaviour the whole project is for. But the deferral is bounded: if the post never lands,
waiting for ever is silence with a plausible explanation. Past the bound, the resolution
posts on its own.

Note what the ordering guarantees here. The post and the resolve are separate outbox rows for
the same fingerprint, drained oldest-first per channel, so the post normally wins the race by
construction. The deferral covers the case where it does not — a rate-limited channel, a
retrying post — rather than being the primary mechanism.

### A template panics or errors

**Fall back to a hardcoded minimal message. Never drop.**

A user-supplied template ([D10](../adr/001-adr.md)) is the most likely thing in a running
deployment to break, because it is the only part an operator edits with no compiler in the
way. It must not be able to take alerting down, so rendering is always wrapped, and a failure
produces a fixed message built in Rust from the alert's own fields —
`alertthread_fallback_posts_total{reason}` counts it.

There is a divergence worth naming here, recorded as ROADMAP known open item 4. D9 says
rendering "panics or errors", which implies catching a panic. The release profile sets
`panic = "abort"`, so `catch_unwind` is dead code in every shipped binary. What is built
instead is stronger than what D9 describes: no rendering path *can* panic, enforced by the
workspace's lint denials on `unwrap`, `expect`, indexing and integer division. The fallback
still exists for the errors — a template that will not compile, one that renders empty output
— and is tested by feeding the renderer a deliberately broken template.

### The store is unreachable at ingest

**Return `503`.** The one row where refusing the request is correct.

This is the exception that proves the rule rather than breaking it. Everywhere else the relay
holds the alert because it can do so durably; here it cannot. A `200` would tell Alertmanager
the delivery is safe when nothing has been written, and Alertmanager would never send it
again. A `503` hands the alert back to a component whose retry is more durable than anything
this relay could offer with its database gone.

The same reasoning is why `/readyz` checks the store and `/healthz` does not, and why
`/readyz` deliberately does **not** check Slack auth. Readiness controls whether this pod
receives webhooks at all; going unready over a broken bot token would make Alertmanager's POST
fail, and it would give up after a few retries — silence produced by a condition the outbox
was specifically designed to survive. The argument in full is in
[HTTP API](../reference/http-api.md) and [Metrics](../reference/metrics.md); it is recorded as
a deliberate divergence from D11 in ROADMAP known open item 8.

### Slack returns 429

**Honour `Retry-After`, and do not count it as a failed attempt.**

Rate limiting is not the operation failing; it is Slack telling us when to come back. Burning
an attempt on it would march an alert toward the dead-letter queue for the crime of arriving
during a storm — precisely when it matters most, and precisely when 429s are most likely. The
relay's own token bucket works the same way: an operation the bucket holds back is deferred
with its attempt returned.

The deferral goes through the outbox rather than through a sleep. A worker that slept past its
lease would let a second worker reclaim the row and post it, and then post it again itself.

### Slack returns a 5xx

**Exponential backoff with jitter, up to `worker.max_attempts`** — ten by default, about half
an hour.

The jitter is deterministic (±12.5%, keyed on the attempt number) so that a hundred operations
deferred by one outage do not all come back in the same millisecond and re-create the outage
they were waiting out.

### Slack returns `invalid_auth`

**Dead-letter immediately. Do not burn retries. Fire a metric.**

The taxonomy in [Slack errors](../reference/slack-errors.md) splits every failure into "will
this ever succeed?" — the same question startup asks. A revoked token will not become valid
by being retried, so nine more attempts over half an hour only delay the moment somebody finds
out. Parking it immediately puts the alert in front of a human sooner, which for a terminal
error is the only thing that helps.

Note that this classification is what stops a Slack *outage* being treated as a bad token. A
transport error or a 5xx is retryable and stays in the queue; only a definitive rejection
parks. Startup makes the same split — see [`slack.auth_startup_grace`](../reference/configuration.md).

### An operation exhausts its attempts

**Dead-letter, `alertthread_dead_letter_total{reason}++`, log the full payload at ERROR.**

This is the end of the line, and it is the one place where the rule genuinely runs out: an
alert has been accepted and will not be delivered. Since the outcome cannot be made
acceptable, everything here is about making it *impossible to miss*:

- The row stays in the `outbox` table. It is the only record that the alert existed, and
  deleting it would erase the evidence of the one failure this project treats as unacceptable.
- The payload is logged at ERROR when it parks, and every parked row is announced again at
  ERROR **once per process** by a background reporter. The second line is the one that
  matters: the first is out of your log retention by the time anybody is paged, and it does
  not come back on a restart. The reporter's does.
- `alertthread_dead_letter_total` is the counter to page on. Every increment is an alert
  nobody was told about.
- The alert's row is marked `failed`, so its eventual resolution posts as an orphan rather
  than trying to edit a message that was never sent.

And parking is not necessarily permanent. When the background auth probe sees the token go
from rejected to accepted, everything parked is returned to the queue with a full attempt
budget — `alertthread_dead_letter_revived_total` counts it. Those alerts are late. Late is the
point, because the alternative is never. The revival is all-or-nothing rather than filtered by
reason, which is a deliberate trade recorded as ROADMAP known open item 13; a row parked for a
reason the token has nothing to do with fails once more and re-parks, at a cost of one Slack
call, on an event that only happens when a human has just fixed something.

What nothing yet recovers is a row parked for `channel_unusable` — the bot was not in the
channel, somebody has since invited it, and no probe watches channel membership the way the
auth probe watches the token. The row and its payload survive and can be replayed by hand;
there is no supported command for it. That is ROADMAP known open item 14, and it is the
honest current limit of this page's claim.

## The two paths that lose an alert on purpose

Both are worth stating plainly, because a document that claims nothing is ever lost stops
being useful the moment somebody finds a case.

**A body this build cannot parse** is answered `400`, counted as
`alertthread_webhook_requests_total{outcome="rejected"}` and logged at ERROR. A retry cannot
fix malformed JSON, so there is nothing to hold. Note that *unrecognised fields* do not cause
this: Alertmanager has added fields to the webhook payload before and will again, and
answering `400` because the sender learned a new word would turn an upgrade into silence.

**A delivery that fails the bearer-token check** is answered `401` and counted as
`auth_missing` or `auth_mismatch`. Alertmanager does not retry a `401`, so those alerts are
lost too. This one is an operator misconfiguration rather than a relay failure — the token is
off by default, and turning it on means putting the same credential on the receiver — but the
consequence is identical, which is why the refusal is logged at ERROR and has its own alert
rule in `deploy/alertthread.rules.yaml`. The alternative, accepting a delivery that failed
authentication, is not a trade this project gets to make on a user's behalf.

## The one genuinely unresolvable case

A worker posts to Slack and the process dies before it commits the returned timestamp. The
window is microseconds wide and it cannot be closed: Slack has no idempotency key on
`chat.postMessage`, so there is no way to ask "did my last message land?" and no way to make
the send and the commit atomic across two systems.

The relay resolves it toward duplication. The next worker leases the row, posts again, and the
channel gets two identical firing messages — the second of which is the one that gets the
timestamp, so the alert still goes green on resolve. This is enumerated rather than hidden, in
[ADR 001 D3](../adr/001-adr.md), and it is asserted by a test rather than merely accepted:
`a_post_that_reached_slack_before_the_crash_comes_back_as_a_duplicate_not_a_silence` counts
the two messages and says so in its name.

Choosing the other direction would mean marking the row done before Slack confirmed it, which
converts a rare duplicate into a rare silence. That is the trade this project never makes.

## Where to look next

The full table is [D9](../adr/001-adr.md). What each Slack error code does is
[Slack errors](../reference/slack-errors.md). If something is wrong right now,
[Troubleshoot](../how-to/troubleshoot.md) is organised by symptom.
