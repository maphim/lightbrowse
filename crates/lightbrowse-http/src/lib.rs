//! HTTP/REST API for lightbrowse.
//!
//! Useful for scripting, and for hosting the browser as a microservice that
//! any agent (or human) can call:
//!
//! ```text
//! GET /health
//! GET /v1/page?url=...            page summary + text preview
//! GET /v1/extract?url=...&mode=text|links|forms|meta|headings
//! GET /v1/snapshot?url=...&max_nodes=400
//! GET /v1/search?q=...&max_results=8
//! ```

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::{Config, Engine};
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::service;
use lightbrowse_core::session::Session;
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn BrowserBackend>,
    pub cdp: Option<Arc<dyn BrowserBackend>>,
    pub session: Arc<Mutex<Session>>,
    pub engine: Engine,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/page", get(page))
        .route("/v1/extract", get(extract))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/search", get(search))
        .with_state(state)
}

/// Bind and serve forever.
pub async fn serve(addr: &str, state: AppState) -> lightbrowse_core::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(lightbrowse_core::Error::Io)?;
    tracing::info!("lightbrowse HTTP API listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(lightbrowse_core::Error::Io)
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "lightbrowse",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": state.engine,
        "headless": !state.config.ui,
        "memory_budget_mb": state.config.memory_budget_mb,
        "idle_timeout_secs": state.config.idle_timeout_secs,
    }))
}

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
    engine: Option<String>,
}

async fn nav_page(
    state: &AppState,
    url: &str,
    engine: Engine,
) -> Result<lightbrowse_core::Page, ApiError> {
    // Clone out of the lock: MutexGuard is !Send and must not cross .await.
    let session = state
        .session
        .lock()
        .map_err(|_| ApiError::internal("session lock poisoned"))?
        .clone();
    service::navigate(&*state.backend, state.cdp.as_deref(), &session, url, engine)
        .await
        .map_err(ApiError::from)
}

fn parse_engine(s: Option<&str>, default: Engine) -> Result<Engine, ApiError> {
    match s {
        None => Ok(default),
        Some(v) => Engine::parse(v).ok_or_else(|| {
            ApiError::bad_request(format!("invalid engine '{v}' (expected auto|fetch|cdp)"))
        }),
    }
}

async fn page(
    State(state): State<AppState>,
    Query(q): Query<UrlQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine).await?;
    let t = extract::extract_text(&p.html);
    let body = json!({
        "url": p.url,
        "title": p.title,
        "status": p.status,
        "mime": p.mime,
        "truncated": p.truncated,
        "body_bytes": p.body_len(),
        "word_count": t.word_count,
        "reading_time_secs": t.reading_time_secs,
        "text_preview": t.text.chars().take(4000).collect::<String>(),
    });
    Ok(Json(body).into_response())
}

#[derive(Deserialize)]
struct ExtractQuery {
    url: String,
    mode: Option<String>,
    engine: Option<String>,
}

async fn extract(
    State(state): State<AppState>,
    Query(q): Query<ExtractQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine).await?;
    let mode = match q.mode.as_deref().unwrap_or("text") {
        "text" => ExtractMode::Text,
        "links" => ExtractMode::Links,
        "forms" => ExtractMode::Forms,
        "meta" => ExtractMode::Meta,
        "headings" => ExtractMode::Headings,
        other => return Err(ApiError::bad_request(format!("unknown mode '{other}'"))),
    };
    let data = extract::extract(&p.html, &p.url, mode);
    Ok(
        Json(
            json!({ "url": p.url, "mode": q.mode.unwrap_or_else(|| "text".into()), "data": data }),
        )
        .into_response(),
    )
}

#[derive(Deserialize)]
struct SnapshotQuery {
    url: String,
    max_nodes: Option<usize>,
    engine: Option<String>,
}

async fn snapshot(
    State(state): State<AppState>,
    Query(q): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine).await?;
    let opts = SnapshotOptions {
        max_nodes: q.max_nodes.unwrap_or(400).clamp(10, 2000),
        ..SnapshotOptions::default()
    };
    let tree = snapshot::snapshot(&p.html, &p.url, &opts);
    Ok(Json(tree).into_response())
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    max_results: Option<usize>,
}

async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let ddg = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&q.q));
    let p = nav_page(&state, &ddg, Engine::Fetch).await?;
    let mut results = extract::extract_search_results(&p.html);
    results.truncate(q.max_results.unwrap_or(8).clamp(1, 20));
    Ok(Json(json!({ "query": q.q, "results": results })).into_response())
}

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<lightbrowse_core::Error> for ApiError {
    fn from(e: lightbrowse_core::Error) -> Self {
        use lightbrowse_core::Error as E;
        let status = match &e {
            E::InvalidUrl(_) | E::Unsupported(_) => StatusCode::BAD_REQUEST,
            E::Http { status, .. } if *status == 404 => StatusCode::NOT_FOUND,
            E::Http { status, .. } if *status >= 500 => StatusCode::BAD_GATEWAY,
            _ => StatusCode::BAD_GATEWAY,
        };
        Self {
            status,
            message: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes();
            bytes
                .iter()
                .map(|&b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{:02X}", b),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
