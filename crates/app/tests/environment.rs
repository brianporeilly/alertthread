//! The relay, started against its own documented environment.
//!
//! Every variable this file sets is one `reference/configuration.md` tells an operator to
//! export, and until ROADMAP known open item 22 was fixed each of them stopped the relay
//! dead: `Config` denies unknown fields — deliberately, so a misspelled key is never a
//! setting somebody wrongly believes is in effect — and the `ALERTTHREAD_` environment layer
//! reads a name with no `__` in it as a *top-level* key. `ALERTTHREAD_LOG` arrived at the
//! deserialiser as `log`, which is not a field, and the process exited before
//! `init_tracing`'s reading of it could matter.
//!
//! # Why this spawns processes instead of calling `Config::load`
//!
//! A unit test that layers a `Figment` by hand cannot reach this bug, because the bug *is*
//! the process environment. `crates/app/src/config.rs` holds the cheap half — that the
//! reserved set covers every bare name the documentation carries — and it would have passed
//! throughout the outage. Nothing had ever started the binary with `ALERTTHREAD_LOG` set,
//! which is exactly why nothing caught it. So these tests run
//! `CARGO_BIN_EXE_alertthread` and ask it for a `200`.
//!
//! The environment is also global to a process, and `unsafe_code` is forbidden workspace-wide
//! while `std::env::set_var` is `unsafe`. A child process is the only way a test here can set
//! a variable at all, and it happens to be the honest one.
//!
//! # What each test pins
//!
//! - Every name in `RESERVED_ENV_VARS`, all set at once, and the relay serves.
//! - `ALERTTHREAD_CONFIG` names a file whose contents take effect — the code reading it was
//!   correct all along and simply never ran.
//! - `ALERTTHREAD_LOG` and `RUST_LOG` each raise the log level, and `ALERTTHREAD_LOG` wins.
//! - `ALERTTHREAD_LOG_FORMAT=json` produces JSON.
//! - A name that is *not* reserved is still fatal, and says which one it was.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code; see clippy.toml"
)]

mod harness;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use alertthread::config::{ENV_CONFIG, ENV_LOG, ENV_LOG_FORMAT, RESERVED_ENV_VARS};
use harness::{CHANNEL, slack_that_works, sqlite_url};
use wiremock::MockServer;

/// Every variable a test here might inherit from the developer's shell or from CI.
///
/// Cleared on every spawn before anything is set. A `RUST_LOG` left over from an outer
/// `cargo` invocation would make the control run in
/// `the_default_filter_logs_nothing_below_info` pass or fail for a reason that has nothing to
/// do with this code.
fn inherited_names() -> Vec<String> {
    RESERVED_ENV_VARS
        .iter()
        .map(|name| (*name).to_owned())
        .chain(std::iter::once("RUST_LOG".to_owned()))
        .collect()
}

/// A relay running as its own process, with its stdout on disk.
struct Process {
    child: Child,
    base: String,
    log: PathBuf,
}

impl Process {
    /// Starts the shipping binary with `env` layered over the minimum it needs.
    ///
    /// `listen` is `None` for the one test whose whole point is that the address comes from
    /// the file `ALERTTHREAD_CONFIG` names; every other test pins it so the port is known.
    ///
    /// stdout goes to a file rather than a pipe. At `ALERTTHREAD_LOG=debug` the relay emits
    /// enough that a pipe nobody is draining fills its buffer and blocks the process, which
    /// would look exactly like the startup failure these tests exist to detect.
    fn start(
        name: &str,
        slack: &MockServer,
        db: &str,
        listen: Option<&str>,
        env: &[(&str, &str)],
    ) -> Self {
        let log = PathBuf::from(format!("{}/{name}.log", env!("CARGO_TARGET_TMPDIR")));
        let out = std::fs::File::create(&log).expect("creating the log file");

        let mut command = Command::new(env!("CARGO_BIN_EXE_alertthread"));
        for inherited in inherited_names() {
            command.env_remove(inherited);
        }
        command
            .env("ALERTTHREAD_SLACK__TOKEN", "xoxb-test")
            .env("ALERTTHREAD_SLACK__DEFAULT_CHANNEL", CHANNEL)
            .env(
                "ALERTTHREAD_SLACK__BASE_URL",
                format!("{}/api/", slack.uri()),
            )
            // Nothing here is allowed to wait on a wall clock the test does not control.
            .env("ALERTTHREAD_SLACK__AUTH_STARTUP_GRACE", "0s")
            .env("ALERTTHREAD_SLACK__AUTH_PROBE_INTERVAL", "1h")
            .env("ALERTTHREAD_STORAGE__URL", db)
            .env("ALERTTHREAD_STORAGE__RETENTION__INTERVAL", "1h")
            .env("ALERTTHREAD_WORKER__IDLE_POLL", "1h")
            .env("ALERTTHREAD_WORKER__SAMPLE_INTERVAL", "1h");
        if let Some(addr) = listen {
            command.env("ALERTTHREAD_SERVER__LISTEN", addr);
        }
        for (key, value) in env {
            command.env(key, value);
        }

        let child = command
            .stdout(Stdio::from(out))
            .stderr(Stdio::null())
            .spawn()
            .expect("the relay binary starts");

        Self {
            child,
            base: String::new(),
            log,
        }
    }

