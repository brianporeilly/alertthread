//! Configuration: defaults, then a YAML file, then the environment.
//!
//! # Refusing to start is a feature here
//!
//! Three settings make the relay incoherent rather than merely odd, and all three are
//! rejected at startup rather than at the first webhook:
//!
//! - **No channel anywhere.** ADR 001 D8's resolution order is `?channel=` →
//!   `slack.default_channel` → refuse to start. A relay with neither would accept alerts,
//!   persist them, and then be unable to say where they go — which is an outbox full of
//!   work nobody can drain.
//! - **Both resolve behaviours off.** ADR 001 D6: a resolve that does nothing is
//!   indistinguishable from the bug this project exists to fix. Enforced by
//!   [`Policy::validate`], which is called here.
//! - **No bot token.** There is no degraded mode without one.
//!
//! Everything *else* degrades rather than refusing. A template override that will not
//! compile is dropped and the built-in kept (ADR 001 D9's reasoning, applied one step
//! earlier); a missing config file is not an error, because the environment alone is a
//! perfectly good way to configure a container.
//!
//! # No secret reaches a log line
//!
//! [`SlackConfig::token`] is a [`SlackToken`] and [`ServerConfig::auth_token`] is a
//! [`WebhookToken`], and the `Debug` of each prints `<redacted>`. That is why they are
//! newtypes rather than `String`s: the property is inherited by every struct that embeds one,
//! instead of being something each new struct has to remember.
//! `debug_never_shows_the_bot_token` and `debug_never_shows_the_webhook_token` are the tests
//! that say so.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use alertthread_core::{ChannelId, Policy, PolicyError};
use alertthread_slack::{DEFAULT_BASE_URL, SlackToken, TemplateKind};
use alertthread_store::{Backend, RetentionPolicy};
use chrono::TimeDelta;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::{Figment, Profile};
use serde::{Deserialize, Deserializer, Serialize};

use crate::auth::{WebhookAuth, WebhookToken};

/// The environment-variable prefix for everything below.
///
/// Nested keys are separated by a double underscore, because a single one is legal inside a
/// key name: `ALERTTHREAD_SLACK__DEFAULT_CHANNEL` is unambiguous where
/// `ALERTTHREAD_SLACK_DEFAULT_CHANNEL` would need the reader to know the schema.
pub const ENV_PREFIX: &str = "ALERTTHREAD_";

/// The nesting separator inside an environment variable name.
pub const ENV_SEPARATOR: &str = "__";

/// Everything the relay reads at startup.
///
/// `Debug` is derived and is safe to log: the only secret is the bot token, and it is a
/// [`SlackToken`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the HTTP server listens, and how it behaves.
    pub server: ServerConfig,
    /// Where correlation state lives (ADR 001 D4).
    pub storage: StorageConfig,
    /// How to talk to Slack (ADR 001 D1, D8).
    pub slack: SlackConfig,
    /// What a resolution does (ADR 001 D6).
    pub resolve: ResolveConfig,
    /// When a batch becomes a threaded summary (ADR 001 D5).
    pub collapse: CollapseConfig,
    /// How the outbox is drained (ADR 001 D2).
    pub worker: WorkerConfig,
    /// Message template overrides (ADR 001 D10).
    pub templates: TemplateConfig,
}

/// `server.*`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// The address to bind. Defaults to every interface on port 8080.
    pub listen: SocketAddr,
    /// How long a request may take before the server abandons it.
    ///
    /// Comfortably above the 50 ms p99 ADR 001 D2 targets for ingest, because the thing it
    /// is protecting against is a store that has stopped answering — and in that case the
    /// right outcome is a fast `503` that Alertmanager retries, not a socket held open.
    #[serde(with = "duration")]
    pub request_timeout: TimeDelta,
    /// How long to let in-flight work finish after a shutdown signal.
    ///
    /// A clean shutdown drains leased ops rather than relying on their leases expiring — an
    /// abandoned lease is not a bug, but waiting 60 seconds for one is 60 seconds an alert
    /// spends undelivered for no reason.
    #[serde(with = "duration")]
    pub shutdown_grace: TimeDelta,
    /// The bearer token `POST /webhook` requires (ADR 001 D11, "Security").
    ///
    /// Unset by default, which leaves the webhook unauthenticated. Redacted in `Debug` by
    /// [`WebhookToken`], and skipped when this struct is serialised for exactly the reason
    /// [`no_secret`] gives.
    ///
    /// `/healthz`, `/readyz` and `/metrics` are never affected by it — see [`crate::auth`].
    #[serde(default, serialize_with = "no_secret")]
    pub auth_token: Option<WebhookToken>,
    /// A file to read [`Self::auth_token`] from, if it is mounted rather than passed in.
    ///
    /// The usual Kubernetes shape, and the same trailing-newline handling as
    /// [`SlackConfig::token_file`].
    pub auth_token_file: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            request_timeout: TimeDelta::seconds(15),
            shutdown_grace: TimeDelta::seconds(20),
            auth_token: None,
            auth_token_file: None,
        }
    }
}

/// `storage.*`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// `sqlite` or `postgres` (ADR 001 D4).
    pub backend: String,
    /// The connection string, in the dialect of the backend.
    pub url: String,
    /// How long finished state is kept.
    pub retention: RetentionConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Sqlite.as_str().to_owned(),
            url: "sqlite:///var/lib/alertthread/state.sqlite".to_owned(),
            retention: RetentionConfig::default(),
        }
    }
}

