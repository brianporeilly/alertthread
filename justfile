# alertthread task runner.
#
# `just` is the only entry point. CI runs these same recipes, so if it passes
# locally it passes in CI. Do not hand-run cargo commands that a recipe already
# wraps — the recipe carries the flags CI uses.

set shell := ["bash", "-euo", "pipefail", "-c"]

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
test-pg:
    #!/usr/bin/env bash
    set -euo pipefail
    export DATABASE_URL="${DATABASE_URL:-postgres://alertthread:alertthread@localhost:5432/alertthread}"
    if ! pg_isready -d "$DATABASE_URL" >/dev/null 2>&1; then
        echo "PostgreSQL is not reachable at $DATABASE_URL — run 'just up' first." >&2
        exit 1
    fi
    cargo nextest run --workspace --features postgres -- --include-ignored

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

# Start the podman compose dev stack (postgres + slack-mock).
up:
    # --build so a changed slack-mock does not silently run as a stale image.
    # Layer caching makes the no-op case cheap.
    podman compose up -d --build
    @echo "Waiting for PostgreSQL..."
    @timeout 60 bash -c 'until podman compose exec -T postgres pg_isready -U alertthread >/dev/null 2>&1; do sleep 1; done'
    @echo "Dev stack up."

# Stop the dev stack and remove its volumes.
down:
    podman compose down --volumes

# ---------------------------------------------------------------------------
# CI
# ---------------------------------------------------------------------------

# Everything CI runs, including the coverage gate.
ci: lint test docs
    cargo deny check
    @echo
    @echo "just ci: all checks passed."