    /// Polls `/healthz` at `addr` until it answers, or fails the test.
    ///
    /// Getting here at all is the assertion: on `main` the process was gone within
    /// milliseconds, having printed an unknown-field error naming the key `log`.
    async fn serving(mut self, addr: &str) -> Self {
        self.base = format!("http://{addr}");
        let base = self.base.clone();
        let up = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if reqwest::get(format!("{base}/healthz"))
                    .await
                    .is_ok_and(|response| response.status() == 200)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            up.is_ok(),
            "the relay never served at {}. Its output was:\n{}",
            self.base,
            self.output()
        );
        self
    }

    /// Everything the relay has written to stdout so far.
    fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Stops the relay and returns what it logged.
    fn stop(mut self) -> String {
        self.child.kill().expect("killing the relay");
        self.child.wait().expect("reaping the relay");
        self.output()
    }
}

/// An address nothing is listening on, for a child process to bind.
fn free_port() -> String {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    socket
        .local_addr()
        .expect("the socket has an address")
        .to_string()
}

/// Writes `yaml` under the integration-test temp directory and returns its path.
fn config_file(name: &str, yaml: &str) -> PathBuf {
    let path = PathBuf::from(format!("{}/{name}.yaml", env!("CARGO_TARGET_TMPDIR")));
    std::fs::write(&path, yaml).expect("writing the configuration file");
    path
}

fn as_str(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_owned()
}

#[tokio::test]
async fn the_relay_starts_with_every_reserved_variable_set() {
    // The reproduction from ROADMAP known open item 22, inverted into a gate. All three at
    // once rather than one per test, because the set is the thing being reserved: an `ignore`
    // list that dropped a name would fail here whichever name it dropped.
    let slack = slack_that_works().await;
    let db = sqlite_url("env-reserved");
    let addr = free_port();
    let file = config_file("env-reserved", "collapse:\n  threshold: 7\n");

    let path = as_str(&file);
    let env: [(&str, &str); 3] = [
        (ENV_CONFIG, &path),
        (ENV_LOG, "debug"),
        (ENV_LOG_FORMAT, "text"),
    ];
    // If a name is ever added to the reserved set without being given a value here, the set
    // and this test have drifted and the assertion below is weaker than it reads.
    assert_eq!(
        env.len(),
        RESERVED_ENV_VARS.len(),
        "RESERVED_ENV_VARS has {} names and this test sets {} — set them all",
        RESERVED_ENV_VARS.len(),
        env.len()
    );
    for (name, _) in &env {
        assert!(
            RESERVED_ENV_VARS.contains(name),
            "{name} is not in RESERVED_ENV_VARS"
        );
    }

    let relay = Process::start("env-reserved", &slack, &db, Some(&addr), &env)
        .serving(&addr)
        .await;
    relay.stop();
}

#[tokio::test]
async fn the_config_variable_names_a_file_that_is_actually_read() {
    // `run::config_path` was correct for three phases and never executed once, because the
    // figment layer rejected the variable before `main` reached it. Proving it is tolerated
    // is not enough — the file it names has to take effect.
    //
    // `server.listen` is the observable, and it comes only from the file: no
    // ALERTTHREAD_SERVER__LISTEN is set, so a relay answering on this port read the file.
    let slack = slack_that_works().await;
    let db = sqlite_url("env-config-file");
    let addr = free_port();
    let file = config_file(
        "env-config-file",
        &format!("server:\n  listen: \"{addr}\"\n"),
    );

    let relay = Process::start(
        "env-config-file",
        &slack,
        &db,
        None,
        &[(ENV_CONFIG, &as_str(&file))],
    )
    .serving(&addr)
    .await;
    relay.stop();
}

