#!/usr/bin/env python3
"""Enforce per-crate line-coverage thresholds from cargo-llvm-cov JSON output.

The thresholds are per-crate on purpose. A single workspace percentage is the
standard way these gates fail: it lets genuinely critical logic sit undertested
so long as easy-to-cover code drags the average up, and it pushes people to
write line-touching tests for `main.rs` to buy back headroom. Tiering by how
testable and how critical each crate actually is puts the strictest bar exactly
where the risk lives. See ROADMAP.md "Coverage policy".

Usage:
    coverage-gate.py <llvm-cov-export.json> [--profile <name>]

Profiles exist because `alertthread-store` ships two backends behind cargo
features and no single build can run the tests for both. `just test` has no
containers, so it compiles SQLite only; `just test-pg` has PostgreSQL, so it
compiles PostgreSQL only. Each build is gated against the code it actually
compiled, at the same 95% threshold. See "Two gated builds, one crate" below.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# (label, path prefix relative to the repo root, minimum line coverage %)
#
# Keep this table in sync with the one in ROADMAP.md and AGENTS.md. Lowering a
# threshold or adding an exclusion requires saying so in the PR description and
# justifying it — AGENTS.md calls silently weakening this gate "the one move
# that is never acceptable here".
WORKSPACE: list[tuple[str, str, float]] = [
    # Pure, no I/O, no clock, no runtime. Every branch is reachable with a plain
    # function call, so anything below 100% is dead code or an untested branch —
    # and this crate holds every correctness decision in the project.
    ("alertthread-core", "crates/core/src", 100.0),
    # Conformance suite covers the trait exhaustively; the residue is
    # driver-level error paths that need fault injection to reach.
    #
    # This build compiles the SQLite backend only — see below.
    ("alertthread-store", "crates/store/src", 95.0),
    # wiremock covers the API surface; the residue is reqwest transport failures.
    ("alertthread-slack", "crates/slack/src", 95.0),
    # Handlers, workers, config and the rate limiter are all directly testable.
    ("alertthread (app)", "crates/app/src", 95.0),
]

# ---------------------------------------------------------------------------
# Two gated builds, one crate
# ---------------------------------------------------------------------------
# `alertthread-store` has two backends behind cargo features, and the tests for
# one of them need a PostgreSQL server. `just test` runs with no containers, so
# if PostgresStore were compiled into that build its several hundred lines would
# be counted as uncovered and drag the crate under 95% — and the only ways to
# make that pass would be to lower the threshold or to blanket-exclude the file,
# both of which AGENTS.md rules out, and a store backend is the last place to do
# either.
#
# So neither build is asked about code it cannot run:
#
#   just test     --workspace, default features       -> SQLite backend, gated
#   just test-pg  -p store --no-default-features -F postgres -> PG backend, gated
#
# `crates/store/src` appears in both, at the same threshold, because the shared
# code (the trait, the payload format, the row mapping) is exercised by both and
# each backend is exercised by exactly one. Nothing is unmeasured, and nothing is
# measured against tests that could not have run.
STORE_POSTGRES: list[tuple[str, str, float]] = [
    ("alertthread-store (pg)", "crates/store/src", 95.0),
]

PROFILES: dict[str, list[tuple[str, str, float]]] = {
    "workspace": WORKSPACE,
    "store-postgres": STORE_POSTGRES,
}

# Excluded outright, rather than absorbed by a softer threshold. An explicit,
# justified exclusion is honest; a soft threshold hides how well the code that
# *does* matter is covered.
EXCLUSIONS: list[tuple[str, str]] = [
    ("crates/app/src/main.rs", "~10 lines of wiring and signal handling"),
    ("dev/slack-mock", "development tooling, not shipped"),
]


def is_excluded(rel_path: str) -> bool:
    return any(rel_path.startswith(prefix) for prefix, _ in EXCLUSIONS)


def main() -> int:
    args = sys.argv[1:]
    profile_name = "workspace"

    if "--profile" in args:
        index = args.index("--profile")
        if index + 1 >= len(args):
            print("coverage gate: --profile needs a name", file=sys.stderr)
            return 2
        profile_name = args[index + 1]
        del args[index : index + 2]

    if len(args) != 1:
        print(__doc__, file=sys.stderr)
        return 2

    thresholds = PROFILES.get(profile_name)
    if thresholds is None:
        known = ", ".join(sorted(PROFILES))
        print(
            f"coverage gate: unknown profile {profile_name!r}; known: {known}",
            file=sys.stderr,
        )
        return 2

    export_path = Path(args[0])
    if not export_path.is_file():
        print(f"coverage gate: no coverage data at {export_path}", file=sys.stderr)
        return 2

    with export_path.open(encoding="utf-8") as handle:
        report = json.load(handle)

    repo_root = Path(__file__).resolve().parent.parent

    # llvm-cov export format: {"data": [{"files": [{"filename", "summary"}]}]}
    files: list[dict] = []
    for datum in report.get("data", []):
        files.extend(datum.get("files", []))

    if not files:
        print("coverage gate: report contained no files at all", file=sys.stderr)
        return 2

    # Aggregate raw line counts per crate. Summing counts is correct;
    # averaging the per-file percentages is not, because it weights a 3-line
    # file the same as a 300-line one.
    totals: dict[str, list[int]] = {label: [0, 0] for label, _, _ in thresholds}
    excluded_files: list[str] = []

    for entry in files:
        filename = entry.get("filename", "")
        try:
            rel = str(Path(filename).resolve().relative_to(repo_root))
        except ValueError:
            # Outside the repo: registry sources, std. Not ours to cover.
            continue

        if is_excluded(rel):
            excluded_files.append(rel)
            continue

        lines = entry.get("summary", {}).get("lines", {})
        count = int(lines.get("count", 0))
        covered = int(lines.get("covered", 0))

        for label, prefix, _ in thresholds:
            if rel.startswith(prefix):
                totals[label][0] += covered
                totals[label][1] += count
                break

    print()
    print(f"Per-crate line coverage ({profile_name})")
    print("=" * 62)
    print(f"{'crate':<22} {'covered':>9} {'lines':>8} {'actual':>8} {'min':>7}  ")
    print("-" * 62)

    failures: list[str] = []

    for label, _, threshold in thresholds:
        covered, count = totals[label]
        if count == 0:
            # No instrumented lines at all. This is a real state during
            # scaffolding, but it must not silently read as success later, so
            # it is reported distinctly rather than as 100%.
            print(f"{label:<22} {'-':>9} {'0':>8} {'n/a':>8} {threshold:>6.1f}%  (no code)")
            continue

        actual = 100.0 * covered / count
        # Round to the displayed precision before comparing, so a crate showing
        # "100.0%" can never fail against a 100% threshold on a float artefact.
        ok = round(actual, 1) >= threshold
        status = "PASS" if ok else "FAIL"
        print(
            f"{label:<22} {covered:>9} {count:>8} {actual:>7.1f}% {threshold:>6.1f}%  {status}"
        )
        if not ok:
            failures.append(f"{label}: {actual:.1f}% < {threshold:.1f}% required")

    print("=" * 62)

    if excluded_files:
        print("\nExcluded from the gate (by policy, see ROADMAP.md):")
        for prefix, reason in EXCLUSIONS:
            touched = [f for f in excluded_files if f.startswith(prefix)]
            if touched:
                print(f"  {prefix} — {reason}")

    if failures:
        print("\nCOVERAGE GATE FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print(
            "\nDo not chase the threshold with tests that assert nothing. If a branch is\n"
            "genuinely unreachable, delete it. If it is genuinely untestable, exclude it\n"
            "explicitly with a comment saying why, and say so in the PR. See AGENTS.md.",
            file=sys.stderr,
        )
        return 1

    print("\nCoverage gate passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
