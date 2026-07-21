//! The JSON that goes on the wire: Block Kit blocks inside a legacy attachment.
//!
//! # Why the attachment is here at all
//!
//! ADR 001 D10: the colour bar — red firing, green resolved — is the highest-signal
//! element for scanning an alert channel, and **Block Kit has no colour concept**. The
//! only way to get one is the deprecated `attachments` array with `color` set and the
//! modern `blocks` nested inside it. It is slightly inelegant and it is well-trodden
//! Slack practice; the alternative is a channel where firing and resolved look identical
//! at a glance, which defeats the point of the message.
//!
//! # Limits are enforced here, not discovered at the API
//!
//! Slack rejects a message whose section text exceeds 3000 characters, or which carries
//! more than 50 blocks. A `PrometheusRule` with a verbose `description` annotation will
//! exceed the first of those, and the failure mode is `invalid_blocks` — a
//! [`Terminal`](crate::Disposition::Terminal) error, which is to say a dead-lettered
//! alert, which is to say silence. So the limits are applied at render time, and the
//! truncation is *visible in the message* rather than being something the operator has to
//! infer from a metric.

use serde::{Deserialize, Serialize};

/// Slack's limit on the `text` of a single `section` block, in characters.
pub const MAX_SECTION_CHARS: usize = 3000;

/// Slack's limit on the number of blocks in one message (or one attachment).
pub const MAX_BLOCKS: usize = 50;

/// How much of the `text` notification preview is kept.
///
/// This is the line that appears in a Slack desktop notification and in the channel list;
/// Slack truncates it itself, but doing it here keeps the payload small and the snapshots
/// readable.
pub const MAX_NOTIFICATION_CHARS: usize = 200;

/// The colour bar down the left of the attachment (ADR 001 D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    /// An alert that is firing.
    Firing,
    /// An alert that has resolved.
    Resolved,
}

impl Colour {
    /// The hex value Slack is given.
    ///
    /// Slack also accepts the keywords `danger` and `good`, which are *theme-dependent*.
    /// Fixed hex values render identically in light mode, dark mode and on mobile, which
    /// matters for something whose only job is to be recognisable at a glance.
    pub const fn as_hex(self) -> &'static str {
        match self {
            Self::Firing => "#d40e0d",
            Self::Resolved => "#2eb886",
        }
    }
}

/// A Block Kit block.
///
/// Only the two kinds this relay emits. A larger enum would be modelling Slack's API
/// rather than this project's messages, and every variant is a thing that has to be
/// length-checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// A body paragraph.
    Section {
        /// Its text, always `mrkdwn`.
        text: Text,
    },
    /// Small, dimmed text under the body. Used for the truncation notice.
    Context {
        /// Its elements, always exactly one `mrkdwn` run here.
        elements: Vec<Text>,
    },
}

impl Block {
    /// A `section` holding `mrkdwn`.
    ///
    /// Not public: sections are only ever built by [`crate::render`], which is the code
    /// that also applies [`MAX_SECTION_CHARS`]. A public constructor would be a way to
    /// build an over-long block without going past the limit check.
    pub(crate) fn section(text: impl Into<String>) -> Self {
        Self::Section {
            text: Text::mrkdwn(text),
        }
    }

    /// A `context` holding one `mrkdwn` run.
    pub(crate) fn context(text: impl Into<String>) -> Self {
        Self::Context {
            elements: vec![Text::mrkdwn(text)],
        }
    }
}

/// A Slack composition object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    /// Always `mrkdwn`. Slack's `plain_text` cannot carry links or bold, and every
    /// message this relay sends wants at least one of those.
    ///
    /// Written on the wire and never read back from it: the only thing that deserialises
    /// these is a test or the dev mock, and neither has any business telling this crate
    /// that a block it produced was `plain_text` after all.
    #[serde(rename = "type", default = "mrkdwn", skip_deserializing)]
    pub kind: &'static str,
    /// The content.
    pub text: String,
}

const fn mrkdwn() -> &'static str {
    "mrkdwn"
}

impl Text {
    fn mrkdwn(text: impl Into<String>) -> Self {
        Self {
            kind: mrkdwn(),
            text: text.into(),
        }
    }
}

/// A legacy attachment: the colour bar, wrapped around modern blocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// The colour bar, as a hex string.
    pub color: String,
    /// Plain text for clients that cannot render blocks, and for accessibility.
    pub fallback: String,
    /// The Block Kit content.
    pub blocks: Vec<Block>,
}

/// A rendered message body, ready to be addressed to a channel.
///
/// Deliberately *not* addressed: the same body is posted by `chat.postMessage` and
/// replayed by `chat.update`, and keeping the channel and timestamp out of it means a
/// renderer cannot accidentally decide where a message goes. That is the client's job,
/// and the newtypes in `alertthread-core` are what keep it honest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    /// The notification preview, and the accessibility fallback.
    pub text: String,
    /// Exactly one attachment: the colour bar with the blocks inside it.
    pub attachments: Vec<Attachment>,
}

