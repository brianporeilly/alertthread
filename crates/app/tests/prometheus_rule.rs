//! The shipped alert rules only name metrics and label values this build publishes.
//!
//! `deploy/alertthread.rules.yaml` is the operator-facing half of ADR 001 D11. A rule that
//! names a metric the relay does not export is not a broken rule — it is a *silent* rule: it
//! evaluates to an empty vector for ever, fires nothing, and looks exactly like a healthy
//! relay. There is no runtime signal for it and no test in Prometheus that catches it, so it
//! is caught here, against the real registry's real exposition.
//!
//! `promtool check rules` (`just check-rules`) validates the PromQL and the YAML. It cannot
//! know what this binary exports, which is the half this file covers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

use std::collections::BTreeSet;

use alertthread::metrics::Metrics;
use alertthread_slack::{Degradation, FallbackReason, SlackError, SlackMethod, TemplateKind};
use alertthread_store::{OpKind, StoreStats};
use chrono::{DateTime, Utc};

/// The rules as shipped.
const RULES: &str = include_str!("../../../deploy/alertthread.rules.yaml");

/// The metric-name prefix ADR 001 D12 settles on.
const PREFIX: &str = "alertthread_";

/// Label names the relay itself puts on a series.
///
/// Deliberately not every label a rule mentions: `job` and `instance` are added by Prometheus
/// at scrape time and `alertname`/`severity` by the rules themselves, so none of them appears
/// in the relay's own exposition and looking for them here would fail for the wrong reason.
const RELAY_LABELS: [&str; 6] = ["outcome", "reason", "op", "method", "source", "status"];

/// A registry with every family populated, so every series it can emit is in the exposition.
///
/// A `Family` with no members emits *nothing* — not even a `# TYPE` line — so an empty
/// registry would make this test claim half the metrics do not exist. Each label value used
/// here is one the shipping code passes; `webhook` in particular is given all six outcomes,
/// including the two the perimeter added.
fn populated() -> Metrics {
    let metrics = Metrics::new();

    metrics.alert_received(&alertthread_core::AlertStatus::Firing);
    metrics.alert_received(&alertthread_core::AlertStatus::Resolved);
    metrics.alert_received(&alertthread_core::AlertStatus::Unknown("odd".to_owned()));

    for outcome in [
        "accepted",
        "rejected",
        "store_unavailable",
        "misconfigured",
        "auth_missing",
        "auth_mismatch",
    ] {
        metrics.webhook(outcome);
    }

    for method in [
        SlackMethod::PostMessage,
        SlackMethod::UpdateMessage,
        SlackMethod::AuthTest,
    ] {
        metrics.slack_ok(method, 0.1);
        metrics.rate_limited_by_slack(method);
        metrics.rate_limited_locally(method);
    }
    metrics.slack_failed(
        SlackMethod::PostMessage,
        &SlackError::MessageNotFound {
            method: SlackMethod::PostMessage,
            code: "message_not_found".to_owned(),
        },
        0.1,
    );

    for reason in [FallbackReason::RenderFailed, FallbackReason::EmptyOutput] {
        metrics.degraded(&Degradation {
            template: TemplateKind::Firing,
            reason,
            detail: String::new(),
        });
    }

    metrics.dead_lettered("invalid_auth");
    metrics.dead_letters_revived(1);
    metrics.observe(&[
        alertthread_core::Notice::OrphanResolve {
            fingerprint: alertthread_core::Fingerprint::new("abc"),
        },
        alertthread_core::Notice::AlertsTruncated { count: 1 },
        alertthread_core::Notice::StormCollapsed {
            group_key: alertthread_core::GroupKey::new("gk"),
            members: 6,
        },
    ]);
    metrics.slack_auth_valid.set(1);
    metrics.publish(
        &StoreStats {
            outbox_depth: [(OpKind::Post, 1)].into_iter().collect(),
            dead_lettered: 1,
            oldest_queued_at: Some(DateTime::<Utc>::from_timestamp(0, 0).unwrap()),
            tracked_fingerprints: 1,
        },
        DateTime::<Utc>::from_timestamp(60, 0).unwrap(),
    );

    metrics
}

/// Every series name in an exposition, exactly as Prometheus would see it.
///
/// Taken from the sample lines rather than from `# TYPE`, because the two differ: a counter
/// registered as `dead_letter` is typed as `alertthread_dead_letter` and sampled as
/// `alertthread_dead_letter_total`, and a rule can only use the second.
fn exposed_series(exposition: &str) -> BTreeSet<String> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let end = line.find(['{', ' ']).unwrap_or(line.len());
            Some(line[..end].to_owned()).filter(|name| name.starts_with(PREFIX))
        })
        .collect()
}

