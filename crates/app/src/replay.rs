//! `alertthread replay`: returning parked operations to the outbox by hand.
//!
//! ADR 003 §5.2 decided the shape — a subcommand of the existing binary, not an `/admin`
//! endpoint — and this is it. The motivating case is `channel_unusable`: nothing watches
//! channel membership the way the auth prober watches the token, so an operator who invites
//! the bot to a channel fixes every *future* alert and leaves every alert parked before that
//! parked for ever.
//!
//! # What it does and does not do
//!
//! It does not talk to Slack. It clears `dead_lettered_at`, resets `attempts` and lets the
//! row become leasable again; the relay's outbox worker delivers it, under the same
//! exactly-once lease as any other queued op. That is what makes it safe to run against a
//! store a relay is actively draining — which matters, because the relay must not have to be
//! stopped to recover an alert from it.
//!
//! It does not run migrations either. A recovery command that can alter the schema is a
//! recovery command that can make an incident worse; the server owns that.

use std::io::Write;

use alertthread_core::{ChannelId, Fingerprint};
use alertthread_store::{
    Backend, DeadLetter, DeadLetterScope, OpKind, StateStore, Store, StoreError,
};
use anyhow::Context as _;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};

use crate::cli::Replay;
use crate::config::Config;

/// How many parked rows one invocation reads back.
///
/// A cap rather than "everything", because this renders a table into a terminal and a
/// deployment that has parked a million rows has a different problem. `--commit` acts on
/// every matching row regardless, and the output says so when the cap is reached.
const LIST_LIMIT: u32 = 10_000;

/// How many rows are printed before the listing summarises the rest.
const SHOW_LIMIT: usize = 50;

/// How much of a row's `last_error` is shown before it is elided.
const ERROR_WIDTH: usize = 120;

/// What one invocation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// How many parked rows the scope matched, as far as [`LIST_LIMIT`] could see.
    pub matched: usize,
    /// How many rows were returned to the queue. Always `0` for a dry run.
    pub revived: u64,
    /// Whether `--commit` was given.
    pub committed: bool,
    /// Whether the listing hit [`LIST_LIMIT`] and there may be more.
    pub capped: bool,
}

/// Lists the parked operations a scope matches, and re-queues them if asked.
///
/// `now` is a parameter for the same reason it is everywhere else in this codebase: it is
/// what the revived rows' `next_attempt_at` is set to, and a test that had to sleep to
/// observe that would be a test nobody runs.
///
/// # Errors
///
/// An unreachable or unreadable store, a `storage.backend` this build does not have, or a
/// parked row whose payload this build cannot decode. Nothing here is a partial success:
/// the re-queue is one transaction in the store.
pub async fn run<W: Write>(
    args: &Replay,
    config: &Config,
    now: DateTime<Utc>,
    out: &mut W,
) -> anyhow::Result<Summary> {
    let backend = Backend::parse(&config.storage.backend)
        .context("storage.backend names a backend this build does not have")?;
    let store = Store::connect(backend, &config.storage.url)
        .await
        .with_context(|| format!("could not open the {backend} state store"))?;

    let scope = scope_of(args);
    let parked = store
        .dead_letters(&scope, LIST_LIMIT)
        .await
        .context("could not read the dead-letter queue")?;

    report(&store, &scope, &parked, args.commit, now, out).await
}

/// Everything after the store is open, so the reporting is testable against any backend.
async fn report<S: StateStore, W: Write>(
    store: &S,
    scope: &DeadLetterScope,
    parked: &[DeadLetter],
    commit: bool,
    now: DateTime<Utc>,
    out: &mut W,
) -> anyhow::Result<Summary> {
    let mut summary = Summary {
        matched: parked.len(),
        capped: parked.len() >= usize::try_from(LIST_LIMIT).unwrap_or(usize::MAX),
        committed: commit,
        ..Summary::default()
    };

    if parked.is_empty() {
        writeln!(out, "{}", nothing_parked(scope))?;
        return Ok(summary);
    }

    writeln!(out, "{}", headline(scope, summary.matched, summary.capped))?;
    writeln!(out)?;
    write!(out, "{}", table(parked, now))?;
    writeln!(out)?;

    if !commit {
        writeln!(out, "{}", dry_run_footer(summary.matched))?;
        return Ok(summary);
    }

    summary.revived = requeue(store, scope, now).await?;
    writeln!(out, "{}", committed_footer(summary.revived))?;
    Ok(summary)
}

