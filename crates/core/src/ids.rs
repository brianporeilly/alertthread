//! Newtyped identifiers for everything the relay correlates on.
//!
//! Every type here wraps a `String`, and every one of them is a separate type. That is
//! deliberate and it is not ceremony (AGENTS.md rule 4): `chat.update(channel, ts)` takes
//! two strings, swapping them compiles cleanly, and the failure lands at runtime, in the
//! alerting path, under load. Distinct types make that mistake unrepresentable.
//!
//! [`MessageTs`] and [`ThreadTs`] are the pair most worth separating. Both are Slack
//! message timestamps and both are `String` on the wire, but they mean different things:
//! a `MessageTs` identifies *this* alert's own message, while a `ThreadTs` identifies the
//! storm-collapse parent that message hangs under (ADR 001 D5). Passing one where the
//! other belongs would thread an alert under itself.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Alertmanager's stable identity for a single alert.
///
/// This is the whole basis of the project: a fingerprint is stable across the
/// firing → resolved lifecycle, where message text and Slack timestamps are not, so it is
/// what a resolution is correlated back to. Alertmanager computes it from the alert's
/// label set and sends it in the webhook body as `fingerprint`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Wraps an Alertmanager fingerprint.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Slack channel, as the relay was told to address it.
///
/// Held verbatim as supplied by the `?channel=` query parameter (ADR 001 D8), which means
/// it may be a name such as `#alerts` or an ID such as `C01234567`. Slack accepts either,
/// and normalising here would only add a way to be wrong.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelId(String);

impl ChannelId {
    /// Wraps a channel name or ID.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The Slack timestamp of the message posted for one alert.
///
/// `chat.update` addresses a message by `(channel, ts)`, so this is the handle that makes
/// update-on-resolve possible at all. It does not exist until a post has succeeded, which
/// is why the store holds it as nullable and why [`ClaimResult`](crate::ClaimResult)
/// hands it to the planner as an `Option` on the paths where it may still be absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageTs(String);

impl MessageTs {
    /// Wraps a Slack message timestamp.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageTs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Alertmanager's `groupKey` for the batch an alert arrived in.
///
/// Storm collapse (ADR 001 D5) keys on this rather than inventing a grouping concept of
/// its own, because Alertmanager has already decided what belongs together — that is what
/// its `group_by` configuration is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupKey(String);

impl GroupKey {
    /// Wraps an Alertmanager group key.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GroupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The Slack timestamp of a storm-collapse parent message.
///
/// Slack calls this `thread_ts` when replying into a thread. It is a message timestamp
/// like [`MessageTs`], but it belongs to the group summary rather than to any one alert,
/// and mixing the two up is exactly the class of bug these newtypes exist to prevent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadTs(String);

impl ThreadTs {
    /// Wraps a Slack thread parent timestamp.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the underlying identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadTs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{ChannelId, Fingerprint, GroupKey, MessageTs, ThreadTs};

    // Each identifier is checked on all four of the things it can do: construct, borrow,
    // display, and round-trip through serde. The serde round-trip is not padding — these
    // types are deserialised straight out of the webhook body and serialised back into
    // outbox payloads, so a wrapper that is not `transparent` would silently change the
    // wire format to `{"0": "..."}`.

    #[test]
    fn fingerprint_carries_its_value() {
        let fingerprint = Fingerprint::new("a1b2c3d4e5f60718");
        assert_eq!(fingerprint.as_str(), "a1b2c3d4e5f60718");
        assert_eq!(fingerprint.to_string(), "a1b2c3d4e5f60718");
        assert_eq!(
            format!("{fingerprint:?}"),
            "Fingerprint(\"a1b2c3d4e5f60718\")"
        );
    }

    #[test]
    fn fingerprint_round_trips_as_a_bare_json_string() {
        let fingerprint = Fingerprint::new("a1b2c3d4e5f60718");
        let json = serde_json::to_string(&fingerprint).unwrap();
        assert_eq!(json, "\"a1b2c3d4e5f60718\"");
        assert_eq!(
            serde_json::from_str::<Fingerprint>(&json).unwrap(),
            fingerprint
        );
    }

    #[test]
    fn channel_id_carries_its_value() {
        let channel = ChannelId::new("#alerts-critical");
        assert_eq!(channel.as_str(), "#alerts-critical");
        assert_eq!(channel.to_string(), "#alerts-critical");
        assert_eq!(format!("{channel:?}"), "ChannelId(\"#alerts-critical\")");
    }

    #[test]
    fn channel_id_round_trips_as_a_bare_json_string() {
        let channel = ChannelId::new("C01234567");
        let json = serde_json::to_string(&channel).unwrap();
        assert_eq!(json, "\"C01234567\"");
        assert_eq!(serde_json::from_str::<ChannelId>(&json).unwrap(), channel);
    }

    #[test]
    fn message_ts_carries_its_value() {
        let ts = MessageTs::new("1721500000.000100");
        assert_eq!(ts.as_str(), "1721500000.000100");
        assert_eq!(ts.to_string(), "1721500000.000100");
        assert_eq!(format!("{ts:?}"), "MessageTs(\"1721500000.000100\")");
    }

    #[test]
    fn message_ts_round_trips_as_a_bare_json_string() {
        let ts = MessageTs::new("1721500000.000100");
        let json = serde_json::to_string(&ts).unwrap();
        assert_eq!(json, "\"1721500000.000100\"");
        assert_eq!(serde_json::from_str::<MessageTs>(&json).unwrap(), ts);
    }

    #[test]
    fn group_key_carries_its_value() {
        let key = GroupKey::new("{}:{alertname=\"KubePodNotReady\"}");
        assert_eq!(key.as_str(), "{}:{alertname=\"KubePodNotReady\"}");
        assert_eq!(key.to_string(), "{}:{alertname=\"KubePodNotReady\"}");
        assert!(format!("{key:?}").starts_with("GroupKey("));
    }

    #[test]
    fn group_key_round_trips_as_a_bare_json_string() {
        let key = GroupKey::new("{}:{alertname=\"CephOSDDown\"}");
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"{}:{alertname=\\\"CephOSDDown\\\"}\"");
        assert_eq!(serde_json::from_str::<GroupKey>(&json).unwrap(), key);
    }

    #[test]
    fn thread_ts_carries_its_value() {
        let ts = ThreadTs::new("1721500000.000200");
        assert_eq!(ts.as_str(), "1721500000.000200");
        assert_eq!(ts.to_string(), "1721500000.000200");
        assert_eq!(format!("{ts:?}"), "ThreadTs(\"1721500000.000200\")");
    }

    #[test]
    fn thread_ts_round_trips_as_a_bare_json_string() {
        let ts = ThreadTs::new("1721500000.000200");
        let json = serde_json::to_string(&ts).unwrap();
        assert_eq!(json, "\"1721500000.000200\"");
        assert_eq!(serde_json::from_str::<ThreadTs>(&json).unwrap(), ts);
    }

    #[test]
    fn a_message_ts_and_a_thread_ts_holding_the_same_string_are_different_types() {
        // The compile-time property is the point, and it cannot be asserted at runtime.
        // What can be asserted is that neither type quietly normalises its input, so the
        // two are interchangeable in *value* and only distinguishable by *type*.
        let raw = "1721500000.000300";
        assert_eq!(MessageTs::new(raw).as_str(), ThreadTs::new(raw).as_str());
    }
}
