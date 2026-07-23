//! The browser face: server-rendered HTML, no JavaScript, no build step.
//!
//! Threads render as nested lists and the attachment colour renders as the bar
//! down the left, because the two things this page exists to make visible are a
//! child sitting under its parent and a red message turning green in place.

use chrono::{SecondsFormat, Utc};
use minijinja::{Environment, Value, context};

use crate::messages::WorkspaceView;

/// How often the page reloads itself, in seconds.
pub(crate) const REFRESH_SECONDS: u32 = 3;

/// Slack shortcodes the built-in templates emit.
const EMOJI: [(&str, &str); 4] = [
    (":rotating_light:", "🚨"),
    (":white_check_mark:", "✅"),
    (":warning:", "⚠️"),
    (":mag:", "🔍"),
];

/// Builds the template environment.
///
/// # Errors
///
/// If the compiled-in template does not compile, which is a build-time mistake.
pub(crate) fn environment() -> Result<Environment<'static>, minijinja::Error> {
    let mut environment = Environment::new();
    environment.add_template("index.html", include_str!("../templates/index.html"))?;
    environment.add_filter("mrkdwn", |text: &str| {
        Value::from_safe_string(to_html(text))
    });
    Ok(environment)
}

/// Renders the whole workspace.
///
/// # Errors
///
/// If rendering fails; the caller answers 500 rather than panicking.
pub(crate) fn render(
    environment: &Environment<'_>,
    view: &WorkspaceView,
) -> Result<String, minijinja::Error> {
    environment.get_template("index.html")?.render(context! {
        workspace => Value::from_serialize(view),
        refresh => REFRESH_SECONDS,
        generated_at => Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    })
}

/// Slack `mrkdwn` as HTML: links, bold, italic, code, emoji, line breaks.
fn to_html(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;

    // Links are lifted out first: `<url|label>` has to be read before `<` is
    // escaped, and its label must not be scanned for emphasis.
    while let Some(open) = rest.find('<') {
        out.push_str(&inline(rest.get(..open).unwrap_or_default()));
        let after = rest.get(open + 1..).unwrap_or_default();
        if let Some(close) = after.find('>') {
            let link = after.get(..close).unwrap_or_default();
            let (url, label) = link.split_once('|').unwrap_or((link, link));
            out.push_str("<a href=\"");
            out.push_str(&escape(url));
            out.push_str("\" rel=\"noreferrer\">");
            out.push_str(&escape(label));
            out.push_str("</a>");
            rest = after.get(close + 1..).unwrap_or_default();
        } else {
            out.push_str("&lt;");
            rest = after;
        }
    }
    out.push_str(&inline(rest));

    for (code, glyph) in EMOJI {
        if out.contains(code) {
            out = out.replace(code, glyph);
        }
    }
    out.replace('\n', "<br>")
}

/// Bold, italic and code, applied only where the delimiter actually closes.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some((at, delimiter, tag)) = next_delimiter(rest) {
        let after = rest.get(at + delimiter.len_utf8()..).unwrap_or_default();
        if let Some(close) = after.find(delimiter) {
            out.push_str(&escape(rest.get(..at).unwrap_or_default()));
            out.push('<');
            out.push_str(tag);
            out.push('>');
            out.push_str(&escape(after.get(..close).unwrap_or_default()));
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
            rest = after
                .get(close + delimiter.len_utf8()..)
                .unwrap_or_default();
        } else {
            // An unpaired delimiter is literal text, not the start of a tag that
            // never closes.
            out.push_str(&escape(
                rest.get(..at + delimiter.len_utf8()).unwrap_or_default(),
            ));
            rest = after;
        }
    }
    out.push_str(&escape(rest));
    out
}

/// The earliest emphasis delimiter in `text`, with the tag it opens.
fn next_delimiter(text: &str) -> Option<(usize, char, &'static str)> {
    [('`', "code"), ('*', "strong"), ('_', "em")]
        .into_iter()
        .filter_map(|(delimiter, tag)| text.find(delimiter).map(|at| (at, delimiter, tag)))
        .min_by_key(|(at, _, _)| *at)
}

/// The four characters that would otherwise close a tag we did not open.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::{environment, to_html};
    use crate::messages::WorkspaceView;

    #[test]
    fn markup_becomes_tags() {
        assert_eq!(to_html("*bold*"), "<strong>bold</strong>");
        assert_eq!(to_html("_soft_"), "<em>soft</em>");
        assert_eq!(to_html("`code`"), "<code>code</code>");
        assert_eq!(to_html("a\nb"), "a<br>b");
    }

    #[test]
    fn a_link_keeps_its_label() {
        assert_eq!(
            to_html("<http://example.test/x|source>"),
            "<a href=\"http://example.test/x\" rel=\"noreferrer\">source</a>"
        );
        assert_eq!(
            to_html("<http://example.test/x>"),
            "<a href=\"http://example.test/x\" rel=\"noreferrer\">http://example.test/x</a>"
        );
    }

    #[test]
    fn injected_markup_is_escaped_rather_than_rendered() {
        // Alert labels come from whatever fired the rule, so this page renders
        // untrusted text. The `<…>` reads as Slack link syntax, but the payload
        // is escaped into the anchor's attribute and text — never emitted as a
        // live `<script>` tag.
        let rendered = to_html("<script>alert(1)</script>");
        assert!(
            !rendered.contains("<script"),
            "no executable tag survives: {rendered}"
        );
        assert_eq!(to_html("a & b"), "a &amp; b");
        assert_eq!(to_html("`a & b`"), "<code>a &amp; b</code>");
    }

    #[test]
    fn an_unpaired_delimiter_is_literal_text() {
        // Otherwise a stray asterisk in an annotation would open a tag that
        // never closes and swallow the rest of the page.
        assert_eq!(to_html("2 * 3 is 6"), "2 * 3 is 6");
        assert_eq!(to_html("half `open"), "half `open");
    }

    #[test]
    fn shortcodes_become_emoji() {
        assert_eq!(to_html(":rotating_light: fire"), "🚨 fire");
        assert_eq!(to_html("*:white_check_mark: ok*"), "<strong>✅ ok</strong>");
    }

    #[test]
    fn the_page_renders_when_nothing_has_been_posted_yet() {
        // The first thing a newcomer sees, and the one state the tutorial cannot
        // avoid showing them.
        let environment = environment().expect("the template compiles");
        let empty = WorkspaceView {
            message_count: 0,
            channels: Vec::new(),
        };
        let page = super::render(&environment, &empty).expect("the page renders");
        assert!(page.contains("<html"));
        assert!(page.contains("Nothing posted yet"));
    }
}
