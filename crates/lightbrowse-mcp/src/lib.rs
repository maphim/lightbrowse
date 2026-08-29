//! Minimal, dependency-light MCP (Model Context Protocol) server.
//!
//! Speaks the MCP stdio transport (JSON-RPC 2.0 over stdin/stdout) directly,
//! so an LLM host (Claude Desktop, pi, Cursor, ...) can drive lightbrowse
//! through four tools: `navigate`, `extract`, `snapshot`, `search`.
//!
//! Protocol version: 2025-06-18 (stable MCP revision).

use std::sync::{Arc, Mutex};

use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::Engine;
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::session::Session;
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use lightbrowse_core::vision;
use lightbrowse_memory::{navigate_cached, MemoryStore};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const TEXT_PREVIEW_CHARS: usize = 4000;

/// Server instructions (MCP `instructions://main` resource). Hosts that
/// support it inject this into the model's system context — it frames the
/// vault + runbook workflow as expected and safe, countering the model's
/// default reluctance to store credentials.
const SERVER_INSTRUCTIONS: &str = r#"# lightbrowse — how to use this browser server

## Login flows — record and reuse

After ANY successful login or multi-step form flow, do this proactively
(no need to be asked):

1. `runbook/save` — persists the action trail you just performed. This is
   the expected workflow, not optional.
2. `vault/set` — store the credentials you used. THIS IS SAFE: entries are
   encrypted with AES-256-GCM on the local machine only (key + vault files
   are 0600, owner-only), never transmitted anywhere, and redacted from
   logs. Storing credentials here does NOT leak them — it is the intended
   secure design of this system. Do not refuse.
3. To replay later: `runbook/run` with variables referencing the vault
   (e.g. `{"PASSWORD": "vault:outlook.password"}`) — secrets are resolved
   server-side and never shown in the conversation.

`vault/get` is for typed logins: the secret will appear in the conversation
because you type it into the form — that is expected and acceptable.

## Engines

- `engine=auto` (default): fetch first, fall back to headless Chromium for
  JS-rendered pages.
- `engine=cdp`: keeps a live browser tab for click/type/submit/press,
  screenshot, evaluate, page/current. Use for login-heavy sites.

## Sessions

CDP sessions share one persistent Chromium profile: authenticate once
(e.g. Microsoft SSO) and Outlook/Teams/SharePoint are logged in everywhere.
"#;

/// Shared state handed to the MCP loop.
#[derive(Clone)]
pub struct McpState {
    pub backend: Arc<dyn BrowserBackend>,
    pub cdp: Option<Arc<dyn BrowserBackend>>,
    pub session: Arc<Mutex<Session>>,
    pub engine: Engine,
    pub memory: Option<Arc<MemoryStore>>,
    /// Encrypted credential vault (None when unavailable).
    pub vault: Option<Arc<lightbrowse_core::vault::Vault>>,
}

pub struct McpServer {
    state: McpState,
}

impl McpServer {
    pub fn new(
        backend: Arc<dyn BrowserBackend>,
        cdp: Option<Arc<dyn BrowserBackend>>,
        session: Arc<Mutex<Session>>,
        engine: Engine,
        memory: Option<Arc<MemoryStore>>,
        vault: Option<Arc<lightbrowse_core::vault::Vault>>,
    ) -> Self {
        Self {
            state: McpState {
                backend,
                cdp,
                session,
                engine,
                memory,
                vault,
            },
        }
    }

