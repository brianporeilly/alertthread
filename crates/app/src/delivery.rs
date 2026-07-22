//! Turning one leased [`Op`] into one Slack call, and one Slack call into an outcome.
//!
//! # This module executes; it does not decide
//!
//! AGENTS.md rule 2: any question of the form "given this state, what should we do?"
//! belongs in [`plan`](alertthread_core::plan). What is left here is genuinely different —
//! *how* to carry out a decision already taken, and what to do about the answer Slack gave.
//! Both of those need I/O to ask, and neither is expressible in the pure core.
//!
//! The two places that look like decisions are not. An op that has spent its attempts
//! waiting for a storm-collapse parent posts at top level, and a resolve whose alert never
//! got a message posts a standalone one — both are ADR 001 D9, which already says
//! *"self-defer with backoff; on timeout, post standalone"*. This is where that is carried
//! out.
//!
//! # Every path ends in a message or a loud noise
//!
//! No arm below quietly drops an op. The exhaustive list of what can happen to one is
//! [`Outcome`], and reading its four variants is the fastest way to check that claim.

use std::time::Instant;

use alertthread_core::{
    ChannelId, Fingerprint, GroupKey, MessageTs, Op, Placement, ResolveTarget, ThreadTs,
};
use alertthread_slack::{
    AlertView, Disposition, GroupView, PostMessage, RenderRequest, Rendered, Renderer, SlackClient,
    SlackError, SlackMethod, UpdateMessage,
};
use alertthread_store::{
    AlertRecord, AlertState, GroupMembership, GroupRecord, LeasedOp, OpEffect, StateStore,
    StoreError,
};
use chrono::{DateTime, TimeDelta, Utc};

use crate::metrics::Metrics;
use crate::ratelimit::{Permit, SlackLimits};

/// What happened to one leased op.
///
/// Deliberately exhaustive and deliberately small. Every arm is "it is done", "come back
/// later", or "stop, loudly" — there is no arm meaning "forget about it", because that arm
/// would be silence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Delivered. Apply the effect and delete the row.
    Done(OpEffect),
    /// Come back at this instant, with the attempt given back (ADR 001 D2).
    ///
    /// Slack's 429, or the relay's own token bucket. Neither is the op failing, and
    /// counting either would march an alert toward the dead-letter queue for arriving
    /// during a storm — which is exactly when it matters most.
    Wait {
        /// When to try again.
        until: DateTime<Utc>,
    },
    /// Come back at this instant, having spent an attempt.
    Retry {
        /// When to try again.
        until: DateTime<Utc>,
        /// What went wrong, for the operator reading `last_error` on a stuck row.
        error: String,
    },
    /// Park it: this will never succeed. `alertthread_dead_letter_total{reason}`.
    DeadLetter {
        /// The low-cardinality reason, which becomes the metric label.
        reason: &'static str,
        /// The full detail, which goes in the log line and in `last_error`.
        detail: String,
    },
}

/// How long an op waits, and how many times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// How many attempts an op gets before it is parked (ADR 001 D9, default 10).
    pub max_attempts: i32,
    /// The first delay. Doubles per attempt.
    pub base: TimeDelta,
    /// The longest a backoff ever waits.
    pub max: TimeDelta,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            // 4s doubling, capped at ten minutes, so the nine waits an op actually spends
            // before it is parked add up to roughly the half hour ADR 001 D9 quotes.
            // `the_total_backoff_reaches_roughly_the_half_hour_d9_describes` is what holds
            // these three numbers to that claim rather than to whatever looked tidy.
            base: TimeDelta::seconds(4),
            max: TimeDelta::minutes(10),
        }
    }
}

impl Backoff {
    /// The delay after `attempts` attempts, capped, with a deterministic spread.
    ///
    /// The spread is derived from the attempt number rather than from an RNG. This project
    /// has no random-number dependency and does not want one for this: what jitter is *for*
    /// is stopping a hundred ops deferred by the same outage from returning in the same
    /// millisecond, and separating them by attempt does that without needing to be
    /// unpredictable.
    #[must_use]
    pub fn delay(&self, attempts: i32) -> TimeDelta {
        let steps = attempts.clamp(1, 20) - 1;
        let factor = i32::try_from(1_i64 << steps).unwrap_or(i32::MAX);
        let raw = self
            .base
            .checked_mul(factor)
            .unwrap_or(self.max)
            .min(self.max)
            .max(TimeDelta::zero());

        // ±12.5%, keyed on the attempt. Two ops on the same attempt still land together,
        // which is fine — what matters is that a queue retrying at several different
        // attempt counts does not converge on one instant.
        //
        // `integer_division` is denied workspace-wide because a truncating divide in the
        // delivery path is usually a bug. Here the truncation is the point: this is a
        // millisecond count being spread, and losing a fraction of a millisecond off the
        // spread changes nothing about what the spread is for.
        #[expect(clippy::integer_division, reason = "see above")]
        let spread = raw.num_milliseconds() / 8;
        let offset = spread.saturating_mul(i64::from(attempts % 3) - 1);
        (raw + TimeDelta::try_milliseconds(offset).unwrap_or_else(TimeDelta::zero))
            .max(TimeDelta::zero())
    }

