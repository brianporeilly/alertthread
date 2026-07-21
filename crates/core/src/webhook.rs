//! The Alertmanager webhook payload, version 4.
//!
//! These types mirror Alertmanager's own `notify/webhook.Message` and `template.Data`
//! structures rather than a remembered approximation of them. The field set and its JSON
//! spelling were taken from Alertmanager's source and its configuration documentation:
//! `Message` embeds `*template.Data` and adds `version`, `groupKey` and — the field that
//! matters most to this project — `truncatedAlerts`.
//!
//! ## Two deliberate decisions about how strictly this parses
//!
//! **Unknown fields are ignored, not rejected.** There is no `deny_unknown_fields` here
//! and there must not be. Alertmanager has added fields to this payload before
//! (`notification_reason`, `routeLabels`) and will again; a relay that returns `400` when
//! the sender adds a field turns an upgrade into an outage, and in this project an outage
//! means silence. Serde's default of ignoring what it does not recognise is the correct
//! behaviour.
//!
//! **Optional-looking fields carry `#[serde(default)]`.** Everything except each alert's
//! identity and timestamps tolerates being absent, for the same reason. What is *not*
//! defaulted is `fingerprint`, `startsAt` and `endsAt` — an alert without a fingerprint
//! cannot be correlated at all, so accepting one would create the illusion of tracking
//! rather than the fact of it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{Fingerprint, GroupKey};

/// Alertmanager's label and annotation maps.
///
/// A `BTreeMap` rather than a `HashMap` so iteration order is stable, which keeps the
/// Block Kit snapshots of Phase 3 from flapping between runs.
pub type LabelMap = BTreeMap<String, String>;

/// The `status` field Alertmanager sets on a batch and on each alert within it.
///
/// Alertmanager only ever sends `firing` or `resolved`. [`AlertStatus::Unknown`] exists
/// because the relay does not get to assume that: a proxy, a replay tool, or a future
/// Alertmanager could put something else there, and rejecting the whole batch over it
/// would drop every alert in it. Keeping the raw string means the value survives into a
/// log line instead of being flattened into "not firing".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum AlertStatus {
    /// The alert is active.
    Firing,
    /// The alert has stopped firing.
    Resolved,
    /// Something the relay does not recognise, preserved verbatim.
    Unknown(String),
}

impl AlertStatus {
    /// The status as it appeared on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Firing => "firing",
            Self::Resolved => "resolved",
            Self::Unknown(raw) => raw.as_str(),
        }
    }

    /// What the relay should do about an alert carrying this status.
    ///
    /// An unrecognised status resolves to [`Intent::Firing`]. That direction is chosen,
    /// not incidental. Treating it as firing posts a visible message and starts tracking
    /// the fingerprint, so a later `resolved` for it still correlates; treating it as
    /// resolved would do neither, and would additionally risk an orphan-resolve message
    /// for an alert nobody was ever told about. Both choices are wrong in some sense —
    /// this one is wrong in the direction of noise rather than silence, which is the
    /// direction every trade-off in this codebase resolves (AGENTS.md).
    pub fn intent(&self) -> Intent {
        match self {
            Self::Resolved => Intent::Resolved,
            Self::Firing | Self::Unknown(_) => Intent::Firing,
        }
    }
}

impl From<String> for AlertStatus {
    fn from(raw: String) -> Self {
        if raw == "firing" {
            Self::Firing
        } else if raw == "resolved" {
            Self::Resolved
        } else {
            Self::Unknown(raw)
        }
    }
}

impl From<AlertStatus> for String {
    fn from(status: AlertStatus) -> Self {
        match status {
            AlertStatus::Firing => "firing".to_owned(),
            AlertStatus::Resolved => "resolved".to_owned(),
            AlertStatus::Unknown(raw) => raw,
        }
    }
}

