//! The four endpoints, and the one decision in them that diverges from ADR 001.
//!
//! # `POST /webhook` does no network I/O
//!
//! ADR 001 D2: the handler classifies, writes rows, and returns `200`. Everything else is
//! the outbox's problem. The durable write happens *before* the ack, so nothing is lost;
//! the ack happens *before* any Slack call, so nothing blocks. Target p99 is 50 ms, and
//! nothing in this module can exceed it except the store.
//!
//! # `/readyz` checks the store and **not** Slack auth — a divergence from D11
//!
//! ADR 001 D11 says readiness checks "store reachability and Slack auth validity". The
//! second half is not implemented here, deliberately.
//!
//! Readiness controls whether this pod receives webhooks. If the bot token is broken, the
//! correct behaviour is to **accept** the webhook, persist it, and retry — that is what the
//! outbox is *for*. Going unready makes Alertmanager's POST fail; it retries a few times,
//! gives up, and **the alert is lost**. That is silence, which AGENTS.md names as the one
//! failure mode this project does not accept.
//!
//! It is worse with replicas. Every pod shares one token, so a revocation flips them *all*
//! unready simultaneously, and a condition the outbox was designed to ride out becomes a
//! total refusal to ingest. There is no healthy pod to route to, so shedding traffic fixes
//! nothing.
//!
//! Token validity is still watched — see [`crate::worker::auth_probe_loop`], which feeds
//! `alertthread_slack_auth_valid`. Operators alert on the metric.
//!
//! The store is different and *does* belong here: if the store is unreachable the relay
//! cannot durably accept a webhook, so a `200` would acknowledge an alert it cannot
//! persist.
//!
//! `/healthz` stays process-alive only, exactly per D11 — deliberately no store check, so a
//! brief database blip does not make Kubernetes restart a pod that is correctly buffering.

use std::sync::Arc;

use alertthread_core::{
    AlertBatch, ChannelId, Fingerprint, Notice, Plan, Policy, WebhookPayload, plan,
};
use alertthread_store::StateStore;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use serde::Deserialize;

use crate::config::Config;
use crate::metrics::Metrics;

/// Everything the handlers share.
pub struct AppState<S: StateStore> {
    /// Correlation state and the outbox.
    pub store: Arc<S>,
    /// The metric registry.
    pub metrics: Arc<Metrics>,
    /// The planner's configuration, resolved once at startup.
    pub policy: Policy,
    /// Where to post when the URL carries no `?channel=` (ADR 001 D8).
    pub default_channel: Option<ChannelId>,
}

impl<S: StateStore> AppState<S> {
    /// Builds the shared state from a validated configuration.
    #[must_use]
    pub fn new(store: Arc<S>, metrics: Arc<Metrics>, config: &Config) -> Self {
        Self {
            store,
            metrics,
            policy: config.policy(),
            default_channel: config.default_channel(),
        }
    }
}

/// The `?channel=` parameter of ADR 001 D8.
#[derive(Debug, Deserialize)]
pub struct WebhookQuery {
    /// The channel Alertmanager's receiver named. `#alerts` arrives percent-encoded as
    /// `%23alerts`, which axum decodes for us.
    #[serde(default)]
    pub channel: Option<String>,
}

/// The router, with every endpoint ADR 001 D11 specifies.
///
/// The state is `Arc`-shared rather than cloned per request: `AppState` holds the store,
/// and cloning a connection pool per webhook would be the one avoidable allocation on a
/// path with a 50 ms budget.
pub fn router<S: StateStore + 'static>(state: Arc<AppState<S>>) -> Router {
    Router::new()
        .route("/webhook", post(webhook::<S>))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz::<S>))
        .route("/metrics", get(metrics::<S>))
        .with_state(state)
}

