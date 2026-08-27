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
use lightbrowse_core::session::Session;
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use lightbrowse_memory::{navigate_cached, MemoryStore};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn BrowserBackend>,
    pub cdp: Option<Arc<dyn BrowserBackend>>,
    pub session: Arc<Mutex<Session>>,
    pub engine: Engine,
    pub config: Arc<Config>,
    pub memory: Arc<MemoryStore>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/page", get(page))
        .route("/v1/extract", get(extract))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/search", get(search))
        .route("/v1/ask", get(ask))
        .route("/v1/memory/search", get(memory_search))
        .route("/v1/memory/recent", get(memory_recent))
        .route("/v1/current", get(current_page))
        .route("/v1/evaluate", get(evaluate))
        .route("/v1/screenshot", get(screenshot))
        .route("/v1/runbook/list", get(runbook_list))
        .route("/v1/runbook/get", get(runbook_get))
        .route("/v1/runbook/run", axum::routing::post(runbook_run))
        .route("/v1/click", get(click_action))
        .route("/v1/type", get(type_action))
        .route("/v1/submit", get(submit_action))
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
    // RAM is reported when the CDP engine has a live Chromium.
    let (cdp_running, cdp_ram_mb) = if let Some(cdp) = &state.cdp {
        let backend = cdp.clone();
        let cdp = backend
            .as_any()
            .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>());
        match cdp {
            Some(c) => (c.is_running().await, c.memory_usage_mb().await),
            None => (false, 0),
        }
    } else {
        (false, 0)
    };
    // Own process RSS (the featherweight engine's real footprint).
    let self_ram_mb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|st| {
            st.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0);

    let (cdp_peak_mb, cdp_navs) = if let Some(cdp) = &state.cdp {
        let backend = cdp.clone();
        let cdp = backend
            .as_any()
            .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>());
        match cdp {
            Some(c) => (c.peak_ram_mb(), c.navigate_count()),
            None => (0, 0),
        }
    } else {
        (0, 0)
    };
    Json(json!({
        "status": "ok",
        "service": "lightbrowse",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": state.engine,
        "headless": !state.config.ui,
        "memory_budget_mb": state.config.memory_budget_mb,
        "idle_timeout_secs": state.config.idle_timeout_secs,
        "self_ram_mb": self_ram_mb,
        "cdp_running": cdp_running,
        "cdp_ram_mb": cdp_ram_mb,
        "cdp_peak_ram_mb": cdp_peak_mb,
        "cdp_navigations": cdp_navs,
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
    if engine == Engine::Cdp {
        return lightbrowse_core::service::navigate(
            &*state.backend,
            state.cdp.as_deref(),
            &session,
            url,
            engine,
        )
        .await
        .map_err(ApiError::from);
    }
    navigate_cached(
        &state.memory,
        &*state.backend,
        state.cdp.as_deref(),
        &session,
        url,
        engine,
        300,
    )
    .await
    .map(|(p, _)| p)
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

#[derive(Deserialize)]
struct AskQuery {
    url: String,
    question: String,
    engine: Option<String>,
}

async fn ask(
    State(state): State<AppState>,
    Query(q): Query<AskQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine).await?;
    state.memory.store_page(&p).map_err(ApiError::from)?;
    let hits: Vec<Value> = state
        .memory
        .search(&q.question, 6, None)
        .map_err(ApiError::from)?
        .into_iter()
        .map(|h| {
            let mut v = serde_json::to_value(h).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                    obj.insert(
                        "text".into(),
                        Value::String(text.chars().take(300).collect()),
                    );
                }
            }
            v
        })
        .collect();
    let meta = extract::extract_meta(&p.html);
    Ok(Json(json!({
        "url": p.url,
        "title": meta.title,
        "question": q.question,
        "hits": hits,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct MemorySearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn memory_search(
    State(state): State<AppState>,
    Query(q): Query<MemorySearchQuery>,
) -> Result<Response, ApiError> {
    let hits = state
        .memory
        .search(&q.q, q.limit.unwrap_or(8).clamp(1, 50), None)
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "query": q.q, "hits": hits })).into_response())
}

#[derive(Deserialize)]
struct MemoryRecentQuery {
    limit: Option<usize>,
}

async fn memory_recent(
    State(state): State<AppState>,
    Query(q): Query<MemoryRecentQuery>,
) -> Result<Response, ApiError> {
    let pages = state
        .memory
        .recent(q.limit.unwrap_or(10).clamp(1, 50))
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "pages": pages })).into_response())
}

#[derive(Deserialize)]
struct ActionQuery {
    selector: String,
    text: Option<String>,
}

