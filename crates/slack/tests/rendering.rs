//! Snapshots of what actually reaches Slack.
//!
//! # These snapshots assert something, they do not merely freeze it
//!
//! `insta` will happily record whatever the code produces today, which makes a snapshot
//! suite an excellent way to make a bug permanent. So each snapshot here is paired with
//! ordinary assertions about the property it is supposed to be demonstrating — the colour
//! matches the state, the untrusted text is escaped, no block exceeds Slack's limit — and
//! the snapshot's job is to catch *unintended* change in everything else.
//!
//! Read a snapshot diff as a question: "is this new output better?" If a reviewer cannot
//! answer that from the diff, the assertion next to it is the thing to trust.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a test that unwraps is a test that fails loudly, which is what \
              clippy.toml's allow-*-in-tests settings say for unit tests. Integration \
              tests reach these helpers from outside a #[test] function, where clippy \
              cannot see the context, so the same policy is stated here."
)]

use alertthread_core::{Fingerprint, GroupKey, LabelMap};
use alertthread_slack::{
    AlertView, Block, Colour, GroupView, MAX_BLOCKS, MAX_SECTION_CHARS, MessageBody, RenderRequest,
    Rendered, Renderer, TemplateKind,
};
use chrono::{DateTime, Utc};

/// 2026-07-21 14:02:00 UTC — the time in ADR 001 D6's worked example.
const FIRED_AT: i64 = 1_784_642_520;
/// 2026-07-21 14:31:00 UTC — 29 minutes later, as in the same example.
const RESOLVED_AT: i64 = 1_784_644_260;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0).expect("timestamp is in range")
}

fn labels(pairs: &[(&str, &str)]) -> LabelMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// ADR 001 D6's example alert, so the snapshots and the ADR describe the same message.
fn ceph_osd_down() -> AlertView {
    AlertView {
        fingerprint: Fingerprint::new("a1b2c3d4e5f60718"),
        labels: labels(&[
            ("alertname", "CephOSDDown"),
            ("severity", "critical"),
            ("osd", "osd.3"),
            ("instance", "ceph-node-2"),
            ("job", "rook-ceph-mgr"),
        ]),
        annotations: labels(&[
            ("summary", "Ceph OSD osd.3 is down"),
            (
                "description",
                "OSD osd.3 on ceph-node-2 has been marked down for more than 5 minutes.",
            ),
            ("runbook_url", "https://runbooks.example/ceph/osd-down"),
        ]),
        starts_at: at(FIRED_AT),
        resolved_at: None,
        generator_url: "https://prometheus.example/graph?g0.expr=ceph_osd_up==0&g0.tab=1"
            .to_owned(),
    }
}

fn resolved_ceph_osd_down() -> AlertView {
    AlertView {
        resolved_at: Some(at(RESOLVED_AT)),
        ..ceph_osd_down()
    }
}

fn kube_pod_not_ready() -> GroupView {
    GroupView {
        group_key: GroupKey::new(
            "{}/{severity=\"critical\"}:{alertname=\"KubePodNotReady\", job=\"kube-state-metrics\"}",
        ),
        firing: 9,
        resolved: 6,
    }
}

/// The rendered body as JSON, which is exactly the shape Slack receives.
fn snapshot_of(rendered: &Rendered) -> serde_json::Value {
    serde_json::to_value(&rendered.body).expect("a message body always serialises")
}

/// Every property that must hold of *any* message this relay sends.
///
/// Asserted on every snapshot, so a snapshot accepted carelessly still cannot ship a
/// message Slack would reject or a notification nobody would see.
fn assert_universally_valid(body: &MessageBody) {
    assert!(
        !body.text.trim().is_empty(),
        "Slack shows a blank notification for an empty `text`"
    );
    assert_eq!(
        body.attachments.len(),
        1,
        "exactly one attachment: two would draw two colour bars (ADR 001 D10)"
    );
    assert!(
        !body.blocks().is_empty(),
        "a message with no blocks is silence"
    );
    assert!(body.blocks().len() <= MAX_BLOCKS);
    for block in body.blocks() {
        let text = match block {
            Block::Section { text } => &text.text,
            Block::Context { elements } => {
                assert_eq!(elements.len(), 1);
                &elements[0].text
            }
        };
        assert!(
            text.chars().count() <= MAX_SECTION_CHARS,
            "a block over {MAX_SECTION_CHARS} characters is rejected as `invalid_blocks`"
        );
        assert!(!text.is_empty(), "Slack rejects an empty text object");
    }
}

