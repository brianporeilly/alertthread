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
# docker because docker's Compose v2 is a self-contained plugin, whereas
# `podman compose` delegates to an external provider that then needs podman's
# API socket listening. Preferring docker means the tie-break lands on the one
# that works with no further setup.
#
# GitHub runners are exactly this case: they ship podman *and* docker, with
# podman's socket inactive. Preferring podman there picks an engine whose
# compose cannot connect — which is precisely how this was found.
#
# Force either one with CONTAINER_ENGINE=podman (or =docker).
engine := env_var_or_default("CONTAINER_ENGINE", `command -v docker >/dev/null 2>&1 && echo docker || echo podman`)
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
    # No --in-place: cargo-mutants copies the tree to a scratch directory by
    # default, so the working tree is never left holding a mutated source file
    # if a run is interrupted. `--in-place` is the opt-out and takes no value.
    cargo mutants --workspace {{ ARGS }}

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
