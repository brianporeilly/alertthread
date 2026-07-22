//! Binary entry point for `alertthread`.
//!
//! Deliberately thin: argument handling, logging setup, and a signal handler. Everything
//! with a decision in it lives in the library — [`alertthread::run::start`] opens the store,
//! builds the client and spawns every task — which is what makes excluding this file from
//! the coverage gate honest rather than convenient. See the coverage policy in `ROADMAP.md`.
//!
//! `expect` appears once below, on the `Config` that failed to load. AGENTS.md permits it in
//! `main()` startup, and the alternative — a `?` that prints a bare `Debug` — is a worse
//! message for the one person who ever reads it: an operator whose container will not start.

use alertthread::run;

fn main() -> std::process::ExitCode {
    // Before tracing and before the runtime: `--version` has to work on a `scratch` image
    // with no configuration, because proving the static binary executes is what the image
    // smoke test is for.
    if run::wants_version(std::env::args()) {
        println!("{}", alertthread::build_identity());
        return std::process::ExitCode::SUCCESS;
    }

    run::init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("alertthread: could not start the async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        match serve().await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                // `{error:#}` rather than `{error}`: anyhow's alternate form prints the
                // whole context chain, and every layer of it was written to be the thing an
                // operator needs — "could not open the sqlite state store: unable to open
                // database file" says where to look; the last line alone does not.
                tracing::error!("alertthread failed to start: {error:#}");
                eprintln!("alertthread: {error:#}");
                std::process::ExitCode::FAILURE
            }
        }
    })
}

/// Loads the configuration, starts the relay, and waits for a signal.
async fn serve() -> anyhow::Result<()> {
    tracing::info!("{}", alertthread::build_identity());

    let path = run::config_path(std::env::args());
    let config = alertthread::config::Config::load(path.as_deref())?;
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
