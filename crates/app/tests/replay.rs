//! `alertthread replay`, end to end against a real store and a real Slack.
//!
//! ROADMAP known open item 14 is the gap this closes: `channel_unusable` is the park reason
//! with no probe behind it, so an operator who invites the bot to a channel fixes every
//! future alert and leaves every alert parked before that parked for ever. What these tests
//! pin is the whole loop — an alert refused by Slack, parked, listed by the subcommand, and
//! then actually delivered by the same worker the relay runs.
//!
//! Three properties, in the order they matter:
//!
//! 1. **A dry run changes nothing.** It is the default, so anything else is a command that
//!    re-sends production alerts because somebody wanted to look.
//! 2. **A commit re-queues and does not deliver.** The rows go back to the outbox and the
//!    worker takes them under the ordinary lease, which is what makes running this against a
//!    store a relay is draining safe rather than merely untested.
//! 3. **The alert reaches Slack.** The point of the command is not the row.
//!
//! The subcommand opens its *own* connection from the configuration, exactly as a
//! `kubectl exec` into the pod would, while the harness's pool is still open on the same
//! database. That is the concurrency case, not a simulation of it.
//!
//! Time is injected everywhere, as it is throughout this codebase: Slack allows one post per
//! second per channel, so two alerts parked at the same instant would be a test of the rate
//! limiter rather than of the replay.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use alertthread::cli::{Command, Replay, parse};
use alertthread::replay::{Summary, run};
use alertthread_core::{AlertBatch, ChannelId, Fingerprint, Policy, WebhookPayload, plan};
use alertthread_store::{AlertState, DeadLetterScope, StateStore};
use chrono::{DateTime, TimeDelta, Utc};
use harness::{
    CHANNEL, Harness, alert, payload, slack_error, slack_that_works, slack_with_auth_only, t0,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// When the operator gets to a shell — long after everything below was parked.
fn recovery_time() -> DateTime<Utc> {
    t0() + TimeDelta::hours(1)
}

/// A Slack that refuses every post the way an uninvited bot is refused.
async fn slack_that_will_not_take_the_channel() -> MockServer {
    let slack = slack_with_auth_only().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(slack_error("channel_not_found"))
        .mount(&slack)
        .await;
    slack
}

/// Ingests one firing alert into `channel` and lets the worker park it.
///
/// `channel_not_found` is `Disposition::Terminal`, so ADR 001 D9 parks it rather than
/// spending ten retries on a channel the bot is not in. It still takes more than one pass
/// when a previous alert has just used this channel's token.
async fn park(relay: &Harness, fingerprint: &str, channel: &str, now: DateTime<Utc>) {
    let body = payload("firing", &[alert(fingerprint, "firing")]);
    let parsed: WebhookPayload = serde_json::from_str(&body).expect("the fixture parses");
    let batch = AlertBatch::from_webhook(parsed, ChannelId::new(channel));
    let policy = Policy::default();
    relay
        .store
        .ingest(&batch, now, |outcomes, group| {
            plan(outcomes, &batch, group, &policy, now)
        })
        .await
        .expect("ingesting");

    let drained = relay.drain_from(now, 10).await;
    assert_eq!(
        drained.dead_lettered, 1,
        "the alert has to be parked for this test to mean anything: {drained:?}"
    );
}

/// What inviting the bot to the channel looks like from the relay's side.
///
/// The same `MockServer` — and therefore the same `slack.base_url` the harness was built
/// with — starts accepting posts. A second server would be a different relay.
async fn invite_the_bot(slack: &MockServer) {
    slack.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C0123456789",
            "ts": "1784642520.000100",
        })))
        .mount(slack)
        .await;
}

/// Runs the subcommand and returns what it printed alongside what it did.
async fn replay(relay: &Harness, args: &Replay) -> (Summary, String) {
    let mut out = Vec::new();
    let summary = run(args, &relay.config, recovery_time(), &mut out)
        .await
        .expect("the subcommand runs against a healthy store");
    (summary, String::from_utf8(out).expect("the output is text"))
}

