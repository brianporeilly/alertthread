//! A relay wired to a real store, a real HTTP server, and a fake Slack.
//!
//! Nothing here is a mock of this project's own code. The store is the one that ships, the
//! router is the one `main` serves, and Slack is `wiremock` — which means a test that passes
//! here has exercised the SQL, the axum extractors and the JSON on the wire. A handler
//! tested through a hand-rolled fake router is a test of the fake.
//!
//! Time is the one thing that is not real. Every entry point takes `now`, so the tests below
//! advance a clock by hand instead of sleeping: the per-channel rate limiter is one message
//! per second, and a suite that waited for it would take a minute to deliver a storm.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code,
    unreachable_pub,
    reason = "test support. clippy.toml turns these back on inside `#[test]` functions, but \
              this module is called from outside one, where clippy cannot see the context — \
              so the same policy is stated here. `dead_code` and `unreachable_pub` because \
              cargo compiles this module once per integration-test binary and each uses a \
              different subset of it."
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alertthread::config::Config;
use alertthread::delivery::Backoff;
use alertthread::http::{AppState, router};
use alertthread::metrics::Metrics;
use alertthread::ratelimit::SlackLimits;
use alertthread::worker::{Pass, Worker};
use alertthread_slack::{Renderer, SlackClient, SlackToken};
use alertthread_store::{Backend, StateStore, Store, WorkerId};
use chrono::{DateTime, TimeDelta, Utc};
use figment::Figment;
use figment::providers::{Format, Serialized, Yaml};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The channel every test posts to unless it says otherwise.
pub const CHANNEL: &str = "#alerts";

/// A fixed instant, so a test reads as a timeline rather than as arithmetic.
#[must_use]
pub fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_784_642_520, 0).expect("timestamp is in range")
}

/// Names a database file under the directory cargo gives integration tests, having first
/// removed anything a previous run left there.
///
/// The removal is load-bearing rather than tidiness: `CARGO_TARGET_TMPDIR` survives between
/// runs and sqlx checksums migrations, so a database migrated by an earlier run fails
/// `migrate()` with `VersionMismatch` the moment a migration is legitimately edited.
#[must_use]
pub fn sqlite_url(name: &str) -> String {
    let path = format!("{}/{name}.sqlite", env!("CARGO_TARGET_TMPDIR"));
    for suffix in ["", "-wal", "-shm"] {
        match std::fs::remove_file(format!("{path}{suffix}")) {
            Ok(()) => {}
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::NotFound,
                "could not clear a previous run's SQLite database at {path}{suffix}"
            ),
        }
    }
    format!("sqlite://{path}")
}

/// Answers `chat.postMessage` with a fresh timestamp every time.
///
/// A fixed `ts` would make a storm-collapse parent and its children indistinguishable, and
/// the whole point of the threading tests is which timestamp ended up where.
struct IncrementingTs {
    next: AtomicU64,
}

impl Respond for IncrementingTs {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C0123456789",
            "ts": format!("1784642520.{n:06}"),
        }))
    }
}

/// A Slack that accepts everything.
pub async fn slack_that_works() -> MockServer {
    let server = MockServer::start().await;
    mount_auth(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(IncrementingTs {
            next: AtomicU64::new(1),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    server
}

/// A Slack with no handlers mounted beyond `auth.test`, for tests that mount their own.
pub async fn slack_with_auth_only() -> MockServer {
    let server = MockServer::start().await;
    mount_auth(&server).await;
    server
}

async fn mount_auth(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "team": "acme",
            "team_id": "T0123",
            "user": "alertthread",
            "user_id": "U0123",
            "bot_id": "B0123",
        })))
        .mount(server)
        .await;
}

/// The relay's `ok: false` shape: HTTP 200 with the failure in the body.
#[must_use]
pub fn slack_error(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": false, "error": code }))
}

/// A relay assembled from real parts.
pub struct Harness {
    pub store: Arc<Store>,
    pub slack: Arc<SlackClient>,
    pub metrics: Arc<Metrics>,
    pub worker: Worker<Store>,
    pub state: Arc<AppState<Store>>,
    pub config: Config,
}

impl Harness {
    /// Builds a relay against `slack`, storing state in a database named after the test.
    #[must_use = "the harness owns the store; dropping it closes the pool"]
    pub async fn new(name: &str, slack: &MockServer) -> Self {
        Self::with_config(name, slack, "").await
    }

