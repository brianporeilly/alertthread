//! [`plan`] — the one function that decides what the relay does.
//!
//! Everything ADR 001 settles about ingest behaviour converges here: the classification of
//! D2, the storm-collapse decision of D5, the repeat-debounce of D7, and the
//! orphan-resolve path of D9 and PRD §5.5. It is a pure function of its arguments, so
//! every one of those decisions is a unit test with no database, no runtime and no mocks.

use chrono::{DateTime, Utc};

use crate::domain::{
    AlertBatch, ClaimOutcome, ClaimResult, GroupState, Notice, Op, Placement, Plan, ResolveTarget,
};
use crate::ids::{MessageTs, ThreadTs};
use crate::policy::Policy;
use crate::webhook::AlertStatus;

/// Decides what to do about one webhook delivery.
///
/// The shell has already run the atomic claims inside a transaction (ADR 001 D3) and
/// looked up any storm-collapse parent for this group; this turns those facts into work.
/// The caller persists the returned [`Op`]s in that same transaction and then commits,
/// which is what makes the durable write happen before the `200` (D2).
///
/// ## Arguments
///
/// - `outcomes` — one entry per alert in `batch`, in the same order, pairing the alert
///   with what its claim did. Built by the shell because that is where the claim runs.
/// - `batch` — the delivery, with its channel resolved. One delivery is one channel and
///   one group key, which is why the collapse decision below is per-batch.
/// - `group` — the `group_message` row for this `(group_key, channel)`, if one exists.
///   This is what makes collapse sticky, and it is state only the shell can supply.
/// - `policy` — configuration.
/// - `now` — injected. This crate cannot read a clock; see the workspace `Cargo.toml`.
///
/// ## What it will not do
///
/// It never returns an empty plan for a batch that contained work. Every claim outcome
/// either produces an op or is a deliberate, documented no-op — a duplicate delivery, or
/// a repeat inside the debounce window — and both of those describe work that has already
/// been done, not work being skipped.
#[must_use]
pub fn plan(
    outcomes: &[ClaimOutcome],
    batch: &AlertBatch,
    group: Option<&GroupState>,
    policy: &Policy,
    now: DateTime<Utc>,
) -> Plan {
    let mut ops = Vec::new();
    let mut notices = batch_notices(outcomes, batch);

    // --- The collapse decision (ADR 001 D5) -------------------------------------------
    //
    // "New post ops for one channel" means newly claimed alerts. A batch is always one
    // channel, so per-batch and per-channel are the same question here. Orphan resolves
    // are deliberately excluded from this count — see below.
    let new_posts = outcomes
        .iter()
        .filter(|outcome| outcome.result == ClaimResult::Claimed)
        .count();

    // `collapse_threshold: 0` disables collapse entirely, stickiness included. Ignoring
    // the existing group row is the point: an operator who turns collapse off and still
    // sees alerts threading has no way to tell the setting works.
    let existing_parent = if policy.collapse_threshold > 0 {
        group
    } else {
        None
    };

    // Sticky: an existing parent captures new members however small the batch. Otherwise
    // it takes strictly more than the threshold, per D5's "more than `collapse_threshold`".
    let collapsing = new_posts > 0
        && (existing_parent.is_some() || new_posts > policy.collapse_threshold)
        && policy.collapse_threshold > 0;
    let opening_group = collapsing && existing_parent.is_none();

    if opening_group {
        // The parent goes first so the summary lands within a second while the children
        // fill in behind it at one message per second (D5).
        ops.push(Op::PostGroup {
            group_key: batch.group_key.clone(),
            channel: batch.channel.clone(),
            initial_members: new_posts,
        });
        notices.push(Notice::StormCollapsed {
            group_key: batch.group_key.clone(),
            members: new_posts,
        });
    }

    let placement = if collapsing {
        thread_placement(existing_parent, batch)
    } else {
        Placement::Channel
    };

    // --- Per-alert decisions ----------------------------------------------------------
    let mut resolved_members = 0_usize;

    for outcome in outcomes {
        let fingerprint = outcome.alert.fingerprint.clone();

        if let AlertStatus::Unknown(raw) = &outcome.alert.status {
            notices.push(Notice::UnknownStatus {
                fingerprint: fingerprint.clone(),
                status: raw.clone(),
            });
        }

        match &outcome.result {
            // D2: the insert won, so we own the notification.
            ClaimResult::Claimed => ops.push(Op::Post {
                fingerprint,
                channel: batch.channel.clone(),
                placement: placement.clone(),
            }),

            // D2/D3: another replica is posting it, or this resolution has already been
            // accepted. Both are duplicate deliveries of work already in hand.
            ClaimResult::AlreadyClaimed | ClaimResult::AlreadyResolving => {}

            // D7: an in-place refresh, but only if enough time has passed to distinguish a
            // `repeat_interval` re-send from an HTTP retry of the same delivery.
            ClaimResult::AlreadyPosted {
                last_seen,
                message_ts,
            } => {
                if now.signed_duration_since(*last_seen) >= policy.refresh_debounce {
                    ops.push(Op::Refresh {
                        fingerprint,
                        channel: batch.channel.clone(),
                        message_ts: message_ts.clone(),
                    });
                }
            }

            // D6: edit in place and/or reply in thread. A `thread_parent_ts` means this
            // alert is a collapsed child, so its resolution changes the parent's count.
            ClaimResult::Resolving {
                message_ts,
                thread_parent_ts,
            } => {
                if thread_parent_ts.is_some() {
                    resolved_members += 1;
                }
                ops.push(Op::Resolve {
                    fingerprint,
                    channel: batch.channel.clone(),
                    target: resolve_target(message_ts.as_ref(), thread_parent_ts.as_ref()),
                    update_in_place: policy.resolve_update_in_place,
                    thread_reply: policy.resolve_thread_reply,
                });
            }

            // D9 and PRD §5.5: nothing to correlate to, so post something anyway.
            //
            // These are top-level and are not counted toward the collapse threshold. An
            // orphan resolve is the visible symptom of lost state or a truncated payload,
            // and burying it in a thread under a summary of *firing* alerts would hide
            // the one message an operator most needs to see.
            ClaimResult::Orphan => {
                notices.push(Notice::OrphanResolve {
                    fingerprint: fingerprint.clone(),
                });
                ops.push(Op::PostOrphanResolved {
                    fingerprint,
                    channel: batch.channel.clone(),
                });
            }
        }
    }

    // D5: the parent shows a live firing/resolved count, so it needs updating whenever
    // this batch changed the membership. Only when it has actually been posted — a parent
    // still awaiting its own post will render current counts when it lands.
    if let Some(state) = existing_parent
        && let Some(parent_ts) = &state.message_ts
        && (collapsing || resolved_members > 0)
    {
        ops.push(Op::RefreshGroup {
            group_key: state.group_key.clone(),
            channel: batch.channel.clone(),
            message_ts: parent_ts.clone(),
        });
    }

    Plan { ops, notices }
}

