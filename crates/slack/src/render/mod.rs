//! Turning an alert into a Slack message — and never failing to.
//!
//! # The D9 guarantee
//!
//! [`Renderer::render`] **has no error return.** That is the whole design. ADR 001 D9
//! says a template that panics or errors falls back to a hardcoded minimal message and
//! never drops the alert; a `Result` here would put the decision to honour that in every
//! call site, and Phase 4's worker has enough decisions already. So the fallback is
//! inside, the return type is total, and "post nothing" is not a value this function can
//! produce.
//!
//! [`Rendered::degraded`] reports that it happened, which is what drives
//! `alertthread_fallback_posts_total{reason}` (ADR 001 D11). AGENTS.md: never swallow an
//! error without either handling it or emitting a metric. This does both.
//!
//! ## Why there is no `catch_unwind`
//!
//! D9's wording is "panics **or** errors". Catching the first of those is not available
//! to us: the release profile sets `panic = "abort"`, so `catch_unwind` would be dead
//! code in every build that ships. The guarantee this crate can actually make is the
//! stronger one — that no rendering path *can* panic — and it is enforced rather than
//! asserted: the workspace denies `unwrap_used`, `expect_used`, `panic`,
//! `indexing_slicing` and `integer_division`, and MiniJinja reports template failures as
//! `Result`. Recorded in the PR, because it is a place where the ADR asks for something
//! the build profile makes impossible.
//!
//! # Overrides never stop the relay starting
//!
//! A bad override is *rejected* and the built-in kept; [`Renderer::new`] returns the
//! rejections alongside a working renderer rather than an `Err`. D9 covers a template
//! that fails at render time and is silent about one that fails to compile, but the
//! argument is identical and stronger: a pod that refuses to start over a typo in a
//! `ConfigMap` is total silence, which is strictly worse than the degraded-but-alive
//! outcome D9 chooses everywhere else.

mod blocks;
mod view;

use std::collections::BTreeMap;

use minijinja::{Environment, UndefinedBehavior, Value, context};

use crate::message::{Colour, MessageBody};

pub use blocks::Truncation;
pub use view::{AlertView, GroupView, RenderRequest, TemplateKind};

use view::{AlertVars, GroupVars};

/// Why a message came out of the hardcoded fallback instead of a template.
///
/// A closed, low-cardinality set, because it becomes the `reason` label on
/// `alertthread_fallback_posts_total` (ADR 001 D11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// The template raised an error while rendering: an undefined name, a filter applied
    /// to the wrong type, a division by zero in an expression.
    RenderFailed,
    /// The template rendered successfully and produced nothing usable.
    ///
    /// Not a hypothetical. `{% if severity == "critical" %}…{% endif %}` with no `else`
    /// renders to an empty string for every other alert, and Slack rejects a message with
    /// neither text nor blocks. Without this branch that template would silence every
    /// warning-level alert in the workspace and look like it was working.
    EmptyOutput,
}

impl FallbackReason {
    /// The metric label for this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RenderFailed => "render_failed",
            Self::EmptyOutput => "empty_output",
        }
    }
}

/// A message that had to be built without its template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Degradation {
    /// The template that let us down.
    pub template: TemplateKind,
    /// Why, as a metric label.
    pub reason: FallbackReason,
    /// MiniJinja's own description, including the line number, for the log line.
    ///
    /// Empty for [`FallbackReason::EmptyOutput`], which has no underlying error.
    pub detail: String,
}

/// The result of rendering one message.
///
/// `body` is always usable. The other two fields describe how much of the intended
/// message survived, and exist so the shell can count what happened instead of the
/// renderer deciding on its own that nobody needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The message to send.
    pub body: MessageBody,
    /// Set when the hardcoded fallback was used.
    pub degraded: Option<Degradation>,
    /// Set when Slack's limits forced something out of the message.
    pub truncated: Option<Truncation>,
}

impl Rendered {
    /// Whether this message is exactly what its template asked for.
    pub const fn is_intact(&self) -> bool {
        self.degraded.is_none() && self.truncated.is_none()
    }
}

/// An override that would not compile, and was therefore not installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedOverride {
    /// Which template the override was for.
    pub template: TemplateKind,
    /// MiniJinja's syntax error, with its line number.
    pub detail: String,
}