/// Hands the matching rows back to the outbox.
async fn requeue<S: StateStore>(
    store: &S,
    scope: &DeadLetterScope,
    now: DateTime<Utc>,
) -> Result<u64, StoreError> {
    store.revive_dead_letters(scope, now).await
}

/// The store-level scope the flags describe.
fn scope_of(args: &Replay) -> DeadLetterScope {
    let mut scope = DeadLetterScope::ALL;
    if let Some(channel) = &args.channel {
        scope = scope.with_channel(ChannelId::new(channel.clone()));
    }
    if let Some(fingerprint) = &args.fingerprint {
        scope = scope.with_fingerprint(Fingerprint::new(fingerprint.clone()));
    }
    scope
}

/// How to name a scope in a sentence.
fn describe(scope: &DeadLetterScope) -> String {
    match (scope.channel(), scope.fingerprint()) {
        (None, None) => "the dead-letter queue".to_owned(),
        (Some(channel), None) => format!("channel {channel}"),
        (None, Some(fingerprint)) => format!("fingerprint {fingerprint}"),
        (Some(channel), Some(fingerprint)) => {
            format!("channel {channel} and fingerprint {fingerprint}")
        }
    }
}

/// What to say when the scope matched nothing.
///
/// Not an error, and deliberately worded so it cannot be read as "the replay worked": an
/// operator who mistyped a channel name gets the same exit code either way, so the sentence
/// is the only thing that distinguishes them.
fn nothing_parked(scope: &DeadLetterScope) -> String {
    format!(
        "Nothing is parked in {}. There is nothing to replay.",
        describe(scope)
    )
}

/// The line above the table.
fn headline(scope: &DeadLetterScope, matched: usize, capped: bool) -> String {
    let noun = if matched == 1 {
        "operation is"
    } else {
        "operations are"
    };
    let count = if capped {
        format!("At least {matched}")
    } else {
        matched.to_string()
    };
    format!(
        "{count} {noun} parked in {} and never reached Slack.",
        describe(scope)
    )
}

/// What to print after a dry run.
fn dry_run_footer(matched: usize) -> String {
    format!(
        "DRY RUN — nothing has been changed.\n\
         Re-run with --commit to return {matched} operation(s) to the outbox."
    )
}

/// What to print after a commit.
///
/// The second paragraph is the whole reason this is not one line. `replay` re-queues; it
/// does not deliver. An operator who read "returned to the outbox" as "sent" and walked away
/// from a store with no relay running against it would have been told the opposite of what
/// happened, which is the failure mode this project spends an ADR on.
fn committed_footer(revived: u64) -> String {
    format!(
        "Returned {revived} parked operation(s) to the outbox.\n\
         \n\
         They are queued, not sent: the relay's outbox worker delivers them on its next\n\
         pass, under the same lease as any other queued work. If no relay is running\n\
         against this store, they go out when one starts. Watch alertthread_outbox_depth\n\
         fall back to zero, or the relay's logs."
    )
}

/// The listing itself, one row per parked operation.
fn table(parked: &[DeadLetter], now: DateTime<Utc>) -> String {
    let shown: Vec<Row> = parked
        .iter()
        .take(SHOW_LIMIT)
        .map(|row| Row::of(row, now))
        .collect();

    let mut widths = Row::HEADINGS.map(str::len);
    for row in &shown {
        for (width, cell) in widths.iter_mut().zip(row.cells()) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    push_line(&mut out, &Row::HEADINGS.map(String::from), widths);
    for row in &shown {
        push_line(&mut out, &row.cells(), widths);
    }
    if parked.len() > shown.len() {
        let hidden = parked.len() - shown.len();
        out.push_str("  ... and ");
        out.push_str(&hidden.to_string());
        out.push_str(" more not shown.\n");
    }
    out
}

/// Writes one padded row. The last column is not padded, so a long error does not trail
/// spaces into somebody's terminal.
fn push_line(out: &mut String, cells: &[String; Row::COLUMNS], widths: [usize; Row::COLUMNS]) {
    out.push_str("  ");
    let last = Row::COLUMNS - 1;
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        out.push_str(cell);
        if index != last {
            // Padded by hand rather than with a width specifier: `write!` into a `String`
            // returns a `Result` that cannot fail, and the workspace denies discarding one.
            for _ in cell.chars().count()..width {
                out.push(' ');
            }
            out.push_str("  ");
        }
    }
    out.push('\n');
}

