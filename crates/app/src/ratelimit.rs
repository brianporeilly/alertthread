//! The token bucket that stands between the outbox and Slack.
//!
//! # Why this exists at all
//!
//! **Slack allows roughly one `chat.postMessage` per second per channel**, thread replies
//! included (ADR 001 §1, fact 1). It is a "Special Tier" limit, distinct from the numbered
//! tiers. Exceeding it earns a 429 with a `Retry-After`, which the outbox handles correctly
//! — but handling it correctly still costs a round trip, a lease, and an attempt's worth of
//! delay on an alert somebody is waiting for. Pacing ourselves is cheaper than being paced.
//!
//! # Time is a parameter
//!
//! Nothing here reads a clock. `now` arrives from the caller, the same way it does in
//! `alertthread-core`, which is what makes every case below a plain unit test rather than a
//! test with a `sleep` in it. A rate limiter tested by sleeping is a rate limiter tested at
//! one speed, on one machine, and it is the first thing to go flaky in CI.
//!
//! # It never blocks
//!
//! [`RateLimiter::acquire`] returns *when* a token will be available, and never waits for
//! one. That is not a stylistic choice. A worker holding a 60-second lease cannot sleep
//! through a `Retry-After` that Slack routinely makes longer than that: the lease expires,
//! a second worker reclaims the row and posts it, and then the first worker wakes up and
//! posts it too. Releasing the lease is the only safe way to wait, and only the store can
//! release a lease — so the answer to "not yet" is a `next_attempt_at`, not a sleep.

use std::collections::HashMap;
use std::sync::Mutex;

use alertthread_core::ChannelId;
use chrono::{DateTime, TimeDelta, Utc};

/// The key a bucket is kept under.
///
/// `chat.postMessage` is limited per channel; `chat.update` is a Tier 3 method limited per
/// workspace. Both go through one type so the worker cannot reach for the wrong bucket, and
/// the difference is which key the [`SlackLimits`] wrapper hands over.
type BucketKey = String;

/// How many `chat.postMessage` calls Slack allows per channel per second.
pub const POST_PER_SECOND: f64 = 1.0;

/// How many `chat.update` calls Slack's Tier 3 allows per minute, per workspace.
pub const UPDATE_PER_MINUTE: f64 = 50.0;

/// How long an untouched, full bucket is kept before it is forgotten.
///
/// Buckets are keyed by channel, and channels come from an Alertmanager URL parameter — so
/// the key space is operator-controlled but unbounded, and a long-lived process that never
/// forgot a channel would leak one small entry per channel it ever saw. A bucket that is
/// full has no state worth keeping: recreating it produces exactly the same answers.
const IDLE_EVICTION: TimeDelta = TimeDelta::minutes(10);

/// How close to a whole token counts as a whole token.
///
/// A nanosecond's worth at one token per second. See [`RateLimiter::acquire`] for why the
/// slack is needed at all.
const TOKEN_EPSILON: f64 = 1e-9;

/// One channel's bucket.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    /// Tokens available at [`updated`](Self::updated).
    tokens: f64,
    /// When [`tokens`](Self::tokens) was last computed.
    updated: DateTime<Utc>,
}

/// A per-key token bucket.
///
/// Cheap to share: one `Mutex` around a map, held only for the arithmetic. There is no
/// `await` inside the critical section, so a `std::sync::Mutex` is correct here and a
/// `tokio::sync::Mutex` would only add a scheduler round trip.
#[derive(Debug)]
pub struct RateLimiter {
    /// Tokens added per second.
    rate: f64,
    /// The most tokens the bucket ever holds — how much of a quiet period may be spent at
    /// once.
    burst: f64,
    buckets: Mutex<HashMap<BucketKey, Bucket>>,
}

/// What [`RateLimiter::acquire`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permit {
    /// Send it now. A token has been taken.
    Granted,
    /// Not yet. Defer the op until this instant.
    ///
    /// The op keeps its attempt: this is the relay pacing itself, not the op failing, and
    /// counting it would march an alert toward the dead-letter queue for arriving during a
    /// storm — which is exactly when it matters most (ADR 001 D2).
    Wait {
        /// When the next token is available.
        until: DateTime<Utc>,
    },
}

