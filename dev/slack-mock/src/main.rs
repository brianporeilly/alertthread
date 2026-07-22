//! A fake Slack for local development. **Not shipped, and excluded from the
//! coverage gate** — this is tooling, not product.
//!
//! Two faces on one store:
//!
//! - the **API** face at `/api/*`, matching the three methods
//!   `alertthread-slack` calls (`chat.postMessage`, `chat.update`, `auth.test`),
//!   answering every one with an HTTP 200 whose `ok` field carries success or
//!   failure — because that is the Slack behaviour the whole client is built
//!   around;
//! - the **browser** face at `/`, rendering channels, messages and threads so a
//!   red firing message turning green in place is something you can watch.
//!
//! `/api/state` returns the store as JSON, which is what the end-to-end CI job
//! asserts against — scraping HTML for a pass/fail signal is brittle.

mod api;
mod messages;
mod ui;

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use minijinja::Environment;
use tokio::sync::Mutex;

use crate::api::{dispatch, failure};
use crate::messages::Workspace;

/// Everything the handlers share.
struct AppState {
    workspace: Mutex<Workspace>,
    templates: Environment<'static>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match serve().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "slack-mock could not start");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Builds the store, binds the socket and serves until the process is stopped.
async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let templates = ui::environment()?;
    let state = Arc::new(AppState {
        workspace: Mutex::new(Workspace::default()),
        templates,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(state_json))
        .route("/api/{method}", post(api_call))
        .with_state(state);

    let listen = std::env::var("SLACK_MOCK_LISTEN").unwrap_or_else(|_| "0.0.0.0:8081".to_owned());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "slack-mock listening");

    axum::serve(listener, app).await?;
    Ok(())
}

/// `POST /api/{method}` — the Slack Web API face.
async fn api_call(
    State(state): State<Arc<AppState>>,
    Path(method): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let answer = {
        let mut workspace = state.workspace.lock().await;
        dispatch(&method, &body, &mut workspace, Utc::now())
    };
    let ok = answer.get("ok").and_then(serde_json::Value::as_bool);
    tracing::info!(%method, ?ok, "api call");
    Json(answer).into_response()
}

/// `GET /api/state` — the whole store as JSON, for the CI assertions.
async fn state_json(State(state): State<Arc<AppState>>) -> Response {
    let view = state.workspace.lock().await.view();
    Json(view).into_response()
}

/// `GET /` — the browser face.
async fn index(State(state): State<Arc<AppState>>) -> Response {
    let view = state.workspace.lock().await.view();
    match ui::render(&state.templates, &view) {
        Ok(html) => ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response(),
        Err(error) => {
            tracing::error!(%error, "rendering the UI failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(failure("render_failed")),
            )
                .into_response()
        }
    }
}
