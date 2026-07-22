//! The outbox worker: lease a batch, fan out by channel, drain each channel serially.
//!
//! # Why this shape and not the two obvious ones
//!
//! Slack allows roughly one `chat.postMessage` per second **per channel** (ADR 001 §1).
//! That single fact rules out both alternatives:
//!
//! - **A single serial drain** would collapse total throughput to about one message per
//!   second *across every channel*, and one busy channel would starve every other. A
//!   fifteen-alert storm in `#alerts-critical` would hold up a single alert in `#database`
//!   for fifteen seconds, for no reason Slack asked for.
//! - **N independent workers, each leasing separately**, lets two workers hold ops for the
//!   same channel and then contend on one token bucket. That is head-of-line blocking whose
//!   behaviour depends on lease timing, which makes it both unpleasant to reason about and
//!   unpleasant to test.
//!
//! So: **one lease, grouped by channel, channels concurrent, ops within a channel serial.**
//! Ordering within a channel is what makes the storm-collapse parent land before its
//! children, and concurrency across channels is what keeps them independent.
//!
//! # A channel whose bucket is empty defers rather than sleeping
//!
//! [`crate::delivery`] returns [`Outcome::Wait`] with the instant a token appears, and the
//! op goes back to the queue with its attempt returned. That reuses ADR 001 D2's existing
//! self-deferral rather than inventing a second waiting mechanism, and — more importantly —
//! a worker that slept instead would hold a 60-second lease across a `Retry-After` that
//! Slack routinely makes longer than that. The lease would expire, another worker would
//! reclaim the row and post it, and the sleeper would wake up and post it too.
//!
//! # Shutdown drains rather than abandons
//!
//! [`Worker::run`] stops leasing when the shutdown token fires but finishes the batch it is
//! holding. An abandoned lease is not a bug — it expires and is reclaimed — but a clean
//! shutdown should not *rely* on expiry, because expiry costs a full lease duration of
//! delay on an alert somebody is waiting for.

use std::collections::HashMap;
use std::sync::Arc;

use alertthread_core::{ChannelId, Op};
use alertthread_slack::{Renderer, SlackClient};
use alertthread_store::{Deferral, LeasedOp, RetentionPolicy, StateStore, StoreError, WorkerId};
use chrono::{DateTime, TimeDelta, Utc};

use crate::config::WorkerConfig;
use crate::delivery::{Backoff, Delivery, Outcome};
use crate::metrics::Metrics;
use crate::ratelimit::SlackLimits;
use crate::shutdown::CancelToken;

/// Drains the outbox.
///
/// Cloneable and cheap: everything inside is behind an `Arc`.
pub struct Worker<S: StateStore> {
    store: Arc<S>,
    slack: Arc<SlackClient>,
    renderer: Arc<Renderer>,
    limits: Arc<SlackLimits>,
    metrics: Arc<Metrics>,
    config: WorkerConfig,
    id: WorkerId,
}

/// What one pass of the worker did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// How many ops were leased.
    pub leased: usize,
    /// How many were delivered and removed from the queue.
    pub completed: usize,
    /// How many went back for another go, by either kind of deferral.
    pub deferred: usize,
    /// How many were parked. **Every one of these is an alert nobody was told about.**
    pub dead_lettered: usize,
}

impl Pass {
    /// Whether this pass found anything at all to do.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.leased == 0
    }
}

/// Hand-written rather than derived.
///
/// `#[derive(Clone)]` would add a `S: Clone` bound, which the store does not need to satisfy
/// — everything here is behind an `Arc`. Worse, the bound is not an error at the definition:
/// it makes `self.clone()` in `run_once` resolve to `<&Worker<S>>::clone`, which compiles
/// and then fails with a lifetime error two hundred lines away.
impl<S: StateStore> Clone for Worker<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            slack: Arc::clone(&self.slack),
            renderer: Arc::clone(&self.renderer),
            limits: Arc::clone(&self.limits),
            metrics: Arc::clone(&self.metrics),
            config: self.config,
            id: self.id.clone(),
        }
    }
}

