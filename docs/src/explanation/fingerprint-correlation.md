# Fingerprint correlation

*Status: written in Phase 1, alongside the pure core.*

*Why the relay keys on `(fingerprint, channel)`, and what that buys.*

This page will explain:

- What Alertmanager's alert fingerprint is, and why it is a stable identity across the
  firing → resolved lifecycle where a message timestamp is not.
- **Why the channel is part of the key, not just the fingerprint.** If the same alert is
  ever routed to two channels, a fingerprint-only key silently loses one of them. Adding the
  channel costs nothing and removes the failure mode.
- How the atomic claim on that key makes duplicate suppression a single database statement,
  which is what makes it correct under concurrent replicas *and* Alertmanager retries at the
  same time.
- Why Slack's lack of an idempotency key on `chat.postMessage` forces suppression to happen
  on our side, before the call.
- Storm collapse: how a group of alerts threads under one summary message while each child
  keeps its own row, so per-alert resolve still updates the correct child in place.

Decisions are recorded in [ADR 001 D3 and D5](../adr/001-adr.md).
