//! The bot token, and the one thing that must never happen to it.

use std::fmt;

/// A Slack bot token (`xoxb-…`).
///
/// The only reason this is a newtype rather than a `String` is its [`fmt::Debug`] impl,
/// which prints `SlackToken(<redacted>)` and never the value. ADR 001 D11 requires a
/// redacting `Debug` on anything holding the token; putting it on the token itself rather
/// than on the config struct means every future struct that embeds one inherits the
/// property instead of having to remember it.
///
/// AGENTS.md: a committed token is a burned token. A token in a `tracing` field of a
/// `#[derive(Debug)]` struct is committed to whatever ships those logs.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackToken(String);

impl SlackToken {
    /// Wraps a bot token.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the token, for the one place that needs it: the `Authorization` header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SlackToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackToken(<redacted>)")
    }
}

impl From<String> for SlackToken {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SlackToken {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::SlackToken;

    #[test]
    fn a_token_carries_its_value_for_the_authorization_header() {
        assert_eq!(SlackToken::new("xoxb-secret").expose(), "xoxb-secret");
    }

    #[test]
    fn debug_never_prints_the_token() {
        // The whole reason this type exists. A token that reaches a log line is burned,
        // and `#[derive(Debug)]` on a config struct is how it gets there.
        let token = SlackToken::new("xoxb-1234-5678-abcdefghijklmnop");
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "SlackToken(<redacted>)");
        assert!(!rendered.contains("xoxb"), "{rendered}");
        assert!(!rendered.contains("abcdefghijklmnop"), "{rendered}");
    }

    #[test]
    fn debug_redacts_inside_a_derived_debug_too() {
        // The property has to survive being embedded, because that is how it will be used.
        #[derive(Debug)]
        struct Config {
            #[expect(dead_code, reason = "read only by the derived Debug this test checks")]
            token: SlackToken,
        }
        let rendered = format!(
            "{:?}",
            Config {
                token: SlackToken::new("xoxb-secret"),
            }
        );
        assert!(!rendered.contains("xoxb-secret"), "{rendered}");
    }

    #[test]
    fn a_token_can_be_built_from_either_string_type() {
        assert_eq!(SlackToken::from("xoxb-a"), SlackToken::new("xoxb-a"));
        assert_eq!(
            SlackToken::from("xoxb-a".to_owned()),
            SlackToken::new("xoxb-a")
        );
    }
}
