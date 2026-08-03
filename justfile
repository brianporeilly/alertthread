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

# Where cargo-mutants writes its verdict. The `mutants` recipe reads the
# survivor lists back out of here to decide its own exit code.
mutants_dir := justfile_directory() / "mutants.out"

# The Helm chart. Phase 6 PR B publishes it as an OCI artifact from here.
chart_dir := justfile_directory() / "charts" / "alertthread"

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

# Assert the four places the version lives agree, and that each is still
# annotated for release-please. Private for the same reason `check-deps` is;
# `ci` calls it.
[private]
check-version:
    @version=$(./scripts/release-version.py) && echo "just check-version: ${version}, everywhere."

# actionlint over every workflow. A YAML or expression error in a release
# workflow surfaces at the first tag, which is the worst possible moment.
#
# Not in `just ci`: actionlint is not part of a Rust toolchain, same class as
# `check-rules` and `chart`. `just pre-push` reaches it and it has its own CI job.
[private]
check-workflows:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v actionlint >/dev/null 2>&1; then
        echo "error: actionlint is not installed — https://github.com/rhysd/actionlint" >&2
        echo "       go install github.com/rhysd/actionlint/cmd/actionlint@latest" >&2
        exit 1
    fi
    actionlint

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
    #!/usr/bin/env bash
    set -uo pipefail
    # Runs the whole workspace and prints every survivor. The *exit code* is
    # narrowed to `crates/core` and `crates/store`.
    #
    # Why narrowed rather than excluded (ROADMAP known open item #10): the app
    # crate's survivors are *unkilled*, not unkillable — process lifecycle,
    # log-only branches, field accessors. Naming them in --exclude-re would
    # assert they are equivalent, which is false, and would hide a real one
    # arriving at the same site later. Narrowing the gate keeps them printed
    # and visible on every run while giving the exit code a meaning that a
    # correct tree can actually satisfy. A gate that is always red carries no
    # information and gets waved through.
    #
    # postgres.rs is not compiled by this build, so every mutant in it would
    # report as a survivor; `just mutants-pg` tests it instead.
    #
    # --exclude-re suppresses one equivalent mutant, by name because the line
    # moves. ROADMAP known open item #5 carries the argument. It is in `store`,
    # which this gate covers, so it has to stay.
    cargo mutants --workspace --test-tool nextest \
        --exclude 'crates/store/src/postgres.rs' \
        --exclude-re 'replace > with >= in persist_group' {{ ARGS }}
    status=$?

    # 2 = mutants missed, 3 = mutants timed out. Anything else is the run
    # itself failing — a usage error, a build error, a failing baseline — and
    # is never something to reinterpret.
    if [[ $status -ne 0 && $status -ne 2 && $status -ne 3 ]]; then
        exit $status
    fi

    survivors=$(cat "{{ mutants_dir }}/missed.txt" "{{ mutants_dir }}/timeout.txt" 2>/dev/null || true)
    if [[ -z "$survivors" ]]; then
        echo
        echo "just mutants: no surviving mutants anywhere in the workspace."
        exit 0
    fi

    echo
    echo "Surviving mutants across the workspace:"
    echo "$survivors" | sed 's/^/  /'

    gated=$(echo "$survivors" | grep -E '^crates/(core|store)/' || true)
    if [[ -n "$gated" ]]; then
        echo
        echo "GATED — these are in core or store, where no mutant may survive:" >&2
        echo "$gated" | sed 's/^/  /' >&2
        exit 1
    fi

    echo
    echo "just mutants: survivors above are outside core and store; triage them"
    echo "              (AGENTS.md, 'Mutation testing'). The gate passes."

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

# Validate deploy/alertthread.rules.yaml with promtool, from the Prometheus image.
#
# The rules are a shipped artefact and PromQL is not compiled by anything else in
# this repo, so without this a syntax error reaches a cluster. promtool comes from
# the same pinned Prometheus image the demo stack runs, which is why this needs a
# container engine and therefore cannot live in `just ci`.
#
# The other half of the check is in Rust: crates/app/tests/prometheus_rule.rs
# asserts every metric and label value the rules name is one this build exports,
# which promtool cannot know.
#
# Validate the shipped Prometheus alert rules.
[private]
check-rules: check-engine
    {{ engine }} run --rm \
        -v {{ justfile_directory() }}/deploy:/rules:ro,z \
        --entrypoint promtool \
        docker.io/prom/prometheus:v3.1.0 check rules /rules/alertthread.rules.yaml

# ---------------------------------------------------------------------------
# Helm chart
# ---------------------------------------------------------------------------