impl<S: StateStore + 'static> Worker<S> {
    /// Builds a worker.
    pub fn new(
        store: Arc<S>,
        slack: Arc<SlackClient>,
        renderer: Arc<Renderer>,
        limits: Arc<SlackLimits>,
        metrics: Arc<Metrics>,
        config: WorkerConfig,
        id: WorkerId,
    ) -> Self {
        Self {
            store,
            slack,
            renderer,
            limits,
            metrics,
            config,
            id,
        }
    }

    /// The backoff policy this worker was configured with.
    fn backoff(&self) -> Backoff {
        Backoff {
            max_attempts: self.config.max_attempts,
            base: self.config.backoff_base,
            max: self.config.backoff_max,
        }
    }

    /// Leases a batch and drains it.
    ///
    /// Returns what it did, so a caller can decide whether to poll again immediately or
    /// wait: a pass that emptied its batch probably has more waiting behind it.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the lease itself failed. A failure *within* a batch is recorded
    /// against that op and does not abandon the rest — one undecodable payload must not
    /// stop every other alert in the queue from being delivered.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<Pass, StoreError> {
        let leased = self
            .store
            .lease_batch(&self.id, self.config.batch_size, self.config.lease, now)
            .await?;

        if leased.is_empty() {
            return Ok(Pass::default());
        }

        let total = leased.len();
        // Grouped by channel, and the *order within each group* is the leased order, which
        // is id order, which is plan order — that is what keeps a storm-collapse parent
        // ahead of its children (ADR 001 D5). The order the channels themselves come out in
        // does not matter, because they are about to be run concurrently.
        let mut by_channel: HashMap<ChannelId, Vec<LeasedOp>> = HashMap::new();
        for op in leased {
            by_channel
                .entry(channel_of(&op.op).clone())
                .or_default()
                .push(op);
        }

        // Channels concurrently, ops within a channel serially. The serial half is what
        // keeps a storm-collapse parent ahead of its children; the concurrent half is what
        // keeps one busy channel from starving every other.
        let mut tasks = Vec::with_capacity(by_channel.len());
        for (channel, ops) in by_channel {
            let worker = self.clone();
            tasks.push(tokio::spawn(async move {
                worker.drain_channel(&channel, ops, now).await
            }));
        }

        let mut pass = Pass {
            leased: total,
            ..Pass::default()
        };
        for task in tasks {
            match task.await {
                Ok(channel_pass) => pass.merge(channel_pass),
                Err(error) => {
                    // A panicked task. Its ops keep their leases and are reclaimed when they
                    // expire, so nothing is lost — but a silent join failure would make a
                    // queue that stops draining look like a queue with nothing in it.
                    tracing::error!(%error, "an outbox channel task did not finish");
                }
            }
        }

        Ok(pass)
    }

    /// Delivers one channel's ops, in order.
    async fn drain_channel(
        &self,
        channel: &ChannelId,
        ops: Vec<LeasedOp>,
        now: DateTime<Utc>,
    ) -> Pass {
        let mut pass = Pass::default();
        for op in ops {
            match self.deliver(&op, now).await {
                Ok(counted) => pass.merge(counted),
                Err(error) => {
                    // The store failed while delivering this op — not Slack. The lease
                    // stands and the row is reclaimed when it expires, which is the same
                    // recovery a crashed worker gets. Loud, because a store that cannot be
                    // read is the one condition the outbox cannot ride out silently.
                    tracing::error!(
                        %channel,
                        op = %op.id,
                        %error,
                        "could not deliver an outbox row; its lease will expire and it will \
                         be retried"
                    );
                }
            }
        }
        pass
    }

    /// Delivers one op and records what happened to it.
    async fn deliver(&self, leased: &LeasedOp, now: DateTime<Utc>) -> Result<Pass, StoreError> {
        let delivery = Delivery {
            store: self.store.as_ref(),
            slack: self.slack.as_ref(),
            renderer: self.renderer.as_ref(),
            limits: self.limits.as_ref(),
            metrics: self.metrics.as_ref(),
            backoff: self.backoff(),
        };

        let outcome = delivery.run(leased, now).await?;
        self.apply(leased, outcome, now).await
    }

    /// Writes an [`Outcome`] back to the store.
    async fn apply(
        &self,
        leased: &LeasedOp,
        outcome: Outcome,
        now: DateTime<Utc>,
    ) -> Result<Pass, StoreError> {
        let mut pass = Pass::default();
        match outcome {
            Outcome::Done(effect) => {
                self.store.complete(leased.id, &effect, now).await?;
                pass.completed = 1;
            }
            Outcome::Wait { until } => {
                // The attempt is given back: this is Slack scheduling us, or the relay
                // pacing itself. Neither is the op failing (ADR 001 D2).
                self.store
                    .defer(leased.id, &Deferral::RateLimited { until })
                    .await?;
                pass.deferred = 1;
            }
            Outcome::Retry { until, error } => {
                self.store
                    .defer(leased.id, &Deferral::Backoff { until, error })
                    .await?;
                pass.deferred = 1;
            }
            Outcome::DeadLetter { reason, detail } => {
                // ADR 001 D9: log the full payload at ERROR. The row is the only record of
                // an alert that never reached Slack, and this line is the only place it
                // appears in a log aggregator.
                tracing::error!(
                    op = %leased.id,
                    kind = ?leased.op,
                    attempts = leased.attempts,
                    reason,
                    detail,
                    "dead-lettering an outbox row: this alert did not reach Slack"
                );
                self.metrics.dead_lettered(reason);
                self.store.dead_letter(leased.id, &detail, now).await?;
                pass.dead_lettered = 1;
            }
        }
        Ok(pass)
    }

    /// Drains the outbox until shutdown is signalled.
    ///
    /// Polls rather than being woken. A `LISTEN`/`NOTIFY` would be PostgreSQL-only, and the
    /// SQLite deployment would then need a second mechanism doing the same job — which is
    /// exactly the divergence two backends behind one trait exist to avoid. The poll
    /// interval is short because it is also how long a self-deferred op waits past its
    /// `next_attempt_at`.
    pub async fn run(&self, shutdown: CancelToken) {
        while !shutdown.is_cancelled() {
            let pass = match self.run_once(Utc::now()).await {
                Ok(pass) => pass,
                Err(error) => {
                    tracing::error!(%error, "could not lease outbox work");
                    // Back off on a store failure rather than spinning: the store being
                    // unreachable is precisely when hammering it helps least.
                    sleep_or_shutdown(&shutdown, self.config.lease.min(TimeDelta::seconds(5)))
                        .await;
                    continue;
                }
            };

            if should_pause(pass.leased, self.config.batch_size as usize) {
                sleep_or_shutdown(&shutdown, self.config.idle_poll).await;
            }
        }

        // The batch in hand is already finished — `run_once` returns only after its channel
        // tasks have joined — so there is nothing left to drain here. Saying so explicitly
        // is what stops somebody adding a "release the leases" step that would race the
        // work it was releasing.
        tracing::info!(worker = %self.id, "outbox worker stopped");
    }
}