/// `storage.retention.*` (ADR 001 D4; PRD §5.7).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// How long a resolved alert's correlation state is kept.
    #[serde(with = "duration")]
    pub resolved: TimeDelta,
    /// How long an alert is kept after it was last seen, whatever state it is in.
    #[serde(with = "duration")]
    pub stale: TimeDelta,
    /// How often the pruner sweeps.
    #[serde(with = "duration")]
    pub interval: TimeDelta,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            resolved: TimeDelta::days(RetentionPolicy::DEFAULT_RESOLVED_DAYS),
            stale: TimeDelta::days(RetentionPolicy::DEFAULT_STALE_DAYS),
            interval: TimeDelta::hours(1),
        }
    }
}

impl RetentionConfig {
    /// The store's view of this policy.
    #[must_use]
    pub const fn policy(&self) -> RetentionPolicy {
        RetentionPolicy {
            resolved_after: self.resolved,
            stale_after: self.stale,
        }
    }
}

/// `slack.*`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlackConfig {
    /// The bot token, `xoxb-…`.
    ///
    /// Redacted in `Debug` by [`SlackToken`], and skipped entirely when this struct is
    /// serialised — `Serialize` is only here so `figment` can layer defaults, and a token
    /// written into a defaults provider is a token in a `Figment`'s error messages.
    #[serde(
        default,
        deserialize_with = "token",
        serialize_with = "no_secret",
        alias = "bot_token"
    )]
    pub token: Option<SlackToken>,
    /// A file to read the token from, if it is mounted rather than passed in.
    ///
    /// The usual Kubernetes shape. Trailing whitespace is trimmed, because
    /// `kubectl create secret --from-file` keeps the newline and the resulting error names
    /// HTTP headers rather than tokens.
    pub token_file: Option<PathBuf>,
    /// Where to post when the webhook URL carries no `?channel=` (ADR 001 D8).
    pub default_channel: Option<String>,
    /// The Slack API root. Pointed at `dev/slack-mock` locally.
    pub base_url: String,
    /// How long one Slack call may take.
    #[serde(with = "duration")]
    pub timeout: TimeDelta,
    /// Divides the local rate limits by the replica count (ADR 001 D2).
    pub rate_limit_divisor: f64,
    /// How often to re-check that the bot token is still valid.
    ///
    /// Startup already fails fast on a bad token (ADR 001 D11). This covers the case that
    /// leaves: a token revoked at 2pm with nothing firing until 3am, discovered at the worst
    /// possible moment. The result feeds `alertthread_slack_auth_valid` and **not**
    /// `/readyz` — see [`crate::http`] for why.
    #[serde(with = "duration")]
    pub auth_probe_interval: TimeDelta,
    /// How long to keep retrying a *transient* startup `auth.test` before starting anyway.
    ///
    /// A token Slack definitively rejects still refuses to start, whatever this is set to
    /// (ADR 001 D9's terminal errors, ADR 001 D11's fail-fast). This bounds only the other
    /// case: a relay restarted during a Slack outage, a DNS blip or a proxy 503, where
    /// refusing to start is refusing to accept the webhooks the outbox exists to hold.
    ///
    /// It is a bound rather than an unlimited retry because a pod stuck in startup is
    /// invisible to `/readyz` and to `/metrics` alike, so "keep trying" is indistinguishable
    /// from "hung". Past it the relay starts with `alertthread_slack_auth_valid = 0` and
    /// lets the 15-minute prober report recovery.
    #[serde(with = "duration")]
    pub auth_startup_grace: TimeDelta,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            token: None,
            token_file: None,
            default_channel: None,
            base_url: DEFAULT_BASE_URL.to_owned(),
            timeout: TimeDelta::seconds(15),
            rate_limit_divisor: 1.0,
            auth_probe_interval: TimeDelta::minutes(15),
            // Comfortably inside a default Kubernetes startup budget, and long enough to
            // ride out the blips that make up most of them.
            auth_startup_grace: TimeDelta::seconds(30),
        }
    }
}

/// `resolve.*` (ADR 001 D6).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResolveConfig {
    /// Rewrite the original message.
    pub update_in_place: bool,
    /// Post a threaded reply, which is what generates the unread indicator.
    pub thread_reply: bool,
}

impl Default for ResolveConfig {
    fn default() -> Self {
        Self {
            update_in_place: true,
            thread_reply: true,
        }
    }
}

/// `collapse.*` (ADR 001 D5).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollapseConfig {
    /// New messages one delivery may produce for a channel before it is collapsed into a
    /// threaded summary. `0` disables collapse entirely.
    pub threshold: usize,
    /// How long after a message was last seen a repeat counts as a genuine
    /// `repeat_interval` re-send rather than an HTTP retry (ADR 001 D7).
    #[serde(with = "duration")]
    pub refresh_debounce: TimeDelta,
}

impl Default for CollapseConfig {
    fn default() -> Self {
        Self {
            threshold: Policy::DEFAULT_COLLAPSE_THRESHOLD,
            refresh_debounce: TimeDelta::seconds(Policy::DEFAULT_REFRESH_DEBOUNCE_SECONDS),
        }
    }
}