# Lint and template the Helm chart, and assert what it renders.
#
# Separate from `just ci` for the same reason `check-rules` is: it needs a tool
# that is not part of a Rust toolchain. It has its own CI job and `just pre-push`
# reaches it.
#
# The interesting half is scripts/chart-test.py, not `helm lint` — lint checks
# that a chart is well-formed, which an empty PrometheusRule spec also is. The
# assertions are what hold ROADMAP known open item 18's hardening in place.
chart:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v helm >/dev/null 2>&1; then
        echo "error: helm is not installed — https://helm.sh/docs/intro/install/" >&2
        exit 1
    fi
    helm lint {{ chart_dir }} --strict \
        --set config.slack.default_channel='#alerts' \
        --set slack.existingSecret=alertthread-slack
    ./scripts/chart-test.py

# Copy deploy/alertthread.rules.yaml into the chart.
#
# Helm cannot read a file outside its own chart directory, so the chart carries a
# copy and `just chart` fails when the two differ. deploy/ is the original; this
# is the only supported way to update the copy.
chart-sync:
    cp {{ justfile_directory() }}/deploy/alertthread.rules.yaml \
       {{ chart_dir }}/files/alertthread.rules.yaml
    @echo "Synced the alert rules into {{ chart_dir }}/files/."

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

# Bring up the full end-to-end demo to watch by hand: relay + Prometheus +
# Alertmanager + the fake Slack. Follows tutorials/01-first-alert-locally.md.
# The demo alert fires immediately and resolves itself ~60s later, so open the
# UI right away. Use `just e2e` for the asserted version.
demo: check-engine
    {{ compose }} --profile demo up -d --build
    @echo
    @echo "Demo stack up ({{ engine }}). Open the fake Slack and watch:"
    @echo "    Slack UI      http://localhost:8081"
    @echo "    Prometheus    http://localhost:9090/alerts"
    @echo "    Alertmanager  http://localhost:9093"
    @echo
    @echo "Five alerts fire now and resolve themselves in ~60s. 'just demo-down' when done."

# Stop the demo stack and remove its volumes.
demo-down: check-engine
    {{ compose }} --profile demo down --volumes

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
    # --version, because the relay refuses to start without a Slack token (D8) and this
    # check is about whether the static binary executes on `scratch` at all.
    {{ engine }} run --rm {{ TAG }} --version
    # And that the refusal is a clean non-zero rather than a hang or a panic.
    @if {{ engine }} run --rm {{ TAG }} 2>/dev/null; then \
        echo "expected the relay to refuse to start with no token" >&2; exit 1; \
    fi
    @{{ engine }} images {{ TAG }}

# End-to-end demo, asserted: bring up the real stack, let a Prometheus rule fire,
# and check the mock's state shows it threaded and then resolved in place. This
# is the Phase 4 exit criterion as a gate. Runnable locally, invoked by its own
# CI job — deliberately NOT part of `just ci`, so the fast checks stay fast.
e2e: check-engine
    COMPOSE="{{ compose }}" ./scripts/e2e.sh

# ---------------------------------------------------------------------------
# CI
# ---------------------------------------------------------------------------

# NOT all of CI. Three jobs need a container engine and are separate recipes —
# `image`, `e2e` and `check-rules` — and a fourth needs helm, which is `chart`.
# `just pre-push` is the full local equivalent.
#
# The fast half of CI: lint, the version check, tests + the coverage gate, docs, licences.
ci: lint check-version test docs
    cargo deny check
    @echo
    @echo "just ci: all checks passed. This is not all of CI — see 'just pre-push'."

# This exists because `just ci` passing was documented as meaning CI passes, and it
# did not: the image job and the end-to-end job were CI-only, and that gap let a
# real break reach CI once (ROADMAP known open item 11).
#
# `check-engine` is first so a missing container engine fails immediately with a
# legible message rather than after the several minutes `ci` takes — and fails
# rather than skipping. A check that quietly passes is the failure mode this
# project keeps legislating against.
#
# Two CI jobs remain outside it, deliberately: `test-pg` needs the compose stack
# up and would tear down a running dev stack to get it, and the MSRV job needs the
# 1.94 toolchain installed. Run those two by hand — `just up && just test-pg` —
# when you touch the store or reach for a newer language feature.
#
# Everything CI runs that one machine can: `ci` plus the workflow lint, the chart
# and the three container jobs.
pre-push: check-engine check-workflows check-rules chart ci image e2e
    @echo
    @echo "just pre-push: workflows + rules + chart + ci + image + e2e all passed."
    @echo "Not covered here: 'just test-pg' (needs 'just up') and the MSRV job."