/// Which store operation an alert's status calls for.
///
/// The shell needs this *before* [`plan`](crate::plan) runs, because the atomic claim
/// (ADR 001 D3) happens first and the two statuses take different SQL paths. It lives in
/// the core anyway: "given this alert, what should we do?" is a decision, and decisions do
/// not belong in handlers (AGENTS.md rule 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Attempt the `INSERT ... ON CONFLICT DO NOTHING` claim.
    Firing,
    /// Attempt the `UPDATE ... SET state = 'resolving'` transition.
    Resolved,
}

/// One entry of the webhook body's `alerts` array.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookAlert {
    /// `firing` or `resolved`.
    pub status: AlertStatus,
    /// The alert's full label set, including `alertname` and `severity`.
    #[serde(default)]
    pub labels: LabelMap,
    /// The alert's annotations, typically `summary` and `description`.
    #[serde(default)]
    pub annotations: LabelMap,
    /// When the alert started firing.
    pub starts_at: DateTime<Utc>,
    /// When the alert stopped firing.
    ///
    /// For an alert that is still firing Alertmanager sends the zero time,
    /// `0001-01-01T00:00:00Z`, rather than omitting the field.
    pub ends_at: DateTime<Utc>,
    /// A link back to the rule that produced the alert.
    ///
    /// Spelled `generatorURL` on the wire, which `rename_all = "camelCase"` would
    /// otherwise render as `generatorUrl`.
    #[serde(rename = "generatorURL", default)]
    pub generator_url: String,
    /// Alertmanager's stable identity for this alert.
    pub fingerprint: Fingerprint,
}

/// The body Alertmanager `POST`s to a `webhook_config` receiver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookPayload {
    /// Payload schema version. Alertmanager has sent `"4"` for its entire v0.x line.
    #[serde(default)]
    pub version: String,
    /// The key identifying the group of alerts this delivery represents.
    pub group_key: GroupKey,
    /// How many alerts Alertmanager dropped from `alerts` because of `max_alerts`.
    ///
    /// **This is the highest-value field in the payload for operating the relay**, and
    /// modelling it is a requirement rather than completeness. ADR 001 D8 records that a
    /// non-zero `max_alerts` silently truncates alerts out of the body, that the
    /// truncated alerts are therefore never tracked, and that their eventual `resolved`
    /// notifications arrive as orphans — a symptom that "points nowhere near the cause".
    ///
    /// Alertmanager tells us directly. `truncateAlerts` sets this to
    /// `len(alerts) - max_alerts` whenever it trims the array, so a non-zero value *is*
    /// the misconfiguration, observed at the moment it happens rather than inferred later
    /// from a rising orphan-resolve counter. [`plan`](crate::plan) turns it into
    /// [`Notice::AlertsTruncated`](crate::Notice::AlertsTruncated) so the shell can log
    /// and count it in Phase 4.
    #[serde(default)]
    pub truncated_alerts: u64,
    /// The status of the group as a whole.
    pub status: AlertStatus,
    /// The name of the Alertmanager receiver that produced this delivery.
    #[serde(default)]
    pub receiver: String,
    /// The labels Alertmanager grouped on, per its `group_by`.
    #[serde(default)]
    pub group_labels: LabelMap,
    /// Labels common to every alert in the group.
    #[serde(default)]
    pub common_labels: LabelMap,
    /// Annotations common to every alert in the group.
    #[serde(default)]
    pub common_annotations: LabelMap,
    /// A backlink to the sending Alertmanager.
    #[serde(rename = "externalURL", default)]
    pub external_url: String,
    /// Why Alertmanager sent this notification, when it says.
    ///
    /// Spelled `notification_reason` on the wire — `snake_case`, unlike its neighbours.
    #[serde(rename = "notification_reason", default)]
    pub notification_reason: Option<String>,
    /// The alerts themselves, possibly trimmed — see [`truncated_alerts`].
    ///
    /// [`truncated_alerts`]: WebhookPayload::truncated_alerts
    #[serde(default)]
    pub alerts: Vec<WebhookAlert>,
}

