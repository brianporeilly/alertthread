//! What the planner consumes and what it produces.
//!
//! The split here follows the sequence in ROADMAP.md's "shape that makes this work":
//! the shell executes the atomic claims first, because their correctness *is* the
//! database's atomicity and cannot be made pure; it then hands the results to
//! [`plan`](crate::plan), which is where every actual decision lives.
//!
//! - [`AlertBatch`] is what Alertmanager sent, with the channel resolved onto it.
//! - [`ClaimOutcome`] is one alert paired with what the store said about it.
//! - [`GroupState`] is the storm-collapse state the shell looked up for this group.
//! - [`Plan`] is the answer: [`Op`]s to persist, and [`Notice`]s to log and count.

use chrono::{DateTime, Utc};

use crate::ids::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};
use crate::webhook::{WebhookAlert, WebhookPayload};

/// One webhook delivery, once the destination channel is known.
///
/// The channel is not part of Alertmanager's payload — it comes from the `?channel=`
/// query parameter or the configured default (ADR 001 D8) — so it is attached here rather
/// than in [`WebhookPayload`]. One delivery is therefore always exactly one channel and
/// exactly one group key, which is what makes the collapse decision in
/// [`plan`](crate::plan) a per-batch question rather than a grouping problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertBatch {
    /// Where these alerts are going.
    pub channel: ChannelId,
    /// Alertmanager's key for the group this delivery represents.
    pub group_key: GroupKey,
    /// How many alerts Alertmanager trimmed from this body because of `max_alerts`.
    pub truncated_alerts: u64,
    /// The alerts that did arrive.
    pub alerts: Vec<WebhookAlert>,
}

impl AlertBatch {
    /// Attaches a resolved channel to a parsed webhook body.
    pub fn from_webhook(payload: WebhookPayload, channel: ChannelId) -> Self {
        Self {
            channel,
            group_key: payload.group_key,
            truncated_alerts: payload.truncated_alerts,
            alerts: payload.alerts,
        }
    }
}

/// What the atomic claim (ADR 001 D3) did for one alert.
///
/// Every variant corresponds to a row of D2's ingest classification or D3's concurrency
/// table, so the planner's job reduces to a total match over this enum. That is the point
/// of doing the claims first: the hard concurrency question is answered by the database,
/// and what reaches the core is a fact rather than a race.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimResult {
    /// `INSERT ... ON CONFLICT DO NOTHING` returned a row: we created it, so we own the
    /// notification for this fingerprint in this channel.
    Claimed,
    /// The row already existed in state `claimed`. Another replica, or a retried delivery
    /// of this same batch, is already posting it.
    AlreadyClaimed,
    /// The row already existed in state `posted`.
    ///
    /// `last_seen` is the value the store held *before* this ingest updated it, which is
    /// what the repeat-debounce of ADR 001 D7 compares against. `message_ts` is not
    /// optional here: state `posted` is only reached by a successful post, so a `posted`
    /// row without a timestamp is a state the store cannot produce.
    AlreadyPosted {
        /// When this fingerprint was last seen, before this delivery.
        last_seen: DateTime<Utc>,
        /// The Slack timestamp of the message to refresh.
        message_ts: MessageTs,
    },
    /// `UPDATE ... SET state = 'resolving'` matched a row.
    ///
    /// `message_ts` is optional because the update matches `state IN ('claimed',
    /// 'posted')`, and a `claimed` row has not been posted yet — that is exactly ADR 001
    /// D9's "resolve arrives while `message_ts` is `NULL`" row.
    Resolving {
        /// The message to edit, if it has been posted.
        message_ts: Option<MessageTs>,
        /// The storm-collapse parent this alert's message hangs under, if any.
        thread_parent_ts: Option<ThreadTs>,
    },
    /// A `resolved` alert for a fingerprint with no row at all.
    ///
    /// PRD §5.5 and ADR 001 D9: the relay was down when the alert fired, or its state was
    /// lost, or `max_alerts` truncated the firing notification out of the webhook body.
    /// Never silent — this becomes a standalone post.
    Orphan,
    /// A `resolved` alert whose row is already `resolving` or `resolved`: a duplicate
    /// delivery of a resolution we have already accepted.
    AlreadyResolving,
}

