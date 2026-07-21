//! What a template is allowed to see, and how untrusted data is made safe on the way in.
//!
//! # Everything here is escaped for Slack, at the boundary
//!
//! Labels and annotations come from arbitrary `PrometheusRule`s. Slack's `mrkdwn` treats
//! `<…>` as markup, and `<!channel>` in message text **notifies the entire channel** —
//! so a `description` annotation containing that string turns one alert into a
//! workspace-wide ping. Slack's own guidance is to escape `&`, `<` and `>` in any text
//! that came from somewhere else.
//!
//! Escaping is applied when this view is built, not inside the templates, for two
//! reasons. A template author cannot forget it; and the built-in templates' own markup
//! (`*bold*`, `<url|label>`) is written by us and is therefore *not* data, so escaping at
//! the template layer would have to distinguish the two and would get it wrong.
//!
//! Neither ADR 001 nor ADR 002 mentions this. It is recorded in the PR that introduced it.

use std::collections::BTreeMap;

use alertthread_core::{Fingerprint, GroupKey, LabelMap, WebhookAlert};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

/// One alert, as a message is going to describe it.
///
/// Built by the shell from an `alert_message` row at send time (or, for an orphan
/// resolve, straight from the webhook payload). Deliberately not the store's
/// `AlertRecord`: this crate does not depend on `alertthread-store`, and the fields a
/// message needs are a strict subset of the ones a row carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertView {
    /// Alertmanager's identity for the alert.
    pub fingerprint: Fingerprint,
    /// Its full label set.
    pub labels: LabelMap,
    /// Its annotations, typically `summary` and `description`.
    pub annotations: LabelMap,
    /// When it started firing.
    pub starts_at: DateTime<Utc>,
    /// When its resolution was accepted, if it has resolved.
    pub resolved_at: Option<DateTime<Utc>>,
    /// A link back to the rule that produced it.
    pub generator_url: String,
}

impl AlertView {
    /// Builds a view from a webhook alert.
    ///
    /// Used for the orphan-resolve path (ADR 001 D9, PRD §5.5), where there is no stored
    /// row to read: everything the message can say comes from the delivery that told us
    /// the alert had resolved.
    pub fn from_webhook(alert: &WebhookAlert) -> Self {
        Self {
            fingerprint: alert.fingerprint.clone(),
            labels: alert.labels.clone(),
            annotations: alert.annotations.clone(),
            starts_at: alert.starts_at,
            // Alertmanager sends the zero time for an alert that is still firing rather
            // than omitting `endsAt`, so "is it before startsAt?" is the test, not
            // "is it present?".
            resolved_at: (alert.ends_at > alert.starts_at).then_some(alert.ends_at),
            generator_url: alert.generator_url.clone(),
        }
    }

    /// How long the alert has been, or was, firing.
    ///
    /// Clamped at zero. A negative duration means the clocks disagree, and "fired for
    /// -3 minutes" in an alert channel costs more credibility than it buys information.
    fn duration(&self, now: DateTime<Utc>) -> TimeDelta {
        let end = self.resolved_at.unwrap_or(now);
        let delta = end - self.starts_at;
        if delta < TimeDelta::zero() {
            TimeDelta::zero()
        } else {
            delta
        }
    }
}

/// A storm-collapse parent, as its summary message is going to describe it (ADR 001 D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupView {
    /// Alertmanager's key for the group.
    pub group_key: GroupKey,
    /// How many of its members are still firing.
    pub firing: usize,
    /// How many have resolved.
    pub resolved: usize,
}

impl GroupView {
    /// Every member, firing or not.
    pub const fn total(&self) -> usize {
        self.firing + self.resolved
    }

    /// Whether the whole group has cleared.
    ///
    /// Drives the colour bar on the parent: a summary still showing red when every child
    /// underneath it is green is worse than no summary, because it is confidently wrong.
    pub const fn all_resolved(&self) -> bool {
        self.firing == 0
    }
}