/// One parked operation, rendered.
struct Row {
    id: String,
    op: String,
    channel: String,
    fingerprint: String,
    attempts: String,
    parked: String,
    last_error: String,
}

impl Row {
    const COLUMNS: usize = 7;
    const HEADINGS: [&'static str; Self::COLUMNS] = [
        "ID",
        "OP",
        "CHANNEL",
        "FINGERPRINT",
        "TRIES",
        "PARKED",
        "LAST ERROR",
    ];

    fn of(row: &DeadLetter, now: DateTime<Utc>) -> Self {
        Self {
            id: row.id.to_string(),
            op: OpKind::of(&row.op).to_string(),
            channel: row.channel.to_string(),
            // A storm-collapse parent belongs to a group rather than to an alert, so it has
            // no fingerprint — and this column is exactly what `--fingerprint` filters on.
            fingerprint: row
                .fingerprint
                .as_ref()
                .map_or_else(|| "-".to_owned(), ToString::to_string),
            attempts: row.attempts.to_string(),
            parked: format!(
                "{} ({} ago)",
                row.dead_lettered_at
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                age(now, row.dead_lettered_at)
            ),
            last_error: elide(
                row.last_error.as_deref().unwrap_or("(none recorded)"),
                ERROR_WIDTH,
            ),
        }
    }

    fn cells(&self) -> [String; Self::COLUMNS] {
        [
            self.id.clone(),
            self.op.clone(),
            self.channel.clone(),
            self.fingerprint.clone(),
            self.attempts.clone(),
            self.parked.clone(),
            self.last_error.clone(),
        ]
    }
}

/// How long ago something happened, at the coarsest two units that still say it.
///
/// "3h21m" rather than "12060 seconds": the number an operator is comparing this against is
/// "when did I invite the bot to the channel", which they remember in hours.
fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now - then;
    if delta < TimeDelta::zero() {
        // Two clocks, one store. Printing a negative duration would look like a bug in the
        // relay rather than skew between the machine that parked the row and this one.
        return "in the future".to_owned();
    }
    let (days, hours, minutes, seconds) = (
        delta.num_days(),
        delta.num_hours(),
        delta.num_minutes(),
        delta.num_seconds(),
    );
    if days > 0 {
        format!("{days}d{}h", hours - days * 24)
    } else if hours > 0 {
        format!("{hours}h{}m", minutes - hours * 60)
    } else if minutes > 0 {
        format!("{minutes}m{}s", seconds - minutes * 60)
    } else {
        format!("{seconds}s")
    }
}

