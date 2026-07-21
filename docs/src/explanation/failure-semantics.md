# Failure semantics

*Status: written in Phase 5.*

*Why every degradation path in this system terminates in "post a plain message".*

The governing rule is that **silence is never a valid outcome.** A duplicate message is a
nuisance. A dropped alert is an outage nobody hears about. Wherever the two trade off, this
codebase chooses noise.

This page will walk the full failure table — what happens when a resolve arrives for an
untracked fingerprint, when `chat.update` reports `message_not_found`, when a template
throws, when Slack returns 429 or 5xx or `invalid_auth`, and when an operation finally
exhausts its retries — and explain why each terminates where it does.

Two cases deserve emphasis:

- **A user-supplied template is the most likely thing to break in production**, and it must
  not be able to take alerting down. Rendering is always wrapped so that a broken template
  degrades to a hardcoded minimal message.
- **The store being unreachable at ingest returns `503`**, and that is the one case where
  refusing the request is correct. Alertmanager's own retry is more durable than anything
  the relay could do while its database is gone.

The one genuinely unresolvable case — the millisecond-wide window where a worker posts to
Slack and crashes before committing the timestamp — is enumerated rather than hidden, in
[ADR 001 D3](../adr/001-adr.md). The full table is [D9](../adr/001-adr.md).