/// `worker.*` (ADR 001 D2).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    /// How many outbox rows one lease takes.
    pub batch_size: u32,
    /// How long a lease is held before it becomes reclaimable.
    #[serde(with = "duration")]
    pub lease: TimeDelta,
    /// How long to wait after finding nothing to do.
    ///
    /// Short, because it is also how long a self-deferred op waits past its
    /// `next_attempt_at` — and the per-channel rate limiter defers roughly one op per
    /// channel per tick, so a long poll would turn one message per second into one message
    /// per poll.
    #[serde(with = "duration")]
    pub idle_poll: TimeDelta,
    /// How many attempts an op gets before it is dead-lettered (ADR 001 D9).
    pub max_attempts: i32,
    /// The first backoff delay. Doubles per attempt, up to `backoff_max`.
    #[serde(with = "duration")]
    pub backoff_base: TimeDelta,
    /// The longest a backoff ever waits.
    #[serde(with = "duration")]
    pub backoff_max: TimeDelta,
    /// How often the store is sampled for the gauges in ADR 001 D11.
    #[serde(with = "duration")]
    pub sample_interval: TimeDelta,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch_size: 64,
            lease: TimeDelta::seconds(60),
            idle_poll: TimeDelta::milliseconds(250),
            // ADR 001 D9: "up to `max_attempts` (default 10, ~30 min)".
            max_attempts: 10,
            backoff_base: TimeDelta::seconds(4),
            backoff_max: TimeDelta::minutes(10),
            sample_interval: TimeDelta::seconds(15),
        }
    }
}

/// `templates.*` (ADR 001 D10).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TemplateConfig {
    /// A directory of overrides, one file per template, named after it.
    ///
    /// The `ConfigMap` shape. Files whose names are not one of the four templates are
    /// ignored with a warning rather than rejected: a `ConfigMap` mount brings `..data` and
    /// friends with it, and refusing to start over a symlink Kubernetes created would be a
    /// relay that cannot run in the place it is designed for.
    pub dir: Option<PathBuf>,
}

/// A configuration the relay refuses to start with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file or the environment could not be read into the schema.
    #[error("configuration is not valid: {0}")]
    Figment(#[from] Box<figment::Error>),

    /// `storage.backend` named something that is not a backend.
    #[error(transparent)]
    Backend(#[from] alertthread_store::StoreError),

    /// `resolve.*` or `collapse.refresh_debounce` is incoherent (ADR 001 D6, D7).
    #[error(transparent)]
    Policy(#[from] PolicyError),

    /// Neither a token nor a readable token file.
    #[error(
        "no Slack bot token: set slack.token, ALERTTHREAD_SLACK__TOKEN, or slack.token_file. \
         There is no degraded mode without one — the relay would accept every alert and be \
         unable to deliver any of them"
    )]
    NoToken,

    /// `slack.token_file` was set and could not be read.
    #[error("slack.token_file {path} could not be read: {source}")]
    UnreadableToken {
        /// The path that was configured.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },

    /// ADR 001 D8's last resort: no `?channel=` and no default.
    #[error(
        "no default channel: set slack.default_channel or ALERTTHREAD_SLACK__DEFAULT_CHANNEL. \
         ADR 001 D8 resolves the channel as ?channel= then slack.default_channel then refuse \
         to start, and refusing to start means now rather than at the first webhook — an \
         alert that arrives with nowhere to go is one the relay has already acknowledged"
    )]
    NoDefaultChannel,

    /// `server.auth_token_file` was set and could not be read.
    ///
    /// Fatal rather than degrading to an unauthenticated webhook: the operator asked for a
    /// perimeter, and a relay that quietly served without one would be a security setting
    /// nobody could tell was missing.
    #[error("server.auth_token_file {path} could not be read: {source}")]
    UnreadableWebhookToken {
        /// The path that was configured.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },

    /// A template override directory was configured and could not be read.
    #[error("templates.dir {path} could not be read: {source}")]
    UnreadableTemplates {
        /// The directory that was configured.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        Self::Figment(Box::new(error))
    }
}