/// Renders alerts into Slack message bodies.
///
/// Cheap to share and `Send + Sync`; build one at startup and hand it to every worker.
pub struct Renderer {
    env: Environment<'static>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Renderer {
    /// The four templates ADR 001 D10 ships, with no overrides.
    #[must_use]
    pub fn builtin() -> Self {
        let (renderer, rejected) = Self::new(std::iter::empty());
        debug_assert!(
            rejected.is_empty(),
            "a built-in template failed to compile: {rejected:?}"
        );
        renderer
    }

    /// Installs user overrides over the built-in templates.
    ///
    /// Returns a renderer that always works, plus every override that was refused. An
    /// override that does not compile is dropped and its built-in kept — see the module
    /// documentation for why this is not an `Err`.
    ///
    /// The caller is expected to log and count the rejections; they are the operator's
    /// only signal that the `ConfigMap` they just applied is not in effect.
    #[must_use]
    pub fn new(
        overrides: impl IntoIterator<Item = (TemplateKind, String)>,
    ) -> (Self, Vec<RejectedOverride>) {
        let mut env = Environment::new();

        // SemiStrict, not Lenient. Under Lenient a mistyped `{{ alert.alertnmae }}`
        // renders as an empty string: the message posts, looks subtly wrong, and nothing
        // anywhere records that a template is broken. Under SemiStrict it raises, the
        // fallback catches it, and `alertthread_fallback_posts_total` increments — which
        // is AGENTS.md's "never swallow an error without emitting a metric".
        //
        // SemiStrict rather than Strict so `{% if alert.thing %}` still works as a
        // presence check, which is the one place leniency is what an author means.
        env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
        // Bounds `{% include %}`/macro recursion. A template cannot be made to recurse
        // until the stack runs out, which in a `panic = "abort"` binary is not an error
        // but a dead relay.
        env.set_recursion_limit(32);
        // Puts the line number and the offending expression in the error, which is the
        // whole content of the log line an operator gets when their template breaks.
        env.set_debug(true);

        // Collected into a map so a caller that supplies the same template twice gets a
        // defined answer (the last one) rather than whichever the iterator happened to
        // yield second.
        let mut supplied: BTreeMap<TemplateKind, String> = overrides.into_iter().collect();
        let mut rejected = Vec::new();

        for template in TemplateKind::ALL {
            // One `add_template` call site for both the override and the built-in, so the
            // rejection path is the same code in both cases — and is therefore exercised
            // by the tests that break an override, rather than being a second, identical
            // branch that only a malformed release could ever reach.
            let took = supplied
                .remove(&template)
                .is_some_and(|source| install(&mut env, template, source, &mut rejected));
            if !took {
                install(
                    &mut env,
                    template,
                    template.source().to_owned(),
                    &mut rejected,
                );
            }
        }

        (Self { env }, rejected)
    }

    /// Renders a message. Cannot fail.
    ///
    /// If the template errors, or renders to nothing, the returned body is the hardcoded
    /// minimal message of ADR 001 D9 and [`Rendered::degraded`] says so.
    #[must_use]
    pub fn render(&self, request: &RenderRequest<'_>, now: chrono::DateTime<Utc>) -> Rendered {
        let template = request.kind();
        let colour = request.colour();

        let (vars, fallback): (Value, Fallback) = match request {
            RenderRequest::Firing(alert) => {
                let vars = AlertVars::build(alert, now);
                let text = fallback_alert_text(&vars, "FIRING");
                (context! { alert => Value::from_serialize(&vars) }, text)
            }
            RenderRequest::Resolved(alert) | RenderRequest::ThreadReply(alert) => {
                let vars = AlertVars::build(alert, now);
                let text = fallback_alert_text(&vars, "RESOLVED");
                (context! { alert => Value::from_serialize(&vars) }, text)
            }
            RenderRequest::GroupSummary(group) => {
                let vars = GroupVars::build(group);
                let text = fallback_group_text(&vars);
                (context! { group => Value::from_serialize(&vars) }, text)
            }
        };

        match self.render_text(template, &vars) {
            Ok(text) if !text.trim().is_empty() => Self::assemble(colour, &text, None),
            Ok(_) => Self::assemble(
                colour,
                &fallback.0,
                Some(Degradation {
                    template,
                    reason: FallbackReason::EmptyOutput,
                    detail: String::new(),
                }),
            ),
            Err(detail) => Self::assemble(
                colour,
                &fallback.0,
                Some(Degradation {
                    template,
                    reason: FallbackReason::RenderFailed,
                    detail,
                }),
            ),
        }
    }

