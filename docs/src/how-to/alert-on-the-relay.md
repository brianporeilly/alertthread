# Alert on the relay

Goal: know when `alertthread` stops delivering alerts to Slack.

⚠️ **Read the next section before you apply anything.** The rules in this repository are
*actively harmful* if you route them the obvious way, and the obvious way is the default.

## The relay cannot alert on itself through itself

If the relay is down, an alert saying the relay is down cannot be delivered by the relay.

Every rule in `deploy/alertthread.rules.yaml` describes a way the relay stops reaching Slack.
Route those alerts to the relay and each one becomes a notification that arrives only in the
case where you do not need it. Worse, the *appearance* of monitoring is what stops anybody
checking by hand — so shipping the rules without a bypassing route is worse than shipping no
rules at all. [ADR 001 D11](../adr/001-adr.md) calls this out as a real circular dependency and
this page as the mandatory other half.

So the deliverable is two things, and the second is not optional:

1. the rules, loaded into Prometheus;
2. an Alertmanager route sending them to a **direct Slack receiver** that does not involve the
   relay.

## 1. Load the rules

**With the Helm chart, they are already loaded.** `metrics.prometheusRule.enabled` defaults to
`true`, and the chart embeds this file verbatim — warning included, plus the same warning as an
annotation so it survives into the cluster object where a YAML comment does not. Skip to
[step 2](#2-route-them-away-from-the-relay), which the chart cannot do for you.

Otherwise the file is a plain Prometheus rules file, so it works three ways.

**With the Prometheus Operator**, wrap it in a `PrometheusRule` — `spec` takes the `groups:`
key verbatim:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: alertthread
  namespace: observability
  labels:
    # Whatever your Prometheus's ruleSelector matches. kube-prometheus-stack's
    # default is release: <your-release-name>.
    release: kube-prometheus-stack
spec:
  groups:
    # …paste the `groups:` contents of deploy/alertthread.rules.yaml here…
```

**With plain Prometheus**, mount it and name it in `rule_files:`:

```yaml
rule_files:
  - /etc/prometheus/alertthread.rules.yaml
```

**Either way**, validate before applying:

```bash
promtool check rules deploy/alertthread.rules.yaml
```

`just check-rules` does exactly that from the pinned Prometheus image, and it has its own CI
job. A separate unit test asserts that every metric and label value the rules name is one the
binary actually exports, because a rule on a metric that does not exist never fires and looks
identical to a healthy relay.

The rules need a scrape job named `alertthread` for `AlertthreadDown` and `AlertthreadAbsent`
to work. With the operator, that is a `ServiceMonitor`; `/metrics` needs no credentials even
when the webhook does.

⚠️ A `ServiceMonitor` labels the scrape `job` with the **Service's name** unless `jobLabel`
says otherwise, and a Helm-installed Service is named after the release. If you write your own
`ServiceMonitor`, set `jobLabel: app.kubernetes.io/name` or relabel `job` to `alertthread`
explicitly — otherwise those two rules match nothing, for ever, and that looks exactly like a
healthy relay. The chart does this and asserts it.

## 2. Route them away from the relay

```yaml
route:
  receiver: alertthread                 # your normal receiver
  routes:
    # FIRST, and it does not continue: alertthread's own alerts never reach the
    # relay, whatever the rest of the tree says.
    - matchers:
        - 'alertname=~"Alertthread.*"'
      receiver: slack-direct
      continue: false
      group_wait: 10s
      repeat_interval: 1h

receivers:
  - name: alertthread
    webhook_configs:
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts
        send_resolved: true
        max_alerts: 0

  # No webhook, no relay. Alertmanager talks to Slack itself.
  - name: slack-direct
    slack_configs:
      - channel: "#alerts-meta"
        api_url_file: /etc/alertmanager/secrets/slack-webhook/url
        send_resolved: true
        title: '{{ .CommonLabels.alertname }}'
        text: '{{ range .Alerts }}{{ .Annotations.description }}{{ end }}'
```

Three details in there are load-bearing:

- **The sub-route is first, and `continue: false`.** Alertmanager takes the first matching
  child route; a later one, or one that continues, sends the same alert to the relay as well.
- **`slack_configs`, not `webhook_configs`.** This receiver needs its own Slack incoming
  webhook URL or bot token. That is a second credential to hold, and it is the price of the
  bypass — there is no way to avoid it that does not reintroduce the circularity.
- **Match on `alertname`, not on `job`.** Both work: every rule in the file carries
  `job="alertthread"`, and the aggregations are written `by (job, instance)` deliberately so it
  survives. But a bare `sum()` in some future edit would drop `job` and silently unroute the
  alert, while the alert *names* cannot be changed by accident. A test asserts every alert in
  the file starts with `Alertthread` and that no expression aggregates the label away.

### Put it in a different channel

`#alerts-meta` above, rather than the channel the relay posts to. Not tidiness: when the relay
is broken, its channel is by definition the one where nothing is arriving, and a message about
the outage landing in the quiet channel is easy to miss. A channel a human actively watches for
this purpose is worth the extra room.

## 3. Prove the bypass works

Test the path, not the config. Break the relay deliberately and watch a message arrive:

```bash
kubectl -n observability scale deploy/alertthread --replicas=0
# AlertthreadAbsent has `for: 15m`; AlertthreadDown fires after 5m.
# Wait, then check #alerts-meta.
kubectl -n observability scale deploy/alertthread --replicas=1
```

If nothing arrives, the route is wrong — and you have just found that out on a Tuesday
afternoon rather than during an incident. An untested bypass is the same as no bypass, because
the case it exists for is the case where nothing else works.

While the relay is scaled to zero, note what Alertmanager's own logs say about the *other*
receiver: webhook POSTs failing, retried a few times, then given up on. That is the silence
this route exists to be immune to.

## What the rules cover

Full descriptions are in the file. The shape of it:

| Alert | Fires when | Severity |
|---|---|---|
| `AlertthreadOutboxNotDraining` | The oldest queued delivery has waited over 5 minutes | critical |
| `AlertthreadDeadLetter` | An operation was parked — an alert nobody was told about | critical |
| `AlertthreadDeadLettersStillParked` | Parked rows are still there an hour later | warning |
| `AlertthreadOutboxBacklog` | Over 500 queued operations for 15 minutes | warning |
| `AlertthreadSlackTokenRejected` | `auth.test` is failing | critical |
| `AlertthreadSlackCallsFailing` | Over 10% of Slack calls failing for 15 minutes | warning |
| `AlertthreadRateLimitedBySlack` | Slack has been returning 429 for half an hour | warning |
| `AlertthreadTemplateFallback` | A template override is broken; messages are the built-in ones | warning |
| `AlertthreadAlertsTruncated` | Alertmanager's `max_alerts` is not `0` | critical |
| `AlertthreadOrphanResolves` | Resolutions arriving with no state behind them | warning |
| `AlertthreadWebhookBodyRejected` | A body the relay could not parse — those alerts are lost | critical |
| `AlertthreadStoreUnavailableAtIngest` | Sustained `503`s: the store is unreachable | critical |
| `AlertthreadWebhookUnauthenticated` | Deliveries refused for a missing or wrong credential | warning |
| `AlertthreadDown` | Prometheus cannot scrape it | critical |
| `AlertthreadAbsent` | There is no scrape target at all — the deadman for the rest | critical |
| `AlertthreadStoreSampleFailing` | The store gauges above are stale | warning |
| `AlertthreadDeadLettersRevived` | Parked alerts were just re-queued; expect late messages | info |

`AlertthreadOutboxNotDraining` is the one that matters most. D11 puts it plainly:
`alertthread_outbox_oldest_age_seconds` is the metric that actually means "alerts are not
reaching Slack", and it is worth more than any error counter because it fires for causes nobody
has thought of yet. Every other rule in the file is a cause; that one is the symptom.

The thresholds are starting points for a homelab-to-small-cluster alert volume, not
measurements. `> 300` on the outbox age is the first one to tune to your own delivery latency.

## Also worth having: a watchdog through the relay

The rules above watch the relay from outside. They cannot tell you that the *whole* path works,
because every one of them is satisfied by a relay that is up, healthy, and posting into a
channel nobody reads.

`kube-prometheus-stack` ships an always-firing `Watchdog` alert for this. Route it *through*
the relay, to a quiet channel, and set `repeat_interval` to something like 6 hours:

```yaml
    - matchers: [ 'alertname="Watchdog"' ]
      receiver: alertthread
      repeat_interval: 6h
```

Now a message arriving every six hours means Prometheus, Alertmanager, the webhook, the outbox,
the Slack client and the token are all working. A message that stops arriving means one of them
is not, and no metric had to be correct for you to notice — which is the only kind of check
that survives the relay's own reporting being broken.

## Where to look next

- [Metrics](../reference/metrics.md) — every metric these rules read, and what its labels mean
- [Troubleshoot](troubleshoot.md) — what to do once one of them fires
- [Failure semantics](../explanation/failure-semantics.md) — why the outbox age is the signal