/// Best-effort alert name for a group, read out of Alertmanager's `groupKey`.
///
/// The `group_message` table has no room for labels — ADR 001 D4's schema predates the
/// question — and neither `Op::PostGroup` nor `Op::RefreshGroup` carries any. The key
/// itself is the only thing a summary has to work with, and it looks like:
///
/// ```text
/// {}/{severity="critical"}:{alertname="KubePodNotReady", job="kubelet"}
/// ```
///
/// So this reads the `alertname="…"` pair out of it and returns `None` when there is not
/// one. Best-effort by construction: the key's format is Alertmanager's business, and a
/// summary that falls back to showing the raw key is degraded, not broken.
fn alertname_from_group_key(key: &GroupKey) -> Option<String> {
    const NEEDLE: &str = "alertname=\"";
    let raw = key.as_str();
    let after = raw.split_once(NEEDLE)?.1;
    let (name, _) = after.split_once('"')?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Which built-in template a render is asking for, and the data it needs.
///
/// A single enum rather than four methods so that "render the group summary from an
/// alert" is not expressible. The four templates take two different shapes of data, and
/// pairing them wrongly would produce an empty message.
#[derive(Clone, Copy, Debug)]
pub enum RenderRequest<'a> {
    /// An alert has started firing. Red.
    Firing(&'a AlertView),
    /// An alert has resolved: the in-place edit of ADR 001 D6. Green.
    Resolved(&'a AlertView),
    /// The threaded reply that accompanies a resolve, because `chat.update` does not
    /// notify (ADR 001 D6). Green.
    ThreadReply(&'a AlertView),
    /// The storm-collapse parent (ADR 001 D5). Red while anything under it is firing.
    GroupSummary(&'a GroupView),
}

/// The name of a built-in template.
///
/// The four ADR 001 D10 names, as a type. Phase 4 reads overrides from a `ConfigMap` keyed
/// by these names, and a typo in a key must be a rejected override rather than a
/// silently-ignored file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TemplateKind {
    /// `firing`.
    Firing,
    /// `resolved`.
    Resolved,
    /// `group_summary`.
    GroupSummary,
    /// `thread_reply`.
    ThreadReply,
}

impl TemplateKind {
    /// Every template that ships built in.
    pub const ALL: [Self; 4] = [
        Self::Firing,
        Self::Resolved,
        Self::GroupSummary,
        Self::ThreadReply,
    ];

    /// The template's name, as used for overrides and in the environment.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Firing => "firing",
            Self::Resolved => "resolved",
            Self::GroupSummary => "group_summary",
            Self::ThreadReply => "thread_reply",
        }
    }

    /// Reads a template name, or `None` if it is not one of the four.
    ///
    /// A `.j2` or `.jinja` suffix is accepted and stripped, because a `ConfigMap` of
    /// templates is a directory of files and people name files with extensions.
    pub fn parse(name: &str) -> Option<Self> {
        let stem = name
            .strip_suffix(".j2")
            .or_else(|| name.strip_suffix(".jinja"))
            .or_else(|| name.strip_suffix(".txt"))
            .unwrap_or(name);
        Self::ALL.into_iter().find(|kind| kind.as_str() == stem)
    }

    /// The built-in source for this template.
    pub(crate) const fn source(self) -> &'static str {
        match self {
            Self::Firing => include_str!("../../templates/firing.j2"),
            Self::Resolved => include_str!("../../templates/resolved.j2"),
            Self::GroupSummary => include_str!("../../templates/group_summary.j2"),
            Self::ThreadReply => include_str!("../../templates/thread_reply.j2"),
        }
    }
}

impl std::fmt::Display for TemplateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RenderRequest<'_> {
    /// Which template this request wants.
    pub const fn kind(&self) -> TemplateKind {
        match self {
            Self::Firing(_) => TemplateKind::Firing,
            Self::Resolved(_) => TemplateKind::Resolved,
            Self::ThreadReply(_) => TemplateKind::ThreadReply,
            Self::GroupSummary(_) => TemplateKind::GroupSummary,
        }
    }

    /// The colour bar this message gets (ADR 001 D10).
    pub const fn colour(&self) -> crate::Colour {
        match self {
            Self::Firing(_) => crate::Colour::Firing,
            Self::Resolved(_) | Self::ThreadReply(_) => crate::Colour::Resolved,
            // A summary whose children have all cleared goes green with them. The
            // alternative — a permanently red rollup over a thread of green replies — is
            // confidently wrong, which is worse than uninformative.
            Self::GroupSummary(group) => {
                if group.all_resolved() {
                    crate::Colour::Resolved
                } else {
                    crate::Colour::Firing
                }
            }
        }
    }
}

