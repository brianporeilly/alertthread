//! Golden Alertmanager payloads, parsed and planned.
//!
//! The fixtures in `tests/fixtures/` are webhook bodies in the shape Alertmanager v0.2x
//! actually sends: field names and spellings taken from its `notify/webhook.Message` and
//! `template.Data`, alert content modelled on the `kube-prometheus-stack` rules and the
//! `groupBy: [alertname, job]` routing this relay was built for.
//!
//! They are `include_str!`d rather than read at runtime. This crate does no filesystem
//! I/O, and a test that quietly introduced some would be a test that stopped proving the
//! crate is pure.
//!
//! The unit tests in `src/` cover `plan`'s branches. What these cover is the seam between
//! the wire format and those branches — that the bytes Alertmanager sends reach the right
//! decision, which is the part no amount of internal testing can establish.

use chrono::{DateTime, TimeDelta, Utc};

use alertthread_core::{
    AlertBatch, AlertStatus, ChannelId, ClaimOutcome, ClaimResult, Fingerprint, GroupKey, Intent,
    Notice, Op, Placement, Policy, WebhookPayload, plan,
};

const FIRING: &str = include_str!("fixtures/firing.json");
const RESOLVED: &str = include_str!("fixtures/resolved.json");
const GROUPED: &str = include_str!("fixtures/grouped.json");
const MIXED: &str = include_str!("fixtures/mixed.json");
const EMPTY: &str = include_str!("fixtures/empty.json");
const TRUNCATED: &str = include_str!("fixtures/truncated.json");

const OSD_FINGERPRINT: &str = "a1b2c3d4e5f60718";
const CEPH_GROUP: &str =
    "{}/{severity=\"critical\"}:{alertname=\"CephOSDDown\", job=\"rook-ceph-mgr\"}";
const PODS_GROUP: &str =
    "{}/{severity=\"warning\"}:{alertname=\"KubePodNotReady\", job=\"kube-state-metrics\"}";

// `allow-unwrap-in-tests` in clippy.toml reaches `#[test]` functions but not helpers
// called from them, and an integration-test target has no `#[cfg(test)]` module to hang
// the exemption on. Both of these are test scaffolding over a compile-time constant: if
// the fixture stops parsing, panicking is the entire point.
#[expect(
    clippy::unwrap_used,
    reason = "test helper over an include_str! constant"
)]
fn parse(json: &str) -> WebhookPayload {
    serde_json::from_str(json).unwrap()
}

#[expect(clippy::unwrap_used, reason = "test helper over a literal timestamp")]
fn now() -> DateTime<Utc> {
    "2026-07-21T15:00:00Z".parse().unwrap()
}

fn batch(json: &str) -> AlertBatch {
    AlertBatch::from_webhook(parse(json), ChannelId::new("#alerts"))
}

/// Every alert in the batch claimed successfully — a first delivery with no prior state.
fn all_claimed(batch: &AlertBatch) -> Vec<ClaimOutcome> {
    batch
        .alerts
        .iter()
        .map(|alert| ClaimOutcome::new(alert.clone(), ClaimResult::Claimed))
        .collect()
}

#[test]
fn a_single_firing_alert_parses_into_something_plannable() {
    let payload = parse(FIRING);

    assert_eq!(payload.version, "4");
    assert_eq!(payload.status, AlertStatus::Firing);
    assert_eq!(payload.group_key, GroupKey::new(CEPH_GROUP));
    assert_eq!(payload.truncated_alerts, 0);
    assert_eq!(payload.receiver, "alertthread");
    assert_eq!(
        payload.external_url,
        "http://alertmanager.observability.svc:9093"
    );
    assert_eq!(payload.alerts.len(), 1);

    let alert = &payload.alerts[0];
    assert_eq!(alert.status, AlertStatus::Firing);
    assert_eq!(alert.status.intent(), Intent::Firing);
    assert_eq!(alert.fingerprint, Fingerprint::new(OSD_FINGERPRINT));
    assert_eq!(alert.labels["alertname"], "CephOSDDown");
    assert_eq!(alert.labels["ceph_daemon"], "osd.3");
    assert_eq!(alert.annotations["summary"], "An OSD has been marked down");
    assert_eq!(
        alert.starts_at.to_rfc3339(),
        "2026-07-21T14:02:11.283+00:00"
    );
    assert!(
        alert.generator_url.starts_with("http://prometheus"),
        "generatorURL is spelled with an uppercase URL on the wire"
    );

    // A firing alert carries the zero time in `endsAt`, not a missing field.
    assert_eq!(alert.ends_at.to_rfc3339(), "0001-01-01T00:00:00+00:00");
}