impl RateLimiter {
    /// A limiter allowing `rate` calls per second, with a burst of `burst`.
    ///
    /// A `rate` of zero or less, or a `burst` below one, is clamped rather than rejected: a
    /// configuration mistake must not be able to stop the relay from ever posting anything,
    /// which is the one outcome this project does not accept.
    #[must_use]
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate: if rate.is_finite() && rate > 0.0 {
                rate
            } else {
                POST_PER_SECOND
            },
            burst: if burst.is_finite() && burst >= 1.0 {
                burst
            } else {
                1.0
            },
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes a token for `key`, or says when one will be there.
    ///
    /// Never blocks, and never fails: a poisoned mutex is recovered from rather than
    /// propagated, because the alternative is a relay that stops posting because one task
    /// panicked while holding a lock around two floating-point numbers.
    pub fn acquire(&self, key: &str, now: DateTime<Utc>) -> Permit {
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        self.evict_idle(&mut buckets, now);

        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });

        // Clamped at zero: a `now` that went backwards (clock skew, an NTP step) must not
        // *remove* tokens, which would be a rate limiter that punishes the relay for the
        // machine's timekeeping.
        let elapsed = seconds_between(bucket.updated, now).max(0.0);
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        bucket.updated = now;

        // `- EPSILON`, because a bucket refilled by exactly the wait this limiter itself
        // reported can land a fraction of a nanosecond short in binary floating point. That
        // would defer the op again, to the same instant, for ever — a queue that spins
        // instead of draining, which is silence with a busy CPU.
        if bucket.tokens >= 1.0 - TOKEN_EPSILON {
            bucket.tokens = (bucket.tokens - 1.0).max(0.0);
            return Permit::Granted;
        }

        let shortfall = 1.0 - bucket.tokens;
        let wait = shortfall / self.rate;
        Permit::Wait {
            until: now + delta_from_seconds(wait),
        }
    }

    /// Forgets buckets that have not been touched for [`IDLE_EVICTION`] and would by now
    /// have refilled completely.
    ///
    /// The refill has to be *projected*, not read: a bucket is stored in the state it was
    /// left in, and one left empty an hour ago still says zero. Asking "what would this
    /// bucket hold if somebody asked now?" is the only version of the question with an
    /// answer — and a bucket that would be full has no state worth keeping, because
    /// recreating it produces exactly the same answers.
    ///
    /// A bucket that would still be mid-refill is kept. Dropping one would hand the next
    /// caller a fresh full bucket, which is precisely the rate limit not being applied.
    fn evict_idle(&self, buckets: &mut HashMap<BucketKey, Bucket>, now: DateTime<Utc>) {
        buckets.retain(|_, bucket| {
            let idle = now.signed_duration_since(bucket.updated);
            if idle < IDLE_EVICTION {
                return true;
            }
            let refilled =
                bucket.tokens + seconds_between(bucket.updated, now).max(0.0) * self.rate;
            refilled < self.burst
        });
    }

    /// How many buckets are being tracked. For tests and for the eviction argument above.
    #[must_use]
    pub fn tracked(&self) -> usize {
        match self.buckets.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// The two buckets ADR 001 D2 specifies, with the right key for each.
///
/// Kept together so a caller picks a *method*, not a bucket. The two limits are keyed
/// differently — posts per channel, updates per workspace — and that asymmetry is exactly
/// the kind of detail a call site gets wrong.
#[derive(Debug)]
pub struct SlackLimits {
    post: RateLimiter,
    update: RateLimiter,
}

/// The single key every `chat.update` shares.
///
/// Tier 3 is a per-workspace limit and this relay talks to one workspace, so keying updates
/// per channel would permit 50/min *per channel* and silently multiply the real budget by
/// the number of channels an operator routes to.
const WORKSPACE: &str = "";

impl SlackLimits {
    /// Slack's documented limits, divided by `divisor`.
    ///
    /// `divisor` is ADR 001 D2's `slack.rate_limit_divisor`: with N replicas each holding
    /// its own bucket, the aggregate rate is N times the per-process one, and setting the
    /// divisor to the replica count is the stated mitigation. A divisor below 1 is treated
    /// as 1 — a misconfiguration must not be able to make the relay post *faster* than
    /// Slack allows and then blame the 429s on itself.
    #[must_use]
    pub fn new(divisor: f64) -> Self {
        let divisor = if divisor.is_finite() && divisor >= 1.0 {
            divisor
        } else {
            1.0
        };
        Self {
            post: RateLimiter::new(POST_PER_SECOND / divisor, 1.0),
            // A burst equal to the whole minute's budget: Tier 3 is measured per minute, and
            // refusing to spend it in the first ten seconds would rate-limit a storm's
            // summary refreshes for no reason Slack asked for.
            update: RateLimiter::new(UPDATE_PER_MINUTE / 60.0 / divisor, UPDATE_PER_MINUTE),
        }
    }

    /// A permit for `chat.postMessage` into `channel`, thread replies included.
    pub fn post(&self, channel: &ChannelId, now: DateTime<Utc>) -> Permit {
        self.post.acquire(channel.as_str(), now)
    }

    /// A permit for `chat.update`, anywhere in the workspace.
    pub fn update(&self, now: DateTime<Utc>) -> Permit {
        self.update.acquire(WORKSPACE, now)
    }
}

impl Default for SlackLimits {
    fn default() -> Self {
        Self::new(1.0)
    }
}

/// Seconds between two instants, as a float.
///
/// Via microseconds rather than `num_seconds`, which truncates: a limiter that rounded a
/// 900 ms gap down to zero would never refill under sub-second polling.
#[expect(
    clippy::cast_precision_loss,
    reason = "an i64 microsecond count only loses precision past 2^53 microseconds, which \
              is roughly 285 years between two polls of an outbox"
)]
fn seconds_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    let micros = to.signed_duration_since(from).num_microseconds();
    // `None` only for spans beyond ~292,000 years, which is not a gap between two polls of
    // an outbox. Treated as "very long ago", which refills the bucket — the safe direction,
    // because the alternative is a bucket that never refills and an alert that never posts.
    micros.map_or(f64::MAX, |micros| micros as f64 / 1e6)
}

