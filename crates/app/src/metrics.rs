//! ADR 001 D11's metrics, and the one rule about how they are read.
//!
//! # Gauges are sampled, never queried on scrape
//!
//! Four of these — `outbox_depth`, `outbox_oldest_age_seconds`, `tracked_fingerprints` and
//! the dead-letter level — describe the *store*, and none of them is read inside
//! `GET /metrics`. A background task samples them on an interval and the handler serves the
//! last sample.
//!
//! That is deliberate and it is not premature optimisation. Prometheus scrapes every 15
//! seconds, from every replica, for ever: querying the outbox from the handler makes the
//! monitoring system a load generator pointed at the queue it is monitoring. Worse, a slow
//! store would make the scrape time out — and a timed-out scrape loses *every* metric in
//! the response, including the counters that would have said what was wrong.
//!
//! # Cardinality
//!
//! Every label value in this module comes from a closed set — a Rust enum's `as_str`, or a
//! `SlackError::outcome`. Slack's error codes are open-ended and none of them reaches a
//! label; that is what [`SlackError::outcome`](alertthread_slack::SlackError::outcome)
//! exists for.

use std::sync::atomic::{AtomicI64, AtomicU64};

use alertthread_core::Notice;
use alertthread_slack::{Degradation, SlackError, SlackMethod};
use alertthread_store::{OpKind, StoreStats};
use chrono::{DateTime, Utc};
use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};

/// `{status}` on `alertthread_alerts_received_total`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StatusLabel {
    /// `firing`, `resolved`, or whatever unrecognised value the sender used.
    ///
    /// Unrecognised statuses are folded to `other` rather than passed through: the raw
    /// string comes from outside the relay, and a label value an attacker or a broken proxy
    /// controls is an unbounded label value. The verbatim string reaches the log line
    /// instead, via [`Notice::UnknownStatus`].
    pub status: &'static str,
}

/// `{method}` on the Slack call metrics.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MethodLabel {
    /// The Web API method name, as Slack spells it.
    pub method: &'static str,
}

/// `{method, outcome}` on `alertthread_slack_calls_total`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct CallLabel {
    /// The Web API method name.
    pub method: &'static str,
    /// `ok`, or [`SlackError::outcome`].
    pub outcome: &'static str,
}

/// `{method, source}` on `alertthread_rate_limited_total`.
///
/// **`source` is not in ADR 001 D11**, which lists `{method}` alone. It is here because the
/// relay is rate-limited in two quite different ways and the operator's next action differs:
/// `slack` means Slack pushed back and the outbox is riding it out, `local` means this
/// process's own token bucket paced itself and no request was made at all. Without the
/// label those are one number, and "are we being throttled or are we throttling ourselves?"
/// is unanswerable — which is the only question this counter gets asked.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RateLimitLabel {
    /// The Web API method that was limited.
    pub method: &'static str,
    /// `slack` or `local`.
    pub source: &'static str,
}

/// `{op}` on `alertthread_outbox_depth`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OpLabel {
    /// The op kind, as stored in the `outbox.op` column.
    pub op: &'static str,
}

/// `{reason}` on `alertthread_fallback_posts_total` and `alertthread_dead_letter_total`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabel {
    /// Why, from a closed set.
    pub reason: &'static str,
}

/// `{outcome}` on `alertthread_webhook_requests_total`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OutcomeLabel {
    /// `accepted`, `rejected`, `store_unavailable`, `misconfigured`, `auth_missing` or
    /// `auth_mismatch`.
    ///
    /// The two `auth_*` values come from [`crate::auth`] and are separated because the fix
    /// differs: `auth_missing` is a sender with no credential configured at all, and
    /// `auth_mismatch` is one whose credential is not the one this relay holds.
    pub outcome: &'static str,
}