    fn render_text(&self, template: TemplateKind, vars: &Value) -> Result<String, String> {
        let compiled = self
            .env
            .get_template(template.as_str())
            .map_err(|error| describe(&error))?;
        compiled.render(vars).map_err(|error| describe(&error))
    }

    fn assemble(colour: Colour, text: &str, degraded: Option<Degradation>) -> Rendered {
        let (block_list, truncated) = blocks::sections(text);
        Rendered {
            body: MessageBody::new(colour, blocks::notification(text), block_list),
            degraded,
            truncated,
        }
    }
}

use chrono::Utc;

/// Compiles one template into the environment, recording it if it will not compile.
///
/// Returns whether it took, so the caller can fall back to the built-in.
fn install(
    env: &mut Environment<'static>,
    template: TemplateKind,
    source: String,
    rejected: &mut Vec<RejectedOverride>,
) -> bool {
    match env.add_template_owned(template.as_str().to_owned(), source) {
        Ok(()) => true,
        Err(error) => {
            rejected.push(RejectedOverride {
                template,
                detail: describe(&error),
            });
            false
        }
    }
}

/// The hardcoded minimal message, as plain text.
///
/// A newtype only to keep the two builders from being passed to each other.
struct Fallback(String);

/// ADR 001 D9's minimal message for a single alert.
///
/// Built by `format!` from values that are already `String`s, with no templating, no
/// indexing and no fallible step. It says three things and they are the three that
/// matter: what fired, how long ago, and that the message you are reading is degraded.
fn fallback_alert_text(vars: &AlertVars, status: &str) -> Fallback {
    Fallback(format!(
        "*{status} · {name}*\n\
         `{fingerprint}`\n\
         _alertthread could not render its message template, so this is the built-in \
         minimal message. The alert itself is unaffected; check \
         `alertthread_fallback_posts_total`._",
        name = vars.alertname(),
        fingerprint = vars.fingerprint(),
    ))
}

/// ADR 001 D9's minimal message for a storm-collapse summary.
fn fallback_group_text(vars: &GroupVars) -> Fallback {
    Fallback(format!(
        "*{firing} of {total} alerts firing · {title}*\n\
         _alertthread could not render its message template, so this is the built-in \
         minimal message. Individual alerts are threaded under this message._",
        firing = vars.firing(),
        total = vars.total(),
        title = vars.title(),
    ))
}

