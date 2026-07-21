//! Turning rendered text into blocks Slack will accept.
//!
//! # Why this is not just `blocks: [section(body)]`
//!
//! Slack rejects a `section` whose text exceeds 3000 characters, and a message carrying
//! more than 50 blocks, with `invalid_blocks`. That is a
//! [`Terminal`](crate::Disposition::Terminal) error — the op dead-letters, and the alert
//! is never seen. A `PrometheusRule` with a long `description`, or one whose annotation
//! interpolates a `for` loop over pod names, will hit the first limit without anybody
//! having done anything unusual.
//!
//! So the limits are applied here, before the call, and the result says so. Truncation is
//! recorded in the returned [`Truncation`] *and* rendered into the message as a visible
//! context line: an operator who sees a cut-off description should not have to correlate
//! a metric to work out why.
//!
//! Nothing in this module can panic. It counts characters rather than bytes — Slack's
//! limit is in characters, and counting them also makes UTF-8 boundaries a non-question —
//! and it never indexes or slices, which `indexing_slicing` denies anyway.

use crate::message::{Block, MAX_BLOCKS, MAX_NOTIFICATION_CHARS, MAX_SECTION_CHARS};

/// What had to be dropped to fit Slack's limits.
///
/// Returned rather than logged, so Phase 4 can count it. A relay that truncates every
/// message from one noisy rule and says nothing is a relay quietly losing information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Truncation {
    /// How many characters of rendered text did not fit.
    pub dropped_chars: usize,
    /// How many whole section blocks did not fit.
    pub dropped_blocks: usize,
}

/// How many section blocks a message may hold.
///
/// One below Slack's limit, because the truncation notice itself is a block and has to
/// fit inside the same budget. Reserving it unconditionally rather than only when
/// truncating costs one block and removes an off-by-one from the path that only executes
/// during the failure it exists to describe.
const MAX_SECTIONS: usize = MAX_BLOCKS - 1;

/// Splits rendered text into section blocks, applying both of Slack's limits.
///
/// Chunks break at the last newline inside the limit when there is one, so a paragraph is
/// not cut mid-word for the sake of a boundary nobody can see.
pub(crate) fn sections(text: &str) -> (Vec<Block>, Option<Truncation>) {
    let tidied = tidy(text);
    let trimmed = tidied.as_str();
    if trimmed.is_empty() {
        // An empty message is silence with extra steps. The caller's fallback handles the
        // "template produced nothing" case; this is the last line of defence.
        return (Vec::new(), None);
    }

    let chunks = chunk(trimmed, MAX_SECTION_CHARS);
    let kept: Vec<&String> = chunks.iter().take(MAX_SECTIONS).collect();
    let dropped: Vec<&String> = chunks.iter().skip(MAX_SECTIONS).collect();

    let mut blocks: Vec<Block> = kept.into_iter().map(Block::section).collect();

    if dropped.is_empty() {
        return (blocks, None);
    }

    let truncation = Truncation {
        dropped_chars: dropped.iter().map(|c| c.chars().count()).sum(),
        dropped_blocks: dropped.len(),
    };
    blocks.push(Block::context(format!(
        ":scissors: _truncated to fit Slack's limits — {} characters omitted. \
         Shorten the annotation or the template._",
        truncation.dropped_chars
    )));
    (blocks, Some(truncation))
}

/// Normalises the whitespace a Jinja template leaves behind.
///
/// Trailing spaces at the end of a line and runs of blank lines are what `{% if %}` and
/// `{% for %}` blocks produce unless every one of them carries whitespace control, and
/// getting all of that right is not a reasonable thing to ask of somebody overriding a
/// template. Slack renders both faithfully, so the message ends up with visible gaps in
/// it that the author never wrote.
///
/// Only whitespace is touched, and trailing-space-as-a-line-break is not a thing in
/// Slack's `mrkdwn`, so nothing an author could have meant is lost.
fn tidy(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0_usize;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            // One blank line separates paragraphs; more is a gap in the message.
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push(trimmed);
    }
    out.join("\n").trim().to_owned()
}

