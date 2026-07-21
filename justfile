# alertthread task runner.
#
# `just` is the only entry point. CI runs these same recipes, so if it passes
# locally it passes in CI. Do not hand-run cargo commands that a recipe already
# wraps — the recipe carries the flags CI uses.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Container engine for the dev stack and image builds.
#
# Detected rather than hardcoded: this is not a podman-only project. Whichever
# engine you have, the recipes work unchanged — which is what lets CI run these
# same recipes on a Docker-only GitHub runner instead of installing podman to
# satisfy them.
#
# The order is only a tie-break for machines that have both, and it favours
# podman because the compose file and the SELinux notes in the docs are written
# against it. Force the other with CONTAINER_ENGINE=docker.
engine := env_var_or_default("CONTAINER_ENGINE", `command -v podman >/dev/null 2>&1 && echo podman || echo docker`)
compose := engine + " compose"

# Coverage output lives here; .gitignore'd.
coverage_dir := justfile_directory() / "coverage"
llvm_cov_json := coverage_dir / "llvm-cov.json"

# main.rs is wiring and signal handling; slack-mock is dev tooling, not shipped.
# Both exclusions are policy, stated in ROADMAP.md, and enforced identically by
# the gate script — this regex only keeps them out of the HTML report too.
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

# PostgreSQL conformance suite. Needs `just up` first.
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
    cargo nextest run --package alertthread-store --features postgres -- --include-ignored

# Coverage report only, no gate — for finding the gaps.
coverage:
    mkdir -p {{ coverage_dir }}
    cargo llvm-cov --workspace \
        --ignore-filename-regex '{{ ignore_regex }}' \
        --html --output-dir {{ coverage_dir }} \
        nextest
    @echo "Report: {{ coverage_dir }}/html/index.html"

# Coverage proves a line ran; it does not prove a test would have caught the
# regression. For a system whose worst failure is silence, "would we have
# noticed?" is the question that matters, and this is the tool that answers it.
#
# Mutation testing. Required for any change to alertthread-core.
mutants *ARGS:
    cargo mutants --workspace --in-place=false {{ ARGS }}

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
