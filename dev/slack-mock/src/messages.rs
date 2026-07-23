//! The message store: channels, messages, and the Slack behaviours that decide
//! where a threaded reply actually lands.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// A Slack API error code, returned inside an HTTP 200 exactly as Slack does.
pub(crate) type ApiError = &'static str;

/// A Block Kit block, in the two shapes `alertthread-slack` emits.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Block {
    /// A body paragraph.
    Section {
        /// Its text, always `mrkdwn`.
        text: Text,
    },
    /// Dimmed text under the body.
    Context {
        /// Its runs, one in practice.
        elements: Vec<Text>,
    },
    /// Any other kind. Kept rather than rejected: a fake that 400s on a block it
    /// has not met yet would report a rendering change as a delivery failure.
    #[serde(other)]
    Other,
}

impl Block {
    /// The block's `mrkdwn`, as one string.
    fn mrkdwn(&self) -> String {
        match self {
            Self::Section { text } => text.text.clone(),
            Self::Context { elements } => elements
                .iter()
                .map(|element| element.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            Self::Other => String::new(),
        }
    }
}

/// A Slack composition object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Text {
    /// `mrkdwn` or `plain_text`.
    #[serde(rename = "type", default)]
    pub(crate) kind: String,
    /// The content.
    #[serde(default)]
    pub(crate) text: String,
}

/// The legacy attachment that carries the colour bar (ADR 001 D10).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Attachment {
    /// The colour bar, as a hex string.
    #[serde(default)]
    pub(crate) color: String,
    /// Plain text for clients that cannot render blocks.
    #[serde(default)]
    pub(crate) fallback: String,
    /// The Block Kit content.
    #[serde(default)]
    pub(crate) blocks: Vec<Block>,
}

/// A `chat.postMessage` request, as the relay sends it.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PostRequest {
    /// A `#name` or a `C…` ID.
    #[serde(default)]
    pub(crate) channel: String,
    /// The message to reply under, if any.
    #[serde(default)]
    pub(crate) thread_ts: Option<String>,
    /// Whether the reply is echoed into the channel. Always `false` here.
    #[serde(default)]
    pub(crate) reply_broadcast: bool,
    /// The notification preview.
    #[serde(default)]
    pub(crate) text: String,
    /// The attachments, of which the relay sends exactly one.
    #[serde(default)]
    pub(crate) attachments: Vec<Attachment>,
}

/// A `chat.update` request.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct UpdateRequest {
    /// The channel the message lives in.
    #[serde(default)]
    pub(crate) channel: String,
    /// The message to rewrite.
    #[serde(default)]
    pub(crate) ts: String,
    /// Its replacement preview text.
    #[serde(default)]
    pub(crate) text: String,
    /// Its replacement attachments.
    #[serde(default)]
    pub(crate) attachments: Vec<Attachment>,
}

/// What the API reports about a message it accepted.
#[derive(Clone, Debug)]
pub(crate) struct Posted {
    /// The canonical channel ID, which is what Slack echoes back.
    pub(crate) channel: String,
    /// The message timestamp.
    pub(crate) ts: String,
}

/// One stored message.
#[derive(Clone, Debug)]
struct Message {
    ts: String,
    /// The thread this message is in — the *root*, never another reply.
    thread_ts: Option<String>,
    /// The `thread_ts` the caller asked for, which may have been a reply's.
    reply_to: Option<String>,
    reply_broadcast: bool,
    text: String,
    attachments: Vec<Attachment>,
    posted_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
    edits: u32,
}

/// One channel and everything posted to it, oldest first.
#[derive(Clone, Debug)]
struct Channel {
    id: String,
    name: String,
    messages: Vec<Message>,
}

/// Every channel this fake workspace has seen.
#[derive(Clone, Debug, Default)]
pub(crate) struct Workspace {
    channels: BTreeMap<String, Channel>,
    sequence: u64,
}

impl Workspace {
    /// `chat.postMessage`.
    ///
    /// # Errors
    ///
    /// `channel_not_found` for a blank channel, `thread_not_found` when
    /// `thread_ts` names no message here.
    pub(crate) fn post(
        &mut self,
        request: &PostRequest,
        now: DateTime<Utc>,
    ) -> Result<Posted, ApiError> {
        let name = request.channel.trim();
        if name.is_empty() {
            return Err("channel_not_found");
        }
        let id = self.open(name);

        let (thread_ts, reply_to) = match request.thread_ts.as_deref() {
            None => (None, None),
            Some(requested) => (
                Some(self.thread_root(&id, requested)?),
                Some(requested.to_owned()),
            ),
        };

        let ts = self.next_ts(now);
        let message = Message {
            ts: ts.clone(),
            thread_ts,
            reply_to,
            reply_broadcast: request.reply_broadcast,
            text: request.text.clone(),
            attachments: request.attachments.clone(),
            posted_at: now,
            updated_at: None,
            edits: 0,
        };
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.messages.push(message);
        }

