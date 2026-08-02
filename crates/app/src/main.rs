//! Binary entry point for `alertthread`.
//!
//! Deliberately thin: argument dispatch, logging setup, and a signal handler. Everything
//! with a decision in it lives in the library — [`alertthread::cli::parse`] reads the
//! command line, [`alertthread::run::start`] opens the store and spawns every task, and
//! [`alertthread::replay::run`] is the subcommand — which is what makes excluding this file
//! from the coverage gate honest rather than convenient. See the coverage policy in
//! `ROADMAP.md`.
//!
//! Nothing here decides anything. `Command::Serve` with no path is what a bare `alertthread`
//! has always produced, and this file's only job is to hand each variant to the code that
//! already knows what to do with it.

use std::process::ExitCode;

use alertthread::cli::{self, Command, Replay};
use alertthread::{config::Config, replay, run};

fn main() -> ExitCode {
    // Before tracing and before the runtime. `--version` has to work on a `scratch` image
    // with no configuration, because proving the static binary executes is what the image
    // smoke test is for; and a mistyped flag should not have to build a runtime to be told
    // about.
    let command = match cli::parse(std::env::args()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("alertthread: {error}\n\n{}", cli::USAGE);
            return ExitCode::FAILURE;
        }
    };

    match command {
        Command::Version => {
            println!("{}", alertthread::build_identity());
            return ExitCode::SUCCESS;
        }
        Command::Help => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Command::Serve { .. } | Command::Replay(_) => {}
    }

    run::init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("alertthread: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        let result = match command {
            Command::Replay(args) => run_replay(&args).await,
            Command::Serve { config } => serve(config).await,
            // Both were answered above, before the runtime existed.
            Command::Version | Command::Help => Ok(()),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                // `{error:#}` rather than `{error}`: anyhow's alternate form prints the
                // whole context chain, and every layer of it was written to be the thing an
                // operator needs — "could not open the sqlite state store: unable to open
                // database file" says where to look; the last line alone does not.
                tracing::error!("alertthread failed: {error:#}");
                eprintln!("alertthread: {error:#}");
                ExitCode::FAILURE
            }
        }
    })
}

/// Loads the configuration, starts the relay, and waits for a signal.
async fn serve(config: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    tracing::info!("{}", alertthread::build_identity());

    let config = Config::load(run::config_path(config).as_deref())?;
    tracing::info!(config = ?config, "configuration loaded");

    let mut relay = run::start(config).await?;

    tokio::select! {
        result = run::signal() => result?,
        () = relay.wait() => {
            tracing::error!("a relay task exited on its own; shutting down");
        }
    }

    relay.shutdown().await;
    Ok(())
}

/// Runs `alertthread replay` against the configuration the server would have used.
///
/// Same loader, same precedence, same file — so a replay cannot quietly act on a different
/// store from the one the relay is draining.
async fn run_replay(args: &Replay) -> anyhow::Result<()> {
    let config = Config::load(run::config_path(args.config.clone()).as_deref())?;

    let mut stdout = std::io::stdout().lock();
    replay::run(args, &config, chrono::Utc::now(), &mut stdout).await?;
    Ok(())
}
