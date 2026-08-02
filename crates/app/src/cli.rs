//! The binary's command line.
//!
//! Two shapes, and the first one is load-bearing: **a bare `alertthread`, or `alertthread
//! <config-path>`, runs the relay.** That is what every existing entrypoint, `Dockerfile`
//! `CMD` and Kubernetes `args:` does today, and adding a subcommand must not change it.
//! Only the exact literal `replay` in `argv[1]` selects anything else, so a configuration
//! file at a path spelled any other way — including `./replay` — is still a path.
//!
//! # Why this is hand-written
//!
//! There is no argument-parsing crate in the workspace, and AGENTS.md treats a new
//! dependency as a decision to argue rather than take. The surface here is one subcommand
//! with four flags, all of which are `--name value` or a bare toggle; a parser for that is
//! shorter than the paragraph justifying pulling one in. `run::config_path` and
//! `run::wants_version` were already the hand-written version of the same thing.

use std::path::PathBuf;

/// Usage, printed by `--help` and alongside every rejected command line.
pub const USAGE: &str = "\
alertthread — Alertmanager to Slack relay with fingerprint-keyed threading.

USAGE:
    alertthread [CONFIG]                run the relay (the default)
    alertthread replay [OPTIONS]        return parked operations to the outbox
    alertthread --version               print the build identity
    alertthread --help                  print this message

CONFIG is an optional path to a YAML configuration file. Without one the relay reads
ALERTTHREAD_CONFIG, then the environment, then its defaults.

REPLAY OPTIONS:
    --channel <CHANNEL>          only operations addressed to this channel
    --fingerprint <FINGERPRINT>  only operations for this alert fingerprint
    --commit                     actually re-queue; without it this is a dry run
    --config <PATH>              configuration file, as the positional argument above
    -h, --help                   print this message

`replay` is a dry run unless --commit is given. Both filters may be combined, and
giving neither selects every parked operation.
";

/// What the command line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the relay. What a bare `alertthread` does, and what it has always done.
    Serve {
        /// The positional configuration path, if one was given.
        config: Option<PathBuf>,
    },
    /// Return parked operations to the outbox.
    Replay(Replay),
    /// Print the build identity and exit.
    ///
    /// Handled before tracing and before the async runtime: it has to work on a `scratch`
    /// image with no configuration, because proving the static binary executes is what the
    /// image smoke test is for.
    Version,
    /// Print [`USAGE`] and exit successfully.
    Help,
}

/// The arguments to `alertthread replay`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Replay {
    /// Where the configuration file is, if it is not being read from the environment.
    pub config: Option<PathBuf>,
    /// Only operations addressed to this channel.
    pub channel: Option<String>,
    /// Only operations for this alert fingerprint.
    pub fingerprint: Option<String>,
    /// Whether to actually re-queue. Off by default: a replay is a dry run until asked.
    pub commit: bool,
}

/// A command line this binary cannot act on.
///
/// Every variant names the offending argument. An operator who has just typed this at a
/// `kubectl exec` prompt is the only reader, and "invalid arguments" would send them to the
/// source.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    /// A flag this build does not know.
    #[error("unknown argument `{0}`")]
    UnknownArgument(String),
    /// A flag that takes a value, given none.
    #[error("`{0}` needs a value")]
    MissingValue(&'static str),
    /// A flag that takes a value, given an empty one.
    ///
    /// Rejected rather than ignored: an empty `--channel` is a filter that matches no row,
    /// and silently treating it as "no filter" would turn a shell variable that failed to
    /// expand into a replay of the entire queue.
    #[error("`{0}` was given an empty value")]
    EmptyValue(&'static str),
    /// The same flag twice, with different values.
    #[error("`{0}` was given more than once")]
    Repeated(&'static str),
}

/// The literal that selects the subcommand.
const REPLAY: &str = "replay";

/// Reads the process arguments, `argv[0]` included.
///
/// # Errors
///
/// [`CliError`] for anything `replay` cannot act on. The serve form accepts one positional
/// argument and does not validate it here, because it has never done so and a path that
/// does not exist is already reported by the configuration loader with the path in it.
pub fn parse<I>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().skip(1).collect();

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        return Ok(Command::Version);
    }

    match args.split_first() {
        Some((first, rest)) if first == REPLAY => parse_replay(rest),
        _ => {
            if args.iter().any(|arg| arg == "--help" || arg == "-h") {
                return Ok(Command::Help);
            }
            Ok(Command::Serve {
                config: args.first().map(PathBuf::from),
            })
        }
    }
}