        Ok(Posted { channel: id, ts })
    }

    /// `chat.update`.
    ///
    /// # Errors
    ///
    /// `channel_not_found` or `message_not_found` — the latter is ADR 001 D9's
    /// liveness probe, so it has to be a real answer and not a panic.
    pub(crate) fn update(
        &mut self,
        request: &UpdateRequest,
        now: DateTime<Utc>,
    ) -> Result<Posted, ApiError> {
        let id = self.resolve(request.channel.trim())?;
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or::<ApiError>("channel_not_found")?;
        let message = channel
            .messages
            .iter_mut()
            .find(|message| message.ts == request.ts)
            .ok_or::<ApiError>("message_not_found")?;

        message.text.clone_from(&request.text);
        message.attachments.clone_from(&request.attachments);
        message.updated_at = Some(now);
        message.edits += 1;

        Ok(Posted {
            channel: id,
            ts: request.ts.clone(),
        })
    }

    /// The whole store, for the UI and for the end-to-end assertions.
    pub(crate) fn view(&self) -> WorkspaceView {
        WorkspaceView {
            message_count: self
                .channels
                .values()
                .map(|channel| channel.messages.len())
                .sum(),
            channels: self
                .channels
                .values()
                .map(|channel| ChannelView {
                    id: channel.id.clone(),
                    name: channel.name.clone(),
                    message_count: channel.messages.len(),
                    messages: channel
                        .messages
                        .iter()
                        .filter(|message| message.thread_ts.is_none())
                        .map(|parent| {
                            let mut view = MessageView::of(parent);
                            view.replies = channel
                                .messages
                                .iter()
                                .filter(|reply| reply.thread_ts.as_ref() == Some(&parent.ts))
                                .map(MessageView::of)
                                .collect();
                            view
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Returns the channel's canonical ID, opening it if it is new.
    fn open(&mut self, key: &str) -> String {
        if self.channels.contains_key(key) {
            return key.to_owned();
        }
        let id = canonical_id(key);
        self.channels.entry(id.clone()).or_insert_with(|| Channel {
            id: id.clone(),
            name: key.to_owned(),
            messages: Vec::new(),
        });
        id
    }

    /// Resolves a `#name` or a `C…` ID to a channel that already exists.
    fn resolve(&self, key: &str) -> Result<String, ApiError> {
        if self.channels.contains_key(key) {
            return Ok(key.to_owned());
        }
        let id = canonical_id(key);
        if self.channels.contains_key(&id) {
            return Ok(id);
        }
        Err("channel_not_found")
    }

    /// The thread a reply to `requested` actually lands in.
    ///
    /// Slack does not nest threads: replying to a reply puts the message in that
    /// reply's own thread. Reproducing that is the point — flattening here is
    /// what shows a resolve reply for a collapsed child landing in the group
    /// thread rather than under the child (ADR 001 D5 meeting D6).
    fn thread_root(&self, id: &str, requested: &str) -> Result<String, ApiError> {
        let parent = self
            .channels
            .get(id)
            .and_then(|channel| {
                channel
                    .messages
                    .iter()
                    .find(|message| message.ts == requested)
            })
            .ok_or::<ApiError>("thread_not_found")?;
        Ok(parent
            .thread_ts
            .clone()
            .unwrap_or_else(|| parent.ts.clone()))
    }

    /// The next `seconds.microseconds` timestamp, unique and increasing.
    fn next_ts(&mut self, now: DateTime<Utc>) -> String {
        self.sequence = self.sequence.wrapping_add(1);
        format!("{}.{:06}", now.timestamp(), self.sequence % 1_000_000)
    }
}

/// A stable `C…` ID for a channel name.
///
/// Slack echoes a canonical ID from `chat.postMessage` and the relay addresses
/// later `chat.update` calls with it, so a mock that echoed `#alerts` back would
/// never exercise that path.
fn canonical_id(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("C{:010X}", hash & 0xFF_FFFF_FFFF)
}

/// The whole store, as `/api/state` serves it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkspaceView {
    /// Every message in every channel, replies included.
    pub(crate) message_count: usize,
    /// The channels, by canonical ID.
    pub(crate) channels: Vec<ChannelView>,
}

/// One channel's top-level messages, each carrying its thread.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChannelView {
    /// The canonical `C…` ID.
    pub(crate) id: String,
    /// The name the first post addressed.
    pub(crate) name: String,
    /// Top-level messages plus replies.
    pub(crate) message_count: usize,
    /// Top-level messages, oldest first.
    pub(crate) messages: Vec<MessageView>,
}

/// One message, with its thread when it has one.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MessageView {
    /// The timestamp `chat.update` addresses it by.
    pub(crate) ts: String,
    /// The notification preview.
    pub(crate) text: String,
    /// The attachment colour: red firing, green resolved.
    pub(crate) color: String,
    /// The attachment fallback text.
    pub(crate) fallback: String,
    /// Each block's `mrkdwn`, in order.
    pub(crate) blocks: Vec<String>,
    /// The thread root, when this is a reply.
    pub(crate) thread_ts: Option<String>,
    /// The message the caller asked to reply under, before Slack flattened it.
    pub(crate) reply_to: Option<String>,
    /// Whether the reply was echoed into the channel.
    pub(crate) reply_broadcast: bool,
    /// Whether `chat.update` has rewritten this message.
    pub(crate) edited: bool,
    /// How many times.
    pub(crate) edits: u32,
    /// When it was posted, RFC 3339.
    pub(crate) posted_at: String,
    /// When it was last edited, RFC 3339.
    pub(crate) updated_at: Option<String>,
    /// Its thread, oldest first. Always empty on a reply.
    pub(crate) replies: Vec<MessageView>,
}

impl MessageView {
    fn of(message: &Message) -> Self {
        let attachment = message.attachments.first();
        Self {
            ts: message.ts.clone(),
            text: message.text.clone(),
            color: attachment.map(|a| a.color.clone()).unwrap_or_default(),
            fallback: attachment.map(|a| a.fallback.clone()).unwrap_or_default(),
            blocks: attachment
                .map(|a| a.blocks.iter().map(Block::mrkdwn).collect())
                .unwrap_or_default(),
            thread_ts: message.thread_ts.clone(),
            reply_to: message.reply_to.clone(),
            reply_broadcast: message.reply_broadcast,
            edited: message.edits > 0,
            edits: message.edits,
            posted_at: message.posted_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            updated_at: message
                .updated_at
                .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            replies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{Attachment, PostRequest, UpdateRequest, Workspace, canonical_id};

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_784_642_520, 0).unwrap_or_default()
    }

    fn post(channel: &str, text: &str, thread_ts: Option<&str>) -> PostRequest {
        PostRequest {
            channel: channel.to_owned(),
            thread_ts: thread_ts.map(ToOwned::to_owned),
            reply_broadcast: false,
            text: text.to_owned(),
            attachments: vec![Attachment {
                color: "#d40e0d".to_owned(),
                fallback: text.to_owned(),
                blocks: Vec::new(),
            }],
        }
    }

    #[test]
    fn a_post_is_answered_with_a_canonical_channel_id_not_the_name_it_addressed() {
        let mut workspace = Workspace::default();
        let posted = workspace
            .post(&post("#alerts", "firing", None), now())
            .expect("the post succeeds");

        assert_eq!(posted.channel, canonical_id("#alerts"));
        assert_ne!(posted.channel, "#alerts");
    }

    #[test]
    fn an_update_addressed_by_the_canonical_id_finds_the_message_posted_by_name() {
        // The relay stores the ID Slack echoed and addresses `chat.update` with
        // it. A mock that could not resolve it back would fail every resolve.
        let mut workspace = Workspace::default();
        let posted = workspace
            .post(&post("#alerts", "firing", None), now())
            .expect("the post succeeds");

        workspace
            .update(
                &UpdateRequest {
                    channel: posted.channel.clone(),
                    ts: posted.ts.clone(),
                    text: "resolved".to_owned(),
                    attachments: Vec::new(),
                },
                now(),
            )
            .expect("the update succeeds");

        let view = workspace.view();
        let message = &view.channels[0].messages[0];
        assert_eq!(message.text, "resolved");
        assert!(message.edited);
        assert_eq!(message.edits, 1);
        assert!(message.updated_at.is_some());
    }

    #[test]
    fn updating_a_timestamp_nobody_posted_is_message_not_found() {
        let mut workspace = Workspace::default();
        workspace
            .post(&post("#alerts", "firing", None), now())
            .expect("the post succeeds");

        let error = workspace
            .update(
                &UpdateRequest {
                    channel: "#alerts".to_owned(),
                    ts: "1.000001".to_owned(),
                    ..UpdateRequest::default()
                },
                now(),
            )
            .expect_err("the timestamp does not exist");
        assert_eq!(error, "message_not_found");
    }

    #[test]
    fn updating_a_channel_nobody_posted_to_is_channel_not_found() {
        let mut workspace = Workspace::default();
        let error = workspace
            .update(&UpdateRequest::default(), now())
            .expect_err("there are no channels");
        assert_eq!(error, "channel_not_found");
    }

    #[test]
    fn a_post_with_no_channel_is_channel_not_found() {
        let mut workspace = Workspace::default();
        let error = workspace
            .post(&post("   ", "firing", None), now())
            .expect_err("a blank channel is not a channel");
        assert_eq!(error, "channel_not_found");
    }

    #[test]
    fn a_reply_is_threaded_under_its_parent_and_not_posted_top_level() {
        let mut workspace = Workspace::default();
        let parent = workspace
            .post(&post("#alerts", "group summary", None), now())
            .expect("the post succeeds");
        workspace
            .post(&post("#alerts", "child", Some(&parent.ts)), now())
            .expect("the reply succeeds");

        let view = workspace.view();
        assert_eq!(view.channels[0].messages.len(), 1);
        assert_eq!(view.channels[0].message_count, 2);
        assert_eq!(view.channels[0].messages[0].replies.len(), 1);
        assert_eq!(
            view.channels[0].messages[0].replies[0].thread_ts.as_deref(),
            Some(parent.ts.as_str())
        );
    }

    #[test]
    fn replying_to_a_reply_lands_in_the_same_thread_and_records_what_was_asked_for() {
        // Slack does not nest threads. The relay's resolve reply for a collapsed
        // child asks to reply under the child; it arrives in the group thread.
        let mut workspace = Workspace::default();
        let parent = workspace
            .post(&post("#alerts", "group summary", None), now())
            .expect("the post succeeds");
        let child = workspace
            .post(&post("#alerts", "child", Some(&parent.ts)), now())
            .expect("the reply succeeds");
        workspace
            .post(&post("#alerts", "resolved", Some(&child.ts)), now())
            .expect("the reply to a reply succeeds");

        let view = workspace.view();
        let replies = &view.channels[0].messages[0].replies;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1].thread_ts.as_deref(), Some(parent.ts.as_str()));
        assert_eq!(replies[1].reply_to.as_deref(), Some(child.ts.as_str()));
    }

    #[test]
    fn replying_under_a_message_that_does_not_exist_is_thread_not_found() {
        let mut workspace = Workspace::default();
        let error = workspace
            .post(&post("#alerts", "orphan", Some("1.000001")), now())
            .expect_err("there is nothing to reply to");
        assert_eq!(error, "thread_not_found");
    }

    #[test]
    fn every_timestamp_is_distinct_even_within_one_second() {
        let mut workspace = Workspace::default();
        let first = workspace
            .post(&post("#alerts", "one", None), now())
            .expect("the post succeeds");
        let second = workspace
            .post(&post("#alerts", "two", None), now())
            .expect("the post succeeds");
        assert_ne!(first.ts, second.ts);
    }

    #[test]
    fn a_channel_id_is_stable_and_slack_shaped() {
        assert_eq!(canonical_id("#alerts"), canonical_id("#alerts"));
        assert_ne!(canonical_id("#alerts"), canonical_id("#alerts-critical"));
        assert_eq!(canonical_id("#alerts").len(), 11);
        assert!(canonical_id("#alerts").starts_with('C'));
    }

    #[test]
    fn block_text_is_extracted_from_both_kinds_and_survives_an_unknown_one() {
        let mut workspace = Workspace::default();
        let mut request = post("#alerts", "firing", None);
        request.attachments[0].blocks = serde_json::from_str(
            r#"[
                {"type": "section", "text": {"type": "mrkdwn", "text": "body"}},
                {"type": "context", "elements": [{"type": "mrkdwn", "text": "note"}]},
                {"type": "divider"}
            ]"#,
        )
        .expect("the blocks parse");
        workspace.post(&request, now()).expect("the post succeeds");

        let view = workspace.view();
        assert_eq!(
            view.channels[0].messages[0].blocks,
            vec!["body".to_owned(), "note".to_owned(), String::new()]
        );
    }
}