impl MessageBody {
    /// Assembles a body from already-limited blocks.
    pub(crate) fn new(colour: Colour, notification: String, blocks: Vec<Block>) -> Self {
        Self {
            text: notification.clone(),
            attachments: vec![Attachment {
                color: colour.as_hex().to_owned(),
                fallback: notification,
                blocks,
            }],
        }
    }

    /// The blocks inside the attachment.
    ///
    /// Used by tests and by the dev mock; the production path serialises the whole body.
    pub fn blocks(&self) -> &[Block] {
        self.attachments
            .first()
            .map_or(&[], |attachment| attachment.blocks.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::{Attachment, Block, Colour, MAX_BLOCKS, MAX_SECTION_CHARS, MessageBody, Text};

    #[test]
    fn the_limits_are_slacks_documented_ones() {
        // Both are load-bearing: exceeding either produces `invalid_blocks`, which is a
        // terminal error, which is a dead-lettered alert.
        assert_eq!(MAX_SECTION_CHARS, 3000);
        assert_eq!(MAX_BLOCKS, 50);
    }

    #[test]
    fn the_two_colours_are_distinct_fixed_hex_values() {
        // Not `danger`/`good`: those are theme-dependent, and this bar exists to be
        // recognisable at a glance in whatever theme the reader uses.
        assert_eq!(Colour::Firing.as_hex(), "#d40e0d");
        assert_eq!(Colour::Resolved.as_hex(), "#2eb886");
        assert_ne!(Colour::Firing.as_hex(), Colour::Resolved.as_hex());
        assert_eq!(format!("{:?}", Colour::Firing), "Firing");
    }

    #[test]
    fn a_section_serialises_to_the_shape_slack_documents() {
        let json = serde_json::to_value(Block::section("*hello*")).expect("block serialises");
        assert_eq!(
            json,
            serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": "*hello*" }
            })
        );
    }

    #[test]
    fn a_context_serialises_to_the_shape_slack_documents() {
        let json = serde_json::to_value(Block::context("truncated")).expect("block serialises");
        assert_eq!(
            json,
            serde_json::json!({
                "type": "context",
                "elements": [{ "type": "mrkdwn", "text": "truncated" }]
            })
        );
    }

    #[test]
    fn a_body_wraps_its_blocks_in_exactly_one_coloured_attachment() {
        // ADR 001 D10: one attachment, because the colour bar is the whole reason the
        // deprecated wrapper is here at all. Two would draw two bars.
        let body = MessageBody::new(
            Colour::Resolved,
            "RESOLVED CephOSDDown".to_owned(),
            vec![Block::section("body")],
        );
        assert_eq!(body.attachments.len(), 1);
        let attachment = body.attachments.first().expect("one attachment");
        assert_eq!(attachment.color, "#2eb886");
        assert_eq!(attachment.fallback, "RESOLVED CephOSDDown");
        assert_eq!(body.text, "RESOLVED CephOSDDown");
        assert_eq!(body.blocks(), &[Block::section("body")]);
    }

    #[test]
    fn a_body_with_no_attachment_reports_no_blocks_rather_than_panicking() {
        // `blocks()` is reachable from the dev mock and from tests, and this crate denies
        // `indexing_slicing` precisely so that a shape nobody expected degrades instead of
        // aborting the process that was about to post an alert.
        let body = MessageBody {
            text: "x".to_owned(),
            attachments: Vec::new(),
        };
        assert!(body.blocks().is_empty());
    }

    #[test]
    fn a_body_round_trips_through_json() {
        // The dev slack-mock deserialises what we post; a field that does not survive the
        // round trip is a field the mock silently drops.
        let body = MessageBody::new(
            Colour::Firing,
            "FIRING".to_owned(),
            vec![Block::section("body"), Block::context("note")],
        );
        let json = serde_json::to_string(&body).expect("body serialises");
        let back: MessageBody = serde_json::from_str(&json).expect("body deserialises");
        assert_eq!(back, body);
    }

    #[test]
    fn text_is_always_mrkdwn() {
        // `plain_text` cannot carry a link or bold, and every message here wants one.
        let text = Text::mrkdwn("x");
        assert_eq!(text.kind, "mrkdwn");
        assert_eq!(text.text, "x");
    }

    #[test]
    fn attachment_debug_shows_the_colour() {
        let attachment = Attachment {
            color: Colour::Firing.as_hex().to_owned(),
            fallback: "f".to_owned(),
            blocks: Vec::new(),
        };
        assert!(format!("{attachment:?}").contains("#d40e0d"));
    }
}
