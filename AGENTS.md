# AGENTS.md

Constraints for anyone — human or agent — working in this repository.

`alertthread` sits between Alertmanager and Slack. It correlates firing→resolved events by
alert fingerprint so a resolution updates or threads under the original message instead of
posting an unrelated new one.

**This is alerting infrastructure. The worst possible bug is silence.** A duplicate
message is a nuisance; a dropped alert is an outage nobody hears about. Every trade-off in
this codebase resolves in that direction, and code that could produce silence does not
merge.

## Read before changing anything

| Document | What it is |
|---|---|
| [`docs/src/adr/001-adr.md`](docs/src/adr/001-adr.md) | The architecture. Decisions D1–D12 are settled |
| [`ROADMAP.md`](ROADMAP.md) | Phases, pinned versions, settled choices |
| [`docs/src/adr/000-prd.md`](docs/src/adr/000-prd.md) | Problem statement and prior research |

**Do not re-litigate settled decisions.** Rust over Go, outbox over synchronous posting,
SQLite-default with Postgres-optional, per-fingerprint with storm-collapse — all decided,
with reasoning recorded. If you believe one is wrong, say so explicitly and argue it; do
not quietly implement something else.

---

## Architecture rules

Functional core, imperative shell. Dependency direction is enforced by Cargo, and CI
fails if it is violated:

```
app ──→ store ──→ core
 └────→ slack ──→ core
                  core ──→ (nothing with I/O)
```

**1. `alertthread-core` is pure.** No `tokio`, `sqlx`, `axum`, `reqwest`, no filesystem,
no network, no clock reads, no RNG. Time enters as a `now: DateTime<Utc>` parameter.
If you need I/O in the core, the design is wrong — move the I/O to the shell and pass
its result in.

**2. Decision logic goes in `plan()`, not in handlers.** Any question of the form "given
this state, what should we do?" belongs in the pure core where it can be tested without
mocks. Handlers and workers execute decisions; they do not make them.

**3. Traits only at genuine I/O seams** — `StateStore`, the Slack client, the clock. Do
not add a trait per layer. `Arc<dyn Trait>` in the core is a design smell; concrete types
internally are correct Rust.

**4. Newtypes for all identifiers.** `Fingerprint`, `ChannelId`, `MessageTs`, `GroupKey`,
`ThreadTs` are distinct types, never `String`.

> This is not ceremony. `chat.update(channel, ts)` takes two strings; swapping them
> compiles fine and fails at runtime, in the alerting path, under load. The type system
> should make that unrepresentable.

**5. Errors:** `thiserror` with a typed enum in library crates; `anyhow` only in the
binary. Never `unwrap()` or `expect()` outside tests and `main()` startup. Never swallow
an error without either handling it or emitting a metric.

---

## Testing

Testing is not a phase. A change without tests is not done.

| Change | Required |
|---|---|
| Anything in `core` | Unit tests covering every branch. No mocks — it's pure |
| Anything in `store` | Conformance suite passing on **both** SQLite and Postgres |
| Concurrency / claims | An explicit racing test asserting exactly-once |
| Slack rendering | `insta` snapshot |
| Slack error handling | `wiremock` test for that specific failure |
| A fixed bug | A test that fails without the fix |

- `#[sqlx::test]` gives an isolated database per test. Use it. **Do not use
  testcontainers** — it needs a docker socket and its Ryuk reaper misbehaves under
  rootless podman.
- Store tests are written once against the trait and run against both backends. Never test
  only one.
- Never write a test that asserts an alert was dropped. That is not a behaviour we have.

### Coverage gate

`just test` and `just ci` fail if any crate drops below its threshold.

| Crate | Threshold |
|---|---|
| `alertthread-core` | **100%** |
| `alertthread-store` | 95% — gated twice, see below |
| `alertthread-slack` | 95% |
| `alertthread` (app) | 95% — `main.rs` excluded |
| `dev/slack-mock` | excluded |

