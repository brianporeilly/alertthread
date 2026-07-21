//! How an [`Op`] is written to, and read back from, the `outbox.payload` column.
//!
//! ## Why there is a mirror type rather than `#[derive(Serialize)]` on [`Op`] itself
//!
//! `alertthread-core` is pure and holds every correctness decision in the project, and its
//! types are shaped for *deciding*, not for storage. The moment `Op` becomes serialisable
//! in the core, its field names become an on-disk format, and a rename in the core silently
//! stops a queued alert from being decodable by the process that restarts after the
//! deployment. Ops outlive the process that planned them — that is the entire point of a
//! durable outbox (ADR 001 D2) — so the format they are stored in has to be a deliberate,
//! separately tested artefact.
//!
//! The mirror is that artefact. A rename in the core breaks this file at compile time,
//! which is a conversation; without it, a rename in the core changes the format quietly,
//! which is an outage during a rolling upgrade.

use alertthread_core::{
    ChannelId, Fingerprint, GroupKey, MessageTs, Op, Placement, ResolveTarget, ThreadTs,
};
use serde::{Deserialize, Serialize};

/// Which kind of work an outbox row holds.
///
/// Stored denormalised in the `outbox.op` column so
/// `alertthread_outbox_depth{op}` (ADR 001 D11) can be reported with a `GROUP BY` rather
/// than by deserialising every queued row on every scrape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OpKind {
    /// [`Op::Post`].
    Post,
    /// [`Op::PostGroup`].
    PostGroup,
    /// [`Op::Refresh`].
    Refresh,
    /// [`Op::RefreshGroup`].
    RefreshGroup,
    /// [`Op::Resolve`].
    Resolve,
    /// [`Op::PostOrphanResolved`].
    PostOrphanResolved,
}

impl OpKind {
    /// The value stored in the `op` column, and the metric label that goes with it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::PostGroup => "post_group",
            Self::Refresh => "refresh",
            Self::RefreshGroup => "refresh_group",
            Self::Resolve => "resolve",
            Self::PostOrphanResolved => "post_orphan_resolved",
        }
    }

    /// Which kind of work this op is.
    pub const fn of(op: &Op) -> Self {
        match op {
            Op::Post { .. } => Self::Post,
            Op::PostGroup { .. } => Self::PostGroup,
            Op::Refresh { .. } => Self::Refresh,
            Op::RefreshGroup { .. } => Self::RefreshGroup,
            Op::Resolve { .. } => Self::Resolve,
            Op::PostOrphanResolved { .. } => Self::PostOrphanResolved,
        }
    }
}

impl std::fmt::Display for OpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The channel an op is addressed to.
///
/// Every op has one — the channel comes from the request, not the store (ADR 001 D8) — so
/// this is total rather than optional, and `outbox.channel` is `NOT NULL` because of it.
pub(crate) const fn channel_of(op: &Op) -> &ChannelId {
    match op {
        Op::Post { channel, .. }
        | Op::PostGroup { channel, .. }
        | Op::Refresh { channel, .. }
        | Op::RefreshGroup { channel, .. }
        | Op::Resolve { channel, .. }
        | Op::PostOrphanResolved { channel, .. } => channel,
    }
}

/// The alert an op acts on, for ops that act on one.
pub(crate) const fn fingerprint_of(op: &Op) -> Option<&Fingerprint> {
    match op {
        Op::Post { fingerprint, .. }
        | Op::Refresh { fingerprint, .. }
        | Op::Resolve { fingerprint, .. }
        | Op::PostOrphanResolved { fingerprint, .. } => Some(fingerprint),
        Op::PostGroup { .. } | Op::RefreshGroup { .. } => None,
    }
}

/// The storm-collapse group an op belongs to, for ops that belong to one.
///
/// A threaded [`Op::Post`] counts: the worker has to find its parent's timestamp, and the
/// pruner has to know the group still has queued work before deleting it.
pub(crate) const fn group_key_of(op: &Op) -> Option<&GroupKey> {
    match op {
        Op::PostGroup { group_key, .. }
        | Op::RefreshGroup { group_key, .. }
        | Op::Post {
            placement: Placement::Thread { group_key, .. },
            ..
        } => Some(group_key),
        Op::Post {
            placement: Placement::Channel,
            ..
        }
        | Op::Refresh { .. }
        | Op::Resolve { .. }
        | Op::PostOrphanResolved { .. } => None,
    }
}