    /// Whether an op has used up the attempts ADR 001 D9 gives it.
    #[must_use]
    pub const fn exhausted(&self, attempts: i32) -> bool {
        attempts >= self.max_attempts
    }

    /// ADR 001 D9's retry policy for one Slack failure, in one place.
    ///
    /// Driven by [`SlackError::disposition`] rather than by matching on error strings: a
    /// typo in a string match here would be an alert that never posts.
    #[must_use]
    pub fn classify(&self, error: &SlackError, attempts: i32, now: DateTime<Utc>) -> Outcome {
        match error.disposition() {
            Disposition::RateLimited { retry_after } => Outcome::Wait {
                until: now
                    + TimeDelta::from_std(retry_after).unwrap_or_else(|_| TimeDelta::seconds(1)),
            },
            Disposition::MessageGone => Outcome::Done(OpEffect::MessageLost),
            Disposition::Terminal => Outcome::DeadLetter {
                reason: error.outcome(),
                detail: error.to_string(),
            },
            Disposition::Retry if self.exhausted(attempts) => Outcome::DeadLetter {
                reason: error.outcome(),
                detail: format!("{error} (after {attempts} attempts)"),
            },
            Disposition::Retry => Outcome::Retry {
                until: now + self.delay(attempts),
                error: error.to_string(),
            },
        }
    }
}

/// Everything one delivery needs.
///
/// A struct rather than six arguments threaded through every branch: a positional mistake
/// between two reference-shaped things is exactly what AGENTS.md rule 4 exists to prevent.
pub struct Delivery<'a, S: StateStore> {
    /// Where correlation state lives.
    pub store: &'a S,
    /// The Slack client.
    pub slack: &'a SlackClient,
    /// The renderer, with any overrides already installed.
    pub renderer: &'a Renderer,
    /// The token buckets.
    pub limits: &'a SlackLimits,
    /// Where the counters live.
    pub metrics: &'a Metrics,
    /// How long an op waits, and how many times.
    pub backoff: Backoff,
}

