# ADR 003: Divergences and gaps found while implementing Phases 3–5

**Status:** Accepted
**Date:** 2026-07-30
**Supersedes:** nothing
**Amends:** [ADR 001](./001-adr.md) — D4, D9, D11

---

## Context

[ADR 002](./002-implementation-gaps.md) batched what Phases 1 and 2 found. Phases 3, 4 and 5
built the Slack layer, wired the relay end to end, and hardened it — which means they
implemented D9, D10 and D11 for the first time, and put D3 and D4 under a real process that
gets `kill -9`'d.

The house rule is the same one ADR 002 established: **ADR 001 is not rewritten.** Divergences
are recorded, argued and superseded in place. A reader must be able to see what was decided
then, not a tidied version that never disagrees with the code.

Where ADR 002's gaps were all one shape — silence-shaped — these are two.

**A set was enumerated before the question it answers had been asked.** D4's schema sketch and
D11's metric list are both enumerations, and three separate features have since needed a member
neither list has. This happened once already, in ADR 002 §3.3; it has now happened three more
times in the same two sections. Saying it once, as a class, is worth more than saying it three
times (Part 3).

**The guarantee built is not the guarantee described.** Twice — `/readyz` and startup auth —
doing literally what ADR 001 says would have produced the failure ADR 001 exists to prevent.
Once it asks for a mechanism the release profile makes impossible. And once the promise itself
turned out to be narrower than its opening sentence claims. That shape is the reason this is an
ADR rather than a changelog entry (Parts 1, 2 and 4).

---

## Part 1 — D9's central promise now has two documented exceptions

D9's opening sentence is the load-bearing one in the whole document:

> **every degradation path terminates in "post a plain message". Silence is never a valid
> outcome.**

That still holds everywhere below the ingest handler. At the HTTP perimeter it now has two
exceptions. Both lose exactly the alerts in one delivery, both are at the door rather than
inside, and one of them is new.

### 1.1 A `401` on the webhook loses the alerts in that delivery

Phase 5 PR B added the optional bearer token D11's Security section asked for. Alertmanager
retries `5xx` and `429`; it does not retry `4xx`. A refused delivery is never re-sent.

**Decision: refuse anyway, and refuse loudly.** Both alternatives are worse:

- **Accept the delivery and log it.** This makes the setting a lie. An operator who configured
  a credential and sees `200`s has no perimeter and no way to find that out.
- **Answer `503` so Alertmanager retries.** Retrying with the same wrong credential cannot
  succeed. It converts an immediate, diagnosable failure into an hour of retries that also ends
  in loss, with the cause now buried an hour back in the logs.

So the refusal is loud instead of retried: ERROR on every occurrence naming which mistake it
was, two metric label values (§3.3), `AlertthreadWebhookUnauthenticated` in the shipped rules, a
`how-to/troubleshoot.md` section, and warnings in both the reference and the how-to. The token
is off by default, which bounds who can be surprised: the failure mode requires somebody to have
turned it on and then to have got the credential wrong on one side only.

**This is a real narrowing of the project's central promise, and it is worth saying in those
words.** The trade is a delivery lost on misconfiguration in exchange for a perimeter the
operator explicitly asked for. Accepting a delivery that failed authentication is not a trade
this project gets to make on a user's behalf, and there was no third option that kept both.

### 1.2 A `400` on a body this build cannot parse

Older than the `401` and previously recorded only in passing, which is why it is here. An
unparseable body is answered `400`, counted as
`alertthread_webhook_requests_total{outcome="rejected"}`, and logged at ERROR. A retry cannot
fix malformed JSON, so there is nothing worth holding.

The important part is what does *not* trigger it: **unrecognised fields are not a parse error.**
Alertmanager has added fields to the webhook payload before and will again, and answering `400`
because the sender learned a new word would turn a routine upstream upgrade into silence across
every relay in the fleet.

### 1.3 The invariant that actually holds