/// Escapes text for Slack `mrkdwn`.
///
/// The three characters Slack's documentation names, and only those. Escaping `*` or `_`
/// as well would stop an annotation that legitimately contains them from rendering, and
/// neither can be used to address anybody.
fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

fn escape_map(map: &LabelMap) -> BTreeMap<String, String> {
    map.iter()
        .map(|(key, value)| (escape(key), escape(value)))
        .collect()
}

fn lookup(map: &LabelMap, key: &str) -> String {
    map.get(key).map_or_else(String::new, |value| escape(value))
}

/// Renders a duration the way an on-call engineer reads one.
///
/// Two units at most, largest first, because "3d 4h 17m 9s" is not a thing anybody needs
/// to know about an alert. Sub-minute durations keep seconds, because for a flapping
/// alert that *is* the interesting fact.
///
/// Uses `chrono`'s accessors rather than arithmetic: `integer_division` is denied in this
/// workspace, and a hand-rolled `secs / 3600` is exactly the kind of code that is one
/// typo away from being wrong in an alerting message.
fn humanize(delta: TimeDelta) -> String {
    let days = delta.num_days();
    let hours = delta.num_hours() - TimeDelta::days(days).num_hours();
    let minutes = delta.num_minutes() - TimeDelta::hours(delta.num_hours()).num_minutes();
    let seconds = delta.num_seconds() - TimeDelta::minutes(delta.num_minutes()).num_seconds();

    if days > 0 {
        format!("{days}d {hours}h")
    } else if delta.num_hours() > 0 {
        format!("{}h {minutes}m", delta.num_hours())
    } else if delta.num_minutes() > 0 {
        format!("{}m {seconds}s", delta.num_minutes())
    } else {
        format!("{seconds}s")
    }
}

/// The UTC timestamp format every message uses.
///
/// UTC, unconditionally, and spelled out. An alerting relay's messages are read by people
/// in more than one timezone and quoted into incident channels; a local time with no zone
/// on it is the kind of ambiguity that costs ten minutes during an incident.
fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// The variables a `firing`, `resolved` or `thread_reply` template receives.
///
/// Every field is present on every render — none is ever undefined — which is what makes
/// [`minijinja::UndefinedBehavior::SemiStrict`] safe to use: a name that is undefined is
/// necessarily a typo in the template, and typos are worth the fallback message.
#[derive(Debug, Serialize)]
pub(crate) struct AlertVars {
    fingerprint: String,
    alertname: String,
    severity: String,
    summary: String,
    description: String,
    runbook_url: String,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    generator_url: String,
    started_at: String,
    resolved_at: String,
    duration: String,
    firing: bool,
}

impl AlertVars {
    pub(crate) fn build(view: &AlertView, now: DateTime<Utc>) -> Self {
        let alertname = view.labels.get("alertname").map_or_else(
            // Never blank. A message headed by nothing at all is unusable, and an alert
            // with no `alertname` label is a rule that will be found faster if the
            // message says so.
            || "(unnamed alert)".to_owned(),
            |name| escape(name),
        );
        Self {
            fingerprint: escape(view.fingerprint.as_str()),
            alertname,
            severity: lookup(&view.labels, "severity"),
            summary: lookup(&view.annotations, "summary"),
            description: lookup(&view.annotations, "description"),
            runbook_url: lookup(&view.annotations, "runbook_url"),
            labels: escape_map(&view.labels),
            annotations: escape_map(&view.annotations),
            generator_url: escape(&view.generator_url),
            started_at: stamp(view.starts_at),
            resolved_at: view.resolved_at.map(stamp).unwrap_or_default(),
            duration: humanize(view.duration(now)),
            firing: view.resolved_at.is_none(),
        }
    }

    /// The alert name, for the hardcoded fallback message.
    pub(crate) fn alertname(&self) -> &str {
        &self.alertname
    }