    /// The same, with extra YAML layered over the defaults.
    pub async fn with_config(name: &str, slack: &MockServer, extra: &str) -> Self {
        let base = format!(
            "slack:\n  token: \"xoxb-test\"\n  default_channel: \"{CHANNEL}\"\n  \
             base_url: {}/api/\nstorage:\n  url: {}\n",
            slack.uri(),
            sqlite_url(name),
        );
        let config = Config::from_figment(
            &Figment::from(Serialized::defaults(Config::default()))
                .merge(Yaml::string(&base))
                .merge(Yaml::string(extra)),
        )
        .expect("the harness configuration is valid");

        let store = Arc::new(
            Store::connect(Backend::Sqlite, &config.storage.url)
                .await
                .expect("opening the test store"),
        );
        store.migrate().await.expect("applying migrations/sqlite");

        let client = Arc::new(
            SlackClient::builder(SlackToken::new("xoxb-test"))
                .base_url(config.slack.base_url.clone())
                .build()
                .expect("building the Slack client"),
        );
        let metrics = Arc::new(Metrics::new());
        let worker = Worker::new(
            Arc::clone(&store),
            Arc::clone(&client),
            Arc::new(Renderer::builtin()),
            Arc::new(SlackLimits::new(config.slack.rate_limit_divisor)),
            Arc::clone(&metrics),
            config.worker,
            WorkerId::new("test-worker"),
        );
        let state = Arc::new(AppState::new(
            Arc::clone(&store),
            Arc::clone(&metrics),
            &config,
        ));

        Self {
            store,
            slack: client,
            metrics,
            worker,
            state,
            config,
        }
    }

    /// The backoff policy the worker was built with.
    #[must_use]
    pub fn backoff(&self) -> Backoff {
        Backoff {
            max_attempts: self.config.worker.max_attempts,
            base: self.config.worker.backoff_base,
            max: self.config.worker.backoff_max,
        }
    }

    /// Runs the outbox until it stops making progress, advancing the clock a second per
    /// pass so the per-channel rate limiter lets one message through each time.
    ///
    /// Returns the totals. Bounded, because a bug that makes an op defer for ever should
    /// fail a test rather than hang CI.
    pub async fn drain_from(&self, start: DateTime<Utc>, passes: usize) -> Pass {
        let mut total = Pass::default();
        for step in 0..passes {
            let now = start + TimeDelta::seconds(i64::try_from(step).unwrap_or(0));
            let pass = self
                .worker
                .run_once(now)
                .await
                .expect("the worker leases against a healthy store");
            if pass.is_idle() {
                break;
            }
            total.leased += pass.leased;
            total.completed += pass.completed;
            total.deferred += pass.deferred;
            total.dead_lettered += pass.dead_lettered;
        }
        total
    }

    /// The whole exposition, for asserting on a metric.
    #[must_use]
    pub fn metrics_text(&self) -> String {
        self.metrics.render().expect("the registry encodes")
    }

    /// Whether the exposition contains a line.
    pub fn assert_metric(&self, line: &str) {
        let text = self.metrics_text();
        assert!(text.contains(line), "expected {line:?} in:\n{text}");
    }

    /// Serves the router on an ephemeral port and returns its base URL.
    ///
    /// A real socket rather than `tower::ServiceExt::oneshot`, because the extractors, the
    /// status codes and the graceful-shutdown path are all things a direct service call
    /// would skip — and they are exactly what these tests are about.
    pub async fn serve(&self) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral port");
        let addr = listener.local_addr().expect("the socket has an address");
        let (source, token) = alertthread::shutdown::cancellation();
        let app = router(Arc::clone(&self.state));

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { token.cancelled().await })
                .await
                .expect("the server runs");
        });

        Server {
            base: format!("http://{addr}"),
            source,
            handle,
        }
    }
}

/// A running HTTP server, stopped when it is dropped out of scope by `stop`.
pub struct Server {
    pub base: String,
    source: alertthread::shutdown::CancelSource,
    handle: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Signals shutdown and waits for the server to finish.
    pub async fn stop(self) {
        self.source.cancel();
        self.handle.await.expect("the server shuts down cleanly");
    }
}

/// One Alertmanager webhook body.
#[must_use]
pub fn payload(status: &str, alerts: &[serde_json::Value]) -> String {
    serde_json::json!({
        "version": "4",
        "groupKey": "{}:{alertname=\"CephOSDDown\"}",
        "truncatedAlerts": 0,
        "status": status,
        "receiver": "alertthread",
        "groupLabels": { "alertname": "CephOSDDown" },
        "commonLabels": { "alertname": "CephOSDDown", "severity": "critical" },
        "commonAnnotations": {},
        "externalURL": "http://alertmanager",
        "alerts": alerts,
    })
    .to_string()
}

/// One alert inside a webhook body.
#[must_use]
pub fn alert(fingerprint: &str, status: &str) -> serde_json::Value {
    let ends_at = if status == "resolved" {
        "2026-07-21T14:31:00Z"
    } else {
        "0001-01-01T00:00:00Z"
    };
    serde_json::json!({
        "status": status,
        "labels": {
            "alertname": "CephOSDDown",
            "severity": "critical",
            "instance": fingerprint,
        },
        "annotations": { "summary": format!("osd {fingerprint} is down") },
        "startsAt": "2026-07-21T14:02:00Z",
        "endsAt": ends_at,
        "generatorURL": "http://prometheus/graph",
        "fingerprint": fingerprint,
    })
}