D9's sentence was written about the delivery path and is still exactly right about it. What
Phase 5 established is that it is not a statement about the whole process. Stated precisely:

> **Once a delivery has been accepted, no path terminates in silence.** Refusal happens at the
> door, is always a `4xx` an operator caused, and is always counted and logged.

Six `outcome` values now separate the two halves — `accepted`, `store_unavailable` and
`misconfigured` mean Alertmanager still holds the alerts; `rejected`, `auth_missing` and
`auth_mismatch` mean it does not. `reference/metrics.md` marks which three mean an alert was
lost, because that is the distinction an operator reading a dashboard needs and it is not
recoverable from the status code alone.

---

## Part 2 — Two places where the letter of ADR 001 would have made things worse

### 2.1 D9's "template rendering panics or errors" — the panic half cannot be caught

D9's row reads *Template rendering panics or errors → fall back to a hardcoded minimal text
message. **Never** drop*, and its emphasis paragraph says templates render in a
catch-and-fall-back wrapper, always.

Half of that is not implementable in anything we ship. `[profile.release]` sets
`panic = "abort"`, so a `catch_unwind` wrapper is dead code in every released binary. Writing
one would have produced the appearance of the guarantee and none of it.

**Decision: accept the divergence and make the stronger guarantee instead.** Phase 3 built:

- **`Renderer::render` has no error return.** The fallback is inside the function and "post
  nothing" is not a value it can produce, so honouring D9 is not a decision left at each call
  site. `Rendered::degraded` reports that the fallback engaged, which drives
  `alertthread_fallback_posts_total{reason}` — handled *and* counted, per AGENTS.md.
- **No rendering path *can* panic**, enforced rather than asserted. The workspace denies
  `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` and `integer_division`; MiniJinja
  reports template failures as `Result`; and the recursion limit is set to 32 so a recursive
  `{% include %}` cannot exhaust the stack, which under `panic = "abort"` would not be an error
  but a dead relay.
- **`UndefinedBehavior::SemiStrict`**, so a mistyped `{{ alert.alertnmae }}` raises and hits the
  fallback rather than rendering as an empty string. Lenient would have posted a message that
  looks subtly wrong with nothing anywhere recording that a template is broken — which is the
  silence-shaped version of a rendering bug.

**Why D9 is not reworded.** D9 decided what must never happen, and it got that right; a broken
template must not be able to take alerting down. What it got wrong is a mechanism, in a sentence
written before the release profile existed. The decision stands, one implementation detail
inside it does not, and the ADR records that rather than pretending it always said `Result`.

### 2.2 `/readyz` deliberately does not check Slack auth, and D11 says it should

D11 reads *`GET /readyz` — readiness. Checks store reachability and Slack auth validity.*

**Decision: `/readyz` checks the store and does not check Slack auth.** Argued and decided in
review before Phase 4 was written, so this is an explicit divergence rather than a quiet one.

Readiness controls whether the pod receives webhooks. If Slack auth is broken, the correct
behaviour is to **accept** the delivery, persist it to the outbox and retry — that is precisely
what the outbox is for. Going unready makes Alertmanager's POST fail; it retries a few times,
gives up, and **the alert is lost**. That is silence, produced by a readiness probe, from the
exact condition D2 spent an outbox to survive. It is worse with replicas, which all share one
token and would therefore all flip unready simultaneously, leaving no healthy pod to shed
traffic to.

The store *is* checked, because a relay that cannot reach its store cannot durably accept a
delivery, and a `200` would then acknowledge an alert it cannot persist. That is the test the
D11 sentence should have applied: **a readiness check asks "can this pod do its job?", and this
pod's job is to accept and persist, not to deliver.** Delivery is asynchronous by decision; a
readiness probe that gates on it is asking the wrong question.

Mid-life token revocation is caught elsewhere. Three mechanisms, three jobs:

