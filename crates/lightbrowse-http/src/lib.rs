#![recursion_limit = "512"]

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
use lightbrowse_core::vision;
use lightbrowse_memory::{navigate_cached, MemoryStore};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn BrowserBackend>,
    pub cdp: Option<Arc<dyn BrowserBackend>>,
    pub session: Arc<Mutex<Session>>,
    /// Named sessions created on demand via `?session=<id>` — each gets its
    /// own cookies + CDP tab, so concurrent clients never share state.
    pub sessions: Arc<Mutex<std::collections::HashMap<String, Session>>>,
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
        .route("/v1/click_at", get(click_at_action))
        .route("/v1/login", get(login_action))
        .route("/v1/form/fill", axum::routing::post(form_fill))
        .route("/v1/visual_snapshot", get(visual_snapshot))
        .route("/v1/type", get(type_action))
        .route("/v1/submit", get(submit_action))
        .route("/v1/tabs", get(tabs_list))
        .route("/v1/tab/close", get(tab_close))
        .route("/v1/cookies", get(cookies))
        .route("/v1/download", get(download))
        .route("/v1/downloads", get(downloads))
        .route("/v1/network/log", get(network_log))
        .route("/v1/network/capture", get(network_capture_action))
        .route("/v1/proxy", get(proxy_get).put(proxy_set))
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(docs_page))
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
    let (cdp_tabs, cdp_max_tabs) = if let Some(cdp) = &state.cdp {
        let backend = cdp.clone();
        let cdp = backend
            .as_any()
            .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>());
        match cdp {
            Some(c) => (c.tab_count().await, state.config.max_tabs),
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
        "tabs": cdp_tabs,
        "max_tabs": cdp_max_tabs,
    }))
}

/// Current proxy in effect for each backend.
///
/// ```text
/// GET /v1/proxy  →  { "fetch": "socks5h://...", "cdp": "socks5h://..." }
/// ```
async fn proxy_get(State(state): State<AppState>) -> impl IntoResponse {
    let fetch_proxy = state
        .backend
        .as_any()
        .and_then(|b| b.downcast_ref::<lightbrowse_fetch::FetchBackend>())
        .and_then(|f| f.proxy());
    let cdp_proxy = state
        .cdp
        .as_ref()
        .and_then(|c| c.as_any())
        .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
        .and_then(|c| c.proxy());
    Json(json!({ "fetch": fetch_proxy, "cdp": cdp_proxy }))
}

#[derive(Deserialize)]
struct ProxyBody {
    /// Proxy URL (http/https/socks5/socks5h) or null/"" to go direct.
    #[serde(default)]
    proxy: Option<String>,
}

/// Set the proxy for both backends at runtime. Chromium (if running) is
/// restarted so the change applies immediately.
///
/// ```text
/// PUT /v1/proxy  body: { "proxy": "socks5h://host:1080" }
/// PUT /v1/proxy  body: { "proxy": null }   # back to direct
/// ```
async fn proxy_set(State(state): State<AppState>, Json(body): Json<ProxyBody>) -> Response {
    let proxy = match body.proxy {
        Some(p) if p.trim().is_empty() => None,
        Some(p) => {
            // Validate before touching any backend so a typo leaves state intact.
            if let Err(e) = lightbrowse_core::parse_proxy(&p) {
                return ApiError::bad_request(format!("invalid proxy: {e}")).into_response();
            }
            Some(p)
        }
        None => None,
    };

    let mut applied = Vec::new();
    if let Some(f) = state
        .backend
        .as_any()
        .and_then(|b| b.downcast_ref::<lightbrowse_fetch::FetchBackend>())
    {
        match f.set_proxy(proxy.as_deref()) {
            Ok(_) => applied.push("fetch"),
            Err(e) => {
                return ApiError::internal(format!("fetch backend: {e}")).into_response();
            }
        }
    }
    if let Some(c) = state
        .cdp
        .as_ref()
        .and_then(|c| c.as_any())
        .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
    {
        match c.set_proxy(proxy.clone()).await {
            Ok(_) => applied.push("cdp"),
            Err(e) => {
                return ApiError::internal(format!("cdp backend: {e}")).into_response();
            }
        }
    }
    tracing::info!(
        "proxy: set to {:?} (applied to {})",
        proxy,
        applied.join(", ")
    );
    Json(json!({ "ok": true, "proxy": proxy, "applied": applied })).into_response()
}

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
    engine: Option<String>,
    /// Optional named session: its own cookies + CDP tab (isolation between
    /// concurrent clients). Omit to use the shared default session.
    session: Option<String>,
}