impl Config {
    /// The layering ADR 001 D1 describes: built-in defaults, then a YAML file, then the
    /// environment.
    ///
    /// A missing file is not an error. Configuring a container purely through environment
    /// variables is the normal case, and requiring a file for it would mean shipping an
    /// empty one in the image.
    #[must_use]
    pub fn figment(file: Option<&Path>) -> Figment {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));
        if let Some(path) = file {
            // `Yaml::file` is already tolerant of a missing path; being explicit here is
            // what makes that a decision rather than a behaviour somebody has to look up.
            figment = figment.merge(Yaml::file(path));
        }
        figment.merge(
            Env::prefixed(ENV_PREFIX)
                .split(ENV_SEPARATOR)
                .map(|key| key.as_str().to_lowercase().into()),
        )
    }

    /// Reads, layers and validates the configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for anything that would leave the relay unable to deliver an alert.
    /// Every variant is something an operator sees in a container that has failed to start,
    /// so every message says what to set and where.
    pub fn load(file: Option<&Path>) -> Result<Self, ConfigError> {
        Self::from_figment(&Self::figment(file))
    }

    /// Extracts and validates from an already-built [`Figment`].
    ///
    /// Separate from [`Self::load`] so tests can layer a configuration without touching the
    /// process environment, which is global and would make them order-dependent.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn from_figment(figment: &Figment) -> Result<Self, ConfigError> {
        let mut config: Self = figment.extract()?;
        config.resolve_token()?;
        config.resolve_webhook_token()?;
        config.validate()?;
        Ok(config)
    }

    /// Folds `slack.token_file` into `slack.token`.
    fn resolve_token(&mut self) -> Result<(), ConfigError> {
        let Some(path) = self.slack.token_file.clone() else {
            return Ok(());
        };
        // The file wins over an inline token: a mounted secret is the more specific answer,
        // and a deployment that sets both meant the mount.
        let raw = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::UnreadableToken { path, source })?;
        // `kubectl create secret --from-file` keeps the trailing newline, and the error a
        // newline produces further downstream names HTTP headers rather than tokens.
        self.slack.token = Some(SlackToken::new(raw.trim()));
        Ok(())
    }

    /// Folds `server.auth_token_file` into `server.auth_token`.
    fn resolve_webhook_token(&mut self) -> Result<(), ConfigError> {
        let Some(path) = self.server.auth_token_file.clone() else {
            return Ok(());
        };
        // The file wins over an inline value, for the same reason it does for the bot token.
        let raw = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::UnreadableWebhookToken { path, source })?;
        self.server.auth_token = Some(WebhookToken::new(raw.trim()));
        Ok(())
    }

    /// The three refusals this module's documentation lists.
    fn validate(&self) -> Result<(), ConfigError> {
        Backend::parse(&self.storage.backend)?;
        self.policy().validate()?;

        if self
            .slack
            .token
            .as_ref()
            .is_none_or(|token| token.expose().trim().is_empty())
        {
            return Err(ConfigError::NoToken);
        }

        // ADR 001 D8's third step. Checked at startup rather than at the first webhook,
        // because by the time a webhook arrives the relay has already told Alertmanager it
        // would take it.
        if self
            .slack
            .default_channel
            .as_ref()
            .is_none_or(|channel| channel.trim().is_empty())
        {
            return Err(ConfigError::NoDefaultChannel);
        }

        Ok(())
    }

    /// The planner's view of this configuration.
    #[must_use]
    pub fn policy(&self) -> Policy {
        Policy {
            collapse_threshold: self.collapse.threshold,
            refresh_debounce: self.collapse.refresh_debounce,
            resolve_update_in_place: self.resolve.update_in_place,
            resolve_thread_reply: self.resolve.thread_reply,
        }
    }

    /// The default channel, which [`Self::validate`] has already proved exists.
    ///
    /// Returns `None` only for a `Config` built by hand rather than through
    /// [`Self::load`] — the handler falls back to a `500` in that case rather than posting
    /// somewhere nobody asked for.
    #[must_use]
    pub fn default_channel(&self) -> Option<ChannelId> {
        self.slack
            .default_channel
            .as_ref()
            .map(|name| ChannelId::new(name.trim()))
    }

    /// The bot token, which [`Self::validate`] has already proved exists.
    #[must_use]
    pub fn token(&self) -> Option<SlackToken> {
        self.slack.token.clone()
    }

    /// What `server.auth_token` resolves to (ADR 001 D11's optional bearer token).
    ///
    /// A configured-but-blank value is [`WebhookAuth::Blank`] rather than an error: refusing
    /// to start would be silence caused by an *optional* security setting, and treating it as
    /// `Open` without saying so would leave an operator believing the webhook is closed. It is
    /// reported at startup instead.
    #[must_use]
    pub fn webhook_auth(&self) -> WebhookAuth {
        match self.server.auth_token.as_ref() {
            None => WebhookAuth::Open,
            Some(token) if token.is_blank() => WebhookAuth::Blank,
            Some(token) => WebhookAuth::Required(token.clone()),
        }
    }

    /// Reads the template overrides from `templates.dir`.
    ///
    /// A file whose name is not one of the four templates is skipped and reported, not
    /// rejected: a `ConfigMap` mount brings `..data` and dotted symlinks with it, and a
    /// relay that refused to start over one could not run in the place it is designed for.
    /// A file that is not readable at all is skipped for the same reason — the built-in
    /// template is a working fallback, and ADR 001 D9's argument applies one step earlier.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnreadableTemplates`] if the directory itself cannot be listed, which
    /// means the operator configured a path that is not there — a mistake worth naming,
    /// unlike a stray file inside a directory that is.
    pub fn templates(&self) -> Result<(BTreeMap<TemplateKind, String>, Vec<String>), ConfigError> {
        let mut overrides = BTreeMap::new();
        let mut skipped = Vec::new();

        let Some(dir) = self.templates.dir.as_ref() else {
            return Ok((overrides, skipped));
        };

        let entries =
            std::fs::read_dir(dir).map_err(|source| ConfigError::UnreadableTemplates {
                path: dir.clone(),
                source,
            })?;

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(kind) = TemplateKind::parse(&name) else {
                skipped.push(name);
                continue;
            };
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    overrides.insert(kind, source);
                }
                Err(error) => skipped.push(format!("{name}: {error}")),
            }
        }

        Ok((overrides, skipped))
    }
}

/// Deserialises a bot token into the newtype that redacts it.
fn token<'de, D: Deserializer<'de>>(de: D) -> Result<Option<SlackToken>, D::Error> {
    Ok(Option::<String>::deserialize(de)?.map(SlackToken::new))
}

/// Serialises a secret as absent, always.
///
/// `Serialize` exists on [`SlackConfig`] and [`ServerConfig`] so `figment` can use the
/// `Default` as its defaults layer. Writing a secret into that layer would put it inside a
/// `Figment`, whose error messages quote the values they came from — which is a token in a
/// startup log, which is a burned token.
#[expect(
    clippy::ref_option,
    reason = "the signature is dictated by serde's serialize_with, which always hands the \
              field by reference"
)]
fn no_secret<T, S: serde::Serializer>(
    _secret: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_none()
}

