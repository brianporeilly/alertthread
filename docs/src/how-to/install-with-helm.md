# Install with Helm

**Goal:** get `alertthread` running in a cluster, hardened, with Alertmanager posting to it
and Prometheus alerting on it.

The chart lives in [`charts/alertthread`](https://github.com/brianporeilly/alertthread/tree/main/charts/alertthread).
It renders the Deployment, Service, ServiceAccount, ConfigMap, PVC, `ServiceMonitor` and
`PrometheusRule`, and it is where the container hardening in
[Harden a deployment](harden-a-deployment.md) is *enforced* rather than described.

> **Status.** The pipeline that publishes the chart and its images is built and has never
> run: nothing is released until `v0.1.0` is tagged. Until then, `helm install` from a git
> checkout — the `oci://` reference below and the default image tag both name things that do
> not exist yet, and the chart's `appVersion` is `0.0.0`.
> [Published artefacts](../reference/published-artifacts.md) is what a release will produce,
> and [Verify artefacts](verify-artifacts.md) is how to check it.

## Before you start

You need:

- A Slack bot token (`xoxb-…`) with `chat:write`, in a Secret.
- A channel to post to.
- The Prometheus Operator CRDs, unless you turn `metrics.serviceMonitor.enabled` and
  `metrics.prometheusRule.enabled` off. The chart does **not** silently skip those objects
  when the CRDs are missing — see [Why the CRDs are not optional](#why-the-crds-are-not-optional).

## 1. Put the bot token in a Secret

```bash
kubectl -n observability create secret generic alertthread-slack \
    --from-literal=token='xoxb-…'
```

`--from-literal`, not `--from-file`: a file written with `echo` keeps its newline. The relay
trims trailing whitespace from a token file, so this one is survivable — but Alertmanager does
not trim `credentials_file`, and getting into the habit here is what saves you in step 4.

## 2. Install

```bash
helm install alertthread oci://ghcr.io/brianporeilly/charts/alertthread \
    --namespace observability --create-namespace \
    --set config.slack.default_channel='#alerts' \
    --set slack.existingSecret=alertthread-slack
```

Those two settings are the only required ones. Everything else has a default, and every
default is in [`values.yaml`](https://github.com/brianporeilly/alertthread/blob/main/charts/alertthread/values.yaml),
which is written to be read.

`slack.existingSecret` rather than `slack.token`: an inline token is stored in the release and
`helm get values` prints it back. The chart accepts one and warns about it at install time.

Read the `NOTES.txt` it prints. It carries the Alertmanager receiver for your release name,
and the warning in step 5.

## 3. Confirm it started

```bash
kubectl -n observability rollout status deploy/alertthread
kubectl -n observability port-forward deploy/alertthread 8080:8080
curl -s localhost:8080/readyz                                     # ready
curl -s localhost:8080/metrics | grep alertthread_slack_auth_valid # 1
```

`alertthread_slack_auth_valid 0` on a pod that *is* running means Slack was unreachable at
startup and the relay started degraded on purpose: the outbox keeps accepting alerts and the
background prober delivers them when Slack comes back. A token Slack *definitively* rejects
refuses to start instead, and says so in the log.

That distinction is why the chart's `startupProbe` gets 60 seconds against a default
`config.slack.auth_startup_grace` of `30s`. Raise one and you must raise the other, or a
Slack outage arrives as `CrashLoopBackOff` — which looks exactly like a bad token and is not
one.

## 4. Point Alertmanager at it

```yaml
receivers:
  - name: alertthread
    webhook_configs:
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts-critical
        send_resolved: true
        max_alerts: 0
```

⚠️ `max_alerts` must be `0` and `send_resolved` must be `true`. Both break correlation
silently if they are wrong — see [Troubleshoot](troubleshoot.md).

To require a bearer token on the webhook, set `webhookAuth.enabled=true` and
`webhookAuth.existingSecret`, then add the `http_config` block from
[Harden a deployment](harden-a-deployment.md). Do both halves in one change: the relay answers
`401` to a delivery without the credential, and Alertmanager does not retry a `401`.

## 5. ⚠️ Route the relay's own alerts away from the relay

The chart installs a `PrometheusRule` by default. **Every rule in it describes a way
`alertthread` stops delivering to Slack**, so routing those alerts through `alertthread` means
the notification saying "alerts are not reaching Slack" is itself an alert that will not reach
Slack.

```yaml
route:
  routes:
    # First, and it does not continue.
    - matchers: [ 'alertname=~"Alertthread.*"' ]
      receiver: slack-direct        # slack_configs, NOT this relay
      continue: false
```

This is not optional. The appearance of monitoring is what stops anybody checking by hand, so
the rules without the bypass route are worse than no rules at all. The full procedure, and the
`slack-direct` receiver, are in [Alert on the relay](alert-on-the-relay.md).

## Tune the thresholds

The numbers in the shipped rules are **starting points for a homelab-to-small-cluster alert
volume, not measurements.** Four are exposed:

```yaml
metrics:
  prometheusRule:
    thresholds:
      outboxOldestAgeSeconds: 300      # AlertthreadOutboxNotDraining
      outboxDepth: 500                 # AlertthreadOutboxBacklog
      slackCallErrorRatio: 0.1         # AlertthreadSlackCallsFailing
      slackRateLimitedPerSecond: 0.05  # AlertthreadRateLimitedBySlack
```

Tune `outboxOldestAgeSeconds` first. It is the primary delivery signal
([ADR 001 D11](../adr/001-adr.md)) and it is the answer to "how long may an alert sit
undelivered before somebody is woken up" — a question about your alerting, not about the
relay. A busy channel legitimately queues for longer than 300 seconds, because Slack allows
roughly one post per second per channel.

Everything else — the `for:` windows, the severities, the `> 0` thresholds on dead letters and
truncated alerts — ships as written. There is no threshold below "one" worth setting on an
alert nobody was told about. To change one, set `metrics.prometheusRule.enabled=false` and
apply your own `PrometheusRule` built from
[`deploy/alertthread.rules.yaml`](https://github.com/brianporeilly/alertthread/blob/main/deploy/alertthread.rules.yaml).

## SQLite or PostgreSQL

The chart defaults to SQLite on a PVC, because that is the deployment with no external
dependency and it is what the relay is designed around. It is exactly one replica.

```yaml
replicaCount: 1
config:
  storage:
    backend: sqlite
    url: sqlite:///var/lib/alertthread/state.sqlite
persistence:
  enabled: true
  size: 1Gi
```

The chart **refuses to render** `replicaCount > 1` on SQLite. Two processes on one SQLite file
corrupts correlation state, and the Kubernetes-side symptom — a pod stuck `Pending` on a
multi-attach error — points nowhere near the cause.

For more than one replica, switch the backend:

```yaml
replicaCount: 3
config:
  storage:
    backend: postgres
  slack:
    rate_limit_divisor: 3   # each replica holds its own token bucket
postgres:
  existingSecret: pg-cluster-app
  existingSecretKey: uri
```

That drops the PVC, drops the state mount, and switches the Deployment to `RollingUpdate`. The
connection string arrives as `ALERTTHREAD_STORAGE__URL` from the Secret rather than through the
ConfigMap, because it carries a password. The migration for an existing install is
[Enable HA with PostgreSQL](enable-ha-postgres.md).

## Message template overrides

```yaml
templates:
  firing: |
    …your MiniJinja template…
```

Any non-empty key becomes a `ConfigMap` mounted at `/etc/alertthread/templates`, which the
chart names to the relay as `templates.dir`. An override that will not compile is dropped and
its built-in kept, so a broken template degrades rather than stopping the relay — watch
`alertthread_fallback_posts_total` and the ERROR log. See
[Customize message templates](customize-templates.md).

## Recover a parked alert

The binary is in the image, so [`alertthread replay`](../reference/cli.md) is a `kubectl exec`
away. Pass the chart's config file explicitly:

```bash
kubectl -n observability exec deploy/alertthread -- \
    /alertthread replay --config /etc/alertthread/config/config.yaml
kubectl -n observability exec deploy/alertthread -- \
    /alertthread replay --config /etc/alertthread/config/config.yaml --commit
```

It is a dry run without `--commit`, and it is safe against a live relay.

`--config` rather than the `ALERTTHREAD_CONFIG` environment variable. Both work; the flag is
explicit about which file this invocation used, and the chart does not set the variable, so
there is nothing to disagree with.

Three `ALERTTHREAD_` names with no `__` in them are reserved — `ALERTTHREAD_CONFIG`,
`ALERTTHREAD_LOG` and `ALERTTHREAD_LOG_FORMAT` — and those three are safe to put in `env` or
`extraEnv`. `RUST_LOG` works too, and carries no prefix to collide with. Any *other* bare
`ALERTTHREAD_<WORD>` parses as a top-level configuration key that does not exist and stops the
relay, which is what makes a misspelling visible instead of silent. A chart test asserts the
chart itself only ever sets reserved bare names. See
[Configuration](../reference/configuration.md) for the full rule.

To turn the relay's logging up:

```bash
helm upgrade alertthread alertthread/alertthread \
    --reuse-values --set env.ALERTTHREAD_LOG='info\,alertthread=debug'
```

## Why the CRDs are not optional

`ServiceMonitor` and `PrometheusRule` are `monitoring.coreos.com/v1`, and the chart renders
both unless you turn them off explicitly. It does **not** gate them on a
`.Capabilities.APIVersions.Has` check, which is the usual pattern.

A capabilities check silently drops the objects when the CRD is absent, and the result is a
relay that looks monitored and is not — which is the exact failure `alertthread` exists to
prevent. A missing CRD should fail the install, loudly, once. Set
`metrics.serviceMonitor.enabled=false` and `metrics.prometheusRule.enabled=false` if you
scrape some other way, and then alert on
`alertthread_outbox_oldest_age_seconds` yourself.

One thing to keep in step if you change it: `metrics.serviceMonitor.jobLabel` decides the
`job` label on every scraped series, and `AlertthreadDown` and `AlertthreadAbsent` select on
`job="alertthread"`. The chart keeps them in agreement (including under `nameOverride`) and a
test asserts it, because a rule whose job label matches nothing evaluates empty for ever and
looks identical to a healthy relay.

## Where to look next

- [Verify artefacts](verify-artifacts.md) — check the signature and read the SBOM before you install
- [Harden a deployment](harden-a-deployment.md) — what the chart enforces, and the webhook token
- [Alert on the relay](alert-on-the-relay.md) — the bypass route, in full
- [Configuration](../reference/configuration.md) — every key under `config:`
- [Enable HA with PostgreSQL](enable-ha-postgres.md) — migrating an existing install
- [Troubleshoot](troubleshoot.md) — including the read-only-filesystem crash loop