`alertthread-store` has two backends behind cargo features and no single build can run the
tests for both. `just test` compiles SQLite only and `just test-pg` compiles PostgreSQL only,
and each gates the store at 95% against the code it actually compiled. **Both must pass.**
Adding a backend means adding a gated build, not widening an existing one. Rationale in
ROADMAP.md and `scripts/coverage-gate.py`.

Use `just test-fast` in the inner loop; instrumentation costs 2–3× runtime. It is a
convenience, not a way around the gate.

**Do not chase the threshold with tests that assert nothing.** A test that calls a function
to touch its lines and checks no output scores full coverage and catches nothing — in this
codebase it manufactures false confidence about the alerting path, which is worse than the
uncovered line was. If a branch is genuinely unreachable, delete it. If it is genuinely
untestable, exclude it explicitly with a comment saying why, and say so in the PR.

**If you lower a threshold or add an exclusion, say so in the PR description and justify
it.** Silently weakening the gate to make a change pass is the one move that is never
acceptable here.

### Mutation testing

Coverage proves a line ran. It does not prove a test would have caught the regression.
`cargo-mutants` closes that gap by breaking the code and checking that something fails.

- **Required for any change to `alertthread-core`**: `just mutants`, no surviving mutants.
- **`just mutants` runs the whole workspace and prints every survivor. Its exit code covers
  `crates/core` and `crates/store` only.** Survivors elsewhere are printed and must be
  triaged; they do not fail the recipe. That split is deliberate — a gate that a correct
  tree cannot satisfy carries no information and gets waved through, and the next genuinely
  new survivor then arrives behind the ones everybody already ignores.
- **Do not silence a survivor with `--exclude-re` to make the gate pass.** The one exclusion
  in the recipe is for a mutant that is *equivalent* — provably unkillable — and it argues
  itself in place. Everything else is merely unkilled, which is a different thing.
- Runs nightly across the workspace.

For a system whose worst failure is silence, "would we have noticed?" is the question that
matters, and this is the only tool that answers it.

---

## Documentation

Diátaxis, rendered by mdBook, in `docs/`. **Docs ship with the change, not after it.**

| Quadrant | Directory | Answers |
|---|---|---|
| Tutorial | `docs/src/tutorials/` | "Teach me this, I'm new" |
| How-to | `docs/src/how-to/` | "I have a specific goal" |
| Reference | `docs/src/reference/` | "What are the exact options?" |
| Explanation | `docs/src/explanation/` | "Why is it built this way?" |

**Pick one quadrant per page and hold the line.** The most common failure is a how-to guide
that drifts into explanation, or reference that turns into a tutorial. If a page needs
both, write two pages and link them.

Every PR that changes behaviour updates the relevant quadrant. A new config option is not
merged until it appears in `reference/configuration.md`. A new metric is not merged until
it appears in `reference/metrics.md`. State in the PR description which quadrant you
touched and why.

### Comments

**Reasoning goes in the PR and the commit message, not in the file.** What was tried, what
broke, why this over that — a reader who wants it can find it; a reader who does not should
not have to scroll past it. Comment only what the code cannot say itself, and keep it to a
line. The exception is narrow and deliberate: where a comment is the only thing stopping
someone from "simplifying" a load-bearing choice into a bug.

### ADRs

ADRs live in `docs/src/adr/`. "Append-only" protects **decisions**, not every character:

- **Decisions, rationale, alternatives, consequences — never rewritten.** Changed your mind?
  Write a new ADR that supersedes the old one. A reader must be able to see what was decided
  *then*, not a tidied version.
- **Factual drift — corrected in place.** Renames, moved paths, broken links, typos. Record
  it in the ADR's **Amendments** section: what changed, and why it was not a decision.