impl Pass {
    fn merge(&mut self, other: Self) {
        self.leased += other.leased;
        self.completed += other.completed;
        self.deferred += other.deferred;
        self.dead_lettered += other.dead_lettered;
    }
}

/// Sweeps finished correlation state on its own schedule (ADR 001 D4; PRD §5.7).
///
/// A separate task from the worker on purpose. Retention runs hourly and delivery runs four
/// times a second; folding the sweep into the worker loop would either run it far too often
/// or make the delivery interval hostage to how long a `DELETE` takes.
pub async fn prune_loop<S: StateStore>(
    store: Arc<S>,
    policy: RetentionPolicy,
    interval: TimeDelta,
    shutdown: CancelToken,
) {
    while !shutdown.is_cancelled() {
        match store.prune(&policy, Utc::now()).await {
            Ok(stats) if stats.is_empty() => {}
            Ok(stats) => tracing::info!(
                resolved_alerts = stats.resolved_alerts,
                stale_alerts = stats.stale_alerts,
                empty_groups = stats.empty_groups,
                "pruned finished correlation state"
            ),
            // Never fatal. A pruner that cannot run costs disk; a relay that stopped because
            // its pruner failed costs alerts.
            Err(error) => tracing::error!(%error, "retention sweep failed"),
        }
        sleep_or_shutdown(&shutdown, interval).await;
    }
}