/// Resolve a named session to the CDP tab key (the session's real id).
/// `None` input → `None` (actions fall back to the most-recently-used tab).
fn resolve_cdp_session(state: &AppState, sid: Option<&str>) -> Result<Option<String>, ApiError> {
    match sid {
        None => Ok(None),
        Some(id) => Ok(Some(session_for(state, Some(id))?.id)),
    }
}

/// Resolve the session to use: a named one (created on demand) or the
/// shared default. Cookies are per-session, so named sessions are isolated.
fn session_for(state: &AppState, sid: Option<&str>) -> Result<Session, ApiError> {
    match sid {
        None => Ok(state
            .session
            .lock()
            .map_err(|_| ApiError::internal("session lock poisoned"))?
            .clone()),
        Some(id) => {
            let mut map = state
                .sessions
                .lock()
                .map_err(|_| ApiError::internal("sessions lock poisoned"))?;
            let entry = map.entry(id.to_string()).or_insert_with(|| {
                tracing::info!("http: new named session '{id}'");
                Session::new()
            });
            Ok(entry.clone())
        }
    }
}

async fn nav_page(
    state: &AppState,
    url: &str,
    engine: Engine,
    session_id: Option<&str>,
) -> Result<lightbrowse_core::Page, ApiError> {
    // Clone out of the lock: MutexGuard is !Send and must not cross .await.
    let session = session_for(state, session_id)?;
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
    let p = nav_page(&state, &q.url, engine, q.session.as_deref()).await?;
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
    session: Option<String>,
}

async fn extract(
    State(state): State<AppState>,
    Query(q): Query<ExtractQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine, q.session.as_deref()).await?;
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
    session: Option<String>,
}

async fn snapshot(
    State(state): State<AppState>,
    Query(q): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine, q.session.as_deref()).await?;
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
    let p = nav_page(&state, &ddg, Engine::Fetch, None).await?;
    let mut results = extract::extract_search_results(&p.html);
    results.truncate(q.max_results.unwrap_or(8).clamp(1, 20));
    Ok(Json(json!({ "query": q.q, "results": results })).into_response())
}

#[derive(Deserialize)]
struct AskQuery {
    url: String,
    question: String,
    engine: Option<String>,
    session: Option<String>,
}

async fn ask(
    State(state): State<AppState>,
    Query(q): Query<AskQuery>,
) -> Result<Response, ApiError> {
    let engine = parse_engine(q.engine.as_deref(), state.engine)?;
    let p = nav_page(&state, &q.url, engine, q.session.as_deref()).await?;
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
    /// Optional session id — target the tab of this session instead of the
    /// most-recently-used one (see `navigate`'s `session` parameter).
    session: Option<String>,
}

#[derive(Deserialize)]
struct SessionQuery {
    session: Option<String>,
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

/// GET /v1/cookies — all cookies (incl. httpOnly) for the active CDP session.
async fn cookies(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let cdp = require_cdp(&state)?;
    let v = cdp
        .cookies(None)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    Ok(Json(json!({
        "count": arr.len(),
        "cookies": arr.iter().map(|c| json!({
            "name": c.get("name"),
            "value": c.get("value"),
            "domain": c.get("domain"),
            "path": c.get("path"),
            "httpOnly": c.get("httpOnly"),
            "secure": c.get("secure"),
            "sameSite": c.get("sameSite")
        })).collect::<Vec<_>>()
    })))
}

/// GET /v1/download?url=...&filename=... — trigger a programmatic download
/// on the active CDP tab and wait for the file to land.
#[derive(Deserialize)]
struct DownloadQuery {
    url: String,
    filename: Option<String>,
}

async fn download(
    State(state): State<AppState>,
    Query(q): Query<DownloadQuery>,
) -> Result<Json<Value>, ApiError> {
    let cdp = require_cdp(&state)?;
    let v = cdp
        .download(&q.url, q.filename.as_deref(), None)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(v))
}

