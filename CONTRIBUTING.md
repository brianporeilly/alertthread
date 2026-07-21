# Contributing

Thanks for looking at `alertthread`.

Before changing anything, read [`AGENTS.md`](AGENTS.md). It holds the constraints this
codebase works under and applies to humans and agents alike. This file covers only the
mechanics of getting set up.

## Setup

You need a Rust toolchain and a container engine — **either podman or docker**. The
toolchain version is pinned in `rust-toolchain.toml` and rustup will install it
automatically on first build.

```
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Task runner and test tooling. cargo-binstall fetches prebuilt binaries,
# which is dramatically faster than compiling each of these from source.
cargo install cargo-binstall
cargo binstall just cargo-nextest cargo-llvm-cov cargo-mutants cargo-deny mdbook
```

### Container engine

The dev stack and the image build need a container engine. The recipes **detect podman or
docker and use whichever you have**, so neither is a hard dependency and no configuration
is needed. This is also what lets CI run the same recipes on a Docker-only GitHub runner
rather than installing podman to satisfy them.

If you have both installed, docker wins the tie-break — not a statement of preference, but
because docker's Compose v2 is a self-contained plugin, while `podman compose` delegates to
an external provider that needs podman's API socket listening. The tie-break lands on
whichever works with no further setup. Override it explicitly:

```
CONTAINER_ENGINE=podman just up
```

`just up`, `just down`, `just test-pg` and the image build fail early with a clear message
if no usable engine is found, rather than surfacing it as a confusing compose error later.
The check contacts the engine rather than just looking for the binary, because an engine
that is installed but not listening is the more common failure.

Separately, and unrelated to which engine you run: **testcontainers is deliberately not
used**. It needs a docker socket and its Ryuk reaper misbehaves under rootless podman.
Integration tests use `#[sqlx::test]`, which creates an isolated database per test.

Verify everything works:

```
just ci
```

## The loop

```
just            # list every recipe
just test-fast  # inner loop — no coverage instrumentation
just test       # pre-push — instrumented, with the coverage gate
just ci         # everything CI runs
```

**`just` is the only entry point, and CI runs these same recipes.** If `just ci` passes
locally it passes in CI. Do not hand-run `cargo` commands that a recipe already wraps — the
recipe carries the flags CI uses, and running the bare command silently drops them.

`just test-fast` exists because coverage instrumentation costs 2–3× on runtime, which is too
slow for a tight edit-test cycle. It is a convenience, not a way around the gate.

## What the gates check

Three things fail the build, and all three are meant to.

**Per-crate coverage thresholds.** `alertthread-core` must be at 100%; the other crates at
95%. `main.rs` and `dev/slack-mock` are excluded outright. The thresholds are per-crate
rather than one workspace number so that critical logic cannot hide behind
easy-to-cover code dragging an average up.

Do not chase the threshold with tests that assert nothing. A test that calls a function to
touch its lines and checks no output scores full coverage and catches nothing — here it
manufactures false confidence about the alerting path, which is worse than the uncovered
line was. If you lower a threshold or add an exclusion, say so in the PR and justify it.

**Dependency direction.** `alertthread-core` is pure: no `tokio`, `sqlx`, `axum`, `reqwest`,
no I/O of any kind. `scripts/check-deps.sh` inspects the resolved `cargo tree` — not the
manifests — because the failure that actually happens is a pure-looking crate pulling a
runtime in through some dependency's default features.

**Mutation testing.** Required for any change to `alertthread-core`: `just mutants`, no
survivors. Coverage proves a line ran; it does not prove a test would have caught the
regression. For a system whose worst failure is silence, "would we have noticed?" is the
only question worth asking, and this is the tool that answers it.

## Why there is a Python script in a Rust repo

`scripts/coverage-gate.py` parses `cargo-llvm-cov`'s JSON output and enforces the per-crate
thresholds. Python is a deliberate, narrow exception, and worth explaining because mdBook
was chosen over MkDocs partly to keep a Python toolchain out of this project.

The distinction is between a *toolchain* and an *interpreter*. MkDocs would have meant pip,
a virtualenv, a `requirements.txt`, plugin version drift, and a second dependency ecosystem
to keep patched. This is a single stdlib-only script with **no third-party imports and
nothing to install** — `python3` is present by default on Fedora, on Debian and Ubuntu, and
on every GitHub Actions runner.

The alternatives were each worse:

- **Bash + `jq`** — adds a genuine external binary that is *not* installed by default on
  Fedora, to do JSON manipulation in the language least suited to it.
- **A Rust `xtask`** — the most idiomatic answer, and the one to switch to if this script
  ever grows. Today it would add a compiled workspace member that itself needs a coverage
  exclusion, and it puts a compile step in front of every CI coverage run, to replace
  roughly 150 lines of JSON arithmetic.

If the gate logic outgrows a single file, move it to an `xtask`. Until then this is the
smaller dependency surface, which is the principle the repo is actually applying.

## Documentation

Docs ship with the change, not after it. They use [Diátaxis](https://diataxis.fr/) and live
in `docs/src/`:

| Quadrant | Directory | Answers |
|---|---|---|
| Tutorial | `docs/src/tutorials/` | "Teach me this, I'm new" |
| How-to | `docs/src/how-to/` | "I have a specific goal" |
| Reference | `docs/src/reference/` | "What are the exact options?" |
| Explanation | `docs/src/explanation/` | "Why is it built this way?" |

**Pick one quadrant per page and hold the line.** The most common failure is a how-to guide
drifting into explanation. If a page needs both, write two and link them.

A new config option is not merged until it appears in `reference/configuration.md`; a new
metric until it appears in `reference/metrics.md`. Say in your PR which quadrant you touched
and why.

ADRs live in `docs/src/adr/` and are append-only — supersede, never rewrite.

## Commits and pull requests

[Conventional Commits](https://www.conventionalcommits.org/) — `feat:`, `fix:`, `docs:`,
`refactor:`, `test:`, `chore:`. `release-please` derives the changelog from these, so the
prefix matters.

**Explain why in the body, not what.** The diff already shows what changed.

Branch for changes; do not commit to `main`.

Never commit secrets. Slack bot tokens look like `xoxb-…`. If one is ever committed it is
burned — rotate it, do not just amend the commit.

## Before you open a PR

1. `just ci` passes, including the coverage gate.
2. Tests exist per the table in AGENTS.md, and fail without your change.
3. Changes to `alertthread-core` leave no surviving mutants.
4. Docs updated in the right quadrant.
5. **No new path can result in an alert going unposted.**

## When to stop and ask

- A settled ADR decision looks wrong. Say so explicitly and argue it — do not quietly
  implement something else.
- The change would let an alert be silently dropped.
- You need a new dependency. This project deliberately runs a small dependency surface, and
  "we call three endpoints, an SDK is not warranted" is a decision already made.
- Correctness would need a schema change to an already-released migration.

## Licence

By contributing you agree that your work is dual-licensed under MIT and Apache-2.0, matching
the project.