/// A store that answers the readiness probe slowly, and everything else normally.
///
/// Exists because `server.request_timeout` protects against *a store that has stopped
/// answering*, and that is not a state the shipping store can be argued into: an unmigrated
/// database fails fast, and a broken socket fails faster. Neither of them holds the request
/// open, which is the only condition the timeout layer reacts to.
///
/// It delegates every method to a real [`Store`] and stalls only in
/// [`alert`](StateStore::alert), which is what `/readyz` awaits. A wrapper rather than a
/// hand-written fake so that the test still exercises the shipping SQL underneath the delay
/// — the point is a slow store, not an absent one.
pub struct SlowStore {
    inner: Store,
    delay: std::time::Duration,
}

impl SlowStore {
    #[must_use]
    pub fn new(inner: Store, delay: std::time::Duration) -> Self {
        Self { inner, delay }
    }
}

impl StateStore for SlowStore {
    async fn migrate(&self) -> Result<(), alertthread_store::StoreError> {
        self.inner.migrate().await
    }

    async fn ingest<F>(
        &self,
        batch: &alertthread_core::AlertBatch,
        now: DateTime<Utc>,
        decide: F,
    ) -> Result<alertthread_core::Plan, alertthread_store::StoreError>
    where
        F: FnOnce(
                &[alertthread_core::ClaimOutcome],
                Option<&alertthread_core::GroupState>,
            ) -> alertthread_core::Plan
            + Send,
    {
        self.inner.ingest(batch, now, decide).await
    }

    async fn lease_batch(
        &self,
        worker: &WorkerId,
        limit: u32,
        lease: TimeDelta,
        now: DateTime<Utc>,
    ) -> Result<Vec<alertthread_store::LeasedOp>, alertthread_store::StoreError> {
        self.inner.lease_batch(worker, limit, lease, now).await
    }

    async fn complete(
        &self,
        id: alertthread_store::OpId,
        effect: &alertthread_store::OpEffect,
        now: DateTime<Utc>,
    ) -> Result<(), alertthread_store::StoreError> {
        self.inner.complete(id, effect, now).await
    }

    async fn defer(
        &self,
        id: alertthread_store::OpId,
        deferral: &alertthread_store::Deferral,
    ) -> Result<(), alertthread_store::StoreError> {
        self.inner.defer(id, deferral).await
    }

    async fn dead_letter(
        &self,
        id: alertthread_store::OpId,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), alertthread_store::StoreError> {
        self.inner.dead_letter(id, reason, now).await
    }

    async fn dead_letters(
        &self,
        scope: &alertthread_store::DeadLetterScope,
        limit: u32,
    ) -> Result<Vec<alertthread_store::DeadLetter>, alertthread_store::StoreError> {
        self.inner.dead_letters(scope, limit).await
    }

    async fn revive_dead_letters(
        &self,
        scope: &alertthread_store::DeadLetterScope,
        now: DateTime<Utc>,
    ) -> Result<u64, alertthread_store::StoreError> {
        self.inner.revive_dead_letters(scope, now).await
    }

    async fn prune(
        &self,
        policy: &alertthread_store::RetentionPolicy,
        now: DateTime<Utc>,
    ) -> Result<alertthread_store::PruneStats, alertthread_store::StoreError> {
        self.inner.prune(policy, now).await
    }

    async fn alert(
        &self,
        fingerprint: &alertthread_core::Fingerprint,
        channel: &alertthread_core::ChannelId,
    ) -> Result<Option<alertthread_store::AlertRecord>, alertthread_store::StoreError> {
        // The stall, and the whole reason this type exists.
        tokio::time::sleep(self.delay).await;
        self.inner.alert(fingerprint, channel).await
    }

    async fn group(
        &self,
        group_key: &alertthread_core::GroupKey,
        channel: &alertthread_core::ChannelId,
    ) -> Result<Option<alertthread_store::GroupRecord>, alertthread_store::StoreError> {
        self.inner.group(group_key, channel).await
    }

    async fn group_membership(
        &self,
        group_key: &alertthread_core::GroupKey,
        channel: &alertthread_core::ChannelId,
    ) -> Result<alertthread_store::GroupMembership, alertthread_store::StoreError> {
        self.inner.group_membership(group_key, channel).await
    }

    async fn stats(&self) -> Result<alertthread_store::StoreStats, alertthread_store::StoreError> {
        self.inner.stats().await
    }

    async fn describe_table(
        &self,
        table: &str,
    ) -> Result<Vec<alertthread_store::ColumnDef>, alertthread_store::StoreError> {
        self.inner.describe_table(table).await
    }
}