The test is not "is this obviously wrong?" — everyone thinks their edit obviously qualifies,
and that is how the convention erodes. The test is: **was this string ever decided?** ADR 001
never decided the metric prefix should be `sturdy_telegram_`; it recorded the name as
provisional and expected the rename. Correcting those was completing a decision, not
reversing one. Contrast D12's naming rationale, which *was* the decision and is preserved
verbatim with a resolution note appended.

If you cannot tell which side something falls on, it is a decision. Supersede it.

---

## Commands

`just` is the only entry point, and **every CI job invokes one of these recipes** rather than
spelling the flags out again in a workflow file.

```
just fmt        # rustfmt
just lint       # clippy -D warnings
just test       # unit + sqlite integration, instrumented + coverage gate
just test-fast  # same tests, no instrumentation — for the inner loop
just test-pg    # postgres conformance (needs `just up`)
just coverage   # report only, no gate — for finding the gaps
just mutants    # mutation testing; required for core changes
just docs       # mdbook build + link check
just chart      # helm lint + the assertions on what the chart renders
just chart-sync # copy deploy/alertthread.rules.yaml into the chart
just up/down    # compose dev stack (podman or docker, auto-detected)
just e2e        # the asserted end-to-end demo (needs a container engine)
just ci         # the fast half of CI: lint, version check, test + coverage gate, docs, licences
just pre-push   # ci + the workflow lint + the alert-rule check + the image build + e2e
```

**`just ci` is not all of CI, and saying it was cost us a break in `main` once.** Three jobs
need a container engine and are separate recipes — the image build, `just e2e`, and the
`promtool` check on `deploy/alertthread.rules.yaml` — `just chart` needs `helm`, and
`just check-workflows` needs `actionlint`. `just pre-push` is `ci` plus those five, and it
fails with a legible message when a tool is missing rather than skipping the job.

Three checks are private recipes that a public one calls, so `just --list` stays the set
above: `check-deps` and `check-links` from `lint` and `docs`, and **`check-version`** from
`ci` — `scripts/release-version.py`, which fails when the four places the version lives
disagree or when one of them loses its `x-release-please-version` marker. `check-workflows`
runs `actionlint` over every workflow and is reached by `pre-push`.

**Run `just pre-push` before proposing a change.** Two CI jobs are still outside it and have to
be run by hand when relevant: `just test-pg` (needs `just up`, and `pre-push` will not tear
down a dev stack to get it) and the MSRV job (needs the 1.94 toolchain installed).

Do not hand-run `cargo` commands that a recipe already wraps — the recipe carries the flags CI
uses.

---

## Footguns

Real ones, discovered the hard way. Each has cost somebody time.

- **`sqlx` 0.9 requires `SqlSafeStr`** — query functions accept only `&'static str` or an
  explicit `AssertSqlSafe` wrapper. Do not build query strings dynamically to work around
  this; restructure the query.
- **MSRV is 1.94**, dictated by `sqlx` 0.9, not chosen. Do not use newer language features
  without raising it deliberately.
- **SELinux, on Fedora.** Bind mounts in `compose.yaml` need `:z` or `:Z`, or the container
  gets permission denied for reasons that look nothing like SELinux. Docker users will not
  reproduce this, which is exactly why it is easy to break for everyone else.
- **The container engine is detected, not assumed.** `just` picks podman or docker,
  whichever is installed. Never hardcode either one in a recipe, a workflow, or
  `compose.yaml` — use `{{ engine }}` / `{{ compose }}`. CI relies on this to run the same
  recipes on a Docker-only runner.
- **`chrono`, not `jiff`**, throughout — deliberate, for `sqlx` compatibility. Do not
  introduce a second time type.
- **Slack allows ~1 `chat.postMessage` per second per channel**, thread replies included.
  Never post in a request handler. Never bypass the rate limiter.
- **`chat.update` does not notify, bump, or mark unread.** An in-place edit alone is
  invisible to anyone watching the channel live. This is why resolve does both an edit and
  a thread reply.
