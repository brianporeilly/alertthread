# Metrics

*Status: populated in Phase 4, alongside the metrics registry.*

All metrics are prefixed `alertthread_` and exposed at `GET /metrics`.

**A new metric is not merged until it appears on this page.** That rule is in AGENTS.md.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `alertthread_alerts_received_total` | counter | `status` | Alerts arriving by firing/resolved |
| `alertthread_slack_calls_total` | counter | `method`, `outcome` | Slack API calls |
| `alertthread_slack_call_duration_seconds` | histogram | `method` | Slack API latency |
| `alertthread_outbox_depth` | gauge | `op` | Pending operations |
| `alertthread_outbox_oldest_age_seconds` | gauge | — | **Primary SLO signal** |
| `alertthread_tracked_fingerprints` | gauge | — | Correlation state size |
| `alertthread_orphan_resolves_total` | counter | — | State was lost |
| `alertthread_fallback_posts_total` | counter | `reason` | Degraded, but not silent |
| `alertthread_dead_letter_total` | counter | `reason` | **Silent: page on this** |
| `alertthread_rate_limited_total` | counter | `method` | Slack 429s |

## What to alert on

`alertthread_outbox_oldest_age_seconds` is the metric that actually means "alerts are not
reaching Slack". Alert on it in preference to the error counters, which tell you something
went wrong but not whether delivery is still happening.

`alertthread_dead_letter_total` is the other one that matters: it is the only counter that
means an alert was genuinely not delivered.

## ⚠️ The relay cannot alert on itself through itself

If the relay is down, an alert about the relay being down cannot be delivered *by* the
relay. Any `PrometheusRule` shipped for `alertthread` **must** be accompanied by an
Alertmanager route sending `job=alertthread` alerts to a **direct Slack receiver**,
bypassing the relay entirely.

Shipping the rule without that route is worse than shipping no rule at all, because it
creates the appearance of monitoring where there is none. This is the single most important
operational note in the project.