/// Every `alertthread_*` token in the rules file, from expressions and prose alike.
///
/// Prose counts on purpose: a description that names a metric is telling an operator what to
/// look at next, and one that names a metric that no longer exists sends them nowhere.
fn referenced_metrics(rules: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = rules;
    while let Some(start) = rest.find(PREFIX) {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(tail.len());
        found.insert(tail[..end].to_owned());
        rest = &tail[end..];
    }
    found
}

/// Every `label="value"` a rule selects on, including the branches of a `label=~"a|b"`.
///
/// Negated matchers (`!=`, `!~`) are skipped: excluding a value that has never been observed
/// is legitimate, so requiring it to exist would reject a correct rule.
fn referenced_label_values(rules: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for label in RELAY_LABELS {
        let mut rest = rules;
        while let Some(at) = rest.find(label) {
            let tail = &rest[at + label.len()..];
            rest = tail;

            let (raw, is_pattern) = if let Some(exact) = tail.strip_prefix("=\"") {
                (exact, false)
            } else if let Some(pattern) = tail.strip_prefix("=~\"") {
                (pattern, true)
            } else {
                continue;
            };
            let Some(end) = raw.find('"') else { continue };
            let value = &raw[..end];

            for branch in value.split('|') {
                // A branch with regex metacharacters is not a literal to look up. `.*` in a
                // matcher is a deliberate catch-all, not a value that has to exist.
                if branch.contains(['.', '*', '+', '?', '(', '[', '\\'])
                    || (is_pattern && branch.is_empty())
                {
                    continue;
                }
                found.insert(format!("{label}=\"{branch}\""));
            }
        }
    }
    found
}

#[test]
fn every_metric_the_rules_name_is_one_this_build_exports() {
    let exposition = populated().render().expect("the registry encodes");
    let exposed = exposed_series(&exposition);
    assert!(
        exposed.len() > 10,
        "the fixture registry is not populated: {exposed:?}"
    );

    let referenced = referenced_metrics(RULES);
    assert!(
        referenced.len() > 10,
        "the rules file was not read: {referenced:?}"
    );

    let missing: Vec<_> = referenced
        .iter()
        .filter(|name| !exposed.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "deploy/alertthread.rules.yaml names metrics this build does not export: {missing:?}\n\
         A rule on a metric that does not exist never fires and looks exactly like a healthy \
         relay.\nExported:\n{exposed:#?}"
    );
}

#[test]
fn every_label_value_the_rules_select_on_is_one_this_build_emits() {
    let exposition = populated().render().expect("the registry encodes");
    let referenced = referenced_label_values(RULES);
    assert!(
        referenced.contains("outcome=\"rejected\""),
        "the label scan found nothing to check: {referenced:?}"
    );

    let missing: Vec<_> = referenced
        .iter()
        .filter(|pair| !exposition.contains(pair.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "deploy/alertthread.rules.yaml selects on label values this build never emits: \
         {missing:?}\nThat rule matches nothing, for ever.\n{exposition}"
    );
}

#[test]
fn the_rules_carry_the_routing_warning_that_makes_them_safe_to_ship() {
    // ADR 001 D11 and AGENTS.md both say it: shipping the rule without the route that
    // bypasses the relay is worse than shipping no rule. The YAML travels away from the
    // documentation — into a chart, into somebody's cluster — so the warning has to travel
    // with it. This test is what stops a tidy-up deleting it.
    assert!(
        RULES.contains("CANNOT ALERT ON ITSELF"),
        "the circular-dependency warning must stay in the rules file itself"
    );
    assert!(RULES.contains("slack_configs"), "{RULES}");
    assert!(
        RULES.contains("alertname=~\"Alertthread.*\""),
        "the file has to show the matcher the bypass route uses"
    );

    // Every alert is named so that matcher catches it, whatever an aggregation does to `job`.
    let alerts: Vec<&str> = RULES
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- alert: "))
        .collect();
    assert!(alerts.len() >= 10, "{alerts:?}");
    for alert in &alerts {
        assert!(
            alert.starts_with("Alertthread"),
            "{alert} would not be routed by alertname=~\"Alertthread.*\""
        );
    }

    // And `job` survives the aggregations, so the other documented matcher works too.
    for line in RULES.lines() {
        let trimmed = line.trim();
        if let Some(expr) = trimmed.strip_prefix("expr: ") {
            let aggregates =
                expr.contains("sum(") || expr.contains("max(") || expr.contains("min(");
            assert!(
                !aggregates,
                "a bare aggregation drops the job label and unroutes the alert: {expr}"
            );
        }
    }
}
