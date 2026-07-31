# Implementation roadmap

Phased plan for building `alertthread`, the Alertmanager → Slack threading relay.
Architecture is specified in [ADR 001](docs/src/adr/001-adr.md); this document is *how we get
there*, not *what it is*. Where they conflict, the ADR wins.

**Guiding rule:** each phase ends with something that runs and is tested. No phase leaves
the tree in a state where the next phase is the only thing that makes it work.

---

## Current status

*Last updated 2026-07-29. Keep this current — it is the first thing anyone picking the
project up will read, and git log does not distinguish "in review" from "abandoned".*

| Phase | State |
|---|---|
| 0 — Foundations | ✅ merged (#3) |
| 1 — Pure core | ✅ merged (#4) |
| 2 — Store layer | ✅ merged (#5) |
| 3 — Slack layer | ✅ merged (#7) |
| ↳ group labels | ✅ built — `group_message.group_labels`, both backends |
| 4 — Wiring, PR A | ✅ merged (#12) — the walking skeleton |
| 4 — Wiring, PR B | ✅ merged (#13) — mock UI, compose demo, tutorial; the exit criterion is an asserted CI job |
| 5 — Hardening, PR A | ✅ merged (#15) — resilience: crash recovery, storm-under-load, dead letters, startup auth |
| **5 — Hardening, PR B** | 🟡 **in review — webhook bearer auth, container hardening, alert rules, the two Diátaxis pages, `just pre-push`** |
| 5 — closeout | ⬜ not started — ADR 003 batching the known open items below |
| 6 — Release | ⬜ not started |

ADRs [001](docs/src/adr/001-adr.md) and [002](docs/src/adr/002-implementation-gaps.md) are
merged and accepted.

### Built: group labels on `group_message`

Landed before Phase 4, so Phase 4 can wire the renderer to the store and assume the labels
are simply there. `group_message` gained a `group_labels` JSON column on both backends,
written when the group is opened and never rewritten; `alertname_from_group_key` and its
dependency on Alertmanager's group-key serialisation are gone, and templates receive
`group.labels` plus a computed `group.title` that is never empty.

`commonLabels` / `commonAnnotations` remain deliberately unstored: they are recomputed per
delivery from current membership, so written once they go stale, and a stale annotation is
worse than an absent one. `groupLabels` has no such problem — it is what *defines* the
group and cannot change while the group exists. Revisit only with refresh-on-every-delivery
semantics thought through.

---

## Settled decisions

Recorded here so they are not re-litigated mid-build. Rationale lives in ADR 001 and the
sessions that produced it.

| | |
|---|---|
| Name | `alertthread` — binary, image, chart, crates, `alertthread_*` metric prefix |
| Language | Rust, edition 2024 |
| **MSRV** | **1.94** — dictated by `sqlx` 0.9, not chosen |
| Architecture | Functional core / imperative shell, in a 4-crate workspace |
| Docs | Diátaxis, rendered by mdBook, written per-phase not at the end |
| Test env | compose stack on podman *or* docker + `#[sqlx::test]`; no testcontainers |
| Task runner | `just` — **every CI job invokes one of these recipes**; `just pre-push` is the full local set, `just ci` the fast subset |
| Coverage | Enforced per-crate, gating `just test` and `just ci` — see below |
| Licence | Dual `MIT OR Apache-2.0` |

---

## Coverage policy

Coverage gates `just test` and `just ci`. A change that drops a crate below its threshold
fails the build.

**Thresholds are per-crate, not a single workspace number.** A flat percentage across the
workspace is the standard way these gates fail: it lets genuinely critical logic sit
undertested as long as easy-to-cover code drags the average up, and it simultaneously
pushes people to write line-touching tests for `main.rs` to buy back headroom. Tiering by
how testable and how critical each crate actually is puts the strictest bar exactly where
the risk lives.

| Crate | Line coverage | Why that number |
|---|---|---|
| `alertthread-core` | **100%** | It is pure. No I/O, no clock, no runtime. Every branch is reachable with a plain function call, so anything less than 100% means dead code or an untested branch — and this crate holds every correctness decision in the project |
| `alertthread-store` | 95% | Conformance suite covers the trait exhaustively; the residue is driver-level error paths that need fault injection to reach |
| `alertthread-slack` | 95% | `wiremock` covers the API surface; the residue is `reqwest` transport failures |
| `alertthread` (app) | 95% | Handlers, workers, config and the rate limiter are all directly testable. `main.rs` is **excluded** — ~50 lines of wiring and signal handling — rather than papered over with a lower threshold |
| `dev/slack-mock` | excluded | Development tooling, not shipped |

Excluding `main.rs` outright is deliberate and preferable to setting the app threshold low
enough to absorb it. An explicit, justified exclusion is honest; a soft threshold hides how
well the code that *does* matter is covered.

### `alertthread-store` is gated twice, at the same threshold

The store ships two backends behind cargo features, and the tests for one of them need a
PostgreSQL server. `just test` has no containers, so compiling `PostgresStore` into that
build would count its several hundred lines as uncovered and drag the crate under 95% — and
the only ways to make that pass would be to lower the threshold or to blanket-exclude the
file. Neither is acceptable, and a store backend is the last place to do either.

So neither build is asked about code it cannot run:

| Recipe | Compiles | Gated at |
|---|---|---|
| `just test` | `--workspace`, default features → SQLite backend | 95% |
| `just test-pg` | `-p store --no-default-features -F postgres` → PostgreSQL backend | 95% |

`crates/store/src` is measured in both. The shared code — the trait, the outbox payload
format, the row mapping — is exercised by both; each backend is exercised by exactly one.
Nothing ends up unmeasured, and nothing is measured against tests that could not have run.
`scripts/coverage-gate.py --profile store-postgres` is the second gate; CI's `test-pg` job
runs it.

### Coverage is a floor, not the goal

Line coverage proves a line *executed*. It does not prove an assertion would have caught a
regression, and the gap between those two is exactly where alerting bugs live. A test that
runs `plan()` and asserts nothing scores 100%.

So the real quality gate on the core is **mutation testing** via `cargo-mutants`: it
changes the code and checks that a test fails. For a system whose worst failure mode is
silence, "would we have noticed?" is the only question worth asking, and mutation testing
is the only tool that answers it.

Mutation runs are slow, so they are not in the default loop:

- `just mutants` — on demand, and **required for any change to `alertthread-core`**
- Nightly in CI across the workspace
- **It runs the whole workspace and prints every survivor; its exit code covers `core` and
  `store`.** Survivors elsewhere stay visible on every run and are triaged in the PR rather
  than gating it — see known open item 10 for why that is a narrowing and not an exclusion

### Tooling and the inner loop

`cargo-llvm-cov` (with `nextest`) produces the report; a thresholds table is enforced from
its JSON output.

Instrumentation costs roughly 2–3× on test runtime, which is too slow to sit in a
tight edit-test cycle. So there is an explicit escape hatch:

- `just test-fast` — no instrumentation, for the inner loop
- `just test` — instrumented and gated, as the pre-push check
- `just ci` — everything, including coverage and the per-crate thresholds

`just test-fast` is a convenience, not a way to avoid the gate. CI runs `just ci`.

### Pinned dependency versions

Verified against crates.io on 2026-07-21. Several are newer than they may appear from
memory — check before assuming an API.

| Crate | Version | Note |
|---|---|---|
| `axum` | 0.8 | |
| `tokio` | 1.53 | |
| `sqlx` | **0.9** | Big release. Sets MSRV 1.94; `SqlSafeStr` is new |
| `reqwest` | **0.13** | Newer than the common 0.12 |
| `tower-http` | **0.7** | Newer than the common 0.6 |
| `minijinja` | 2.21 | |
| `prometheus-client` | 0.25 | Official Prometheus-org crate |
| `figment` | 0.10 | |
| `thiserror` / `anyhow` | 2.0 / 1.0 | libs / binary respectively |
| `insta` | 1.48 | Block Kit snapshots |
| `wiremock` | 0.6 | Slack API fake |
| `chrono` | 0.4 | See below |

**`chrono`, not `jiff`.** `jiff` is the better-designed library and this is a deliberate
concession. Timestamps cross the core→store boundary constantly, and `sqlx` has
first-class `chrono` support; using `jiff` in the core would mean converting at every
store call — two time types in one codebase is a reliable source of off-by-a-timezone
bugs. Revisit if `sqlx` gains native `jiff` support.

---

## Workspace layout

```
alertthread/
├── Cargo.toml                  # workspace root, [workspace.lints]
├── rust-toolchain.toml         # pinned toolchain
├── justfile                    # the only entry point for fmt/lint/test/run
├── compose.yaml                # dev stack; podman or docker
├── Dockerfile                  # cargo-chef → musl static → scratch
├── AGENTS.md                   # contributor + agent constraints
├── deny.toml                   # cargo-deny: licences + advisories
├── crates/
│   ├── core/                   # alertthread-core   — PURE
│   ├── store/                  # alertthread-store  — StateStore + backends
│   ├── slack/                  # alertthread-slack  — client + rendering
│   └── app/                    # alertthread        — the binary
├── deploy/                     # raw manifests, for Phase 6 to package
│   └── alertthread.rules.yaml  # Prometheus alert rules (ADR 001 D11)
├── dev/
│   └── slack-mock/             # dev-only fake Slack with a web UI
└── docs/                       # mdBook, Diátaxis
```

`deploy/` holds artefacts an operator consumes directly and Phase 6 will wrap in the Helm
chart. It is deliberately *not* a chart yet: a rules file that `promtool` can check and a chart
can embed verbatim is useful now, and inventing half a chart to hold it would be Phase 6 work
done badly.

**Dependency direction, enforced by Cargo rather than by review:**

```
app ──→ store ──→ core
 └────→ slack ──→ core
                  core ──→ (nothing with I/O)
```

`alertthread-core` must not depend on `tokio`, `sqlx`, `axum`, or `reqwest`. This is
checked in CI, not merely documented.

### The shape that makes this work

The valuable logic — ingest classification (ADR D2), storm-collapse (D5), repeat-debounce
(D7) — is one pure function:

```rust
pub fn plan(
    outcomes: &[ClaimOutcome],   // what the DB told us, already resolved
    batch:    &AlertBatch,       // what Alertmanager sent
    policy:   &Policy,           // config
    now:      DateTime<Utc>,     // injected, never read from the clock in here
) -> Vec<Op>
```

The atomic claim (D3) *cannot* be pure — its correctness IS the database atomicity. So the
shell runs claims first and feeds their outcomes in. Sequence per request:

1. **Shell**: open transaction, execute claims, collect `ClaimOutcome`s.
2. **Core**: `plan(...)` decides which `Op`s to emit. No I/O, no clock, no randomness.
3. **Shell**: persist ops in the *same* transaction, commit, return 200.

`plan` is exhaustively testable with zero mocks, no database and no runtime. That is the
entire point of the layout.

---

## Phase 0 — Foundations

Scaffolding only. No relay behaviour.

- `rustup` install, `rust-toolchain.toml` pinned to 1.94+
- Cargo workspace, four empty crates with correct dependency edges
- `[workspace.lints]` — `unsafe_code = "forbid"`, clippy `pedantic` with a documented
  allow-list
- `rustfmt.toml`, `deny.toml`, `.editorconfig`
- `justfile`: `fmt`, `lint`, `test`, `test-fast`, `test-pg`, `coverage`, `mutants`,
  `docs`, `up`, `down`, `ci`
- `cargo-llvm-cov` + `cargo-nextest` + `cargo-mutants` installed and wired
- Per-crate coverage thresholds enforced from `cargo-llvm-cov` JSON output
- GitHub Actions calling those same recipes: fmt, clippy, test, coverage, deny, MSRV,
  docs; plus a nightly `mutants` job
- mdBook skeleton with the full Diátaxis tree; move PRD + ADR to `docs/src/adr/`
- `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `CONTRIBUTING.md`
- `compose.yaml`: postgres + a stub slack-mock
- Dependabot/Renovate config

### ⚠️ Phase 0 must include a build spike

**Validate that `sqlx` + bundled SQLite cross-compiles to `x86_64-unknown-linux-musl` as a
static binary, and produces a working `scratch` image — before writing any code.**

This is the single highest-risk assumption in ADR 001. `libsqlite3-sys` is a C dependency,
and the ADR's "~8 MB static binary on scratch" claim depends entirely on it linking cleanly
under musl. If it does not, we need to know in Phase 0 while the fallbacks are cheap
(distroless-glibc, or the pure-Rust `rusqlite` alternatives) rather than in Phase 6 when
the Dockerfile is load-bearing.

**Exit:** `just ci` green locally and in CI, *including the coverage gate* — proven by
deliberately committing an uncovered branch and watching the build fail. `podman compose up`
starts. `mdbook build` works. A hello-world static binary runs from a `scratch` image.

> The coverage gate must be verified to actually fail in Phase 0. A threshold nobody has
> seen reject anything is not a gate.

---

## Phase 1 — The pure core

The hardest logic in the project, with no I/O anywhere near it.

- Newtypes: `Fingerprint`, `ChannelId`, `MessageTs`, `GroupKey`, `ThreadTs`
- Alertmanager webhook payload types + `serde` deserialization
- Golden fixtures: real captured payloads (firing, resolved, grouped, mixed, empty)
- `ClaimOutcome`, `Op`, `Policy`, `AlertBatch` domain types
- **`plan()`** — D2 classification, D5 collapse decision, D7 debounce
- Exhaustive unit tests, including every row of ADR D3's concurrency table that is
  expressible without a database

**Why first:** if the `plan()` signature is wrong, this is the cheapest possible moment to
discover it. No database or HTTP work gets thrown away.

**Docs:** `explanation/fingerprint-correlation.md`, `explanation/why-outbox.md`.

**Exit:** `alertthread-core` at **100% line coverage** with **no surviving mutants** under
`just mutants`. Zero I/O dependencies, verified in CI.

---

## Phase 2 — Store layer

- `StateStore` trait (ADR D4)
- Migrations: `migrations/sqlite/`, `migrations/postgres/`
- `SqliteStore`, `PostgresStore` behind `sqlite` / `postgres` cargo features
- **Conformance suite** — one macro generating identical tests for both backends
- Concurrency tests: N tasks racing one fingerprint, assert exactly one `post` op
- Lease reclamation tests: kill a worker mid-lease
- Retention pruner

**Exit:** every row of ADR D3's table is a passing test on *both* backends.

**Docs:** `reference/configuration.md` (storage section), `how-to/enable-ha-postgres.md`.

---

## Phase 3 — Slack layer

- Client: `chat.postMessage`, `chat.update`, `auth.test`. Hand-rolled on `reqwest`, no SDK
- Typed error taxonomy: `rate_limited{retry_after}`, `message_not_found`, `invalid_auth`, …
- Block Kit rendering inside a coloured attachment (ADR D10)
- MiniJinja templates: `firing`, `resolved`, `group_summary`, `thread_reply`
- Template rendering wrapped in the D9 hardcoded fallback — *tested by feeding it a
  deliberately broken template*
- `insta` snapshots of rendered output; `wiremock` tests for every D9 failure row

**Exit:** the D9 failure table is a passing test matrix.

**Docs:** `how-to/customize-templates.md`, `reference/http-api.md`.

---

## Phase 4 — Wiring: the walking skeleton goes live

- `figment` config, with a redacting `Debug` impl on anything holding the token
- axum handlers: `POST /webhook`, `/healthz`, `/readyz`, `/metrics`
- Outbox worker loop: leasing, backoff, self-deferral (ADR D2)
- Per-channel token-bucket rate limiter
- Metrics from ADR D11
- Graceful shutdown that drains in-flight leases
- Upgrade `dev/slack-mock` to a real web UI showing messages *and threads*

**Exit:** `podman compose up`, then a real Prometheus rule fires → Alertmanager groups it →
relay threads it → resolution updates in place, visible in the mock UI. End-to-end,
no human in the loop.

**Docs:** `tutorials/01-first-alert-locally.md` — the money tutorial.

---

## Phase 5 — Hardening

Split in two. PR A is the resilience half — everything that decides whether an alert
survives a crash, a storm or a Slack outage. PR B is the security and packaging half.

### PR A — resilience

- Crash-recovery tests: `kill -9` at every stage of delivery, assert no silence
- Storm-collapse end-to-end under concurrent load
- Dead-letter handling: reporting and recovery
- Startup auth split on the D9 error taxonomy (known open item 12)
- `just mutants` gate narrowed to `core` and `store` (known open item 10)

### PR B — security and packaging

- Optional bearer-token auth on the webhook endpoint
- Container hardening: non-root, read-only rootfs, dropped caps, seccomp
- Alert rules **plus** the circular-dependency documentation (ADR D11) — the rules are
  actively harmful shipped without it
- Troubleshooting docs: `send_resolved`, `max_alerts` (ADR D8)
- The `just ci` / e2e recipe gap (known open item 11)

**Exit:** kill -9 during any phase of delivery never produces silence.

**Docs:** `how-to/troubleshoot.md`, `explanation/failure-semantics.md` — both PR B, plus
`how-to/harden-a-deployment.md` and `how-to/alert-on-the-relay.md`, which are where the D11
security and circular-dependency notes ended up living.

---

## Phase 6 — Release

- Multi-arch (`amd64`/`arm64`) images to ghcr.io
- Cosign keyless signing + SBOM attestation
- Helm chart published as an OCI artifact (matching home-ops' existing consumption pattern)
- `release-please` for changelog + tagging
- mdBook published to GitHub Pages
- All four Diátaxis quadrants complete
- **v0.1.0**

**Exit:** home-ops can consume this exactly like any other third-party app.

---

## Deferred

Not in v1, per ADR 001: the §2.1 passthrough endpoint; Slack interactivity ("Silence 1h");
label-based routing rules in relay config; escalating repeat-firing notifications;
a `/still-firing` slash command.

## Known open items

1. `collapse_threshold` default of 5 is a guess — revisit against real volume (ADR).
2. Whether group summaries should list members inline as well as threading them (ADR).
3. Whether to publish `alertthread-core` to crates.io as a reusable Alertmanager payload
   library. Costs API-stability obligations; decide at Phase 6, not before.
4. **ADR 001 D9 says template rendering "panics or errors"**, but `panic = "abort"` in the
   release profile makes `catch_unwind` dead code in every shipped build. What Phase 3 built
   instead is a stronger guarantee — no rendering path *can* panic, enforced by the workspace
   lint denials — but it is not what D9 describes. Reword D9, or accept the divergence
   explicitly.
5. **One equivalent mutant is excluded by name**, `replace > with >= in persist_group`,
   currently `crates/store/src/sqlite.rs:488`. `rows_affected()` is unsigned, so `>= 0` is a
   tautology, and the fall-through it controls is unreachable on SQLite — no test worth
   writing can kill it. Excluded via `--exclude-re` in the `mutants` recipe, which carries
   the argument; cite it by name and not by line, because the line moves whenever anything
   above it does. The other two mutants at the same site, `> with ==` and `> with <`, are
   deliberately still gated and both caught.
6. **ADR 003 should collect the Phase 3–4 gaps** once Phase 4 lands, the way ADR 002 batched
   Phases 1–2. Deliberately batched: one ADR per finding would make the numbering noise
   rather than signal. Items 4, 7, 8 and 9 above are candidates, along with the eight
   decisions listed in PR #7's body.
7. **`group_message.group_labels` is not in ADR 001 D4's schema sketch.** Same class of
   finding as ADR 002 §3.3's `outbox.dead_lettered_at`: D4 sketched the table before the
   question of how a summary names itself had been asked. The column is write-once and the
   reasoning is recorded in both migrations and on `GroupRecord`; the ADR is not rewritten.
   Fold into ADR 003.
8. **`/readyz` deliberately does NOT check Slack auth, and D11 says it should.** Argued and
   decided in review before Phase 4 was written, so this is an explicit divergence rather
   than a quiet one.

   Readiness controls whether the pod receives webhooks. If Slack auth is broken the correct
   behaviour is to *accept* the webhook, persist it to the outbox and retry — that is what
   the outbox is for. Going unready makes Alertmanager's POST fail, it retries a few times,
   then gives up, and **the alert is lost**: silence, from a condition the outbox was
   specifically designed to survive. It is worse with replicas, which all share one token
   and would therefore all flip unready at once, leaving no healthy pod to shed traffic to.

   The store *is* checked, because a relay that cannot reach its store cannot durably accept
   a delivery, so a 200 would acknowledge an alert it cannot persist.

   Mid-life token revocation is caught instead by a 15-minute background prober feeding
   `alertthread_slack_auth_valid`, which operators alert on. Three mechanisms, three jobs:
   startup fails fast on a bad token, `/readyz` gates on the store, the prober catches
   revocation. Fold into ADR 003.
9. **`alertthread_slack_auth_valid` is not in D11's metric list.** It exists because of item
   8 — the prober needs somewhere to put its verdict. Same class as item 7: D11 enumerated
   the metrics before the question it answers had been asked. Fold into ADR 003.
10. ~~**`just mutants` exits non-zero, and that is currently expected.**~~ **Resolved in
    Phase 5 PR A: the gate was narrowed, not excluded.** The recipe still runs
    `--workspace` and still prints every survivor on every run; only its *exit code* is
    scoped to `crates/core` and `crates/store`. The app crate's 14 survivors plus 2 timeouts
    are therefore still in front of a reader, and a new one appears among them, but a correct
    tree exits `0`.

    Excluding them by name was considered and rejected. `--exclude-re` asserts a mutant is
    *equivalent*, which is true of item 5's and false of these — they are unkilled, not
    unkillable, and naming them would also suppress a genuinely new survivor arriving at the
    same site. Both directions of the new gate were watched working before it landed: a
    stubbed assertion in `Policy::validate`'s tests made it exit `1` naming the two core
    survivors, and the app crate's existing `shutdown.rs` timeouts made it print three
    survivors and exit `0`.
11. ~~**`just ci` runs neither the image job nor `just e2e`.**~~ **Resolved in Phase 5 PR B:
    both, as decided in review.** `just pre-push` is `check-engine`, `check-rules`, `ci`,
    `image` and `e2e`, in that order so the cheapest failure comes first; AGENTS.md now says
    "every CI job invokes one of these recipes" and states plainly that `just ci` is not all of
    CI. `check-engine` runs first so a missing container engine fails legibly rather than
    silently skipping the container jobs.

    Two CI jobs are still not reachable from one local recipe, which is item 15.
12. ~~**Fail-fast startup auth conflicts with what the outbox promises.**~~ **Resolved in
    Phase 5 PR A: split on the D9 error taxonomy.** `SlackError::disposition` already answers
    "will this ever succeed?", and startup now asks it rather than asking "did `auth.test`
    work". `Disposition::Terminal` — `invalid_auth`, `account_inactive`, `token_revoked`, a
    malformed token, an unusable `base_url` — still refuses to start, with no retry at all.
    Everything else retries with bounded backoff for `slack.auth_startup_grace` (default 30 s)
    and then starts anyway with `alertthread_slack_auth_valid = 0`, leaving the outbox and
    the 15-minute prober to do their jobs.

    D11's "fail fast on a bad token" is preserved exactly; what changed is that a Slack
    outage is no longer treated as a bad token. Container ordering in the demo stack stops
    being load-bearing as a side effect.

13. **A dead-lettered op is recovered by an all-or-nothing sweep, not selectively.** Phase 5
    PR A added `StateStore::revive_dead_letters`, fired by the auth prober on an
    invalid→valid transition, and it returns *every* parked row rather than only the ones
    parked for an auth reason. Reviving selectively would need the low-cardinality reason
    persisted per row — an `outbox` column and a `dead_letter` signature change — and the
    coarse version's cost is bounded and self-correcting: a row parked for `msg_too_long`
    fails once more and re-parks, at one Slack call, on an event that only happens when a
    human has just fixed something. Revisit if a deployment ever accumulates enough
    permanently-unusable rows for that churn to matter.

14. **Nothing revives a dead letter parked for a non-auth reason.** `channel_unusable` is the
    case that will come up: an operator invites the bot to the channel and the alerts parked
    before that stay parked, because no probe watches channel membership the way the auth
    prober watches the token. The row and its payload survive, so recovery is possible by
    hand; there is no supported command for it yet. Worth an operator-facing path — a
    `/admin` endpoint behind PR B's bearer token, or a binary subcommand — decided
    deliberately rather than bolted on.

    PR B shipped the bearer token that such an endpoint would sit behind, so the blocker is now
    only the decision, not the perimeter.

15. **`just pre-push` still does not cover the `test-pg` or MSRV jobs.** Item 11 is resolved for
    the two recipes it named; these two are what is left, and both are deliberate rather than
    forgotten.

    `just test-pg` needs the compose stack up. Folding `just up` into `pre-push` would mean
    `just down --volumes` around it, which destroys whatever a developer had running — a recipe
    that eats your dev database to check a gate is a recipe people stop running. The MSRV job
    needs the 1.94 toolchain installed and `RUSTUP_TOOLCHAIN` overriding `rust-toolchain.toml`;
    a recipe cannot install a toolchain without being the kind of thing nobody wants a task
    runner doing.

    Both are named in AGENTS.md and in `pre-push`'s own closing output, so the gap is stated
    rather than implied. The honest fix is either a `pre-push-full` that manages the stack
    explicitly, or accepting that two jobs are CI's alone.

16. **A `401` on the webhook loses the alerts in that delivery, and that is a deliberate
    exception to "silence is never a valid outcome".** Alertmanager retries `5xx` and `429`, not
    `4xx`, so a refused delivery is never re-sent.

    The alternative — accepting a delivery that failed authentication — is not a trade this
    project gets to make for a user who asked for a perimeter. What it does instead is refuse to
    be quiet about it: ERROR on every refusal, two `outcome` label values separating "no
    credential" from "wrong credential", and `AlertthreadWebhookUnauthenticated` in the shipped
    rules. The token is also off by default, so the failure mode requires somebody to have
    turned it on.

    Worth stating in ADR 003 next to D9's table, because D9's "every degradation path terminates
    in post a plain message" now has two documented exceptions and the other one (`400` on an
    unparseable body) was already recorded only in passing.

17. **`alertthread_webhook_requests_total{outcome="auth_missing"|"auth_mismatch"}` are not in
    D11's metric list.** Same class as items 7 and 9: D11 specified the bearer token under
    "Security" and the metrics under "Observability", and did not connect the two. Splitting the
    two values is the same argument as `source` on `alertthread_rate_limited_total` — the
    operator's next action differs — with the extra wrinkle that the split is *deliberately
    invisible from outside*, since every refusal is an identical `401`.

18. **Nothing enforces the container hardening, and the alert thresholds are guesses.** Two
    findings from PR B that both land in Phase 6's lap.

    `compose.yaml` runs the relay read-only with all capabilities dropped and `just e2e` proves
    it, but the Kubernetes half — `readOnlyRootFilesystem`, `seccompProfile: RuntimeDefault`,
    `fsGroup` on the SQLite PVC — exists only as a documented fragment in
    `how-to/harden-a-deployment.md`. The chart is where it becomes real, and where something can
    check it. Note also that Prometheus and Alertmanager in the dev stack are *not* read-only:
    both write to a data directory inside their own WORKDIR, and a tmpfs over it comes up with
    the image directory's ownership while both run as `nobody`. Tried, reverted, commented in
    place.

    And every threshold in `deploy/alertthread.rules.yaml` is a starting point rather than a
    measurement — `alertthread_outbox_oldest_age_seconds > 300` most of all. Same status as
    item 1's `collapse_threshold`: revisit against real volume, and say so in the file, which it
    does.

## Process notes worth keeping

- **A gate nobody has watched reject something is not a gate.** Phase 0 proved the coverage
  and dependency-direction checks by deliberately breaking them. `just mutants` was *not*
  given that treatment and shipped broken — it passed an invalid flag and had never run,
  while AGENTS.md mandated it. Apply the rule to every gate, including the ones that look
  too simple to fail.

  It has since been given the treatment, in both directions: it exits 0 on a correct tree,
  and stubbing out two assertions in `Policy::validate`'s tests made it report the two
  resulting survivors and exit 2. Worth knowing what that second run actually showed, which
  was not what was expected: deleting the *negative*-debounce assertion alone left every
  mutant caught, because the `< TimeDelta::zero()` boundary was incidentally pinned by the
  *zero*-debounce test next to it. A test can look like the thing holding a branch down
  while a neighbour is doing the work — which is the same lesson as the gate itself, one
  level in.
- **A gate that rejects everything is not a gate either.** The exclusion above exists
  because the recipe otherwise exits non-zero on a correct tree, forever. That is not the
  safe direction to fail in: an exit code that is always the same carries no information,
  and people learn to wave it through, so the next genuinely new survivor arrives behind the
  one everybody already ignores. Exclude the equivalent mutant by name, argue it in place,
  and keep the exit code meaningful.
- **The rule check was given the treatment before it landed.** `deploy/alertthread.rules.yaml`
  is validated two ways, and both were watched rejecting something rather than merely passing:
  renaming `alertthread_outbox_oldest_age_seconds` in the file failed
  `every_metric_the_rules_name_is_one_this_build_exports` naming the metric; changing
  `outcome="rejected"` to a value nothing emits failed the label test; replacing a
  `sum by (job, instance)` with a bare `sum()` failed the routing test; and truncating a `{` was
  caught by `promtool` through `just check-rules`. The failure this guards against is specific
  and invisible: a rule naming a metric that does not exist evaluates empty for ever and looks
  exactly like a healthy relay.
- **Mutation testing has earned its place.** It found a real bug in Phase 3 that coverage
  could not see: `AlertView::from_webhook` decided "resolved?" by comparing `endsAt` against
  `startsAt`, so a resolution landing in the same second it fired rendered as still firing.
  The line was covered; the assertion just did not care.
- **Phases ending in working code find things re-reading documents does not.** Every gap in
  ADR 002 was found by implementing the case, not by review.
- **Amending a migration invalidates more local state than the compose volume.** sqlx
  checksums migrations, so editing `0001_initial.sql` before release — which is the agreed
  way to change the schema while nothing is released — fails every database an earlier run
  left behind. `just down` clears the PostgreSQL volume, but the SQLite files the wiring
  tests kept under `CARGO_TARGET_TMPDIR` survived it and broke `just ci` on a tree that was
  otherwise correct. Tests that persist a database now delete it first. Worth re-checking
  the next time a migration is amended: the failure names `VersionMismatch` and points at
  the migration, not at the stale file.