/// Samples the store for ADR 001 D11's gauges.
///
/// Deliberately not inside `GET /metrics`: a scrape every 15 s from every replica would make
/// Prometheus a load generator pointed at the outbox, and a slow store would time the scrape
/// out and take every other metric with it.
pub async fn sample_loop<S: StateStore>(
    store: Arc<S>,
    metrics: Arc<Metrics>,
    interval: TimeDelta,
    shutdown: CancelToken,
) {
    while !shutdown.is_cancelled() {
        match store.stats().await {
            Ok(stats) => metrics.publish(&stats, Utc::now()),
            Err(error) => {
                // The gauges keep their last values; `store_sample_ok` is what says they
                // are stale. Zeroing them would claim the queue is empty, which is the most
                // misleading thing this relay could say while its store is unreachable.
                metrics.sample_failed();
                tracing::error!(%error, "could not sample the store for metrics");
            }
        }
        sleep_or_shutdown(&shutdown, interval).await;
    }
}

/// Re-checks the bot token, and reports the answer as a metric.
///
/// **Not as readiness.** Startup already fails fast on a bad token (ADR 001 D11); what this
/// covers is mid-life revocation — a token revoked at 2pm with nothing firing until 3am.
/// Feeding it into `/readyz` instead would make every replica unready at once (they share
/// one token), Alertmanager's POST would fail, it would retry a few times and give up, and
/// the alert would be **lost**. Accepting the webhook into the outbox and retrying is
/// exactly what the outbox is for.
pub async fn auth_probe_loop(
    slack: Arc<SlackClient>,
    metrics: Arc<Metrics>,
    interval: TimeDelta,
    shutdown: CancelToken,
) {
    while !shutdown.is_cancelled() {
        match slack.auth_test().await {
            Ok(identity) => {
                metrics.slack_auth_valid.set(1);
                tracing::debug!(team = %identity.team, user = %identity.user, "bot token is valid");
            }
            Err(error) => {
                metrics.slack_auth_valid.set(0);
                tracing::error!(
                    %error,
                    "Slack rejected the bot token; queued alerts will not be delivered until \
                     it is replaced. This does not make the relay unready — refusing \
                     webhooks would lose alerts the outbox could have held."
                );
            }
        }
        sleep_or_shutdown(&shutdown, interval).await;
    }
}

/// Whether the worker should wait before leasing again.
///
/// A pass that filled its batch probably has more behind it, so it goes straight round
/// again. Anything short of a full batch waits — which is the only thing stopping an idle
/// relay from spinning against the store as fast as the loop turns.
const fn should_pause(leased: usize, batch_size: usize) -> bool {
    leased < batch_size
}

/// Waits, or returns early when shutdown is signalled.
///
/// The early return is why this exists: an hourly pruner that only checked its flag after
/// sleeping would keep a container alive for up to an hour past `SIGTERM`, and Kubernetes
/// would `SIGKILL` it instead — mid-delivery, which is the one moment worth avoiding.
async fn sleep_or_shutdown(shutdown: &CancelToken, delay: TimeDelta) {
    let Ok(delay) = delay.to_std() else {
        // A negative or absurd interval. Yield rather than sleeping forever or panicking:
        // the loops above all re-check the shutdown flag immediately afterwards.
        tokio::task::yield_now().await;
        return;
    };
    tokio::select! {
        () = tokio::time::sleep(delay) => {}
        () = shutdown.cancelled() => {}
    }
}

/// Which channel an op is addressed to.
///
/// Every op has one — the channel comes from the request, not from the store (ADR 001 D8) —
/// which is what makes grouping by channel total rather than best-effort.
const fn channel_of(op: &Op) -> &ChannelId {
    match op {
        Op::Post { channel, .. }
        | Op::PostGroup { channel, .. }
        | Op::Refresh { channel, .. }
        | Op::RefreshGroup { channel, .. }
        | Op::Resolve { channel, .. }
        | Op::PostOrphanResolved { channel, .. } => channel,
    }
}