impl<S: StateStore> Delivery<'_, S> {
    /// Carries out one leased op.
    ///
    /// # Errors
    ///
    /// [`StoreError`] only when the *store* failed — reading an alert row, counting a
    /// group. A Slack failure is not an error here: it is an [`Outcome`], because every one
    /// of them has a defined next step and none of them is "give up quietly".
    pub async fn run(&self, leased: &LeasedOp, now: DateTime<Utc>) -> Result<Outcome, StoreError> {
        match &leased.op {
            Op::Post {
                fingerprint,
                channel,
                placement,
            } => {
                self.post(leased, fingerprint, channel, placement, now)
                    .await
            }
            Op::PostGroup {
                group_key,
                channel,
                initial_members,
            } => {
                self.post_group(leased, group_key, channel, *initial_members, now)
                    .await
            }
            Op::Refresh {
                fingerprint,
                channel,
                message_ts,
            } => {
                self.refresh(leased, fingerprint, channel, message_ts, now)
                    .await
            }
            Op::RefreshGroup {
                group_key,
                channel,
                message_ts,
            } => {
                self.refresh_group(leased, group_key, channel, message_ts, now)
                    .await
            }
            Op::Resolve {
                fingerprint,
                channel,
                target,
                update_in_place,
                thread_reply,
            } => {
                self.resolve(
                    leased,
                    fingerprint,
                    channel,
                    target,
                    (*update_in_place, *thread_reply),
                    now,
                )
                .await
            }
            Op::PostOrphanResolved {
                fingerprint,
                channel,
            } => self.post_orphan(leased, fingerprint, channel, now).await,
        }
    }

    // -- Post -------------------------------------------------------------------------

    async fn post(
        &self,
        leased: &LeasedOp,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
        placement: &Placement,
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        let Some(record) = self.store.alert(fingerprint, channel).await? else {
            // The pruner refuses to delete a row with queued work, so reaching this needs
            // the row to have gone some other way. There is nothing to render and nothing
            // to recover; parking it is the loud option, and the outbox payload survives as
            // the record of an alert that never went out.
            return Ok(missing_row(fingerprint, channel, "post"));
        };

        let parent = match placement {
            Placement::Channel => None,
            Placement::Thread {
                group_key,
                parent_ts,
            } => {
                match self
                    .thread_parent(leased, group_key, channel, parent_ts.as_ref(), now)
                    .await?
                {
                    Ok(parent) => parent,
                    Err(outcome) => return Ok(outcome),
                }
            }
        };

        let view = view_of(&record);
        // Resolved by the time its own post is drained. Rendering it as firing would put a
        // red message in the channel for an alert that has already cleared, and the resolve
        // op queued behind it would edit it green a moment later — two notifications for
        // one event, the second of which is the interesting one.
        let request = if record.resolved_at.is_some() {
            RenderRequest::Resolved(&view)
        } else {
            RenderRequest::Firing(&view)
        };
        let rendered = self.render(request, now);

        let message = match &parent {
            Some(parent) => PostMessage::in_thread(channel, &rendered.body, parent),
            None => PostMessage::to_channel(channel, &rendered.body),
        };

        Ok(self
            .send(&message, leased.attempts, now, |posted| OpEffect::Posted {
                message_ts: posted.ts,
                thread_parent_ts: parent.clone(),
            })
            .await)
    }

    /// Finds the timestamp a collapsed child threads under.
    ///
    /// `Ok(Ok(_))` is a parent to use, or a deliberate `None`. `Ok(Err(outcome))` is the
    /// self-deferral of ADR 001 D2: the parent's own post has not completed, so this child
    /// comes back later — the same mechanism the ordering guarantee already uses, rather
    /// than a second one doing the same job.
    ///
    /// The last resort is the part that matters. An op that has spent its attempts waiting
    /// posts **at top level** rather than dead-lettering. A threaded message with no thread
    /// is not a possible outcome; an unthreaded one is merely untidy, and untidy beats
    /// absent every time.
    async fn thread_parent(
        &self,
        leased: &LeasedOp,
        group_key: &GroupKey,
        channel: &ChannelId,
        planned: Option<&ThreadTs>,
        now: DateTime<Utc>,
    ) -> Result<Result<Option<ThreadTs>, Outcome>, StoreError> {
        if let Some(parent) = planned {
            return Ok(Ok(Some(parent.clone())));
        }

        let landed = self
            .store
            .group(group_key, channel)
            .await?
            .and_then(|group| group.message_ts);
        if let Some(parent) = landed {
            return Ok(Ok(Some(parent)));
        }

        if self.backoff.exhausted(leased.attempts) {
            tracing::warn!(
                %group_key,
                %channel,
                attempts = leased.attempts,
                "storm-collapse parent never posted; posting this alert at top level rather \
                 than dropping it (ADR 001 D9)"
            );
            return Ok(Ok(None));
        }

        Ok(Err(Outcome::Retry {
            until: now + self.backoff.delay(leased.attempts),
            error: format!("waiting for the group summary for {group_key} to post"),
        }))
    }

    async fn post_group(
        &self,
        leased: &LeasedOp,
        group_key: &GroupKey,
        channel: &ChannelId,
        initial_members: usize,
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        let record = self.store.group(group_key, channel).await?;
        let membership = self.store.group_membership(group_key, channel).await?;
        let view = group_view(group_key, record.as_ref(), membership, initial_members);
        let rendered = self.render(RenderRequest::GroupSummary(&view), now);
        let message = PostMessage::to_channel(channel, &rendered.body);

        Ok(self
            .send(&message, leased.attempts, now, |posted| {
                OpEffect::GroupPosted {
                    message_ts: posted.thread_ts(),
                }
            })
            .await)
    }

    async fn post_orphan(
        &self,
        leased: &LeasedOp,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        // ⚠️ `Op::PostOrphanResolved` carries a fingerprint and a channel and nothing else,
        // and by definition there is no `alert_message` row to read: an orphan resolve is a
        // resolution for an alert this relay never saw fire. So this message cannot name
        // the alert, quote its summary, or say how long it fired — none of that data exists
        // anywhere the worker can reach by the time the op is drained.
        //
        // What it can do is say precisely that, which is more useful than it sounds: a
        // fingerprint plus "we have no record of this firing" *is* the diagnosis for a
        // truncated `max_alerts` payload (ADR 001 D8) or for state lost across a restart.
        //
        // Carrying the alert's labels on the op would fix it properly. That is a change to
        // a core type and to the outbox payload format, so it is recorded in the PR rather
        // than smuggled in here.
        let view = orphan_view(fingerprint, leased.created_at);
        let rendered = self.render(RenderRequest::Resolved(&view), now);
        let message = PostMessage::to_channel(channel, &rendered.body);

        Ok(self
            .send(&message, leased.attempts, now, |_| OpEffect::Standalone)
            .await)
    }

    // -- Refresh (ADR 001 D7) ---------------------------------------------------------

    async fn refresh(
        &self,
        leased: &LeasedOp,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
        message_ts: &MessageTs,
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        let Some(record) = self.store.alert(fingerprint, channel).await? else {
            // Nothing to refresh, and nothing lost: a refresh only ever edits a message
            // that is already in the channel. Completing it is honest — there is no work
            // left to do, and dead-lettering would raise an alarm about a message that is
            // sitting there being read.
            return Ok(Outcome::Done(OpEffect::Refreshed));
        };

        let view = view_of(&record);
        let request = if record.resolved_at.is_some() {
            RenderRequest::Resolved(&view)
        } else {
            RenderRequest::Firing(&view)
        };
        let rendered = self.render(request, now);
        let message = UpdateMessage::new(channel, message_ts, &rendered.body);

        Ok(self
            .edit(&message, leased.attempts, now, OpEffect::Refreshed)
            .await)
    }

    async fn refresh_group(
        &self,
        leased: &LeasedOp,
        group_key: &GroupKey,
        channel: &ChannelId,
        message_ts: &ThreadTs,
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        let record = self.store.group(group_key, channel).await?;
        let membership = self.store.group_membership(group_key, channel).await?;
        let view = group_view(group_key, record.as_ref(), membership, 0);
        let rendered = self.render(RenderRequest::GroupSummary(&view), now);
        let message = UpdateMessage::group(channel, message_ts, &rendered.body);

        Ok(self
            .edit(&message, leased.attempts, now, OpEffect::Refreshed)
            .await)
    }

    // -- Resolve (ADR 001 D6) ---------------------------------------------------------

    async fn resolve(
        &self,
        leased: &LeasedOp,
        fingerprint: &Fingerprint,
        channel: &ChannelId,
        target: &ResolveTarget,
        behaviour: (bool, bool),
        now: DateTime<Utc>,
    ) -> Result<Outcome, StoreError> {
        let (update_in_place, thread_reply) = behaviour;
        let record = self.store.alert(fingerprint, channel).await?;

        // ADR 001 D9, "resolve arrives while `message_ts` is NULL". The planner emitted
        // `AwaitingPost`; by the time the op is drained the post may have landed, so the
        // store is asked again rather than the op's own snapshot being trusted.
        let landed = match target {
            ResolveTarget::Message { message_ts, .. } => Some(message_ts.clone()),
            ResolveTarget::AwaitingPost => record.as_ref().and_then(|row| row.message_ts.clone()),
        };

        let Some(message_ts) = landed else {
            return Ok(self
                .resolve_without_a_message(leased, record.as_ref(), channel, now)
                .await);
        };

        let Some(record) = record else {
            return Ok(missing_row(fingerprint, channel, "resolve"));
        };
        let view = resolved_view(&record, now);

        if update_in_place {
            let rendered = self.render(RenderRequest::Resolved(&view), now);
            let message = UpdateMessage::new(channel, &message_ts, &rendered.body);
            match self
                .edit(&message, leased.attempts, now, OpEffect::Resolved)
                .await
            {
                Outcome::Done(OpEffect::Resolved) => {}
                // Everything else is returned as it stands — and `Done(MessageLost)` is
                // the reason this arm matches the *effect* rather than just `Done(_)`.
                //
                // `message_not_found` means the message this resolution addresses is gone.
                // Falling through to the thread reply would post "resolved after 29m" under
                // a `thread_ts` that no longer exists, which Slack answers with an error
                // and which would in any case have thrown away the healing:
                // `complete(MessageLost)` clears the stale timestamp and enqueues a fresh
                // post in the same transaction, and that post renders green because the
                // row's `resolved_at` is already set. The alert still gets a green message;
                // it just gets a new one instead of an edit.
                other => return Ok(other),
            }
        }

        if thread_reply {
            // The half that generates the unread indicator (ADR 001 D6): `chat.update` does
            // not notify, bump, or mark a channel unread, so the edit alone is invisible to
            // anybody watching live. It threads under the alert's *own* message, which is
            // the one legitimate MessageTs → ThreadTs crossing and has a named constructor
            // for exactly that reason.
            let rendered = self.render(RenderRequest::ThreadReply(&view), now);
            let message = PostMessage::in_reply_to(channel, &rendered.body, &message_ts);
            return Ok(self
                .send(&message, leased.attempts, now, |_| OpEffect::Resolved)
                .await);
        }

        Ok(Outcome::Done(OpEffect::Resolved))
    }

    /// A resolution whose alert never got a message.
    ///
    /// ADR 001 D9: self-defer with backoff, and on timeout post a standalone message. The
    /// standalone post is the whole point — the alternative is a resolution nobody hears
    /// about, for an alert nobody heard about.
    async fn resolve_without_a_message(
        &self,
        leased: &LeasedOp,
        record: Option<&AlertRecord>,
        channel: &ChannelId,
        now: DateTime<Utc>,
    ) -> Outcome {
        // A post that has already been parked will never produce a timestamp, so there is
        // nothing left to wait for and burning the remaining attempts would only delay the
        // message. `Failed` is the state `dead_letter` leaves behind for exactly this.
        let hopeless = record.is_none_or(|row| row.state == AlertState::Failed);

        if !hopeless && !self.backoff.exhausted(leased.attempts) {
            return Outcome::Retry {
                until: now + self.backoff.delay(leased.attempts),
                error: "waiting for the alert's own post to land".to_owned(),
            };
        }

        let Some(record) = record else {
            return Outcome::DeadLetter {
                reason: "alert_row_missing",
                detail: "a resolution whose alert row is gone has nothing to render".to_owned(),
            };
        };

        tracing::warn!(
            fingerprint = %record.fingerprint,
            %channel,
            attempts = leased.attempts,
            state = record.state.as_str(),
            "the alert's own post never landed; posting a standalone resolved message \
             (ADR 001 D9)"
        );

        let view = resolved_view(record, now);
        let rendered = self.render(RenderRequest::Resolved(&view), now);
        let message = PostMessage::to_channel(channel, &rendered.body);

        self.send(&message, leased.attempts, now, |_| OpEffect::Resolved)
            .await
    }

    // -- Slack ------------------------------------------------------------------------

    /// Sends a `chat.postMessage`, once the per-channel token bucket agrees.
    ///
    /// Pacing happens here rather than in the worker loop so that no path to
    /// `post_message` exists that skips it — AGENTS.md: never bypass the rate limiter.
    async fn send(
        &self,
        request: &PostMessage<'_>,
        attempts: i32,
        now: DateTime<Utc>,
        effect: impl FnOnce(alertthread_slack::PostedMessage) -> OpEffect,
    ) -> Outcome {
        if let Permit::Wait { until } = self.limits.post(request.channel, now) {
            self.metrics.rate_limited_locally(SlackMethod::PostMessage);
            return Outcome::Wait { until };
        }

        let timed = Timed::start(SlackMethod::PostMessage);
        match timed.finish(self.metrics, self.slack.post_message(request).await) {
            Ok(posted) => Outcome::Done(effect(posted)),
            Err(error) => self.failed(&error, attempts, now),
        }
    }

    /// Sends a `chat.update`, once the workspace token bucket agrees.
    async fn edit(
        &self,
        request: &UpdateMessage<'_>,
        attempts: i32,
        now: DateTime<Utc>,
        effect: OpEffect,
    ) -> Outcome {
        if let Permit::Wait { until } = self.limits.update(now) {
            self.metrics
                .rate_limited_locally(SlackMethod::UpdateMessage);
            return Outcome::Wait { until };
        }

        let timed = Timed::start(SlackMethod::UpdateMessage);
        match timed.finish(self.metrics, self.slack.update_message(request).await) {
            Ok(()) => Outcome::Done(effect),
            Err(error) => self.failed(&error, attempts, now),
        }
    }

    fn failed(&self, error: &SlackError, attempts: i32, now: DateTime<Utc>) -> Outcome {
        if let Some(method) = error.method()
            && matches!(error.disposition(), Disposition::RateLimited { .. })
        {
            self.metrics.rate_limited_by_slack(method);
        }
        self.backoff.classify(error, attempts, now)
    }

    /// Renders, counting the fallback if it engaged (ADR 001 D9, D11).
    fn render(&self, request: RenderRequest<'_>, now: DateTime<Utc>) -> Rendered {
        let rendered = self.renderer.render(&request, now);
        if let Some(degradation) = &rendered.degraded {
            self.metrics.degraded(degradation);
            tracing::error!(
                template = %degradation.template,
                reason = degradation.reason.as_str(),
                detail = %degradation.detail,
                "message template failed; posting the built-in minimal message instead"
            );
        }
        if let Some(truncation) = &rendered.truncated {
            tracing::warn!(
                dropped_chars = truncation.dropped_chars,
                dropped_blocks = truncation.dropped_blocks,
                "message exceeded Slack's block limits and was truncated"
            );
        }
        rendered
    }
}