async fn parked(relay: &Harness) -> Vec<alertthread_store::DeadLetter> {
    relay
        .store
        .dead_letters(&DeadLetterScope::ALL, 100)
        .await
        .expect("listing")
}

async fn state_of(relay: &Harness, fingerprint: &str) -> AlertState {
    relay
        .store
        .alert(&Fingerprint::new(fingerprint), &ChannelId::new(CHANNEL))
        .await
        .expect("reading")
        .expect("row exists")
        .state
}

#[tokio::test]
async fn a_dry_run_lists_the_parked_alert_and_leaves_it_parked() {
    // The default. An operator recovering an incident looks before they re-send, and a
    // command that re-sent on being looked at would be one nobody dares run.
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-dry-run", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;

    let (summary, output) = replay(&relay, &Replay::default()).await;

    assert_eq!(summary.matched, 1);
    assert_eq!(summary.revived, 0, "a dry run revives nothing");
    assert!(!summary.committed);
    assert!(output.contains("DRY RUN"), "{output}");
    assert!(output.contains("--commit"), "{output}");
    assert!(output.contains(CHANNEL), "{output}");
    assert!(output.contains("abc"), "{output}");
    assert!(
        output.contains("channel_not_found"),
        "the verbatim Slack code is what the troubleshooting guide is indexed by: {output}"
    );
    assert!(output.contains("1h0m ago"), "{output}");

    assert_eq!(parked(&relay).await.len(), 1, "and the row is still parked");
}

#[tokio::test]
async fn a_committed_replay_returns_the_alert_to_the_outbox_and_the_worker_delivers_it() {
    // The whole point, in one test: a parked alert can always be recovered. The Slack that
    // refused the post starts accepting it, which is the moment somebody invites the bot to
    // the channel — and nothing else in the relay notices that has happened.
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-commit", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;
    assert_eq!(
        state_of(&relay, "abc").await,
        AlertState::Failed,
        "parking a post marks the alert failed; reviving has to undo that"
    );

    invite_the_bot(&slack).await;

    // Nothing has happened on its own. This is the gap the command exists to close.
    let idle = relay.drain_from(recovery_time(), 3).await;
    assert_eq!(
        idle.leased, 0,
        "a parked row is invisible to the lease for ever: {idle:?}"
    );

    let (summary, output) = replay(
        &relay,
        &Replay {
            commit: true,
            ..Replay::default()
        },
    )
    .await;

    assert_eq!(summary.revived, 1, "{output}");
    assert!(summary.committed);
    assert!(output.contains("Returned 1"), "{output}");
    assert!(
        output.contains("queued, not sent"),
        "the command must not read as though it delivered anything: {output}"
    );
    assert!(parked(&relay).await.is_empty());

    // The alert is claimed again rather than failed, so its eventual resolution correlates
    // to the message that is about to be posted instead of arriving as an orphan.
    assert_eq!(state_of(&relay, "abc").await, AlertState::Claimed);

    // And the ordinary worker — not the subcommand — is what delivers it.
    let drained = relay.drain_from(recovery_time(), 10).await;
    assert_eq!(drained.completed, 1, "{drained:?}");
    assert_eq!(
        state_of(&relay, "abc").await,
        AlertState::Posted,
        "the alert that never reached Slack has now reached Slack"
    );
}

#[tokio::test]
async fn a_channel_scoped_replay_leaves_the_other_channel_parked() {
    // The motivating case. Inviting the bot to one channel says nothing about the others,
    // and re-sending those would spend a Slack call each to re-park them.
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-scoped", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;
    park(&relay, "def", "#other", t0() + TimeDelta::minutes(1)).await;
    assert_eq!(parked(&relay).await.len(), 2);

    let (summary, output) = replay(
        &relay,
        &Replay {
            channel: Some(CHANNEL.to_owned()),
            commit: true,
            ..Replay::default()
        },
    )
    .await;

    assert_eq!(summary.matched, 1, "{output}");
    assert_eq!(summary.revived, 1, "{output}");
    assert!(output.contains(CHANNEL), "{output}");

    let left = parked(&relay).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].channel, ChannelId::new("#other"));
}

