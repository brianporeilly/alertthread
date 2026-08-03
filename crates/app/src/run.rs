//! Startup: everything `main.rs` would otherwise contain.
//!
//! `main.rs` is excluded from the coverage gate on the grounds that it is wiring and signal
//! handling only (ROADMAP.md, "Coverage policy"). That exclusion is only honest if the
//! wiring itself is testable, so it lives here: [`Relay::start`] takes a validated
//! [`Config`] and returns a running server plus its background tasks, and `main.rs` is left
//! with argument parsing, logging setup and a signal handler.

use std::sync::Arc;

use alertthread_slack::{Disposition, Renderer, SlackClient, SlackToken};
use alertthread_store::{Backend, StateStore, Store, WorkerId};
use anyhow::Context as _;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::auth::WebhookAuth;
use crate::config::{Config, ENV_CONFIG, ENV_LOG, ENV_LOG_FORMAT};
use crate::http::{AppState, router};
use crate::metrics::Metrics;
use crate::ratelimit::SlackLimits;
use crate::shutdown::{CancelSource, CancelToken, cancellation};
use crate::worker::{Worker, auth_probe_loop, dead_letter_loop, prune_loop, sample_loop};

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
    /// sampler, the dead-letter reporter, the auth prober.
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
/// Anything that means the relay can *never* deliver an alert: an unreachable store, a
/// migration that will not apply, a bot token Slack definitively rejects, a port already in
/// use. All of it happens before the first webhook, which is the point — ADR 001 D11's "fail
/// fast on a bad token" generalised to everything else that is fatal.
///
/// A Slack it merely cannot *reach* is not in that list; see [`authenticate`].
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
    let auth_valid = authenticate(&slack, &metrics, config.slack.auth_startup_grace).await?;

    let renderer = Arc::new(build_renderer(&config)?);
    let limits = Arc::new(SlackLimits::new(config.slack.rate_limit_divisor));
    report_webhook_auth(&config.webhook_auth());

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
        Arc::clone(&store),
        Arc::clone(&metrics),
        config.worker.sample_interval,
        shutdown.clone(),
    ));
    tasks.spawn(dead_letter_loop(
        Arc::clone(&store),
        config.worker.sample_interval,
        shutdown.clone(),
    ));
    tasks.spawn(auth_probe_loop(
        slack,
        store,
        metrics,
        config.slack.auth_probe_interval,
        auth_valid,
        shutdown,
    ));

    tracing::info!(%addr, "alertthread listening");

    Ok(Relay {
        addr,
        tasks,
        source,
    })
}

/// ADR 001 D11's startup `auth.test`, split on ADR 001 D9's error taxonomy.
///
/// Returns whether Slack accepted the token. `Err` means the relay must not start.
///
/// # Errors
///
/// Any failure `SlackError::disposition` classifies as [`Disposition::Terminal`] —
/// `invalid_auth`, `account_inactive`, `token_revoked`, a malformed token, an unusable
/// `base_url`. None of those becomes true by waiting.
async fn authenticate(
    slack: &SlackClient,
    metrics: &Metrics,
    grace: chrono::TimeDelta,
) -> anyhow::Result<bool> {
    /// The first retry delay, doubling up to `RETRY_MAX`.
    const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
    /// The longest gap between startup probes, so a long grace still gets several tries.
    const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(8);

    let deadline = std::time::Instant::now() + grace.to_std().unwrap_or(std::time::Duration::ZERO);
    let mut delay = RETRY_BASE;

    let last_error = loop {
        let error = match slack.auth_test().await {
            Ok(identity) => {
                metrics.slack_auth_valid.set(1);
                tracing::info!(
                    team = %identity.team,
                    team_id = %identity.team_id,
                    user = %identity.user,
                    bot_id = %identity.bot_id,
                    "authenticated to Slack"
                );
                return Ok(true);
            }
            Err(error) => error,
        };

        if matches!(error.disposition(), Disposition::Terminal) {
            return Err(anyhow::Error::new(error).context(
                "Slack rejected the bot token at startup, and it will not become \
                          valid by being retried",
            ));
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break error;
        }
        tracing::warn!(
            %error,
            retry_in_ms = delay.min(remaining).as_millis(),
            "could not reach Slack to check the bot token; retrying"
        );
        tokio::time::sleep(delay.min(remaining)).await;
        delay = delay.saturating_mul(2).min(RETRY_MAX);
    };

    metrics.slack_auth_valid.set(0);
    tracing::error!(
        error = %last_error,
        "could not reach Slack to check the bot token within slack.auth_startup_grace; \
         starting anyway with alertthread_slack_auth_valid=0. Webhooks will be accepted and \
         queued, and the outbox will deliver them when Slack comes back — refusing to start \
         through a Slack outage is the one behaviour the outbox exists to make unnecessary"
    );
    Ok(false)
}