/// One alert from the batch, paired with what the store said when the shell claimed it.
///
/// Pairing happens in the shell, where the claim is actually executed, so the core never
/// has to correlate two parallel slices — which matters because a batch may legitimately
/// contain the same fingerprint twice, making a fingerprint-keyed lookup ambiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimOutcome {
    /// The alert as Alertmanager sent it.
    pub alert: WebhookAlert,
    /// What the claim did.
    pub result: ClaimResult,
}

impl ClaimOutcome {
    /// Pairs an alert with its claim result.
    pub fn new(alert: WebhookAlert, result: ClaimResult) -> Self {
        Self { alert, result }
    }
}

/// The storm-collapse state the shell found for this batch's `(group_key, channel)`.
///
/// `Some(_)` means a `group_message` row exists, and is what makes collapse **sticky**
/// (ADR 001 D5): once a group has a parent, later alerts joining it thread underneath
/// even when they arrive in a batch far too small to trigger collapse on its own.
/// Without that, a group's alerts would be split between top-level messages and thread
/// replies depending on batch timing, which is worse than either consistent behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupState {
    /// The group this parent belongs to.
    pub group_key: GroupKey,
    /// The parent message's timestamp, or `None` if its post has not succeeded yet.
    pub message_ts: Option<ThreadTs>,
}

/// Where a newly posted alert message goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A top-level message in the channel.
    Channel,
    /// A reply threaded under a storm-collapse parent.
    Thread {
        /// The group whose parent to thread under.
        group_key: GroupKey,
        /// The parent's timestamp when it is already known.
        ///
        /// `None` when the parent is being posted by this same plan and therefore has no
        /// timestamp yet. The worker resolves it from the `group_message` row, deferring
        /// with backoff until the parent's own post completes — the same self-deferral
        /// ADR 001 D2 uses for resolve-before-post ordering, rather than a second
        /// mechanism doing the same job.
        parent_ts: Option<ThreadTs>,
    },
}

/// Which message a resolve is going to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveTarget {
    /// The alert's message exists and can be edited and replied to.
    Message {
        /// The message to edit.
        message_ts: MessageTs,
        /// Its storm-collapse parent, if it was threaded.
        thread_parent_ts: Option<ThreadTs>,
    },
    /// The alert is claimed but its post has not landed, so there is nothing to edit yet.
    ///
    /// ADR 001 D9: the worker self-defers with backoff, and posts a standalone resolved
    /// message if the underlying post never succeeds. Modelling this as a distinct
    /// variant rather than an `Option<MessageTs>` is what keeps "update a message we have
    /// not posted yet" from being expressible at all.
    AwaitingPost,
}