| Mechanism | Catches |
|---|---|
| Startup auth (Part 4) | A token that was already bad when the process started |
| `/readyz` on the store | A pod that cannot durably accept a delivery |
| 15-minute prober → `alertthread_slack_auth_valid` | A token revoked while running |

---

## Part 3 — Sets enumerated before the question was asked

D4's schema sketch and D11's metric list were both written before the code that populates them.
Three findings, one shape — and the shape is the finding.

| Missing from | What | Item |
|---|---|---|
| D4's schema | `group_message.group_labels` | 7 |
| D11's metric list | `alertthread_slack_auth_valid` | 9 |
| D11's metric list | `alertthread_webhook_requests_total{outcome="auth_missing"\|"auth_mismatch"}` | 17 |

**Decision in all three cases: add it, record it here, do not rewrite D4 or D11.**

### 3.1 `group_message.group_labels`

D4 sketched the table before the question of *how a group summary names itself* had been asked.
The column is write-once — the labels are what define the group and cannot change while it
exists — and it replaced a function that reverse-engineered a name out of Alertmanager's
group-key serialisation. `commonLabels` and `commonAnnotations` remain deliberately unstored,
because they are recomputed per delivery from current membership and a stale annotation is worse
than an absent one.

Exactly the same class as ADR 002 §3.3's `outbox.dead_lettered_at`: a column D4 has no way to
express, needed by a decision D4 itself made.

### 3.2 `alertthread_slack_auth_valid`

It exists because of §2.2. The prober that replaces the readiness check needs somewhere to put
its verdict, and D11 enumerated the metrics before that question had been asked. Phase 5 gave it
a second job as well: the invalid→valid transition it reports is what triggers dead-letter
revival (§5.1).

### 3.3 The two `auth_*` outcome values

D11 specified the bearer token under *Security* and the metrics under *Observability* and did
not connect the two. Splitting `auth_missing` from `auth_mismatch` is the same argument as
`source` on `alertthread_rate_limited_total` — the operator's next action differs, so the metric
has to as well. `auth_missing` is a receiver with no `authorization:` block, or a proxy stripping
the header; `auth_mismatch` is two secrets that have drifted.

With one extra wrinkle: **the split is deliberately invisible from outside.** Every refusal is a
byte-for-byte identical `401` with a bare `WWW-Authenticate: Bearer`; RFC 6750's
`error="invalid_token"` was rejected precisely because its purpose is to tell the caller which
mistake it made. The operator learns everything, the caller learns nothing, and the metric is one
of the two channels that makes that possible.

### 3.4 The rule worth extracting

**A schema sketch and a metric list are the two parts of an ADR most likely to be incomplete,
because both enumerate the consequences of a decision rather than stating one.** A decision can
be complete on the day it is written; a list of what it will imply cannot be, until it is built.
Neither should be read as closed, and the current answer for both lives in
`reference/configuration.md` and `reference/metrics.md`, which are maintained as part of the
definition of done.

---

## Part 4 — Startup auth, and what "fail fast on a bad token" has to mean

D11's Security section reads *Startup calls `auth.test` once and logs the resolved bot identity,
failing fast on a bad token.*

Read as "refuse to start unless `auth.test` succeeds", it conflicts with what the outbox
promises. A Slack **outage** at the moment the relay starts is not a bad token, and refusing to
start turns a transient upstream condition into a pod that accepts no webhooks at all — the same
silence D9 spends an outbox to avoid, arriving through the front door instead of the back.

**Decision: split on the D9 error taxonomy rather than on "did `auth.test` work".**
`SlackError::disposition` is already the one place in this codebase that answers *will this ever
succeed?*, and startup now asks it.

| Result | Behaviour |
|---|---|
| Accepted | Start; `alertthread_slack_auth_valid = 1` |
| `Disposition::Terminal` — `invalid_auth`, `not_authed`, `account_inactive`, `token_revoked`, `token_expired`, `missing_scope`, a malformed token, an unusable `base_url` | **Refuse to start. No retry at all** |
| Anything else — transport failure, `5xx`, `429`, unrecognised | Retry with bounded backoff for `slack.auth_startup_grace` (default 30 s), then **start anyway** with `alertthread_slack_auth_valid = 0` |