/// Says which side of the webhook perimeter this process came up on.
///
/// One line at startup, always, including the default case: "is the webhook authenticated?"
/// is otherwise a question an operator can only answer by sending an unauthenticated request
/// to their own production relay.
fn report_webhook_auth(auth: &WebhookAuth) {
    let (warn, message) = webhook_auth_report(auth);
    if warn {
        tracing::warn!("{message}");
    } else {
        tracing::info!("{message}");
    }
}

/// Whether a webhook auth mode is worth a warning, and what to say about it.
///
/// Split from the emission so the wording is assertable: [`WebhookAuth::Blank`] behaves
/// exactly like the default, so the warning naming the setting is the *only* thing that
/// distinguishes "I did not configure a token" from "my token did not arrive".
const fn webhook_auth_report(auth: &WebhookAuth) -> (bool, &'static str) {
    match auth {
        WebhookAuth::Required(_) => (
            false,
            "POST /webhook requires the bearer token in server.auth_token; /healthz, /readyz \
             and /metrics do not",
        ),
        WebhookAuth::Open => (
            false,
            "POST /webhook is unauthenticated: server.auth_token is not set (ADR 001 D11 \
             makes it optional)",
        ),
        WebhookAuth::Blank => (
            true,
            "server.auth_token is set to an empty value, so POST /webhook is unauthenticated. \
             Set it to the credential Alertmanager sends, or remove it — an empty value is \
             what a chart renders for a secret that did not resolve",
        ),
    }
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
/// The command line wins, then [`ENV_CONFIG`], then nothing — which is the ordinary container
/// case, where everything comes from the environment and shipping an empty file in the image
/// to satisfy a required argument would be worse.
///
/// Which argument `given` came from is [`crate::cli`]'s problem: the relay takes it
/// positionally and `alertthread replay` takes `--config`, and both resolve here so the
/// subcommand cannot drift onto a different store from the server's.
#[must_use]
pub fn config_path(given: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    given.or_else(|| std::env::var(ENV_CONFIG).ok().map(std::path::PathBuf::from))
}

/// The variables that set the log filter, in the order they are consulted.
///
/// `RUST_LOG` needs no reservation in [`crate::config`]: it carries no `ALERTTHREAD_` prefix,
/// so the configuration layer never sees it.
pub const LOG_FILTER_VARS: [&str; 2] = [ENV_LOG, "RUST_LOG"];

/// The filter used when no variable supplies a usable one.
pub const DEFAULT_LOG_FILTER: &str = "info,alertthread=info";

/// Picks the env-filter directive from [`LOG_FILTER_VARS`], falling back to
/// [`DEFAULT_LOG_FILTER`].
///
/// Takes a lookup instead of reading the environment because [`init_tracing`] installs a
/// process-global subscriber and therefore cannot be called twice; this half can.
///
/// A variable that is set but does not parse is skipped rather than fatal, and the next one
/// is tried. Refusing to start over a malformed log filter would trade the whole relay for a
/// typo in a directive.
fn log_filter<F>(lookup: F) -> tracing_subscriber::EnvFilter
where
    F: Fn(&str) -> Option<String>,
{
    use tracing_subscriber::EnvFilter;

    LOG_FILTER_VARS
        .iter()
        .filter_map(|name| lookup(name))
        .find_map(|directive| EnvFilter::try_new(directive).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_LOG_FILTER))
}