fn colour_of(body: &MessageBody) -> &str {
    &body.attachments[0].color
}

#[test]
fn firing() {
    let alert = ceph_osd_down();
    let rendered = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(FIRED_AT + 1_740));

    assert!(rendered.is_intact());
    assert_universally_valid(&rendered.body);
    // The properties the snapshot is *for*, stated so a careless `cargo insta accept`
    // cannot quietly change them.
    assert_eq!(colour_of(&rendered.body), Colour::Firing.as_hex());
    assert!(rendered.body.text.contains("CephOSDDown"));

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn resolved() {
    let alert = resolved_ceph_osd_down();
    let rendered =
        Renderer::builtin().render(&RenderRequest::Resolved(&alert), at(RESOLVED_AT + 86_400));

    assert!(rendered.is_intact());
    assert_universally_valid(&rendered.body);
    assert_eq!(colour_of(&rendered.body), Colour::Resolved.as_hex());
    // The duration is measured to the resolution, not to `now` — otherwise a message
    // re-rendered a day later would claim the alert lasted a day.
    let body = serde_json::to_string(&rendered.body).expect("serialises");
    assert!(body.contains("29m 0s"), "{body}");

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn thread_reply() {
    let alert = resolved_ceph_osd_down();
    let rendered = Renderer::builtin().render(&RenderRequest::ThreadReply(&alert), at(RESOLVED_AT));

    assert!(rendered.is_intact());
    assert_universally_valid(&rendered.body);
    assert_eq!(colour_of(&rendered.body), Colour::Resolved.as_hex());
    // ADR 001 D6: this reply exists to generate an unread indicator, because
    // `chat.update` does not. Short is the requirement, not a preference.
    assert_eq!(rendered.body.blocks().len(), 1);

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn group_summary() {
    let group = kube_pod_not_ready();
    let rendered = Renderer::builtin().render(&RenderRequest::GroupSummary(&group), at(FIRED_AT));

    assert!(rendered.is_intact());
    assert_universally_valid(&rendered.body);
    assert_eq!(colour_of(&rendered.body), Colour::Firing.as_hex());

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn group_summary_fully_resolved() {
    let group = GroupView {
        firing: 0,
        resolved: 15,
        ..kube_pod_not_ready()
    };
    let rendered = Renderer::builtin().render(&RenderRequest::GroupSummary(&group), at(FIRED_AT));

    assert_universally_valid(&rendered.body);
    assert_eq!(
        colour_of(&rendered.body),
        Colour::Resolved.as_hex(),
        "a red rollup over a thread of green replies is confidently wrong"
    );

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn a_sparse_alert_still_renders_a_usable_message() {
    // The other end of the range: no severity, no annotations, no generator URL. This is
    // what a hand-written recording rule looks like, and the message must still identify
    // the alert.
    let alert = AlertView {
        fingerprint: Fingerprint::new("0011223344556677"),
        labels: labels(&[("alertname", "SomethingBroke")]),
        annotations: LabelMap::new(),
        starts_at: at(FIRED_AT),
        resolved_at: None,
        generator_url: String::new(),
    };
    let rendered = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(FIRED_AT + 60));

    assert!(rendered.is_intact());
    assert_universally_valid(&rendered.body);
    assert!(rendered.body.text.contains("SomethingBroke"));

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn markup_in_an_annotation_is_escaped_rather_than_rendered() {
    // An annotation is written by whoever wrote the `PrometheusRule`. `<!channel>` in
    // Slack message text notifies the entire workspace, so this is the difference between
    // an alert and a workspace-wide ping.
    let alert = AlertView {
        annotations: labels(&[(
            "summary",
            "<!channel> disk > 90% & climbing — see <https://evil.example|here>",
        )]),
        ..ceph_osd_down()
    };
    let rendered = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(FIRED_AT + 60));

    assert_universally_valid(&rendered.body);
    let json = serde_json::to_string(&rendered.body).expect("serialises");
    assert!(!json.contains("<!channel>"), "{json}");
    assert!(!json.contains("<https://evil.example|here>"), "{json}");

    insta::assert_json_snapshot!(snapshot_of(&rendered));
}

#[test]
fn a_broken_template_degrades_to_the_hardcoded_message() {
    // ADR 001 D9's fallback, snapshotted so its wording is reviewable: this is the
    // message an operator sees at 3am when their own template is the problem, and it has
    // to be self-explanatory.
    let (renderer, rejected) =
        Renderer::new([(TemplateKind::Firing, "{{ alert.labels | int }}".to_owned())]);
    assert!(
        rejected.is_empty(),
        "the override must compile: {rejected:?}"
    );

    let alert = ceph_osd_down();
    let message = renderer.render(&RenderRequest::Firing(&alert), at(FIRED_AT + 1_740));

    assert!(message.degraded.is_some());
    assert_universally_valid(&message.body);
    assert_eq!(
        colour_of(&message.body),
        Colour::Firing.as_hex(),
        "a degraded message still has to look like the state it describes"
    );
    assert!(message.body.text.contains("CephOSDDown"));

    insta::assert_json_snapshot!(snapshot_of(&message));
}

#[test]
fn an_over_long_annotation_is_split_across_blocks_and_loses_nothing() {
    // Slack rejects a section over 3000 characters with `invalid_blocks`, which is
    // terminal, which is a dead-lettered alert. Below the *block* limit the fix costs
    // nothing: the body is split and every character survives.
    let alert = AlertView {
        annotations: labels(&[("description", &"noise ".repeat(1_200))]),
        ..ceph_osd_down()
    };
    let rendered = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(FIRED_AT + 1_740));

    assert_universally_valid(&rendered.body);
    assert!(rendered.body.blocks().len() > 1, "the body must have split");
    assert_eq!(
        rendered.truncated, None,
        "under the block limit, splitting is lossless — nothing should be dropped"
    );

    // This one is snapshotted by shape rather than by content: seven kilobytes of "noise"
    // in a snapshot file would be unreviewable, which is its own way of ossifying a bug.
    let shape: Vec<String> = rendered
        .body
        .blocks()
        .iter()
        .map(|block| match block {
            Block::Section { text } => format!("section({} chars)", text.text.chars().count()),
            Block::Context { elements } => format!("context({})", elements[0].text),
        })
        .collect();
    insta::assert_json_snapshot!(shape);
}

#[test]
fn a_pathological_annotation_is_truncated_at_the_block_limit_too() {
    // The other Slack limit. 50 blocks is roughly 150 000 characters, which no human
    // writes — but a `description` built by a Prometheus template function over a large
    // label set will get there.
    let alert = AlertView {
        annotations: labels(&[("description", &"x".repeat(400_000))]),
        ..ceph_osd_down()
    };
    let rendered = Renderer::builtin().render(&RenderRequest::Firing(&alert), at(FIRED_AT));

    assert_universally_valid(&rendered.body);
    let truncation = rendered
        .truncated
        .expect("truncation must be reported to the caller");
    assert_eq!(rendered.body.blocks().len(), MAX_BLOCKS);
    assert!(truncation.dropped_chars > 0);
    assert!(truncation.dropped_blocks > 0);

    let notice = match rendered.body.blocks().last().expect("a last block") {
        Block::Context { elements } => elements[0].text.clone(),
        Block::Section { .. } => panic!("the notice must be a context block"),
    };
    assert!(notice.contains("truncated"), "{notice}");
    insta::assert_snapshot!(notice);
}
