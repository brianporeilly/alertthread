#!/usr/bin/env bash
#
# The Phase 4 exit criterion, as an assertion instead of a paragraph.
#
# Brings up the real stack — relay + Prometheus + Alertmanager + the fake Slack —
# lets a genuine Prometheus rule fire, and checks against slack-mock's /api/state
# that the relay did the two things the whole project exists to do:
#
#   1. FIRING:   a storm of alerts collapses into one group-summary parent with
#                the individual alerts threaded *under* it, not posted top-level
#                (ADR 001 D5).
#   2. RESOLVED: when the alerts resolve, each child message is edited in place
#                *and* a thread reply is posted (ADR 001 D6 — chat.update does
#                not notify, which is why it must be both).
#
# The firing and resolution both come from the real pipeline: the demo rule
# fires the instant Prometheus starts and resolves itself 60s later. No captured
# JSON, no human, no file editing.
#
# Every wait is bounded, and a timeout dumps the mock's state and the container
# logs rather than failing with a bare "timed out" — a flaky end-to-end job that
# says nothing when it breaks gets disabled within a month, and then the exit
# criterion is fiction.
#
# COMPOSE is the compose command ("podman compose" or "docker compose"), passed
# in by the `just e2e` recipe so this script never hardcodes an engine.

set -euo pipefail

COMPOSE="${COMPOSE:-docker compose}"
MOCK="${MOCK_URL:-http://localhost:8081}"
RELAY="${RELAY_URL:-http://localhost:8080}"

# The bearer token the demo stack configures. Must match
# ALERTTHREAD_SERVER__AUTH_TOKEN in compose.yaml and the credentials in
# dev/alertmanager/alertmanager.yml. Not a secret; it guards a container that
# lives for ninety seconds.
WEBHOOK_TOKEN="${WEBHOOK_TOKEN:-demo-webhook-token-not-a-secret}"

# How long each phase may take before we give up. The resolve fires at Prometheus
# uptime 60s, so its budget is the largest.
READY_TIMEOUT="${READY_TIMEOUT:-90}"
FIRING_TIMEOUT="${FIRING_TIMEOUT:-55}"
RESOLVE_TIMEOUT="${RESOLVE_TIMEOUT:-120}"

# The demo defines five alerts and the relay's collapse threshold is 3, so all
# five thread under one summary.
EXPECT_ALERTS=5

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

dump() {
    echo "----- slack-mock /api/state -----" >&2
    curl -fsS "${MOCK}/api/state" 2>/dev/null | python3 -m json.tool >&2 || echo "(state unavailable)" >&2
    for service in relay prometheus alertmanager slack-mock; do
        echo "----- ${service} logs (tail) -----" >&2
        # shellcheck disable=SC2086
        ${COMPOSE} logs --tail 40 "${service}" 2>&1 | sed 's/^/  /' >&2 || true
    done
}

teardown() {
    say "Tearing down"
    # shellcheck disable=SC2086
    ${COMPOSE} --profile demo down --volumes >/dev/null 2>&1 || true
}
trap teardown EXIT

# Polls the mock's state, feeding it to a python predicate until the predicate
# prints "OK" or the deadline passes. On timeout it dumps everything and fails.
#
#   wait_for <label> <timeout-seconds> <python-predicate>
#
# The predicate reads the state as `s` (a dict) and the expected alert count as
# `n`; it prints "OK" to pass or anything else (a reason) to keep waiting.
wait_for() {
    local label="$1" timeout="$2" predicate="$3"
    local deadline=$(( SECONDS + timeout ))
    local reason="no state yet"
    while (( SECONDS < deadline )); do
        local state
        if state="$(curl -fsS "${MOCK}/api/state" 2>/dev/null)"; then
            reason="$(printf '%s' "$state" | EXPECT="$EXPECT_ALERTS" python3 -c '
import json, os, sys
s = json.load(sys.stdin)
n = int(os.environ["EXPECT"])
'"$predicate"'
' 2>/dev/null || echo "predicate error")"
            if [[ "$reason" == "OK" ]]; then
                echo "    $label: OK"
                return 0
            fi
        fi
        sleep 2
    done
    echo "TIMED OUT waiting for: ${label}" >&2
    echo "  last reason: ${reason}" >&2
    dump
    exit 1
}

say "Clearing any previous stack so the demo starts from a clean slate"
# shellcheck disable=SC2086
${COMPOSE} --profile demo down --volumes >/dev/null 2>&1 || true

say "Bringing up the demo stack (this builds the relay image the first time)"
# --force-recreate: a rebuilt image is useless if compose reuses the old
# container, which podman-compose will do for a container that is still running.
# shellcheck disable=SC2086
${COMPOSE} --profile demo up -d --build --force-recreate

say "Waiting for the relay to be healthy"
deadline=$(( SECONDS + READY_TIMEOUT ))
until curl -fsS "${RELAY}/healthz" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
        echo "the relay never became healthy" >&2
        dump
        exit 1
    fi
    sleep 2
done
echo "    relay: healthy"