/// Shortens a string to `max` characters, marking that it was shortened.
///
/// Counts characters rather than bytes: Slack error details are relayed verbatim and a byte
/// slice through a multi-byte character would panic, in a release profile that aborts.
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    //! What these pin: that the dry run is the default and says so, that the table is
    //! readable and never lies about how much it is showing, and that a commit's output
    //! cannot be read as "sent".

    use super::{
        Summary, age, committed_footer, describe, dry_run_footer, elide, headline, nothing_parked,
        scope_of, table,
    };
    use crate::cli::Replay;
    use alertthread_core::{ChannelId, Fingerprint, Op, Placement};
    use alertthread_store::{DeadLetter, DeadLetterScope, OpId};
    use chrono::{DateTime, TimeDelta, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn parked(id: i64, fingerprint: Option<&str>, error: Option<&str>) -> DeadLetter {
        DeadLetter {
            id: OpId::new(id),
            op: Op::Post {
                fingerprint: Fingerprint::new(fingerprint.unwrap_or("abc")),
                channel: ChannelId::new("#alerts"),
                placement: Placement::Channel,
            },
            channel: ChannelId::new("#alerts"),
            fingerprint: fingerprint.map(Fingerprint::new),
            attempts: 10,
            last_error: error.map(ToOwned::to_owned),
            created_at: at(1_721_500_000),
            dead_lettered_at: at(1_721_503_600),
        }
    }

    #[test]
    fn no_flags_means_the_whole_dead_letter_queue() {
        assert_eq!(scope_of(&Replay::default()), DeadLetterScope::ALL);
        assert!(scope_of(&Replay::default()).is_everything());
    }

    #[test]
    fn each_flag_narrows_the_scope_it_names() {
        let scope = scope_of(&Replay {
            channel: Some("#alerts".to_owned()),
            ..Replay::default()
        });
        assert_eq!(scope.channel(), Some(&ChannelId::new("#alerts")));
        assert_eq!(scope.fingerprint(), None);

        let scope = scope_of(&Replay {
            fingerprint: Some("abc".to_owned()),
            ..Replay::default()
        });
        assert_eq!(scope.channel(), None);
        assert_eq!(scope.fingerprint(), Some(&Fingerprint::new("abc")));

        let scope = scope_of(&Replay {
            channel: Some("#alerts".to_owned()),
            fingerprint: Some("abc".to_owned()),
            ..Replay::default()
        });
        assert_eq!(scope.channel(), Some(&ChannelId::new("#alerts")));
        assert_eq!(scope.fingerprint(), Some(&Fingerprint::new("abc")));
    }

    #[test]
    fn every_scope_describes_itself_in_a_sentence() {
        // The description appears in three messages, one of which is the "nothing matched"
        // line — the only thing distinguishing a clean queue from a mistyped channel name.
        assert_eq!(
            describe(&DeadLetterScope::ALL),
            "the dead-letter queue".to_owned()
        );
        assert_eq!(
            describe(&DeadLetterScope::ALL.with_channel(ChannelId::new("#alerts"))),
            "channel #alerts"
        );
        assert_eq!(
            describe(&DeadLetterScope::ALL.with_fingerprint(Fingerprint::new("abc"))),
            "fingerprint abc"
        );
        assert_eq!(
            describe(
                &DeadLetterScope::ALL
                    .with_channel(ChannelId::new("#alerts"))
                    .with_fingerprint(Fingerprint::new("abc"))
            ),
            "channel #alerts and fingerprint abc"
        );
    }

    #[test]
    fn an_empty_result_names_the_scope_it_searched() {
        let message = nothing_parked(&DeadLetterScope::ALL.with_channel(ChannelId::new("#alrets")));
        assert!(message.contains("#alrets"), "{message}");
        assert!(message.contains("nothing to replay"), "{message}");
    }

    #[test]
    fn the_headline_counts_and_says_where() {
        let scope = DeadLetterScope::ALL.with_channel(ChannelId::new("#alerts"));
        let one = headline(&scope, 1, false);
        assert!(one.contains("1 operation is parked"), "{one}");
        assert!(one.contains("#alerts"), "{one}");

        let many = headline(&scope, 12, false);
        assert!(many.contains("12 operations are parked"), "{many}");

        // A capped listing must not report its cap as a total.
        let capped = headline(&scope, 10_000, true);
        assert!(capped.contains("At least 10000"), "{capped}");
    }

    #[test]
    fn a_dry_run_says_it_changed_nothing_and_how_to_change_something() {
        let footer = dry_run_footer(3);
        assert!(footer.contains("DRY RUN"), "{footer}");
        assert!(footer.contains("nothing has been changed"), "{footer}");
        assert!(footer.contains("--commit"), "{footer}");
        assert!(footer.contains('3'), "{footer}");
    }

    #[test]
    fn a_commit_cannot_be_read_as_having_sent_anything() {
        // The distinction this project cares about: re-queued is not delivered. An operator
        // who walked away believing otherwise would have been told the opposite of the truth.
        let footer = committed_footer(3);
        assert!(footer.contains("Returned 3"), "{footer}");
        assert!(footer.contains("queued, not sent"), "{footer}");
        assert!(
            footer.contains("If no relay is running"),
            "the case where nothing will deliver them has to be stated: {footer}"
        );
        assert!(footer.contains("alertthread_outbox_depth"), "{footer}");
    }

    #[test]
    fn the_table_shows_every_column_an_operator_needs() {
        let rendered = table(
            &[parked(1042, Some("9f2ab1c4"), Some("channel_unusable"))],
            at(1_721_600_000),
        );
        for heading in [
            "ID",
            "OP",
            "CHANNEL",
            "FINGERPRINT",
            "TRIES",
            "PARKED",
            "LAST ERROR",
        ] {
            assert!(rendered.contains(heading), "{rendered}");
        }
        assert!(rendered.contains("1042"), "{rendered}");
        assert!(rendered.contains("post"), "{rendered}");
        assert!(rendered.contains("#alerts"), "{rendered}");
        assert!(rendered.contains("9f2ab1c4"), "{rendered}");
        assert!(rendered.contains("channel_unusable"), "{rendered}");
        assert!(rendered.contains("2024-07-20T19:26:40Z"), "{rendered}");
        assert!(rendered.contains("ago"), "{rendered}");
    }

    #[test]
    fn a_row_with_no_fingerprint_renders_as_a_dash_rather_than_a_blank() {
        // A storm-collapse parent. A blank cell reads as a rendering bug; a dash reads as
        // "this op does not have one", which is the truth and is why --fingerprint skips it.
        let rendered = table(
            &[parked(7, None, Some("channel_unusable"))],
            at(1_721_600_000),
        );
        assert!(rendered.contains(" - "), "{rendered}");
    }

    #[test]
    fn a_row_parked_without_a_recorded_reason_says_so() {
        let rendered = table(&[parked(7, Some("abc"), None)], at(1_721_600_000));
        assert!(rendered.contains("(none recorded)"), "{rendered}");
    }

    #[test]
    fn the_table_says_how_many_rows_it_is_not_showing() {
        // Silently truncating would understate the number this project treats as
        // unacceptable, in the one place somebody counts it.
        let rows: Vec<DeadLetter> = (0..60)
            .map(|i| parked(i, Some("abc"), Some("boom")))
            .collect();
        let rendered = table(&rows, at(1_721_600_000));
        assert!(rendered.contains("and 10 more not shown"), "{rendered}");
    }

    #[test]
    fn columns_line_up_whatever_is_in_them() {
        // The table is the entire interface, and a misaligned one is unreadable at 3am.
        let rows = vec![
            parked(1, Some("a"), Some("short")),
            parked(1_000_000, Some("a-much-longer-fingerprint"), Some("longer")),
        ];
        let rendered = table(&rows, at(1_721_600_000));
        let lines: Vec<&str> = rendered.lines().collect();
        let column = |line: &str| line.find("TRIES").or_else(|| line.find("10"));
        assert_eq!(lines.len(), 3, "{rendered}");
        assert_eq!(
            column(lines.first().copied().unwrap_or_default()),
            column(lines.get(1).copied().unwrap_or_default()),
            "{rendered}"
        );
    }

    #[test]
    fn ages_read_at_the_scale_the_operator_is_thinking_in() {
        let then = at(1_000_000);
        assert_eq!(age(then, then), "0s");
        assert_eq!(age(then + TimeDelta::seconds(45), then), "45s");
        assert_eq!(age(then + TimeDelta::seconds(125), then), "2m5s");
        assert_eq!(age(then + TimeDelta::minutes(201), then), "3h21m");
        assert_eq!(age(then + TimeDelta::hours(52), then), "2d4h");
    }

    #[test]
    fn a_row_from_a_clock_ahead_of_this_one_says_so_rather_than_printing_a_negative() {
        let then = at(1_000_000);
        assert_eq!(age(then - TimeDelta::seconds(30), then), "in the future");
    }

    #[test]
    fn a_long_slack_error_is_elided_without_splitting_a_character() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("exactly-10", 10), "exactly-10");
        assert_eq!(elide("0123456789x", 10), "012345678…");
        // Slack details are relayed verbatim, and a byte slice through a multi-byte
        // character is a panic in a profile that aborts.
        assert_eq!(elide("ααααα", 3), "αα…");
        assert_eq!(elide("ααααα", 5), "ααααα");
    }

    #[test]
    fn a_summary_defaults_to_having_done_nothing() {
        let summary = Summary::default();
        assert_eq!(summary.matched, 0);
        assert_eq!(summary.revived, 0);
        assert!(!summary.committed);
        assert!(!summary.capped);
        assert!(format!("{summary:?}").contains("committed"));
    }
}