#[cfg(test)]
mod tests {
    use super::{AlertStatus, Intent, WebhookAlert, WebhookPayload};
    use crate::ids::{Fingerprint, GroupKey};

    #[test]
    fn known_statuses_parse_to_their_variants() {
        assert_eq!(AlertStatus::from("firing".to_owned()), AlertStatus::Firing);
        assert_eq!(
            AlertStatus::from("resolved".to_owned()),
            AlertStatus::Resolved
        );
    }

    #[test]
    fn an_unrecognised_status_is_preserved_verbatim() {
        // Not flattened to "not firing": the raw value has to survive far enough to reach
        // a log line, or the operator has no way to find out what the sender is saying.
        let status = AlertStatus::from("suppressed".to_owned());
        assert_eq!(status, AlertStatus::Unknown("suppressed".to_owned()));
        assert_eq!(status.as_str(), "suppressed");
    }

    #[test]
    fn status_matching_is_exact() {
        // Case and whitespace variants are *not* the known statuses. Alertmanager sends
        // lowercase; anything else came from somewhere we do not control, and quietly
        // normalising it would hide that.
        assert_eq!(
            AlertStatus::from("Firing".to_owned()),
            AlertStatus::Unknown("Firing".to_owned())
        );
        assert_eq!(
            AlertStatus::from("resolved ".to_owned()),
            AlertStatus::Unknown("resolved ".to_owned())
        );
    }

    #[test]
    fn as_str_reproduces_the_wire_spelling() {
        assert_eq!(AlertStatus::Firing.as_str(), "firing");
        assert_eq!(AlertStatus::Resolved.as_str(), "resolved");
        assert_eq!(AlertStatus::Unknown("weird".to_owned()).as_str(), "weird");
    }

    #[test]
    fn status_serialises_back_to_a_plain_string() {
        assert_eq!(
            serde_json::to_string(&AlertStatus::Firing).unwrap(),
            "\"firing\""
        );
        assert_eq!(
            serde_json::to_string(&AlertStatus::Resolved).unwrap(),
            "\"resolved\""
        );
        assert_eq!(
            serde_json::to_string(&AlertStatus::Unknown("weird".to_owned())).unwrap(),
            "\"weird\""
        );
    }

    #[test]
    fn status_debug_names_the_variant() {
        assert_eq!(format!("{:?}", AlertStatus::Firing), "Firing");
        assert_eq!(format!("{:?}", AlertStatus::Resolved), "Resolved");
        assert_eq!(
            format!("{:?}", AlertStatus::Unknown("weird".to_owned())),
            "Unknown(\"weird\")"
        );
    }

    #[test]
    fn firing_and_resolved_map_to_their_obvious_intents() {
        assert_eq!(AlertStatus::Firing.intent(), Intent::Firing);
        assert_eq!(AlertStatus::Resolved.intent(), Intent::Resolved);
    }

    #[test]
    fn an_unknown_status_is_treated_as_firing() {
        // Deliberate: firing is the treatment that both posts a visible message and
        // starts tracking the fingerprint, so a later genuine `resolved` still correlates.
        assert_eq!(
            AlertStatus::Unknown("suppressed".to_owned()).intent(),
            Intent::Firing
        );
    }

    #[test]
    fn intent_debug_names_the_variant() {
        assert_eq!(format!("{:?}", Intent::Firing), "Firing");
        assert_eq!(format!("{:?}", Intent::Resolved), "Resolved");
    }

