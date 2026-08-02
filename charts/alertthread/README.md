# alertthread

Alertmanager → Slack relay with fingerprint-keyed threading and update-on-resolve.

```bash
helm install alertthread oci://ghcr.io/brianporeilly/charts/alertthread \
    --namespace observability --create-namespace \
    --set config.slack.default_channel='#alerts' \
    --set slack.existingSecret=alertthread-slack
```

Those two settings are the only required ones.

**The documentation for this chart is not here.** `values.yaml` is written to be read and
documents every setting in place; [Install with Helm](../../docs/src/how-to/install-with-helm.md)
is the how-to, and [Configuration](../../docs/src/reference/configuration.md) is the authority
on every key under `config:`. Duplicating them into a chart README would give the next reader
two answers and no way to tell which one is stale.

Three things worth knowing before you install:

- **Route the relay's own alerts away from the relay.** The chart installs a `PrometheusRule`
  whose every rule describes a way this relay stops delivering to Slack.
  [Alert on the relay](../../docs/src/how-to/alert-on-the-relay.md) is not optional reading.
- **The thresholds are starting points, not measurements.** Four are exposed under
  `metrics.prometheusRule.thresholds`.
- **`files/alertthread.rules.yaml` is generated.** `deploy/alertthread.rules.yaml` at the
  repository root is the original; `just chart-sync` copies it and `just chart` fails if they
  differ.