/// The conditions worth reporting about the delivery as a whole, before any alert in it
/// is looked at.
fn batch_notices(outcomes: &[ClaimOutcome], batch: &AlertBatch) -> Vec<Notice> {
    let mut notices = Vec::new();

    // Alertmanager told us it dropped alerts from this body. ADR 001 D8 warns that the
    // symptom of a non-zero `max_alerts` — resolutions arriving with nothing to correlate
    // to — points nowhere near the cause. This is the cause, reported by the sender, at
    // the moment it happens.
    if batch.truncated_alerts > 0 {
        notices.push(Notice::AlertsTruncated {
            count: batch.truncated_alerts,
        });
    }

    if batch.alerts.is_empty() {
        notices.push(Notice::EmptyBatch);
    }

    // The shell must produce exactly one outcome per alert. Fewer means an alert reached
    // us and produced no op, which is the one failure mode this project does not accept.
    // The core cannot repair that — it has no claim for the missing alert — but it can
    // refuse to let it pass unremarked.
    if outcomes.len() != batch.alerts.len() {
        notices.push(Notice::OutcomeCountMismatch {
            alerts: batch.alerts.len(),
            outcomes: outcomes.len(),
        });
    }

    notices
}

/// Threads a new message under this group's parent.
///
/// When the parent already exists its own key and timestamp are used. When it is being
/// posted by this same plan it has no timestamp yet, so the child carries none and the
/// worker resolves it from the `group_message` row.
fn thread_placement(existing_parent: Option<&GroupState>, batch: &AlertBatch) -> Placement {
    match existing_parent {
        Some(state) => Placement::Thread {
            group_key: state.group_key.clone(),
            parent_ts: state.message_ts.clone(),
        },
        None => Placement::Thread {
            group_key: batch.group_key.clone(),
            parent_ts: None,
        },
    }
}

