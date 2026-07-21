//! The configured behaviour [`plan`](crate::plan) applies.
//!
//! Every field here is an operator-facing setting from ADR 001, gathered into one struct
//! so the planner takes configuration as an argument rather than reaching for it. That is
//! what makes "what does the relay do with a 12-hour repeat?" a question answerable by a
//! unit test.

use chrono::TimeDelta;
use thiserror::Error;

/// How the relay is configured to behave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// How many *new* messages one batch may produce for a channel before the batch is
    /// collapsed into a threaded group summary (ADR 001 D5).
    ///
    /// The comparison is strictly greater-than, matching D5's "more than
    /// `collapse_threshold` new post ops". `0` disables collapse **entirely**, which is
    /// D5's own word for it: no group parent is posted, nothing is threaded, and an
    /// existing group parent stops attracting new members. The alternative reading —
    /// honouring stickiness while refusing to create new groups — would make the setting
    /// mean "no new collapse", which is not what it says and is not something an operator
    /// could confirm by watching the channel.
    pub collapse_threshold: usize,
    /// How long after a message was last seen a repeat delivery counts as a genuine
    /// `repeat_interval` re-send rather than an HTTP retry (ADR 001 D2, D7).
    ///
    /// This is the whole mechanism that separates "Alertmanager retried the request"
    /// (seconds apart) from "Alertmanager re-sent on its repeat interval" (12 hours
    /// apart) without the relay having to model either timer.
    pub refresh_debounce: TimeDelta,
    /// Rewrite the original message when an alert resolves (ADR 001 D6).
    pub resolve_update_in_place: bool,
    /// Post a threaded reply when an alert resolves (ADR 001 D6).
    ///
    /// Kept separate from `resolve_update_in_place` because the two solve different
    /// problems: `chat.update` does not notify, bump, or mark a channel unread, so an
    /// in-place edit alone is invisible to anyone watching live; a thread reply generates
    /// the unread indicator.
    pub resolve_thread_reply: bool,
}

impl Policy {
    /// ADR 001's defaults: collapse above five, one-minute debounce, both resolve
    /// behaviours on.
    ///
    /// The threshold of `5` is flagged in the ADR's own open questions as a guess to
    /// revisit against real alert volume; it is repeated here rather than reasoned about
    /// afresh.
    pub const DEFAULT_COLLAPSE_THRESHOLD: usize = 5;

    /// ADR 001 D2's default `refresh_debounce`, in seconds.
    pub const DEFAULT_REFRESH_DEBOUNCE_SECONDS: i64 = 60;

    /// Rejects configurations that would make the relay quieter than the bug it replaces.
    ///
    /// Called by the shell at startup. ADR 001 D6 is explicit that both resolve
    /// behaviours off is a config error and must refuse to start, because a resolve that
    /// does nothing is indistinguishable from the failure this project exists to fix.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] describing the first problem found.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if !self.resolve_update_in_place && !self.resolve_thread_reply {
            return Err(PolicyError::ResolveDoesNothing);
        }
        if self.refresh_debounce < TimeDelta::zero() {
            return Err(PolicyError::NegativeRefreshDebounce);
        }
        Ok(())
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            collapse_threshold: Self::DEFAULT_COLLAPSE_THRESHOLD,
            refresh_debounce: TimeDelta::seconds(Self::DEFAULT_REFRESH_DEBOUNCE_SECONDS),
            resolve_update_in_place: true,
            resolve_thread_reply: true,
        }
    }
}

/// A configuration the relay refuses to start with.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PolicyError {
    /// Both resolve behaviours are disabled, so resolving an alert would do nothing.
    #[error(
        "resolve.update_in_place and resolve.thread_reply are both false: a resolve that \
         does nothing is indistinguishable from the bug this relay exists to fix (ADR 001 D6)"
    )]
    ResolveDoesNothing,
    /// The debounce is negative, which would treat every duplicate HTTP delivery as a
    /// genuine repeat and refresh the message on each one.
    #[error(
        "refresh_debounce is negative: every retried delivery would be treated as a \
         repeat-interval re-send and refresh the message (ADR 001 D7)"
    )]
    NegativeRefreshDebounce,
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::{Policy, PolicyError};

    #[test]
    fn the_defaults_are_the_ones_adr_001_specifies() {
        let policy = Policy::default();
        assert_eq!(policy.collapse_threshold, 5);
        assert_eq!(policy.refresh_debounce, TimeDelta::seconds(60));
        assert!(policy.resolve_update_in_place);
        assert!(policy.resolve_thread_reply);
    }

    #[test]
    fn the_default_policy_is_valid() {
        assert_eq!(Policy::default().validate(), Ok(()));
    }

    #[test]
    fn either_resolve_behaviour_alone_is_accepted() {
        let edit_only = Policy {
            resolve_thread_reply: false,
            ..Policy::default()
        };
        assert_eq!(edit_only.validate(), Ok(()));

        let reply_only = Policy {
            resolve_update_in_place: false,
            ..Policy::default()
        };
        assert_eq!(reply_only.validate(), Ok(()));
    }

    #[test]
    fn disabling_both_resolve_behaviours_is_rejected() {
        let policy = Policy {
            resolve_update_in_place: false,
            resolve_thread_reply: false,
            ..Policy::default()
        };
        assert_eq!(policy.validate(), Err(PolicyError::ResolveDoesNothing));
    }

    #[test]
    fn a_negative_debounce_is_rejected() {
        let policy = Policy {
            refresh_debounce: TimeDelta::seconds(-1),
            ..Policy::default()
        };
        assert_eq!(policy.validate(), Err(PolicyError::NegativeRefreshDebounce));
    }

    #[test]
    fn a_zero_debounce_is_accepted() {
        // Zero is a legitimate setting: it means "refresh on every repeat delivery",
        // which is noisy but not wrong. Only a negative value is incoherent.
        let policy = Policy {
            refresh_debounce: TimeDelta::zero(),
            ..Policy::default()
        };
        assert_eq!(policy.validate(), Ok(()));
    }

    #[test]
    fn a_zero_collapse_threshold_is_a_valid_configuration() {
        // D5: setting it to 0 disables collapse for anyone who prefers strict per-alert
        // messages. It is a supported choice, not a misconfiguration.
        let policy = Policy {
            collapse_threshold: 0,
            ..Policy::default()
        };
        assert_eq!(policy.validate(), Ok(()));
    }

    #[test]
    fn policy_errors_explain_themselves_and_cite_the_decision() {
        // These strings reach an operator at startup, in a container that has already
        // failed to start, so they carry the whole explanation.
        let both_off = PolicyError::ResolveDoesNothing.to_string();
        assert!(both_off.contains("update_in_place"), "{both_off}");
        assert!(both_off.contains("D6"), "{both_off}");

        let negative = PolicyError::NegativeRefreshDebounce.to_string();
        assert!(negative.contains("refresh_debounce"), "{negative}");
        assert!(negative.contains("D7"), "{negative}");
    }

    #[test]
    fn policy_debug_shows_the_thresholds() {
        let rendered = format!("{:?}", Policy::default());
        assert!(rendered.contains("collapse_threshold: 5"), "{rendered}");
    }

    #[test]
    fn policy_error_debug_names_the_variant() {
        assert_eq!(
            format!("{:?}", PolicyError::ResolveDoesNothing),
            "ResolveDoesNothing"
        );
        assert_eq!(
            format!("{:?}", PolicyError::NegativeRefreshDebounce),
            "NegativeRefreshDebounce"
        );
    }
}