/// Every metric the relay publishes.
///
/// Built once at startup and shared. Each metric is `Arc`-free because
/// `prometheus-client`'s families and counters are internally shared; the whole struct goes
/// behind one `Arc`.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,

    /// Alerts arriving, by status. ADR 001 D11.
    pub alerts_received: Family<StatusLabel, Counter>,
    /// Webhook deliveries, by what the relay did with them.
    ///
    /// **Not in D11.** AGENTS.md forbids swallowing an error without a metric, and a body
    /// the relay cannot parse is answered with `400` — a real, if rare, way for an alert to
    /// go undelivered. Without this counter that outcome exists only in a log line.
    pub webhook_requests: Family<OutcomeLabel, Counter>,
    /// Slack calls, by method and outcome.
    pub slack_calls: Family<CallLabel, Counter>,
    /// Slack call latency, by method.
    pub slack_call_duration: Family<MethodLabel, Histogram>,
    /// Deliveries deferred by a rate limit — Slack's, or the relay's own.
    pub rate_limited: Family<RateLimitLabel, Counter>,
    /// Resolutions that arrived with no correlation state behind them.
    pub orphan_resolves: Counter,
    /// Storm collapses opened.
    ///
    /// **Not in D11.** [`Notice::StormCollapsed`] is one of the planner's outputs, and a
    /// notice the shell logged but never counted would be a decision nobody could see the
    /// rate of.
    pub storm_collapses: Counter,
    /// Deliveries in which Alertmanager reported dropping alerts (`max_alerts`, ADR 001 D8).
    ///
    /// **Not in D11.** D8 says the symptom of a non-zero `max_alerts` — orphan resolves —
    /// "points nowhere near the cause". This *is* the cause, reported by the sender itself,
    /// and burying it in a log line would leave D8's warning with nothing to alert on.
    pub alerts_truncated: Counter,
    /// Messages that came out of the hardcoded fallback instead of a template (D9).
    pub fallback_posts: Family<ReasonLabel, Counter>,
    /// Ops parked because they will never succeed. **Page on this.**
    pub dead_letters: Family<ReasonLabel, Counter>,
    /// Parked ops returned to the queue after the condition that parked them cleared.
    ///
    /// **Not in D11**, which stops at the dead-letter counter. It is here because the
    /// recovery is automatic: without a counter, an operator who replaced a revoked token
    /// would see `alertthread_dead_letter_total` stop rising and have no way to tell whether
    /// the alerts already parked were delivered or written off.
    pub dead_letters_revived: Counter,

    /// Pending outbox rows, by kind. Sampled.
    pub outbox_depth: Family<OpLabel, Gauge>,
    /// How long the oldest pending row has been waiting. **The primary SLO signal.** Sampled.
    pub outbox_oldest_age: Gauge<f64, AtomicU64>,
    /// Rows parked by the dead-letter path and never cleared. Sampled.
    pub outbox_dead_lettered: Gauge,
    /// `alert_message` rows. Sampled.
    pub tracked_fingerprints: Gauge,
    /// Whether the bot token was accepted at the last check: 1 or 0.
    ///
    /// **Not in D11**, and deliberately a metric rather than an input to `/readyz`. A token
    /// revoked at 2pm with nothing firing until 3am is a silent failure found at the worst
    /// possible moment, so it is probed periodically — but going *unready* over it would
    /// make Alertmanager's POST fail and the alert be lost, when accepting it into the
    /// outbox and retrying is exactly what the outbox is for.
    pub slack_auth_valid: Gauge,
    /// Whether the last store sample succeeded: 1 or 0.
    ///
    /// **Not in D11.** Every gauge above it is a *sample*, and a sample that stopped being
    /// refreshed looks identical to one whose value stopped changing. This is what tells
    /// the two apart.
    pub store_sample_ok: Gauge,
}

/// The metric name prefix ADR 001 D12 settles on.
const PREFIX: &str = "alertthread";