**D11's sentence is preserved exactly.** A bad token still fails fast, with no retry; a test
asserts exactly one `auth.test` call even with a one-hour grace configured, so the refusal is
about the classification and not about running out of time. What changed is that a Slack outage
is no longer classified as a bad token.

The one new config field, `slack.auth_startup_grace`, falls out rather than being invented: the
retry has to be bounded, the bound interacts with a Kubernetes `startupProbe` budget, and a pod
stuck in startup serves neither `/metrics` nor `/readyz` — so "keep trying for ever" would trade
one kind of invisibility for another. `0s` gives exactly one attempt. No setting makes a rejected
token start, and none makes an outage stop a start.

A side effect worth recording: container ordering in the demo stack stops being load-bearing,
because the relay no longer needs Slack to be up before it is.

---

## Part 5 — Recovering an operation that was parked

D9's last row dead-letters an op that exhausts its attempts, counts it and logs the payload at
ERROR. It says nothing about what happens next, and until Phase 5 nothing did: a parked row was
invisible to `lease_batch` for ever, and that ERROR line is one entry in a log-retention window
that is gone by the time anybody is paged. Replacing a revoked token delivered every future alert
and silently wrote off every one that arrived while it was broken — which are precisely the ones
somebody is about to ask about.

Phase 5 added `StateStore::dead_letters`, `StateStore::revive_dead_letters`, and a background
reporter that announces every parked row **once per process** — once per process rather than once
per sweep, because at 15-second intervals a week-old dead letter is fifty thousand identical
ERROR lines, and a signal nobody can read is not a signal. Two limits remain, and both are
decided rather than pending.

### 5.1 Revival is all-or-nothing — accepted

`revive_dead_letters`, fired by the auth prober on an invalid→valid transition, returns **every**
parked row rather than only the ones parked for an auth reason.

**Decision: accept the coarseness.** Filtering would need the low-cardinality reason persisted
per row — an `outbox` column and a `dead_letter` signature change. The coarse version's cost is
bounded and self-correcting: a row parked for a non-auth reason fails once more and re-parks, at
a cost of one Slack call, on an event that only happens when a human has just fixed something.
The cost of getting a *filter* wrong is an alert nobody hears about. That is not a symmetric
trade, and it resolves the way everything in this project resolves.

**Revisit if** a deployment ever accumulates enough permanently-unusable rows for that churn to
matter.

### 5.2 The replay path is a subcommand, not an HTTP endpoint — decided, not built

`channel_unusable` is the case with no probe behind it: an operator invites the bot to a channel
and everything parked before that stays parked, because nothing watches channel membership the
way the prober watches the token. The rows and their payloads survive, so recovery is possible by
hand. There is no supported command for it.

**Decision: `alertthread replay`, a subcommand of the existing binary.** The other candidate was
an `/admin` endpoint behind Phase 5 PR B's bearer token — the perimeter for it already exists, so
the blocker was only ever the decision. Three reasons, in order of weight:

- **No new authenticated mutating HTTP surface on the server that accepts webhooks.** The token
  that exists protects one route whose only job is to accept and persist. A replay endpoint would
  be a second route on the same listener, with materially different consequences, sharing that
  one credential — and the argument in §1.1 for keeping the webhook's perimeter narrow works just
  as well against widening it here.
- **It works on the `scratch` image.** `kubectl exec` into the pod runs the binary that is
  already there; there is no shell and nothing else to add. Authorization becomes "who can exec
  into this pod", which the cluster has already decided, with RBAC and an audit log, and decided
  better than a shared bearer token would.
- **The operator running it has just fixed something by hand and is already at a shell.** The
  ergonomics match the moment it is used in, which is not a moment anybody reaches through a
  browser.