/// The on-disk form of an [`Op`].
///
/// `tag = "kind"` gives a self-describing document, so an operator reading a stuck row out
/// of the database sees what the work is without a decoder ring. Field names are chosen to
/// match the core's, but that is a courtesy to the reader rather than a coupling: the
/// conversion below is explicit in both directions, and the round-trip test is what holds
/// the format still.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum StoredOp {
    Post {
        fingerprint: Fingerprint,
        channel: ChannelId,
        placement: StoredPlacement,
    },
    PostGroup {
        group_key: GroupKey,
        channel: ChannelId,
        initial_members: usize,
    },
    Refresh {
        fingerprint: Fingerprint,
        channel: ChannelId,
        message_ts: MessageTs,
    },
    RefreshGroup {
        group_key: GroupKey,
        channel: ChannelId,
        message_ts: ThreadTs,
    },
    Resolve {
        fingerprint: Fingerprint,
        channel: ChannelId,
        target: StoredResolveTarget,
        update_in_place: bool,
        thread_reply: bool,
    },
    PostOrphanResolved {
        fingerprint: Fingerprint,
        channel: ChannelId,
    },
}

/// The on-disk form of [`Placement`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "where", rename_all = "snake_case")]
pub(crate) enum StoredPlacement {
    Channel,
    Thread {
        group_key: GroupKey,
        parent_ts: Option<ThreadTs>,
    },
}

/// The on-disk form of [`ResolveTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub(crate) enum StoredResolveTarget {
    Message {
        message_ts: MessageTs,
        thread_parent_ts: Option<ThreadTs>,
    },
    AwaitingPost,
}

impl From<&Op> for StoredOp {
    fn from(op: &Op) -> Self {
        match op {
            Op::Post {
                fingerprint,
                channel,
                placement,
            } => Self::Post {
                fingerprint: fingerprint.clone(),
                channel: channel.clone(),
                placement: placement.into(),
            },
            Op::PostGroup {
                group_key,
                channel,
                initial_members,
            } => Self::PostGroup {
                group_key: group_key.clone(),
                channel: channel.clone(),
                initial_members: *initial_members,
            },
            Op::Refresh {
                fingerprint,
                channel,
                message_ts,
            } => Self::Refresh {
                fingerprint: fingerprint.clone(),
                channel: channel.clone(),
                message_ts: message_ts.clone(),
            },
            Op::RefreshGroup {
                group_key,
                channel,
                message_ts,
            } => Self::RefreshGroup {
                group_key: group_key.clone(),
                channel: channel.clone(),
                message_ts: message_ts.clone(),
            },
            Op::Resolve {
                fingerprint,
                channel,
                target,
                update_in_place,
                thread_reply,
            } => Self::Resolve {
                fingerprint: fingerprint.clone(),
                channel: channel.clone(),
                target: target.into(),
                update_in_place: *update_in_place,
                thread_reply: *thread_reply,
            },
            Op::PostOrphanResolved {
                fingerprint,
                channel,
            } => Self::PostOrphanResolved {
                fingerprint: fingerprint.clone(),
                channel: channel.clone(),
            },
        }
    }
}

impl From<StoredOp> for Op {
    fn from(stored: StoredOp) -> Self {
        match stored {
            StoredOp::Post {
                fingerprint,
                channel,
                placement,
            } => Self::Post {
                fingerprint,
                channel,
                placement: placement.into(),
            },
            StoredOp::PostGroup {
                group_key,
                channel,
                initial_members,
            } => Self::PostGroup {
                group_key,
                channel,
                initial_members,
            },
            StoredOp::Refresh {
                fingerprint,
                channel,
                message_ts,
            } => Self::Refresh {
                fingerprint,
                channel,
                message_ts,
            },
            StoredOp::RefreshGroup {
                group_key,
                channel,
                message_ts,
            } => Self::RefreshGroup {
                group_key,
                channel,
                message_ts,
            },
            StoredOp::Resolve {
                fingerprint,
                channel,
                target,
                update_in_place,
                thread_reply,
            } => Self::Resolve {
                fingerprint,
                channel,
                target: target.into(),
                update_in_place,
                thread_reply,
            },
            StoredOp::PostOrphanResolved {
                fingerprint,
                channel,
            } => Self::PostOrphanResolved {
                fingerprint,
                channel,
            },
        }
    }
}