# The perimeter, before anything else touches the relay. This is the only place
# the bearer token is exercised against the shipped container rather than against
# the router in a test binary — and the only place the *routing* of it is proven:
# the three endpoints Kubernetes and Prometheus use must stay open, or the pod
# never becomes ready and its own metrics stop being scrapable.
say "Checking the webhook perimeter (ADR 001 D11)"
status_of() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

# An empty-alerts body: accepted, writes nothing, posts nothing, so it cannot
# disturb the assertions below. What is under test is the credential, not ingest.
EMPTY_BODY='{"version":"4","groupKey":"probe","truncatedAlerts":0,"status":"firing","receiver":"alertthread","groupLabels":{},"commonLabels":{},"commonAnnotations":{},"externalURL":"http://alertmanager","alerts":[]}'

code="$(status_of -X POST -H 'content-type: application/json' -d "$EMPTY_BODY" "${RELAY}/webhook")"
if [[ "$code" != "401" ]]; then
    echo "an unauthenticated POST /webhook returned ${code}, expected 401" >&2
    dump
    exit 1
fi
echo "    unauthenticated POST /webhook: 401"

code="$(status_of -X POST -H 'content-type: application/json' \
    -H "authorization: Bearer wrong-${WEBHOOK_TOKEN}" -d "$EMPTY_BODY" "${RELAY}/webhook")"
if [[ "$code" != "401" ]]; then
    echo "POST /webhook with the wrong credential returned ${code}, expected 401" >&2
    dump
    exit 1
fi
echo "    wrong credential: 401"

code="$(status_of -X POST -H 'content-type: application/json' \
    -H "authorization: Bearer ${WEBHOOK_TOKEN}" -d "$EMPTY_BODY" "${RELAY}/webhook")"
if [[ "$code" != "200" ]]; then
    echo "POST /webhook with the configured credential returned ${code}, expected 200" >&2
    dump
    exit 1
fi
echo "    configured credential: 200"

for open_path in healthz readyz metrics; do
    code="$(status_of "${RELAY}/${open_path}")"
    if [[ "$code" != "200" ]]; then
        echo "GET /${open_path} returned ${code} with no credential, expected 200 — probes and" >&2
        echo "scrapes do not carry one, and a 401 here is a pod that never becomes ready" >&2
        dump
        exit 1
    fi
done
echo "    /healthz, /readyz, /metrics: 200 without a credential"

# The channel #alerts (canonical id aside) must contain exactly one top-level
# message — the group summary — with the individual alerts threaded under it.
say "Waiting for the storm to collapse into a threaded group (ADR 001 D5)"
wait_for "group summary parent with ${EXPECT_ALERTS} threaded children" "$FIRING_TIMEOUT" '
chans = s["channels"]
if not chans:
    print("no channels yet"); raise SystemExit
tops = chans[0]["messages"]
if len(tops) != 1:
    print(f"expected 1 top-level message, saw {len(tops)}"); raise SystemExit
parent = tops[0]
body = " ".join(parent["blocks"]) + parent["text"]
if "FIRING" not in body:
    print("top-level message is not a firing summary yet"); raise SystemExit
replies = parent["replies"]
if len(replies) < n:
    print(f"{len(replies)}/{n} children threaded so far"); raise SystemExit
# Every child must be threaded under the parent, not posted top-level.
for r in replies:
    if r["thread_ts"] != parent["ts"]:
        print("a child is not threaded under the parent"); raise SystemExit
    if r["color"] != "#d40e0d":
        print("a child is not showing the firing colour"); raise SystemExit
print("OK")
'

# The resolve fires ~60s after Prometheus started. Each child is edited green in
# place, and a fresh thread reply (pointing at the child, so its reply_to is not
# the parent) is posted — the two halves of D6.
say "Waiting for resolution: edit-in-place AND thread reply (ADR 001 D6)"
wait_for "each child edited green, with a resolve reply threaded" "$RESOLVE_TIMEOUT" '
parent = s["channels"][0]["messages"][0]
replies = parent["replies"]
# The original children: threaded directly under the parent.
children = [r for r in replies if r["reply_to"] == parent["ts"]]
# The resolve replies: threaded under a child, flattened into this thread, so
# their reply_to is a child timestamp rather than the parent (D5 meets D6).
resolve_replies = [r for r in replies if r["reply_to"] not in (None, parent["ts"])]
edited_green = [c for c in children if c["edited"] and c["color"] == "#2eb886"]
if len(edited_green) < n:
    print(f"{len(edited_green)}/{n} children edited to resolved-green"); raise SystemExit
if len(resolve_replies) < n:
    print(f"{len(resolve_replies)}/{n} resolve replies posted"); raise SystemExit
# The summary itself should have flipped to resolved too.
pbody = " ".join(parent["blocks"]) + parent["text"]
if "RESOLVED" not in pbody:
    print("group summary has not flipped to resolved"); raise SystemExit
print("OK")
'

say "End-to-end demo passed: fired, threaded, resolved in place — no human in the loop."