/// GET /v1/downloads — recent programmatic downloads (newest first).
async fn downloads(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let cdp = require_cdp(&state)?;
    let list = cdp.downloads();
    Ok(Json(json!({
        "count": list.len(),
        "downloads": list
    })))
}

/// GET /v1/network/log — captured network events + capture status.
async fn network_log(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let cdp = require_cdp(&state)?;
    let events = cdp.network_log();
    Ok(Json(json!({
        "capturing": cdp.network_capturing(),
        "count": events.len(),
        "events": events
    })))
}

/// GET /v1/network/capture?action=start|stop|flush — control the capture.
async fn network_capture_action(
    State(state): State<AppState>,
    Query(q): Query<NetworkCaptureQuery>,
) -> Result<Json<Value>, ApiError> {
    let cdp = require_cdp(&state)?;
    let v = match q.action.as_deref().unwrap_or("log") {
        "start" => cdp
            .network_capture(true, None)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?,
        "stop" => cdp
            .network_capture(false, None)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?,
        "flush" => {
            cdp.network_log_clear();
            json!({ "cleared": true })
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "action must be start|stop|flush|log, got {other}"
            )))
        }
    };
    Ok(Json(v))
}

#[derive(Deserialize)]
struct NetworkCaptureQuery {
    action: Option<String>,
}