impl From<&Placement> for StoredPlacement {
    fn from(placement: &Placement) -> Self {
        match placement {
            Placement::Channel => Self::Channel,
            Placement::Thread {
                group_key,
                parent_ts,
            } => Self::Thread {
                group_key: group_key.clone(),
                parent_ts: parent_ts.clone(),
            },
        }
    }
}

impl From<StoredPlacement> for Placement {
    fn from(stored: StoredPlacement) -> Self {
        match stored {
            StoredPlacement::Channel => Self::Channel,
            StoredPlacement::Thread {
                group_key,
                parent_ts,
            } => Self::Thread {
                group_key,
                parent_ts,
            },
        }
    }
}

impl From<&ResolveTarget> for StoredResolveTarget {
    fn from(target: &ResolveTarget) -> Self {
        match target {
            ResolveTarget::Message {
                message_ts,
                thread_parent_ts,
            } => Self::Message {
                message_ts: message_ts.clone(),
                thread_parent_ts: thread_parent_ts.clone(),
            },
            ResolveTarget::AwaitingPost => Self::AwaitingPost,
        }
    }
}

impl From<StoredResolveTarget> for ResolveTarget {
    fn from(stored: StoredResolveTarget) -> Self {
        match stored {
            StoredResolveTarget::Message {
                message_ts,
                thread_parent_ts,
            } => Self::Message {
                message_ts,
                thread_parent_ts,
            },
            StoredResolveTarget::AwaitingPost => Self::AwaitingPost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OpKind, StoredOp, channel_of, fingerprint_of, group_key_of};
    use alertthread_core::{
        ChannelId, Fingerprint, GroupKey, MessageTs, Op, Placement, ResolveTarget, ThreadTs,
    };

    fn channel() -> ChannelId {
        ChannelId::new("#alerts")
    }

    /// One of every op the planner can emit, including both placements and both resolve
    /// targets. Anything added to `Op` in the core makes the `From` impls above fail to
    /// compile, and this list is what makes sure the new variant is also *exercised*.
    fn every_op() -> Vec<Op> {
        vec![
            Op::Post {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                placement: Placement::Channel,
            },
            Op::Post {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                placement: Placement::Thread {
                    group_key: GroupKey::new("gk"),
                    parent_ts: None,
                },
            },
            Op::Post {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                placement: Placement::Thread {
                    group_key: GroupKey::new("gk"),
                    parent_ts: Some(ThreadTs::new("1721500000.000001")),
                },
            },
            Op::PostGroup {
                group_key: GroupKey::new("gk"),
                channel: channel(),
                initial_members: 6,
            },
            Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                message_ts: MessageTs::new("1721500000.000100"),
            },
            Op::RefreshGroup {
                group_key: GroupKey::new("gk"),
                channel: channel(),
                message_ts: ThreadTs::new("1721500000.000001"),
            },
            Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::AwaitingPost,
                update_in_place: true,
                thread_reply: false,
            },
            Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1721500000.000100"),
                    thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
                },
                update_in_place: false,
                thread_reply: true,
            },
            Op::PostOrphanResolved {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
            },
        ]
    }

    #[test]
    fn every_op_survives_a_round_trip_through_json() {
        // An op that does not come back out of the database identical is an alert that
        // reaches Slack as something other than what was planned — or, if the decode
        // fails, not at all.
        for op in every_op() {
            let json = serde_json::to_string(&StoredOp::from(&op)).expect("op serialises");
            let decoded: StoredOp = serde_json::from_str(&json).expect("op deserialises");
            assert_eq!(Op::from(decoded), op, "{json}");
        }
    }

    #[test]
    fn the_stored_format_is_the_one_this_test_pins() {
        // Ops outlive the process that planned them, so this JSON *is* a compatibility
        // surface: a rolling upgrade has the new binary draining rows the old one wrote.
        // Changing this string is changing a format, and it should require editing a test
        // that says so.
        let op = Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: ChannelId::new("#alerts"),
            placement: Placement::Thread {
                group_key: GroupKey::new("gk"),
                parent_ts: Some(ThreadTs::new("1721500000.000001")),
            },
        };
        assert_eq!(
            serde_json::to_string(&StoredOp::from(&op)).expect("op serialises"),
            r##"{"kind":"post","fingerprint":"abc","channel":"#alerts","placement":{"where":"thread","group_key":"gk","parent_ts":"1721500000.000001"}}"##
        );
    }

    #[test]
    fn a_resolve_pins_its_format_too() {
        let op = Op::Resolve {
            fingerprint: Fingerprint::new("abc"),
            channel: ChannelId::new("#alerts"),
            target: ResolveTarget::AwaitingPost,
            update_in_place: true,
            thread_reply: true,
        };
        assert_eq!(
            serde_json::to_string(&StoredOp::from(&op)).expect("op serialises"),
            r##"{"kind":"resolve","fingerprint":"abc","channel":"#alerts","target":{"target":"awaiting_post"},"update_in_place":true,"thread_reply":true}"##
        );
    }

    #[test]
    fn a_payload_this_build_does_not_understand_fails_to_decode() {
        // The alternative — a lenient decode that yields something plausible — would turn
        // a downgrade into wrong messages rather than a loud error.
        let error = serde_json::from_str::<StoredOp>(r#"{"kind":"send_carrier_pigeon"}"#)
            .expect_err("an unknown op kind must not decode");
        assert!(error.to_string().contains("send_carrier_pigeon"), "{error}");
    }

    #[test]
    fn every_op_kind_has_a_distinct_column_value() {
        let kinds = [
            OpKind::Post,
            OpKind::PostGroup,
            OpKind::Refresh,
            OpKind::RefreshGroup,
            OpKind::Resolve,
            OpKind::PostOrphanResolved,
        ];
        let mut seen: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), kinds.len(), "two op kinds share a column value");
    }

    #[test]
    fn op_kind_names_match_the_ops_they_classify() {
        assert_eq!(
            OpKind::of(&Op::PostGroup {
                group_key: GroupKey::new("gk"),
                channel: channel(),
                initial_members: 1,
            }),
            OpKind::PostGroup
        );
        assert_eq!(OpKind::PostGroup.to_string(), "post_group");
        assert_eq!(format!("{:?}", OpKind::Resolve), "Resolve");
    }

    #[test]
    fn every_op_classifies_to_some_kind() {
        for op in every_op() {
            let kind = OpKind::of(&op);
            assert!(!kind.as_str().is_empty(), "{op:?}");
        }
    }

    #[test]
    fn every_op_is_addressed_to_a_channel() {
        // `outbox.channel` is NOT NULL, and the rate limiter is per-channel, so an op that
        // could not name one would be unschedulable.
        for op in every_op() {
            assert_eq!(channel_of(&op), &channel(), "{op:?}");
        }
    }

    #[test]
    fn only_alert_scoped_ops_denormalise_a_fingerprint() {
        for op in every_op() {
            let expected = match &op {
                Op::PostGroup { .. } | Op::RefreshGroup { .. } => None,
                _ => Some(Fingerprint::new("abc")),
            };
            assert_eq!(fingerprint_of(&op).cloned(), expected, "{op:?}");
        }
    }

    #[test]
    fn a_threaded_post_denormalises_its_group_but_a_top_level_one_does_not() {
        // The pruner refuses to delete a group that still has queued work, and a threaded
        // child is queued work for its group. A child that did not record its group would
        // let the parent be pruned out from under it.
        for op in every_op() {
            let expected = match &op {
                Op::PostGroup { .. } | Op::RefreshGroup { .. } => Some(GroupKey::new("gk")),
                Op::Post {
                    placement: Placement::Thread { group_key, .. },
                    ..
                } => Some(group_key.clone()),
                _ => None,
            };
            assert_eq!(group_key_of(&op).cloned(), expected, "{op:?}");
        }
    }
}