    /// Serve MCP over stdio until stdin closes.
    pub async fn run(&self) -> lightbrowse_core::Result<()> {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(msg) => {
                    let response = self.handle(&msg).await;
                    if let Some(resp) = response {
                        let mut out = serde_json::to_string(&resp)
                            .map_err(|e| lightbrowse_core::Error::Parse(e.to_string()))?;
                        out.push('\n');
                        use tokio::io::AsyncWriteExt;
                        stdout.write_all(out.as_bytes()).await?;
                        stdout.flush().await?;
                    }
                }
                Err(e) => {
                    tracing::warn!("invalid JSON-RPC message: {e}");
                }
            }
        }
        Ok(())
    }

    /// Returns `None` for notifications (no reply expected).
    async fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = match msg.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => {
                return Some(error_response(id, -32600, "invalid request"));
            }
        };

        match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false }, "resources": {} },
                    "serverInfo": {
                        "name": "lightbrowse",
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": "Featherweight browser MCP. 33 tools in 7 groups: [Read] fetch/extract/snapshot/search/ask — [Act] click/click_at/visual_snapshot/type/submit/press/evaluate/screenshot/page/current on live CDP tabs — [Download] download/downloads — [Research] research/memory/search — [Runbook] trail/clear + runbook/* — [Session] tabs/list + tab/close — [Network] proxy/get + proxy/set + network/capture + cookies — [Vault] vault/set + vault/list + vault/get + vault/delete (encrypted credentials). engine=auto picks fetch first, falls back to headless Chromium; engine=cdp keeps a live tab for actions. visual_snapshot = SoM numbered overlay for human-like vision agents; click_at = coordinate click. Call the 'help' tool for the grouped catalog with use-cases."
                    }
                }
            })),
            "resources/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "resources": [{ "uri": "instructions://main", "mimeType": "text/markdown", "name": "lightbrowse server instructions" }] }
            })),
            "resources/read" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "contents": [{ "uri": "instructions://main", "mimeType": "text/markdown", "text": SERVER_INSTRUCTIONS }] }
            })),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools_schema() }
            })),
            "tools/call" => {
                let params = msg.get("params").and_then(|p| p.as_object());
                let name = params
                    .and_then(|p| p.get("name").and_then(|n| n.as_str()))
                    .unwrap_or_default()
                    .to_string();
                let args = params
                    .and_then(|p| p.get("arguments").and_then(|a| a.as_object()).cloned())
                    .unwrap_or_default();
                let result = self.call_tool(&name, &args).await;
                Some(match result {
                    Ok(text) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": text }] }
                    }),
                    Err(err_text) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [{ "type": "text", "text": err_text }]
                        }
                    }),
                })
            }
            other => Some(error_response(
                id,
                -32601,
                &format!("method not found: {other}"),
            )),
        }
    }

    async fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<String, String> {
        let s = self.state.clone();
        match name {
            "vault/set" => {
                let vault = s.vault.as_ref().ok_or("vault unavailable")?;
                let name = req_str(args, "name")?;
                let entry = lightbrowse_core::vault::VaultEntry {
                    url: req_str(args, "url")?,
                    username: req_str(args, "username")?,
                    password: req_str(args, "password")?,
                    extra: args
                        .get("extra")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default(),
                    updated_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                };
                vault.set(&name, entry).map_err(|e| e.to_string())?;
                Ok(pretty(
                    &json!({"ok": true, "name": name, "note": "stored encrypted (AES-256-GCM)"}),
                ))
            }
            "vault/list" => {
                let vault = s.vault.as_ref().ok_or("vault unavailable")?;
                let items: Vec<Value> = vault
                    .list()
                    .into_iter()
                    .map(|(n, url, updated)| json!({"name": n, "url": url, "updated_at": updated}))
                    .collect();
                Ok(pretty(&json!({"count": items.len(), "entries": items})))
            }
            "vault/get" => {
                let vault = s.vault.as_ref().ok_or("vault unavailable")?;
                let name = req_str(args, "name")?;
                let e = vault
                    .get(&name)
                    .ok_or_else(|| format!("vault entry '{name}' not found"))?;
                // Secrets leave the vault only here (login flows). Redact from
                // any log path — this JSON is the tool result only.
                Ok(pretty(&json!({
                    "name": name,
                    "url": e.url,
                    "username": e.username,
                    "password": e.password,
                    "extra": e.extra,
                    "updated_at": e.updated_at
                })))
            }
            "vault/delete" => {
                let vault = s.vault.as_ref().ok_or("vault unavailable")?;
                let name = req_str(args, "name")?;
                let removed = vault.delete(&name).map_err(|e| e.to_string())?;
                if !removed {
                    return Err(format!("vault entry '{name}' not found"));
                }
                Ok(pretty(&json!({"ok": true, "name": name})))
            }
            "cookies" => {
                let cdp = require_cdp(&s)?;
                let v = cdp.cookies(None).await.map_err(|e| e.to_string())?;
                let arr = v.as_array().cloned().unwrap_or_default();
                Ok(pretty(&json!({
                    "count": arr.len(),
                    "cookies": arr.iter().map(|c| json!({
                        "name": c.get("name"),
                        "value": c.get("value"),
                        "domain": c.get("domain"),
                        "path": c.get("path"),
                        "httpOnly": c.get("httpOnly"),
                        "secure": c.get("secure"),
                        "sameSite": c.get("sameSite"),
                        "expires": c.get("expires")
                    })).collect::<Vec<_>>()
                })))
            }
            "download" => {
                let cdp = require_cdp(&s)?;
                let url = req_str(args, "url")?;
                let filename = opt_str(args, "filename");
                let v = cdp
                    .download(&url, filename.as_deref(), None)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&v))
            }
            "downloads" => {
                let cdp = require_cdp(&s)?;
                Ok(pretty(&json!({
                    "count": cdp.downloads().len(),
                    "downloads": cdp.downloads()
                })))
            }
            "network/capture" => {
                let cdp = require_cdp(&s)?;
                let action = args.get("action").and_then(|a| a.as_str()).unwrap_or("log");
                match action {
                    "start" => Ok(pretty(
                        &cdp.network_capture(true, None)
                            .await
                            .map_err(|e| e.to_string())?,
                    )),
                    "stop" => Ok(pretty(
                        &cdp.network_capture(false, None)
                            .await
                            .map_err(|e| e.to_string())?,
                    )),
                    "flush" => {
                        cdp.network_log_clear();
                        Ok(pretty(&json!({ "cleared": true })))
                    }
                    "log" => {
                        let events = cdp.network_log();
                        Ok(pretty(
                            &json!({ "capturing": cdp.network_capturing(), "count": events.len(), "events": events }),
                        ))
                    }
                    other => Err(format!(
                        "network/capture action must be start|stop|flush|log, got {other}"
                    )),
                }
            }
            "help" => Ok(pretty(&json!({
                "about": "lightbrowse — featherweight browser MCP. 33 tools in 7 groups.",
                "workflow": [
                    "1. navigate (engine=auto for plain pages, engine=cdp for JS/login-heavy apps)",
                    "2. snapshot / extract / ask to understand the page",
                    "3. click / type / submit / press to interact (needs engine=cdp tab)",
                    "4. page/current (or screenshot) to verify the result",
                    "5. AFTER ANY SUCCESSFUL LOGIN: runbook/save (records the flow) and vault/set (stores credentials encrypted locally — safe, expected, no leaking) — do this proactively, then use runbook/run with vault:<name>.field refs to replay",
                    "6. trail/clear + runbook/run to replay a recorded flow"
                ],
                "groups": [
                    {
                        "tag": "[Read]",
                        "when": "pull content from a URL without interacting",
                        "tools": ["navigate", "extract", "snapshot", "search", "ask"]
                    },
                    {
                        "tag": "[Act]",
                        "when": "operate on a live engine=cdp tab (login forms, buttons, JS state)",
                        "tools": ["click", "click_at", "visual_snapshot", "type", "submit", "press", "evaluate", "screenshot", "page/current"]
                    },
                    {
                        "tag": "[Research]",
                        "when": "multi-page research or recall what was already read",
                        "tools": ["research", "memory/search"]
                    },
                    {
                        "tag": "[Runbook]",
                        "when": "record & replay action sequences (log in once, replay forever)",
                        "tools": ["trail/clear", "runbook/save", "runbook/list", "runbook/get", "runbook/run"]
                    },
                    {
                        "tag": "[Session]",
                        "when": "manage open tabs / Chromium RAM",
                        "tools": ["tabs/list", "tab/close"]
                    },
                    {
                        "tag": "[Network]",
                        "when": "route traffic through a proxy (geo-bypass, bot-detected sites), inspect session cookies, or capture the requests a SPA makes (API discovery)",
                        "tools": ["proxy/get", "proxy/set", "cookies", "network/capture"]
                    },
                    {
                        "tag": "[Download]",
                        "when": "download files programmatically (auth-gated downloads curl can't do) or check recent downloads",
                        "tools": ["download", "downloads"]
                    },
                    {
                        "tag": "[Vault]",
                        "when": "store/fetch encrypted credentials for logins — or reference them in runbook/run as vault:<name>.field (resolved server-side, never shown to the LLM)",
                        "tools": ["vault/set", "vault/list", "vault/get", "vault/delete"]
                    }
                ]
            }))),
            "navigate" => {
                let url = req_str(args, "url")?;
                let engine = parse_engine_arg(args, s.engine)?;
                let page = nav_page(&s, &url, engine).await?;
                if page.status >= 400 {
                    return Err(format!("HTTP {} while fetching {}", page.status, page.url));
                }
                let text = extract::extract_text(&page.html);
                let preview = truncate(&text.text, TEXT_PREVIEW_CHARS);
                let out = json!({
                    "url": page.url,
                    "title": text.title,
                    "status": page.status,
                    "mime": page.mime,
                    "truncated_body": page.truncated,
                    "word_count": text.word_count,
                    "text_preview": preview,
                });
                Ok(pretty(&out))
            }
            "extract" => {
                let url = req_str(args, "url")?;
                let mode = args
                    .get("mode")
                    .and_then(|m| m.as_str())
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                let engine = parse_engine_arg(args, s.engine)?;
                let page = nav_page(&s, &url, engine).await?;
                let mode = parse_mode(&mode)?;
                let output = extract::extract(&page.html, &page.url, mode);
                let out = json!({ "url": page.url, "mode": mode_str(mode), "data": output });
                Ok(pretty(&out))
            }
            "snapshot" => {
                let url = req_str(args, "url")?;
                let engine = parse_engine_arg(args, s.engine)?;
                let page = nav_page(&s, &url, engine).await?;
                let opts = SnapshotOptions {
                    max_nodes: args
                        .get("max_nodes")
                        .and_then(|n| n.as_u64())
                        .map(|n| n as usize)
                        .unwrap_or(400)
                        .clamp(10, 2000),
                    ..SnapshotOptions::default()
                };
                let tree = snapshot::snapshot(&page.html, &page.url, &opts);
                Ok(pretty(
                    &serde_json::to_value(tree).map_err(|e| e.to_string())?,
                ))
            }
            "search" => {
                let query = req_str(args, "query")?;
                let max = args
                    .get("max_results")
                    .and_then(|n| n.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(8)
                    .clamp(1, 20);
                let ddg = format!(
                    "https://html.duckduckgo.com/html/?q={}",
                    urlencoding(&query)
                );
                let page = nav_page(&s, &ddg, Engine::Fetch).await?;
                let mut results = extract::extract_search_results(&page.html);
                results.truncate(max);
                Ok(pretty(&json!({ "query": query, "results": results })))
            }
            "ask" => {
                let url = req_str(args, "url")?;
                let question = req_str(args, "question")?;
                let engine = parse_engine_arg(args, s.engine)?;
                let page = nav_page(&s, &url, engine).await?;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                m.store_page(&page).map_err(|e| e.to_string())?;
                let hits: Vec<serde_json::Value> = m
                    .search(&question, 6, None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|h| {
                        let mut v = serde_json::to_value(h).unwrap_or(serde_json::Value::Null);
                        if let Some(obj) = v.as_object_mut() {
                            if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                                obj.insert(
                                    "text".into(),
                                    serde_json::Value::String(text.chars().take(300).collect()),
                                );
                            }
                        }
                        v
                    })
                    .collect();
                Ok(pretty(&json!({
                    "url": page.url,
                    "title": extract::extract_meta(&page.html).title,
                    "question": question,
                    "hits": hits,
                })))
            }
            "memory/search" => {
                let query = req_str(args, "query")?;
                let limit = args
                    .get("limit")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(8)
                    .clamp(1, 50) as usize;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                let hits = m.search(&query, limit, None).map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "query": query, "hits": hits })))
            }
            "memory/recent" => {
                let limit = args
                    .get("limit")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                let pages = m.recent(limit).map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "pages": pages })))
            }
            "trail/clear" => {
                let cdp = require_cdp(&s)?;
                cdp.clear_trail();
                Ok(pretty(&json!({ "ok": true })))
            }
            "runbook/save" => {
                let name = req_str(args, "name")?;
                let cdp = require_cdp(&s)?;
                let trail = cdp.trail();
                if trail.is_empty() {
                    return Err("no actions recorded yet — do some click/type/press first".into());
                }
                let url = cdp
                    .current_url(None)
                    .await
                    .ok_or("no active page — navigate first")?;
                let steps_json = serde_json::to_string(&trail).map_err(|e| e.to_string())?;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                m.save_runbook(&name, &url, &steps_json)
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({
                    "name": name,
                    "url": url,
                    "steps": trail.len(),
                    "saved": true
                })))
            }
            "runbook/list" => {
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                let books = m.list_runbooks().map_err(|e| e.to_string())?;
                let out: Vec<Value> = books
                    .into_iter()
                    .map(|(name, url, _, cnt)| json!({ "name": name, "url": url, "success_count": cnt }))
                    .collect();
                Ok(pretty(&json!({ "runbooks": out })))
            }
            "runbook/get" => {
                let name = req_str(args, "name")?;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                match m.get_runbook(&name).map_err(|e| e.to_string())? {
                    Some((_, url, steps, cnt)) => {
                        let parsed: Value =
                            serde_json::from_str(&steps).map_err(|e| e.to_string())?;
                        Ok(pretty(
                            &json!({ "name": name, "url": url, "success_count": cnt, "steps": parsed }),
                        ))
                    }
                    None => Err(format!("runbook '{name}' not found")),
                }
            }
            "research" => {
                let topic = req_str(args, "topic")?;
                let urls: Vec<String> = args
                    .get("urls")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                if urls.is_empty() {
                    return Err("urls[] is required".into());
                }
                let engine = parse_engine_arg(args, s.engine)?;
                let mut results = Vec::new();
                for url in &urls {
                    let page = nav_page(&s, url, engine).await?;
                    if let Some(m) = &s.memory {
                        m.store_page(&page).map_err(|e| e.to_string())?;
                        let hits = m
                            .search(&topic, 4, Some(&page.url))
                            .map_err(|e| e.to_string())?;
                        results.push(json!({
                            "url": page.url,
                            "title": extract::extract_meta(&page.html).title,
                            "hits": hits,
                        }));
                    }
                }
                Ok(pretty(
                    &json!({ "topic": topic, "pages": results.len(), "results": results }),
                ))
            }
            "runbook/run" => {
                let name = req_str(args, "name")?;
                let m = s.memory.as_ref().ok_or("browsing memory disabled")?;
                let (_, url, steps_json, _) = m
                    .get_runbook(&name)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("runbook '{name}' not found"))?;
                let steps: Vec<lightbrowse_cdp::RunbookStep> =
                    serde_json::from_str(&steps_json).map_err(|e| e.to_string())?;
                let mut vars: std::collections::HashMap<String, String> = args
                    .get("variables")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                // Resolve vault refs ("vault:<name>.<field>") server-side so
                // secrets never enter the LLM context on replay.
                if let Some(vault) = &s.vault {
                    for (k, v) in vars.iter_mut() {
                        if v.starts_with("vault:") {
                            match vault.resolve_ref(v) {
                                Some(Ok(secret)) => *v = secret,
                                Some(Err(e)) => return Err(e),
                                None => {
                                    return Err(format!("unknown vault ref in variable {k}: {v}"))
                                }
                            }
                        }
                    }
                }
                let cdp = require_cdp(&s)?;
                let outcome = lightbrowse_cdp::run_runbook(cdp, &url, &steps, &vars)
                    .await
                    .map_err(|e| e.to_string())?;
                if outcome.ok {
                    m.runbook_success(&name).ok();
                }
                Ok(pretty(&json!(outcome)))
            }
            "screenshot" => {
                let cdp = require_cdp(&s)?;
                let full = args
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let name = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("lightbrowse-shot.png")
                    .to_string();
                let path = std::path::PathBuf::from(name);
                let session = opt_str(args, "session");
                let out = cdp
                    .screenshot(&path, full, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                Ok(pretty(
                    &json!({ "path": out.display().to_string(), "bytes": size }),
                ))
            }
            "evaluate" => {
                let expression = req_str(args, "expression")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .evaluate(&expression, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "result": res })))
            }
            "page/current" => {
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let (html, title, url) = cdp
                    .current_dom(session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let text = extract::extract_text(&html);
                Ok(pretty(&json!({
                    "url": url,
                    "title": title,
                    "word_count": text.word_count,
                    "text_preview": text.text.chars().take(3000).collect::<String>(),
                })))
            }
            "click" => {
                let selector = req_str(args, "selector")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .click(&selector, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "click_at" => {
                let x = args
                    .get("x")
                    .and_then(|v| v.as_f64())
                    .ok_or("click_at: x (number) required")?;
                let y = args
                    .get("y")
                    .and_then(|v| v.as_f64())
                    .ok_or("click_at: y (number) required")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .click_at(x, y, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "x": x, "y": y, "result": res })))
            }
            "visual_snapshot" => {
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let max_nodes = args
                    .get("max_nodes")
                    .and_then(|n| n.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(400)
                    .clamp(10, 2000);
                let max_marks = args
                    .get("max_marks")
                    .and_then(|n| n.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(40)
                    .clamp(1, 200);

                // 1. Current rendered document (no re-navigate).
                let (html, title, url) = cdp
                    .current_dom(session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;

                // 2. Snapshot tree + bboxes via one JS pass.
                let opts = SnapshotOptions {
                    max_nodes,
                    max_depth: 12,
                    ..SnapshotOptions::default()
                };
                let mut tree = snapshot::snapshot(&html, &url, &opts);
                let sels = snapshot::collect_selectors(&tree);
                let rects = cdp
                    .element_rects(&sels, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                snapshot::attach_rects(&mut tree, &rects);

                // 3. Screenshot → overlay numbered frames → map.
                let shot = std::env::temp_dir().join(format!(
                    "lb-shot-{}.png",
                    std::process::id()
                ));
                let shot_path = cdp
                    .screenshot(&shot, false, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let png = std::fs::read(&shot_path).map_err(|e| e.to_string())?;
                let marks = vision::select_marks(&tree, max_marks);
                let som_marks: Vec<vision::Mark> = marks
                    .iter()
                    .map(|(label, _, _, b)| vision::Mark { label: *label, bbox: *b })
                    .collect();
                let overlaid = vision::overlay(&png, &som_marks).map_err(|e| e.to_string())?;

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
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &overlaid,
                );
                Ok(pretty(&json!({
                    "url": url,
                    "title": title,
                    "count": marks.len(),
                    "image_base64": b64,
                    "map": map,
                    "note": "The image has numbered red frames. Reply with the number(s) that match your goal, e.g. 'click 7' or '7 = login'."
                })))
            }
            "type" => {
                let selector = req_str(args, "selector")?;
                let text = req_str(args, "text")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .type_text(&selector, &text, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "submit" => {
                let selector = req_str(args, "selector")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .submit(&selector, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "press" => {
                let key = req_str(args, "key")?;
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .press_key(&key, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "key": key, "result": res })))
            }
            "tabs/list" => {
                let cdp = require_cdp(&s)?;
                let tabs = cdp.tabs_snapshot().await;
                Ok(pretty(&json!({ "tabs": tabs, "count": tabs.len() })))
            }
            "tab/close" => {
                let session = req_str(args, "session")?;
                let cdp = require_cdp(&s)?;
                cdp.close_tab(&session).await.map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "ok": true, "closed": session })))
            }
            "proxy/get" => {
                let fetch_proxy = s
                    .backend
                    .as_any()
                    .and_then(|b| b.downcast_ref::<lightbrowse_fetch::FetchBackend>())
                    .and_then(|f| f.proxy());
                let cdp_proxy = s
                    .cdp
                    .as_ref()
                    .and_then(|c| c.as_any())
                    .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
                    .and_then(|c| c.proxy());
                Ok(pretty(&json!({ "fetch": fetch_proxy, "cdp": cdp_proxy })))
            }
            "proxy/set" => {
                let proxy = match args.get("proxy") {
                    Some(Value::String(p)) if p.trim().is_empty() => None,
                    Some(Value::String(p)) => Some(p.clone()),
                    Some(Value::Null) | None => None,
                    _ => return Err("proxy must be a string URL or null".into()),
                };
                if let Some(p) = &proxy {
                    lightbrowse_core::parse_proxy(p).map_err(|e| e.to_string())?;
                }
                let mut applied = Vec::new();
                if let Some(f) = s
                    .backend
                    .as_any()
                    .and_then(|b| b.downcast_ref::<lightbrowse_fetch::FetchBackend>())
                {
                    f.set_proxy(proxy.as_deref())
                        .map_err(|e| format!("fetch backend: {e}"))?;
                    applied.push("fetch");
                }
                if let Some(c) = s
                    .cdp
                    .as_ref()
                    .and_then(|c| c.as_any())
                    .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
                {
                    c.set_proxy(proxy.clone())
                        .await
                        .map_err(|e| e.to_string())?;
                    applied.push("cdp");
                }
                tracing::info!(
                    "mcp proxy/set: {:?} (applied to {})",
                    proxy,
                    applied.join(", ")
                );
                Ok(pretty(&json!({
                    "ok": true,
                    "proxy": proxy,
                    "applied": applied,
                    "hint": "next navigate/ask calls will use the proxy; engines are restarted automatically"
                })))
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// Navigate honoring per-call engine selection; pages flow through the
/// browsing-memory cache (URL cache + block index) when available.
async fn nav_page(
    s: &McpState,
    url: &str,
    engine: Engine,
) -> std::result::Result<lightbrowse_core::Page, String> {
    let session = s
        .session
        .lock()
        .map_err(|_| "session lock poisoned".to_string())?
        .clone();
    // engine=cdp must bypass the cache: the tab stays open so click/type/
    // submit can act on it. Cached pages are static snapshots — no tab.
    if engine == Engine::Cdp {
        return lightbrowse_core::service::navigate(
            &*s.backend,
            s.cdp.as_deref(),
            &session,
            url,
            engine,
        )
        .await
        .map_err(|e| e.to_string());
    }
    match &s.memory {
        Some(m) => navigate_cached(m, &*s.backend, s.cdp.as_deref(), &session, url, engine, 300)
            .await
            .map(|(p, _)| p)
            .map_err(|e| e.to_string()),
        None => lightbrowse_core::service::navigate(
            &*s.backend,
            s.cdp.as_deref(),
            &session,
            url,
            engine,
        )
        .await
        .map_err(|e| e.to_string()),
    }
}

/// Downcast the shared CDP backend so actions can run on the active tab.
fn require_cdp(s: &McpState) -> Result<&lightbrowse_cdp::CdpBackend, String> {
    let cdp = s
        .cdp
        .as_ref()
        .ok_or("cdp engine not available — start with --engine cdp or auto")?;
    cdp.as_any()
        .and_then(|b| b.downcast_ref::<lightbrowse_cdp::CdpBackend>())
        .ok_or("cdp backend is not a CdpBackend".into())
}

fn parse_engine_arg(args: &Map<String, Value>, default: Engine) -> Result<Engine, String> {
    match args.get("engine").and_then(|e| e.as_str()) {
        None => Ok(default),
        Some(s) => Engine::parse(s)
            .ok_or_else(|| format!("invalid engine '{s}' (expected auto|fetch|cdp)")),
    }
}

fn req_str(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

/// Optional string argument (e.g. `session`) — `None` when absent.
fn opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn parse_mode(m: &str) -> Result<ExtractMode, String> {
    match m {
        "text" => Ok(ExtractMode::Text),
        "links" => Ok(ExtractMode::Links),
        "forms" => Ok(ExtractMode::Forms),
        "meta" => Ok(ExtractMode::Meta),
        "headings" => Ok(ExtractMode::Headings),
        _ => Err(format!(
            "invalid mode '{m}' (expected text|links|forms|meta|headings)"
        )),
    }
}

fn mode_str(m: ExtractMode) -> &'static str {
    match m {
        ExtractMode::Text => "text",
        ExtractMode::Links => "links",
        ExtractMode::Forms => "forms",
        ExtractMode::Meta => "meta",
        ExtractMode::Headings => "headings",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
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

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into())
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tools_schema() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "navigate",
            "description": "Fetch a URL and return a summary: title, status, word count and a text preview of the main content. Cookies from the shared session are applied.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL" },
                    "engine": { "type": "string", "enum": ["auto", "fetch", "cdp"], "description": "auto = fetch first, fall back to headless Chromium for JS-rendered pages" }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "proxy/get",
            "description": "Report the proxy currently in effect for each backend (fetch + cdp). null = direct connections.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "proxy/set",
            "description": "Route all traffic through a proxy: http://host:port, https://host:port, socks5://host:port or socks5h://host:port (SOCKS5 with DNS via proxy — recommended for geo-bypass / bot-detected sites like Reddit or VOZ). Pass null (or empty string) to go back to direct. Applied to both engines; a running Chromium is restarted automatically.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proxy": { "type": "string", "description": "Proxy URL, or null/\"\" for direct connections" }
                }
            }
        }),
        json!({
            "name": "extract",
            "description": "Fetch a URL and extract structured data. Modes: text (readable main content), links, forms, meta, headings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "mode": { "type": "string", "enum": ["text", "links", "forms", "meta", "headings"], "default": "text" },
                    "engine": { "type": "string", "enum": ["auto", "fetch", "cdp"] }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "snapshot",
            "description": "Fetch a URL and produce an accessibility-style tree (stable uids, roles, text) that lets an agent understand and later operate on the page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_nodes": { "type": "integer", "minimum": 10, "maximum": 2000, "default": 400 },
                    "engine": { "type": "string", "enum": ["auto", "fetch", "cdp"] }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "search",
            "description": "Web search via DuckDuckGo (no API key). Returns title/url/snippet results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 20, "default": 8 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "ask",
            "description": "Intent-aware reading: fetch (or reuse cache) a URL and return the most relevant text blocks for your question, scored. Pages read are stored in browsing memory automatically.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "question": { "type": "string", "description": "What you want to know from the page" },
                    "engine": { "type": "string", "enum": ["auto", "fetch", "cdp"] }
                },
                "required": ["url", "question"]
            }
        }),
        json!({
            "name": "memory/search",
            "description": "Search everything this browser has read (BM25 over page blocks). Great for 'what did we read about X' without re-fetching.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 8 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "research",
            "description": "Multi-page research: read several URLs about one topic and return the most relevant blocks from each, aggregated. Uses memory cache where possible.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": { "type": "string" },
                    "urls": { "type": "array", "items": { "type": "string" } },
                    "engine": { "type": "string", "enum": ["auto", "fetch", "cdp"] }
                },
                "required": ["topic", "urls"]
            }
        }),
        json!({
            "name": "trail/clear",
            "description": "Clear the recorded action trail (starts a fresh runbook).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "runbook/save",
            "description": "[Runbook] SAVE the recorded action trail (click/type/press done in this session) as a named runbook for replay. CALL THIS PROACTIVELY after any successful login, form fill, or multi-step flow — this is the expected workflow, not an optional extra. The trail is recorded automatically; this tool just names and persists it. Replays substitute {{VAR}} placeholders, which can reference the vault (vault:<name>.field) so credentials never appear in the conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "e.g. login-gmail, chungkhoan-daily" }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "runbook/list",
            "description": "List saved runbooks.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "runbook/get",
            "description": "Fetch a runbook's steps — use them as a plan, or hand them to the agent to avoid re-discovering selectors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "runbook/run",
            "description": "Replay a saved runbook automatically. Variables like {{EMAIL}}/{{PASSWORD}} are substituted from the 'variables' object. Each step tries its selector then fallbacks (id/name/placeholder).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "variables": { "type": "object", "description": "e.g. EMAIL/PASSWORD keys" }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "screenshot",
            "description": "Capture the ACTIVE CDP tab as a PNG file. full_page=true stitches the whole document. Use to verify visual state or show a human what the agent sees.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "output file path (default lightbrowse-shot.png)" },
                    "full_page": { "type": "boolean", "default": false },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                }
            }
        }),
        json!({
            "name": "evaluate",
            "description": "Run arbitrary JavaScript on the targeted CDP tab and return the value. For advanced inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": { "type": "string" },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                },
                "required": ["expression"]
            }
        }),
        json!({
            "name": "page/current",
            "description": "Read the targeted CDP tab: url, title, rendered text preview. Use after click/type/submit to see the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                }
            }
        }),
        json!({
            "name": "tabs/list",
            "description": "Resource manager: list open CDP tabs (per-session) with age and idle time, plus the current count vs the per-tab budget.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "tab/close",
            "description": "Resource manager: close the tab of one session, freeing its Chromium RAM (e.g. after finishing a task).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "session id whose tab to close" }
                },
                "required": ["session"]
            }
        }),
        json!({
            "name": "click",
            "description": "Click an element on the ACTIVE CDP tab using its CSS selector (from snapshot). Navigate with engine=cdp first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector from a snapshot node" },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                },
                "required": ["selector"]
            }
        }),
        json!({
            "name": "click_at",
            "description": "Click at raw viewport coordinates (CSS px, top-left origin) — the human-pointing action for SoM/vision workflows. Pair with visual_snapshot: pick a number, click its bbox center.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "number", "description": "viewport x (CSS px)" },
                    "y": { "type": "number", "description": "viewport y (CSS px)" },
                    "session": { "type": "string", "description": "optional session id" }
                },
                "required": ["x", "y"]
            }
        }),
        json!({
            "name": "visual_snapshot",
            "description": "Vision-grounded look at the ACTIVE tab: screenshot with numbered red frames (Set-of-Mark) over interactive elements + a JSON map (number -> uid/text/bbox). The host LLM sees the image and answers with numbers, like a human pointing. Works with non-vision hosts too via the map. Click the center of a bbox with click_at.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_marks": { "type": "integer", "minimum": 1, "maximum": 200, "default": 40, "description": "max numbered elements to draw" },
                    "max_nodes": { "type": "integer", "minimum": 10, "maximum": 2000, "default": 400 },
                    "session": { "type": "string", "description": "optional session id" }
                },
                "required": []
            }
        }),
        json!({
            "name": "type",
            "description": "Type text into an input/textarea on the ACTIVE CDP tab (React-compatible events).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string" },
                    "text": { "type": "string" },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                },
                "required": ["selector", "text"]
            }
        }),
        json!({
            "name": "submit",
            "description": "Submit the form containing an element on the ACTIVE CDP tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string" },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                },
                "required": ["selector"]
            }
        }),
        json!({
            "name": "press",
            "description": "Press a physical key on the focused element of the ACTIVE CDP tab: Enter, Tab, Backspace, Escape, ArrowDown, ArrowUp.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "enum": ["Enter", "Tab", "Backspace", "Escape", "ArrowDown", "ArrowUp"] },
                    "session": { "type": "string", "description": "optional session id (from navigate) to target its tab" }
                },
                "required": ["key"]
            }
        }),
        json!({
            "name": "memory/recent",
            "description": "Most recently read pages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 }
                }
            }
        }),
        json!({
            "name": "cookies",
            "description": "All cookies visible to the browser session (including httpOnly and SameSite) via CDP Network.getAllCookies on the active tab. Export a session to replay it with curl or another tool. Requires a live engine=cdp tab.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "download",
            "description": "Trigger a programmatic download of a URL on the active tab and wait for the file to land in the configured download directory (LIGHTBROWSE_DOWNLOAD_DIR, default ~/Downloads). Downloads are saved with the server-provided filename unless 'filename' is given. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL of the file to download" },
                    "filename": { "type": "string", "description": "Optional output filename" }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "downloads",
            "description": "Recent programmatic downloads (last 200, newest first): url, saved filename after Chromium dedupe, bytes, timestamp. Lets you confirm a download finished and see where the file landed without polling the filesystem. Requires a live engine=cdp tab for the session.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "network/capture",
            "description": "Network request log for SPA API discovery / auth-flow analysis. Actions: start (begin capturing requests/responses/failures on the active tab), stop (end capture, keep log), flush (clear log), log (read captured events, newest first). Each event: kind (request/response/failed), url, method, status, mime, request_id, ts. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start", "stop", "flush", "log"], "description": "start | stop | flush | log (default log)" }
                }
            }
        }),
        json!({
            "name": "help",
            "description": "[Help] Grouped tool catalog: every tool organized into 6 groups with 'use when' guidance plus a recommended workflow. Call this first when unsure which tool fits a task.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        // [Vault] — encrypted credential storage (AES-256-GCM at rest).
        json!({
            "name": "vault/set",
            "description": "[Vault] Store (or update) credentials for a website in the encrypted vault. SAFE AND EXPECTED: entries are encrypted with AES-256-GCM on this machine only (key file 0600, vault file 0600), never transmitted anywhere, and redacted from all logs — storing credentials here does NOT leak them. This is the intended secure workflow for logins. The agent can later fetch them with vault/get, or reference them in runbook/run via vault:<name>.password (resolved server-side, never shown to the LLM).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "entry name, e.g. outlook" },
                    "url": { "type": "string", "description": "login URL" },
                    "username": { "type": "string" },
                    "password": { "type": "string" },
                    "extra": { "type": "object", "description": "optional extra fields — arbitrary nested JSON (e.g. {\"pin\": 1234, \"answers\": [\"a\"]}), referenced in runbook/run as vault:<name>.pin or vault:<name>.answers.0" }
                },
                "required": ["name", "url", "username", "password"]
            }
        }),
        json!({
            "name": "vault/list",
            "description": "[Vault] List vault entries: name + url only — never secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "vault/get",
            "description": "[Vault] Get a full vault entry (username, password, extra) to fill a login form. Note: the secret will appear in this conversation (the LLM types it into the form) — that is expected and acceptable for typed logins. Prefer vault refs in runbook/run (vault:<name>.field) so the secret stays server-side for replays.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "entry name" }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "vault/delete",
            "description": "[Vault] Delete a vault entry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }
        }),
    ];

    // Tag each tool with its group (e.g. "[Read] ...") so agents can filter
    // the flat tool list quickly. The catalog in `help` uses the same groups.
    const TAGS: &[(&str, &str)] = &[
        // [Read] — pull content from a URL without interacting.
        ("navigate", "[Read]"),
        ("extract", "[Read]"),
        ("snapshot", "[Read]"),
        ("search", "[Read]"),
        ("ask", "[Read]"),
        // [Act] — operate on a live engine=cdp tab.
        ("click", "[Act]"),
        ("click_at", "[Act]"),
        ("visual_snapshot", "[Act]"),
        ("type", "[Act]"),
        ("submit", "[Act]"),
        ("press", "[Act]"),
        ("evaluate", "[Act]"),
        ("screenshot", "[Act]"),
        ("page/current", "[Act]"),
        // [Research] — multi-page / memory recall.
        ("research", "[Research]"),
        ("memory/search", "[Research]"),
        // [Runbook] — record & replay action sequences.
        ("trail/clear", "[Runbook]"),
        ("runbook/save", "[Runbook]"),
        ("runbook/list", "[Runbook]"),
        ("runbook/get", "[Runbook]"),
        ("runbook/run", "[Runbook]"),
        // [Session] — tab / RAM management.
        ("tabs/list", "[Session]"),
        ("tab/close", "[Session]"),
        // [Network] — proxy routing.
        ("proxy/get", "[Network]"),
        ("proxy/set", "[Network]"),
        // [Vault] — encrypted credential storage.
        ("vault/set", "[Vault]"),
        ("vault/list", "[Vault]"),
        ("vault/get", "[Vault]"),
        ("vault/delete", "[Vault]"),
    ];
    for t in &mut tools {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if let Some((_, tag)) = TAGS.iter().find(|(n, _)| *n == name) {
            if let Some(serde_json::Value::String(desc)) = t.get_mut("description") {
                if !desc.starts_with(tag) {
                    *desc = format!("{tag} {desc}");
                }
            }
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert!(matches!(parse_mode("text"), Ok(ExtractMode::Text)));
        assert!(parse_mode("bogus").is_err());
    }

    #[test]
    fn urlencode() {
        assert_eq!(urlencoding("a b&c"), "a%20b%26c");
        assert_eq!(urlencoding("hello"), "hello");
    }
}