/// Splits `text` into runs of at most `limit` characters, preferring newline boundaries.
///
/// `limit` is assumed non-zero; it is only ever called with [`MAX_SECTION_CHARS`]. A zero
/// limit would still terminate — the "no newline found" path always emits the whole
/// buffer — but it would emit one block per call, which is why the assumption is stated.
fn chunk(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0_usize;
    let mut last_break: Option<usize> = None;

    for c in text.chars() {
        current.push(c);
        count += 1;
        if c == '\n' {
            last_break = Some(count);
        }
        if count >= limit {
            // Prefer the last newline, but only if it leaves a chunk worth having.
            // Breaking at character 3 of a 3000-character budget would produce 999 near
            // -empty blocks out of one long unbroken line.
            let split = last_break.filter(|at| *at * 2 > limit).unwrap_or(count);
            let head: String = current.chars().take(split).collect();
            let tail: String = current.chars().skip(split).collect();
            chunks.push(head);
            count = tail.chars().count();
            current = tail;
            last_break = None;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// The notification preview: the first line, flattened and shortened.
///
/// This is what appears in a desktop notification and in the channel list, so it wants to
/// be the alert's identity and nothing else. Slack markup is stripped rather than
/// rendered, because a notification showing `*:rotating_light: FIRING*` is worse than one
/// showing `FIRING`.
pub(crate) fn notification(text: &str) -> String {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();

    let stripped: String = first_line
        .chars()
        .filter(|c| *c != '*' && *c != '`')
        .collect();
    let squeezed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    if squeezed.chars().count() > MAX_NOTIFICATION_CHARS {
        let kept: String = squeezed.chars().take(MAX_NOTIFICATION_CHARS).collect();
        format!("{kept}…")
    } else if squeezed.is_empty() {
        // Slack requires a non-empty `text` when blocks are used, or the notification is
        // blank. Never let that be the reason an alert goes unnoticed.
        "alertthread notification".to_owned()
    } else {
        squeezed
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SECTIONS, Truncation, chunk, notification, sections, tidy};
    use crate::message::{Block, MAX_BLOCKS, MAX_SECTION_CHARS};

    fn section_text(block: &Block) -> String {
        match block {
            Block::Section { text } => text.text.clone(),
            Block::Context { elements } => {
                elements.first().map(|t| t.text.clone()).unwrap_or_default()
            }
        }
    }

    #[test]
    fn short_text_becomes_exactly_one_section() {
        let (blocks, truncation) = sections("*FIRING* CephOSDDown");
        assert_eq!(blocks.len(), 1);
        assert_eq!(section_text(&blocks[0]), "*FIRING* CephOSDDown");
        assert_eq!(truncation, None);
    }

    #[test]
    fn surrounding_whitespace_from_a_template_is_trimmed() {
        // Jinja control blocks leave newlines behind; a leading blank line in a Slack
        // message is a visible gap above the alert name.
        let (blocks, _) = sections("\n\n  body  \n\n");
        assert_eq!(section_text(&blocks[0]), "body");
    }

    #[test]
    fn text_that_is_entirely_whitespace_produces_no_blocks() {
        let (blocks, truncation) = sections("   \n\t\n  ");
        assert!(blocks.is_empty());
        assert_eq!(truncation, None);
    }

    #[test]
    fn trailing_spaces_and_stacked_blank_lines_left_by_jinja_are_cleaned_up() {
        // A `{% for %}` that emits "`k=v` " per item leaves a trailing space on the line,
        // and every `{% if %}` without whitespace control leaves a blank line behind.
        // Slack renders both faithfully, so the author sees gaps they never wrote.
        assert_eq!(tidy("a   \nb\t\n"), "a\nb");
        assert_eq!(tidy("a\n\n\n\n\nb"), "a\n\nb");
        // Leading whitespace survives — see the next test — so only the first line's is
        // removed here, by the overall trim.
        assert_eq!(tidy("\n\n  a  \n\n\n  b  \n\n"), "a\n\n  b");
    }

    #[test]
    fn tidying_keeps_a_single_blank_line_because_that_is_a_paragraph_break() {
        assert_eq!(tidy("a\n\nb"), "a\n\nb");
        assert_eq!(tidy("a\nb"), "a\nb");
    }

    #[test]
    fn tidying_does_not_touch_leading_indentation() {
        // Slack renders leading spaces, and an author may want them.
        assert_eq!(tidy("a\n    indented"), "a\n    indented");
    }

    #[test]
    fn a_long_annotation_is_split_rather_than_rejected_by_slack() {
        // The concrete failure this exists to prevent: a section over 3000 characters is
        // `invalid_blocks`, which is terminal, which is a dead-lettered alert.
        let body = "x".repeat(MAX_SECTION_CHARS * 2 + 10);
        let (blocks, truncation) = sections(&body);
        assert_eq!(blocks.len(), 3);
        assert_eq!(truncation, None);
        for block in &blocks {
            assert!(
                section_text(block).chars().count() <= MAX_SECTION_CHARS,
                "a block exceeded Slack's section limit"
            );
        }
        let rejoined: String = blocks.iter().map(section_text).collect();
        assert_eq!(rejoined, body, "splitting must not lose or reorder text");
    }

    #[test]
    fn a_chunk_boundary_prefers_a_newline() {
        // Cutting mid-word inside a description is avoidable noise; cutting at a line
        // break is invisible.
        let line = "y".repeat(2_000);
        let body = format!("{line}\n{}", "z".repeat(2_000));
        let (blocks, _) = sections(&body);
        assert_eq!(blocks.len(), 2);
        assert!(section_text(&blocks[0]).ends_with('\n'));
        assert_eq!(section_text(&blocks[0]).chars().count(), 2_001);
    }

    #[test]
    fn an_early_newline_is_not_used_as_a_boundary() {
        // A newline at character 3 of a 3000-character budget would produce a message
        // made of hundreds of near-empty blocks, which hits the *other* limit.
        let body = format!("a\n{}", "b".repeat(MAX_SECTION_CHARS * 2));
        let (blocks, _) = sections(&body);
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            section_text(&blocks[0]).chars().count(),
            MAX_SECTION_CHARS,
            "the early newline should have been ignored"
        );
    }

    #[test]
    fn text_beyond_the_block_limit_is_dropped_with_a_visible_notice() {
        // Slack's other limit. The notice is a block itself, which is why the budget for
        // content is 49 and not 50.
        let body = "q".repeat(MAX_SECTION_CHARS * (MAX_SECTIONS + 2));
        let (blocks, truncation) = sections(&body);

        assert_eq!(blocks.len(), MAX_BLOCKS, "must fill exactly to the limit");
        assert!(blocks.len() <= MAX_BLOCKS);

        let truncation = truncation.expect("truncation must be reported");
        assert_eq!(truncation.dropped_blocks, 2);
        assert_eq!(truncation.dropped_chars, MAX_SECTION_CHARS * 2);

        let notice = section_text(blocks.last().expect("a notice block"));
        assert!(notice.contains("truncated"), "{notice}");
        assert!(
            notice.contains(&truncation.dropped_chars.to_string()),
            "the notice must say how much was lost: {notice}"
        );
        assert!(
            matches!(blocks.last(), Some(Block::Context { .. })),
            "the notice is a context block, not a section"
        );
    }

    #[test]
    fn truncation_defaults_to_nothing_dropped() {
        assert_eq!(
            Truncation::default(),
            Truncation {
                dropped_chars: 0,
                dropped_blocks: 0
            }
        );
        assert!(format!("{:?}", Truncation::default()).contains("dropped_chars"));
    }

    #[test]
    fn chunking_counts_characters_and_not_bytes() {
        // Slack's limit is in characters, and counting them is also what makes a UTF-8
        // boundary a non-question. A byte-based split would cut a multi-byte character
        // in half and produce a body Slack rejects outright.
        let body = "é".repeat(MAX_SECTION_CHARS + 5);
        let chunks = chunk(&body, MAX_SECTION_CHARS);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MAX_SECTION_CHARS);
        assert_eq!(chunks[1].chars().count(), 5);
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn chunking_text_exactly_at_the_limit_produces_one_chunk() {
        let body = "w".repeat(MAX_SECTION_CHARS);
        assert_eq!(chunk(&body, MAX_SECTION_CHARS), vec![body]);
    }

    #[test]
    fn chunking_empty_text_produces_nothing() {
        assert!(chunk("", MAX_SECTION_CHARS).is_empty());
    }

    #[test]
    fn the_notification_is_the_first_line_without_markup() {
        assert_eq!(
            notification("*:rotating_light: FIRING* · `CephOSDDown`\nosd.3 is down"),
            ":rotating_light: FIRING · CephOSDDown"
        );
    }

    #[test]
    fn the_notification_skips_leading_blank_lines() {
        assert_eq!(notification("\n\n  headline  \nbody"), "headline");
    }

    #[test]
    fn a_long_notification_is_shortened() {
        let long = "n".repeat(500);
        let preview = notification(&long);
        assert_eq!(preview.chars().count(), 201);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn a_notification_is_never_empty() {
        // Slack shows a blank desktop notification for an empty `text` when blocks are
        // used. A silent notification is the failure mode this project is named after.
        for input in ["", "   ", "\n\n", "***", "``"] {
            assert_eq!(notification(input), "alertthread notification", "{input:?}");
        }
    }
}