/// The outcome for an op whose `alert_message` row has gone.
fn missing_row(fingerprint: &Fingerprint, channel: &ChannelId, what: &str) -> Outcome {
    Outcome::DeadLetter {
        reason: "alert_row_missing",
        detail: format!(
            "no alert_message row for {fingerprint} in {channel}: a {what} renders from \
             correlation state, and there is none"
        ),
    }
}

/// The store's record, as a message is going to describe it.
///
/// `generatorURL` is read from the annotations because that is the only place the store
/// keeps it: `alert_message` has `labels` and `annotations` columns and no column of its
/// own for the link. An absent link renders as an empty string rather than as a broken one.
fn view_of(record: &AlertRecord) -> AlertView {
    AlertView {
        fingerprint: record.fingerprint.clone(),
        labels: record.labels.clone(),
        annotations: record.annotations.clone(),
        starts_at: record.first_seen,
        resolved_at: record.resolved_at,
        generator_url: record
            .annotations
            .get("generatorURL")
            .cloned()
            .unwrap_or_default(),
    }
}

/// The same, but certain the alert has resolved.
///
/// A resolve op exists because a resolution was accepted, so a missing `resolved_at` here
/// means the row was re-claimed underneath it — and rendering a *resolve* as still firing
/// would leave the message red for an alert that has cleared.
fn resolved_view(record: &AlertRecord, now: DateTime<Utc>) -> AlertView {
    let mut view = view_of(record);
    view.resolved_at = Some(record.resolved_at.unwrap_or(now));
    view
}