/// Durations, as an operator writes them.
///
/// `7d`, `30s`, `250ms`, `1h30m`. A bare number is seconds, which is the reading somebody
/// who wrote `timeout: 15` meant.
///
/// Hand-rolled rather than pulling in a duration crate: this project deliberately runs a
/// small dependency surface, and the whole grammar is four suffixes.
mod duration {
    use chrono::TimeDelta;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    /// Parses `1h30m`, `250ms`, `7d`, `90`.
    ///
    /// # Errors
    ///
    /// A message naming the input and the suffixes that are understood. This reaches an
    /// operator whose container will not start.
    pub(super) fn parse(raw: &str) -> Result<TimeDelta, String> {
        let raw = raw.trim();
        // A leading `-` is understood so that a negative value reaches `Policy::validate`,
        // which rejects it by name and explains why (ADR 001 D7). Refusing to *parse* it
        // would produce a message about duration syntax for what is a semantic mistake.
        let (negative, trimmed) = match raw.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        if trimmed.is_empty() {
            return Err("a duration cannot be empty".to_owned());
        }
        if negative {
            return parse(trimmed).map(|delta| -delta);
        }

        let mut total = TimeDelta::zero();
        let mut digits = String::new();
        let mut unit = String::new();
        let mut saw_unit = false;

        for c in trimmed.chars() {
            if c.is_ascii_digit() {
                if !unit.is_empty() {
                    total += component(&digits, &unit, trimmed)?;
                    digits.clear();
                    unit.clear();
                }
                digits.push(c);
            } else {
                saw_unit = true;
                unit.push(c);
            }
        }

        if unit.is_empty() && saw_unit {
            return Err(format!("{trimmed:?} is not a duration"));
        }
        if unit.is_empty() {
            // A bare number. Seconds is the reading somebody who wrote `timeout: 15` meant.
            return seconds(&digits, trimmed);
        }
        Ok(total + component(&digits, &unit, trimmed)?)
    }

    fn seconds(digits: &str, whole: &str) -> Result<TimeDelta, String> {
        let value: i64 = digits
            .parse()
            .map_err(|_| format!("{whole:?} is not a duration"))?;
        Ok(TimeDelta::seconds(value))
    }

    fn component(digits: &str, unit: &str, whole: &str) -> Result<TimeDelta, String> {
        let value: i64 = digits.parse().map_err(|_| {
            format!("{whole:?} is not a duration: expected a number before {unit:?}")
        })?;
        // Checked construction: `TimeDelta::days(i64::MAX)` panics, and a typo in a config
        // file must not be able to abort a process that denies `panic` everywhere else.
        let delta = match unit {
            "ms" => TimeDelta::try_milliseconds(value),
            "s" => TimeDelta::try_seconds(value),
            "m" => TimeDelta::try_minutes(value),
            "h" => TimeDelta::try_hours(value),
            "d" => TimeDelta::try_days(value),
            other => {
                return Err(format!(
                    "{whole:?} is not a duration: {other:?} is not one of ms, s, m, h, d"
                ));
            }
        };
        delta.ok_or_else(|| format!("{whole:?} is longer than a duration can be"))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<TimeDelta, D::Error> {
        // Accepts both `30s` and a bare YAML integer, because a `30` in a YAML file arrives
        // as an integer and rejecting it would be pedantry aimed at the wrong audience.
        let raw = Raw::deserialize(de)?;
        match raw {
            Raw::Text(text) => parse(&text).map_err(D::Error::custom),
            Raw::Number(seconds) => TimeDelta::try_seconds(seconds)
                .ok_or_else(|| D::Error::custom(format!("{seconds} is longer than a duration"))),
        }
    }

    pub(super) fn serialize<S: Serializer>(
        delta: &TimeDelta,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Milliseconds, always, so the defaults layer round-trips through the parser above
        // rather than through a second format that has to agree with it.
        serializer.serialize_str(&format!("{}ms", delta.num_milliseconds()))
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(i64),
        Text(String),
    }
}

/// Reads a duration the way the configuration schema does. Exposed for the docs' sake, and
/// for the tests that pin the grammar.
///
/// # Errors
///
/// A message naming the input and the suffixes that are understood.
pub fn parse_duration(raw: &str) -> Result<TimeDelta, String> {
    duration::parse(raw)
}

/// The profile everything is read under. Named so it is not a bare string in two places.
pub const PROFILE: Profile = Profile::const_new("default");

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, parse_duration};
    use alertthread_core::{ChannelId, PolicyError};
    use alertthread_slack::TemplateKind;
    use chrono::TimeDelta;
    use figment::Figment;
    use figment::providers::{Format, Serialized, Yaml};

    /// A configuration that starts: the two things ADR 001 D8 and D11 require.
    const MINIMAL: &str = r##"
slack:
  token: "xoxb-test-token"
  default_channel: "#alerts"
