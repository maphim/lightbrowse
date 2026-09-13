//! Minimal, dependency-light MCP (Model Context Protocol) server.
//!
//! Speaks the MCP stdio transport (JSON-RPC 2.0 over stdin/stdout) directly,
//! so an LLM host (Claude Desktop, pi, Cursor, ...) can drive lightbrowse
//! through four tools: `navigate`, `extract`, `snapshot`, `search`.
//!
//! Protocol version: 2025-06-18 (stable MCP revision).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::Engine;
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::reduce::{
    estimate_tokens, fingerprint, prune_snapshot, reduce_text, ObservationKind, ReduceConfig,
};
use lightbrowse_core::session::Session;
use lightbrowse_core::snapshot::{self, SnapshotOptions, SnapshotTree};
use lightbrowse_core::vision;
use lightbrowse_memory::{navigate_cached, ArtifactRecord, ArtifactStore, MemoryStore};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const TEXT_PREVIEW_CHARS: usize = 4000;
/// Default per-response token budget for tool output (`0` disables).
pub const DEFAULT_MAX_TOKENS: usize = 1000;
/// Only responses long enough to matter are reduced.
const BUDGETABLE_TOOLS: &[&str] = &[
    "navigate",
    "extract",
    "ask",
    "snapshot",
    "visual_snapshot",
    "page/current",
    "evaluate",
    "search",
    "research",
    "memory/search",
];
/// Strings shorter than this are never reduced on their own.
const MIN_STRING_TOKENS: usize = 128;

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
    /// Token budget for tool output. `0` disables reduction entirely.
    pub max_tokens: usize,
    /// Raw-artifact store backing the reduction. Reduction is only applied when
    /// a store is present, so a bounded projection always has a handle to the
    /// complete response.
    pub artifacts: Option<Arc<ArtifactStore>>,
    /// `tools/list` sends compact schemas (names + arg names) instead of the
    /// full JSON Schema; `tool/inspect` returns the complete definition.
    pub lazy_tools: bool,
    /// Last snapshot fingerprint per URL, for unchanged-page deltas.
    pub snapshot_fingerprints: Arc<Mutex<HashMap<String, String>>>,
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
                max_tokens: DEFAULT_MAX_TOKENS,
                artifacts: None,
                lazy_tools: true,
                snapshot_fingerprints: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    /// Toggle compact `tools/list` schemas (default on).
    pub fn with_lazy_tools(mut self, lazy: bool) -> Self {
        self.state.lazy_tools = lazy;
        self
    }

    /// Set the per-response token budget (`0` disables reduction).
    pub fn with_budget(mut self, max_tokens: usize) -> Self {
        self.state.max_tokens = max_tokens;
        self
    }

    /// Attach the raw-artifact store. Without it no output is ever reduced —
    /// a bounded projection must always be recoverable via `artifact/read`.
    pub fn with_artifacts(mut self, artifacts: Arc<ArtifactStore>) -> Self {
        self.state.artifacts = Some(artifacts);
        self
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
                        "description": "Featherweight browser MCP. 35 tools in 7 groups: [Read] fetch/extract/snapshot/search/ask — [Act] click/click_at/visual_snapshot/type/login/submit/press/evaluate/screenshot/page/current on live CDP tabs — [Download] download/downloads — [Research] research/memory/search — [Runbook] trail/clear + runbook/* — [Session] tabs/list + tab/close — [Network] proxy/get + proxy/set + network/capture + cookies — [Vault] vault/set + vault/list + vault/get + vault/delete (encrypted credentials). engine=auto picks fetch first, falls back to headless Chromium; engine=cdp keeps a live tab for actions. visual_snapshot = SoM numbered overlay for human-like vision agents; click_at = coordinate click; login = one-call username+password fill; fill_form = one-call any-form/survey fill (auto test data). Call the 'help' tool for the grouped catalog with use-cases."
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
            "tools/list" => {
                let tools = if self.state.lazy_tools {
                    compact_tools_schema()
                } else {
                    tools_schema()
                };
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": tools }
                }))
            }
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
                let result = result.map(|text| self.bound_output(&name, text));
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
                let session = require_session(cdp, args).await?;
                // A cookie value is a session secret: redact by default and
                // reveal it only when the caller explicitly opts in.
                let include_values = args
                    .get("include_values")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let v = cdp
                    .cookies(session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let arr = v.as_array().cloned().unwrap_or_default();
                let cookies: Vec<Value> =
                    arr.iter().map(|c| cookie_view(c, include_values)).collect();
                Ok(pretty(&json!({
                    "count": cookies.len(),
                    "include_values": include_values,
                    "session": session,
                    "cookies": cookies
                })))
            }
            "download" => {
                let cdp = require_cdp(&s)?;
                let session = require_session(cdp, args).await?;
                let url = req_str(args, "url")?;
                let filename = opt_str(args, "filename");
                let v = cdp
                    .download(&url, filename.as_deref(), session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&v))
            }
            "downloads" => {
                let cdp = require_cdp(&s)?;
                // Process-global audit list; an optional `session` filters it.
                let session = opt_str(args, "session");
                let all = cdp.downloads();
                let items: Vec<Value> = match &session {
                    Some(s) => all
                        .into_iter()
                        .filter(|d| d.get("session").and_then(|v| v.as_str()) == Some(s.as_str()))
                        .collect(),
                    None => all,
                };
                Ok(pretty(&json!({
                    "count": items.len(),
                    "session": session,
                    "downloads": items
                })))
            }
            "network/capture" => {
                let cdp = require_cdp(&s)?;
                let action = args.get("action").and_then(|a| a.as_str()).unwrap_or("log");
                match action {
                    // start/stop attach capture to a specific tab, so they
                    // follow the same >1-tab rule as cookies/download.
                    "start" => {
                        let session = require_session(cdp, args).await?;
                        Ok(pretty(
                            &cdp.network_capture(true, session.as_deref())
                                .await
                                .map_err(|e| e.to_string())?,
                        ))
                    }
                    "stop" => {
                        let session = require_session(cdp, args).await?;
                        Ok(pretty(
                            &cdp.network_capture(false, session.as_deref())
                                .await
                                .map_err(|e| e.to_string())?,
                        ))
                    }
                    "flush" => {
                        let session = require_session(cdp, args).await?;
                        cdp.network_log_clear(session.as_deref());
                        Ok(pretty(&json!({ "cleared": true, "session": session })))
                    }
                    "log" => {
                        let session = require_session(cdp, args).await?;
                        let events = cdp.network_log(session.as_deref());
                        let capturing = cdp.network_capturing(session.as_deref());
                        Ok(pretty(
                            &json!({ "capturing": capturing, "count": events.len(), "events": events, "session": session }),
                        ))
                    }
                    other => Err(format!(
                        "network/capture action must be start|stop|flush|log, got {other}"
                    )),
                }
            }
            "artifact/read" => {
                let store = s.artifacts.as_ref().ok_or("artifact store disabled")?;
                let id = req_str(args, "id")?;
                let record = store
                    .get(&id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("artifact '{id}' not found or expired"))?;
                let text = record.text();
                let total_chars = text.chars().count();
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let max_chars = args
                    .get("max_chars")
                    .and_then(|v| v.as_u64())
                    .map(|v| (v as usize).clamp(1, 5_000_000));
                let body: String = match (offset, max_chars) {
                    (0, None) => text.clone(),
                    (offset, max) => text
                        .chars()
                        .skip(offset)
                        .take(max.unwrap_or_else(|| total_chars.saturating_sub(offset)))
                        .collect(),
                };
                Ok(pretty(&json!({
                    "id": record.id,
                    "tool": record.tool,
                    "kind": record.kind,
                    "content_type": record.content_type,
                    "url": record.url,
                    "tokens": record.tokens,
                    "created_at": record.created_at,
                    "expires_at": record.expires_at,
                    "offset": offset,
                    "returned_chars": body.chars().count(),
                    "total_chars": total_chars,
                    "text": body,
                })))
            }
            "artifact/ask" => {
                let store = s.artifacts.as_ref().ok_or("artifact store disabled")?;
                let id = req_str(args, "id")?;
                let question = req_str(args, "question")?;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(8)
                    .clamp(1, 50) as usize;
                let record = store
                    .get(&id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("artifact '{id}' not found or expired"))?;
                let hits = ask_passages(&record.text(), &question, limit);
                Ok(pretty(&json!({
                    "id": id,
                    "question": question,
                    "hits": hits,
                    "note": "passages ranked from the stored artifact text (no model call)",
                })))
            }
            "artifact/list" => {
                let store = s.artifacts.as_ref().ok_or("artifact store disabled")?;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                let tool = opt_str(args, "tool");
                let items = store
                    .list(limit, tool.as_deref())
                    .map_err(|e| e.to_string())?;
                let (stored_count, stored_bytes) = store.stats().map_err(|e| e.to_string())?;
                Ok(pretty(&json!({
                    "artifacts": items,
                    "count": items.len(),
                    "stored_count": stored_count,
                    "stored_bytes": stored_bytes,
                })))
            }
            "tool/inspect" => {
                let full = tools_schema();
                match opt_str(args, "name") {
                    Some(name) => {
                        let tool = full
                            .into_iter()
                            .find(|tool| tool["name"].as_str() == Some(name.as_str()))
                            .ok_or_else(|| format!("unknown tool '{name}'"))?;
                        Ok(pretty(&json!({ "tool": tool, "lazy_tools": s.lazy_tools })))
                    }
                    None => {
                        let index: Vec<Value> = full
                            .iter()
                            .map(|tool| {
                                json!({
                                    "name": tool["name"],
                                    "summary": first_sentence(tool["description"].as_str().unwrap_or("")),
                                })
                            })
                            .collect();
                        Ok(pretty(&json!({
                            "count": index.len(),
                            "tools": index,
                            "lazy_tools": s.lazy_tools,
                        })))
                    }
                }
            }
            "help" => Ok(pretty(&json!({
                "about": "lightbrowse — featherweight browser MCP. 35 tools in 7 groups.",
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
                        "tools": ["click", "click_at", "visual_snapshot", "type", "login", "fill_form", "submit", "press", "evaluate", "screenshot", "page/current"]
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
                let payload = serde_json::to_value(tree).map_err(|e| e.to_string())?;
                let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                if !force {
                    let cfg = ReduceConfig::with_max_tokens(s.max_tokens);
                    if let Some(delta) = snapshot_delta(
                        &s.snapshot_fingerprints,
                        &page.url,
                        &payload,
                        cfg.max_tokens,
                        s.artifacts.as_deref(),
                    ) {
                        return Ok(pretty(&delta));
                    }
                }
                Ok(pretty(&payload))
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
                        let mut parsed: Value =
                            serde_json::from_str(&steps).map_err(|e| e.to_string())?;
                        redact_secret_steps(&mut parsed);
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
                let shot = std::env::temp_dir().join(format!("lb-shot-{}.png", std::process::id()));
                let shot_path = cdp
                    .screenshot(&shot, false, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                let png = std::fs::read(&shot_path).map_err(|e| e.to_string())?;
                let marks = vision::select_marks(&tree, max_marks);
                let som_marks: Vec<vision::Mark> = marks
                    .iter()
                    .map(|(label, _, _, b)| vision::Mark {
                        label: *label,
                        bbox: *b,
                    })
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
                let b64 =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &overlaid);
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
            "login" => {
                let mut username = req_str(args, "username")?;
                let mut password = req_str(args, "password")?;
                let save_vault = args
                    .get("save_vault")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let vault_name = opt_str(args, "vault_name");
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");

                // Resolve vault:<name>.<field> refs server-side so the real
                // secret is typed AND re-savable, never shown to the LLM.
                if let Some(vault) = &s.vault {
                    if password.starts_with("vault:") {
                        password = match vault.resolve_ref(&password) {
                            Some(Ok(v)) => v,
                            Some(Err(e)) => return Err(e),
                            None => return Err(format!("unknown vault ref: {password}")),
                        };
                    }
                    if username.starts_with("vault:") {
                        username = match vault.resolve_ref(&username) {
                            Some(Ok(v)) => v,
                            Some(Err(e)) => return Err(e),
                            None => return Err(format!("unknown vault ref: {username}")),
                        };
                    }
                }

                let res = cdp
                    .fill_login(&username, &password, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                if res.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                    return Ok(pretty(&res));
                }

                // Auto-save on detected success.
                let mut saved = json!(null);
                let mut runbook_saved = json!(null);
                let mut probe = json!(null);
                if save_vault {
                    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
                    probe = cdp
                        .login_success_probe(session.as_deref())
                        .await
                        .map_err(|e| e.to_string())?;
                    let detected = probe
                        .get("detected")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if detected {
                        let url = probe.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        let name = vault_name.unwrap_or_else(|| {
                            url::Url::parse(url)
                                .ok()
                                .and_then(|u| u.host_str().map(|h| h.to_string()))
                                .unwrap_or_else(|| "saved-login".into())
                        });
                        if let Some(vault) = &s.vault {
                            let entry = lightbrowse_core::vault::VaultEntry {
                                url: url.to_string(),
                                username: username.clone(),
                                password: password.clone(),
                                extra: serde_json::json!({"source": "login-auto-save"}),
                                updated_at: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0),
                            };
                            match vault.set(&name, entry) {
                                Ok(()) => saved = json!(name),
                                Err(e) => return Err(e),
                            }
                        } else {
                            return Err("vault unavailable — start with a vault path".into());
                        }
                        // Auto-save a replayable runbook of the login steps.
                        let trail = cdp.trail();
                        if !trail.is_empty() {
                            if let Some(m) = &s.memory {
                                let steps_json =
                                    serde_json::to_string(&trail).map_err(|e| e.to_string())?;
                                let rb_name = format!("login-{name}");
                                if m.save_runbook(&rb_name, url, &steps_json).is_ok() {
                                    runbook_saved = json!(rb_name);
                                }
                            }
                        }
                    }
                }

                Ok(pretty(&json!({
                    "login": res,
                    "probe": probe,
                    "vault_saved": saved,
                    "runbook_saved": runbook_saved,
                })))
            }
            "fill_form" => {
                let values = args
                    .get("values")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                let auto = args.get("auto").and_then(|v| v.as_bool()).unwrap_or(true);
                let submit = args
                    .get("submit")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let cdp = require_cdp(&s)?;
                let session = opt_str(args, "session");
                let res = cdp
                    .fill_form(&values, auto, submit, session.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&res))
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

/// Resolve the session for a stateful, tab-scoped tool. With more than one tab
/// open an explicit session is REQUIRED, so a call can never silently act on
/// the most-recently-used tab (e.g. leaking another tab's cookies).
fn resolve_session(explicit: Option<String>, tab_count: usize) -> Result<Option<String>, String> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    if tab_count > 1 {
        return Err(format!(
            "{tab_count} tabs are open — pass an explicit \"session\" (see tabs/list) so this call cannot act on the wrong tab"
        ));
    }
    Ok(None)
}

async fn require_session(
    cdp: &lightbrowse_cdp::CdpBackend,
    args: &Map<String, Value>,
) -> Result<Option<String>, String> {
    resolve_session(opt_str(args, "session"), cdp.tab_count().await)
}

/// A cookie view for tool output. Cookie values are session secrets and are
/// `null` unless the caller explicitly opts in with `include_values: true`.
fn cookie_view(c: &Value, include_values: bool) -> Value {
    json!({
        "name": c.get("name"),
        "value": if include_values {
            c.get("value").cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        },
        "domain": c.get("domain"),
        "path": c.get("path"),
        "httpOnly": c.get("httpOnly"),
        "secure": c.get("secure"),
        "sameSite": c.get("sameSite"),
        "expires": c.get("expires")
    })
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

/// First sentence of a description, capped — used by the compact tool index.
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let cut = text
        .find(". ")
        .map(|index| index + 1)
        .unwrap_or(text.len())
        .min(48);
    let mut sentence: String = text.chars().take(cut).collect();
    if sentence.chars().count() < text.chars().count() && !sentence.ends_with('.') {
        sentence.push('…');
    }
    sentence
}

/// `url, engine?, max_nodes?` — enough for an agent to call the tool without
/// shipping the full JSON Schema.
fn arg_signature(tool: &Value) -> String {
    let Some(props) = tool["inputSchema"]["properties"].as_object() else {
        return String::new();
    };
    let required: Vec<&str> = tool["inputSchema"]["required"]
        .as_array()
        .map(|items| items.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut shown: Vec<String> = Vec::new();
    for name in &required {
        if props.contains_key(*name) {
            shown.push((*name).to_string());
        }
    }
    for name in props.keys() {
        if !required.contains(&name.as_str()) {
            shown.push(format!("{name}?"));
        }
    }
    let extra = shown.len().saturating_sub(4);
    shown.truncate(4);
    let mut signature = shown.join(", ");
    if extra > 0 {
        signature.push_str(&format!(", +{extra} more"));
    }
    signature
}

/// Property names + types (+ short enums) without the prose.
fn compact_properties(tool: &Value) -> Value {
    let Some(props) = tool["inputSchema"]["properties"].as_object() else {
        return json!({});
    };
    let mut out = Map::new();
    for (name, spec) in props {
        let mut compact = Map::new();
        if let Some(kind) = spec.get("type").and_then(|v| v.as_str()) {
            compact.insert("type".to_string(), json!(kind));
        }
        match spec.get("enum").and_then(|v| v.as_array()) {
            Some(values)
                if values.len() <= 6
                    && serde_json::to_string(values).is_ok_and(|s| s.len() <= 40) =>
            {
                compact.insert("enum".to_string(), json!(values));
            }
            _ => {}
        }
        if spec.get("type").and_then(|v| v.as_str()) == Some("array") {
            if let Some(item_type) = spec
                .get("items")
                .and_then(|items| items.get("type"))
                .and_then(|v| v.as_str())
            {
                compact.insert("items".to_string(), json!({ "type": item_type }));
            }
        }
        out.insert(name.clone(), Value::Object(compact));
    }
    Value::Object(out)
}

/// `tools/list` payload with descriptions trimmed to one line and schemas
/// reduced to arg names/types. The full definition stays available through
/// `tool/inspect`, so nothing is lost — it is just not paid for at session start.
fn compact_tools_schema() -> Vec<Value> {
    tools_schema()
        .iter()
        .map(|tool| {
            let signature = arg_signature(tool);
            // The `[Group] ` prefix is cosmetic in the compact index — the name
            // already says what the tool does; `tool/inspect` keeps the full text.
            let raw = tool["description"].as_str().unwrap_or("");
            let raw = match raw.strip_prefix('[') {
                Some(rest) => rest.split_once("] ").map(|(_, text)| text).unwrap_or(raw),
                None => raw,
            };
            let description = first_sentence(raw);
            let description = if signature.is_empty() {
                description
            } else {
                format!("{description} args({signature})")
            };
            let properties = compact_properties(tool);
            let mut schema = Map::new();
            let has_properties = properties.as_object().is_some_and(|p| !p.is_empty());
            if has_properties {
                // `type` is only needed when there is something to describe; a
                // no-argument tool ships `{}` (still a valid JSON Schema).
                // `required` is deliberately omitted: required-ness is already in
                // the `args(name, other?)` signature and in `tool/inspect`.
                schema.insert("type".to_string(), json!("object"));
                schema.insert("properties".to_string(), properties);
            }
            json!({
                "name": tool["name"],
                "description": description,
                "inputSchema": Value::Object(schema),
            })
        })
        .collect()
}

/// Tiny response for a snapshot that is unchanged since the previous one for
/// the same URL. Only kicks in when the tree is big enough to be worth
/// reducing; a small tree is simply sent again. The tree is stored as an
/// artifact so the delta is never a dead end.
fn snapshot_delta(
    cache: &Mutex<HashMap<String, String>>,
    url: &str,
    tree: &Value,
    max_tokens: usize,
    artifacts: Option<&ArtifactStore>,
) -> Option<Value> {
    if max_tokens == 0 || estimate_tokens(&tree.to_string()) <= max_tokens {
        return None;
    }
    let current = fingerprint(tree.to_string().as_bytes());
    let previous = cache
        .lock()
        .unwrap()
        .insert(url.to_string(), current.clone());
    if previous.as_deref() != Some(current.as_str()) {
        return None;
    }
    let mut delta = json!({
        "url": url,
        "unchanged": true,
        "fingerprint": current,
        "note": "page unchanged since the previous snapshot for this URL — pass force:true for the tree, or artifact/read for the stored copy",
    });
    if let Some(store) = artifacts {
        let record = ArtifactRecord::new(
            "snapshot",
            ObservationKind::BrowserTree.as_str(),
            "application/json",
            Some(url.to_string()),
            tree.to_string().into_bytes(),
            store.config().ttl_secs,
        );
        let id = record.id.clone();
        if matches!(store.put(record), Ok(true)) {
            delta["artifact"] = json!(id);
        }
    }
    Some(delta)
}

/// Tools whose output is worth bounding. Deliberately an allow-list: control,
/// credential and form-filling tools (`vault/*`, `login`, `type`, `fill_form`,
/// `cookies`, `runbook/*`) must never have their payload rewritten or stored.
fn is_budgetable(tool: &str) -> bool {
    BUDGETABLE_TOOLS.contains(&tool)
}

/// Collect JSON pointers of every long string value (recursively).
fn collect_long_strings(value: &Value, path: &str, out: &mut Vec<(String, usize)>) {
    match value {
        Value::String(text) => {
            let tokens = estimate_tokens(text);
            if tokens > MIN_STRING_TOKENS {
                out.push((path.to_string(), tokens));
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                collect_long_strings(child, &format!("{path}/{escaped}"), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_long_strings(child, &format!("{path}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// Collapse oversized arrays: keep the leading items that fit a share of the
/// budget and append a marker, so `extract --mode links` on a 900-link page (or
/// a 400-block article) cannot blow the context on its own.
fn collapse_arrays(value: &mut Value, budget: usize) -> bool {
    let share = (budget / 2).max(128);
    let mut collapsed = false;
    collapse_arrays_in(value, share, &mut collapsed);
    collapsed
}

fn collapse_arrays_in(value: &mut Value, share: usize, collapsed: &mut bool) {
    match value {
        Value::Object(map) => {
            for child in map.values_mut() {
                if let Value::Array(items) = child {
                    let total: usize = items
                        .iter()
                        .map(|item| estimate_tokens(&item.to_string()).max(1))
                        .sum();
                    if items.len() > 8 && total > share {
                        let mut used = 0usize;
                        let mut keep = 0usize;
                        for item in items.iter() {
                            let tokens = estimate_tokens(&item.to_string()).max(1);
                            if used + tokens > share {
                                break;
                            }
                            used += tokens;
                            keep += 1;
                        }
                        let dropped = items.len() - keep;
                        items.truncate(keep);
                        items.push(Value::String(format!(
                            "… {dropped} more items omitted (see reduction.artifact for the complete response)"
                        )));
                        *collapsed = true;
                    }
                }
                collapse_arrays_in(child, share, collapsed);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                collapse_arrays_in(child, share, collapsed);
            }
        }
        _ => {}
    }
}

/// Reduce a JSON payload in place, keeping it valid JSON.
///
/// Trees are pruned structurally (so a kept button keeps its ancestors); other
/// payloads have their largest long string fields projected. Returns the
/// strategy that was applied, or `None` when nothing was reduced.
fn compact_payload(
    payload: &str,
    kind: ObservationKind,
    cfg: &ReduceConfig,
) -> Option<(Value, lightbrowse_core::reduce::ReduceStrategy)> {
    let mut value: Value = serde_json::from_str(payload).ok()?;

    if kind == ObservationKind::BrowserTree {
        if let Ok(mut tree) = serde_json::from_value::<SnapshotTree>(value.clone()) {
            let stats = prune_snapshot(&mut tree, cfg);
            if !stats.truncated {
                return None;
            }
            return Some((serde_json::to_value(&tree).ok()?, stats.strategy));
        }
    }

    // Structural first: dropping 400 repeated array items saves more than any
    // string projection, and keeps the remaining items untouched.
    let arrays_collapsed = collapse_arrays(&mut value, cfg.max_tokens);

    let mut pointers: Vec<(String, usize)> = Vec::new();
    collect_long_strings(&value, "", &mut pointers);
    if pointers.is_empty() && !arrays_collapsed {
        return None;
    }
    // Biggest fields first; three is enough to cover the shapes we emit.
    pointers.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    pointers.truncate(3);
    let per_field = (cfg.max_tokens / pointers.len().max(1)).max(256);
    let field_cfg = ReduceConfig::with_max_tokens(per_field);

    let mut strategy = lightbrowse_core::reduce::ReduceStrategy::ArrayCollapsed;
    let mut changed = arrays_collapsed;
    for (pointer, _) in &pointers {
        let Some(Value::String(text)) = value.pointer_mut(pointer) else {
            continue;
        };
        let reduction = reduce_text(text, kind, &field_cfg);
        if reduction.truncated {
            *text = reduction.text.clone();
            strategy = reduction.strategy;
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    Some((value, strategy))
}

/// Rank stored-artifact lines against a question. Deterministic, no model call.
fn ask_passages(text: &str, question: &str, limit: usize) -> Vec<Value> {
    let terms: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.len() > 1)
        .map(|term| term.to_string())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut scored: Vec<(usize, f64)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let mut score = 0.0;
        for term in &terms {
            if lower.contains(term.as_str()) {
                score += 1.0;
            }
        }
        // Prefer lines that answer more than one term, and non-trivial lines.
        if score > 0.0 {
            score += (lower.matches(char::is_whitespace).count().min(40) as f64) / 40.0;
            scored.push((index, score));
        }
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.truncate(limit.clamp(1, 50));
    scored
        .into_iter()
        .map(|(index, score)| {
            json!({
                "line": index + 1,
                "score": (score * 100.0).round() / 100.0,
                "text": lines[index],
                "before": if index > 0 { lines[index - 1] } else { "" },
                "after": lines.get(index + 1).copied().unwrap_or(""),
            })
        })
        .collect()
}

/// Bound a payload to `cfg.max_tokens`, storing the complete version as an
/// artifact first.
///
/// Returns the payload unchanged when: the tool is not allow-listed, the budget
/// is disabled, the payload already fits, nothing could be reduced, or the
/// artifact could not be stored. The last case matters — a lossy answer with no
/// handle is worse than a large answer.
fn bound_payload(tool: &str, payload: String, cfg: &ReduceConfig, store: &ArtifactStore) -> String {
    {
        if cfg.max_tokens == 0 || !is_budgetable(tool) {
            return payload;
        }
        let original_tokens = estimate_tokens(&payload);
        if original_tokens <= cfg.max_tokens {
            return payload;
        }
        let kind = ObservationKind::classify(tool, "");
        let Some((mut compacted, strategy)) = compact_payload(&payload, kind, cfg) else {
            return payload;
        };
        let record = ArtifactRecord::new(
            tool,
            kind.as_str(),
            "application/json",
            None,
            payload.clone().into_bytes(),
            store.config().ttl_secs,
        );
        let id = record.id.clone();
        let expires_at = record.expires_at;
        if !matches!(store.put(record), Ok(true)) {
            return payload;
        }
        let reduced_tokens = estimate_tokens(&compacted.to_string());
        if reduced_tokens >= original_tokens {
            return payload;
        }
        if let Value::Object(map) = &mut compacted {
            map.insert(
                "reduction".to_string(),
                json!({
                    "strategy": strategy,
                    "original_tokens": original_tokens,
                    "reduced_tokens": reduced_tokens,
                    "saved_tokens": original_tokens.saturating_sub(reduced_tokens),
                    "artifact": id,
                    "expires_at": expires_at,
                    "marker": format!(
                        "[reduced {original_tokens}→{reduced_tokens} tokens · full response: artifact/read id={id}]"
                    ),
                }),
            );
        }
        compacted.to_string()
    }
}

impl McpServer {
    /// Bound a tool response to `state.max_tokens`.
    ///
    /// Only called for allow-listed read tools, and only when an artifact store
    /// is attached: the complete payload is stored first, so a reduced response
    /// is always recoverable through `artifact/read`.
    fn bound_output(&self, tool: &str, payload: String) -> String {
        match self.state.artifacts.as_ref() {
            Some(store) => bound_payload(
                tool,
                payload,
                &ReduceConfig::with_max_tokens(self.state.max_tokens),
                store,
            ),
            None => payload,
        }
    }
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
                    "force": { "type": "boolean", "description": "Return the tree even if the page is unchanged since the previous snapshot." },
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
            "name": "fill_form",
            "description": "Fill ANY form/survey like a human, in one call: enumerates all editable fields (inputs/selects/textareas/checkboxes/radios with labels), matches your values by label/name/id/placeholder, auto-generates sensible test data for the rest (auto=true), optionally submits. values is a JSON object of field label-or-name to value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "values": { "type": "object", "description": "field label/name/id/placeholder -> value", "additionalProperties": { "type": "string" } },
                    "auto": { "type": "boolean", "default": true, "description": "fill unmatched fields with generated test data" },
                    "submit": { "type": "boolean", "default": false, "description": "click the submit/register button after filling" },
                    "session": { "type": "string", "description": "optional session id" }
                },
                "required": ["values"]
            }
        }),
        json!({
            "name": "login",
            "description": "ONE-CALL login: detect username+password fields on the current page, fill both, submit. With save_vault=true (default), waits ~2.5s after submit and AUTO-SAVES credentials to the encrypted vault when login success is detected (page left the login URL / logged-in indicator appeared). Password/username may reference the vault as vault:<name>.field (resolved server-side, never in context).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "username": { "type": "string", "description": "username / email / phone, or vault:<name>.field" },
                    "password": { "type": "string", "description": "password or vault:<name>.field" },
                    "save_vault": { "type": "boolean", "default": true, "description": "auto-save to vault on detected login success" },
                    "vault_name": { "type": "string", "description": "vault entry name (default: hostname, e.g. voz.vn)" },
                    "session": { "type": "string", "description": "optional session id" }
                },
                "required": ["username", "password"]
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
            "description": "Cookies visible to the browser session (including httpOnly and SameSite) via CDP Network.getAllCookies on the active tab. Values are REDACTED (null) by default — pass include_values:true only when the secrets are actually needed. When more than one tab is open, 'session' is required. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Tab session id from tabs/list (required when >1 tab is open)" },
                    "include_values": { "type": "boolean", "default": false, "description": "Reveal cookie values (secrets). Default false." }
                }
            }
        }),
        json!({
            "name": "download",
            "description": "Trigger a programmatic download of a URL on the active tab and wait for the file to land in the configured download directory (LIGHTBROWSE_DOWNLOAD_DIR, default ~/Downloads). Downloads are saved with the server-provided filename unless 'filename' is given. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL of the file to download" },
                    "filename": { "type": "string", "description": "Optional output filename" },
                    "session": { "type": "string", "description": "Tab session id from tabs/list (required when >1 tab is open)" }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "downloads",
            "description": "Recent programmatic downloads (last 200, newest first). This is a PROCESS-GLOBAL audit list — every session's downloads share it. Each record: url, saved (final filename after Chromium dedupe), bytes, ts, dir, and the originating `session`. Pass an optional 'session' to filter to one tab. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Optional filter: only downloads started by this tab session (see tabs/list)" }
                }
            }
        }),
        json!({
            "name": "network/capture",
            "description": "Per-session network request log for SPA API discovery / auth-flow analysis. Actions: start (begin capturing on the given session's tab), stop (end that session's capture, keep its log), flush (clear that session's log), log (read that session's events, newest first). Capture state is PER SESSION: one tab cannot stop or read another tab's capture. Each event: kind (request/response/failed), url, method, status, mime, request_id, ts, session. When more than one tab is open, 'session' is required. Requires a live engine=cdp tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start", "stop", "flush", "log"], "description": "start | stop | flush | log (default log)" },
                    "session": { "type": "string", "description": "Tab session id from tabs/list (required when >1 tab is open)" }
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
        // [Artifact] — expand a reduced response again.
        json!({
            "name": "artifact/read",
            "description": "[Artifact] Read the complete payload behind a reduced tool response. Responses carrying a `reduction.artifact` id were bounded to save tokens; call this with that id to get the exact original JSON. Optional `offset`/`max_chars` read a slice.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "artifact id from `reduction.artifact` (obs_...)" },
                    "offset": { "type": "integer", "description": "character offset to start at", "minimum": 0 },
                    "max_chars": { "type": "integer", "description": "maximum characters to return", "minimum": 1 }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "artifact/ask",
            "description": "[Artifact] Ask a question about a stored artifact: returns the best-matching passages (line + context) ranked deterministically, without re-fetching the page or calling a model.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "artifact id (obs_...)" },
                    "question": { "type": "string", "description": "what to look for" },
                    "limit": { "type": "integer", "description": "max passages (default 8, max 50)", "minimum": 1, "maximum": 50 }
                },
                "required": ["id", "question"]
            }
        }),
        json!({
            "name": "artifact/list",
            "description": "[Artifact] List stored artifacts (newest first) with tool, kind, tokens and expiry, plus the store totals. Useful to check what is still expandable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "max entries (default 20, max 200)", "minimum": 1, "maximum": 200 },
                    "tool": { "type": "string", "description": "only artifacts produced by this tool" }
                }
            }
        }),
        // [Meta] — compact tool index + on-demand schemas.
        json!({
            "name": "tool/inspect",
            "description": "[Meta] Return the full JSON Schema of a tool (or the compact index of every tool when `name` is omitted). Use it when the compact `tools/list` signature is not enough — parameter descriptions, defaults and long enums live here.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "tool name, e.g. navigate" }
                }
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
        ("login", "[Act]"),
        ("fill_form", "[Act]"),
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
        ("artifact/read", "[Artifact]"),
        ("artifact/ask", "[Artifact]"),
        ("artifact/list", "[Artifact]"),
        ("tool/inspect", "[Meta]"),
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

/// Defense in depth for `runbook/get`: a step flagged `secret` never returns a
/// literal, even if an older or imported runbook somehow still carries one.
fn redact_secret_steps(steps: &mut Value) {
    let Some(arr) = steps.as_array_mut() else {
        return;
    };
    for step in arr.iter_mut() {
        let is_secret = step
            .get("secret")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_secret {
            if let Some(obj) = step.as_object_mut() {
                obj.insert("text".into(), Value::Null);
            }
        }
    }
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

    #[test]
    fn runbook_get_redacts_secret_steps() {
        let mut steps = serde_json::json!([
            {"action": "type", "selector": "input.user", "text": "alice", "secret": false},
            {"action": "type", "selector": "input.pass", "text": "leaked-pw", "secret": true,
             "secret_ref": "{{PASSWORD}}"}
        ]);
        redact_secret_steps(&mut steps);
        assert_eq!(steps[0]["text"], "alice");
        assert_eq!(steps[1]["text"], serde_json::Value::Null);
        assert_eq!(steps[1]["secret_ref"], "{{PASSWORD}}");
    }

    #[test]
    fn session_required_with_multiple_tabs() {
        // 0/1 tab → the implicit active tab is unambiguous.
        assert_eq!(resolve_session(None, 0).unwrap(), None);
        assert_eq!(resolve_session(None, 1).unwrap(), None);
        // >1 tab → an explicit session is required.
        let err = resolve_session(None, 3).unwrap_err();
        assert!(err.contains("3 tabs"), "{err}");
        assert!(err.contains("session"), "{err}");
        // An explicit session always wins.
        assert_eq!(
            resolve_session(Some("t2".into()), 5).unwrap().as_deref(),
            Some("t2")
        );
    }

    #[test]
    fn cookie_values_are_redacted_by_default() {
        let c = serde_json::json!({
            "name": "SESSIONID", "value": "super-secret", "domain": "x.test",
            "path": "/", "httpOnly": true, "secure": true, "sameSite": "Lax", "expires": 123.0
        });
        let redacted = cookie_view(&c, false);
        assert_eq!(redacted["name"], "SESSIONID");
        assert_eq!(redacted["value"], Value::Null);
        assert!(!redacted.to_string().contains("super-secret"));
        let revealed = cookie_view(&c, true);
        assert_eq!(revealed["value"], "super-secret");
    }

    /// Live two-tab enforcement: with 2 tabs open, every tab-scoped tool must
    /// reject a missing session instead of silently using the MRU tab. Runs in
    /// CI via `cargo test -p lightbrowse-mcp -- --include-ignored`.
    #[tokio::test]
    #[ignore = "requires Chrome + network"]
    #[cfg_attr(windows, ignore = "requires Chrome + network fixtures")]
    async fn call_tool_requires_session_with_two_tabs() {
        let config = lightbrowse_core::config::Config {
            memory_budget_mb: 8192,
            max_tabs: 8,
            ..Default::default()
        };
        let backend = Arc::new(lightbrowse_cdp::CdpBackend::new(config));
        let sa = Session::new();
        let sb = Session::new();
        backend
            .navigate(&sa, "https://example.com/?a")
            .await
            .unwrap();
        backend
            .navigate(&sb, "https://example.com/?b")
            .await
            .unwrap();
        assert_eq!(backend.tab_count().await, 2, "two tabs expected");

        let server = McpServer {
            state: McpState {
                backend: backend.clone(),
                cdp: Some(backend.clone()),
                session: Arc::new(Mutex::new(Session::new())),
                engine: Engine::Cdp,
                memory: None,
                vault: None,
                max_tokens: DEFAULT_MAX_TOKENS,
                artifacts: None,
                lazy_tools: true,
                snapshot_fingerprints: Arc::new(Mutex::new(HashMap::new())),
            },
        };

        // No session + 2 tabs → each tab-scoped tool must refuse with guidance.
        for tool in ["cookies", "download", "network/capture"] {
            let mut args = Map::new();
            match tool {
                "download" => {
                    args.insert("url".into(), json!("https://example.com/file.txt"));
                }
                "network/capture" => {
                    args.insert("action".into(), json!("start"));
                }
                _ => {}
            }
            let err = server.call_tool(tool, &args).await.unwrap_err();
            assert!(
                err.contains("2 tabs are open"),
                "{tool} should require a session: {err}"
            );
        }

        // Explicit session → routed to that tab, cookie values redacted.
        let mut args = Map::new();
        args.insert("session".into(), json!(sa.id));
        let out = server
            .call_tool("cookies", &args)
            .await
            .expect("explicit session must work");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["session"].as_str(), Some(sa.id.as_str()));
        assert_eq!(v["include_values"], false);

        backend.reset_browser().await;
    }

    /// The `args(...)` signature embedded in a compact tool description.
    fn description_after_args(tool: &Value) -> String {
        tool["description"]
            .as_str()
            .and_then(|text| text.split("args(").nth(1))
            .map(|text| text.trim_end_matches(')').to_string())
            .unwrap_or_default()
    }

    fn artifact_store(tag: &str) -> (Arc<ArtifactStore>, std::path::PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lightbrowse-mcp-{tag}-{unique}.db"));
        let store = ArtifactStore::open(&path, lightbrowse_memory::ArtifactConfig::default())
            .expect("open artifact store");
        (Arc::new(store), path)
    }

    #[test]
    fn only_read_tools_are_budgetable() {
        for tool in ["navigate", "extract", "snapshot", "ask", "evaluate"] {
            assert!(is_budgetable(tool), "{tool} should be budgetable");
        }
        // Credential- and control-plane tools must never be rewritten or stored.
        for tool in [
            "vault/get",
            "vault/list",
            "login",
            "type",
            "fill_form",
            "cookies",
            "runbook/get",
            "artifact/read",
            "screenshot",
            "help",
        ] {
            assert!(!is_budgetable(tool), "{tool} must not be budgetable");
        }
    }

    #[test]
    fn compact_payload_reduces_the_largest_string_field() {
        let big = (0..600)
            .map(|index| format!("filler line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let payload = serde_json::json!({
            "url": "https://example.test/",
            "data": { "text": format!("{big}\nerror: something failed\n{big}") }
        })
        .to_string();
        let cfg = ReduceConfig::with_max_tokens(256);
        let (compacted, strategy) =
            compact_payload(&payload, ObservationKind::PageText, &cfg).expect("reduced");
        assert!(compacted["data"]["text"]
            .as_str()
            .unwrap()
            .contains("error: something failed"));
        assert!(estimate_tokens(&compacted.to_string()) < estimate_tokens(&payload));
        assert_ne!(
            strategy,
            lightbrowse_core::reduce::ReduceStrategy::Passthrough
        );
        // The projection stays valid JSON with the untouched fields intact.
        assert_eq!(compacted["url"], "https://example.test/");
        assert!(serde_json::from_str::<Value>(&compacted.to_string()).is_ok());
    }

    #[test]
    fn compact_payload_prunes_snapshot_trees_with_ancestors() {
        let mut nodes = Vec::new();
        for index in 0..400 {
            nodes.push(serde_json::json!({
                "uid": index + 1, "role": "text", "tag": "div",
                "text": format!("row {index} filler filler filler")
            }));
        }
        nodes.push(serde_json::json!({
            "uid": 9000, "role": "form", "tag": "form", "text": "Checkout",
            "children": [{"uid": 9001, "role": "button", "tag": "button", "text": "Submit"}]
        }));
        let payload = serde_json::json!({
            "url": "https://example.test/", "title": "Shop",
            "nodes": nodes, "node_count": 401, "truncated": false
        })
        .to_string();
        let cfg = ReduceConfig::with_max_tokens(256);
        let (compacted, strategy) =
            compact_payload(&payload, ObservationKind::BrowserTree, &cfg).expect("pruned");
        assert_eq!(
            strategy,
            lightbrowse_core::reduce::ReduceStrategy::TreeRanked
        );
        let text = compacted.to_string();
        assert!(text.contains("Submit"), "kept button must survive");
        assert!(text.contains("Checkout"), "its form ancestor must survive");
        assert!(
            compacted["node_count"].as_u64().unwrap() < 401,
            "node_count must be updated"
        );
    }

    #[test]
    fn compact_payload_collapses_large_arrays() {
        let links: Vec<Value> = (0..900)
            .map(|index| serde_json::json!({"href": format!("/page/{index}"), "text": format!("link {index}")}))
            .collect();
        let payload =
            serde_json::json!({"url": "https://example.test/", "mode": "links", "data": links})
                .to_string();
        let cfg = ReduceConfig::with_max_tokens(256);
        let (compacted, _) =
            compact_payload(&payload, ObservationKind::PageText, &cfg).expect("collapsed");
        let items = compacted["data"].as_array().unwrap();
        assert!(items.len() < 900, "array must shrink, got {}", items.len());
        assert!(
            items
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("more items omitted"),
            "a marker must replace the dropped items"
        );
        assert!(compacted["data"][0]["href"]
            .as_str()
            .unwrap()
            .contains("/page/0"));
        assert!(estimate_tokens(&compacted.to_string()) < estimate_tokens(&payload) / 2);
    }

    #[test]
    fn compact_payload_leaves_small_payloads_alone() {
        let payload = serde_json::json!({"url": "https://example.test/", "text": "short"});
        let cfg = ReduceConfig::with_max_tokens(256);
        assert!(compact_payload(&payload.to_string(), ObservationKind::PageText, &cfg).is_none());
    }

    #[test]
    fn bound_payload_stores_the_complete_response_as_an_artifact() {
        let (store, path) = artifact_store("bound");
        let big = (0..800)
            .map(|index| format!("filler line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let payload = serde_json::json!({
            "url": "https://example.test/", "text": format!("{big}\nerror: boom\n{big}")
        })
        .to_string();
        let cfg = ReduceConfig::with_max_tokens(256);
        let bounded = bound_payload("navigate", payload.clone(), &cfg, &store);

        assert!(estimate_tokens(&bounded) < estimate_tokens(&payload));
        let value: Value = serde_json::from_str(&bounded).expect("still valid JSON");
        let marker = value["reduction"]["marker"].as_str().unwrap();
        assert!(marker.contains("artifact/read"), "{marker}");
        let id = value["reduction"]["artifact"].as_str().unwrap();
        assert_eq!(id, ArtifactStore::id_for(payload.as_bytes()));

        // artifact/read equivalent: the exact original payload comes back.
        let stored = store.get(id).unwrap().expect("artifact stored");
        assert_eq!(stored.text(), payload);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bound_payload_is_a_noop_without_an_artifact_store_or_budget() {
        let (store, path) = artifact_store("noop");
        let big = "x".repeat(20_000);
        let payload = serde_json::json!({ "text": big }).to_string();

        // Budget disabled.
        let cfg = ReduceConfig::with_max_tokens(0);
        assert_eq!(
            bound_payload("navigate", payload.clone(), &cfg, &store),
            payload
        );
        // Not an allow-listed tool.
        let cfg = ReduceConfig::with_max_tokens(256);
        assert_eq!(
            bound_payload("cookies", payload.clone(), &cfg, &store),
            payload
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ask_passages_ranks_matching_lines_with_context() {
        let text =
            "alpha beta\nParis is the capital of France\nthe capital city hosts the tour\nzeta";
        let hits = ask_passages(text, "capital France", 2);
        assert_eq!(hits.len(), 2);
        assert!(hits[0]["text"].as_str().unwrap().contains("capital"));
        assert_eq!(hits[0]["before"], "alpha beta");
        assert!(hits[0]["score"].as_f64().unwrap() >= hits[1]["score"].as_f64().unwrap());
        assert!(ask_passages(text, "", 2).is_empty());
    }
    #[test]
    fn compact_tools_schema_keeps_arg_names_and_shrinks_hard() {
        let full = tools_schema();
        let compact = compact_tools_schema();
        assert_eq!(full.len(), compact.len());
        for (full_tool, compact_tool) in full.iter().zip(compact.iter()) {
            assert_eq!(full_tool["name"], compact_tool["name"]);
            // Every argument name survives — agents must still be able to call.
            let full_props = full_tool["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let compact_props = compact_tool["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                full_props.keys().collect::<std::collections::BTreeSet<_>>(),
                compact_props
                    .keys()
                    .collect::<std::collections::BTreeSet<_>>(),
                "{} lost an argument",
                full_tool["name"]
            );
            // `required` is not repeated; the signature marks optional args
            // with `?` and `tool/inspect` keeps the authoritative list.
            let signature = description_after_args(compact_tool);
            for name in full_tool["inputSchema"]["required"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                let name = name.as_str().unwrap();
                assert!(
                    signature.split(", ").any(|arg| arg == name),
                    "{name} must appear as required in `{signature}`"
                );
            }
            // Prose is dropped, arg signature is inlined.
            for spec in compact_props.values() {
                assert!(
                    spec.get("description").is_none(),
                    "compact schema must not carry parameter prose"
                );
            }
            let description = compact_tool["description"].as_str().unwrap();
            assert!(description.chars().count() <= 140, "{description}");
            if !full_props.is_empty() {
                assert!(description.contains("args("), "{description}");
            }
        }
        let full_size = serde_json::to_string(&full).unwrap().len();
        let compact_size = serde_json::to_string(&compact).unwrap().len();
        assert!(
            compact_size * 2 <= full_size,
            "expected >=50% smaller tools/list: {compact_size} vs {full_size}"
        );
    }

    #[test]
    fn arg_signature_marks_optional_args_and_caps_the_list() {
        let tool = serde_json::json!({
            "name": "example",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "engine": {"type": "string"},
                    "mode": {"type": "string", "enum": ["a", "b"]}
                },
                "required": ["url"]
            }
        });
        assert_eq!(arg_signature(&tool), "url, engine?, mode?");

        let wide = serde_json::json!({
            "name": "wide",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {},
                    "f": {}, "g": {}, "h": {}, "i": {}, "j": {}
                }
            }
        });
        let signature = arg_signature(&wide);
        assert!(signature.ends_with("more"), "{signature}");
        assert!(signature.matches(',').count() <= 4, "{signature}");
    }

    #[test]
    fn snapshot_delta_fires_only_for_an_unchanged_big_tree() {
        let cache = Mutex::new(HashMap::new());
        let big = serde_json::json!({
            "url": "https://example.test/",
            "nodes": (0..600)
                .map(|index| serde_json::json!({"uid": index, "role": "text", "text": format!("filler {index}")}))
                .collect::<Vec<_>>(),
        });
        // First call: nothing to compare against.
        assert!(snapshot_delta(&cache, "https://example.test/", &big, 256, None).is_none());
        // Second call, identical content: a fingerprint instead of the tree.
        let delta = snapshot_delta(&cache, "https://example.test/", &big, 256, None)
            .expect("delta on unchanged page");
        assert_eq!(delta["unchanged"], true);
        assert!(delta["fingerprint"].as_str().unwrap().len() == 16);
        assert!(estimate_tokens(&delta.to_string()) <= 60, "{delta}");
        assert!(delta["note"].as_str().unwrap().contains("force:true"));
        // Changed content: back to the full tree.
        let mut changed = big.clone();
        changed["nodes"][0]["text"] = serde_json::json!("different");
        assert!(snapshot_delta(&cache, "https://example.test/", &changed, 256, None).is_none());
        // Different URL keeps its own fingerprint.
        assert!(snapshot_delta(&cache, "https://other.test/", &big, 256, None).is_none());
    }

    #[test]
    fn snapshot_delta_is_skipped_for_small_trees_and_when_disabled() {
        let cache = Mutex::new(HashMap::new());
        let small = serde_json::json!({"url": "https://example.test/", "nodes": [{"uid": 1}]});
        assert!(snapshot_delta(&cache, "https://example.test/", &small, 1000, None).is_none());
        assert!(snapshot_delta(&cache, "https://example.test/", &small, 1000, None).is_none());
        let big =
            serde_json::json!({"nodes": vec![serde_json::json!({"text": "x".repeat(20_000)})]});
        assert!(snapshot_delta(&cache, "https://example.test/", &big, 0, None).is_none());
    }

    #[test]
    fn snapshot_delta_stores_the_tree_as_an_artifact() {
        let (store, path) = artifact_store("delta");
        let cache = Mutex::new(HashMap::new());
        let big = serde_json::json!({
            "url": "https://example.test/",
            "nodes": vec![serde_json::json!({"text": "y".repeat(20_000)})],
        });
        assert!(snapshot_delta(&cache, "https://example.test/", &big, 256, Some(&store)).is_none());
        let delta = snapshot_delta(&cache, "https://example.test/", &big, 256, Some(&store))
            .expect("delta");
        let id = delta["artifact"].as_str().expect("artifact id in delta");
        let stored = store.get(id).expect("read").expect("stored");
        assert_eq!(stored.text(), big.to_string());
        let _ = std::fs::remove_file(path);
    }
}