#[tokio::test]
async fn a_fingerprint_scoped_replay_takes_one_alert() {
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-fingerprint", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;
    park(&relay, "def", CHANNEL, t0() + TimeDelta::minutes(1)).await;

    let (summary, output) = replay(
        &relay,
        &Replay {
            fingerprint: Some("abc".to_owned()),
            commit: true,
            ..Replay::default()
        },
    )
    .await;
    assert_eq!(summary.revived, 1, "{output}");

    let left = parked(&relay).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].fingerprint, Some(Fingerprint::new("def")));
    // The alert that was not replayed keeps its `failed` mark, because it is still
    // undelivered and its resolution still has nothing to edit.
    assert_eq!(state_of(&relay, "def").await, AlertState::Failed);
}

#[tokio::test]
async fn a_scope_that_matches_nothing_says_so_and_does_not_widen() {
    // A mistyped channel exits the same way a clean queue does, so the sentence is the only
    // thing that tells them apart — and falling back to the whole queue would be the worst
    // possible reading of a typo.
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-no-match", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;

    let (summary, output) = replay(
        &relay,
        &Replay {
            channel: Some("#alrets".to_owned()),
            commit: true,
            ..Replay::default()
        },
    )
    .await;

    assert_eq!(summary.matched, 0);
    assert_eq!(summary.revived, 0);
    assert!(output.contains("#alrets"), "{output}");
    assert!(output.contains("nothing to replay"), "{output}");
    assert_eq!(parked(&relay).await.len(), 1, "and nothing was touched");
}

#[tokio::test]
async fn an_empty_dead_letter_queue_is_reported_rather_than_being_an_error() {
    // The healthy case, which is what most invocations of this command will find.
    let slack = slack_that_works().await;
    let relay = Harness::new("replay-empty", &slack).await;

    let (summary, output) = replay(&relay, &Replay::default()).await;
    assert_eq!(summary.matched, 0);
    assert!(output.contains("There is nothing to replay"), "{output}");
}

#[tokio::test]
async fn the_command_line_a_runbook_documents_produces_the_arguments_this_runs() {
    // The parser and the executor are tested separately everywhere else. This is the one
    // place they meet, because a runbook that says `--channel` against a flag spelled
    // `--chan` is a runbook that fails at 3am.
    let slack = slack_that_will_not_take_the_channel().await;
    let relay = Harness::new("replay-cli", &slack).await;
    park(&relay, "abc", CHANNEL, t0()).await;

    let command_line = ["alertthread", "replay", "--channel", CHANNEL, "--commit"]
        .into_iter()
        .map(ToOwned::to_owned);
    let Command::Replay(args) = parse(command_line).expect("the documented command line parses")
    else {
        panic!("`replay` selects the subcommand");
    };

    let (summary, _) = replay(&relay, &args).await;
    assert_eq!(summary.revived, 1);
    assert!(parked(&relay).await.is_empty());
}

#[tokio::test]
async fn a_store_the_configuration_cannot_open_fails_with_the_backend_in_the_message() {
    // An operator running this under `kubectl exec` has one line of output to work from.
    let slack = slack_that_works().await;
    let mut relay = Harness::new("replay-bad-store", &slack).await;
    relay.config.storage.backend = "cassandra".to_owned();

    let mut out = Vec::new();
    let error = run(&Replay::default(), &relay.config, recovery_time(), &mut out)
        .await
        .expect_err("an unknown backend cannot be opened");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("cassandra"), "{rendered}");
}
