#!/usr/bin/env bash
#
# Verify that every relative Markdown link in the repository points at a file
# that actually exists.
#
# This exists because moving the PRD and ADR into docs/src/adr/ in Phase 0 broke
# every cross-link to them, and nothing would have caught that. mdBook's own
# link checking only covers files inside docs/src; AGENTS.md, ROADMAP.md and
# README.md all link into the book from outside it, which is exactly where the
# breakage was.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

failed=0
checked=0

while IFS= read -r -d '' file; do
    dir="$(dirname "$file")"

    # Pull the target out of every [text](target) inline link. Reference-style
    # links and bare autolinks are not used in this repo.
    while IFS= read -r target; do
        [[ -z "$target" ]] && continue

        # Skip external links, mailto, protocol-relative, and pure anchors.
        case "$target" in
            http://*|https://*|mailto:*|//*|\#*) continue ;;
        esac

        # Strip any fragment; we check that the file exists, not the heading.
        path="${target%%#*}"
        [[ -z "$path" ]] && continue

        checked=$((checked + 1))

        if [[ "$path" = /* ]]; then
            resolved="${repo_root}${path}"
        else
            resolved="${dir}/${path}"
        fi

        if [[ ! -e "$resolved" ]]; then
            echo "  BROKEN: $file -> $target"
            failed=$((failed + 1))
        fi
    done < <(grep -oE '\]\([^)]+\)' "$file" 2>/dev/null | sed -E 's/^\]\(//; s/\)$//' | sed -E 's/[[:space:]]+"[^"]*"$//')
done < <(find . -name '*.md' -not -path './target/*' -not -path './docs/book/*' -not -path './.git/*' -print0)

if [[ "$failed" -ne 0 ]]; then
    echo
    echo "Link check FAILED: $failed broken link(s) out of $checked checked."
    exit 1
fi

echo "==> Link check OK ($checked relative links resolve)"