/// Flattens a MiniJinja error into one log line.
///
/// MiniJinja's own `Display` already carries the detail, the template name and the line
/// number — `invalid operation: cannot convert map to integer (in firing:1)` — which is
/// exactly what an operator needs and nothing more. All this adds is the flattening:
/// `tracing` fields spanning several lines are unreadable in a JSON log.
fn describe(error: &minijinja::Error) -> String {
    error.to_string().replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::{
        AlertView, Degradation, FallbackReason, GroupView, RejectedOverride, RenderRequest,
        Renderer, TemplateKind,
    };
    use crate::message::{Block, Colour, MAX_SECTION_CHARS};
    use alertthread_core::{Fingerprint, GroupKey, LabelMap};
    use chrono::{DateTime, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    const FIRED_AT: i64 = 1_784_642_520;
    const NOW: i64 = FIRED_AT + 1_740;

    fn labels(pairs: &[(&str, &str)]) -> LabelMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn alert() -> AlertView {
        AlertView {
            fingerprint: Fingerprint::new("a1b2c3d4e5f60718"),
            labels: labels(&[
                ("alertname", "CephOSDDown"),
                ("severity", "critical"),
                ("osd", "osd.3"),
                ("instance", "ceph-node-2"),
            ]),
            annotations: labels(&[("summary", "Ceph OSD osd.3 is down")]),
            starts_at: at(FIRED_AT),
            resolved_at: None,
            generator_url: "http://prometheus/graph?g0.expr=up".to_owned(),
        }
    }

    fn group() -> GroupView {
        GroupView {
            group_key: GroupKey::new("{}:{alertname=\"KubePodNotReady\"}"),
            firing: 9,
            resolved: 6,
        }
    }

    fn body_text(message: &super::Rendered) -> String {
        message
            .body
            .blocks()
            .iter()
            .map(|block| match block {
                Block::Section { text } => text.text.clone(),
                Block::Context { elements } => {
                    elements.first().map(|t| t.text.clone()).unwrap_or_default()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_built_in_template_compiles() {
        // The one thing `Renderer::builtin` cannot report, because it has nowhere to
        // report it to. This test is that report.
        let (_, rejected) = Renderer::new(std::iter::empty());
        assert_eq!(rejected, Vec::new());
    }

    #[test]
    fn the_default_renderer_is_the_built_in_one() {
        let alert = alert();
        let request = RenderRequest::Firing(&alert);
        assert_eq!(
            Renderer::default().render(&request, at(NOW)).body,
            Renderer::builtin().render(&request, at(NOW)).body
        );
        assert!(format!("{:?}", Renderer::builtin()).starts_with("Renderer"));
    }

    #[test]
    fn a_firing_alert_renders_intact() {
        let alert = alert();
        let message = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(NOW));

        assert!(message.is_intact(), "{:?}", message.degraded);
        let text = body_text(&message);
        assert!(text.contains("FIRING"), "{text}");
        assert!(text.contains("CephOSDDown"), "{text}");
        assert!(text.contains("Ceph OSD osd.3 is down"), "{text}");
        assert!(text.contains("`osd=osd.3`"), "{text}");
        assert!(text.contains("29m 0s"), "{text}");
        assert!(text.contains("|source>"), "{text}");
        // The heading label is not repeated in the label list.
        assert!(!text.contains("`alertname=CephOSDDown`"), "{text}");
    }

    #[test]
    fn a_resolved_alert_renders_green_with_both_timestamps() {
        let mut alert = alert();
        alert.resolved_at = Some(at(NOW));
        let message = Renderer::builtin().render(&RenderRequest::Resolved(&alert), at(NOW));

        assert!(message.is_intact());
        let text = body_text(&message);
        assert!(text.contains("RESOLVED"), "{text}");
        assert!(text.contains("2026-07-21 14:02:00 UTC"), "{text}");
        assert!(text.contains("2026-07-21 14:31:00 UTC"), "{text}");
        assert_eq!(message.body.attachments[0].color, Colour::Resolved.as_hex());
    }

    #[test]
    fn a_thread_reply_is_short_and_green() {
        let mut alert = alert();
        alert.resolved_at = Some(at(NOW));
        let message = Renderer::builtin().render(&RenderRequest::ThreadReply(&alert), at(NOW));

        let text = body_text(&message);
        assert!(text.contains("Resolved"), "{text}");
        assert!(text.contains("29m 0s"), "{text}");
        assert!(
            text.lines().count() <= 2,
            "the reply exists to be noticed, not read: {text}"
        );
        assert_eq!(message.body.attachments[0].color, Colour::Resolved.as_hex());
    }

    #[test]
    fn a_group_summary_reports_its_counts() {
        let group = group();
        let message = Renderer::builtin().render(&RenderRequest::GroupSummary(&group), at(NOW));

        let text = body_text(&message);
        assert!(text.contains("9 of 15"), "{text}");
        assert!(text.contains("KubePodNotReady"), "{text}");
        assert!(text.contains("6 already resolved"), "{text}");
        assert_eq!(message.body.attachments[0].color, Colour::Firing.as_hex());
    }

    #[test]
    fn a_fully_resolved_group_summary_goes_green() {
        let cleared = GroupView {
            firing: 0,
            resolved: 15,
            ..group()
        };
        let message = Renderer::builtin().render(&RenderRequest::GroupSummary(&cleared), at(NOW));

        let text = body_text(&message);
        assert!(text.contains("RESOLVED · 15 alerts"), "{text}");
        assert_eq!(message.body.attachments[0].color, Colour::Resolved.as_hex());
    }

    #[test]
    fn a_template_that_errors_at_render_time_degrades_instead_of_dropping_the_alert() {
        // ADR 001 D9's headline case, and the test the ROADMAP names explicitly: feed the
        // renderer a deliberately broken template and prove the alert still posts.
        //
        // This one compiles cleanly and fails only when message — `int` cannot be applied
        // to a mapping — which is exactly the shape of the bug a template author ships
        // without noticing, because it depends on the data.
        let (renderer, rejected) = Renderer::new([(
            TemplateKind::Firing,
            "{{ alert.labels | int }} exploded".to_owned(),
        )]);
        assert_eq!(rejected, Vec::new(), "this template must compile");

        let alert = alert();
        let message = renderer.render(&RenderRequest::Firing(&alert), at(NOW));

        let degraded = message
            .degraded
            .as_ref()
            .expect("the fallback must have engaged");
        assert_eq!(degraded.template, TemplateKind::Firing);
        assert_eq!(degraded.reason, FallbackReason::RenderFailed);
        assert!(!degraded.detail.is_empty(), "the cause must be reportable");
        assert!(!degraded.detail.contains('\n'), "{}", degraded.detail);

        // The alert still posts, and still says which alert it is.
        let text = body_text(&message);
        assert!(text.contains("CephOSDDown"), "{text}");
        assert!(text.contains("a1b2c3d4e5f60718"), "{text}");
        assert!(text.contains("FIRING"), "{text}");
        assert!(!message.body.text.is_empty());
        assert_eq!(message.body.attachments[0].color, Colour::Firing.as_hex());
    }

    #[test]
    fn an_undefined_variable_degrades_rather_than_rendering_a_blank() {
        // The reason for SemiStrict. Under Lenient this renders "FIRING: " and posts
        // happily, and nothing anywhere records that the template is broken.
        let (renderer, rejected) = Renderer::new([(
            TemplateKind::Firing,
            "FIRING: {{ alert.alertnmae }}".to_owned(),
        )]);
        assert_eq!(rejected, Vec::new());

        let alert = alert();
        let message = renderer.render(&RenderRequest::Firing(&alert), at(NOW));
        assert_eq!(
            message.degraded.as_ref().map(|d| d.reason),
            Some(FallbackReason::RenderFailed)
        );
    }

    #[test]
    fn a_presence_check_on_a_missing_label_still_works_under_semistrict() {
        // The reason for SemiStrict rather than Strict: `{% if %}` as a presence test is
        // what a template author means, and making it an error would be hostile.
        let (renderer, rejected) = Renderer::new([(
            TemplateKind::Firing,
            "{% if alert.labels.team %}team {{ alert.labels.team }}{% else %}no team{% endif %}"
                .to_owned(),
        )]);
        assert_eq!(rejected, Vec::new());

        let alert = alert();
        let message = renderer.render(&RenderRequest::Firing(&alert), at(NOW));
        assert!(message.is_intact(), "{:?}", message.degraded);
        assert_eq!(body_text(&message), "no team");
    }

    #[test]
    fn a_template_that_renders_to_nothing_degrades_rather_than_posting_an_empty_message() {
        // Not hypothetical: `{% if severity == "critical" %}…{% endif %}` with no `else`
        // renders empty for every warning-level alert. Slack rejects a message with
        // neither text nor blocks, so without this branch that template would silence
        // every warning in the workspace and look like it was working.
        let (renderer, _) = Renderer::new([(
            TemplateKind::Firing,
            "{% if alert.severity == 'nonesuch' %}never{% endif %}".to_owned(),
        )]);

        let alert = alert();
        let message = renderer.render(&RenderRequest::Firing(&alert), at(NOW));

        let degraded = message
            .degraded
            .as_ref()
            .expect("the fallback must have engaged");
        assert_eq!(degraded.reason, FallbackReason::EmptyOutput);
        assert!(degraded.detail.is_empty());
        assert!(body_text(&message).contains("CephOSDDown"));
    }

    #[test]
    fn the_group_fallback_still_reports_the_counts() {
        // A degraded summary that lost its counts would be a rollup saying nothing, over
        // a thread of alerts nobody can see without expanding it.
        let (renderer, _) =
            Renderer::new([(TemplateKind::GroupSummary, "{{ group | int }}".to_owned())]);
        let group = group();
        let message = renderer.render(&RenderRequest::GroupSummary(&group), at(NOW));

        assert_eq!(
            message.degraded.as_ref().map(|d| d.reason),
            Some(FallbackReason::RenderFailed)
        );
        let text = body_text(&message);
        assert!(text.contains("9 of 15"), "{text}");
        assert!(text.contains("KubePodNotReady"), "{text}");
    }

    #[test]
    fn an_override_that_does_not_compile_is_rejected_and_the_built_in_kept() {
        // A pod that refuses to start over a typo in a `ConfigMap` is total silence, which
        // is worse than the degraded-but-alive outcome D9 chooses everywhere else.
        let (renderer, rejected) =
            Renderer::new([(TemplateKind::Resolved, "{% for x in %}unclosed".to_owned())]);

        assert_eq!(rejected.len(), 1);
        let rejection: &RejectedOverride = &rejected[0];
        assert_eq!(rejection.template, TemplateKind::Resolved);
        assert!(!rejection.detail.is_empty());

        let mut alert = alert();
        alert.resolved_at = Some(at(NOW));
        let message = renderer.render(&RenderRequest::Resolved(&alert), at(NOW));
        assert!(
            message.is_intact(),
            "the built-in must still be in place: {:?}",
            message.degraded
        );
        assert!(body_text(&message).contains("RESOLVED"));
    }

    #[test]
    fn one_broken_override_does_not_disturb_the_others() {
        let (renderer, rejected) = Renderer::new([
            (TemplateKind::Firing, "{% endfor %}".to_owned()),
            (TemplateKind::ThreadReply, "resolved, ok".to_owned()),
        ]);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].template, TemplateKind::Firing);

        let alert = alert();
        assert!(
            renderer
                .render(&RenderRequest::Firing(&alert), at(NOW))
                .is_intact()
        );
        assert_eq!(
            body_text(&renderer.render(&RenderRequest::ThreadReply(&alert), at(NOW))),
            "resolved, ok"
        );
    }

    #[test]
    fn a_working_override_replaces_only_the_template_it_names() {
        let (renderer, rejected) = Renderer::new([(
            TemplateKind::Firing,
            "custom {{ alert.alertname }}".to_owned(),
        )]);
        assert_eq!(rejected, Vec::new());

        let alert = alert();
        assert_eq!(
            body_text(&renderer.render(&RenderRequest::Firing(&alert), at(NOW))),
            "custom CephOSDDown"
        );
        assert!(
            body_text(&renderer.render(&RenderRequest::ThreadReply(&alert), at(NOW)))
                .contains("Resolved")
        );
    }

    #[test]
    fn an_enormous_annotation_is_truncated_visibly_rather_than_rejected_by_slack() {
        // A section over 3000 characters is `invalid_blocks`, which is terminal, which is
        // a dead-lettered alert. This is the case that has to be caught here.
        let mut alert = alert();
        alert.annotations = labels(&[("summary", &"long ".repeat(40_000))]);
        let message = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(NOW));

        let truncated = message.truncated.expect("truncation must be reported");
        assert!(truncated.dropped_chars > 0);
        assert!(truncated.dropped_blocks > 0);
        assert!(!message.is_intact());

        assert!(message.body.blocks().len() <= crate::message::MAX_BLOCKS);
        for block in message.body.blocks() {
            if let Block::Section { text } = block {
                assert!(text.text.chars().count() <= MAX_SECTION_CHARS);
            }
        }
        assert!(
            body_text(&message).contains("truncated"),
            "the reader has to be able to see that something was cut"
        );
    }

    #[test]
    fn a_degraded_message_is_still_length_limited() {
        // The fallback goes through the same block builder, so a pathological alertname
        // cannot make the fallback itself unsendable.
        let mut alert = alert();
        alert.labels = labels(&[("alertname", &"N".repeat(200_000))]);
        let (renderer, _) = Renderer::new([(TemplateKind::Firing, "{{ boom }}".to_owned())]);
        let message = renderer.render(&RenderRequest::Firing(&alert), at(NOW));

        assert!(message.degraded.is_some());
        assert!(message.body.blocks().len() <= crate::message::MAX_BLOCKS);
        assert!(message.body.text.chars().count() <= 201);
    }

    #[test]
    fn every_fallback_reason_has_a_distinct_label() {
        let labels: Vec<&str> = [FallbackReason::RenderFailed, FallbackReason::EmptyOutput]
            .iter()
            .map(|r| r.as_str())
            .collect();
        assert_eq!(labels, ["render_failed", "empty_output"]);
        assert_eq!(format!("{:?}", FallbackReason::EmptyOutput), "EmptyOutput");
    }

    #[test]
    fn a_degradation_carries_enough_to_find_the_broken_template() {
        let degradation = Degradation {
            template: TemplateKind::GroupSummary,
            reason: FallbackReason::RenderFailed,
            detail: "syntax error".to_owned(),
        };
        let message = format!("{degradation:?}");
        assert!(message.contains("GroupSummary"), "{message}");
        assert!(message.contains("syntax error"), "{message}");
    }

    #[test]
    fn markup_in_an_annotation_cannot_address_the_channel() {
        // The escaping test, at the level it actually matters: end to end, through a real
        // template, into the bytes that would go to Slack.
        let mut alert = alert();
        alert.annotations = labels(&[("summary", "<!channel> everything is on fire")]);
        let message = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(NOW));

        let json = serde_json::to_string(&message.body).expect("body serialises");
        assert!(!json.contains("<!channel>"), "{json}");
        assert!(json.contains("&lt;!channel&gt;"), "{json}");
    }
}