#[tokio::test]
async fn the_default_filter_logs_nothing_below_info() {
    // The control for the two tests below. Without it, "DEBUG appears when the variable is
    // set" would also pass on a build that logs at debug unconditionally.
    let slack = slack_that_works().await;
    let db = sqlite_url("env-log-default");
    let addr = free_port();

    let relay = Process::start("env-log-default", &slack, &db, Some(&addr), &[])
        .serving(&addr)
        .await;
    let output = relay.stop();

    assert!(
        output.contains("INFO"),
        "the relay logged nothing at all:\n{output}"
    );
    assert!(
        !output.contains("DEBUG"),
        "the default filter is not info:\n{output}"
    );
}

#[tokio::test]
async fn the_project_log_variable_raises_the_level() {
    let slack = slack_that_works().await;
    let db = sqlite_url("env-log");
    let addr = free_port();

    let relay = Process::start(
        "env-log",
        &slack,
        &db,
        Some(&addr),
        &[(ENV_LOG, "info,alertthread=debug")],
    )
    .serving(&addr)
    .await;
    let output = relay.stop();

    assert!(
        output.contains("DEBUG"),
        "{ENV_LOG} did not reach the subscriber:\n{output}"
    );
}

#[tokio::test]
async fn rust_log_raises_the_level_and_the_project_variable_beats_it() {
    // Two claims in one process pair. The first is `compose.yaml`'s: it has set
    // `RUST_LOG: "info,alertthread=debug"` on the relay since Phase 4 and it did nothing,
    // first because nothing read `RUST_LOG` and then because the variable that *was* read
    // stopped the process. The second is the precedence: a `RUST_LOG` inherited from a base
    // image must not override what a deployment asked for by name.
    let slack = slack_that_works().await;
    let db = sqlite_url("env-rust-log");
    let addr = free_port();

    let relay = Process::start(
        "env-rust-log",
        &slack,
        &db,
        Some(&addr),
        &[("RUST_LOG", "info,alertthread=debug")],
    )
    .serving(&addr)
    .await;
    let output = relay.stop();
    assert!(
        output.contains("DEBUG"),
        "RUST_LOG did not reach the subscriber:\n{output}"
    );

    let db = sqlite_url("env-rust-log-loses");
    let addr = free_port();
    let relay = Process::start(
        "env-rust-log-loses",
        &slack,
        &db,
        Some(&addr),
        &[(ENV_LOG, "warn"), ("RUST_LOG", "debug")],
    )
    .serving(&addr)
    .await;
    let output = relay.stop();
    assert!(
        !output.contains("DEBUG"),
        "RUST_LOG overrode {ENV_LOG}:\n{output}"
    );
}

#[tokio::test]
async fn the_log_format_variable_produces_json() {
    let slack = slack_that_works().await;
    let db = sqlite_url("env-log-format");
    let addr = free_port();

    let relay = Process::start(
        "env-log-format",
        &slack,
        &db,
        Some(&addr),
        &[(ENV_LOG_FORMAT, "json")],
    )
    .serving(&addr)
    .await;
    let output = relay.stop();

    let first = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(first)
        .unwrap_or_else(|error| panic!("the first log line is not JSON ({error}): {first}"));
    assert!(
        parsed.get("fields").is_some(),
        "a tracing JSON event has a `fields` object: {first}"
    );
}

#[tokio::test]
async fn a_bare_variable_that_is_not_reserved_still_refuses_to_start() {
    // The other half of the fix, and the reason `deny_unknown_fields` was not simply dropped.
    // `ALERTTHREAD_SLACK__TOKNE` is the case that matters — a token an operator believes is
    // set — and it stays fatal. The reserved list buys three names an exemption; it does not
    // make the environment layer permissive.
    let slack = slack_that_works().await;
    let db = sqlite_url("env-unknown");
    let addr = free_port();

    let mut command = Command::new(env!("CARGO_BIN_EXE_alertthread"));
    for inherited in inherited_names() {
        command.env_remove(inherited);
    }
    let output = command
        .env("ALERTTHREAD_SLACK__TOKEN", "xoxb-test")
        .env("ALERTTHREAD_SLACK__DEFAULT_CHANNEL", CHANNEL)
        .env(
            "ALERTTHREAD_SLACK__BASE_URL",
            format!("{}/api/", slack.uri()),
        )
        .env("ALERTTHREAD_STORAGE__URL", db)
        .env("ALERTTHREAD_SERVER__LISTEN", addr)
        .env("ALERTTHREAD_TOKNE", "xoxb-not-a-key")
        .output()
        .expect("the relay binary runs");

    assert!(
        !output.status.success(),
        "an unrecognised bare variable has to be fatal: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tokne"),
        "the failure has to name the variable that caused it:\n{stderr}"
    );
}