#[cfg(test)]
mod tests {
    use super::{Pass, channel_of};
    use crate::shutdown::cancellation;
    use alertthread_core::{
        ChannelId, Fingerprint, GroupKey, MessageTs, Op, Placement, ResolveTarget, ThreadTs,
    };

    fn channel() -> ChannelId {
        ChannelId::new("#alerts")
    }

    #[test]
    fn every_op_names_the_channel_it_is_addressed_to() {
        // Grouping by channel is what makes the per-channel rate limit enforceable. An op
        // that could not name one would be unschedulable.
        let ops = [
            Op::Post {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                placement: Placement::Channel,
            },
            Op::PostGroup {
                group_key: GroupKey::new("gk"),
                channel: channel(),
                initial_members: 6,
            },
            Op::Refresh {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                message_ts: MessageTs::new("1.1"),
            },
            Op::RefreshGroup {
                group_key: GroupKey::new("gk"),
                channel: channel(),
                message_ts: ThreadTs::new("1.0"),
            },
            Op::Resolve {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
                target: ResolveTarget::AwaitingPost,
                update_in_place: true,
                thread_reply: true,
            },
            Op::PostOrphanResolved {
                fingerprint: Fingerprint::new("abc"),
                channel: channel(),
            },
        ];
        for op in &ops {
            assert_eq!(channel_of(op), &channel(), "{op:?}");
        }
    }

    #[test]
    fn only_a_full_batch_goes_straight_round_again() {
        // Both boundaries. Pausing on a full batch costs throughput under load; not
        // pausing on a short one spins against the store as fast as the loop turns, which
        // burns a core and answers nothing.
        assert!(super::should_pause(0, 10), "an empty queue waits");
        assert!(super::should_pause(9, 10), "a short batch waits");
        assert!(!super::should_pause(10, 10), "a full batch does not");
        assert!(!super::should_pause(11, 10), "nor an over-full one");
    }

    #[test]
    fn a_pass_that_leased_nothing_is_idle() {
        assert!(Pass::default().is_idle());
        assert!(
            !Pass {
                leased: 1,
                ..Pass::default()
            }
            .is_idle()
        );
    }

    #[test]
    fn passes_merge_field_by_field() {
        // Each counter separately: a hand-written merge that forgot one would make a worker
        // that only ever dead-letters look idle.
        let mut pass = Pass {
            leased: 1,
            completed: 1,
            deferred: 0,
            dead_lettered: 0,
        };
        pass.merge(Pass {
            leased: 2,
            completed: 0,
            deferred: 1,
            dead_lettered: 1,
        });
        assert_eq!(
            pass,
            Pass {
                leased: 3,
                completed: 1,
                deferred: 1,
                dead_lettered: 1,
            }
        );
        assert!(format!("{pass:?}").contains("dead_lettered"));
    }

    #[tokio::test]
    async fn a_cancel_token_reports_and_wakes() {
        let (source, token) = cancellation();
        assert!(!token.is_cancelled());

        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        source.cancel();
        handle.await.expect("the waiter wakes");

        assert!(token.is_cancelled());
        // Idempotent: shutdown is signalled from a signal handler that may fire twice.
        source.cancel();
        assert!(token.is_cancelled());
        // And already-cancelled resolves immediately rather than hanging.
        token.cancelled().await;
    }

    #[tokio::test]
    async fn a_token_whose_source_is_gone_reads_as_cancelled() {
        // Otherwise a background task would outlive the thing that was supposed to stop it,
        // and the process would never exit.
        let (source, token) = cancellation();
        drop(source);
        token.cancelled().await;
    }

    #[tokio::test]
    async fn a_nonsensical_interval_yields_rather_than_sleeping_for_ever() {
        // A negative `sample_interval` in a ConfigMap must not be able to park a background
        // task permanently, and must not panic either.
        let (_source, token) = cancellation();
        super::sleep_or_shutdown(&token, chrono::TimeDelta::seconds(-1)).await;
    }
}