- **Alertmanager `max_alerts` must be `0`** and `send_resolved` must be `true`. Non-zero
  `max_alerts` silently truncates alerts out of the webhook body, and the symptom
  (orphaned resolves) points nowhere near the cause.
- **The relay cannot alert on itself through itself.** `deploy/alertthread.rules.yaml` requires
  a documented Alertmanager route to a *direct* Slack receiver
  ([`how-to/alert-on-the-relay.md`](docs/src/how-to/alert-on-the-relay.md)). Shipping the rules
  without that documentation is worse than shipping no rules. The warning lives in the YAML
  itself as well as in the docs, because that file travels into charts and clusters on its own,
  and a test asserts it is still there.
- **The container runs read-only, and SQLite is the only reason that needs thought.** The
  database, its `-wal` and its `-shm` all live beside `storage.url` and need a declared writable
  mount; `/tmp` needs one too, for the spill file a large SQLite statement would want. If a
  read-only rootfs breaks something, the fix is another declared mount, never relaxing the flag.
  `compose.yaml` runs the relay under the full set of flags, so `just e2e` proves them, and
  `charts/alertthread` sets the Kubernetes equivalents so `just chart` proves those. The two
  are separate copies of the same settings: neither check sees a regression in the other's.
- **Nothing may be mounted inside another mount in the chart.** The image is `scratch` and the
  root filesystem is read-only, so there is no writable directory for the kubelet to create an
  inner mount point in. `/etc/alertthread/config`, `/etc/alertthread/secrets/*` and
  `/etc/alertthread/templates` are siblings for that reason, and a test asserts it.
- **`deploy/alertthread.rules.yaml` is the original; the chart's copy is generated.** Helm
  cannot read outside its own chart directory. Edit `deploy/`, run `just chart-sync`, and
  `just chart` will tell you if you forgot.
- **`/healthz`, `/readyz` and `/metrics` are never authenticated.** A probe carries no
  credential, so a `401` on the first two is a pod Kubernetes restarts for ever or one that never
  joins the Service, and a `401` on `/metrics` breaks the relay's own alerting. Only
  `POST /webhook` can be closed, and only when `server.auth_token` is set.

---

## Commits and PRs

- Conventional Commits — `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`.
  `release-please` derives the changelog *and the next version number* from these, so the
  prefix matters. `feat:` bumps the minor, `fix:` the patch, and a `!` or a
  `BREAKING CHANGE:` footer bumps the minor while the project is pre-1.0.
- **Never edit a version by hand.** Four places carry it and `release-please` owns all four —
  `Cargo.toml`'s `[workspace.package]` version and its three path dependencies, and
  `Chart.yaml`'s `version` and `appVersion`. `just check-version` fails when they disagree.
- Explain **why** in the body, not what. The diff shows what.
- Never commit secrets. Slack tokens are `xoxb-…`; if one is ever committed, it is burned —
  rotate it, do not just amend the commit.
- Branch for changes; do not commit to `main` directly.

## Definition of done

1. `just pre-push` passes — `ci` including the per-crate coverage gate, plus the alert-rule
   check, the chart checks, the image build and the end-to-end demo.
2. Tests exist per the table above, and fail without the change.
3. `just mutants` exits `0` — no surviving mutants in `core` or `store`, and any new
   survivor it prints elsewhere is triaged in the PR.
4. Docs updated in the correct quadrant.
5. No new path can result in an alert going unposted.
6. Public items have doc comments.
7. New config appears in `reference/configuration.md`; new metrics in
   `reference/metrics.md`.

## When to stop and ask

- A settled ADR decision looks wrong.
- The change would let an alert be silently dropped.
- A new dependency is needed — this project deliberately runs a small dependency surface,
  and "we call three endpoints, an SDK is not warranted" is a decision already made.
- Correctness would require a schema change to an already-released migration.
