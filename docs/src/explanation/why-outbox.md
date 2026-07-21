# Why an outbox

*Status: written in Phase 1.*

*Why the webhook handler persists intent and returns immediately, rather than posting to
Slack inline.*

The obvious design — receive the webhook, post to Slack, store the timestamp, return `200` —
fails in three distinct ways, and this page will work through each:

- **Rate limits.** Slack allows roughly one `chat.postMessage` per second per channel,
  thread replies included. Fifteen alerts means a fifteen-second handler; Alertmanager times
  out and retries, and the same batch is now in flight twice.
- **The crash window.** Acknowledge before posting and a crash loses the alert silently.
  Post before acknowledging and a crash in between causes Alertmanager to retry a batch that
  was already delivered.
- **Backpressure.** Slack being slow becomes Alertmanager being blocked.

The outbox resolves all three: the durable write happens *before* the ack, so nothing is
lost; the ack happens *before* the Slack call, so nothing blocks; and retries become our
problem to make idempotent rather than Alertmanager's problem to guess at.

It will also be honest about the cost. This is meaningfully more machinery than a
synchronous handler — perhaps two to three times the code — and there remains exactly one
unresolvable window, where a worker posts to Slack and crashes before recording the
timestamp. That produces a duplicate. The direction of that failure is chosen deliberately:
**duplicate, never silence.**

Decisions are recorded in [ADR 001 D2 and D3](../adr/001-adr.md).