/// A float number of seconds as a [`TimeDelta`], rounded up to the next microsecond.
///
/// Rounding up matters: rounding down would schedule the retry a hair *before* the token
/// exists, and the op would come back, find an empty bucket, and be deferred again.
#[expect(
    clippy::cast_possible_truncation,
    reason = "clamped into i64 range on the line above, so the cast is exact by construction"
)]
fn delta_from_seconds(seconds: f64) -> TimeDelta {
    // Clamped into range rather than unwrapped. `TimeDelta::microseconds` panics past its
    // bounds, and this workspace denies `panic` in the delivery path for good reason.
    let micros = (seconds * 1e6).ceil().clamp(0.0, 9e15) as i64;
    TimeDelta::microseconds(micros)
}

#[cfg(test)]
mod tests {
    use super::{IDLE_EVICTION, Permit, RateLimiter, SlackLimits, delta_from_seconds};
    use alertthread_core::ChannelId;
    use chrono::{DateTime, TimeDelta, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn channel() -> ChannelId {
        ChannelId::new("#alerts")
    }

    #[test]
    fn the_first_call_for_a_channel_is_granted_immediately() {
        // A relay that made the very first alert of the day wait a second for a bucket to
        // fill would be adding latency to the case it exists to serve.
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);
    }

    #[test]
    fn a_second_call_in_the_same_instant_is_told_when_to_come_back() {
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);
        assert_eq!(
            limiter.acquire("#alerts", at(0)),
            Permit::Wait { until: at(1) },
            "one per second means the next token is a second away"
        );
    }

    #[test]
    fn a_token_is_available_again_once_the_interval_has_passed() {
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);
        assert_eq!(limiter.acquire("#alerts", at(1)), Permit::Granted);
        assert_eq!(limiter.acquire("#alerts", at(2)), Permit::Granted);
    }

    #[test]
    fn the_wait_it_reports_is_exactly_when_the_next_call_succeeds() {
        // The whole contract. The worker writes this instant into `next_attempt_at`, so a
        // value that is even slightly early means the op comes back, finds an empty bucket,
        // and is deferred again — a queue that spins instead of draining.
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);

        let Permit::Wait { until } =
            limiter.acquire("#alerts", at(0) + TimeDelta::milliseconds(250))
        else {
            panic!("the bucket is empty");
        };
        assert_eq!(limiter.acquire("#alerts", until), Permit::Granted);
    }

    #[test]
    fn channels_do_not_take_each_others_tokens() {
        // Slack's limit is per channel. One busy channel starving every other is the
        // failure this keying exists to prevent, and it is the reason the worker fans out
        // by channel rather than draining serially.
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);
        assert_eq!(limiter.acquire("#alerts-critical", at(0)), Permit::Granted);
        assert_eq!(limiter.acquire("#database", at(0)), Permit::Granted);
        assert!(matches!(
            limiter.acquire("#alerts", at(0)),
            Permit::Wait { .. }
        ));
    }

    #[test]
    fn a_bucket_refills_to_its_burst_and_no_further() {
        // Otherwise an hour of quiet would buy an hour's worth of tokens, and the first
        // storm after it would empty them into Slack in one go and earn a 429 for every
        // message.
        let limiter = RateLimiter::new(1.0, 5.0);
        for _ in 0..5 {
            assert_eq!(limiter.acquire("#alerts", at(3_600)), Permit::Granted);
        }
        assert!(matches!(
            limiter.acquire("#alerts", at(3_600)),
            Permit::Wait { .. }
        ));
    }

    #[test]
    fn a_partial_interval_refills_proportionally() {
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted);
        assert_eq!(
            limiter.acquire("#alerts", at(0) + TimeDelta::milliseconds(400)),
            Permit::Wait {
                until: at(0) + TimeDelta::seconds(1)
            }
        );

        // A microsecond late rather than exactly on the second, because the wait is rounded
        // up. Late is the safe direction and early is not, so the assertion is a bound
        // rather than an equality — pinning the exact microsecond would be pinning a
        // floating-point artefact.
        let Permit::Wait { until } =
            limiter.acquire("#alerts", at(0) + TimeDelta::milliseconds(999))
        else {
            panic!("999 ms of a one-second interval is not a whole token");
        };
        assert!(until >= at(1), "{until}");
        assert!(until <= at(1) + TimeDelta::milliseconds(1), "{until}");
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_remove_tokens() {
        // NTP steps happen, and replicas disagree. Subtracting tokens for it would be a
        // limiter that punishes the relay for the machine's timekeeping.
        let limiter = RateLimiter::new(1.0, 2.0);
        assert_eq!(limiter.acquire("#alerts", at(100)), Permit::Granted);
        assert_eq!(limiter.acquire("#alerts", at(90)), Permit::Granted);
        assert!(matches!(
            limiter.acquire("#alerts", at(90)),
            Permit::Wait { .. }
        ));
    }

    #[test]
    fn a_nonsensical_rate_falls_back_to_slacks_documented_one() {
        // A configuration mistake must not be able to stop the relay posting anything at
        // all, which is the one outcome this project does not accept.
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let limiter = RateLimiter::new(rate, 1.0);
            assert_eq!(limiter.acquire("#alerts", at(0)), Permit::Granted, "{rate}");
            assert_eq!(
                limiter.acquire("#alerts", at(1)),
                Permit::Granted,
                "a second later, at one per second: {rate}"
            );
        }
    }

    #[test]
    fn a_nonsensical_burst_still_lets_one_message_through() {
        for burst in [0.0, -3.0, 0.5, f64::NAN] {
            let limiter = RateLimiter::new(1.0, burst);
            assert_eq!(
                limiter.acquire("#alerts", at(0)),
                Permit::Granted,
                "{burst}"
            );
        }
    }

    #[test]
    fn an_idle_full_bucket_is_eventually_forgotten() {
        // Channels come from an operator-controlled URL parameter, so the key space is
        // unbounded. A full bucket has no state worth keeping — recreating it gives exactly
        // the same answers.
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.acquire("#gone", at(0)), Permit::Granted);
        assert_eq!(limiter.tracked(), 1);

        // Refilled and untouched for longer than the eviction window: swept when some other
        // channel next asks for a token.
        let later = at(0) + IDLE_EVICTION + TimeDelta::seconds(1);
        assert_eq!(limiter.acquire("#still-here", later), Permit::Granted);
        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn a_bucket_that_still_owes_tokens_is_not_forgotten() {
        // The one that matters: dropping a half-empty bucket would hand the next caller a
        // fresh full one, which is the rate limit quietly not being applied.
        let limiter = RateLimiter::new(0.001, 1.0);
        assert_eq!(limiter.acquire("#busy", at(0)), Permit::Granted);

        let later = at(0) + IDLE_EVICTION + TimeDelta::seconds(1);
        assert_eq!(limiter.acquire("#other", later), Permit::Granted);
        assert_eq!(limiter.tracked(), 2, "the empty bucket must survive");
        assert!(matches!(
            limiter.acquire("#busy", later),
            Permit::Wait { .. }
        ));
    }

    #[test]
    fn posts_are_limited_per_channel_and_updates_per_workspace() {
        // ADR 001 D2 gives the two methods different limits *and* different scopes. Keying
        // updates per channel would permit 50/min per channel and silently multiply the
        // real budget by however many channels an operator routes to.
        let limits = SlackLimits::default();
        let other = ChannelId::new("#alerts-critical");

        assert_eq!(limits.post(&channel(), at(0)), Permit::Granted);
        assert_eq!(limits.post(&other, at(0)), Permit::Granted);
        assert!(matches!(
            limits.post(&channel(), at(0)),
            Permit::Wait { .. }
        ));

        for _ in 0..50 {
            assert_eq!(limits.update(at(0)), Permit::Granted);
        }
        assert!(matches!(limits.update(at(0)), Permit::Wait { .. }));
    }

    #[test]
    fn the_replica_divisor_slows_posting_down_by_that_factor() {
        // ADR 001 D2's honest limitation: N replicas hold N buckets, so the aggregate rate
        // is N times the per-process one. Setting the divisor to the replica count is the
        // stated mitigation.
        let limits = SlackLimits::new(4.0);
        assert_eq!(limits.post(&channel(), at(0)), Permit::Granted);
        assert!(
            matches!(limits.post(&channel(), at(3)), Permit::Wait { .. }),
            "at a quarter rate, three seconds is not yet enough"
        );
        assert_eq!(limits.post(&channel(), at(4)), Permit::Granted);
    }

    #[test]
    fn a_divisor_below_one_cannot_make_the_relay_post_faster_than_slack_allows() {
        for divisor in [0.0, 0.25, -2.0, f64::NAN] {
            let limits = SlackLimits::new(divisor);
            assert_eq!(limits.post(&channel(), at(0)), Permit::Granted, "{divisor}");
            assert!(
                matches!(limits.post(&channel(), at(0)), Permit::Wait { .. }),
                "{divisor}"
            );
        }
    }

    #[test]
    fn a_wait_is_rounded_up_rather_than_down() {
        // Rounding down schedules the retry a hair before the token exists, and the op
        // comes straight back to an empty bucket — a queue that spins instead of draining.
        assert_eq!(delta_from_seconds(0.000_000_1), TimeDelta::microseconds(1));
        assert_eq!(delta_from_seconds(0.0), TimeDelta::zero());
        assert_eq!(delta_from_seconds(-5.0), TimeDelta::zero());
        assert_eq!(delta_from_seconds(1.5), TimeDelta::microseconds(1_500_000));
    }

    #[test]
    fn a_limiter_reports_what_it_is_tracking() {
        let limiter = RateLimiter::new(1.0, 1.0);
        assert_eq!(limiter.tracked(), 0);
        assert!(format!("{limiter:?}").contains("burst"));
        assert_eq!(format!("{:?}", Permit::Granted), "Granted");
    }
}