/// Downcast the shared CDP backend for stateful actions.
fn require_cdp(state: &AppState) -> Result<&lightbrowse_cdp::CdpBackend, ApiError> {
    let cdp = state
        .cdp
        .as_ref()
        .ok_or_else(|| ApiError::bad_request("cdp engine not available"))?;
    cdp.as_any()
        .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
        .ok_or_else(|| ApiError::internal("cdp backend type mismatch"))
}

#[derive(Deserialize)]
struct EvaluateQuery {
    expression: String,
}

async fn evaluate(
    State(state): State<AppState>,
    Query(q): Query<EvaluateQuery>,
) -> Result<Response, ApiError> {
    let res = require_cdp(&state)?
        .evaluate(&q.expression)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "result": res })).into_response())
}

async fn runbook_list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let books = state.memory.list_runbooks().map_err(ApiError::from)?;
    let out: Vec<Value> = books
        .into_iter()
        .map(|(name, url, _, cnt)| json!({ "name": name, "url": url, "success_count": cnt }))
        .collect();
    Ok(Json(json!({ "runbooks": out })).into_response())
}

#[derive(Deserialize)]
struct RunbookGetQuery {
    name: String,
}

async fn runbook_get(
    State(state): State<AppState>,
    Query(q): Query<RunbookGetQuery>,
) -> Result<Response, ApiError> {
    match state.memory.get_runbook(&q.name).map_err(ApiError::from)? {
        Some((name, url, steps, cnt)) => {
            let parsed: Value =
                serde_json::from_str(&steps).map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(
                Json(json!({ "name": name, "url": url, "success_count": cnt, "steps": parsed }))
                    .into_response(),
            )
        }
        None => Err(ApiError::bad_request(format!(
            "runbook '{}' not found",
            q.name
        ))),
    }
}

#[derive(Deserialize)]
struct RunbookRunBody {
    name: String,
    #[serde(default)]
    variables: std::collections::HashMap<String, String>,
}

async fn runbook_run(
    State(state): State<AppState>,
    Json(body): Json<RunbookRunBody>,
) -> Result<Response, ApiError> {
    let (_, url, steps_json, _) = state
        .memory
        .get_runbook(&body.name)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::bad_request(format!("runbook '{}' not found", body.name)))?;
    let steps: Vec<lightbrowse_cdp::RunbookStep> =
        serde_json::from_str(&steps_json).map_err(|e| ApiError::internal(e.to_string()))?;
    let cdp = require_cdp(&state)?;
    let outcome = lightbrowse_cdp::run_runbook(cdp, &url, &steps, &body.variables)
        .await
        .map_err(ApiError::from)?;
    if outcome.ok {
        state.memory.runbook_success(&body.name).ok();
    }
    Ok(Json(outcome).into_response())
}

#[derive(Deserialize)]
struct ScreenshotQuery {
    path: Option<String>,
    full_page: Option<bool>,
}

async fn screenshot(
    State(state): State<AppState>,
    Query(q): Query<ScreenshotQuery>,
) -> Result<Response, ApiError> {
    let path = std::path::PathBuf::from(q.path.unwrap_or_else(|| "lightbrowse-shot.png".into()));
    let full = q.full_page.unwrap_or(false);
    let out = require_cdp(&state)?
        .screenshot(&path, full)
        .await
        .map_err(ApiError::from)?;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(Json(json!({ "path": out.display().to_string(), "bytes": size })).into_response())
}

async fn current_page(State(state): State<AppState>) -> Result<Response, ApiError> {
    let (html, title, url) = require_cdp(&state)?
        .current_dom()
        .await
        .map_err(ApiError::from)?;
    let t = extract::extract_text(&html);
    Ok(Json(json!({
        "url": url,
        "title": title,
        "word_count": t.word_count,
        "text_preview": t.text.chars().take(3000).collect::<String>(),
    }))
    .into_response())
}

async fn click_action(
    State(state): State<AppState>,
    Query(q): Query<ActionQuery>,
) -> Result<Response, ApiError> {
    let res = require_cdp(&state)?
        .click(&q.selector)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
}

async fn type_action(
    State(state): State<AppState>,
    Query(q): Query<ActionQuery>,
) -> Result<Response, ApiError> {
    let text = q.text.as_deref().unwrap_or("");
    let res = require_cdp(&state)?
        .type_text(&q.selector, text)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
}

async fn submit_action(
    State(state): State<AppState>,
    Query(q): Query<ActionQuery>,
) -> Result<Response, ApiError> {
    let res = require_cdp(&state)?
        .submit(&q.selector)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
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
