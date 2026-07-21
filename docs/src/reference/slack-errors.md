# Slack errors

Every failure `alertthread` can get from Slack, what it does about each one, and the
metric label it is counted under.

This is reference: it states what is, not how to fix it. The reasoning behind the
classification is [ADR 001 D9](../adr/001-adr.md), extended to storm-collapse group
summaries by ADR 002 §1.3.

---

## Dispositions

Every Slack failure resolves to exactly one of four actions. These are the values behind
`alertthread_dead_letter_total`, `alertthread_rate_limited_total` and the retry behaviour
of the outbox worker.

| Disposition | Action | Consumes an attempt |
|---|---|---|
| `rate_limited` | Defer to `now + Retry-After`, release the lease | **No** |
| `retry` | Defer with exponential backoff; dead-letter once `max_attempts` is reached | Yes |
| `terminal` | Dead-letter immediately | — |
| `message_gone` | Clear the stored `message_ts` and queue a fresh post | — |

`rate_limited` not consuming an attempt is required behaviour, not an optimisation. An
alert storm is exactly when rate limits happen and exactly when the alerts matter; counting
them would dead-letter alerts for arriving at a busy moment.

`message_gone` applies identically to an alert's own message and to a storm-collapse group
summary. A summary whose message is gone leaves its threaded children attached to nothing,
so it is re-posted rather than skipped.

---

## Errors by Slack error code

Slack returns application errors as **HTTP 200 with `{"ok": false, "error": "…"}`**. The
relay reads the body, not the status line.

| Slack `error` | Outcome label | Disposition |
|---|---|---|
| `ratelimited`, `rate_limited` | `rate_limited` | `rate_limited` |
| `message_not_found` | `message_not_found` | `message_gone` |
| `invalid_auth`, `not_authed`, `account_inactive`, `token_revoked`, `token_expired`, `no_permission`, `missing_scope`, `ekm_access_denied` | `invalid_auth` | `terminal` |
| `channel_not_found`, `not_in_channel`, `is_archived`, `restricted_action`, `restricted_action_read_only_channel`, `restricted_action_thread_only_channel`, `restricted_action_non_threadable_channel` | `channel_unusable` | `terminal` |
| `msg_too_long`, `no_text`, `too_many_attachments`, `invalid_blocks`, `invalid_blocks_format`, `invalid_arguments`, `invalid_arg_name`, `cant_update_message`, `edit_window_closed`, `cant_broadcast_message`, `as_user_not_supported`, `invalid_form_data`, `invalid_post_type`, `missing_post_type` | `bad_request` | `terminal` |
| `fatal_error`, `internal_error`, `service_unavailable`, `request_timeout`, `server_error` | `slack_unavailable` | `retry` |
| anything else | `unrecognised` | `retry` |

An unrecognised code is retried rather than dead-lettered. Both paths end in a dead-letter
if the condition persists; retrying first is what gives a transient-but-unfamiliar failure
a chance to succeed.

## Errors below the Slack API

| Condition | Outcome label | Disposition |
|---|---|---|
| HTTP 429 | `rate_limited` | `rate_limited` |
| HTTP 408, 425, or any 5xx | `http_status` | `retry` |
| Any other non-2xx | `http_status` | `terminal` |
| DNS, TCP, TLS, timeout, truncated body | `transport` | `retry` |
| HTTP 200 whose body is not Slack's JSON envelope | `malformed_response` | `retry` |
| `chat.postMessage` succeeded but returned no `ts` | `incomplete_response` | `retry` |
| `auth.test` succeeded but returned no `user_id` | `incomplete_response` | `retry` |

A `chat.postMessage` with no `ts` in the response is treated as a failure, not a success. A
message whose timestamp was never recorded can never be updated or replied to, so the
alert would stay red for ever and its resolution would have nothing to edit. Retrying can
post a duplicate; that is [ADR 001 D3](../adr/001-adr.md)'s duplicate-over-silence
trade-off, reached by a different road.

## Errors before any call is made

| Condition | Outcome label | Disposition |
|---|---|---|
| The bot token cannot be an HTTP header value | `malformed_token` | `terminal` |
| `slack.base_url` will not parse, or cannot carry a path | `invalid_base_url` | `terminal` |
| The HTTP client could not be constructed | `build` | `terminal` |

All three are detected at startup, before the first alert. The commonest by far is a
trailing newline on a token read from a mounted Kubernetes secret.

---

## `Retry-After`

Read from the `Retry-After` response header, in seconds. The HTTP-date form is not parsed.

| | Value |
|---|---|
| Default when absent or unparseable | 1 second |
| Minimum | 1 second |
| Maximum | 15 minutes |

One second is Slack's documented Special Tier limit for `chat.postMessage` — one per second
per channel, thread replies included — which is the limit this relay is most likely to be
hitting.

The maximum is a clamp against a header that would park an alert for hours. If the wait was
genuinely needed, the next attempt receives another 429 and waits again.

---

## What the client does not do

**It never retries or sleeps internally**, on 429 or on anything else. One call is one HTTP
round trip. Scheduling belongs to the outbox worker, for three reasons:

- ADR 001 D2 specifies the 429 response as `next_attempt_at = now + Retry-After` plus a
  lease release — queue scheduling, not a sleep.
- `alertthread_outbox_oldest_age_seconds` is the primary SLO signal. A worker that absorbs
  a rate limit by sleeping holds its op outside that measurement, so the queue reads healthy
  while nothing moves.
- A sleep longer than the 60-second lease lets a second worker reclaim the same row, and
  both post the message.