/// What a storm-collapse summary says about itself.
///
/// `initial_members` is a floor rather than the value: it is what one plan *added*, and the
/// store knows what the group holds. Taking the larger keeps a summary from claiming fewer
/// members than the batch that opened it, which is the direction that looks like a bug to
/// somebody counting the replies underneath it.
fn group_view(
    group_key: &GroupKey,
    record: Option<&GroupRecord>,
    membership: GroupMembership,
    initial_members: usize,
) -> GroupView {
    GroupView {
        group_key: group_key.clone(),
        labels: record
            .map(|row| row.group_labels.clone())
            .unwrap_or_default(),
        firing: membership.firing.max(initial_members),
        resolved: membership.resolved,
    }
}

/// The best a message can say about a resolution for an alert nobody told us about.
///
/// See `post_orphan` for why this is so thin. The `alertname` label is synthesised so the
/// heading is not blank, and the annotation says what happened — because "no record of this
/// firing" plus a fingerprint is the whole diagnosis for a truncated `max_alerts` payload
/// or for state lost across a restart.
fn orphan_view(fingerprint: &Fingerprint, at: DateTime<Utc>) -> AlertView {
    AlertView {
        fingerprint: fingerprint.clone(),
        labels: [("alertname".to_owned(), "(untracked alert)".to_owned())]
            .into_iter()
            .collect(),
        // The fingerprint goes in the *summary* as well as in the view's own field. The
        // built-in `resolved` template renders `summary` and does not render `fingerprint`
        // — reasonably, because for a tracked alert the alertname and labels say far more.
        // Here they say nothing, and the fingerprint is the only handle anybody has for
        // finding this alert in Alertmanager. A message that omitted it would be a
        // notification that something resolved without saying what.
        annotations: [(
            "summary".to_owned(),
            format!(
                "alertthread has no record of alert `{fingerprint}` firing, so this \
                 resolution could not be correlated to a message. Its state was lost, or \
                 Alertmanager's max_alerts truncated it out of the firing notification — \
                 see alertthread_orphan_resolves_total and ADR 001 D8."
            ),
        )]
        .into_iter()
        .collect(),
        // Zero duration rather than an invented one: the relay genuinely does not know when
        // this alert started, and "firing for 4 days" would be a number somebody acts on.
        starts_at: at,
        resolved_at: Some(at),
        generator_url: String::new(),
    }
}

