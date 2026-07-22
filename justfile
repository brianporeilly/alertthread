# alertthread task runner.
#
# `just` is the only entry point. CI runs these same recipes, so if it passes
# locally it passes in CI. Do not hand-run cargo commands that a recipe already
# wraps — the recipe carries the flags CI uses.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Container engine for the dev stack and image builds. Detected, not hardcoded.
#
# Docker wins the tie-break because its Compose v2 is self-contained, whereas
# `podman compose` needs podman's API socket listening — which on a GitHub
# runner, where both are installed, it is not.
#
# Force either one with CONTAINER_ENGINE=podman (or =docker).
engine := env_var_or_default("CONTAINER_ENGINE", `command -v docker >/dev/null 2>&1 && echo docker || echo podman`)
compose := engine + " compose"

# Coverage output lives here; .gitignore'd.
coverage_dir := justfile_directory() / "coverage"
llvm_cov_json := coverage_dir / "llvm-cov.json"

# Each backend is gated by the run that can exercise it. See ROADMAP.md's
# coverage policy.
pg_cov_json := coverage_dir / "llvm-cov-postgres.json"

# Its own target directory: cargo-llvm-cov reports over every instrumented
# object it finds, so sharing one with `just test` lets that profile's SQLite
# build into this profile's report and fails the gate on code this build does
# not contain.
pg_target_dir := justfile_directory() / "target" / "llvm-cov-pg"

# Policy, stated in ROADMAP.md; the gate script enforces it independently.
ignore_regex := '(crates/app/src/main\.rs|dev/slack-mock/)'

[private]
default:
    @just --list --unsorted

# ---------------------------------------------------------------------------
# Formatting and linting
# ---------------------------------------------------------------------------

# Format all Rust sources.
fmt:
    cargo fmt --all

# Check formatting and run clippy with warnings denied.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    @just check-deps

# Enforce the dependency direction: core stays pure, layering is not inverted.
# Private so `just --list` shows exactly the recipe set AGENTS.md documents.
# It is still directly invokable, and `lint` calls it.
[private]
check-deps:
    ./scripts/check-deps.sh

# ---------------------------------------------------------------------------
# Testing
# ---------------------------------------------------------------------------

# Unit + SQLite integration tests, instrumented, with the per-crate coverage gate.
test:
    mkdir -p {{ coverage_dir }}
    cargo llvm-cov --workspace \
        --ignore-filename-regex '{{ ignore_regex }}' \
        --json --output-path {{ llvm_cov_json }} \
        nextest
    ./scripts/coverage-gate.py {{ llvm_cov_json }}

# Instrumentation costs 2-3x on runtime, which is too slow for a tight
# edit-test cycle. This is a convenience, not a way around the gate: CI runs
# `just ci`, which does not use this recipe.
#
# The same tests without instrumentation — for the inner loop.
test-fast:
    cargo nextest run --workspace

# PostgreSQL conformance suite, instrumented, with its own coverage gate. Needs `just up`.
test-pg: check-engine
    #!/usr/bin/env bash
    set -euo pipefail
    export DATABASE_URL="${DATABASE_URL:-postgres://alertthread:alertthread@localhost:5432/alertthread}"
    # Check readiness by exec'ing into the container rather than calling
    # pg_isready on the host: the postgres client is not installed on a stock
    # Fedora workstation or a GitHub runner, but it is always present in the
    # postgres image.
    if ! {{ compose }} exec -T postgres pg_isready -U alertthread >/dev/null 2>&1; then
        echo "PostgreSQL is not reachable — run 'just up' first." >&2
        exit 1
    fi
    mkdir -p {{ coverage_dir }}
    export CARGO_TARGET_DIR="{{ pg_target_dir }}"
    # --no-default-features drops the SQLite backend, so this measures
    # PostgreSQL against the tests that just ran.
    cargo llvm-cov --package alertthread-store \
        --no-default-features --features postgres \
        --ignore-filename-regex '{{ ignore_regex }}' \
        --json --output-path {{ pg_cov_json }} \
        nextest
    ./scripts/coverage-gate.py {{ pg_cov_json }} --profile store-postgres

# Coverage report only, no gate — for finding the gaps.
coverage:
    mkdir -p {{ coverage_dir }}
    cargo llvm-cov --workspace \
        --ignore-filename-regex '{{ ignore_regex }}' \
        --html --output-dir {{ coverage_dir }} \
        nextest
    @echo "Report: {{ coverage_dir }}/html/index.html"