/// A unit of work for the outbox.
///
/// These are intentions, not Slack calls. They are persisted in the same transaction as
/// the claim that produced them (ADR 001 D2) and drained by workers afterwards, which is
/// why none of them carries rendered message content: the worker reads the alert's row at
/// send time, so a template change does not require rewriting queued work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Post the storm-collapse parent for a group (ADR 001 D5).
    ///
    /// Emitted before the children so the summary appears within a second while the
    /// children fill in at Slack's one-per-second-per-channel rate.
    PostGroup {
        /// The group being collapsed.
        group_key: GroupKey,
        /// Where to post it.
        channel: ChannelId,
        /// How many alerts in this batch are joining the new parent.
        initial_members: usize,
    },
    /// Post the first message for a newly claimed alert.
    Post {
        /// The alert.
        fingerprint: Fingerprint,
        /// Where to post it.
        channel: ChannelId,
        /// Top-level, or threaded under a group parent.
        placement: Placement,
    },
    /// Refresh an already-posted firing message in place (ADR 001 D7).
    ///
    /// No thread reply and no re-post: the message stays where it is in history, and the
    /// `chat.update` doubles as a liveness probe on our own correlation state, because
    /// `message_not_found` tells us the timestamp is stale.
    Refresh {
        /// The alert.
        fingerprint: Fingerprint,
        /// Where its message lives.
        channel: ChannelId,
        /// The message to edit.
        message_ts: MessageTs,
    },
    /// Mark a tracked alert resolved (ADR 001 D6).
    Resolve {
        /// The alert.
        fingerprint: Fingerprint,
        /// Where its message lives.
        channel: ChannelId,
        /// The message to act on, or the fact that it does not exist yet.
        target: ResolveTarget,
        /// Rewrite the original message. `resolve.update_in_place` in config.
        update_in_place: bool,
        /// Post a threaded reply. `resolve.thread_reply` in config.
        ///
        /// Both flags travel with the op rather than being read by the worker so that a
        /// config change mid-flight cannot alter the meaning of work already queued.
        thread_reply: bool,
    },
    /// Post a standalone resolved message for a fingerprint we never tracked.
    ///
    /// PRD §5.5 and ADR 001 D9. There is nothing to correlate to, and the alternative to
    /// posting is silence.
    PostOrphanResolved {
        /// The alert we have no record of.
        fingerprint: Fingerprint,
        /// Where to post it.
        channel: ChannelId,
    },
    /// Update a storm-collapse parent's live firing/resolved count (ADR 001 D5).
    ///
    /// Deliberately carries no counts. Membership is a property of the store, not of one
    /// batch — this delivery knows how many members it just added or resolved, but not
    /// how many the group has — so the worker reads them at render time. Inventing a
    /// number here would put a wrong count on the most-read message of a storm.
    RefreshGroup {
        /// The group whose parent to update.
        group_key: GroupKey,
        /// Where the parent lives.
        channel: ChannelId,
        /// The parent message to edit.
        message_ts: ThreadTs,
    },
}

/// Something worth logging or counting that came out of planning a batch.
///
/// Notices are not errors and never suppress an [`Op`]. They exist because several
/// conditions this relay cares about are invisible in the ops alone — a truncated payload
/// produces perfectly ordinary work, and looks fine right up until resolutions start
/// arriving as orphans. Phase 4 maps each of these onto a log line and a metric from
/// ADR 001 D11.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Alertmanager dropped alerts from this body because `max_alerts` is not `0`.
    ///
    /// This is the direct detection of ADR 001 D8's footgun. The dropped alerts are never
    /// tracked, so their resolutions will arrive as orphans, and D8 notes the symptom
    /// "points nowhere near the cause". Here the cause is reported at the moment it
    /// happens, by the sender itself.
    AlertsTruncated {
        /// How many alerts were dropped.
        count: u64,
    },
    /// The delivery contained no alerts at all.
    ///
    /// Alertmanager does not do this. Something in front of it might, and a webhook that
    /// silently accepts empty bodies is indistinguishable from one that is working.
    EmptyBatch,
    /// An alert carried a `status` that is neither `firing` nor `resolved`.
    UnknownStatus {
        /// The alert in question.
        fingerprint: Fingerprint,
        /// The raw status string, preserved so it can reach a log line.
        status: String,
    },
    /// A resolution arrived for a fingerprint that was never tracked.
    ///
    /// Drives `alertthread_orphan_resolves_total`. A rising count with no restarts is the
    /// signature of a non-zero `max_alerts` (ADR 001 D8), which is why this and
    /// [`Notice::AlertsTruncated`] are worth correlating.
    OrphanResolve {
        /// The alert we have no record of.
        fingerprint: Fingerprint,
    },
    /// Storm collapse engaged, and a new group parent is being posted.
    StormCollapsed {
        /// The group being collapsed.
        group_key: GroupKey,
        /// How many alerts are threading under the new parent.
        members: usize,
    },
    /// The shell returned a different number of claim outcomes than the batch had alerts.
    ///
    /// This is a bug in the shell, not in the payload, and it is the specific bug this
    /// project cannot tolerate: an alert with no outcome produces no op and is therefore
    /// silent. The core cannot repair it — it has no claim for the missing alert — but it
    /// can refuse to let it pass unremarked, which is why the count is carried here
    /// instead of being trusted.
    OutcomeCountMismatch {
        /// How many alerts the batch carried.
        alerts: usize,
        /// How many outcomes the shell produced.
        outcomes: usize,
    },
}