/// Times a Slack call for `alertthread_slack_call_duration_seconds`.
struct Timed {
    started: Instant,
    method: SlackMethod,
}

impl Timed {
    fn start(method: SlackMethod) -> Self {
        Self {
            started: Instant::now(),
            method,
        }
    }

    /// Records the call and returns its result unchanged.
    fn finish<T>(self, metrics: &Metrics, result: Result<T, SlackError>) -> Result<T, SlackError> {
        let seconds = self.started.elapsed().as_secs_f64();
        match &result {
            Ok(_) => metrics.slack_ok(self.method, seconds),
            Err(error) => metrics.slack_failed(self.method, error, seconds),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, Outcome, group_view, orphan_view, resolved_view, view_of};
    use alertthread_core::{ChannelId, Fingerprint, GroupKey, LabelMap, MessageTs, ThreadTs};
    use alertthread_slack::{SlackError, SlackMethod};
    use alertthread_store::{AlertRecord, AlertState, GroupMembership, GroupRecord, OpEffect};
    use chrono::{DateTime, TimeDelta, Utc};
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn record() -> AlertRecord {
        AlertRecord {
            fingerprint: Fingerprint::new("abc"),
            channel: ChannelId::new("#alerts"),
            state: AlertState::Posted,
            message_ts: Some(MessageTs::new("1.1")),
            thread_parent_ts: None,
            group_key: Some(GroupKey::new("gk")),
            first_seen: at(1_000),
            last_seen: at(2_000),
            resolved_at: None,
            labels: [("alertname".to_owned(), "CephOSDDown".to_owned())]
                .into_iter()
                .collect(),
            annotations: [
                ("summary".to_owned(), "osd.3 is down".to_owned()),
                (
                    "generatorURL".to_owned(),
                    "http://prometheus/graph".to_owned(),
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn a_view_carries_the_stored_state_a_message_describes() {
        let view = view_of(&record());
        assert_eq!(view.fingerprint, Fingerprint::new("abc"));
        assert_eq!(view.starts_at, at(1_000));
        assert_eq!(view.resolved_at, None);
        assert_eq!(view.generator_url, "http://prometheus/graph");
        assert_eq!(
            view.labels.get("alertname").map(String::as_str),
            Some("CephOSDDown")
        );
    }

    #[test]
    fn an_alert_with_no_generator_url_renders_an_empty_link_rather_than_a_broken_one() {
        let mut record = record();
        record.annotations.remove("generatorURL");
        assert_eq!(view_of(&record).generator_url, "");
    }

    #[test]
    fn a_resolve_always_renders_as_resolved_even_if_the_row_forgot_when() {
        // A resolve op exists because a resolution was accepted. Rendering one as still
        // firing would leave a red message for an alert that has cleared, which is the
        // exact bug this project exists to fix.
        let mut record = record();
        record.resolved_at = None;
        assert_eq!(
            resolved_view(&record, at(9_000)).resolved_at,
            Some(at(9_000))
        );

        record.resolved_at = Some(at(3_000));
        assert_eq!(
            resolved_view(&record, at(9_000)).resolved_at,
            Some(at(3_000))
        );
    }

    fn group() -> GroupRecord {
        GroupRecord {
            group_key: GroupKey::new("gk"),
            channel: ChannelId::new("#alerts"),
            message_ts: Some(ThreadTs::new("1.0")),
            member_count: 6,
            group_labels: [("alertname".to_owned(), "KubePodNotReady".to_owned())]
                .into_iter()
                .collect(),
            created_at: at(1_000),
        }
    }

    #[test]
    fn a_summary_reports_the_live_split_the_store_counted() {
        let view = group_view(
            &GroupKey::new("gk"),
            Some(&group()),
            GroupMembership {
                firing: 4,
                resolved: 2,
            },
            0,
        );
        assert_eq!(view.firing, 4);
        assert_eq!(view.resolved, 2);
        assert_eq!(view.total(), 6);
        assert_eq!(
            view.labels.get("alertname").map(String::as_str),
            Some("KubePodNotReady")
        );
    }

    #[test]
    fn a_summary_never_claims_fewer_members_than_the_batch_that_opened_it() {
        // The parent posts before its children (ADR 001 D5), so at the moment it is
        // rendered the store may not have recorded them yet. "1 of 1 firing" over a thread
        // of six replies looks like a bug to whoever is counting them.
        let view = group_view(
            &GroupKey::new("gk"),
            Some(&group()),
            GroupMembership::default(),
            6,
        );
        assert_eq!(view.firing, 6);
    }

    #[test]
    fn a_summary_for_a_group_row_that_is_gone_still_renders() {
        // The pruner deletes resolved alerts before their parent, so a summary can outlive
        // the labels it would have been titled with. `GroupVars` has a fallback chain for
        // exactly this; what matters here is that the absence is not an error.
        let view = group_view(
            &GroupKey::new("gk"),
            None,
            GroupMembership {
                firing: 0,
                resolved: 3,
            },
            0,
        );
        assert_eq!(view.labels, LabelMap::new());
        assert!(view.all_resolved());
    }

    #[test]
    fn an_orphan_resolve_says_what_it_does_not_know() {
        // ⚠️ The op carries a fingerprint and nothing else, so this is everything the
        // message can say. It is deliberately explicit about that rather than rendering a
        // blank alert, because the fingerprint plus "no record of this firing" is the whole
        // diagnosis for a truncated max_alerts payload (ADR 001 D8).
        let view = orphan_view(&Fingerprint::new("a1b2c3"), at(5_000));
        assert_eq!(view.fingerprint, Fingerprint::new("a1b2c3"));
        assert_eq!(view.resolved_at, Some(at(5_000)));
        assert_eq!(
            view.starts_at,
            at(5_000),
            "an invented duration is worse than none"
        );
        let summary = view
            .annotations
            .get("summary")
            .expect("the message has to explain itself");
        assert!(summary.contains("no record"), "{summary}");
        assert!(summary.contains("max_alerts"), "{summary}");
        assert!(
            summary.contains("alertthread_orphan_resolves_total"),
            "{summary}"
        );
    }

    // -- Backoff and the D9 failure table ----------------------------------------------

    fn backoff() -> Backoff {
        Backoff::default()
    }

    #[test]
    fn the_backoff_defaults_are_the_ones_adr_001_d9_specifies() {
        let policy = backoff();
        assert_eq!(policy.max_attempts, 10, "D9: default 10 attempts");
        assert_eq!(policy.base, TimeDelta::seconds(4));
        assert_eq!(policy.max, TimeDelta::minutes(10));
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_doubling() {
        let policy = backoff();
        let first = policy.delay(1);
        let second = policy.delay(2);
        let third = policy.delay(3);

        assert!(second > first, "{first} then {second}");
        assert!(third > second, "{second} then {third}");
        // The cap holds however many attempts are configured. Without it, attempt 40 would
        // schedule an alert for delivery some time next century.
        assert!(policy.delay(40) <= policy.max, "{}", policy.delay(40));
        assert!(policy.delay(i32::MAX) <= policy.max);
    }

    #[test]
    fn the_backoff_schedule_is_exactly_this_and_not_merely_increasing() {
        // Pinned to the instant, not to an ordering. "Second is longer than first" is true
        // of a great many schedules, including several that would hammer Slack or park an
        // alert for a fortnight — mutation testing found that every assertion here was of
        // that shape, so the arithmetic underneath could be changed without a test caring.
        //
        // Derivation, for whoever has to change these numbers on purpose:
        //
        //   attempt 1: 4s << 0 =  4s, spread  500ms × (1%3 - 1 =  0) =      0 →  4s
        //   attempt 2: 4s << 1 =  8s, spread 1000ms × (2%3 - 1 =  1) = +1000 →  9s
        //   attempt 3: 4s << 2 = 16s, spread 2000ms × (3%3 - 1 = -1) = -2000 → 14s
        let policy = backoff();
        assert_eq!(
            policy.delay(1),
            TimeDelta::seconds(4),
            "no spread on attempt 1"
        );
        assert_eq!(
            policy.delay(2),
            TimeDelta::seconds(9),
            "spread runs forward"
        );
        assert_eq!(policy.delay(3), TimeDelta::seconds(14), "and backward");
    }

    #[test]
    fn a_retry_is_scheduled_for_the_backoff_and_not_for_the_past() {
        // The `until` is what the outbox sleeps on. An arithmetic slip here does not fail
        // loudly: it schedules the retry in the past, the op is immediately leasable
        // again, and the relay spins on Slack at whatever rate the worker loops — which
        // reads as a rate-limit problem a long way from its cause.
        let policy = backoff();
        let error = SlackError::Unrecognised {
            method: SlackMethod::PostMessage,
            code: "some_future_slack_error".to_owned(),
        };
        let Outcome::Retry { until, .. } = policy.classify(&error, 2, at(100)) else {
            panic!("an unrecognised error retries");
        };
        assert_eq!(until, at(100) + policy.delay(2));
        assert!(
            until > at(100),
            "a retry is always in the future, got {until}"
        );
    }

    #[test]
    fn the_total_backoff_reaches_roughly_the_half_hour_d9_describes() {
        // D9: "exponential backoff with jitter, up to `max_attempts` (default 10, ~30 min)".
        let policy = backoff();
        // `1..max_attempts`, not `1..=`: the last attempt dead-letters rather than
        // waiting, so its delay is never spent.
        let total: TimeDelta = (1..policy.max_attempts)
            .map(|attempt| policy.delay(attempt))
            .fold(TimeDelta::zero(), |acc, delay| acc + delay);
        assert!(
            total >= TimeDelta::minutes(20) && total <= TimeDelta::minutes(40),
            "ten attempts should span roughly half an hour, got {total}"
        );
    }

    #[test]
    fn a_backoff_is_never_negative_and_never_panics_on_a_silly_attempt_count() {
        let policy = backoff();
        for attempts in [i32::MIN, -1, 0, 1, 63, 64, i32::MAX] {
            let delay = policy.delay(attempts);
            assert!(delay >= TimeDelta::zero(), "{attempts} gave {delay}");
        }
    }

    #[test]
    fn ops_at_different_attempt_counts_do_not_return_in_the_same_instant() {
        // What jitter is actually for: a hundred ops deferred by one outage coming back
        // together would reproduce the outage's load in a single millisecond.
        let policy = backoff();
        let delays: Vec<_> = (4..=6).map(|attempt| policy.delay(attempt)).collect();
        assert_ne!(delays[0], delays[1]);
        assert_ne!(delays[1], delays[2]);
    }

    #[test]
    fn a_rate_limit_is_not_a_failed_attempt() {
        // ADR 001 D2 and D9, and the single most important row of the table: if a 429 ever
        // consumed an attempt, an alert storm would dead-letter its own alerts.
        let error = SlackError::RateLimited {
            method: SlackMethod::PostMessage,
            retry_after: Duration::from_secs(30),
        };
        assert_eq!(
            backoff().classify(&error, 9, at(0)),
            Outcome::Wait { until: at(30) }
        );
        // Even on the very last attempt: a rate limit never dead-letters.
        assert_eq!(
            backoff().classify(&error, 10_000, at(0)),
            Outcome::Wait { until: at(30) }
        );
    }

    #[test]
    fn a_message_that_is_gone_is_replaced_rather_than_retried() {
        // ADR 001 D7's free liveness probe. `complete(MessageLost)` clears the stale
        // timestamp and enqueues a fresh post in the same transaction.
        let error = SlackError::MessageNotFound {
            method: SlackMethod::UpdateMessage,
            code: "message_not_found".to_owned(),
        };
        assert_eq!(
            backoff().classify(&error, 1, at(0)),
            Outcome::Done(OpEffect::MessageLost)
        );
    }

    #[test]
    fn a_terminal_failure_is_parked_without_burning_retries() {
        // D9: "dead-letter immediately, do not burn retries, fire a metric." A token does
        // not become valid by being tried ten more times.
        let error = SlackError::InvalidAuth {
            method: SlackMethod::PostMessage,
            code: "invalid_auth".to_owned(),
        };
        let outcome = backoff().classify(&error, 1, at(0));
        assert!(
            matches!(
                outcome,
                Outcome::DeadLetter {
                    reason: "invalid_auth",
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_transient_failure_is_retried_until_its_attempts_run_out() {
        let error = SlackError::SlackUnavailable {
            method: SlackMethod::PostMessage,
            code: "internal_error".to_owned(),
        };
        let policy = backoff();

        assert!(matches!(
            policy.classify(&error, 1, at(0)),
            Outcome::Retry { .. }
        ));
        assert!(matches!(
            policy.classify(&error, policy.max_attempts - 1, at(0)),
            Outcome::Retry { .. }
        ));

        let exhausted = policy.classify(&error, policy.max_attempts, at(0));
        let Outcome::DeadLetter { reason, detail } = exhausted else {
            panic!("attempts run out eventually: {exhausted:?}");
        };
        assert_eq!(reason, "slack_unavailable");
        assert!(detail.contains("10 attempts"), "{detail}");
    }

    #[test]
    fn a_retry_records_what_went_wrong_for_the_operator_reading_the_row() {
        // `last_error` on a stuck outbox row is often the only evidence of what happened.
        let error = SlackError::SlackUnavailable {
            method: SlackMethod::PostMessage,
            code: "service_unavailable".to_owned(),
        };
        let Outcome::Retry { error: detail, .. } = backoff().classify(&error, 1, at(0)) else {
            panic!("a 5xx is retryable");
        };
        assert!(detail.contains("service_unavailable"), "{detail}");
    }

    #[test]
    fn an_error_code_this_build_does_not_know_is_retried_rather_than_parked() {
        // Both classifications end in a dead-letter; the only question is whether a
        // transient-but-unfamiliar failure gets a chance first. AGENTS.md resolves that in
        // one direction only.
        let error = SlackError::Unrecognised {
            method: SlackMethod::PostMessage,
            code: "some_future_slack_error".to_owned(),
        };
        assert!(matches!(
            backoff().classify(&error, 1, at(0)),
            Outcome::Retry { .. }
        ));
    }

    #[test]
    fn a_retry_after_beyond_what_a_duration_can_hold_still_schedules_something_sane() {
        // The client clamps `Retry-After` already; this is the belt to that's braces,
        // because the alternative on overflow is a panic in the delivery path.
        let error = SlackError::RateLimited {
            method: SlackMethod::PostMessage,
            retry_after: Duration::MAX,
        };
        let Outcome::Wait { until } = backoff().classify(&error, 1, at(0)) else {
            panic!("a rate limit always waits");
        };
        assert_eq!(until, at(1));
    }

    #[test]
    fn every_outcome_is_debuggable() {
        // These end up in log lines at the moment somebody is trying to work out why an
        // alert did not arrive.
        assert!(format!("{:?}", Outcome::Done(OpEffect::Resolved)).contains("Resolved"));
        assert!(format!("{:?}", Outcome::Wait { until: at(0) }).contains("Wait"));
        assert!(format!("{:?}", backoff()).contains("max_attempts"));
    }
}