    /// The fingerprint, for the hardcoded fallback message.
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// The variables a `group_summary` template receives.
#[derive(Debug, Serialize)]
pub(crate) struct GroupVars {
    group_key: String,
    alertname: String,
    firing: usize,
    resolved: usize,
    total: usize,
    all_resolved: bool,
}

impl GroupVars {
    pub(crate) fn build(view: &GroupView) -> Self {
        Self {
            group_key: escape(view.group_key.as_str()),
            alertname: alertname_from_group_key(&view.group_key)
                .map(|name| escape(&name))
                .unwrap_or_default(),
            firing: view.firing,
            resolved: view.resolved,
            total: view.total(),
            all_resolved: view.all_resolved(),
        }
    }

    /// A title for the hardcoded fallback message.
    pub(crate) fn title(&self) -> &str {
        if self.alertname.is_empty() {
            &self.group_key
        } else {
            &self.alertname
        }
    }

    pub(crate) const fn firing(&self) -> usize {
        self.firing
    }

    pub(crate) const fn total(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AlertVars, AlertView, GroupVars, GroupView, RenderRequest, TemplateKind,
        alertname_from_group_key, escape, humanize, stamp,
    };
    use crate::Colour;
    use alertthread_core::{Fingerprint, GroupKey, LabelMap, WebhookAlert};
    use chrono::{DateTime, TimeDelta, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn labels(pairs: &[(&str, &str)]) -> LabelMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn view() -> AlertView {
        AlertView {
            fingerprint: Fingerprint::new("a1b2c3"),
            labels: labels(&[("alertname", "CephOSDDown"), ("severity", "critical")]),
            annotations: labels(&[("summary", "osd.3 is down")]),
            starts_at: at(1_784_642_520),
            resolved_at: None,
            generator_url: "http://prometheus/graph?g0.expr=up&g0.tab=1".to_owned(),
        }
    }

    #[test]
    fn slack_markup_characters_in_untrusted_text_are_escaped() {
        // `<!channel>` in message text notifies the entire workspace. An annotation is
        // written by whoever wrote the PrometheusRule, which is not necessarily whoever
        // operates the relay.
        assert_eq!(escape("<!channel>"), "&lt;!channel&gt;");
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("<http://x|y>"), "&lt;http://x|y&gt;");
    }