/// Buckets for `alertthread_slack_call_duration_seconds`.
///
/// Reaching into seconds rather than stopping at 1 s: a Slack call that takes eight seconds
/// is the interesting case here, because the client's own timeout is fifteen and everything
/// above that arrives as a transport error instead.
const LATENCY_BUCKETS: [f64; 12] = [
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0,
];

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds and registers every metric.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one registration per metric, in the order ADR 001 D11 lists them. \
                  Splitting it would put the name, the help text and the type of a single \
                  metric in two places, which is where a metric ends up registered twice \
                  under one name."
    )]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix(PREFIX);

        let alerts_received = Family::<StatusLabel, Counter>::default();
        registry.register(
            "alerts_received",
            "Alerts accepted from Alertmanager, by status",
            alerts_received.clone(),
        );

        let webhook_requests = Family::<OutcomeLabel, Counter>::default();
        registry.register(
            "webhook_requests",
            "Webhook deliveries, by what the relay did with them",
            webhook_requests.clone(),
        );

        let slack_calls = Family::<CallLabel, Counter>::default();
        registry.register(
            "slack_calls",
            "Slack Web API calls, by method and outcome",
            slack_calls.clone(),
        );

        let slack_call_duration = Family::<MethodLabel, Histogram>::new_with_constructor(|| {
            Histogram::new(LATENCY_BUCKETS)
        });
        registry.register_with_unit(
            "slack_call_duration",
            "Slack Web API call latency, by method",
            Unit::Seconds,
            slack_call_duration.clone(),
        );

        let rate_limited = Family::<RateLimitLabel, Counter>::default();
        registry.register(
            "rate_limited",
            "Deliveries deferred by a rate limit, by method and whether Slack or the relay \
             imposed it",
            rate_limited.clone(),
        );

        let orphan_resolves = Counter::default();
        registry.register(
            "orphan_resolves",
            "Resolutions that arrived with no correlation state behind them",
            orphan_resolves.clone(),
        );

        let storm_collapses = Counter::default();
        registry.register(
            "storm_collapses",
            "Storm-collapse groups opened",
            storm_collapses.clone(),
        );

        let alerts_truncated = Counter::default();
        registry.register(
            "alerts_truncated",
            "Alerts Alertmanager dropped from a webhook body because max_alerts is not 0",
            alerts_truncated.clone(),
        );

        let fallback_posts = Family::<ReasonLabel, Counter>::default();
        registry.register(
            "fallback_posts",
            "Messages built from the hardcoded fallback instead of a template",
            fallback_posts.clone(),
        );

        let dead_letters = Family::<ReasonLabel, Counter>::default();
        registry.register(
            "dead_letter",
            "Outbox operations parked because they will never succeed",
            dead_letters.clone(),
        );

        let dead_letters_revived = Counter::default();
        registry.register(
            "dead_letter_revived",
            "Parked outbox operations returned to the queue after the condition that parked \
             them cleared",
            dead_letters_revived.clone(),
        );

        let outbox_depth = Family::<OpLabel, Gauge>::default();
        registry.register(
            "outbox_depth",
            "Outbox rows waiting to be delivered, by operation",
            outbox_depth.clone(),
        );

        let outbox_oldest_age = Gauge::<f64, AtomicU64>::default();
        registry.register_with_unit(
            "outbox_oldest_age",
            "Age of the oldest undelivered outbox row",
            Unit::Seconds,
            outbox_oldest_age.clone(),
        );

        let outbox_dead_lettered = Gauge::<i64, AtomicI64>::default();
        registry.register(
            "outbox_dead_lettered",
            "Outbox rows parked by the dead-letter path and not yet cleared",
            outbox_dead_lettered.clone(),
        );

        let tracked_fingerprints = Gauge::<i64, AtomicI64>::default();
        registry.register(
            "tracked_fingerprints",
            "Alerts the relay is holding correlation state for",
            tracked_fingerprints.clone(),
        );

        let slack_auth_valid = Gauge::<i64, AtomicI64>::default();
        registry.register(
            "slack_auth_valid",
            "Whether Slack accepted the bot token at the last periodic check",
            slack_auth_valid.clone(),
        );

        let store_sample_ok = Gauge::<i64, AtomicI64>::default();
        registry.register(
            "store_sample_ok",
            "Whether the last background sample of the store succeeded",
            store_sample_ok.clone(),
        );

        Self {
            registry,
            alerts_received,
            webhook_requests,
            slack_calls,
            slack_call_duration,
            rate_limited,
            orphan_resolves,
            storm_collapses,
            alerts_truncated,
            fallback_posts,
            dead_letters,
            dead_letters_revived,
            outbox_depth,
            outbox_oldest_age,
            outbox_dead_lettered,
            tracked_fingerprints,
            slack_auth_valid,
            store_sample_ok,
        }
    }

    /// Renders the registry in Prometheus text format.
    ///
    /// # Errors
    ///
    /// [`std::fmt::Error`] if the encoder cannot write, which for a `String` sink means the
    /// allocator gave up. Propagated rather than unwrapped: this is the delivery path's
    /// observability, and taking the process down to report a formatting problem would
    /// remove the only thing that could say why.
    pub fn render(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::new();
        encode(&mut out, &self.registry)?;
        Ok(out)
    }

    /// Counts one alert arriving, by the status Alertmanager gave it.
    pub fn alert_received(&self, status: &alertthread_core::AlertStatus) {
        self.alerts_received
            .get_or_create(&StatusLabel {
                status: status_label(status),
            })
            .inc();
    }

    /// Counts everything a plan's notices describe (ADR 001 D11, D8, D9).
    ///
    /// One place, so a notice added to the core cannot be logged by the shell and then
    /// silently not counted — which is how a condition the planner went to the trouble of
    /// reporting ends up invisible.
    pub fn observe(&self, notices: &[Notice]) {
        for notice in notices {
            match notice {
                Notice::OrphanResolve { .. } => {
                    self.orphan_resolves.inc();
                }
                Notice::StormCollapsed { .. } => {
                    self.storm_collapses.inc();
                }
                Notice::AlertsTruncated { count } => {
                    self.alerts_truncated.inc_by(*count);
                }
                // Logged by the caller, which has the raw status string and the counts. A
                // counter here would only say "something was odd" without saying what.
                Notice::EmptyBatch
                | Notice::UnknownStatus { .. }
                | Notice::OutcomeCountMismatch { .. } => {}
            }
        }
    }

    /// Records a Slack call that succeeded.
    pub fn slack_ok(&self, method: SlackMethod, seconds: f64) {
        self.record_call(method, "ok", seconds);
    }

    /// Records a Slack call that failed, with the low-cardinality outcome for its variant.
    pub fn slack_failed(&self, method: SlackMethod, error: &SlackError, seconds: f64) {
        self.record_call(method, error.outcome(), seconds);
    }

    fn record_call(&self, method: SlackMethod, outcome: &'static str, seconds: f64) {
        self.slack_calls
            .get_or_create(&CallLabel {
                method: method.as_str(),
                outcome,
            })
            .inc();
        self.slack_call_duration
            .get_or_create(&MethodLabel {
                method: method.as_str(),
            })
            .observe(seconds);
    }

    /// Counts a delivery Slack asked us to retry later.
    pub fn rate_limited_by_slack(&self, method: SlackMethod) {
        self.rate_limited
            .get_or_create(&RateLimitLabel {
                method: method.as_str(),
                source: "slack",
            })
            .inc();
    }

    /// Counts a delivery the relay's own token bucket held back.
    pub fn rate_limited_locally(&self, method: SlackMethod) {
        self.rate_limited
            .get_or_create(&RateLimitLabel {
                method: method.as_str(),
                source: "local",
            })
            .inc();
    }

    /// Counts a message that had to be built without its template (ADR 001 D9).
    pub fn degraded(&self, degradation: &Degradation) {
        self.fallback_posts
            .get_or_create(&ReasonLabel {
                reason: degradation.reason.as_str(),
            })
            .inc();
    }

    /// Counts an op that was parked. **This is the counter to page on.**
    pub fn dead_lettered(&self, reason: &'static str) {
        self.dead_letters
            .get_or_create(&ReasonLabel { reason })
            .inc();
    }

    /// Counts parked ops returned to the queue.
    pub fn dead_letters_revived(&self, count: u64) {
        self.dead_letters_revived.inc_by(count);
    }

    /// Counts one webhook delivery by what the relay did with it.
    pub fn webhook(&self, outcome: &'static str) {
        self.webhook_requests
            .get_or_create(&OutcomeLabel { outcome })
            .inc();
    }

    /// Publishes one background sample of the store.
    ///
    /// Every op kind is written on every sample, including the ones with nothing queued. A
    /// gauge that simply stops being reported reads as "no data" in Prometheus rather than
    /// as "nothing pending", and an alert on outbox depth would go stale rather than clear.
    pub fn publish(&self, stats: &StoreStats, now: DateTime<Utc>) {
        for kind in OpKind::ALL {
            let depth = stats.outbox_depth.get(&kind).copied().unwrap_or(0);
            self.outbox_depth
                .get_or_create(&OpLabel { op: kind.as_str() })
                .set(i64::try_from(depth).unwrap_or(i64::MAX));
        }

        self.outbox_oldest_age.set(
            stats
                .oldest_queued_at
                .map_or(0.0, |queued| age_seconds(queued, now)),
        );
        self.outbox_dead_lettered
            .set(i64::try_from(stats.dead_lettered).unwrap_or(i64::MAX));
        self.tracked_fingerprints
            .set(i64::try_from(stats.tracked_fingerprints).unwrap_or(i64::MAX));
        self.store_sample_ok.set(1);
    }

    /// Records that a background sample of the store failed.
    ///
    /// The gauges keep their last values rather than being zeroed. Zeroing would say "the
    /// queue is empty", which is the single most misleading thing this relay could claim
    /// while its store is unreachable; `store_sample_ok` is what says the numbers are stale.
    pub fn sample_failed(&self) {
        self.store_sample_ok.set(0);
    }
}