The binary already inspects `argv` before starting the runtime — `--version` has to work on a
`scratch` image with no configuration — so the shape exists.

**It is not implemented.** It gets its own PR. ROADMAP item 14 stays open with the design settled
and the implementation pending.

---

## Part 6 — Two gate decisions

Neither amends ADR 001, whose Testing strategy section predates both. Recorded because both were
settled in review and are exactly the kind of thing that gets re-litigated by the next person to
notice them.

**`just mutants`' exit code is scoped to `crates/core` and `crates/store`.** The recipe still
runs `--workspace` and still prints every survivor on every run; only the exit code narrowed.
Excluding the app crate's survivors by name was considered and rejected: `--exclude-re` asserts
that a mutant is *equivalent*, which is true of the one exclusion the recipe carries and false of
these — they are unkilled, not unkillable, and naming them would also suppress a genuinely new
survivor arriving at the same site. **A gate a correct tree cannot satisfy carries no
information**, people learn to wave it through, and the next real survivor then arrives behind
the ones everybody already ignores. Both directions were watched working before it landed.

**Two CI jobs stay CI's alone.** `just pre-push` is `check-engine`, `check-rules`, `ci`, `image`
and `e2e`; `just test-pg` and the MSRV job are not in it, and **a `pre-push-full` was considered
and rejected**. `test-pg` needs the compose stack up, and folding `just up` in would mean
`just down --volumes` around it — a recipe that eats a developer's dev database to check a gate
is a recipe people stop running. The MSRV job needs a second toolchain installed, and a task
runner that installs toolchains is not a thing anybody wants. Both gaps are named in AGENTS.md
and in `pre-push`'s own closing output, so the limit is stated rather than implied, which is the
honest version of the same information.

---

## Consequences

- **D9's promise is narrower than its first sentence, and now says where.** The invariant that
  holds is §1.3's: once a delivery is accepted, nothing terminates in silence. Both exceptions
  are at the perimeter, both are `4xx`, both are counted, and `reference/metrics.md` marks which
  outcomes mean an alert was lost.
- **D4's schema and D11's metric list have each been extended in two consecutive ADRs.** Treat
  both as open enumerations rather than closed ones, and read the `reference/` quadrant for the
  current answer.
- **Two of the divergences here are cases where following ADR 001 literally would have caused
  the failure ADR 001 exists to prevent.** That is not a criticism of ADR 001 — `/readyz`
  checking Slack auth is the obvious thing to write, and it stops being obviously right only once
  an outbox exists to make the alternative safe. It is an argument for phases that end in a
  running process, which is the next point.
- **Every finding in this ADR was found by building the thing.** ADR 002 made that claim; Phase 5
  is more evidence for it. `panic = "abort"` making `catch_unwind` dead code surfaced while
  writing the wrapper D9 asked for. Read-only rootfs broke Prometheus and Alertmanager in the dev
  stack only when it was actually tried — `tmpcopyup` gives the tmpfs the image directory's
  ownership while both processes run as `nobody`, which no amount of reading would have
  predicted. And mutation testing caught a weak assertion in Phase 5 PR B's *own new code*:
  replacing `Denial::detail`'s return with `"xyzzy"` survived, because the test asserted only
  that the details were non-empty — and those details are the only place the two `auth_missing`
  variants are distinguishable to an operator. `revive_dead_letters` needing to reset the alert
  row's state, not just the outbox row's, is the same story: without it the revived post lands
  and the resolution behind it still arrives as an orphan.

## Open questions unchanged by this ADR

ADR 001's two remaining open items stand: the `collapse_threshold` default of 5, and whether
group summaries should list members inline. Publishing `alertthread-core` to crates.io is a
Phase 6 decision.

Two items are decided here and not yet built, both tracked in `ROADMAP.md`: `alertthread replay`
(§5.2), and the Kubernetes hardening that exists as a documented manifest fragment with nothing
enforcing it until there is a chart.