    #[test]
    fn escaping_leaves_the_formatting_characters_alone() {
        // `*` and `_` cannot address anybody, and escaping them would break an annotation
        // that legitimately contains one — `*` is common in PromQL.
        assert_eq!(escape("rate(*_total[5m])"), "rate(*_total[5m])");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn label_values_reach_the_template_already_escaped() {
        let mut view = view();
        view.annotations = labels(&[("summary", "<!here> disk & io")]);
        let vars = AlertVars::build(&view, at(1_784_642_520));
        assert_eq!(vars.summary, "&lt;!here&gt; disk &amp; io");
        assert_eq!(
            vars.annotations.get("summary").map(String::as_str),
            Some("&lt;!here&gt; disk &amp; io")
        );
    }

    #[test]
    fn label_keys_are_escaped_too() {
        let mut view = view();
        view.labels = labels(&[("alertname", "X"), ("<weird>", "v")]);
        let vars = AlertVars::build(&view, at(0));
        assert!(
            vars.labels.contains_key("&lt;weird&gt;"),
            "{:?}",
            vars.labels
        );
    }

    #[test]
    fn an_alert_without_an_alertname_still_gets_a_usable_heading() {
        // A message headed by an empty string is unusable, and this way the missing label
        // is visible in the channel rather than only in a log.
        let mut view = view();
        view.labels = LabelMap::new();
        let vars = AlertVars::build(&view, at(0));
        assert_eq!(vars.alertname(), "(unnamed alert)");
        assert_eq!(vars.severity, "");
    }

    #[test]
    fn a_firing_alert_reports_its_duration_against_now() {
        let vars = AlertVars::build(&view(), at(1_784_642_520 + 1_740));
        assert!(vars.firing);
        assert_eq!(vars.duration, "29m 0s");
        assert_eq!(vars.resolved_at, "");
        assert_eq!(vars.started_at, "2026-07-21 14:02:00 UTC");
        assert_eq!(vars.fingerprint(), "a1b2c3");
    }

    #[test]
    fn a_resolved_alert_reports_its_duration_against_its_resolution_not_now() {
        // Otherwise a resolved message re-rendered a day later would claim the alert
        // lasted a day.
        let mut view = view();
        view.resolved_at = Some(at(1_784_642_520 + 1_740));
        let vars = AlertVars::build(&view, at(1_784_642_520 + 100_000));
        assert!(!vars.firing);
        assert_eq!(vars.duration, "29m 0s");
        assert_eq!(vars.resolved_at, "2026-07-21 14:31:00 UTC");
    }

    #[test]
    fn a_duration_that_would_be_negative_is_clamped_to_zero() {
        // Clock skew between Prometheus and the relay is normal. "fired for -3 minutes"
        // in an alert channel costs more credibility than it buys information.
        let vars = AlertVars::build(&view(), at(1_784_642_520 - 600));
        assert_eq!(vars.duration, "0s");
    }

    #[test]
    fn durations_render_two_units_at_most() {
        assert_eq!(humanize(TimeDelta::seconds(0)), "0s");
        assert_eq!(humanize(TimeDelta::seconds(45)), "45s");
        assert_eq!(humanize(TimeDelta::seconds(59)), "59s");
        assert_eq!(humanize(TimeDelta::seconds(60)), "1m 0s");
        assert_eq!(humanize(TimeDelta::seconds(1_740)), "29m 0s");
        assert_eq!(humanize(TimeDelta::seconds(3_599)), "59m 59s");
        assert_eq!(humanize(TimeDelta::seconds(3_600)), "1h 0m");
        assert_eq!(humanize(TimeDelta::seconds(43_440)), "12h 4m");
        assert_eq!(humanize(TimeDelta::seconds(86_400)), "1d 0h");
        assert_eq!(humanize(TimeDelta::seconds(270_000)), "3d 3h");
    }

    #[test]
    fn timestamps_always_say_utc() {
        assert_eq!(stamp(at(1_784_642_520)), "2026-07-21 14:02:00 UTC");
    }

    #[test]
    fn a_view_built_from_a_firing_webhook_alert_has_not_resolved() {
        // Alertmanager sends the zero time rather than omitting `endsAt`, so a naive
        // `Option` check would report every firing alert as resolved at year 1.
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"firing","labels":{"alertname":"X"},"annotations":{},
                "startsAt":"2026-07-21T14:02:00Z","endsAt":"0001-01-01T00:00:00Z",
                "generatorURL":"http://p","fingerprint":"abc"}"#,
        )
        .expect("fixture parses");
        let built = AlertView::from_webhook(&alert);
        assert_eq!(built.resolved_at, None);
        assert_eq!(built.fingerprint, Fingerprint::new("abc"));
        assert_eq!(built.generator_url, "http://p");
    }