/// `POST /webhook` — ADR 001 D2's ingest handler.
///
/// The body is taken as `String` rather than through axum's `Json` extractor so that a body
/// which will not parse produces *this* module's `400` with a log line naming the reason,
/// rather than axum's — which says "Failed to deserialize the JSON body" and nothing about
/// which alert, from which sender, was lost.
async fn webhook<S: StateStore>(
    State(state): State<Arc<AppState<S>>>,
    Query(query): Query<WebhookQuery>,
    body: String,
) -> Response {
    let now = Utc::now();

    let Some(channel) = resolve_channel(query.channel.as_deref(), state.default_channel.as_ref())
    else {
        // Config validation refuses to start without a default, so reaching this means a
        // `Config` was assembled by hand. `500`, not `400`: nothing is wrong with the
        // request, and Alertmanager retrying a `500` is the outcome that loses the least.
        state.metrics.webhook("misconfigured");
        tracing::error!(
            "a webhook arrived with no ?channel= and no slack.default_channel configured; \
             ADR 001 D8 requires one of the two"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "no channel: set ?channel= or slack.default_channel\n",
        )
            .into_response();
    };

    let payload: WebhookPayload = match serde_json::from_str(&body) {
        Ok(payload) => payload,
        Err(error) => {
            // The only path in this relay that answers a delivery with anything other than
            // a `200` or a `503`, and the only one where an alert can be lost by design. A
            // retry cannot fix an unparseable body, so it is counted and logged loudly
            // rather than retried for ever.
            state.metrics.webhook("rejected");
            tracing::error!(
                %error,
                bytes = body.len(),
                "rejected a webhook body this build cannot parse; the alerts in it are lost"
            );
            return (
                StatusCode::BAD_REQUEST,
                format!("could not parse the Alertmanager payload: {error}\n"),
            )
                .into_response();
        }
    };

    for alert in &payload.alerts {
        state.metrics.alert_received(&alert.status);
    }

    let batch = AlertBatch::from_webhook(payload, channel);
    let policy = state.policy.clone();

    // The whole of ADR 001 D2's ingest, in one transaction: claim, plan, enqueue, commit.
    // `plan` is the closure because steps either side of it are the store's and it must
    // run between them — see `StateStore::ingest`.
    let result = state
        .store
        .ingest(&batch, now, |outcomes, group| {
            plan(outcomes, &batch, group, &policy, now)
        })
        .await;

    match result {
        Ok(planned) => {
            report(&state.metrics, &batch, &planned);
            state.metrics.webhook("accepted");
            (StatusCode::OK, "ok\n").into_response()
        }
        Err(error) => {
            // ADR 001 D9's one row where refusing the request is correct: Alertmanager's
            // own retry is more durable than anything the relay could do with an
            // unreachable store, and a `200` here would acknowledge an alert nothing has
            // persisted.
            state.metrics.webhook("store_unavailable");
            tracing::error!(
                %error,
                channel = batch.channel.as_str(),
                alerts = batch.alerts.len(),
                "could not persist a delivery; answering 503 so Alertmanager redelivers"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "could not persist the delivery; retry\n",
            )
                .into_response()
        }
    }
}

/// Logs and counts everything the planner reported about a delivery.
fn report(metrics: &Metrics, batch: &AlertBatch, planned: &Plan) {
    metrics.observe(&planned.notices);

    for notice in &planned.notices {
        match notice {
            Notice::AlertsTruncated { count } => tracing::error!(
                count,
                channel = batch.channel.as_str(),
                "Alertmanager truncated alerts out of this webhook body: max_alerts must be \
                 0 (ADR 001 D8). The dropped alerts are untracked, so their resolutions will \
                 arrive as orphans"
            ),
            Notice::EmptyBatch => tracing::warn!(
                channel = batch.channel.as_str(),
                "a webhook delivery carried no alerts at all; Alertmanager does not do this"
            ),
            Notice::UnknownStatus {
                fingerprint,
                status,
            } => tracing::warn!(
                %fingerprint,
                status,
                "an alert carried a status that is neither firing nor resolved; treating it \
                 as firing"
            ),
            Notice::OrphanResolve { fingerprint } => tracing::warn!(
                %fingerprint,
                channel = batch.channel.as_str(),
                "a resolution arrived for an alert this relay has no record of; posting a \
                 standalone message (ADR 001 D9)"
            ),
            Notice::StormCollapsed { group_key, members } => tracing::info!(
                %group_key,
                members,
                channel = batch.channel.as_str(),
                "collapsing a storm into a threaded summary (ADR 001 D5)"
            ),
            // A shell bug, and the specific one this project cannot tolerate: an alert
            // that arrived and produced no op is silent. The core cannot repair it; this
            // is the loudest thing the shell can do about it.
            Notice::OutcomeCountMismatch { alerts, outcomes } => tracing::error!(
                alerts,
                outcomes,
                channel = batch.channel.as_str(),
                "the store produced a different number of claim outcomes than the batch had \
                 alerts; an alert may have gone unplanned"
            ),
        }
    }

    tracing::debug!(
        channel = batch.channel.as_str(),
        alerts = batch.alerts.len(),
        ops = planned.ops.len(),
        "accepted a delivery"
    );
}

/// ADR 001 D8's resolution order: `?channel=` → `slack.default_channel` → nothing.
///
/// A blank or whitespace-only parameter is treated as absent rather than as a channel: an
/// Alertmanager receiver rendered from a template with an unset variable produces
/// `?channel=`, and posting to a channel named `""` fails at Slack with
/// `channel_not_found` — terminal, which is a dead-lettered alert.
#[must_use]
pub fn resolve_channel(query: Option<&str>, default: Option<&ChannelId>) -> Option<ChannelId> {
    query
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
        .map(ChannelId::new)
        .or_else(|| default.cloned())
}