#[test]
fn the_resolved_payload_carries_the_same_fingerprint_as_the_firing_one() {
    // This is the entire premise of the project stated as an assertion. The message text,
    // the timestamps and the batch status all differ between these two payloads; the
    // fingerprint does not, which is why it and not the text is what correlation keys on.
    let firing = parse(FIRING);
    let resolved = parse(RESOLVED);

    assert_ne!(firing.status, resolved.status);
    assert_eq!(
        firing.alerts[0].fingerprint, resolved.alerts[0].fingerprint,
        "fingerprint must be stable across the firing to resolved lifecycle"
    );
    assert_eq!(
        firing.alerts[0].starts_at, resolved.alerts[0].starts_at,
        "startsAt is preserved, which is what makes the fired-to-resolved duration reportable"
    );
}

#[test]
fn a_resolved_alert_carries_a_real_end_time() {
    let payload = parse(RESOLVED);
    let alert = &payload.alerts[0];

    assert_eq!(alert.status, AlertStatus::Resolved);
    assert_eq!(alert.status.intent(), Intent::Resolved);
    assert_eq!(alert.ends_at.to_rfc3339(), "2026-07-21T14:31:41.283+00:00");
    assert_eq!(
        alert.ends_at - alert.starts_at,
        TimeDelta::seconds(29 * 60 + 30)
    );
}

#[test]
fn a_resolved_alert_the_relay_never_saw_still_posts_something() {
    // PRD §5.5 driven straight off the wire: the relay was restarted, or was down when
    // this alert fired, so the claim finds no row.
    let batch = batch(RESOLVED);
    let outcomes: Vec<_> = batch
        .alerts
        .iter()
        .map(|alert| ClaimOutcome::new(alert.clone(), ClaimResult::Orphan))
        .collect();

    let result = plan(&outcomes, &batch, None, &Policy::default(), now());

    assert_eq!(
        result.ops,
        vec![Op::PostOrphanResolved {
            fingerprint: Fingerprint::new(OSD_FINGERPRINT),
            channel: ChannelId::new("#alerts"),
        }]
    );
    assert_eq!(
        result.notices,
        vec![Notice::OrphanResolve {
            fingerprint: Fingerprint::new(OSD_FINGERPRINT),
        }]
    );
}

#[test]
fn a_real_grouped_payload_collapses_into_a_thread() {
    // The case ADR 001 §1 fact (2) is about: one `KubePodNotReady` group carrying seven
    // alerts, which naive per-fingerprint messaging would turn into seven top-level
    // messages where Alertmanager posts one today.
    let batch = batch(GROUPED);
    assert_eq!(batch.alerts.len(), 7);
    assert_eq!(batch.group_key, GroupKey::new(PODS_GROUP));
    assert!(
        batch
            .alerts
            .iter()
            .all(|a| a.status == AlertStatus::Firing && a.labels["alertname"] == "KubePodNotReady")
    );

    let result = plan(
        &all_claimed(&batch),
        &batch,
        None,
        &Policy::default(),
        now(),
    );

    assert_eq!(result.ops.len(), 8, "one parent and seven children");
    assert_eq!(
        result.ops[0],
        Op::PostGroup {
            group_key: GroupKey::new(PODS_GROUP),
            channel: ChannelId::new("#alerts"),
            initial_members: 7,
        }
    );
    assert!(
        result.ops[1..].iter().all(|op| matches!(
            op,
            Op::Post {
                placement: Placement::Thread {
                    parent_ts: None,
                    ..
                },
                ..
            }
        )),
        "{:?}",
        result.ops
    );
    assert_eq!(
        result.notices,
        vec![Notice::StormCollapsed {
            group_key: GroupKey::new(PODS_GROUP),
            members: 7,
        }]
    );
}

#[test]
fn every_fingerprint_in_the_grouped_payload_is_distinct() {
    // If Alertmanager reused a fingerprint across pods, per-fingerprint correlation would
    // silently collapse two alerts into one message. It does not — the fingerprint is
    // derived from the full label set, and `pod` is in it.
    let payload = parse(GROUPED);
    let mut fingerprints: Vec<_> = payload
        .alerts
        .iter()
        .map(|a| a.fingerprint.as_str())
        .collect();
    fingerprints.sort_unstable();
    let distinct = fingerprints.len();
    fingerprints.dedup();
    assert_eq!(fingerprints.len(), distinct);
}