/// GET /docs — human-readable route reference rendered from the OpenAPI
/// spec (#28). Lets users discover the REST API without reading the source.
async fn docs_page() -> axum::response::Html<String> {
    let spec = openapi_spec();
    let mut rows = String::new();
    let mut paths: Vec<(&str, &Value)> = spec["paths"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default();
    paths.sort_by_key(|(p, _)| *p);
    for (path, methods) in paths {
        if let Some(methods) = methods.as_object() {
            for (method, detail) in methods {
                let summary = detail["summary"].as_str().unwrap_or("");
                let params = detail["parameters"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p["name"].as_str())
                            .map(|n| format!("<code>{}</code>", n))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let verb = method.to_uppercase();
                rows.push_str(&format!(
                    "<tr><td class=\"verb {}\">{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                    verb.to_ascii_lowercase(),
                    verb,
                    path,
                    summary,
                    params
                ));
            }
        }
    }
    let version = env!("CARGO_PKG_VERSION");
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>lightbrowse — REST API docs</title>
<style>
body{{font-family:ui-monospace,Menlo,Consolas,monospace;max-width:960px;margin:40px auto;padding:0 16px;color:#222;background:#fafafa}}
h1{{font-size:1.4rem}} h2{{font-size:1.1rem;margin-top:2rem}}
code{{background:#eee;padding:1px 5px;border-radius:4px}}
table{{border-collapse:collapse;width:100%;margin-top:1rem}}
th,td{{text-align:left;padding:8px 10px;border-bottom:1px solid #ddd;vertical-align:top}}
th{{background:#f0f0f0}}
.verb{{font-weight:700;font-size:.75rem;padding:2px 6px;border-radius:4px;color:#fff}}
.verb.get{{background:#2e7d32}}.verb.put{{background:#1565c0}}.verb.post{{background:#e65100}}.verb.delete{{background:#b71c1c}}
.quiet{{color:#777}}
</style></head><body>
<h1>lightbrowse <span class="quiet">v{version}</span> — REST API</h1>
<p class="quiet">Same backend as the CLI/MCP server. Machine-readable spec: <a href="/openapi.json">/openapi.json</a>.</p>
<h2>Endpoints</h2>
<table><tr><th>Method</th><th>Path</th><th>Summary</th><th>Query params</th></tr>{rows}</table>
</body></html>
"#
    );
    axum::response::Html(html)
}

/// The OpenAPI 3.0.3 spec for the REST API (#28). Hand-written summary;
/// the axum routes map 1:1 to the CLI subcommands.
fn openapi_spec() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "lightbrowse",
            "description": "Featherweight, AI-native browser REST API — same backend as the CLI/MCP server.",
            "version": env!("CARGO_PKG_VERSION")
        },
        "paths": {
            "/health": { "get": { "summary": "Health + memory/cdp status", "responses": { "200": {"description": "ok"} } } },
            "/v1/page": { "get": { "summary": "Fetch a URL, return summary + text preview", "parameters": [{"name": "url", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "engine", "in": "query", "schema": {"type": "string", "enum": ["auto","fetch","cdp"]}}], "responses": { "200": {"description": "page summary"} } } },
            "/v1/extract": { "get": { "summary": "Extract text|links|forms|meta|headings", "parameters": [{"name": "url", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "mode", "in": "query", "schema": {"type": "string"}}], "responses": { "200": {"description": "extracted data"} } } },
            "/v1/snapshot": { "get": { "summary": "Accessibility-style snapshot tree", "parameters": [{"name": "url", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "max_nodes", "in": "query", "schema": {"type": "integer"}}], "responses": { "200": {"description": "snapshot tree"} } } },
            "/v1/search": { "get": { "summary": "DuckDuckGo web search", "parameters": [{"name": "q", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "max_results", "in": "query", "schema": {"type": "integer"}}], "responses": { "200": {"description": "search results"} } } },
            "/v1/ask": { "get": { "summary": "Intent-aware reading: scored blocks for a question", "parameters": [{"name": "url", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "question", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "scored hits"} } } },
            "/v1/memory/search": { "get": { "summary": "BM25 over previously read pages", "parameters": [{"name": "query", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "hits"} } } },
            "/v1/current": { "get": { "summary": "Current CDP tab: url, title, text preview", "responses": { "200": {"description": "current page"} } } },
            "/v1/evaluate": { "get": { "summary": "Run JS on the active CDP tab", "parameters": [{"name": "expression", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "JS result"} } } },
            "/v1/screenshot": { "get": { "summary": "Screenshot the active CDP tab", "responses": { "200": {"description": "PNG"} } } },
            "/v1/click": { "get": { "summary": "Click a CSS selector on the active tab", "parameters": [{"name": "selector", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "click result"} } } },
            "/v1/click_at": { "get": { "summary": "Click at viewport coordinates (CSS px) — the human-pointing action for SoM/vision", "parameters": [{"name": "x", "in": "query", "required": true, "schema": {"type": "number"}}, {"name": "y", "in": "query", "required": true, "schema": {"type": "number"}}], "responses": { "200": {"description": "click result"} } } },
            "/v1/login": { "get": { "summary": "One-call login: auto-detect username+password fields on the active tab, fill both, submit", "parameters": [{"name": "username", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "password", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "fill result"} } } },
            "/v1/form/fill": { "post": { "summary": "Fill ANY form/survey in one call: values map + auto test data + optional submit", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "values": { "type": "object" }, "auto": { "type": "boolean" }, "submit": { "type": "boolean" } } } } } }, "responses": { "200": {"description": "fill result"} } } },
            "/v1/visual_snapshot": { "get": { "summary": "Vision-grounded look: screenshot + Set-of-Mark numbered overlay + uid map (base64 image)", "parameters": [{"name": "max_marks", "in": "query", "schema": {"type": "integer"}}, {"name": "max_nodes", "in": "query", "schema": {"type": "integer"}}], "responses": { "200": {"description": "JSON with image_base64 + map"} } } },
            "/v1/type": { "get": { "summary": "Type text into an input on the active tab", "parameters": [{"name": "selector", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "text", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "type result"} } } },
            "/v1/tabs": { "get": { "summary": "List open CDP tabs", "responses": { "200": {"description": "tabs"} } } },
            "/v1/tab/close": { "get": { "summary": "Close a CDP tab", "parameters": [{"name": "session", "in": "query", "schema": {"type": "string"}}], "responses": { "200": {"description": "ok"} } } },
            "/v1/cookies": { "get": { "summary": "All cookies (incl. httpOnly) for the active CDP session", "responses": { "200": {"description": "cookies"} } } },
            "/v1/download": { "get": { "summary": "Programmatic download on the active CDP tab", "parameters": [{"name": "url", "in": "query", "required": true, "schema": {"type": "string"}}, {"name": "filename", "in": "query", "schema": {"type": "string"}}], "responses": { "200": {"description": "saved file info"} } } },
            "/v1/downloads": { "get": { "summary": "Recent programmatic downloads (newest first)", "responses": { "200": {"description": "downloads"} } } },
            "/v1/network/log": { "get": { "summary": "Captured network events + capture status (network/capture must be started first)", "responses": { "200": {"description": "events"} } } },
            "/v1/network/capture": { "get": { "summary": "Control the network capture: action=start|stop|flush", "parameters": [{"name": "action", "in": "query", "required": true, "schema": {"type": "string", "enum": ["start", "stop", "flush"]}}], "responses": { "200": {"description": "capture status"} } } },
            "/v1/submit": { "get": { "summary": "Submit a form on the active tab", "parameters": [{"name": "selector", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "submit result"} } } },
            "/v1/memory/recent": { "get": { "summary": "Recently read pages", "responses": { "200": {"description": "pages"} } } },
            "/v1/runbook/list": { "get": { "summary": "List saved runbooks", "responses": { "200": {"description": "runbooks"} } } },
            "/v1/runbook/get": { "get": { "summary": "Get a runbook's steps", "parameters": [{"name": "name", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "runbook"} } } },
            "/v1/runbook/run": { "post": { "summary": "Replay a runbook", "parameters": [{"name": "name", "in": "query", "required": true, "schema": {"type": "string"}}], "responses": { "200": {"description": "outcome"} } } },
            "/v1/proxy": { "get": { "summary": "Report proxy config", "responses": { "200": {"description": "proxy"} } }, "put": { "summary": "Set proxy", "responses": { "200": {"description": "proxy updated"} } } }
        }
    })
}

/// GET /openapi.json — machine-readable route catalog (#28).
async fn openapi_json(State(_state): State<AppState>) -> Json<Value> {
    Json(openapi_spec())
}

#[derive(Deserialize)]
struct EvaluateQuery {
    expression: String,
    session: Option<String>,
}

async fn evaluate(
    State(state): State<AppState>,
    Query(q): Query<EvaluateQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .evaluate(&q.expression, cdp_session.as_deref())
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
    session: Option<String>,
}

async fn screenshot(
    State(state): State<AppState>,
    Query(q): Query<ScreenshotQuery>,
) -> Result<Response, ApiError> {
    let path = std::path::PathBuf::from(q.path.unwrap_or_else(|| "lightbrowse-shot.png".into()));
    let full = q.full_page.unwrap_or(false);
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let out = require_cdp(&state)?
        .screenshot(&path, full, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(Json(json!({ "path": out.display().to_string(), "bytes": size })).into_response())
}

async fn current_page(
    State(state): State<AppState>,
    Query(q): Query<SessionQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let (html, title, url) = require_cdp(&state)?
        .current_dom(cdp_session.as_deref())
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
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .click(&q.selector, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
}

#[derive(Deserialize)]
struct LoginQuery {
    username: String,
    password: String,
    /// Optional session id.
    session: Option<String>,
}

/// One-call login: auto-detect username+password fields, fill both, submit.
async fn login_action(
    State(state): State<AppState>,
    Query(q): Query<LoginQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .fill_login(&q.username, &q.password, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(res).into_response())
}

#[derive(Deserialize)]
struct FormFillBody {
    /// field label/name/id/placeholder -> value
    values: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_true")]
    auto: bool,
    #[serde(default)]
    submit: bool,
    /// Optional session id.
    session: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Fill any form/survey in one call (values + auto test data + optional submit).
async fn form_fill(
    State(state): State<AppState>,
    Json(body): Json<FormFillBody>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, body.session.as_deref())?;
    let res = require_cdp(&state)?
        .fill_form(&body.values, body.auto, body.submit, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(res).into_response())
}

#[derive(Deserialize)]
struct CoordQuery {
    x: f64,
    y: f64,
    /// Optional session id.
    session: Option<String>,
}

async fn click_at_action(
    State(state): State<AppState>,
    Query(q): Query<CoordQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .click_at(q.x, q.y, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "x": q.x, "y": q.y, "result": res })).into_response())
}

#[derive(Deserialize)]
struct VisualQuery {
    /// Optional session id.
    session: Option<String>,
    max_marks: Option<usize>,
    max_nodes: Option<usize>,
}

/// Vision-grounded look: screenshot + SoM numbered overlay + uid map.
async fn visual_snapshot(
    State(state): State<AppState>,
    Query(q): Query<VisualQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let cdp = require_cdp(&state)?;
    let max_nodes = q.max_nodes.unwrap_or(400).clamp(10, 2000);
    let max_marks = q.max_marks.unwrap_or(40).clamp(1, 200);

    let (html, title, url) = cdp
        .current_dom(cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    let opts = SnapshotOptions {
        max_nodes,
        max_depth: 12,
        ..SnapshotOptions::default()
    };
    let mut tree = snapshot::snapshot(&html, &url, &opts);
    let sels = snapshot::collect_selectors(&tree);
    let rects = cdp
        .element_rects(&sels, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    snapshot::attach_rects(&mut tree, &rects);

    let shot = std::env::temp_dir().join(format!("lb-shot-{}.png", std::process::id()));
    let shot_path = cdp
        .screenshot(&shot, false, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    let png = std::fs::read(&shot_path)
        .map_err(|e| ApiError::internal(format!("read screenshot: {e}")))?;
    let marks = vision::select_marks(&tree, max_marks);
    let som_marks: Vec<vision::Mark> = marks
        .iter()
        .map(|(label, _, _, b)| vision::Mark { label: *label, bbox: *b })
        .collect();
    let overlaid = vision::overlay(&png, &som_marks).map_err(ApiError::from)?;

    let mut map = serde_json::Map::new();
    for (label, uid, text, bbox) in &marks {
        map.insert(
            label.to_string(),
            json!({
                "uid": uid,
                "text": text,
                "bbox": [bbox.x, bbox.y, bbox.w, bbox.h]
            }),
        );
    }
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &overlaid);
    Ok(Json(json!({
        "url": url,
        "title": title,
        "count": marks.len(),
        "image_base64": b64,
        "map": map
    }))
    .into_response())
}

async fn type_action(
    State(state): State<AppState>,
    Query(q): Query<ActionQuery>,
) -> Result<Response, ApiError> {
    let text = q.text.as_deref().unwrap_or("");
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .type_text(&q.selector, text, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
}

async fn submit_action(
    State(state): State<AppState>,
    Query(q): Query<ActionQuery>,
) -> Result<Response, ApiError> {
    let cdp_session = resolve_cdp_session(&state, q.session.as_deref())?;
    let res = require_cdp(&state)?
        .submit(&q.selector, cdp_session.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "selector": q.selector, "result": res })).into_response())
}

/// Resource manager — list open CDP tabs (per-session) with age/idle.
async fn tabs_list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let tabs = require_cdp(&state)?.tabs_snapshot().await;
    Ok(Json(json!({ "tabs": tabs, "count": tabs.len() })).into_response())
}

/// Resource manager — close the tab of one session (manual eviction).
async fn tab_close(
    State(state): State<AppState>,
    Query(q): Query<SessionQuery>,
) -> Result<Response, ApiError> {
    let session = q
        .session
        .ok_or_else(|| ApiError::bad_request("missing ?session=<id>"))?;
    let cdp_key = session_for(&state, Some(&session))?.id;
    require_cdp(&state)?
        .close_tab(&cdp_key)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "closed": session })).into_response())
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
