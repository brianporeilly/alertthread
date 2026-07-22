//! Startup: everything `main.rs` would otherwise contain.
//!
//! `main.rs` is excluded from the coverage gate on the grounds that it is wiring and signal
//! handling only (ROADMAP.md, "Coverage policy"). That exclusion is only honest if the
//! wiring itself is testable, so it lives here: [`Relay::start`] takes a validated
//! [`Config`] and returns a running server plus its background tasks, and `main.rs` is left
//! with argument parsing, logging setup and a signal handler.

use std::sync::Arc;

use alertthread_slack::{Renderer, SlackClient, SlackToken};
use alertthread_store::{Backend, StateStore, Store, WorkerId};
use anyhow::Context as _;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::http::{AppState, router};
use crate::metrics::Metrics;
use crate::ratelimit::SlackLimits;
use crate::shutdown::{CancelSource, CancelToken, cancellation};
use crate::worker::{Worker, auth_probe_loop, prune_loop, sample_loop};

/// A running relay.
///
/// Held by `main.rs` so it can wait for the server and then stop the background tasks.
///
/// `Debug` prints the address and nothing else. There is no secret in here — the token
/// lives inside the Slack client — but a `JoinSet` renders as nothing useful and the address
/// is the one fact worth having in a log line.
pub struct Relay {
    /// The address the server actually bound, which is not the configured one when the
    /// configured port was `0`. Tests need it; so does anybody reading a startup log.
    pub addr: std::net::SocketAddr,
    /// Every long-running task: the HTTP server, the outbox worker, the pruner, the
    /// sampler, the auth prober.
    tasks: JoinSet<()>,
    /// The other end of the shutdown flag.
    source: CancelSource,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("addr", &self.addr)
            .field("tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

impl Relay {
    /// Signals every task to stop and waits for them.
    ///
    /// The worker finishes the batch it is holding rather than abandoning its leases. An
    /// abandoned lease is not a bug — it expires and is reclaimed — but waiting a full
    /// lease duration is a lease duration an alert spends undelivered for no reason.
    pub async fn shutdown(mut self) {
        self.source.cancel();
        while let Some(finished) = self.tasks.join_next().await {
            if let Err(error) = finished {
                tracing::error!(%error, "a task did not shut down cleanly");
            }
        }
        tracing::info!("shutdown complete");
    }

    /// Waits until any task exits on its own.
    ///
    /// In a healthy process this never returns: every task loops until shutdown. A task
    /// that ends early means something has gone wrong that the process cannot continue
    /// past — the listener closed, say — and a relay that kept running with no HTTP server
    /// would be a pod that passes its liveness probe and accepts nothing.
    pub async fn wait(&mut self) {
        if let Some(Err(error)) = self.tasks.join_next().await {
            tracing::error!(%error, "a relay task exited unexpectedly");
        }
    }
}

/// Opens the store, builds the client, binds the socket and starts every task.
///
/// # Errors
///
/// Anything that means the relay cannot deliver an alert: an unreachable store, a migration
/// that will not apply, a bot token Slack rejects, a port already in use. All of it happens
/// before the first webhook, which is the point — ADR 001 D11's "fail fast on a bad token"
/// generalised to everything else that is fatal.
pub async fn start(config: Config) -> anyhow::Result<Relay> {
    let metrics = Arc::new(Metrics::new());

    let backend = Backend::parse(&config.storage.backend)
        .context("storage.backend names a backend this build does not have")?;
    let store = Arc::new(
        Store::connect(backend, &config.storage.url)
            .await
            .with_context(|| format!("could not open the {backend} state store"))?,
    );
    store
        .migrate()
        .await
        .context("could not apply the migrations this build ships")?;
    tracing::info!(%backend, "state store ready");

    let slack = Arc::new(build_client(&config)?);

    // ADR 001 D11: call `auth.test` once at startup, log the resolved identity, and fail
    // fast on a bad token. Failing here rather than at the first alert is the whole value:
    // a container that will not start is visible, and a relay that starts and cannot post
    // is not.
    let identity = slack
        .auth_test()
        .await
        .context("Slack rejected the bot token at startup")?;
    metrics.slack_auth_valid.set(1);
    tracing::info!(
        team = %identity.team,
        team_id = %identity.team_id,
        user = %identity.user,
        bot_id = %identity.bot_id,
        "authenticated to Slack"
    );

    let renderer = Arc::new(build_renderer(&config)?);
    let limits = Arc::new(SlackLimits::new(config.slack.rate_limit_divisor));

    let listener = TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("could not bind {}", config.server.listen))?;
    let addr = listener
        .local_addr()
        .context("the listening socket has no address")?;

    let (source, shutdown) = cancellation();
    let state = Arc::new(AppState::new(
        Arc::clone(&store),
        Arc::clone(&metrics),
        &config,
    ));
    let mut tasks = JoinSet::new();

    let serving = shutdown.clone();
    tasks.spawn(async move {
        let served = axum::serve(listener, router(state))
            .with_graceful_shutdown(async move { serving.cancelled().await })
            .await;
        if let Err(error) = served {
            tracing::error!(%error, "the HTTP server stopped");
        }
    });

    let worker = Worker::new(
        Arc::clone(&store),
        Arc::clone(&slack),
        renderer,
        limits,
        Arc::clone(&metrics),
        config.worker,
        worker_id(),
    );
    let draining = shutdown.clone();
    tasks.spawn(async move { worker.run(draining).await });

    tasks.spawn(prune_loop(
        Arc::clone(&store),
        config.storage.retention.policy(),
        config.storage.retention.interval,
        shutdown.clone(),
    ));
    tasks.spawn(sample_loop(
        store,
        Arc::clone(&metrics),
        config.worker.sample_interval,
        shutdown.clone(),
    ));
    tasks.spawn(auth_probe_loop(
        slack,
        metrics,
        config.slack.auth_probe_interval,
        shutdown,
    ));

    tracing::info!(%addr, "alertthread listening");

    Ok(Relay {
        addr,
        tasks,
        source,
    })
}

/// Builds the Slack client from the validated configuration.
fn build_client(config: &Config) -> anyhow::Result<SlackClient> {
    let token: SlackToken = config
        .token()
        .context("no Slack bot token; configuration validation should have caught this")?;

    SlackClient::builder(token)
        .base_url(config.slack.base_url.clone())
        .timeout(
            config
                .slack
                .timeout
                .to_std()
                .context("slack.timeout is not a positive duration")?,
        )
        .build()
        .context("could not build the Slack client")
}

/// Installs template overrides, reporting the ones that were refused.
///
/// A rejected override is a warning, never a failure. ADR 001 D9's argument applies one step
/// earlier than it was written for: a pod that refuses to start over a typo in a `ConfigMap`
/// is total silence, which is strictly worse than the degraded-but-alive outcome D9 chooses
/// everywhere else.
///
/// # Errors
///
/// Only if `templates.dir` itself cannot be listed. That is an operator naming a path that
/// is not there, which is worth failing on — unlike a stray file inside a directory that is.
fn build_renderer(config: &Config) -> anyhow::Result<Renderer> {
    let (overrides, skipped) = config
        .templates()
        .context("could not read the template override directory")?;

    for name in skipped {
        tracing::warn!(file = %name, "ignoring a file that is not one of the four templates");
    }

    let installed: Vec<_> = overrides.keys().map(ToString::to_string).collect();
    let (renderer, rejected) = Renderer::new(overrides);

    for refusal in &rejected {
        tracing::error!(
            template = %refusal.template,
            detail = %refusal.detail,
            "a template override does not compile; keeping the built-in. The ConfigMap you \
             just applied is not in effect"
        );
    }
    if !installed.is_empty() {
        tracing::info!(templates = ?installed, "installed template overrides");
    }

    Ok(renderer)
}

/// This replica's identity, recorded on every lease it takes.
///
/// The pod name in Kubernetes, so a stuck queue can be traced to the process that stopped
/// draining it — which is the first question asked when
/// `alertthread_outbox_oldest_age_seconds` climbs. Falls back to the process id, which is at
/// least unique on one host.
fn worker_id() -> WorkerId {
    let name = std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()));
    WorkerId::new(name)
}