#[test]
fn a_mixed_batch_is_classified_per_alert_not_per_batch() {
    // The batch-level `status` says "firing" while one of its alerts is resolved. Reading
    // the batch status instead of each alert's own would leave that pod red forever.
    let payload = parse(MIXED);
    assert_eq!(payload.status, AlertStatus::Firing);
    assert_eq!(payload.notification_reason.as_deref(), Some("update"));

    let intents: Vec<_> = payload.alerts.iter().map(|a| a.status.intent()).collect();
    assert_eq!(
        intents,
        vec![Intent::Resolved, Intent::Firing, Intent::Firing]
    );
}

#[test]
fn a_mixed_batch_plans_a_resolve_and_two_posts() {
    let batch = batch(MIXED);
    let outcomes = vec![
        ClaimOutcome::new(
            batch.alerts[0].clone(),
            ClaimResult::Resolving {
                message_ts: Some(alertthread_core::MessageTs::new("1721557000.000100")),
                thread_parent_ts: None,
            },
        ),
        ClaimOutcome::new(batch.alerts[1].clone(), ClaimResult::Claimed),
        ClaimOutcome::new(batch.alerts[2].clone(), ClaimResult::Claimed),
    ];

    let result = plan(&outcomes, &batch, None, &Policy::default(), now());

    assert_eq!(result.ops.len(), 3);
    assert!(matches!(result.ops[0], Op::Resolve { .. }));
    assert!(matches!(
        result.ops[1],
        Op::Post {
            placement: Placement::Channel,
            ..
        }
    ));
    assert!(matches!(result.ops[2], Op::Post { .. }));
    assert!(result.notices.is_empty(), "{:?}", result.notices);
}

#[test]
fn an_empty_batch_parses_and_plans_to_nothing_but_a_notice() {
    let batch = batch(EMPTY);
    assert!(batch.alerts.is_empty());

    let result = plan(&[], &batch, None, &Policy::default(), now());

    assert!(result.ops.is_empty(), "{:?}", result.ops);
    assert_eq!(result.notices, vec![Notice::EmptyBatch]);
}

#[test]
fn a_truncated_payload_is_detected_from_the_field_alertmanager_sets() {
    // ADR 001 D8's footgun, caught at the moment it happens. Alertmanager's own
    // `truncateAlerts` sets `truncatedAlerts` to `len(alerts) - max_alerts`, so this
    // fixture is a seven-alert group delivered under `max_alerts: 3`.
    //
    // The four alerts that were dropped are never tracked, and their `resolved`
    // notifications will arrive later as orphans with nothing to correlate to. Without
    // this field the only symptom is that degraded correlation, which points nowhere near
    // a config setting on the sending side.
    let batch = batch(TRUNCATED);
    assert_eq!(batch.truncated_alerts, 4);
    assert_eq!(batch.alerts.len(), 3);

    let result = plan(
        &all_claimed(&batch),
        &batch,
        None,
        &Policy::default(),
        now(),
    );

    assert_eq!(result.notices, vec![Notice::AlertsTruncated { count: 4 }]);
    assert_eq!(
        result.ops.len(),
        3,
        "the alerts that did arrive are still delivered — truncation is reported, not fatal"
    );
}

#[test]
fn the_untruncated_fixtures_report_no_truncation() {
    for json in [FIRING, RESOLVED, GROUPED, MIXED, EMPTY] {
        let batch = batch(json);
        let result = plan(
            &all_claimed(&batch),
            &batch,
            None,
            &Policy::default(),
            now(),
        );
        assert!(
            !result
                .notices
                .iter()
                .any(|n| matches!(n, Notice::AlertsTruncated { .. })),
            "{json}"
        );
    }
}

#[test]
fn every_fixture_round_trips_through_serde() {
    // Serialisation is not idle: Phase 2 stores these alerts' labels and annotations as
    // JSON, and Phase 3 renders from them. A field that parses but does not survive a
    // round trip would lose data somewhere between the two.
    for json in [FIRING, RESOLVED, GROUPED, MIXED, EMPTY, TRUNCATED] {
        let original = parse(json);
        let reparsed: WebhookPayload =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(reparsed, original, "{json}");
    }
}

#[test]
fn every_fixture_carries_a_group_key_and_a_fingerprint_for_each_alert() {
    // Both are load-bearing: the group key drives storm collapse, the fingerprint drives
    // correlation. A payload missing either is one this relay cannot do its job with.
    for json in [FIRING, RESOLVED, GROUPED, MIXED, EMPTY, TRUNCATED] {
        let payload = parse(json);
        assert!(!payload.group_key.as_str().is_empty(), "{json}");
        for alert in &payload.alerts {
            assert!(!alert.fingerprint.as_str().is_empty(), "{json}");
        }
    }
}
