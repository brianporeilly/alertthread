# Implementation roadmap

Phased plan for building `alertthread`, the Alertmanager → Slack threading relay.
Architecture is specified in [ADR 001](docs/src/adr/001-adr.md); this document is *how we get
there*, not *what it is*. Where they conflict, the ADR wins.

**Guiding rule:** each phase ends with something that runs and is tested. No phase leaves
the tree in a state where the next phase is the only thing that makes it work.

---

## Current status

*Last updated 2026-07-30. Keep this current — it is the first thing anyone picking the
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
| 5 — Hardening, PR B | ✅ merged (#16) — webhook bearer auth, container hardening, alert rules, the two Diátaxis pages, `just pre-push` |
| 5 — closeout | ✅ merged (#17) — ADR 003 batching the divergences below |
| ↳ `alertthread replay` | ✅ merged (#18) — the subcommand ADR 003 §5.2 decided |
| **6 — Release, PR A** | 🟡 **in review — the Helm chart** |
| 6 — Release, PR B | ⬜ next — multi-arch images, cosign, SBOM, `release-please`, Pages, v0.1.0 |

ADRs [001](docs/src/adr/001-adr.md), [002](docs/src/adr/002-implementation-gaps.md) and
[003](docs/src/adr/003-hardening-divergences.md) are merged and accepted.

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
bugs. Revisit if `sqlx` gains native `jiff` support, **or if `chrono` draws a RUSTSEC advisory
or is archived** — `chrono` is soft-deprecated upstream and its maintainer recommends `jiff`.
Known open item 19 carries both triggers and the reason neither has fired.

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
├── deploy/                     # raw manifests, consumed directly
│   └── alertthread.rules.yaml  # Prometheus alert rules (ADR 001 D11)
├── charts/
│   └── alertthread/            # the Helm chart; PR B publishes it as an OCI artifact
├── dev/
│   └── slack-mock/             # dev-only fake Slack with a web UI
└── docs/                       # mdBook, Diátaxis
```

`deploy/` holds artefacts an operator consumes directly, without Helm. It stayed a plain
rules file rather than becoming a chart in Phase 5 precisely so `promtool check rules` could
validate it and a chart could embed `groups:` verbatim, and that is what
`charts/alertthread` does. Helm cannot read a file outside its own chart directory, so the
chart carries a byte-for-byte copy under `files/`; `deploy/` is the original, `just chart-sync`
updates the copy and `just chart` fails when they differ.

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

### Closeout

[ADR 003](docs/src/adr/003-hardening-divergences.md) — the divergences from ADR 001 that
Phases 3–5 produced, batched the way ADR 002 batched Phases 1–2. ADR 001 is not rewritten.

---

## Phase 6 — Release

Split in two. PR A is the chart — the artefact home-ops consumes and the first place the
container hardening can be *checked* rather than described. PR B is the publishing half:
nothing in it changes what the relay does, and all of it changes how somebody gets it.

### PR A — the Helm chart

- `charts/alertthread`: Deployment, Service, ServiceAccount, ConfigMap, Secret handling, PVC,
  probes, `ServiceMonitor`, `PrometheusRule`, `NetworkPolicy`, `NOTES.txt`
- Container hardening enforced rather than documented, with `scripts/chart-test.py` asserting
  every field renders (known open item 18)
- `deploy/alertthread.rules.yaml` embedded verbatim under the `PrometheusRule` `.spec`,
  circular-dependency warning intact, four thresholds exposed in `values.yaml`
- `just chart` / `just chart-sync`, reached by `just pre-push` and its own CI job

### PR B — publishing

- Multi-arch (`amd64`/`arm64`) images to ghcr.io
- Cosign keyless signing + SBOM attestation
- The chart published as an OCI artifact (matching home-ops' existing consumption pattern)
- `release-please` for changelog + tagging — including `Chart.yaml`'s `version` and
  `appVersion`, which PR A left as static numbers
- mdBook published to GitHub Pages
- All four Diátaxis quadrants complete
- Builder base moved to Project Hummingbird if the musl spike allows (known open item 24) —
  sequenced *before* the signing work, so provenance is attached to the base we intend to keep
- **v0.1.0**

### What Phase 5 handed it

Three things arrive here with the reasoning already settled. None is a new decision to take.

- **The chart is where container hardening becomes enforceable** (known open item 18).
  `compose.yaml` runs the relay read-only with all capabilities dropped and `just e2e` proves it,
  but the Kubernetes half — `readOnlyRootFilesystem`, `seccompProfile: RuntimeDefault`, the two
  writable mounts, `fsGroup` on the SQLite PVC — exists only as a documented fragment in
  `how-to/harden-a-deployment.md`. A fragment nothing checks drifts from the code that has to
  honour it; the chart is the first place something can assert it. **Done in PR A.**
- **The chart packages `deploy/alertthread.rules.yaml`.** It ships as a plain `groups:` file
  specifically so `promtool check rules` can validate it and a chart can embed it verbatim under
  a `PrometheusRule`'s `.spec` — that was the reason for not wrapping it in a CRD in Phase 5. It
  travels with the circular-dependency warning in its own header, and a test asserts that warning
  is still there, so packaging must not strip it. Every threshold in it is a starting point
  rather than a measurement, `alertthread_outbox_oldest_age_seconds > 300` most of all: same
  status as item 1's `collapse_threshold`, revisit against real volume.
- **`alertthread replay` is designed and not built** (known open item 14,
  [ADR 003 §5.2](docs/src/adr/003-hardening-divergences.md)). A binary subcommand, not an
  `/admin` endpoint. It gets its own PR and does not have to wait for the release work, but it is
  the remaining gap in "a parked alert can always be recovered" and should not ship after v0.1.0.

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
4. ~~**ADR 001 D9 says template rendering "panics or errors"**, but `panic = "abort"` in the
   release profile makes `catch_unwind` dead code in every shipped build.~~ **Recorded in
   [ADR 003 §2.1](docs/src/adr/003-hardening-divergences.md): the divergence is accepted
   explicitly and D9 is not reworded.** The guarantee built is the stronger one — no rendering
   path *can* panic — and it is enforced by the workspace lint denials rather than asserted.
5. **One equivalent mutant is excluded by name**, `replace > with >= in persist_group`,
   currently `crates/store/src/sqlite.rs:488`. `rows_affected()` is unsigned, so `>= 0` is a
   tautology, and the fall-through it controls is unreachable on SQLite — no test worth
   writing can kill it. Excluded via `--exclude-re` in the `mutants` recipe, which carries
   the argument; cite it by name and not by line, because the line moves whenever anything
   above it does. The other two mutants at the same site, `> with ==` and `> with <`, are
   deliberately still gated and both caught.
6. ~~**ADR 003 should collect the Phase 3–4 gaps** once Phase 4 lands, the way ADR 002 batched
   Phases 1–2.~~ **Written and merged in the Phase 5 closeout as
   [ADR 003](docs/src/adr/003-hardening-divergences.md), covering Phases 3–5 rather than 3–4** —
   Phase 5 was already in flight and splitting the batch would have produced the per-finding
   numbering noise the batching exists to avoid. It carries items 4, 7, 8, 9, 10, 12, 13, 14, 15,
   16 and 17.
7. ~~**`group_message.group_labels` is not in ADR 001 D4's schema sketch.**~~ **Recorded in
   [ADR 003 §3.1](docs/src/adr/003-hardening-divergences.md), grouped with items 9 and 17 as
   one class of finding: a set enumerated before the question it answers had been asked.** Same
   class as ADR 002 §3.3's `outbox.dead_lettered_at` — D4 again, one phase later, and this time
   D11 as well. D4 is not rewritten.
8. ~~**`/readyz` deliberately does NOT check Slack auth, and D11 says it should.**~~ **Recorded
   in [ADR 003 §2.2](docs/src/adr/003-hardening-divergences.md) with the full argument**, which
   is why it moved out of here: going unready on a broken token makes Alertmanager's POST fail
   and the alert is lost, which is silence produced by a readiness probe from the exact condition
   the outbox exists to survive. The ADR also carries the three-mechanisms table.
9. ~~**`alertthread_slack_auth_valid` is not in D11's metric list.**~~ **Recorded in
   [ADR 003 §3.2](docs/src/adr/003-hardening-divergences.md)**, grouped with items 7 and 17.
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

    Recorded as a settled decision in
    [ADR 003 Part 6](docs/src/adr/003-hardening-divergences.md) alongside item 15.
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

    This changed D11's startup behaviour, so it is recorded as a decision in
    [ADR 003 Part 4](docs/src/adr/003-hardening-divergences.md) with the full disposition
    table, not only as a struck-through line here.

13. ~~**A dead-lettered op is recovered by an all-or-nothing sweep, not selectively.**~~
    **Accepted in the Phase 5 closeout and recorded in
    [ADR 003 §5.1](docs/src/adr/003-hardening-divergences.md).** The coarse sweep stands: its
    cost is bounded and self-correcting — a row parked for a non-auth reason fails once more and
    re-parks, at one Slack call, on an event that only happens when a human has just fixed
    something — and the cost of getting a *filter* wrong is an alert nobody hears about.

    **Revisit if** a deployment ever accumulates enough permanently-unusable rows for that churn
    to matter. That condition survives the acceptance; it is the only thing that would reopen it.

14. ~~**Nothing revives a dead letter parked for a non-auth reason — `alertthread replay` is
    decided and not yet built.**~~ **Built.** The shape is the one
    [ADR 003 §5.2](docs/src/adr/003-hardening-divergences.md) decided: a binary subcommand, not
    an `/admin` HTTP endpoint. It is a dry run unless `--commit` is passed, and it scopes by
    `--channel` and `--fingerprint`.

    Those two axes are the low-cardinality columns `outbox` already carries. Scoping by *park
    reason* — the obvious third axis — was considered and rejected here: the reason is not
    persisted per row, so it would have cost a migration and a `dead_letter` signature change
    to narrow an operation whose cost is already bounded and self-correcting (§5.1). Channel is
    the axis the motivating `channel_unusable` case actually needs.

    `revive_dead_letters` gained a `DeadLetterScope` rather than a second method. The automatic
    prober sweep passes `DeadLetterScope::ALL` and so stays all-or-nothing exactly as §5.1
    describes; the narrower scope exists only for the human at the shell. Revival hands rows
    back to `lease_batch` rather than delivering anything itself, which is what makes it safe
    to run against a live relay: a revived row is picked up by whichever worker leases it next,
    under the same exactly-once claim as any other queued op.

15. ~~**`just pre-push` still does not cover the `test-pg` or MSRV jobs.**~~ **Accepted as
    documented in the Phase 5 closeout: the two jobs are CI's alone, and there is no
    `pre-push-full`.** Recorded in
    [ADR 003 Part 6](docs/src/adr/003-hardening-divergences.md) alongside item 10.

    `just test-pg` needs the compose stack up, and folding `just up` in would mean
    `just down --volumes` around it — a recipe that eats a developer's dev database to check a
    gate is a recipe people stop running. The MSRV job needs the 1.94 toolchain installed and
    `RUSTUP_TOOLCHAIN` overriding `rust-toolchain.toml`, and a task runner that installs
    toolchains is not a thing anybody wants. Both gaps are named in AGENTS.md and in
    `pre-push`'s own closing output, so the limit is stated rather than implied.

16. ~~**A `401` on the webhook loses the alerts in that delivery, and that is a deliberate
    exception to "silence is never a valid outcome".**~~ **Recorded in
    [ADR 003 Part 1](docs/src/adr/003-hardening-divergences.md), next to D9's table**, which is
    where it belonged: D9's "every degradation path terminates in post a plain message" has two
    documented exceptions and the ADR now states both, along with the invariant that actually
    holds — *once a delivery has been accepted, no path terminates in silence.* The `400` on an
    unparseable body, previously recorded only in passing, is written down there too.

17. ~~**`alertthread_webhook_requests_total{outcome="auth_missing"|"auth_mismatch"}` are not in
    D11's metric list.**~~ **Recorded in
    [ADR 003 §3.3](docs/src/adr/003-hardening-divergences.md)**, grouped with items 7 and 9 as
    one class of finding rather than stated three times.

18. ~~**Nothing enforces the container hardening, and the alert thresholds are guesses.**~~
    **Resolved in Phase 6 PR A, both halves.**

    The hardening is now the Helm chart's defaults and `scripts/chart-test.py` asserts each
    field renders — `readOnlyRootFilesystem`, `runAsNonRoot`, `runAsUser: 65532`,
    `seccompProfile: RuntimeDefault`, `capabilities.drop: [ALL]`,
    `allowPrivilegeEscalation: false`, `fsGroup` on the PVC, and the two writable mounts a
    read-only rootfs forces the relay to declare. Every one was watched rejecting its own
    deletion before the PR was opened, and `just chart` runs the checks from `just pre-push`
    and its own CI job.

    `just e2e` and `just chart` answer different questions and neither replaces the other:
    the first proves the relay *runs* under these flags, the second proves the flags are still
    *set* in what Kubernetes gets. A regression in the second is invisible to the first,
    because `compose.yaml` holds its own copy of the settings.

    The thresholds ship as written, in `values.yaml`, labelled there as starting points an
    operator is expected to override — a tunable rule beats no rule. Four are exposed
    (`outboxOldestAgeSeconds`, `outboxDepth`, `slackCallErrorRatio`,
    `slackRateLimitedPerSecond`); the rest, including every `> 0`, are not, because there is no
    threshold below "one" worth setting on an alert nobody was told about. **The revisit
    survives the resolution**: `alertthread_outbox_oldest_age_seconds > 300` is still a guess
    with the same status as item 1's `collapse_threshold`, and now it is a guess somebody can
    change without forking the file.

    Still true and still recorded: Prometheus and Alertmanager in the dev stack are *not*
    read-only. Both write to a data directory inside their own WORKDIR, and a tmpfs over it
    comes up with the image directory's ownership while both run as `nobody`. Tried, reverted,
    commented in place.

19. **`chrono` is soft-deprecated, and the revisit trigger written for it is now too narrow.**
    [chronotope/chrono#1768](https://github.com/chronotope/chrono/issues/1768), open since
    January 2026: the lead maintainer intends to wind down `chrono` and `chrono-tz`, calls the
    API dated, and explicitly recommends `jiff` — the crate this project already called "the
    better-designed library" when it chose against it. No timeline, no archival, and handover to
    a credible maintainer is left open.

    **Nothing to do yet, and the reason is unchanged.** `sqlx` 0.9 ships `chrono` and `time`
    features and has no `jiff` feature — verified against the vendored manifest, not from
    memory. Migrating before that exists reinstates conversion at every store call, which is
    precisely the two-time-types-in-one-codebase hazard the original decision avoided. The
    deprecation does not change that mechanic.

    **What changes is that there are now two triggers, either sufficient**, where the settled
    decisions table records only the first:

    1. `sqlx` gains native `jiff` support — the original trigger, and the one that makes the
       migration cheap.
    2. `chrono` draws a RUSTSEC advisory or is archived — the one that makes it urgent.
       `cargo-deny` is already a CI job, so this arrives as a hard build failure rather than as
       a discussion. That tripwire is wired; it does not need building.

    Scale, for whoever picks this up: 65 `chrono` references across 36 files, concentrated in
    `core` — which is at 100% coverage and mutation-gated, so this churns the crate holding
    every correctness decision for no user-visible benefit. Revisit **after** v0.1.0, not
    during it.

20. **The chart carries a duplicate of `deploy/alertthread.rules.yaml`, and a test is the only
    thing keeping it honest.** Helm cannot read a file outside its own chart directory —
    `.Files.Get` is rooted at the chart, and the loader skips symlinks rather than following
    them, so a symlink would go missing at `helm package` time without erroring. The chart
    therefore keeps a byte-for-byte copy at `charts/alertthread/files/alertthread.rules.yaml`,
    `just chart-sync` writes it, and `scripts/chart-test.py` fails when it differs from
    `deploy/`.

    Every alternative was worse. Hand-copying the rules into a template throws away the single
    source of truth and the `promtool` check with it. Moving the original into the chart
    breaks `deploy/`'s reason for existing — an artefact an operator consumes without Helm —
    and moves the path two gates depend on. Generating the copy at package time leaves a
    checkout that `helm template` cannot render, which is the thing the chart tests need.

    **Revisit if** Helm ever gains a supported way to include a file from outside a chart, or
    if `deploy/` stops having non-Helm consumers — at which point the chart's copy becomes the
    original and `deploy/` becomes the generated one.

    Two smaller seams from the same PR, both PR B's to close: `Chart.yaml`'s `version` and
    `appVersion` are static and need wiring into `release-please`, and the default
    `image.repository`/`appVersion` name `ghcr.io/brianporeilly/alertthread:0.1.0`, which does
    not exist until PR B publishes it.

21. ~~**ADR 001 D4 specifies a Downward API replica check that was never built.**~~
    **Accepted as a divergence in Phase 6 PR C: the guard is not being built, and the chart
    is the enforcement point.** D4 says "if SQLite is configured and `replicas > 1` is
    detected (via the Downward API), the process **refuses to start**". It does not, and it
    will not. Nothing in the relay reads the Downward API.

    What does hold: the chart refuses to render `replicaCount > 1` on SQLite, with a message
    naming the backend to switch to. A chart-managed deployment is the realistic consumer of
    this project, and that is where the guard now lives.

    What is therefore not covered, stated rather than implied: a `kubectl scale`, a raw
    manifest, or two pods from different releases all walk straight past it, and two
    processes on one SQLite file corrupts correlation state. `how-to/enable-ha-postgres.md`
    says so at the point an operator would do it.

    Building the relay half was weighed and declined. It is cheap in isolation — read
    `spec.replicas` from an env var the chart already knows how to set — but it buys
    enforcement only for deployments that pass the variable, which is the same set the chart
    already covers, and a `kubectl scale --replicas=2` sets no environment at all. A guard
    that refuses to start is also a new way to be silent, and the way to be wrong about it is
    to read a stale value. Paying for that to re-cover the covered case is the wrong trade.

    Found while writing the chart, not by reading D4; `how-to/enable-ha-postgres.md` had been
    asserting the check existed since Phase 2, and that claim was corrected in Phase 6 PR A.
    Deliberately **not** written up as an ADR here: Phase 6 batches its divergences at
    closeout the way ADR 003 did for Phase 5, and this is one of them.

22. ~~**Every documented bare `ALERTTHREAD_*` environment variable makes the relay refuse to
    start.**~~ **Resolved in Phase 6 PR C: the non-config names are reserved, and the
    strictness that caused it is untouched.** Found by booting the relay against the ConfigMap
    the chart renders, which is a thing nothing had done before.

    `Config::figment` merged `Env::prefixed("ALERTTHREAD_").split("__")`, and `Config` denies
    unknown fields — deliberately, because a misspelled key is a setting an operator believes
    is in effect. A name with no `__` in it therefore parsed as a *top-level* key:
    `ALERTTHREAD_CONFIG` as `config`, `ALERTTHREAD_LOG` as `log`, `ALERTTHREAD_LOG_FORMAT` as
    `log_format`. All three were documented in `reference/configuration.md`, and
    `run::config_path` reads `ALERTTHREAD_CONFIG` — the code that consumed it was correct and
    never ran, because the figment layer rejected the variable first. Nested names were
    unaffected: `ALERTTHREAD_STORAGE__URL` worked, which is why nothing noticed.

    **The fix is `Env::ignore`, not a loosened `deny_unknown_fields`.** The three names are
    one constant, `config::RESERVED_ENV_VARS`, and the provider is told to skip exactly them.
    Every other unknown key is as fatal as it was — `ALERTTHREAD_SLACK__TOKNE` still stops the
    relay, which is the whole reason the strictness is there.

    **`RUST_LOG` is now honoured as a fallback**, after `ALERTTHREAD_LOG`. The argument is in
    the PR: the failure being fixed is "nobody can turn the logging up", and the name an
    operator reaches for without reading anything is `RUST_LOG`. Precedence rather than
    equality, so a `RUST_LOG` inherited from a base image cannot override what a deployment
    set by name. `compose.yaml`'s `RUST_LOG`, inert since Phase 4, does something now.

    Three tests, because the reason this shipped is that each layer alone would have missed
    it. `crates/app/tests/environment.rs` starts the real binary with each reserved variable
    set and asks it for a `200` — nothing had ever done that.
    `config::every_documented_bare_variable_is_reserved` scans the operator-facing surfaces
    for `ALERTTHREAD_*` literals and fails when one is documented but not reserved, because
    the bug arrived as documentation before it arrived as a bug report.
    `a_bare_variable_that_is_not_reserved_still_refuses_to_start` pins the fatality that was
    never the problem.

    Phase 6 PR A's chart-side workaround is retired with it. The chart still passes the config
    file positionally — explicit beats implicit, and `kubectl describe` shows which file the
    pod is on — but `values.yaml` now documents `ALERTTHREAD_LOG` under `env` as the supported
    way to raise the log level, and `scripts/chart-test.py`'s check is narrowed from "no bare
    `ALERTTHREAD_<WORD>` at all" to "no *unreserved* one", reading the list out of
    `config.rs` so the chart cannot end up stricter than the binary.

23. **The reserved environment variables are parsed leniently, and every other setting is
    parsed strictly.** Noticed while fixing item 22, and left as it is on purpose.

    `ALERTTHREAD_LOG_FORMAT` is `json` or it is not; `structured`, `jsonl` and `json ` all
    silently produce human-readable output. `ALERTTHREAD_LOG` and `RUST_LOG` fall through to
    the next source when the directive does not parse, so `ALERTTHREAD_LOG=infoo` logs at
    `info` and says nothing about it. Both are exactly the shape this project calls out
    elsewhere — a setting an operator believes is in effect and is not — and the config layer
    two lines away refuses to start over the same class of typo.

    **Not fixed, because the obvious fix is worse and the good one is not obvious.** Refusing
    to start over a malformed log filter trades the entire relay for a typo in a diagnostic,
    which is the wrong direction for a service whose worst failure is silence. Warning instead
    is the right shape and lands in an awkward place: the only thing that could carry the
    warning is the subscriber being configured, and a filter of `error` — plausible in exactly
    the deployment that is trying to quieten the relay — filters the warning out. Emitting to
    stderr before the subscriber exists sidesteps that and puts an unstructured line in front
    of a JSON log consumer.

    **Revisit** whenever `init_tracing` next changes. The candidate is to hold the rejected
    directive and re-emit it at `error` after the subscriber is up, plus an explicit
    `ALERTTHREAD_LOG_FORMAT` match that treats an unrecognised value the same way — both cheap
    once somebody is already in that function, and neither worth a PR of its own.

24. **The builder base cannot move to Project Hummingbird. It is pinned by digest instead;
    the runtime stays `scratch`.** Spiked and settled on 2026-08-02 — outcome 3 of the three
    below. The reasoning that sent us looking is preserved; the spike result follows it.

    [Hummingbird](https://hummingbird-project.io/docs/using/overview/)
    is Red Hat's minimal-hardened-image project: distroless by default, non-root, hermetic
    builds, cosign-verified signatures, SLSA provenance, SBOMs, and FIPS variants.

    **The runtime image is not the part that benefits.** `Dockerfile`'s final stage is
    `FROM scratch` holding a static musl binary and a CA bundle — no libc, no shell, no package
    manager. Hummingbird's distroless variant is excellent and still has a userland, so swapping
    `scratch` for it would *increase* attack surface. Their own multi-stage guidance for compiled
    languages points the same way; `scratch` is the more aggressive version of the runtime half
    of it, and their documented example uses `hi/core-runtime` rather than `scratch` only because
    it is written for the general case.

    **The builder stage is where the real gap is.** It is currently
    `docker.io/library/rust:1.97.1-alpine3.22` — unsigned, no provenance, no attestation. PR B is
    about to attach cosign signatures and SLSA provenance to artefacts built on top of it, and
    signing an artefact built on an unattested base leaves the weakest link exactly where the
    claims are strongest. **So this is sequenced before the signing work, not after.**

    **Spike first — the assumption is not free.** Hummingbird documents no Rust builder image and
    says nothing about musl or static linking; its example is Go, which links statically without a
    C toolchain. Rust targeting `x86_64-unknown-linux-musl` needs a musl toolchain in the builder,
    and Hummingbird is RHEL-lineage, where `musl-gcc` is not a default package. This is the same
    class of assumption as Phase 0's musl-on-`scratch` spike, and gets the same treatment: prove
    it before building on it. Fallbacks, in order, so nobody stalls on the decision:

    1. A Hummingbird builder that produces the musl static binary. `scratch` runtime unchanged.
    2. Hummingbird for every stage it can serve, with a digest-pinned `rust:alpine` retained only
       for the musl link step. A partial win, labelled as partial.
    3. Neither works: keep `rust:alpine`, pin it by digest, attest it in PR B, and record that the
       builder base is still the weakest link rather than implying it is solved.

    **Abandoning `scratch` for a hardened runtime base is not on that list.** The one thing that
    would reopen it is a FIPS requirement: a static musl binary with `rustls` cannot be
    FIPS-validated, and Hummingbird's FIPS variants are the reason that would matter. Written down
    now so it is not rediscovered under compliance pressure.

    ### What the spike found

    **A Hummingbird Rust image exists — the documentation is wrong about that.** The "using"
    page lists no Rust image and its multi-stage example is Go, but the registries carry one,
    at the exact version this project already pins:

    | Path | Exists | Notes |
    |---|---|---|
    | `registry.access.redhat.com/hi/rust:1.97.1-builder` | **yes** | `sha256:5f9ce5a9651e…827d`, `vendor=Red Hat, Inc.`, multi-arch amd64+arm64 |
    | `quay.io/hummingbird/rust:1.97.1-builder` | **yes** | identical digest — a true mirror, not a rebuild |
    | `quay.io/hummingbird-rawhide/rust:1.97.1-builder` | yes | plain Fedora repos, *not* a hardened stream |
    | `quay.io/hummingbird-community/rust` | **no** | namespace has 19 repos, no Rust |
    | `quay.io/hummingbird-ci/rust` | **no** | build infrastructure only |

    The builder variant is a real toolchain: `dnf`, `rpm`, `bash`, `gcc`, `make`, `pkg-config`,
    and `rustc 1.97.1 (Fedora 1.97.1-2.hum1)` — the same upstream version as `rust:alpine`.
    `cmake`, `gcc-c++`, `clang` and `perl` all install from its repo, so `aws-lc-rs` and
    `libsqlite3-sys` would have had what they need.

    **It fails on exactly one thing: there is no Rust musl standard library, anywhere.**

    - `dnf list rust-std-static-*` offers `x86_64-unknown-none`, `x86_64-unknown-uefi`,
      `wasm32-*`, `*-pc-windows-gnu` and `aarch64-unknown-none-softfloat`. **No
      `x86_64-unknown-linux-musl`.** `dnf install` of that name: `No match for argument`.
    - `dnf search musl` in the supported repo: `No matches found`. No `musl-gcc`, no
      `musl-libc`, no `musl-devel`, and nothing `Provides` them.
    - Proven by building, not by reading the package list: a hello-world
      `cargo build --target x86_64-unknown-linux-musl` in the image fails with
      `error[E0463]: can't find crate for std` — "the `x86_64-unknown-linux-musl` target may
      not be installed".
    - The rawhide stream *does* carry `musl-gcc`, `musl-devel` and `musl-libc-static` — it is
      wired to plain `fedora`/`updates` repos — but **still no Rust musl std**, because Fedora
      does not package one either. It is also not a hardened stream, so it would trade the
      provenance we came for against a rolling development base.
    - `-Z build-std` is closed: `rust-src` is available, but the toolchain is stable and
      `-Z` is nightly-only (`the -Z flag is only accepted on the nightly channel`).
    - `rustup` is absent, and installing it would pull an unsigned toolchain from
      `static.rust-lang.org` into the hardened image — which defeats the entire reason for
      moving, so it is not a fallback.

    **Outcome 2 was considered and rejected as ceremony.** The only stages Hummingbird could
    serve are the `planner` (whose output is `recipe.json`, a text manifest contributing zero
    bytes to the artefact) and the CA-bundle source (180 KB of the shipped image). Neither
    moves the weakest link, which is the *compiler*; both add a ~950 MB base image to every
    build and a second `cargo install cargo-chef` compile. Improving the trust store while the
    thing that decides the binary's contents is unchanged is optimising the visible small half.

    **So: outcome 3.** `rust:1.97.1-alpine3.22` is retained and pinned to
    `sha256:df4efa4e0cdfb5245fa06e3f431387b2bcc96782ce5681b7fb6b0297d745bc29` (a multi-arch
    index, so PR B2's arm64 work is not foreclosed). **The builder base is still the weakest
    link and PR B's signatures do not change that** — they attest what we built, not what we
    built it on.

    The shipped artefact did not move: binary `8 659 056` bytes, sha256 `b1411e4a…b90f`,
    `static-pie linked`, `ldd` → `statically linked`, image `8.85 MB`, `USER 65532:65532`,
    runs `--version` from `scratch`. Baseline and pinned builds produced the **same podman
    image ID** (`13bdd7158ec1`) and byte-identical binary and CA bundle.

    **Re-check trigger:** a `rust-std-static-x86_64-unknown-linux-musl` package appearing in
    the Hummingbird repo, or Hummingbird publishing a musl-target Rust builder. Both are the
    single blocker; everything else was already in place. Nothing else about this needs
    re-deriving.

25. **A digest pin makes the version tag decorative, and the obvious guard against that is
    tautological.** Found while implementing item 24, in both directions, by watching it.

    `FROM …/rust:${RUST_TOOLCHAIN}-alpine${ALPINE_VERSION}@${RUST_ALPINE_DIGEST}` resolves on
    the **digest**; the tag is ignored. Building with the tag deliberately set to
    `1.94-alpine` and the 1.97.1 digest succeeds and yields `rustc 1.97.1`. So bumping the
    version in `Dockerfile` without bumping the digest is a silent no-op — precisely the
    "a setting somebody believes is in effect and is not" failure this project legislates
    against elsewhere. `Dockerfile`'s chef stage now asserts `rustc --version` matches.

    **The first version of that assertion could never fail, and only testing it revealed
    why.** It was written against `ARG RUST_VERSION` — but `docker.io/library/rust` exports
    `ENV RUST_VERSION=1.97.1` itself, and a bare in-stage `ARG` of a name the base image
    already sets resolves to *the inherited ENV*, not to the global `ARG` default. The check
    therefore compared the image's `rustc` against the image's own declaration of its version
    and passed unconditionally. Editing the global default to `1.96.0` still printed
    `1.97.1`. Renaming the arg to `RUST_TOOLCHAIN`, which the base image does not set, fixed
    it; the drifted build now fails by name, and the correct one still passes.

    **A related buildah quirk, worth knowing before trusting a `--build-arg` override:**
    `podman build --build-arg RUST_VERSION=1.96.0` applied the override to the `FROM` line
    while the in-stage `ARG` of the same name did *not* see it. A build arg can therefore be
    live in one part of a Dockerfile and ignored a few lines later. Do not assume an override
    took effect without echoing it.

26. **The build-and-packaging size figures are Phase 0 projections and have been overtaken.**
    `docs/src/explanation/build-and-packaging.md` states **6.02 MB** as "the number to
    compare against" and notes "roughly 2 MB of headroom before the published figure is"
    exceeded. Measured on `main` at the time of item 24's spike: binary **8 659 056 bytes**,
    image **8.85 MB**. The headroom is gone and ADR 001's "~8 MB static binary" is now
    slightly exceeded rather than conservative.

    Nothing is wrong — the projection was made in Phase 0 against crates that had no
    dependencies yet, and the page says so. But the page currently reads as a measurement of
    the shipped artefact and is not one, and the ADR's estimate is quoted elsewhere. **Not
    fixed here** because item 24's PR is a build-provenance change and re-measuring the
    packaging docs is a separate edit with its own review. Worth doing the next time anything
    touches that page, and worth a fresh measurement rather than a patched number.

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
- **The chart's hardening assertions were watched rejecting every field they hold down.**
  Deleting `readOnlyRootFilesystem`, `seccompProfile`, `fsGroup` and the `[ALL]` capability
  drop each failed `just chart` by name, as did repointing `/readyz` at `/healthz`, shrinking
  the startup budget below `slack.auth_startup_grace`, removing the `/tmp` mount, drifting the
  chart's copy of the rules from `deploy/`, breaking a threshold anchor, changing the
  ServiceMonitor's `jobLabel` out from under `up{job="alertthread"}`, deleting the
  circular-dependency warning, and moving a token from a Secret into the ConfigMap.

  Writing them found a bug they then caught: the first `prometheusrule.yaml` piped into
  `regexReplaceAll` with the arguments in the wrong order and rendered a `PrometheusRule` with
  an **empty `spec:`**. `helm lint --strict` passed it. A well-formed document with nothing in
  it is exactly the shape of failure this project keeps legislating against, and it is why the
  assertions parse the objects instead of grepping the text.

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