/// Waits for `SIGTERM` or `SIGINT`.
///
/// Both, because Kubernetes sends `SIGTERM` and a developer pressing Ctrl-C sends `SIGINT`,
/// and a relay that only handled one of them would either ignore the operator or ignore the
/// orchestrator.
///
/// # Errors
///
/// If the signal handlers cannot be installed, which on Linux means the process is in a
/// state where it could not have started.
pub async fn signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).context("could not listen for SIGTERM")?;
    let mut int = signal(SignalKind::interrupt()).context("could not listen for SIGINT")?;

    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received; draining"),
        _ = int.recv() => tracing::info!("SIGINT received; draining"),
    }
    Ok(())
}

/// Where the configuration file lives, when there is one.
///
/// One optional positional argument, and nothing else. A relay with a flag parser is a relay
/// with a flag parser to keep working; everything configurable is configurable through the
/// file or the environment, which is what a container needs anyway.
#[must_use]
pub fn config_path(mut args: impl Iterator<Item = String>) -> Option<std::path::PathBuf> {
    args.nth(1).map(std::path::PathBuf::from).or_else(|| {
        std::env::var("ALERTTHREAD_CONFIG")
            .ok()
            .map(std::path::PathBuf::from)
    })
}

/// Whether the command line asks for the version instead of a run.
///
/// Kept out of `main.rs` so it is testable, and out of `config_path` so that a file
/// literally named `--version` is still a path rather than a surprise.
pub fn wants_version(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

/// Sets up structured logging.
///
/// JSON to stdout when `ALERTTHREAD_LOG_FORMAT=json`, human-readable otherwise. JSON is not
/// the default because the first thing anybody does with this binary is run it in a terminal
/// (PRD §6 asks for "clear, structured logging"; a wall of JSON in a terminal is structured
/// and not clear).
pub fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("ALERTTHREAD_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,alertthread=info"));

    let json = std::env::var("ALERTTHREAD_LOG_FORMAT")
        .is_ok_and(|format| format.eq_ignore_ascii_case("json"));

    if json {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// A [`CancelToken`] that is already cancelled, for a caller that wants one pass and no loop.
#[must_use]
pub fn cancelled_token() -> CancelToken {
    let (source, token) = cancellation();
    source.cancel();
    token
}

#[cfg(test)]
mod tests {
    use super::{build_renderer, cancelled_token, config_path, worker_id};
    use crate::config::Config;
    use alertthread_core::Fingerprint;
    use alertthread_slack::{AlertView, RenderRequest};
    use chrono::{DateTime, Utc};
    use figment::Figment;
    use figment::providers::{Format, Serialized, Yaml};

    const MINIMAL: &str = r##"
slack:
  token: "xoxb-test-token"
  default_channel: "#alerts"
"##;

    fn config(yaml: &str) -> Config {
        Config::from_figment(
            &Figment::from(Serialized::defaults(Config::default())).merge(Yaml::string(yaml)),
        )
        .expect("the test configuration starts")
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp is in range")
    }

    fn view() -> AlertView {
        AlertView {
            fingerprint: Fingerprint::new("abc"),
            labels: [("alertname".to_owned(), "CephOSDDown".to_owned())]
                .into_iter()
                .collect(),
            annotations: alertthread_core::LabelMap::new(),
            starts_at: at(0),
            resolved_at: None,
            generator_url: String::new(),
        }
    }

    #[test]
    fn a_relay_with_no_template_directory_gets_the_built_in_templates() {
        let renderer = build_renderer(&config(MINIMAL)).expect("no directory is not an error");
        let view = view();
        let message = renderer.render(&RenderRequest::Firing(&view), at(60));
        assert!(message.is_intact(), "{:?}", message.degraded);
    }

    #[test]
    fn a_template_override_that_does_not_compile_does_not_stop_the_relay_starting() {
        // ADR 001 D9's argument, one step earlier than it was written for: a pod that
        // refuses to start over a typo in a ConfigMap is total silence, which is strictly
        // worse than degraded-but-alive.
        let dir = std::env::temp_dir().join(format!("alertthread-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("firing"), "{% for x in %}broken").expect("writing a template");

        let config = config(&format!(
            "{MINIMAL}\ntemplates:\n  dir: {}\n",
            dir.display()
        ));
        let renderer = build_renderer(&config).expect("a broken override is not fatal");

        let view = view();
        let message = renderer.render(&RenderRequest::Firing(&view), at(60));
        assert!(
            message.is_intact(),
            "the built-in must still be in place: {:?}",
            message.degraded
        );
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_template_override_that_compiles_replaces_the_built_in() {
        let dir = std::env::temp_dir().join(format!("alertthread-run-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("firing.j2"), "custom {{ alert.alertname }}")
            .expect("writing a template");

        let config = config(&format!(
            "{MINIMAL}\ntemplates:\n  dir: {}\n",
            dir.display()
        ));
        let renderer = build_renderer(&config).expect("a valid override installs");

        let view = view();
        let message = renderer.render(&RenderRequest::Firing(&view), at(60));
        let text = serde_json::to_string(&message.body).expect("body serialises");
        assert!(text.contains("custom CephOSDDown"), "{text}");
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_template_directory_that_is_not_there_is_fatal() {
        // Unlike a stray file *inside* a directory that exists: this is an operator naming
        // a path that is not mounted, which is a deployment mistake worth failing on.
        let config = config(&format!(
            "{MINIMAL}\ntemplates:\n  dir: /nonexistent/alertthread/templates\n"
        ));
        assert!(build_renderer(&config).is_err());
    }

    #[test]
    fn a_worker_identifies_itself_by_hostname_or_by_process() {
        // Recorded on every lease, so a stuck queue can be traced to the replica that
        // stopped draining it.
        let id = worker_id();
        assert!(!id.as_str().is_empty());
    }

    #[test]
    fn the_version_flag_is_recognised_and_nothing_else_is() {
        let args = |rest: &[&str]| {
            std::iter::once("alertthread".to_owned())
                .chain(rest.iter().map(|s| (*s).to_owned()))
                .collect::<Vec<_>>()
        };
        assert!(super::wants_version(args(&["--version"])));
        assert!(super::wants_version(args(&["-V"])));
        assert!(!super::wants_version(args(&[])));
        assert!(!super::wants_version(args(&["/etc/alertthread.yaml"])));
        // argv[0] is skipped, so a binary at a path spelled like the flag still serves.
        assert!(!super::wants_version(vec!["--version".to_owned()]));
    }

    #[test]
    fn the_configuration_path_comes_from_the_first_argument() {
        let args = ["alertthread".to_owned(), "/etc/alertthread.yaml".to_owned()];
        assert_eq!(
            config_path(args.into_iter()),
            Some(std::path::PathBuf::from("/etc/alertthread.yaml"))
        );
    }

    #[test]
    fn no_argument_and_no_environment_variable_means_no_file() {
        // Configuring purely through environment variables is the normal container case,
        // and requiring a file for it would mean shipping an empty one in the image.
        if std::env::var("ALERTTHREAD_CONFIG").is_ok() {
            return;
        }
        assert_eq!(config_path(["alertthread".to_owned()].into_iter()), None);
    }

    #[tokio::test]
    async fn an_already_cancelled_token_does_not_block() {
        cancelled_token().cancelled().await;
        assert!(cancelled_token().is_cancelled());
    }
}