"##;

    fn figment(yaml: &str) -> Figment {
        Figment::from(Serialized::defaults(Config::default())).merge(Yaml::string(yaml))
    }

    fn load(yaml: &str) -> Result<Config, ConfigError> {
        Config::from_figment(&figment(yaml))
    }

    fn minimal() -> Config {
        load(MINIMAL).expect("the minimal configuration starts")
    }

    #[test]
    fn the_defaults_are_the_ones_adr_001_specifies() {
        let config = minimal();
        assert_eq!(config.storage.backend, "sqlite");
        assert_eq!(config.collapse.threshold, 5);
        assert_eq!(config.collapse.refresh_debounce, TimeDelta::seconds(60));
        assert!(config.resolve.update_in_place);
        assert!(config.resolve.thread_reply);
        assert_eq!(config.storage.retention.resolved, TimeDelta::days(7));
        assert_eq!(config.storage.retention.stale, TimeDelta::days(30));
        assert_eq!(config.worker.max_attempts, 10);
        assert_eq!(config.slack.auth_probe_interval, TimeDelta::minutes(15));
        assert_eq!(config.server.listen.port(), 8080);
    }

    #[test]
    fn debug_never_shows_the_bot_token() {
        // The reason `SlackToken` lives in the Slack crate rather than being a String here:
        // the redaction is inherited by every struct that embeds one. A token in a startup
        // log line is a burned token (AGENTS.md).
        let config = load(
            r##"
slack:
  token: "xoxb-1234-abcdefghijklmnop"
  default_channel: "#alerts"
"##,
        )
        .expect("configuration loads");

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("xoxb-1234"), "{rendered}");
        assert!(!rendered.contains("abcdefghijklmnop"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // And it is still there for the one caller that needs it.
        assert_eq!(
            config.token().expect("a token was configured").expose(),
            "xoxb-1234-abcdefghijklmnop"
        );
    }

    #[test]
    fn the_token_is_never_written_into_the_defaults_layer() {
        // `figment` quotes the values a key came from in its error messages. A token in the
        // defaults provider is a token in a startup error.
        let serialised =
            serde_json::to_string(&minimal()).expect("configuration serialises for figment");
        assert!(!serialised.contains("xoxb"), "{serialised}");
    }

    #[test]
    fn a_relay_with_no_token_refuses_to_start() {
        let error = load("slack:\n  default_channel: \"#alerts\"\n")
            .expect_err("there is no degraded mode without a token");
        assert!(matches!(error, ConfigError::NoToken), "{error:?}");
        assert!(
            error.to_string().contains("ALERTTHREAD_SLACK__TOKEN"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_token_is_no_token() {
        // An unset environment variable that a chart still renders arrives as "".
        let error = load("slack:\n  token: \"   \"\n  default_channel: \"#alerts\"\n")
            .expect_err("whitespace is not a token");
        assert!(matches!(error, ConfigError::NoToken), "{error:?}");
    }

    #[test]
    fn a_relay_with_no_default_channel_refuses_to_start() {
        // ADR 001 D8's third step, taken at startup. Deferring it to the first webhook
        // would mean the relay had already acknowledged an alert it cannot place.
        let error = load("slack:\n  token: \"xoxb-test\"\n")
            .expect_err("a relay with nowhere to post cannot start");
        assert!(matches!(error, ConfigError::NoDefaultChannel), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("D8"), "{message}");
        assert!(message.contains("slack.default_channel"), "{message}");
    }

    #[test]
    fn an_empty_default_channel_is_no_default_channel() {
        let error = load("slack:\n  token: \"xoxb-test\"\n  default_channel: \"\"\n")
            .expect_err("an empty channel is not a channel");
        assert!(matches!(error, ConfigError::NoDefaultChannel), "{error:?}");
    }

    #[test]
    fn the_default_channel_is_handed_over_as_a_newtype_with_its_whitespace_trimmed() {
        let config = load("slack:\n  token: \"xoxb-test\"\n  default_channel: \" #alerts \"\n")
            .expect("configuration loads");
        assert_eq!(config.default_channel(), Some(ChannelId::new("#alerts")));
    }

    #[test]
    fn disabling_both_resolve_behaviours_refuses_to_start() {
        // ADR 001 D6, enforced by `Policy::validate` rather than re-implemented here: a
        // resolve that does nothing is indistinguishable from the bug this project fixes.
        let error = load(&format!(
            "{MINIMAL}\nresolve:\n  update_in_place: false\n  thread_reply: false\n"
        ))
        .expect_err("a resolve that does nothing is a configuration error");
        assert!(
            matches!(error, ConfigError::Policy(PolicyError::ResolveDoesNothing)),
            "{error:?}"
        );
    }

    #[test]
    fn a_negative_refresh_debounce_refuses_to_start() {
        let error = load(&format!(
            "{MINIMAL}\ncollapse:\n  refresh_debounce: \"-1s\"\n"
        ))
        .expect_err("a negative debounce refreshes on every retried delivery");
        assert!(
            matches!(
                error,
                ConfigError::Policy(PolicyError::NegativeRefreshDebounce)
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_storage_backend_that_is_not_one_refuses_to_start_by_name() {
        let error = load(&format!("{MINIMAL}\nstorage:\n  backend: postgresql\n"))
            .expect_err("only two values are backends");
        assert!(error.to_string().contains("postgresql"), "{error}");
    }

    #[test]
    fn a_key_nobody_recognises_is_rejected_rather_than_ignored() {
        // The opposite of the webhook payload's tolerance, and for the opposite reason:
        // a misspelled config key is a setting an operator believes is in effect. Silently
        // ignoring `collapse.treshold` would leave them watching a channel and drawing the
        // wrong conclusion about why it behaves as it does.
        let error = load(&format!("{MINIMAL}\ncollapse:\n  treshold: 9\n"))
            .expect_err("an unrecognised key must be named");
        assert!(error.to_string().contains("treshold"), "{error}");
    }

    #[test]
    fn the_policy_the_planner_sees_is_the_configured_one() {
        let config = load(&format!(
            "{MINIMAL}\ncollapse:\n  threshold: 0\n  refresh_debounce: 12h\nresolve:\n  thread_reply: false\n"
        ))
        .expect("configuration loads");

        let policy = config.policy();
        assert_eq!(policy.collapse_threshold, 0);
        assert_eq!(policy.refresh_debounce, TimeDelta::hours(12));
        assert!(policy.resolve_update_in_place);
        assert!(!policy.resolve_thread_reply);
    }

    #[test]
    fn the_retention_policy_the_pruner_sees_is_the_configured_one() {
        let config = load(&format!(
            "{MINIMAL}\nstorage:\n  retention:\n    resolved: 2d\n    stale: 14d\n"
        ))
        .expect("configuration loads");

        let policy = config.storage.retention.policy();
        assert_eq!(policy.resolved_after, TimeDelta::days(2));
        assert_eq!(policy.stale_after, TimeDelta::days(14));
    }

    #[test]
    fn durations_read_the_way_an_operator_writes_them() {
        assert_eq!(parse_duration("7d"), Ok(TimeDelta::days(7)));
        assert_eq!(parse_duration("30s"), Ok(TimeDelta::seconds(30)));
        assert_eq!(parse_duration("250ms"), Ok(TimeDelta::milliseconds(250)));
        assert_eq!(parse_duration("15m"), Ok(TimeDelta::minutes(15)));
        assert_eq!(parse_duration("1h"), Ok(TimeDelta::hours(1)));
        assert_eq!(
            parse_duration("1h30m"),
            Ok(TimeDelta::hours(1) + TimeDelta::minutes(30))
        );
        assert_eq!(
            parse_duration("1d2h3m4s"),
            Ok(TimeDelta::days(1)
                + TimeDelta::hours(2)
                + TimeDelta::minutes(3)
                + TimeDelta::seconds(4))
        );
        assert_eq!(parse_duration(" 90 "), Ok(TimeDelta::seconds(90)));
    }

    #[test]
    fn a_bare_number_is_seconds() {
        // Somebody who writes `timeout: 15` in a YAML file means fifteen seconds, and
        // reading it as fifteen milliseconds would produce a relay that times out every
        // Slack call and looks like a network problem.
        assert_eq!(parse_duration("15"), Ok(TimeDelta::seconds(15)));
        let config = load(
            r##"
slack:
  token: "xoxb-test-token"
  default_channel: "#alerts"
  timeout: 20
"##,
        )
        .expect("a bare number is a duration");
        assert_eq!(config.slack.timeout, TimeDelta::seconds(20));
    }

    #[test]
    fn a_duration_that_is_not_one_says_what_it_understands() {
        // This message reaches an operator whose container has failed to start.
        for bad in ["", "   ", "soon", "10 weeks", "d", "5x", "-"] {
            let error = parse_duration(bad).expect_err(bad);
            assert!(!error.is_empty(), "{bad}");
        }
        let error = parse_duration("5w").expect_err("weeks are not a unit here");
        assert!(error.contains("ms, s, m, h, d"), "{error}");
    }

    #[test]
    fn a_duration_longer_than_a_duration_is_rejected_rather_than_panicking() {
        // `TimeDelta::days(i64::MAX)` panics, and this workspace denies `panic` for good
        // reason: a typo in a ConfigMap must not be able to abort the process.
        let error = parse_duration("999999999999999999d").expect_err("that is not a duration");
        assert!(error.contains("longer than"), "{error}");
    }

    #[test]
    fn a_duration_round_trips_through_the_defaults_layer() {
        // The defaults are serialised into figment and parsed back out, so the writer and
        // the reader have to agree. They did not, once.
        let config = minimal();
        assert_eq!(config.worker.idle_poll, TimeDelta::milliseconds(250));
        assert_eq!(config.server.request_timeout, TimeDelta::seconds(15));
    }

    #[test]
    fn a_token_file_is_read_and_its_trailing_newline_removed() {
        // `kubectl create secret --from-file` keeps the newline, and the error it produces
        // downstream names HTTP headers rather than tokens.
        let dir = std::env::temp_dir().join(format!("alertthread-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("token");
        std::fs::write(&path, "xoxb-from-a-file\n").expect("writing the token");

        let config = load(&format!(
            "slack:\n  default_channel: \"#alerts\"\n  token_file: {}\n",
            path.display()
        ))
        .expect("a token file is a token, on its own");

        assert_eq!(
            config.token().expect("a token was read").expose(),
            "xoxb-from-a-file"
        );
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_token_file_that_is_not_there_names_the_path() {
        let error = load(
            r##"
slack:
  token: "xoxb-test-token"
  default_channel: "#alerts"
  token_file: /nonexistent/alertthread/token
"##,
        )
        .expect_err("a configured token file has to exist");
        assert!(
            matches!(error, ConfigError::UnreadableToken { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("/nonexistent/"), "{error}");
    }

    #[test]
    fn the_webhook_is_unauthenticated_unless_a_token_is_configured() {
        // ADR 001 D11 calls the bearer token optional, and off is the default: a relay that
        // suddenly required a credential on upgrade would 401 every delivery from an
        // Alertmanager nobody had reconfigured yet.
        assert!(minimal().webhook_auth().is_open());
        assert!(minimal().webhook_auth().token().is_none());
    }

    #[test]
    fn a_configured_token_closes_the_webhook() {
        let config = load(&format!(
            "{MINIMAL}\nserver:\n  auth_token: \"s3cret-webhook-token\"\n"
        ))
        .expect("configuration loads");
        let auth = config.webhook_auth();
        assert!(!auth.is_open());
        assert!(
            auth.token()
                .expect("a token was configured")
                .matches("s3cret-webhook-token")
        );
    }

    #[test]
    fn a_blank_webhook_token_leaves_the_webhook_open_rather_than_refusing_to_start() {
        // A chart that renders an unset value produces `""`. Refusing to start over an
        // optional security setting would be silence; the startup log says which mode is in
        // effect, which is what stops it being quiet — see `run::start`.
        for blank in ["\"\"", "\"   \""] {
            let config = load(&format!("{MINIMAL}\nserver:\n  auth_token: {blank}\n"))
                .expect("a blank token is not fatal");
            assert!(
                matches!(config.webhook_auth(), crate::auth::WebhookAuth::Blank),
                "{blank}"
            );
            assert!(config.webhook_auth().is_open());
        }
    }

    #[test]
    fn debug_never_shows_the_webhook_token() {
        // The `Config` is logged at startup in full. Same property as the bot token, and it
        // has to hold for the same reason.
        let config = load(&format!(
            "{MINIMAL}\nserver:\n  auth_token: \"s3cret-webhook-token\"\n"
        ))
        .expect("configuration loads");

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        // And it is not written into the defaults layer figment quotes in its errors.
        let serialised =
            serde_json::to_string(&config).expect("configuration serialises for figment");
        assert!(!serialised.contains("s3cret"), "{serialised}");
    }

    #[test]
    fn a_webhook_token_file_is_read_and_its_trailing_newline_removed() {
        // `kubectl create secret --from-file` keeps the newline, and a token with a newline
        // in it never matches the header Alertmanager sends.
        let dir = std::env::temp_dir().join(format!("alertthread-wh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("webhook-token");
        std::fs::write(&path, "from-a-file\n").expect("writing the token");

        let config = load(&format!(
            "{MINIMAL}\nserver:\n  auth_token: \"overridden\"\n  auth_token_file: {}\n",
            path.display()
        ))
        .expect("configuration loads");

        let auth = config.webhook_auth();
        let token = auth.token().expect("the file is a token");
        assert!(token.matches("from-a-file"), "the newline has to go");
        assert!(
            !token.matches("overridden"),
            "the mount is the more specific answer and wins"
        );
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_webhook_token_file_that_is_not_there_names_the_path() {
        // Fatal, unlike a blank inline value: the operator named a mount that is not there,
        // and serving an unauthenticated webhook because a secret failed to mount is the one
        // outcome they would never find out about.
        let error = load(&format!(
            "{MINIMAL}\nserver:\n  auth_token_file: /nonexistent/alertthread/webhook-token\n"
        ))
        .expect_err("a configured token file has to exist");
        assert!(
            matches!(error, ConfigError::UnreadableWebhookToken { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("/nonexistent/"), "{error}");
    }

    #[test]
    fn no_template_directory_means_no_overrides() {
        let (overrides, skipped) = minimal().templates().expect("no directory is not an error");
        assert!(overrides.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn templates_are_read_by_name_and_unknown_files_are_reported_not_fatal() {
        // A ConfigMap mount brings `..data` and dotted symlinks with it. A relay that
        // refused to start over one could not run in the place it is designed for.
        let dir = std::env::temp_dir().join(format!("alertthread-tpl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("firing.j2"), "custom firing").expect("writing a template");
        std::fs::write(dir.join("..data"), "not a template").expect("writing a decoy");

        let config = load(&format!(
            "{MINIMAL}\ntemplates:\n  dir: {}\n",
            dir.display()
        ))
        .expect("configuration loads");
        let (overrides, skipped) = config.templates().expect("the directory is readable");

        assert_eq!(
            overrides.get(&TemplateKind::Firing).map(String::as_str),
            Some("custom firing")
        );
        assert_eq!(skipped, vec!["..data".to_owned()]);
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_template_directory_that_is_not_there_names_the_path() {
        let config = load(&format!(
            "{MINIMAL}\ntemplates:\n  dir: /nonexistent/alertthread/templates\n"
        ))
        .expect("configuration loads");
        let error = config
            .templates()
            .expect_err("a configured directory has to exist");
        assert!(
            matches!(error, ConfigError::UnreadableTemplates { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_environment_layers_over_the_file() {
        // The layering ADR 001 D1 describes, without touching the process environment —
        // which is global, and would make this test depend on the order it ran in.
        let figment = super::Config::figment(None);
        assert!(figment.metadata().count() >= 2, "defaults plus environment");

        let config = Config::from_figment(&figment.merge(Yaml::string(MINIMAL)))
            .expect("configuration loads");
        assert_eq!(config.default_channel(), Some(ChannelId::new("#alerts")));
    }

    #[test]
    fn a_missing_configuration_file_is_not_an_error() {
        // Configuring a container purely through environment variables is the normal case,
        // and requiring a file for it would mean shipping an empty one in the image.
        let figment = Config::figment(Some(std::path::Path::new("/nonexistent/alertthread.yaml")))
            .merge(Yaml::string(MINIMAL));
        assert!(Config::from_figment(&figment).is_ok());
    }

    #[test]
    fn config_is_cloneable_so_the_worker_and_the_handlers_can_each_hold_one() {
        let config = minimal();
        let copy = config.clone();
        assert_eq!(copy.slack.base_url, config.slack.base_url);
        assert!(format!("{:?}", config.server).contains("listen"));
        assert!(format!("{:?}", config.worker).contains("max_attempts"));
        assert!(format!("{:?}", config.storage).contains("sqlite"));
        assert!(format!("{:?}", config.collapse).contains("threshold"));
        assert!(format!("{:?}", config.templates).contains("dir"));
        assert!(format!("{:?}", config.resolve).contains("thread_reply"));
    }
}