/// The result of planning one batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Work to persist in the same transaction as the claims, in the order given.
    pub ops: Vec<Op>,
    /// Conditions the shell should log and count.
    pub notices: Vec<Notice>,
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{
        AlertBatch, ClaimOutcome, ClaimResult, GroupState, Notice, Op, Placement, Plan,
        ResolveTarget,
    };
    use crate::ids::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};
    use crate::webhook::{AlertStatus, WebhookAlert, WebhookPayload};

    fn payload(json: &str) -> WebhookPayload {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn from_webhook_carries_the_batch_level_fields_across() {
        let batch = AlertBatch::from_webhook(
            payload(
                r#"{"groupKey":"gk","status":"firing","truncatedAlerts":7,
                    "alerts":[{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                    "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc"}]}"#,
            ),
            ChannelId::new("#alerts"),
        );

        assert_eq!(batch.channel, ChannelId::new("#alerts"));
        assert_eq!(batch.group_key, GroupKey::new("gk"));
        assert_eq!(batch.truncated_alerts, 7);
        assert_eq!(batch.alerts.len(), 1);
        assert_eq!(batch.alerts[0].fingerprint, Fingerprint::new("abc"));
    }

    #[test]
    fn batch_debug_names_the_channel() {
        let batch = AlertBatch::from_webhook(
            payload(r#"{"groupKey":"gk","status":"firing"}"#),
            ChannelId::new("#alerts"),
        );
        assert!(format!("{batch:?}").contains("#alerts"));
    }

    fn alert() -> WebhookAlert {
        serde_json::from_str(
            r#"{"status":"firing","startsAt":"2026-07-21T14:02:00Z",
                "endsAt":"0001-01-01T00:00:00Z","fingerprint":"abc"}"#,
        )
        .unwrap()
    }

    #[test]
    fn claim_outcome_pairs_an_alert_with_its_result() {
        let outcome = ClaimOutcome::new(alert(), ClaimResult::Claimed);
        assert_eq!(outcome.alert.fingerprint, Fingerprint::new("abc"));
        assert_eq!(outcome.alert.status, AlertStatus::Firing);
        assert_eq!(outcome.result, ClaimResult::Claimed);
        assert!(format!("{outcome:?}").contains("Claimed"));
    }

    #[test]
    fn claim_results_are_distinguishable_from_one_another() {
        // `plan` matches exhaustively over these, so two variants comparing equal would
        // silently merge two rows of ADR D2's classification table.
        let seen = DateTime::<Utc>::from_timestamp(1_721_500_000, 0).unwrap();
        let results = [
            ClaimResult::Claimed,
            ClaimResult::AlreadyClaimed,
            ClaimResult::AlreadyPosted {
                last_seen: seen,
                message_ts: MessageTs::new("1.1"),
            },
            ClaimResult::Resolving {
                message_ts: None,
                thread_parent_ts: None,
            },
            ClaimResult::Orphan,
            ClaimResult::AlreadyResolving,
        ];
        for (i, left) in results.iter().enumerate() {
            for (j, right) in results.iter().enumerate() {
                assert_eq!(left == right, i == j, "{left:?} vs {right:?}");
            }
        }
    }

    #[test]
    fn group_state_debug_names_its_group_and_parent() {
        let state = GroupState {
            group_key: GroupKey::new("gk"),
            message_ts: Some(ThreadTs::new("1.2")),
        };
        let rendered = format!("{state:?}");
        assert!(rendered.contains("gk"), "{rendered}");
        assert!(rendered.contains("1.2"), "{rendered}");
    }

    #[test]
    fn placement_debug_distinguishes_channel_from_thread() {
        assert_eq!(format!("{:?}", Placement::Channel), "Channel");
        let threaded = Placement::Thread {
            group_key: GroupKey::new("gk"),
            parent_ts: Some(ThreadTs::new("1.2")),
        };
        assert!(format!("{threaded:?}").contains("gk"));
    }

    #[test]
    fn resolve_target_debug_distinguishes_its_variants() {
        assert_eq!(format!("{:?}", ResolveTarget::AwaitingPost), "AwaitingPost");
        let targeted = ResolveTarget::Message {
            message_ts: MessageTs::new("1.1"),
            thread_parent_ts: None,
        };
        assert!(format!("{targeted:?}").contains("1.1"));
    }

    #[test]
    fn op_debug_names_the_alert_it_acts_on() {
        let op = Op::Post {
            fingerprint: Fingerprint::new("abc"),
            channel: ChannelId::new("#alerts"),
            placement: Placement::Channel,
        };
        let rendered = format!("{op:?}");
        assert!(rendered.contains("abc"), "{rendered}");
        assert!(rendered.contains("#alerts"), "{rendered}");
    }

    #[test]
    fn every_op_variant_renders_something_identifying() {
        // Ops are logged when they dead-letter (ADR D9), which is the moment an operator
        // most needs to know what the work was.
        let ops = [
            Op::PostGroup {
                group_key: GroupKey::new("gk"),
                channel: ChannelId::new("#alerts"),
                initial_members: 6,
            },
            Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: ChannelId::new("#alerts"),
                message_ts: MessageTs::new("1.1"),
            },
            Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: ChannelId::new("#alerts"),
                target: ResolveTarget::AwaitingPost,
                update_in_place: true,
                thread_reply: false,
            },
            Op::PostOrphanResolved {
                fingerprint: Fingerprint::new("abc"),
                channel: ChannelId::new("#alerts"),
            },
            Op::RefreshGroup {
                group_key: GroupKey::new("gk"),
                channel: ChannelId::new("#alerts"),
                message_ts: ThreadTs::new("1.2"),
            },
        ];
        for op in &ops {
            assert!(format!("{op:?}").contains("#alerts"), "{op:?}");
        }
    }

    #[test]
    fn every_notice_variant_renders_something_identifying() {
        assert_eq!(
            format!("{:?}", Notice::AlertsTruncated { count: 12 }),
            "AlertsTruncated { count: 12 }"
        );
        assert_eq!(format!("{:?}", Notice::EmptyBatch), "EmptyBatch");
        assert!(
            format!(
                "{:?}",
                Notice::UnknownStatus {
                    fingerprint: Fingerprint::new("abc"),
                    status: "suppressed".to_owned(),
                }
            )
            .contains("suppressed")
        );
        assert!(
            format!(
                "{:?}",
                Notice::OrphanResolve {
                    fingerprint: Fingerprint::new("abc"),
                }
            )
            .contains("abc")
        );
        assert!(
            format!(
                "{:?}",
                Notice::StormCollapsed {
                    group_key: GroupKey::new("gk"),
                    members: 6,
                }
            )
            .contains("gk")
        );
        assert_eq!(
            format!(
                "{:?}",
                Notice::OutcomeCountMismatch {
                    alerts: 3,
                    outcomes: 2,
                }
            ),
            "OutcomeCountMismatch { alerts: 3, outcomes: 2 }"
        );
    }

    #[test]
    fn an_empty_plan_is_the_default() {
        let plan = Plan::default();
        assert!(plan.ops.is_empty());
        assert!(plan.notices.is_empty());
        assert_eq!(format!("{plan:?}"), "Plan { ops: [], notices: [] }");
    }
}