    #[test]
    fn a_minimal_payload_parses_with_every_optional_field_defaulted() {
        let payload: WebhookPayload =
            serde_json::from_str(r#"{"groupKey":"g","status":"firing"}"#).unwrap();

        assert_eq!(payload.group_key, GroupKey::new("g"));
        assert_eq!(payload.status, AlertStatus::Firing);
        assert_eq!(payload.version, "");
        assert_eq!(payload.truncated_alerts, 0);
        assert_eq!(payload.receiver, "");
        assert_eq!(payload.external_url, "");
        assert_eq!(payload.notification_reason, None);
        assert!(payload.group_labels.is_empty());
        assert!(payload.common_labels.is_empty());
        assert!(payload.common_annotations.is_empty());
        assert!(payload.alerts.is_empty());
    }

    #[test]
    fn an_unrecognised_top_level_field_does_not_fail_the_parse() {
        // Alertmanager has added fields to this payload before and will again. Returning
        // 400 because the sender learned a new word would turn an upgrade into silence.
        let payload: WebhookPayload = serde_json::from_str(
            r#"{"groupKey":"g","status":"firing","routeLabels":{"a":"b"},"somethingNew":42}"#,
        )
        .unwrap();
        assert_eq!(payload.group_key, GroupKey::new("g"));
    }

    #[test]
    fn an_unrecognised_alert_field_does_not_fail_the_parse() {
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc","futureField":true}"#,
        )
        .unwrap();
        assert_eq!(alert.fingerprint, Fingerprint::new("abc"));
    }

    #[test]
    fn an_alert_without_a_fingerprint_is_rejected() {
        // The one place strictness is correct. An unfingerprinted alert cannot be
        // correlated, so accepting it would produce the illusion of tracking rather than
        // the fact of it — and the failure would surface much later, as an orphan resolve.
        let error = serde_json::from_str::<WebhookAlert>(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z"}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("fingerprint"), "{error}");
    }

    #[test]
    fn generator_url_reads_the_uppercase_wire_spelling() {
        // camelCase would give `generatorUrl`; Alertmanager sends `generatorURL`.
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc",
                "generatorURL":"http://prometheus/graph"}"#,
        )
        .unwrap();
        assert_eq!(alert.generator_url, "http://prometheus/graph");
    }

    #[test]
    fn a_firing_alert_carries_the_zero_end_time_rather_than_omitting_it() {
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc"}"#,
        )
        .unwrap();
        assert_eq!(alert.ends_at.to_rfc3339(), "0001-01-01T00:00:00+00:00");
        assert!(alert.ends_at < alert.starts_at);
    }

    #[test]
    fn a_payload_round_trips_through_json() {
        let original: WebhookPayload = serde_json::from_str(
            r#"{"version":"4","groupKey":"g","truncatedAlerts":3,"status":"firing",
                "receiver":"relay","groupLabels":{"alertname":"X"},
                "commonLabels":{"job":"j"},"commonAnnotations":{"summary":"s"},
                "externalURL":"http://am","notification_reason":"repeat",
                "alerts":[{"status":"firing","labels":{"a":"b"},"annotations":{"c":"d"},
                "startsAt":"2026-07-21T14:02:00Z","endsAt":"0001-01-01T00:00:00Z",
                "generatorURL":"http://p","fingerprint":"abc"}]}"#,
        )
        .unwrap();

        let reparsed: WebhookPayload =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(reparsed, original);
        assert_eq!(reparsed.truncated_alerts, 3);
        assert_eq!(reparsed.notification_reason.as_deref(), Some("repeat"));
        assert_eq!(reparsed.version, "4");
    }

    #[test]
    fn payload_debug_includes_the_group_key() {
        let payload: WebhookPayload =
            serde_json::from_str(r#"{"groupKey":"g","status":"firing"}"#).unwrap();
        let rendered = format!("{payload:?}");
        assert!(rendered.contains("group_key"), "{rendered}");
        assert!(rendered.contains('g'), "{rendered}");
    }

    #[test]
    fn alert_debug_includes_the_fingerprint() {
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc"}"#,
        )
        .unwrap();
        assert!(format!("{alert:?}").contains("abc"));
    }
}