    #[test]
    fn a_view_built_from_a_resolved_webhook_alert_carries_its_end_time() {
        let alert: WebhookAlert = serde_json::from_str(
            r#"{"status":"resolved","labels":{"alertname":"X"},"annotations":{},
                "startsAt":"2026-07-21T14:02:00Z","endsAt":"2026-07-21T14:31:00Z",
                "fingerprint":"abc"}"#,
        )
        .expect("fixture parses");
        let built = AlertView::from_webhook(&alert);
        assert_eq!(built.resolved_at, Some(at(1_784_644_260)));
    }

    #[test]
    fn a_group_counts_its_members() {
        let group = GroupView {
            group_key: GroupKey::new("{}:{alertname=\"KubePodNotReady\"}"),
            firing: 9,
            resolved: 6,
        };
        assert_eq!(group.total(), 15);
        assert!(!group.all_resolved());

        let cleared = GroupView {
            firing: 0,
            ..group.clone()
        };
        assert!(cleared.all_resolved());
        assert!(format!("{cleared:?}").contains("firing: 0"));
    }

    #[test]
    fn a_group_summary_takes_its_title_from_the_group_key() {
        // The only place a summary can get a human name: `group_message` has no labels
        // column and neither PostGroup nor RefreshGroup carries any.
        let key = GroupKey::new("{}/{severity=\"critical\"}:{alertname=\"KubePodNotReady\"}");
        assert_eq!(
            alertname_from_group_key(&key).as_deref(),
            Some("KubePodNotReady")
        );
    }

    #[test]
    fn a_group_key_without_an_alertname_falls_back_to_the_raw_key() {
        for raw in ["{}:{job=\"kubelet\"}", "", "alertname=", "alertname=\"\""] {
            assert_eq!(alertname_from_group_key(&GroupKey::new(raw)), None, "{raw}");
        }
        let vars = GroupVars::build(&GroupView {
            group_key: GroupKey::new("{}:{job=\"kubelet\"}"),
            firing: 2,
            resolved: 0,
        });
        assert_eq!(vars.title(), r#"{}:{job="kubelet"}"#);
        assert_eq!(vars.firing(), 2);
        assert_eq!(vars.total(), 2);
    }

    #[test]
    fn group_vars_carry_the_derived_counts() {
        let vars = GroupVars::build(&GroupView {
            group_key: GroupKey::new("{}:{alertname=\"X\"}"),
            firing: 0,
            resolved: 4,
        });
        assert_eq!(vars.title(), "X");
        assert_eq!(vars.total(), 4);
        assert!(vars.all_resolved);
    }

    #[test]
    fn each_request_asks_for_the_template_it_names() {
        let alert = view();
        let group = GroupView {
            group_key: GroupKey::new("gk"),
            firing: 1,
            resolved: 0,
        };
        assert_eq!(RenderRequest::Firing(&alert).kind(), TemplateKind::Firing);
        assert_eq!(
            RenderRequest::Resolved(&alert).kind(),
            TemplateKind::Resolved
        );
        assert_eq!(
            RenderRequest::ThreadReply(&alert).kind(),
            TemplateKind::ThreadReply
        );
        assert_eq!(
            RenderRequest::GroupSummary(&group).kind(),
            TemplateKind::GroupSummary
        );
    }

    #[test]
    fn firing_is_red_and_resolved_is_green() {
        let alert = view();
        assert_eq!(RenderRequest::Firing(&alert).colour(), Colour::Firing);
        assert_eq!(RenderRequest::Resolved(&alert).colour(), Colour::Resolved);
        assert_eq!(
            RenderRequest::ThreadReply(&alert).colour(),
            Colour::Resolved
        );
    }

    #[test]
    fn a_group_summary_turns_green_when_its_last_child_resolves() {
        // A permanently red rollup over a thread of green replies is confidently wrong,
        // which is worse than uninformative.
        let firing = GroupView {
            group_key: GroupKey::new("gk"),
            firing: 1,
            resolved: 8,
        };
        assert_eq!(
            RenderRequest::GroupSummary(&firing).colour(),
            Colour::Firing
        );
        let cleared = GroupView {
            firing: 0,
            ..firing
        };
        assert_eq!(
            RenderRequest::GroupSummary(&cleared).colour(),
            Colour::Resolved
        );
    }

    #[test]
    fn template_names_round_trip_and_tolerate_a_file_extension() {
        // Overrides arrive as a `ConfigMap`, which is a directory of files, and people name
        // files with extensions.
        for kind in TemplateKind::ALL {
            assert_eq!(TemplateKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                TemplateKind::parse(&format!("{}.j2", kind.as_str())),
                Some(kind)
            );
            assert_eq!(
                TemplateKind::parse(&format!("{}.jinja", kind.as_str())),
                Some(kind)
            );
            assert_eq!(
                TemplateKind::parse(&format!("{}.txt", kind.as_str())),
                Some(kind)
            );
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn a_name_that_is_not_a_template_is_rejected_rather_than_guessed_at() {
        // Silently ignoring an unrecognised key would let a typo in a `ConfigMap` look
        // exactly like a working override.
        for name in ["firing.md", "Firing", "resolve", "", "group-summary"] {
            assert_eq!(TemplateKind::parse(name), None, "{name}");
        }
    }

    #[test]
    fn every_built_in_template_has_a_non_empty_source() {
        for kind in TemplateKind::ALL {
            assert!(!kind.source().trim().is_empty(), "{kind}");
        }
    }
}
