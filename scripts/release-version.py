#!/usr/bin/env python3
"""Print, or check, the one version number this repository releases.

Four places carry it and release-please updates them through two different
mechanisms. Nothing else notices when one is left behind, and the symptom of a
mismatch is a published chart whose `appVersion` names an image tag that was
never pushed — an `ImagePullBackOff` found by an operator rather than by us.

    Cargo.toml   [workspace.package] version    the binary's --version
    Cargo.toml   [workspace.dependencies] x3    path deps carry a version too
    Chart.yaml   version                        the chart's own number
    Chart.yaml   appVersion                     the image tag the chart renders

Usage:
    release-version.py                 print the version, or fail if they disagree
    release-version.py --check 1.2.3   also require it to equal 1.2.3
    release-version.py --check v1.2.3  a leading v is accepted and stripped
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO / "Cargo.toml"
CHART_YAML = REPO / "charts" / "alertthread" / "Chart.yaml"

SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# Deliberately regex rather than a TOML/YAML parse. These are the exact lines
# release-please rewrites, and matching them literally is what makes a moved or
# renamed key a loud failure here instead of the silent `None` a parser would
# return from the wrong path.
WORKSPACE_PACKAGE_VERSION = re.compile(
    r"^\[workspace\.package\]$.*?^version\s*=\s*\"([^\"]+)\"",
    re.MULTILINE | re.DOTALL,
)
WORKSPACE_DEP_VERSION = re.compile(
    r"^(alertthread-(?:core|slack|store))\s*=\s*\{[^}]*version\s*=\s*\"([^\"]+)\"",
    re.MULTILINE,
)
CHART_VERSION = re.compile(r"^version:\s*(\S+?)\s*(?:#.*)?$", re.MULTILINE)
CHART_APP_VERSION = re.compile(r"^appVersion:\s*\"?([^\"#\s]+)\"?\s*(?:#.*)?$", re.MULTILINE)

# Every line above is rewritten by release-please's generic updater, which acts
# only on lines carrying this marker. A line that loses it stops being updated
# and starts disagreeing, so the marker is checked rather than assumed.
MARKER = "x-release-please-version"


def _one(pattern: re.Pattern[str], text: str, what: str) -> str:
    matches = pattern.findall(text)
    if not matches:
        sys.exit(f"release-version.py: found no {what}")
    return matches[0]


def collect() -> dict[str, str]:
    """Every place the version is written, keyed by a name a failure can name."""
    cargo = CARGO_TOML.read_text(encoding="utf-8")
    chart = CHART_YAML.read_text(encoding="utf-8")

    found = {
        "Cargo.toml [workspace.package] version": _one(
            WORKSPACE_PACKAGE_VERSION, cargo, "[workspace.package] version"
        ),
        "Chart.yaml version": _one(CHART_VERSION, chart, "Chart.yaml version"),
        "Chart.yaml appVersion": _one(CHART_APP_VERSION, chart, "Chart.yaml appVersion"),
    }

    deps = WORKSPACE_DEP_VERSION.findall(cargo)
    if len(deps) != 3:
        sys.exit(
            "release-version.py: expected 3 versioned workspace path dependencies in "
            f"Cargo.toml, found {len(deps)}"
        )
    for name, version in deps:
        found[f"Cargo.toml [workspace.dependencies] {name}"] = version

    return found


def unmarked() -> list[str]:
    """Version-bearing lines release-please would no longer rewrite."""
    missing = []
    for path, patterns in ((CARGO_TOML, (WORKSPACE_PACKAGE_VERSION, WORKSPACE_DEP_VERSION)),
                           (CHART_YAML, (CHART_VERSION, CHART_APP_VERSION))):
        text = path.read_text(encoding="utf-8")
        for pattern in patterns:
            for match in pattern.finditer(text):
                # The version group, not the match: [workspace.package] spans
                # several lines and only the one holding the number is rewritten.
                at = match.start(match.re.groups)
                line = text[text.rfind("\n", 0, at) + 1: text.find("\n", at)]
                if MARKER not in line:
                    missing.append(f"{path.relative_to(REPO)}: {line.strip()}")
    return missing


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        metavar="VERSION",
        help="require the version to equal this (a leading 'v' is stripped)",
    )
    args = parser.parse_args()

    found = collect()
    distinct = sorted(set(found.values()))

    if len(distinct) != 1:
        print("release-version.py: the version is not the same everywhere.", file=sys.stderr)
        for where, version in found.items():
            print(f"  {version:<12} {where}", file=sys.stderr)
        print(
            "\nrelease-please updates all of these together. If you edited one by hand,\n"
            "edit the rest; if a release pull request produced this, its config is wrong.",
            file=sys.stderr,
        )
        return 1

    version = distinct[0]
    if not SEMVER.match(version):
        print(f"release-version.py: {version!r} is not a semantic version.", file=sys.stderr)
        return 1

    if missing := unmarked():
        print(
            f"release-version.py: these lines carry the version but not the {MARKER!r}\n"
            "marker, so release-please will leave them behind on the next release:",
            file=sys.stderr,
        )
        for line in missing:
            print(f"  {line}", file=sys.stderr)
        return 1

    if args.check is not None:
        wanted = args.check.removeprefix("v")
        if version != wanted:
            print(
                f"release-version.py: the tree says {version}, the release says {wanted}.\n"
                "A tag whose version is not the one in the tree publishes a chart whose\n"
                "appVersion names an image tag nothing pushed.",
                file=sys.stderr,
            )
            return 1

    print(version)
    return 0


if __name__ == "__main__":
    sys.exit(main())
