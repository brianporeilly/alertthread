# Implementation roadmap

Phased plan for building `alertthread`, the Alertmanager → Slack threading relay.
Architecture is specified in [ADR 001](docs/001-adr.md); this document is *how we get
there*, not *what it is*. Where they conflict, the ADR wins.

**Guiding rule:** each phase ends with something that runs and is tested. No phase leaves
the tree in a state where the next phase is the only thing that makes it work.

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
| Test env | `podman compose` stack + `#[sqlx::test]`; no testcontainers |
| Task runner | `just` — **CI invokes the same recipes developers do** |
| Licence | Dual `MIT OR Apache-2.0` |

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
├── compose.yaml                # podman-compatible dev stack
├── Dockerfile                  # cargo-chef → musl static → scratch
├── AGENTS.md                   # contributor + agent constraints
├── deny.toml                   # cargo-deny: licences + advisories
├── crates/
│   ├── core/                   # alertthread-core   — PURE
│   ├── store/                  # alertthread-store  — StateStore + backends
│   ├── slack/                  # alertthread-slack  — client + rendering
│   └── app/                    # alertthread        — the binary
├── dev/
│   └── slack-mock/             # dev-only fake Slack with a web UI
└── docs/                       # mdBook, Diátaxis
```

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
- `justfile`: `fmt`, `lint`, `test`, `test-pg`, `docs`, `up`, `down`, `ci`
- GitHub Actions calling those same recipes: fmt, clippy, test, deny, MSRV, docs
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

**Exit:** `just ci` green locally and in CI. `podman compose up` starts. `mdbook build`
works. A hello-world static binary runs from a `scratch` image.

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

**Exit:** `plan()` fully covered. Core crate has zero I/O dependencies, verified in CI.

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

- Storm-collapse end-to-end under load
- Dead-letter handling + alerting
- Optional bearer-token auth on the webhook endpoint
- Container hardening: non-root, read-only rootfs, dropped caps, seccomp
- Crash-recovery tests: kill the process mid-post, assert no silence
- `PrometheusRule` **plus** the circular-dependency documentation (ADR D11) — the rule is
  actively harmful shipped without it
- Troubleshooting docs: `send_resolved`, `max_alerts` (ADR D8)

**Exit:** kill -9 during any phase of delivery never produces silence.

**Docs:** `how-to/troubleshoot.md`, `explanation/failure-semantics.md`.

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