/// Sets up structured logging.
///
/// JSON to stdout when [`ENV_LOG_FORMAT`] is `json`, human-readable otherwise. JSON is not
/// the default because the first thing anybody does with this binary is run it in a terminal
/// (PRD §6 asks for "clear, structured logging"; a wall of JSON in a terminal is structured
/// and not clear).
///
/// The filter comes from [`log_filter`], which is where the precedence lives.
pub fn init_tracing() {
    use tracing_subscriber::fmt;

    let filter = log_filter(|name| std::env::var(name).ok());

    let json =
        std::env::var(ENV_LOG_FORMAT).is_ok_and(|format| format.eq_ignore_ascii_case("json"));

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
    use super::{
        DEFAULT_LOG_FILTER, ENV_LOG, build_renderer, cancelled_token, config_path, log_filter,
        report_webhook_auth, worker_id,
    };
    use crate::auth::{WebhookAuth, WebhookToken};
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
    fn every_webhook_auth_mode_is_announced_at_startup() {
        // `Blank` behaves exactly like the default, so this warning is the only thing that
        // tells an operator their secret did not arrive. It has to name the setting.
        let (warn, message) = super::webhook_auth_report(&WebhookAuth::Blank);
        assert!(warn, "an empty auth token is a warning, not an info line");
        assert!(message.contains("server.auth_token"), "{message}");
        assert!(message.contains("unauthenticated"), "{message}");

        let (warn, message) = super::webhook_auth_report(&WebhookAuth::Open);
        assert!(!warn, "the documented default is not a warning");
        assert!(message.contains("unauthenticated"), "{message}");

        let (warn, message) =
            super::webhook_auth_report(&WebhookAuth::Required(WebhookToken::new("s3cret")));
        assert!(!warn);
        assert!(message.contains("requires"), "{message}");
        // The three endpoints the token never covers are named where somebody reading a
        // startup log will see them.
        for open in ["/healthz", "/readyz", "/metrics"] {
            assert!(message.contains(open), "{message}");
        }
        assert!(!message.contains("s3cret"), "{message}");

        // And the emitting side runs, at both levels.
        report_webhook_auth(&WebhookAuth::Blank);
        report_webhook_auth(&WebhookAuth::Open);
    }

    #[test]
    fn a_worker_identifies_itself_by_hostname_or_by_process() {
        // Recorded on every lease, so a stuck queue can be traced to the replica that
        // stopped draining it.
        let id = worker_id();
        assert!(!id.as_str().is_empty());
    }

    #[test]
    fn an_explicit_path_wins_over_the_environment() {
        // Both the relay's positional argument and `replay --config` land here, so a
        // deployment that sets ALERTTHREAD_CONFIG and then names a file on the command line
        // gets the file it named.
        assert_eq!(
            config_path(Some(std::path::PathBuf::from("/etc/alertthread.yaml"))),
            Some(std::path::PathBuf::from("/etc/alertthread.yaml"))
        );
    }

    #[test]
    fn no_argument_and_no_environment_variable_means_no_file() {
        // Configuring purely through environment variables is the normal container case,
        // and requiring a file for it would mean shipping an empty one in the image.
        if std::env::var(crate::config::ENV_CONFIG).is_ok() {
            return;
        }
        assert_eq!(config_path(None), None);
    }

    /// Reads from a fixed table instead of the process environment.
    ///
    /// `unsafe_code` is forbidden workspace-wide and `std::env::set_var` is unsafe, so a test
    /// cannot arrange one; that is why [`log_filter`] takes a lookup. The variables really
    /// being read is
    /// [`crates/app/tests/environment.rs`](../../tests/environment.rs)'s job — it starts the
    /// shipping binary with each one set.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// [`DEFAULT_LOG_FILTER`] as `EnvFilter` renders it, which reorders the directives.
    fn default() -> String {
        tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER).to_string()
    }

    #[test]
    fn nothing_set_gets_the_default_filter() {
        assert_eq!(log_filter(lookup(&[])).to_string(), default());
    }

    #[test]
    fn rust_log_sets_the_filter_when_the_project_variable_is_unset() {
        // The reason this exists: an operator with a quiet relay types RUST_LOG, because
        // that is what every other Rust binary reads. A relay nobody can turn logging up on
        // is the failure this whole change is about.
        assert_eq!(
            log_filter(lookup(&[("RUST_LOG", "debug")])).to_string(),
            "debug"
        );
    }

    #[test]
    fn the_project_variable_wins_over_rust_log() {
        // ALERTTHREAD_LOG is the documented name and the specific one. RUST_LOG is a
        // fallback, not a peer: a container that inherits RUST_LOG from an image or a
        // sidecar convention must not override what the deployment asked for by name.
        let both = lookup(&[(ENV_LOG, "alertthread=trace"), ("RUST_LOG", "error")]);
        assert_eq!(log_filter(both).to_string(), "alertthread=trace");
    }

    #[test]
    fn a_directive_that_does_not_parse_falls_through_instead_of_stopping_the_relay() {
        // Refusing to start over a malformed log filter would trade the relay for a typo.
        // The next variable is tried, and then the default — so the worst case is logging at
        // the level it would have had anyway, not silence.
        let bad_first = lookup(&[(ENV_LOG, "=@!not a directive"), ("RUST_LOG", "warn")]);
        assert_eq!(log_filter(bad_first).to_string(), "warn");

        let all_bad = lookup(&[(ENV_LOG, "=@!"), ("RUST_LOG", "=@!")]);
        assert_eq!(log_filter(all_bad).to_string(), default());
    }

    #[tokio::test]
    async fn an_already_cancelled_token_does_not_block() {
        cancelled_token().cancelled().await;
        assert!(cancelled_token().is_cancelled());
    }
}