/// Reads everything after the `replay` literal.
fn parse_replay(args: &[String]) -> Result<Command, CliError> {
    let mut replay = Replay::default();
    // Held as a `String` for the length of the loop so a second `--config` is seen as a
    // repeat; converting to a `PathBuf` per occurrence would forget the first one.
    let mut config = None;
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--commit" => replay.commit = true,
            "--channel" => set(&mut replay.channel, "--channel", rest.next())?,
            "--fingerprint" => set(&mut replay.fingerprint, "--fingerprint", rest.next())?,
            "--config" => set(&mut config, "--config", rest.next())?,
            other => return Err(CliError::UnknownArgument(other.to_owned())),
        }
    }

    replay.config = config.map(PathBuf::from);
    Ok(Command::Replay(replay))
}

/// Fills one `--name value` slot, refusing an empty value and a second occurrence.
fn set(
    slot: &mut Option<String>,
    name: &'static str,
    value: Option<&String>,
) -> Result<(), CliError> {
    let value = value.ok_or(CliError::MissingValue(name))?;
    if value.trim().is_empty() {
        return Err(CliError::EmptyValue(name));
    }
    if slot.is_some() {
        return Err(CliError::Repeated(name));
    }
    *slot = Some(value.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    //! What these pin: that adding a subcommand did not move the default, and that every
    //! way of getting `replay` wrong is refused by name rather than acted on approximately.

    use super::{CliError, Command, Replay, USAGE, parse};
    use std::path::PathBuf;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("alertthread".to_owned())
            .chain(rest.iter().map(|arg| (*arg).to_owned()))
            .collect()
    }

    fn command(rest: &[&str]) -> Command {
        parse(argv(rest)).expect("a command line this test expects to parse")
    }

    fn error(rest: &[&str]) -> CliError {
        parse(argv(rest)).expect_err("a command line this test expects to be refused")
    }

    #[test]
    fn a_bare_invocation_still_runs_the_relay() {
        // The whole compatibility requirement in one assertion: every deployment's
        // entrypoint is this, and a subcommand that changed it would stop a running fleet.
        assert_eq!(command(&[]), Command::Serve { config: None });
    }

    #[test]
    fn a_positional_path_still_runs_the_relay_with_that_configuration() {
        assert_eq!(
            command(&["/etc/alertthread.yaml"]),
            Command::Serve {
                config: Some(PathBuf::from("/etc/alertthread.yaml")),
            }
        );
    }

    #[test]
    fn only_the_exact_word_replay_selects_the_subcommand() {
        // A configuration file can be called anything, including something that looks like
        // the subcommand. Matching on `argv[1]` exactly is what keeps `./replay` a path.
        for path in ["./replay", "replay.yaml", "/etc/replay", "Replay"] {
            assert_eq!(
                command(&[path]),
                Command::Serve {
                    config: Some(PathBuf::from(path)),
                },
                "{path} is a configuration path, not the subcommand"
            );
        }
    }

    #[test]
    fn the_version_flag_wins_wherever_it_appears() {
        // It is answered before tracing and before the runtime, so it cannot be conditional
        // on anything else parsing.
        assert_eq!(command(&["--version"]), Command::Version);
        assert_eq!(command(&["-V"]), Command::Version);
        assert_eq!(command(&["replay", "--version"]), Command::Version);
        assert_eq!(command(&["/etc/alertthread.yaml", "-V"]), Command::Version);
        // argv[0] is skipped, so a binary installed at a path spelled like the flag serves.
        assert_eq!(
            parse(vec!["--version".to_owned()]).expect("argv[0] is not a flag"),
            Command::Serve { config: None }
        );
    }

    #[test]
    fn help_is_available_from_both_forms() {
        assert_eq!(command(&["--help"]), Command::Help);
        assert_eq!(command(&["-h"]), Command::Help);
        assert_eq!(command(&["replay", "--help"]), Command::Help);
        assert_eq!(command(&["replay", "-h"]), Command::Help);
    }

    #[test]
    fn replay_with_no_flags_is_a_dry_run_over_the_whole_queue() {
        // Both halves matter. `commit: false` is the safety property; the empty filters are
        // what makes "show me everything that is parked" the thing you get for typing the
        // least.
        assert_eq!(command(&["replay"]), Command::Replay(Replay::default()));
        let Command::Replay(replay) = command(&["replay"]) else {
            panic!("replay parses to a replay");
        };
        assert!(
            !replay.commit,
            "a replay is a dry run until asked otherwise"
        );
        assert_eq!(replay.channel, None);
        assert_eq!(replay.fingerprint, None);
    }

    #[test]
    fn replay_reads_every_flag_it_documents() {
        assert_eq!(
            command(&[
                "replay",
                "--channel",
                "#alerts",
                "--fingerprint",
                "9f2ab1c4",
                "--config",
                "/etc/alertthread.yaml",
                "--commit",
            ]),
            Command::Replay(Replay {
                config: Some(PathBuf::from("/etc/alertthread.yaml")),
                channel: Some("#alerts".to_owned()),
                fingerprint: Some("9f2ab1c4".to_owned()),
                commit: true,
            })
        );
    }

    #[test]
    fn flag_order_does_not_change_the_result() {
        assert_eq!(
            command(&["replay", "--commit", "--channel", "#alerts"]),
            command(&["replay", "--channel", "#alerts", "--commit"])
        );
    }

    #[test]
    fn a_flag_with_no_value_is_refused_by_name() {
        assert_eq!(
            error(&["replay", "--channel"]),
            CliError::MissingValue("--channel")
        );
        assert_eq!(
            error(&["replay", "--fingerprint"]),
            CliError::MissingValue("--fingerprint")
        );
        assert_eq!(
            error(&["replay", "--config"]),
            CliError::MissingValue("--config")
        );
        assert!(
            error(&["replay", "--channel"])
                .to_string()
                .contains("--channel")
        );
    }

    #[test]
    fn an_empty_filter_is_refused_rather_than_treated_as_no_filter() {
        // `--channel "$CHANNEL"` with an unset variable would otherwise silently widen a
        // targeted replay into the whole queue.
        assert_eq!(
            error(&["replay", "--channel", ""]),
            CliError::EmptyValue("--channel")
        );
        assert_eq!(
            error(&["replay", "--channel", "   "]),
            CliError::EmptyValue("--channel")
        );
        assert_eq!(
            error(&["replay", "--fingerprint", ""]),
            CliError::EmptyValue("--fingerprint")
        );
        assert_eq!(
            error(&["replay", "--config", ""]),
            CliError::EmptyValue("--config")
        );
    }

    #[test]
    fn a_repeated_filter_is_refused_rather_than_silently_taking_one() {
        // Taking the last would mean `--channel #a --channel #b` replays `#b` while reading
        // as though it asked for both.
        assert_eq!(
            error(&["replay", "--channel", "#a", "--channel", "#b"]),
            CliError::Repeated("--channel")
        );
        assert_eq!(
            error(&["replay", "--fingerprint", "a", "--fingerprint", "b"]),
            CliError::Repeated("--fingerprint")
        );
        assert_eq!(
            error(&["replay", "--config", "a", "--config", "b"]),
            CliError::Repeated("--config")
        );
    }

    #[test]
    fn an_unknown_replay_flag_is_refused_and_names_itself() {
        // Ignoring it is the dangerous reading: `--dry-run` silently ignored next to
        // `--commit` would send every parked alert.
        let error = error(&["replay", "--dry-run"]);
        assert_eq!(error, CliError::UnknownArgument("--dry-run".to_owned()));
        assert!(error.to_string().contains("--dry-run"));
        assert!(matches!(
            self::error(&["replay", "#alerts"]),
            CliError::UnknownArgument(ref arg) if arg == "#alerts"
        ));
    }

    #[test]
    fn repeating_commit_is_not_an_error() {
        // It is a toggle, not a value, so a second one asks for nothing new.
        assert_eq!(
            command(&["replay", "--commit", "--commit"]),
            Command::Replay(Replay {
                commit: true,
                ..Replay::default()
            })
        );
    }

    #[test]
    fn the_usage_message_documents_every_flag_the_parser_accepts() {
        // A flag that works and is undocumented is a flag nobody uses; one that is
        // documented and does not work is worse. This is the only place both are listed.
        for flag in [
            "replay",
            "--channel",
            "--fingerprint",
            "--commit",
            "--config",
            "--version",
            "--help",
        ] {
            assert!(USAGE.contains(flag), "usage does not mention {flag}");
        }
        assert!(
            USAGE.contains("dry run"),
            "the default has to be stated where somebody will read it"
        );
    }
}
