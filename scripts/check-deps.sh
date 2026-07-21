#!/usr/bin/env bash
#
# Enforce the dependency direction from AGENTS.md and ROADMAP.md:
#
#   app ──→ store ──→ core
#    └────→ slack ──→ core
#                     core ──→ (nothing with I/O)
#
# AGENTS.md promises this is "enforced by Cargo, and CI fails if it is
# violated". This script is that enforcement. It is deliberately a real check
# against the resolved dependency graph rather than a review convention.
#
# `cargo tree` is used because it reports the *transitive* graph after feature
# resolution. Reading the manifests instead would miss the case that actually
# happens in practice: a pure-looking crate that pulls tokio in through an
# innocent-looking dependency's default features.

set -euo pipefail

# Crates that must never appear anywhere beneath alertthread-core. These are the
# four named in AGENTS.md plus the runtimes and drivers they imply — a core that
# has `mio` or `socket2` in its tree is doing I/O whatever the manifest says.
BANNED_IN_CORE=(
    tokio
    sqlx
    sqlx-core
    axum
    reqwest
    hyper
    mio
    socket2
    libsqlite3-sys
    async-std
    smol
    rusqlite
    # Pulled in by chrono's `clock` feature on Linux. Its presence beneath the core means
    # `Utc::now()` is nameable there, which is the one I/O the core is most likely to
    # acquire by accident — a clock read looks like arithmetic at the call site.
    iana-time-zone
)

# Crates that must never depend on each other, as `dependent:forbidden`.
# The core is the only crate with a purity rule; these encode the *direction* of
# the rest of the graph, so a future refactor cannot quietly invert an edge.
FORBIDDEN_EDGES=(
    "alertthread-core:alertthread-store"
    "alertthread-core:alertthread-slack"
    "alertthread-core:alertthread"
    "alertthread-store:alertthread-slack"
    "alertthread-store:alertthread"
    "alertthread-slack:alertthread-store"
    "alertthread-slack:alertthread"
)

failed=0

# `-e normal` excludes dev- and build-dependencies: a dev-dependency on tokio in
# the core's own test suite would be a smell, but it does not ship, and the rule
# we are enforcing is about what ends up in the binary.
core_tree="$(cargo tree --package alertthread-core --edges normal --prefix none --no-dedupe 2>/dev/null)"

echo "==> Checking alertthread-core is free of I/O dependencies"
for banned in "${BANNED_IN_CORE[@]}"; do
    # cargo tree prints "name vX.Y.Z (path)"; anchor on the name field only so
    # that e.g. `tokio-util` does not match `tokio`.
    if grep -qE "^${banned} v" <<<"$core_tree"; then
        echo "  FAIL: alertthread-core depends on '${banned}'"
        echo "        The core is pure: no tokio, sqlx, axum, reqwest, no I/O of any kind."
        echo "        If you need I/O here, the design is wrong — move the I/O to the"
        echo "        shell (crates/app) and pass its result in. See AGENTS.md."
        echo "        Dependency path:"
        cargo tree --package alertthread-core --edges normal --invert "${banned}" 2>/dev/null | sed 's/^/          /' || true
        failed=1
    fi
done

# AGENTS.md: "no clock reads. Time enters as a `now: DateTime<Utc>` parameter." Banning
# `iana-time-zone` above catches chrono's `clock` feature on Linux, but chrono also has a
# bare `now` feature that provides `Utc::now()` with no extra crates behind it — nothing
# in the dependency *graph* would change. So the feature set is checked directly.
#
# Under resolver 3 `cargo tree --package` resolves features for the selected package
# alone, so this stays honest once the shell enables `clock` for itself in Phase 4.
echo "==> Checking alertthread-core cannot read a clock"
core_features="$(cargo tree --package alertthread-core --edges normal --prefix none \
    --format '{p} {f}' 2>/dev/null | grep -E '^chrono v' || true)"

if [[ -z "$core_features" ]]; then
    echo "  FAIL: alertthread-core does not depend on chrono at all."
    echo "        This check assumes it does; if that changed deliberately, update it."
    failed=1
elif grep -qE '(^|,)(clock|now)(,|$)' <<<"${core_features#* }"; then
    echo "  FAIL: alertthread-core's chrono has a clock feature enabled:"
    echo "          ${core_features}"
    echo "        That makes Utc::now() callable from the pure core. Time enters as a"
    echo "        'now' parameter — the shell reads the clock and passes it in."
    failed=1
fi

echo "==> Checking crate-to-crate dependency direction"
for edge in "${FORBIDDEN_EDGES[@]}"; do
    dependent="${edge%%:*}"
    forbidden="${edge##*:}"
    tree="$(cargo tree --package "${dependent}" --edges normal --prefix none --no-dedupe 2>/dev/null)"
    if grep -qE "^${forbidden} v" <<<"$tree"; then
        echo "  FAIL: ${dependent} depends on ${forbidden}, which inverts the layering."
        echo "        Allowed direction: app -> {store, slack} -> core"
        failed=1
    fi
done

if [[ "$failed" -ne 0 ]]; then
    echo
    echo "Dependency direction check FAILED."
    exit 1
fi

echo "==> Dependency direction OK"
