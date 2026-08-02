#!/usr/bin/env python3
"""Assert what `helm template charts/alertthread` renders.

ROADMAP known open item 18: the Kubernetes hardening existed only as a fragment in
`docs/src/how-to/harden-a-deployment.md`, and a fragment nothing checks drifts from the code
that has to honour it. This file is the check. It renders the chart under several value sets
and asserts on the objects, not on the text — an assertion that greps for a string passes for
a chart that renders an empty document, which is not a hypothetical: the first draft of
`prometheusrule.yaml` rendered `spec:` with nothing under it and `helm lint` was happy.

Four groups of assertion, each pinning a decision somebody could undo without noticing:

1.  **Container hardening.** Every field the how-to specifies, on the pod and on the
    container, plus the two writable mounts a read-only root filesystem forces the relay to
    declare. Deleting any one of them fails here.
2.  **The alert rules.** That the chart's copy of `deploy/alertthread.rules.yaml` is still
    byte-identical to it, that the circular-dependency warning survived packaging, that the
    thresholds `values.yaml` claims to expose still match the expressions they substitute
    into, and that the `job` label the rules select on is the one the ServiceMonitor will
    produce. A rule whose metric or job label matches nothing evaluates empty for ever and
    looks exactly like a healthy relay.
3.  **The probes.** That `/readyz` is not repointed at anything that checks Slack auth
    (ADR 003 §2.2), and that the startup budget outlasts `slack.auth_startup_grace` so a
    transient Slack outage is not served as CrashLoopBackOff.
4.  **Secrets.** That no token reaches a rendered manifest anywhere a `secretKeyRef` or a
    mounted file would do.

Usage:
    chart-test.py [--chart charts/alertthread]
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - environment problem, not a chart problem
    sys.exit(
        "chart-test.py needs PyYAML: the assertions are on parsed objects, not on the\n"
        "rendered text, because a text match passes for a document that renders empty.\n"
        "    python3 -m pip install pyyaml"
    )

REPO = Path(__file__).resolve().parent.parent

# The two answers every install has to give. Everything else has a default.
BASE = [
    "--set",
    "config.slack.default_channel=#alerts",
    "--set",
    "slack.existingSecret=bot-token",
]

# Thresholds values.yaml exposes, and the exact substring in the rules file each one
# substitutes into. The template does a literal `replace`, so an expression edited into a
# different shape would silently keep shipping the default while values.yaml claimed
# otherwise. Each anchor must occur exactly once.
THRESHOLD_ANCHORS = {
    "outboxOldestAgeSeconds": "(alertthread_outbox_oldest_age_seconds) > 300",
    "outboxDepth": "(alertthread_outbox_depth) > 500",
    "slackCallErrorRatio": "rate(alertthread_slack_calls_total[15m])) > 0.1",
    "slackRateLimitedPerSecond": (
        'rate(alertthread_rate_limited_total{source="slack"}[30m])) > 0.05'
    ),
}

failures: list[str] = []
checks = 0


def check(condition: object, message: str) -> bool:
    """Record an assertion. Returns whether it held, so a caller can skip dependent ones."""
    global checks
    checks += 1
    if not condition:
        failures.append(message)
        return False
    return True


def render(chart: Path, *args: str) -> list[dict]:
    """`helm template` with the two required values, parsed."""
    result = subprocess.run(
        ["helm", "template", "alertthread", str(chart), *BASE, *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"helm template {' '.join(args)} failed:\n{result.stderr}")
    return [doc for doc in yaml.safe_load_all(result.stdout) if doc]


def render_error(chart: Path, *args: str) -> str:
    """The stderr of a render that is expected to be refused. Empty means it was not."""
    result = subprocess.run(
        ["helm", "template", "alertthread", str(chart), *BASE, *args],
        capture_output=True,
        text=True,
        check=False,
    )
    return "" if result.returncode == 0 else result.stderr


def one(docs: list[dict], kind: str) -> dict | None:
    found = [doc for doc in docs if doc.get("kind") == kind]
    return found[0] if len(found) == 1 else None


# ---------------------------------------------------------------------------
# 1. Container hardening — ROADMAP known open item 18
# ---------------------------------------------------------------------------


def test_hardening(chart: Path) -> None:
    docs = render(chart)
    deployment = one(docs, "Deployment")
    if not check(deployment, "no single Deployment rendered"):
        return

    pod = deployment["spec"]["template"]["spec"]
    container = pod["containers"][0]

    # The pod half. runAsUser matches the uid baked into the scratch image.
    pod_ctx = pod.get("securityContext", {})
    check(pod_ctx.get("runAsNonRoot") is True, "pod securityContext.runAsNonRoot is not true")
    check(pod_ctx.get("runAsUser") == 65532, "pod securityContext.runAsUser is not 65532")
    check(pod_ctx.get("runAsGroup") == 65532, "pod securityContext.runAsGroup is not 65532")
    check(
        pod_ctx.get("seccompProfile", {}).get("type") == "RuntimeDefault",
        "pod seccompProfile is not RuntimeDefault — Kubernetes defaults to Unconfined and "
        "will not supply this on its own",
    )
    check(
        pod_ctx.get("fsGroup") == 65532,
        "pod securityContext.fsGroup is not 65532 — the PVC is then unwritable by the relay "
        "and it fails at startup with `unable to open database file`",
    )

    # The container half.
    ctr_ctx = container.get("securityContext", {})
    check(
        ctr_ctx.get("readOnlyRootFilesystem") is True,
        "container securityContext.readOnlyRootFilesystem is not true",
    )
    check(
        ctr_ctx.get("allowPrivilegeEscalation") is False,
        "container securityContext.allowPrivilegeEscalation is not false",
    )
    check(ctr_ctx.get("privileged") is False, "container securityContext.privileged is not false")
    check(
        ctr_ctx.get("capabilities", {}).get("drop") == ["ALL"],
        "container capabilities.drop is not exactly [ALL] — the relay binds 8080, never "
        "changes uid and touches no device, so it needs none",
    )

    check(
        pod.get("automountServiceAccountToken") is False,
        "automountServiceAccountToken is not false — the relay touches no Kubernetes API",
    )

    # A read-only root filesystem means the two writable paths are declared, never bought by
    # relaxing the flag. compose.yaml worked this out for the compose case and `just e2e`
    # proves it there.
    mounts = {m["mountPath"]: m for m in container.get("volumeMounts", [])}
    volumes = {v["name"]: v for v in pod.get("volumes", [])}

    check(
        "/var/lib/alertthread" in mounts,
        "the SQLite state mount is missing: the database, its -wal and its -shm all have to "
        "be on one declared writable filesystem",
    )
    if check("/tmp" in mounts, "/tmp is not mounted — SQLite spills a temp file there"):
        tmp = volumes.get(mounts["/tmp"]["name"], {})
        check(
            tmp.get("emptyDir", {}).get("medium") == "Memory",
            "/tmp is not an in-memory emptyDir",
        )

    # Nothing may mount inside another volume's mount point: the parent is read-only, and the
    # image is scratch, so there is no writable directory for the kubelet to create one in.
    paths = sorted(mounts)
    for outer in paths:
        for inner in paths:
            if inner != outer and inner.startswith(outer.rstrip("/") + "/"):
                check(False, f"volumeMount {inner} nests inside volumeMount {outer}")

    # The state mount is a PVC on SQLite, and the whole PVC disappears on PostgreSQL.
    check(one(docs, "PersistentVolumeClaim"), "SQLite rendered no PersistentVolumeClaim")
    check(
        deployment["spec"]["strategy"]["type"] == "Recreate",
        "SQLite must deploy Recreate: two processes on one SQLite file is not supported",
    )

    pg = render(
        chart,
        "--set",
        "config.storage.backend=postgres",
        "--set",
        "postgres.existingSecret=pg-app",
        "--set",
        "replicaCount=3",
    )
    pg_deployment = one(pg, "Deployment")
    check(not one(pg, "PersistentVolumeClaim"), "PostgreSQL still rendered a PVC")
    check(
        pg_deployment["spec"]["strategy"]["type"] == "RollingUpdate",
        "PostgreSQL must deploy RollingUpdate",
    )
    pg_mounts = {
        m["mountPath"] for m in pg_deployment["spec"]["template"]["spec"]["containers"][0]["volumeMounts"]
    }
    check("/var/lib/alertthread" not in pg_mounts, "PostgreSQL still mounts the SQLite path")
    check("/tmp" in pg_mounts, "PostgreSQL lost the /tmp mount")

    # ADR 001 D4. The relay does not detect this itself, so the chart is the only thing
    # standing between a scaled Deployment and two writers on one database file.
    check(
        "postgres" in render_error(chart, "--set", "replicaCount=2"),
        "replicaCount=2 on SQLite rendered instead of being refused",
    )
    check(
        render_error(chart, "--set", "config.storage.url=sqlite:///elsewhere/state.sqlite"),
        "a SQLite URL outside the writable mount rendered instead of being refused",
    )
    check(
        render_error(chart, "--set", "config.slack.default_channel="),
        "an empty default channel rendered instead of being refused",
    )


# ---------------------------------------------------------------------------
# 2. The alert rules
# ---------------------------------------------------------------------------


def test_rules(chart: Path) -> None:
    source = (REPO / "deploy" / "alertthread.rules.yaml").read_text(encoding="utf-8")
    packaged_path = chart / "files" / "alertthread.rules.yaml"
    packaged = packaged_path.read_text(encoding="utf-8") if packaged_path.exists() else ""

    check(
        packaged == source,
        f"{packaged_path} has drifted from deploy/alertthread.rules.yaml.\n"
        "  Helm cannot read a file outside its own chart directory, so the chart carries a\n"
        "  copy. deploy/ is the original. Run `just chart-sync`.",
    )

    # Every threshold values.yaml claims to expose has to substitute into something, exactly
    # once. An expression rewritten into a different shape silently keeps shipping the
    # default while values.yaml says the operator is in control of it.
    values = yaml.safe_load((chart / "values.yaml").read_text(encoding="utf-8"))
    declared = values["metrics"]["prometheusRule"]["thresholds"]
    check(
        set(declared) == set(THRESHOLD_ANCHORS),
        f"values.yaml exposes {sorted(declared)} but this test knows "
        f"{sorted(THRESHOLD_ANCHORS)}",
    )
    for name, anchor in THRESHOLD_ANCHORS.items():
        check(
            source.count(anchor) == 1,
            f"the anchor for metrics.prometheusRule.thresholds.{name} occurs "
            f"{source.count(anchor)} times in deploy/alertthread.rules.yaml, not once: "
            f"{anchor!r}",
        )

    docs = render(chart)
    rule = one(docs, "PrometheusRule")
    if not check(rule, "no single PrometheusRule rendered"):
        return

    groups = rule["spec"].get("groups") or []
    check(groups, "the PrometheusRule rendered an empty spec")
    alerts = [r for group in groups for r in group.get("rules", [])]
    check(len(alerts) >= 10, f"only {len(alerts)} alerts survived packaging")

    # ADR 001 D11 and AGENTS.md: shipping the rules without the route that bypasses the relay
    # is worse than shipping no rules. The YAML comment carrying that warning does not survive
    # into the cluster object, so an annotation repeats it.
    warning = " ".join(rule["metadata"].get("annotations", {}).values())
    check(
        "CANNOT ALERT ON ITSELF" in warning,
        "the rendered PrometheusRule lost the circular-dependency warning",
    )
    check(
        "slack_configs" in warning and 'alertname=~"Alertthread.*"' in warning,
        "the warning does not say how to route these rules away from the relay",
    )

    # The alertname matcher the warning tells operators to route on has to catch every rule.
    for alert in alerts:
        check(
            alert["alert"].startswith("Alertthread"),
            f'{alert["alert"]} would not be routed by alertname=~"Alertthread.*"',
        )
        check(
            not re.search(r"(?<![_a-zA-Z])(sum|min|max)\(", alert["expr"]),
            f'a bare aggregation in {alert["alert"]} drops the job label and unroutes it',
        )

    # Overriding a threshold has to reach the expression.
    tuned = render(
        chart,
        "--set",
        "metrics.prometheusRule.thresholds.outboxOldestAgeSeconds=1800",
        "--set",
        "metrics.prometheusRule.thresholds.outboxDepth=2000",
    )
    tuned_exprs = " ".join(
        r["expr"]
        for group in one(tuned, "PrometheusRule")["spec"]["groups"]
        for r in group["rules"]
    )
    check(
        "(alertthread_outbox_oldest_age_seconds) > 1800" in tuned_exprs,
        "overriding thresholds.outboxOldestAgeSeconds did not change the expression",
    )
    check(
        "(alertthread_outbox_depth) > 2000" in tuned_exprs,
        "overriding thresholds.outboxDepth did not change the expression",
    )

    # The `job` label three objects have to agree on. Prometheus Operator takes it off the
    # Service using the ServiceMonitor's jobLabel; the rules select on it by name. Disagree
    # and AlertthreadDown evaluates empty for ever, which looks exactly like a healthy relay.
    monitor = one(docs, "ServiceMonitor")
    service = one(docs, "Service")
    if check(monitor and service, "no single ServiceMonitor and Service rendered"):
        job_label = monitor["spec"].get("jobLabel")
        if check(job_label, "the ServiceMonitor sets no jobLabel, so `job` is the Service name"):
            scraped = service["metadata"]["labels"].get(job_label)
            check(
                scraped,
                f"the Service carries no {job_label} label for the ServiceMonitor to read",
            )
            selected = set(re.findall(r'up\{job="([^"]+)"\}', tuned_exprs))
            check(
                selected == {scraped},
                f"the rules select on job={selected} but the ServiceMonitor will label the "
                f"scrape job={scraped!r}",
            )

    # And under nameOverride they still agree, because both move together.
    renamed = render(chart, "--set", "nameOverride=relay")
    renamed_exprs = " ".join(
        r["expr"]
        for group in one(renamed, "PrometheusRule")["spec"]["groups"]
        for r in group["rules"]
    )
    renamed_label = one(renamed, "ServiceMonitor")["spec"]["jobLabel"]
    check(
        set(re.findall(r'up\{job="([^"]+)"\}', renamed_exprs))
        == {one(renamed, "Service")["metadata"]["labels"][renamed_label]},
        "nameOverride moved the scrape job label without moving the rules",
    )


# ---------------------------------------------------------------------------
# 3. Probes
# ---------------------------------------------------------------------------


def test_probes(chart: Path) -> None:
    docs = render(chart)
    deployment = one(docs, "Deployment")
    if not deployment:
        return
    container = deployment["spec"]["template"]["spec"]["containers"][0]

    liveness = container.get("livenessProbe", {}).get("httpGet", {})
    readiness = container.get("readinessProbe", {}).get("httpGet", {})
    startup = container.get("startupProbe", {})

    # /healthz deliberately does not check the store: a database blip must not restart a pod
    # that is correctly buffering alerts.
    check(liveness.get("path") == "/healthz", f"liveness probe hits {liveness.get('path')}")
    # /readyz checks the store and deliberately does NOT check Slack auth (ADR 003 §2.2).
    # Repointing it at /healthz would make a pod that cannot persist join the Service.
    check(readiness.get("path") == "/readyz", f"readiness probe hits {readiness.get('path')}")
    check(
        "/webhook" not in {liveness.get("path"), readiness.get("path")},
        "a probe points at /webhook, which the bearer token can close — a 401 there is a pod "
        "Kubernetes restarts for ever",
    )

    # A transient startup auth.test failure retries for slack.auth_startup_grace and the relay
    # serves nothing at all meanwhile. A startup budget shorter than the grace turns a Slack
    # outage into CrashLoopBackOff, which looks exactly like a bad token and is not one.
    values = yaml.safe_load((chart / "values.yaml").read_text(encoding="utf-8"))
    grace = values["config"]["slack"]["auth_startup_grace"]
    seconds = {"ms": 0.001, "s": 1, "m": 60, "h": 3600, "d": 86400}
    match = re.fullmatch(r"(\d+)(ms|s|m|h|d)?", str(grace))
    grace_seconds = int(match.group(1)) * seconds.get(match.group(2) or "s", 1) if match else 0
    budget = startup.get("periodSeconds", 0) * startup.get("failureThreshold", 0)
    check(
        budget >= grace_seconds * 2,
        f"the startup probe budget is {budget}s against a slack.auth_startup_grace of "
        f"{grace_seconds}s: a transient Slack outage would be served as CrashLoopBackOff",
    )

    grace_value = values["config"]["server"]["shutdown_grace"]
    match = re.fullmatch(r"(\d+)(ms|s|m|h|d)?", str(grace_value))
    shutdown = int(match.group(1)) * seconds.get(match.group(2) or "s", 1) if match else 0
    check(
        deployment["spec"]["template"]["spec"]["terminationGracePeriodSeconds"] > shutdown,
        "terminationGracePeriodSeconds does not outlast server.shutdown_grace, so Kubernetes "
        "SIGKILLs the relay mid-delivery instead of letting it drain",
    )


# ---------------------------------------------------------------------------
# 4. Secrets
# ---------------------------------------------------------------------------


def test_secrets(chart: Path) -> None:
    # With existingSecret, nothing about either token passes through Helm at all.
    docs = render(chart, "--set", "webhookAuth.enabled=true", "--set", "webhookAuth.existingSecret=hook")
    check(not any(d.get("kind") == "Secret" for d in docs), "a Secret was created for existing ones")

    deployment = one(docs, "Deployment")
    pod = deployment["spec"]["template"]["spec"]
    volumes = {v["name"]: v for v in pod["volumes"]}
    secret_volumes = [v for v in volumes.values() if "secret" in v]
    check(len(secret_volumes) == 2, f"{len(secret_volumes)} secret volumes, expected 2")
    check(
        {v["secret"]["secretName"] for v in secret_volumes} == {"bot-token", "hook"},
        "the Deployment does not mount the named existing Secrets",
    )
    for volume in secret_volumes:
        # With fsGroup set the kubelet gives a secret volume root:fsGroup ownership, so
        # owner-only would be unreadable by uid 65532.
        check(
            volume["secret"].get("defaultMode") == 0o440,
            f'{volume["name"]} defaultMode is {volume["secret"].get("defaultMode")}, not 0440',
        )

    # And the config file names the mounted files rather than carrying a value.
    config = yaml.safe_load(one(docs, "ConfigMap")["data"]["config.yaml"])
    check("token" not in config["slack"], "the ConfigMap carries slack.token inline")
    check(config["slack"].get("token_file"), "the ConfigMap does not set slack.token_file")
    check(
        config["server"].get("auth_token_file"),
        "webhookAuth.enabled did not set server.auth_token_file",
    )
    check("auth_token" not in config["server"], "the ConfigMap carries server.auth_token inline")

    # An inline token is a convenience the chart allows; it must still only ever land in a
    # Secret object, never in the Deployment, the ConfigMap or an environment variable.
    inline = subprocess.run(
        [
            "helm",
            "template",
            "alertthread",
            str(chart),
            "--set",
            "config.slack.default_channel=#alerts",
            "--set",
            "slack.token=xoxb-not-a-real-token",
            "--set",
            "webhookAuth.enabled=true",
            "--set",
            "webhookAuth.token=hunter2",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    check(
        "xoxb-not-a-real-token" not in inline and "hunter2" not in inline,
        "an inline token appears as plaintext in a rendered manifest",
    )
    inline_docs = [d for d in yaml.safe_load_all(inline) if d]
    secrets = [d for d in inline_docs if d.get("kind") == "Secret"]
    check(len(secrets) == 2, f"{len(secrets)} Secrets created for two inline tokens")
    for secret in secrets:
        check("stringData" not in secret, f'{secret["metadata"]["name"]} uses stringData')

    # PostgreSQL connection strings carry a password, so the URL must not reach the ConfigMap.
    pg = render(chart, "--set", "config.storage.backend=postgres", "--set", "postgres.existingSecret=pg-app")
    pg_config = yaml.safe_load(one(pg, "ConfigMap")["data"]["config.yaml"])
    check("url" not in pg_config["storage"], "the ConfigMap carries storage.url alongside a Secret")
    env = one(pg, "Deployment")["spec"]["template"]["spec"]["containers"][0]["env"]
    url = [e for e in env if e["name"] == "ALERTTHREAD_STORAGE__URL"]
    check(len(url) == 1 and "valueFrom" in url[0], "storage.url does not come from a secretKeyRef")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chart", default=str(REPO / "charts" / "alertthread"))
    args = parser.parse_args()
    chart = Path(args.chart).resolve()

    if subprocess.run(["helm", "version"], capture_output=True).returncode != 0:
        sys.exit("chart-test.py needs helm on the PATH — https://helm.sh/docs/intro/install/")

    for name, test in (
        ("hardening", test_hardening),
        ("rules", test_rules),
        ("probes", test_probes),
        ("secrets", test_secrets),
    ):
        try:
            test(chart)
        except Exception as error:  # noqa: BLE001 - a raising test is a failing test
            failures.append(f"{name}: {error}")

    if failures:
        print(f"\nchart-test: {len(failures)} of {checks} assertions FAILED\n")
        for failure in failures:
            print(f"  ✗ {failure}")
        print()
        return 1

    print(f"==> chart-test OK ({checks} assertions over the rendered chart)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