/// `GET /healthz` — liveness. Process-alive only, per ADR 001 D11.
///
/// Deliberately does **not** touch the store. A brief database blip must not cause
/// Kubernetes to restart a pod that is correctly buffering; the outbox is exactly the
/// machinery for riding that out, and restarting the process throws away the in-flight
/// leases that machinery depends on.
async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

/// `GET /readyz` — readiness. Store reachability only.
///
/// See this module's documentation for why Slack auth is not checked here, contrary to
/// ADR 001 D11.
async fn readyz<S: StateStore>(State(state): State<Arc<AppState<S>>>) -> Response {
    // A primary-key lookup for a fingerprint nothing will ever have. It is the cheapest
    // query that still proves the whole path works — a connection, the `alert_message`
    // table, and the row decoder — where a bare `SELECT 1` would prove only the connection
    // and would keep answering after a failed migration.
    let probe = state
        .store
        .alert(
            &Fingerprint::new(READINESS_PROBE),
            &ChannelId::new(READINESS_PROBE),
        )
        .await;

    match probe {
        Ok(_) => (StatusCode::OK, "ready\n").into_response(),
        Err(error) => {
            tracing::warn!(%error, "readiness probe could not reach the store");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the state store is not reachable\n",
            )
                .into_response()
        }
    }
}

/// The `(fingerprint, channel)` the readiness probe looks up.
///
/// Neither is a value Alertmanager can produce, so the probe can never collide with a real
/// alert's row — which matters because a probe that happened to read somebody's alert would
/// be a probe whose cost varied with what was firing.
const READINESS_PROBE: &str = "__alertthread_readiness_probe__";

/// `GET /metrics` — Prometheus text format.
///
/// Serves the registry and nothing else. The store gauges in it were sampled by
/// [`crate::worker::sample_loop`]; querying the database from here would make a 15-second
/// scrape across N replicas into a load generator pointed at the outbox, and a slow store
/// would time the scrape out and lose every other metric with it.
async fn metrics<S: StateStore>(State(state): State<Arc<AppState<S>>>) -> Response {
    match state.metrics.render() {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(PROMETHEUS_CONTENT_TYPE),
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "could not encode the metrics registry");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not encode metrics\n",
            )
                .into_response()
        }
    }
}

/// The content type the Prometheus text exposition format is served with.
const PROMETHEUS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::{PROMETHEUS_CONTENT_TYPE, resolve_channel};
    use alertthread_core::ChannelId;

    #[test]
    fn the_query_parameter_wins_over_the_configured_default() {
        // ADR 001 D8: Alertmanager keeps owning routing, and the receiver that sent this
        // delivery said where it goes.
        let default = ChannelId::new("#alerts");
        assert_eq!(
            resolve_channel(Some("#alerts-critical"), Some(&default)),
            Some(ChannelId::new("#alerts-critical"))
        );
    }

    #[test]
    fn the_default_is_used_when_the_url_names_no_channel() {
        let default = ChannelId::new("#alerts");
        assert_eq!(resolve_channel(None, Some(&default)), Some(default));
    }

    #[test]
    fn a_blank_channel_parameter_is_treated_as_absent() {
        // A receiver URL rendered from a template with an unset variable produces
        // `?channel=`. Taking it literally would post to a channel named "", which Slack
        // answers with `channel_not_found` — terminal, which is a dead-lettered alert.
        let default = ChannelId::new("#alerts");
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                resolve_channel(Some(blank), Some(&default)),
                Some(default.clone()),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn a_channel_parameter_is_trimmed_but_otherwise_kept_verbatim() {
        // ADR 001 D8 keeps whatever the parameter said: Slack accepts `#name` and `C…`
        // alike, and normalising here would only add a way to be wrong.
        assert_eq!(
            resolve_channel(Some(" C01234567 "), None),
            Some(ChannelId::new("C01234567"))
        );
        assert_eq!(
            resolve_channel(Some("#alerts"), None),
            Some(ChannelId::new("#alerts"))
        );
    }

    #[test]
    fn no_channel_anywhere_resolves_to_nothing() {
        // The third step of D8's order. Config validation makes this unreachable in a
        // process that started, and the handler answers 500 rather than guessing.
        assert_eq!(resolve_channel(None, None), None);
        assert_eq!(resolve_channel(Some(""), None), None);
    }

    #[test]
    fn metrics_are_served_as_openmetrics_text() {
        // Prometheus negotiates on this; serving `text/plain` would still scrape but would
        // silently drop the `_created` series that the OpenMetrics encoder emits.
        assert!(PROMETHEUS_CONTENT_TYPE.contains("openmetrics-text"));
        assert!(PROMETHEUS_CONTENT_TYPE.contains("charset=utf-8"));
    }
}