# Mutation testing. Required for any change to alertthread-core.
mutants *ARGS:
    # postgres.rs is not compiled by this build, so every mutant in it would
    # report as a survivor; `just mutants-pg` tests it instead.
    #
    # --exclude-re suppresses one equivalent mutant, by name because the line
    # moves. ROADMAP known open item #5 carries the argument.
    cargo mutants --workspace --test-tool nextest \
        --exclude 'crates/store/src/postgres.rs' \
        --exclude-re 'replace > with >= in persist_group' {{ ARGS }}

# Mutation testing for the PostgreSQL backend. Needs `just up`.
#
# The other half of `just mutants`. Same reasoning as the coverage gate: the
# backend that needs a server is tested by the run that has one.
mutants-pg *ARGS: check-engine
    #!/usr/bin/env bash
    set -euo pipefail
    export DATABASE_URL="${DATABASE_URL:-postgres://alertthread:alertthread@localhost:5432/alertthread}"
    if ! {{ compose }} exec -T postgres pg_isready -U alertthread >/dev/null 2>&1; then
        echo "PostgreSQL is not reachable — run 'just up' first." >&2
        exit 1
    fi
    # --file, not --exclude: with the SQLite backend switched off, mutating
    # sqlite.rs here would be the mirror image of the problem above.
    cargo mutants --package alertthread-store --test-tool nextest \
        --no-default-features --features postgres \
        --file 'crates/store/src/postgres.rs' {{ ARGS }}

# ---------------------------------------------------------------------------
# Docs
# ---------------------------------------------------------------------------

# Build the mdBook and check its links.
docs:
    mdbook build docs
    @just check-links

# Verify no intra-repo Markdown link points at a file that does not exist.
[private]
check-links:
    ./scripts/check-links.sh

# ---------------------------------------------------------------------------
# Dev stack
# ---------------------------------------------------------------------------

# Start the compose dev stack (postgres + slack-mock) on podman or docker.
up: check-engine
    # --build so a changed slack-mock does not silently run as a stale image.
    # Layer caching makes the no-op case cheap.
    {{ compose }} up -d --build
    @echo "Waiting for PostgreSQL..."
    @timeout 60 bash -c 'until {{ compose }} exec -T postgres pg_isready -U alertthread >/dev/null 2>&1; do sleep 1; done'
    @echo "Dev stack up ({{ engine }})."

# Stop the dev stack and remove its volumes.
down: check-engine
    {{ compose }} down --volumes

# Fail early and legibly when no usable container engine is present, rather
# than surfacing it as a confusing compose error three commands later.
# Private so `just --list` shows exactly the recipe set AGENTS.md documents.
[private]
check-engine:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v {{ engine }} >/dev/null 2>&1; then
        echo "error: no container engine found — looked for podman, then docker." >&2
        echo "       install one, or set CONTAINER_ENGINE to the name of yours." >&2
        exit 1
    fi
    if ! {{ compose }} version >/dev/null 2>&1; then
        echo "error: '{{ compose }}' is not usable." >&2
        echo "       podman: needs 4.7+, plus podman-compose or docker-compose" >&2
        echo "       docker: needs the Compose v2 plugin" >&2
        exit 1
    fi
    # `compose version` only proves the provider resolves — it never contacts
    # the engine. `compose ps` does, which is what catches an engine that is
    # installed but not listening. That distinction is not academic: it is the
    # exact failure this preflight was added and then had to be fixed for.
    if ! {{ compose }} ps >/dev/null 2>&1; then
        echo "error: '{{ compose }}' cannot reach the {{ engine }} engine." >&2
        echo "       podman: 'systemctl --user start podman.socket'" >&2
        echo "       docker: check the daemon is running and you can reach it" >&2
        echo "       or select the other engine with CONTAINER_ENGINE=..." >&2
        exit 1
    fi

# Build the release image and smoke-test that it actually runs.
#
# The static musl build on a scratch base is the highest-risk assumption in
# ADR 001. Building it on every PR is what stops it silently regressing: a
# dependency that cannot link statically breaks the image, not the test suite.
[private]
image TAG="localhost/alertthread:dev": check-engine
    {{ engine }} build -t {{ TAG }} .
    {{ engine }} run --rm {{ TAG }}
    @{{ engine }} images {{ TAG }}

# ---------------------------------------------------------------------------
# CI
# ---------------------------------------------------------------------------

# Everything CI runs, including the coverage gate.
ci: lint test docs
    cargo deny check
    @echo
    @echo "just ci: all checks passed."