/// What a resolve has to work with.
///
/// A `None` timestamp is ADR 001 D9's "resolve arrives while `message_ts` is `NULL`": the
/// worker defers with backoff and falls back to a standalone message if the underlying
/// post never lands.
fn resolve_target(
    message_ts: Option<&MessageTs>,
    thread_parent_ts: Option<&ThreadTs>,
) -> ResolveTarget {
    match message_ts {
        Some(ts) => ResolveTarget::Message {
            message_ts: ts.clone(),
            thread_parent_ts: thread_parent_ts.cloned(),
        },
        None => ResolveTarget::AwaitingPost,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};

    use super::plan;
    use crate::domain::{
        AlertBatch, ClaimOutcome, ClaimResult, GroupState, Notice, Op, Placement, ResolveTarget,
    };
    use crate::ids::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};
    use crate::policy::Policy;
    use crate::webhook::{AlertStatus, LabelMap, WebhookAlert};

    const CHANNEL: &str = "#alerts";
    const GROUP: &str = "{}:{alertname=\"KubePodNotReady\"}";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_721_570_000, 0).expect("timestamp is in range")
    }

    fn channel() -> ChannelId {
        ChannelId::new(CHANNEL)
    }

    fn group_key() -> GroupKey {
        GroupKey::new(GROUP)
    }

    /// An alert with the given fingerprint and status.
    fn alert(fingerprint: &str, status: AlertStatus) -> WebhookAlert {
        WebhookAlert {
            status,
            labels: [("alertname".to_owned(), "KubePodNotReady".to_owned())]
                .into_iter()
                .collect(),
            annotations: LabelMap::default(),
            starts_at: now() - TimeDelta::hours(1),
            ends_at: DateTime::from_timestamp(0, 0).expect("epoch is in range"),
            generator_url: "http://prometheus/graph".to_owned(),
            fingerprint: Fingerprint::new(fingerprint),
        }
    }

    fn firing(fingerprint: &str, result: ClaimResult) -> ClaimOutcome {
        ClaimOutcome::new(alert(fingerprint, AlertStatus::Firing), result)
    }

    fn resolved(fingerprint: &str, result: ClaimResult) -> ClaimOutcome {
        ClaimOutcome::new(alert(fingerprint, AlertStatus::Resolved), result)
    }

    /// A batch whose `alerts` mirror the outcomes, which is what the shell always builds.
    fn batch(outcomes: &[ClaimOutcome]) -> AlertBatch {
        AlertBatch {
            channel: channel(),
            group_key: group_key(),
            truncated_alerts: 0,
            alerts: outcomes.iter().map(|o| o.alert.clone()).collect(),
        }
    }

    fn posted(fingerprint: &str) -> Op {
        Op::Post {
            fingerprint: Fingerprint::new(fingerprint),
            channel: channel(),
            placement: Placement::Channel,
        }
    }

    fn threaded(fingerprint: &str, parent_ts: Option<&str>) -> Op {
        Op::Post {
            fingerprint: Fingerprint::new(fingerprint),
            channel: channel(),
            placement: Placement::Thread {
                group_key: group_key(),
                parent_ts: parent_ts.map(ThreadTs::new),
            },
        }
    }

    // -- D2: firing classification ----------------------------------------------------

    #[test]
    fn a_newly_claimed_alert_is_posted() {
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops, vec![posted("abc")]);
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    #[test]
    fn an_alert_already_claimed_by_another_replica_produces_nothing() {
        // ADR D3, rows 1-3: a retried delivery, or a second replica, must not post again.
        // Slack has no idempotency key on chat.postMessage, so this is the suppression.
        let outcomes = [firing("abc", ClaimResult::AlreadyClaimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.ops.is_empty(), "{:?}", result.ops);
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    #[test]
    fn two_replicas_racing_one_fingerprint_produce_exactly_one_post_between_them() {
        // ADR D3 rows 2 and 3, expressed as far as it can be without a database: the
        // store serialises the conflict, so one caller is handed `Claimed` and the other
        // `AlreadyClaimed`. The planner's contribution is that the loser posts nothing.
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let winner = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );
        let outcomes = [firing("abc", ClaimResult::AlreadyClaimed)];
        let loser = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        let posts = winner.ops.len() + loser.ops.len();
        assert_eq!(posts, 1, "{:?} / {:?}", winner.ops, loser.ops);
    }

    // -- D7: repeat-firing debounce ---------------------------------------------------

    #[test]
    fn a_repeat_older_than_the_debounce_refreshes_the_message_in_place() {
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now() - TimeDelta::hours(12),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                message_ts: MessageTs::new("1721500000.000100"),
            }]
        );
    }

    #[test]
    fn a_repeat_inside_the_debounce_window_is_a_duplicate_delivery_and_does_nothing() {
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now() - TimeDelta::seconds(59),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.ops.is_empty(), "{:?}", result.ops);
    }

    #[test]
    fn the_debounce_boundary_itself_counts_as_a_repeat() {
        // Exactly `refresh_debounce` old refreshes. The boundary is decided here rather
        // than left to whichever comparison someone typed.
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now() - TimeDelta::seconds(60),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops.len(), 1, "{:?}", result.ops);
    }

    #[test]
    fn a_last_seen_in_the_future_does_not_refresh() {
        // Clock skew between replicas, or a store timestamp written by a machine that is
        // ahead. Refusing to refresh is the quiet-but-correct outcome: the message is
        // already there and already accurate.
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now() + TimeDelta::hours(1),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.ops.is_empty(), "{:?}", result.ops);
    }

    #[test]
    fn a_zero_debounce_refreshes_on_every_repeat() {
        let policy = Policy {
            refresh_debounce: TimeDelta::zero(),
            ..Policy::default()
        };
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now(),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(&outcomes, &batch(&outcomes), None, &policy, now());

        assert_eq!(result.ops.len(), 1, "{:?}", result.ops);
    }

    // -- D6: resolve ------------------------------------------------------------------

    #[test]
    fn resolving_a_tracked_alert_targets_its_stored_message() {
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1721500000.000100")),
                thread_parent_ts: None,
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1721500000.000100"),
                    thread_parent_ts: None,
                },
                update_in_place: true,
                thread_reply: true,
            }]
        );
    }

    #[test]
    fn a_collapsed_child_resolves_against_its_own_message_not_the_parent() {
        // D5's correctness claim: collapse changes visual placement only. Each child keeps
        // its own row, so per-alert resolve still edits the right message in place.
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1721500000.000100")),
                thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1721500000.000100"),
                    thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
                },
                update_in_place: true,
                thread_reply: true,
            }]
        );
    }

    #[test]
    fn resolving_before_the_post_landed_defers_rather_than_editing_nothing() {
        // ADR D9 row 4. The op is still emitted — the worker self-defers with backoff and
        // falls back to a standalone message. Not emitting it would be silence.
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: None,
                thread_parent_ts: None,
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::AwaitingPost,
                update_in_place: true,
                thread_reply: true,
            }]
        );
    }

    #[test]
    fn the_resolve_behaviour_flags_travel_with_the_op() {
        // Read from policy at plan time, not by the worker at send time, so a config
        // reload cannot change the meaning of work already queued.
        let policy = Policy {
            resolve_thread_reply: false,
            ..Policy::default()
        };
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1.1")),
                thread_parent_ts: None,
            },
        )];
        let result = plan(&outcomes, &batch(&outcomes), None, &policy, now());

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1.1"),
                    thread_parent_ts: None,
                },
                update_in_place: true,
                thread_reply: false,
            }]
        );
    }

    #[test]
    fn the_update_in_place_flag_travels_with_the_op_too() {
        let policy = Policy {
            resolve_update_in_place: false,
            ..Policy::default()
        };
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1.1")),
                thread_parent_ts: None,
            },
        )];
        let result = plan(&outcomes, &batch(&outcomes), None, &policy, now());

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1.1"),
                    thread_parent_ts: None,
                },
                update_in_place: false,
                thread_reply: true,
            }]
        );
    }

    #[test]
    fn a_duplicate_resolution_produces_nothing() {
        let outcomes = [resolved("abc", ClaimResult::AlreadyResolving)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.ops.is_empty(), "{:?}", result.ops);
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    // -- D9 / PRD 5.5: orphan resolves ------------------------------------------------

    #[test]
    fn a_resolution_for_an_untracked_fingerprint_still_posts_something() {
        let outcomes = [resolved("abc", ClaimResult::Orphan)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::PostOrphanResolved {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
            }]
        );
        assert_eq!(
            result.notices,
            vec![Notice::OrphanResolve {
                fingerprint: Fingerprint::new("abc"),
            }]
        );
    }

    #[test]
    fn orphan_resolves_stay_top_level_and_do_not_trigger_collapse() {
        // Deliberate, and not implied by D5. An orphan resolve is the visible symptom of
        // lost state or a truncated payload; threading a pile of them under a summary of
        // firing alerts would hide the message an operator most needs to see.
        let outcomes: Vec<_> = (0..9)
            .map(|i| resolved(&format!("f{i}"), ClaimResult::Orphan))
            .collect();
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops.len(), 9);
        assert!(
            result
                .ops
                .iter()
                .all(|op| matches!(op, Op::PostOrphanResolved { .. })),
            "{:?}",
            result.ops
        );
        assert!(
            !result
                .notices
                .iter()
                .any(|n| matches!(n, Notice::StormCollapsed { .. })),
            "{:?}",
            result.notices
        );
    }

    // -- D5: storm collapse -----------------------------------------------------------

    #[test]
    fn a_batch_at_the_threshold_does_not_collapse() {
        // "More than `collapse_threshold`" — five new posts with a threshold of five stay
        // as five top-level messages.
        let outcomes: Vec<_> = (0..5)
            .map(|i| firing(&format!("f{i}"), ClaimResult::Claimed))
            .collect();
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops.len(), 5);
        assert!(
            result.ops.iter().all(|op| matches!(
                op,
                Op::Post {
                    placement: Placement::Channel,
                    ..
                }
            )),
            "{:?}",
            result.ops
        );
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    #[test]
    fn a_batch_above_the_threshold_collapses_into_a_thread() {
        let outcomes: Vec<_> = (0..6)
            .map(|i| firing(&format!("f{i}"), ClaimResult::Claimed))
            .collect();
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        let mut expected = vec![Op::PostGroup {
            group_key: group_key(),
            channel: channel(),
            initial_members: 6,
        }];
        expected.extend((0..6).map(|i| threaded(&format!("f{i}"), None)));

        assert_eq!(result.ops, expected);
        assert_eq!(
            result.notices,
            vec![Notice::StormCollapsed {
                group_key: group_key(),
                members: 6,
            }]
        );
    }

    #[test]
    fn the_group_parent_is_posted_before_its_children() {
        // D5: the summary lands within a second while children fill in at one per second.
        let outcomes: Vec<_> = (0..6)
            .map(|i| firing(&format!("f{i}"), ClaimResult::Claimed))
            .collect();
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops.first(),
            Some(&Op::PostGroup {
                group_key: group_key(),
                channel: channel(),
                initial_members: 6,
            })
        );
    }

    #[test]
    fn only_newly_claimed_alerts_count_toward_the_threshold() {
        // Six alerts, but five are duplicate deliveries producing no post at all. One new
        // message is not a storm.
        let mut outcomes = vec![firing("new", ClaimResult::Claimed)];
        outcomes.extend((0..5).map(|i| firing(&format!("f{i}"), ClaimResult::AlreadyClaimed)));
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops, vec![posted("new")]);
    }

    #[test]
    fn collapse_is_sticky_once_a_group_has_a_parent() {
        // A single late alert joining an existing group threads under it, even though one
        // post is nowhere near the threshold. Otherwise a group's alerts would be split
        // between top-level messages and thread replies depending on batch timing.
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [firing("late", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![
                threaded("late", Some("1721500000.000001")),
                Op::RefreshGroup {
                    group_key: group_key(),
                    channel: channel(),
                    message_ts: ThreadTs::new("1721500000.000001"),
                },
            ]
        );
        // No second parent, and no collapse notice: the group already exists.
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    #[test]
    fn a_sticky_group_whose_parent_has_not_posted_yet_threads_without_a_timestamp() {
        // The parent's own post is still queued. Children carry no `parent_ts`; the worker
        // resolves it from the group row, deferring until the parent lands.
        let existing = GroupState {
            group_key: group_key(),
            message_ts: None,
        };
        let outcomes = [firing("late", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &Policy::default(),
            now(),
        );

        // No RefreshGroup either: there is no message to edit, and the parent will render
        // current counts when it posts.
        assert_eq!(result.ops, vec![threaded("late", None)]);
    }

    #[test]
    fn resolving_a_collapsed_child_refreshes_the_parents_count() {
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1721500000.000100")),
                thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops.len(), 2, "{:?}", result.ops);
        assert_eq!(
            result.ops.last(),
            Some(&Op::RefreshGroup {
                group_key: group_key(),
                channel: channel(),
                message_ts: ThreadTs::new("1721500000.000001"),
            })
        );
    }

    #[test]
    fn resolving_an_uncollapsed_alert_does_not_refresh_the_parent() {
        // The alert has no `thread_parent_ts`, so it was never a member of the group and
        // its resolution changes no count the parent displays.
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1721500000.000100")),
                thread_parent_ts: None,
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops.len(), 1, "{:?}", result.ops);
        assert!(
            !result
                .ops
                .iter()
                .any(|op| matches!(op, Op::RefreshGroup { .. })),
            "{:?}",
            result.ops
        );
    }

    #[test]
    fn a_repeat_of_a_collapsed_child_does_not_refresh_the_parent() {
        // A refresh changes the child's own "still firing" line. Membership is unchanged,
        // so the parent's count is unchanged, so editing it would be a wasted Slack call
        // against a 50-per-minute tier limit.
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [firing(
            "abc",
            ClaimResult::AlreadyPosted {
                last_seen: now() - TimeDelta::hours(12),
                message_ts: MessageTs::new("1721500000.000100"),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                message_ts: MessageTs::new("1721500000.000100"),
            }]
        );
    }

    #[test]
    fn a_zero_threshold_disables_collapse_however_large_the_batch() {
        let policy = Policy {
            collapse_threshold: 0,
            ..Policy::default()
        };
        let outcomes: Vec<_> = (0..20)
            .map(|i| firing(&format!("f{i}"), ClaimResult::Claimed))
            .collect();
        let result = plan(&outcomes, &batch(&outcomes), None, &policy, now());

        assert_eq!(result.ops.len(), 20);
        assert!(
            result.ops.iter().all(|op| matches!(
                op,
                Op::Post {
                    placement: Placement::Channel,
                    ..
                }
            )),
            "{:?}",
            result.ops
        );
    }

    #[test]
    fn a_zero_threshold_disables_stickiness_too() {
        // "Disables collapse entirely" (D5). An operator who turns collapse off and still
        // sees alerts threading has no way to tell the setting works.
        let policy = Policy {
            collapse_threshold: 0,
            ..Policy::default()
        };
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [firing("late", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &policy,
            now(),
        );

        assert_eq!(result.ops, vec![posted("late")]);
    }

    #[test]
    fn a_zero_threshold_stops_refreshing_an_existing_parents_count() {
        // The other half of "disables collapse entirely", and the one place that choice
        // has a visible cost: an operator who turns collapse off after a storm leaves the
        // existing parent showing the count it had at that moment. That is a stale
        // message, not a lost one, and it is the price of a setting that means what it
        // says. The alternative — honouring the group row for updates but not for new
        // members — would leave collapse half-on with nothing in the config to say so.
        let policy = Policy {
            collapse_threshold: 0,
            ..Policy::default()
        };
        let existing = GroupState {
            group_key: group_key(),
            message_ts: Some(ThreadTs::new("1721500000.000001")),
        };
        let outcomes = [resolved(
            "abc",
            ClaimResult::Resolving {
                message_ts: Some(MessageTs::new("1721500000.000100")),
                thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
            },
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            Some(&existing),
            &policy,
            now(),
        );

        assert_eq!(
            result.ops,
            vec![Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::Message {
                    message_ts: MessageTs::new("1721500000.000100"),
                    thread_parent_ts: Some(ThreadTs::new("1721500000.000001")),
                },
                update_in_place: true,
                thread_reply: true,
            }]
        );
    }

    #[test]
    fn a_batch_with_no_new_posts_never_opens_a_group() {
        // Six alerts, all duplicates. A group parent with no children would be a message
        // saying nothing.
        let policy = Policy {
            collapse_threshold: 1,
            ..Policy::default()
        };
        let outcomes: Vec<_> = (0..6)
            .map(|i| firing(&format!("f{i}"), ClaimResult::AlreadyClaimed))
            .collect();
        let result = plan(&outcomes, &batch(&outcomes), None, &policy, now());

        assert!(result.ops.is_empty(), "{:?}", result.ops);
        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    // -- truncatedAlerts (ADR D8) ------------------------------------------------------

    #[test]
    fn a_truncated_payload_is_reported() {
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let mut batch = batch(&outcomes);
        batch.truncated_alerts = 12;
        let result = plan(&outcomes, &batch, None, &Policy::default(), now());

        assert_eq!(
            result.notices,
            vec![Notice::AlertsTruncated { count: 12 }],
            "a non-zero truncatedAlerts is the max_alerts misconfiguration of ADR D8, \
             reported by Alertmanager itself"
        );
        // And the alerts that *did* arrive are still handled normally.
        assert_eq!(result.ops, vec![posted("abc")]);
    }

    #[test]
    fn an_untruncated_payload_reports_nothing() {
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    // -- Edge cases --------------------------------------------------------------------

    #[test]
    fn an_empty_batch_produces_no_work_and_says_so() {
        let batch = AlertBatch {
            channel: channel(),
            group_key: group_key(),
            truncated_alerts: 0,
            alerts: Vec::new(),
        };
        let result = plan(&[], &batch, None, &Policy::default(), now());

        assert!(result.ops.is_empty(), "{:?}", result.ops);
        assert_eq!(result.notices, vec![Notice::EmptyBatch]);
    }

    #[test]
    fn an_empty_batch_that_was_truncated_reports_both() {
        // `max_alerts: 1` with a single resolved alert can produce exactly this. Reporting
        // only the emptiness would hide the reason for it.
        let batch = AlertBatch {
            channel: channel(),
            group_key: group_key(),
            truncated_alerts: 4,
            alerts: Vec::new(),
        };
        let result = plan(&[], &batch, None, &Policy::default(), now());

        assert_eq!(
            result.notices,
            vec![Notice::AlertsTruncated { count: 4 }, Notice::EmptyBatch]
        );
    }

    #[test]
    fn a_non_empty_batch_does_not_report_emptiness() {
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(
            !result.notices.contains(&Notice::EmptyBatch),
            "{:?}",
            result.notices
        );
    }

    #[test]
    fn an_unknown_status_is_reported_and_the_alert_is_still_posted() {
        let outcomes = [ClaimOutcome::new(
            alert("abc", AlertStatus::Unknown("suppressed".to_owned())),
            ClaimResult::Claimed,
        )];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops, vec![posted("abc")]);
        assert_eq!(
            result.notices,
            vec![Notice::UnknownStatus {
                fingerprint: Fingerprint::new("abc"),
                status: "suppressed".to_owned(),
            }]
        );
    }

    #[test]
    fn a_known_status_is_not_reported_as_unknown() {
        let outcomes = [
            firing("abc", ClaimResult::Claimed),
            resolved("def", ClaimResult::Orphan),
        ];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(
            !result
                .notices
                .iter()
                .any(|n| matches!(n, Notice::UnknownStatus { .. })),
            "{:?}",
            result.notices
        );
    }

    #[test]
    fn a_batch_carrying_both_a_firing_and_a_resolved_for_one_fingerprint_does_both() {
        // Pathological but expressible. The shell runs the claims in order inside one
        // transaction, so the outcomes already reflect the sequence: the insert wins, then
        // the update flips the same row to `resolving`. The planner emits both ops; the
        // outbox drains them in order, and the resolve defers until the post lands.
        //
        // The alternative — dropping one of them — would either lose the message or leave
        // it red forever. Both are worse than a message that goes up and immediately
        // turns green.
        let outcomes = [
            firing("abc", ClaimResult::Claimed),
            resolved(
                "abc",
                ClaimResult::Resolving {
                    message_ts: None,
                    thread_parent_ts: None,
                },
            ),
        ];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![
                posted("abc"),
                Op::Resolve {
                    fingerprint: Fingerprint::new("abc"),
                    channel: channel(),
                    target: ResolveTarget::AwaitingPost,
                    update_in_place: true,
                    thread_reply: true,
                },
            ]
        );
    }

    #[test]
    fn ops_follow_the_order_of_the_outcomes_they_came_from() {
        let outcomes = [
            firing("a", ClaimResult::Claimed),
            firing("b", ClaimResult::Claimed),
            firing("c", ClaimResult::Claimed),
        ];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(result.ops, vec![posted("a"), posted("b"), posted("c")]);
    }

    #[test]
    fn fewer_outcomes_than_alerts_is_reported() {
        // A shell bug, and the specific one this project cannot tolerate: an alert that
        // arrived and produced no op is silent.
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let mut batch = batch(&outcomes);
        batch.alerts.push(alert("def", AlertStatus::Firing));
        let result = plan(&outcomes, &batch, None, &Policy::default(), now());

        assert_eq!(
            result.notices,
            vec![Notice::OutcomeCountMismatch {
                alerts: 2,
                outcomes: 1,
            }]
        );
        // The alerts that *were* claimed are still planned.
        assert_eq!(result.ops, vec![posted("abc")]);
    }

    #[test]
    fn more_outcomes_than_alerts_is_reported_too() {
        let outcomes = [
            firing("abc", ClaimResult::Claimed),
            firing("def", ClaimResult::Claimed),
        ];
        let mut batch = batch(&outcomes);
        batch.alerts.pop();
        let result = plan(&outcomes, &batch, None, &Policy::default(), now());

        assert_eq!(
            result.notices,
            vec![Notice::OutcomeCountMismatch {
                alerts: 1,
                outcomes: 2,
            }]
        );
    }

    #[test]
    fn matching_counts_are_not_reported() {
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert!(result.notices.is_empty(), "{:?}", result.notices);
    }

    #[test]
    fn a_mixed_batch_plans_every_kind_of_outcome_at_once() {
        // The realistic shape: a group where some alerts are new, some are repeats, some
        // have resolved, and one resolution has no state behind it.
        let outcomes = [
            firing("new", ClaimResult::Claimed),
            firing(
                "repeat",
                ClaimResult::AlreadyPosted {
                    last_seen: now() - TimeDelta::hours(12),
                    message_ts: MessageTs::new("1.1"),
                },
            ),
            firing("dupe", ClaimResult::AlreadyClaimed),
            resolved(
                "done",
                ClaimResult::Resolving {
                    message_ts: Some(MessageTs::new("1.2")),
                    thread_parent_ts: None,
                },
            ),
            resolved("ghost", ClaimResult::Orphan),
            resolved("again", ClaimResult::AlreadyResolving),
        ];
        let result = plan(
            &outcomes,
            &batch(&outcomes),
            None,
            &Policy::default(),
            now(),
        );

        assert_eq!(
            result.ops,
            vec![
                posted("new"),
                Op::Refresh {
                    fingerprint: Fingerprint::new("repeat"),
                    channel: channel(),
                    message_ts: MessageTs::new("1.1"),
                },
                Op::Resolve {
                    fingerprint: Fingerprint::new("done"),
                    channel: channel(),
                    target: ResolveTarget::Message {
                        message_ts: MessageTs::new("1.2"),
                        thread_parent_ts: None,
                    },
                    update_in_place: true,
                    thread_reply: true,
                },
                Op::PostOrphanResolved {
                    fingerprint: Fingerprint::new("ghost"),
                    channel: channel(),
                },
            ]
        );
        assert_eq!(
            result.notices,
            vec![Notice::OrphanResolve {
                fingerprint: Fingerprint::new("ghost"),
            }]
        );
    }

    #[test]
    fn every_op_is_addressed_to_the_batchs_channel() {
        // The channel comes from the request, not from the store, and getting it from the
        // wrong place would post correct messages into the wrong room.
        let elsewhere = ChannelId::new("#alerts-critical");
        let outcomes = [firing("abc", ClaimResult::Claimed)];
        let mut batch = batch(&outcomes);
        batch.channel = elsewhere.clone();
        let result = plan(&outcomes, &batch, None, &Policy::default(), now());

        assert_eq!(
            result.ops,
            vec![Op::Post {
                fingerprint: Fingerprint::new("abc"),
                channel: elsewhere,
                placement: Placement::Channel,
            }]
        );
    }
}