/// How long ago `queued` was, never negative.
///
/// Clock skew between the relay and its database is normal, and a negative age on the one
/// gauge an operator alerts on would look like a metric bug rather than like clock skew.
fn age_seconds(queued: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let millis = now.signed_duration_since(queued).num_milliseconds().max(0);
    // Milliseconds via `f64::from` on an i32-safe range: 24 days of backlog is already far
    // past anything worth distinguishing, and it keeps this free of a lossy cast.
    let clamped = i32::try_from(millis).unwrap_or(i32::MAX);
    f64::from(clamped) / 1e3
}

/// The `status` label for an alert, from a closed set.
fn status_label(status: &alertthread_core::AlertStatus) -> &'static str {
    match status {
        alertthread_core::AlertStatus::Firing => "firing",
        alertthread_core::AlertStatus::Resolved => "resolved",
        // Folded rather than passed through: this string comes from outside the relay, and
        // a label value the sender controls is an unbounded label value.
        alertthread_core::AlertStatus::Unknown(_) => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::{Metrics, age_seconds, status_label};
    use alertthread_core::{AlertStatus, Fingerprint, GroupKey, Notice};
    use alertthread_slack::{Degradation, FallbackReason, SlackError, SlackMethod, TemplateKind};
    use alertthread_store::{OpKind, StoreStats};
    use chrono::{DateTime, TimeDelta, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn rendered(metrics: &Metrics) -> String {
        metrics.render().expect("the registry encodes")
    }

    #[test]
    fn every_metric_adr_001_d11_names_is_registered_under_the_project_prefix() {
        // D11's list, verbatim. A metric missing from the exposition is a dashboard panel
        // that says "no data" and an operator who concludes the relay is fine.
        let metrics = Metrics::new();
        metrics.alert_received(&AlertStatus::Firing);
        metrics.slack_ok(SlackMethod::PostMessage, 0.2);
        metrics.rate_limited_by_slack(SlackMethod::PostMessage);
        metrics.dead_lettered("invalid_auth");
        metrics.degraded(&Degradation {
            template: TemplateKind::Firing,
            reason: FallbackReason::RenderFailed,
            detail: "boom".to_owned(),
        });
        metrics.publish(&StoreStats::default(), at(0));

        let text = rendered(&metrics);
        for name in [
            "alertthread_alerts_received_total",
            "alertthread_slack_calls_total",
            "alertthread_slack_call_duration_seconds",
            "alertthread_outbox_depth",
            "alertthread_outbox_oldest_age_seconds",
            "alertthread_tracked_fingerprints",
            "alertthread_orphan_resolves_total",
            "alertthread_fallback_posts_total",
            "alertthread_dead_letter_total",
            "alertthread_rate_limited_total",
            "alertthread_slack_auth_valid",
        ] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
    }

    #[test]
    fn an_alert_is_counted_under_the_status_it_arrived_with() {
        let metrics = Metrics::new();
        metrics.alert_received(&AlertStatus::Firing);
        metrics.alert_received(&AlertStatus::Firing);
        metrics.alert_received(&AlertStatus::Resolved);

        let text = rendered(&metrics);
        assert!(
            text.contains("alertthread_alerts_received_total{status=\"firing\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_alerts_received_total{status=\"resolved\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn a_status_the_sender_invented_does_not_become_a_label_value() {
        // The raw string is attacker- or proxy-controlled. Passing it through would let one
        // misbehaving sender create unbounded label cardinality in somebody's Prometheus.
        assert_eq!(
            status_label(&AlertStatus::Unknown("../x".to_owned())),
            "other"
        );
        assert_eq!(status_label(&AlertStatus::Firing), "firing");
        assert_eq!(status_label(&AlertStatus::Resolved), "resolved");
    }

    #[test]
    fn the_notices_a_plan_produces_are_counted_in_one_place() {
        let metrics = Metrics::new();
        metrics.observe(&[
            Notice::OrphanResolve {
                fingerprint: Fingerprint::new("abc"),
            },
            Notice::OrphanResolve {
                fingerprint: Fingerprint::new("def"),
            },
            Notice::StormCollapsed {
                group_key: GroupKey::new("gk"),
                members: 6,
            },
            Notice::AlertsTruncated { count: 12 },
            Notice::EmptyBatch,
            Notice::UnknownStatus {
                fingerprint: Fingerprint::new("abc"),
                status: "suppressed".to_owned(),
            },
            Notice::OutcomeCountMismatch {
                alerts: 2,
                outcomes: 1,
            },
        ]);

        let text = rendered(&metrics);
        assert!(
            text.contains("alertthread_orphan_resolves_total 2"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_storm_collapses_total 1"),
            "{text}"
        );
        // The count, not the number of deliveries: 12 alerts were dropped, and that is the
        // number worth alerting on.
        assert!(
            text.contains("alertthread_alerts_truncated_total 12"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_slack_call_is_labelled_by_variant_and_never_by_slacks_error_string() {
        // Slack's error codes are open-ended. Putting one in a label is how a Prometheus
        // falls over, which is why `SlackError::outcome` exists.
        let metrics = Metrics::new();
        metrics.slack_failed(
            SlackMethod::UpdateMessage,
            &SlackError::MessageNotFound {
                method: SlackMethod::UpdateMessage,
                code: "message_not_found".to_owned(),
            },
            0.05,
        );

        let text = rendered(&metrics);
        assert!(
            text.contains(
                "alertthread_slack_calls_total{method=\"chat.update\",outcome=\"message_not_found\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "alertthread_slack_call_duration_seconds_count{method=\"chat.update\"} 1"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_rate_limit_records_whether_slack_or_the_relay_imposed_it() {
        // Not in D11, and the reason it is here: "are we being throttled, or throttling
        // ourselves?" is the only question this counter gets asked, and one number cannot
        // answer it.
        let metrics = Metrics::new();
        metrics.rate_limited_by_slack(SlackMethod::PostMessage);
        metrics.rate_limited_locally(SlackMethod::PostMessage);
        metrics.rate_limited_locally(SlackMethod::PostMessage);

        let text = rendered(&metrics);
        assert!(
            text.contains(
                "alertthread_rate_limited_total{method=\"chat.postMessage\",source=\"slack\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "alertthread_rate_limited_total{method=\"chat.postMessage\",source=\"local\"} 2"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_sample_reports_every_op_kind_even_the_empty_ones() {
        // A gauge that stops being reported reads as "no data", not as "nothing pending" —
        // so an alert on outbox depth would go stale rather than clear.
        let metrics = Metrics::new();
        let stats = StoreStats {
            outbox_depth: [(OpKind::Post, 3)].into_iter().collect(),
            dead_lettered: 2,
            oldest_queued_at: Some(at(1_000)),
            tracked_fingerprints: 41,
        };
        metrics.publish(&stats, at(1_040));

        let text = rendered(&metrics);
        assert!(
            text.contains("alertthread_outbox_depth{op=\"post\"} 3"),
            "{text}"
        );
        for empty in [
            "post_group",
            "refresh",
            "refresh_group",
            "resolve",
            "post_orphan_resolved",
        ] {
            assert!(
                text.contains(&format!("alertthread_outbox_depth{{op=\"{empty}\"}} 0")),
                "{empty} missing from:\n{text}"
            );
        }
        assert!(
            text.contains("alertthread_outbox_oldest_age_seconds 40.0"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_outbox_dead_lettered 2"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_tracked_fingerprints 41"),
            "{text}"
        );
        assert!(text.contains("alertthread_store_sample_ok 1"), "{text}");
    }

    #[test]
    fn an_empty_queue_reports_an_age_of_zero_rather_than_the_last_one_it_saw() {
        let metrics = Metrics::new();
        metrics.publish(
            &StoreStats {
                oldest_queued_at: Some(at(0)),
                ..StoreStats::default()
            },
            at(600),
        );
        metrics.publish(&StoreStats::default(), at(600));

        assert!(
            rendered(&metrics).contains("alertthread_outbox_oldest_age_seconds 0.0"),
            "a drained queue has to clear the alert it raised"
        );
    }

    #[test]
    fn a_failed_sample_says_so_without_claiming_the_queue_is_empty() {
        // Zeroing the gauges would say "the queue is empty" — the single most misleading
        // thing this relay could claim while its store is unreachable.
        let metrics = Metrics::new();
        metrics.publish(
            &StoreStats {
                outbox_depth: [(OpKind::Post, 7)].into_iter().collect(),
                oldest_queued_at: Some(at(0)),
                ..StoreStats::default()
            },
            at(90),
        );
        metrics.sample_failed();

        let text = rendered(&metrics);
        assert!(text.contains("alertthread_store_sample_ok 0"), "{text}");
        assert!(
            text.contains("alertthread_outbox_depth{op=\"post\"} 7"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_outbox_oldest_age_seconds 90.0"),
            "{text}"
        );
    }

    #[test]
    fn an_age_is_never_negative() {
        // Clock skew between the relay and its database is normal, and a negative age on
        // the one gauge an operator alerts on looks like a metric bug rather than skew.
        assert!((age_seconds(at(100), at(40)) - 0.0).abs() < f64::EPSILON);
        assert!((age_seconds(at(100), at(160)) - 60.0).abs() < f64::EPSILON);
        assert!(
            (age_seconds(at(100), at(100) + TimeDelta::milliseconds(1_500)) - 1.5).abs() < 1e-9
        );
    }

    #[test]
    fn a_backlog_beyond_what_the_gauge_can_hold_is_clamped_rather_than_wrapped() {
        // 24 days is already far past anything worth distinguishing, and a wrapped value
        // would read as a healthy queue.
        assert!(age_seconds(at(0), at(10_000_000)) > 2_000_000.0);
    }

    #[test]
    fn a_degraded_message_is_counted_under_the_reason_it_degraded() {
        let metrics = Metrics::new();
        for reason in [FallbackReason::RenderFailed, FallbackReason::EmptyOutput] {
            metrics.degraded(&Degradation {
                template: TemplateKind::GroupSummary,
                reason,
                detail: String::new(),
            });
        }

        let text = rendered(&metrics);
        assert!(
            text.contains("alertthread_fallback_posts_total{reason=\"render_failed\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("alertthread_fallback_posts_total{reason=\"empty_output\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn every_webhook_outcome_is_counted() {
        let metrics = Metrics::new();
        metrics.webhook("accepted");
        metrics.webhook("rejected");
        metrics.webhook("store_unavailable");

        let text = rendered(&metrics);
        for outcome in ["accepted", "rejected", "store_unavailable"] {
            assert!(
                text.contains(&format!(
                    "alertthread_webhook_requests_total{{outcome=\"{outcome}\"}} 1"
                )),
                "{outcome} missing from:\n{text}"
            );
        }
    }

    #[test]
    fn the_auth_gauge_reports_both_states() {
        let metrics = Metrics::new();
        metrics.slack_auth_valid.set(1);
        assert!(rendered(&metrics).contains("alertthread_slack_auth_valid 1"));
        metrics.slack_auth_valid.set(0);
        assert!(rendered(&metrics).contains("alertthread_slack_auth_valid 0"));
    }

    #[test]
    fn a_slow_slack_call_lands_in_a_bucket_that_can_hold_it() {
        // The client's own timeout is fifteen seconds, so buckets that stopped at one
        // second would put every genuinely slow call in `+Inf` and lose the distribution
        // exactly where it matters.
        let metrics = Metrics::new();
        metrics.slack_ok(SlackMethod::PostMessage, 8.0);

        let text = rendered(&metrics);
        assert!(
            text.contains("le=\"15.0\",method=\"chat.postMessage\"} 1")
                || text.contains("method=\"chat.postMessage\",le=\"15.0\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn the_registry_is_debuggable_and_the_default_is_a_fresh_one() {
        let metrics = Metrics::default();
        assert!(format!("{metrics:?}").contains("Metrics"));
        assert!(rendered(&metrics).contains("alertthread_"));
    }
}
