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
use lightbrowse_memory::{navigate_cached, MemoryStore};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const TEXT_PREVIEW_CHARS: usize = 4000;

/// Shared state handed to the MCP loop.
#[derive(Clone)]
pub struct McpState {
    pub backend: Arc<dyn BrowserBackend>,
    pub cdp: Option<Arc<dyn BrowserBackend>>,
    pub session: Arc<Mutex<Session>>,
    pub engine: Engine,
    pub memory: Option<Arc<MemoryStore>>,
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
    ) -> Self {
        Self {
            state: McpState {
                backend,
                cdp,
                session,
                engine,
                memory,
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
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "lightbrowse", "version": env!("CARGO_PKG_VERSION") }
                }
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
                let hits = m.search(&question, 6).map_err(|e| e.to_string())?;
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
                let hits = m.search(&query, limit).map_err(|e| e.to_string())?;
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
            "evaluate" => {
                let expression = req_str(args, "expression")?;
                let cdp = require_cdp(&s)?;
                let res = cdp.evaluate(&expression).await.map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "result": res })))
            }
            "page/current" => {
                let cdp = require_cdp(&s)?;
                let (html, title, url) = cdp.current_dom().await.map_err(|e| e.to_string())?;
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
                let res = cdp.click(&selector).await.map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "type" => {
                let selector = req_str(args, "selector")?;
                let text = req_str(args, "text")?;
                let cdp = require_cdp(&s)?;
                let res = cdp
                    .type_text(&selector, &text)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "submit" => {
                let selector = req_str(args, "selector")?;
                let cdp = require_cdp(&s)?;
                let res = cdp.submit(&selector).await.map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "selector": selector, "result": res })))
            }
            "press" => {
                let key = req_str(args, "key")?;
                let cdp = require_cdp(&s)?;
                let res = cdp.press_key(&key).await.map_err(|e| e.to_string())?;
                Ok(pretty(&json!({ "key": key, "result": res })))
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
    vec![
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
            "name": "evaluate",
            "description": "Run arbitrary JavaScript on the ACTIVE CDP tab and return the value. For advanced inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": { "type": "string" }
                },
                "required": ["expression"]
            }
        }),
        json!({
            "name": "page/current",
            "description": "Read the ACTIVE CDP tab: url, title, rendered text preview. Use after click/type/submit to see the result.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "click",
            "description": "Click an element on the ACTIVE CDP tab using its CSS selector (from snapshot). Navigate with engine=cdp first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector from a snapshot node" }
                },
                "required": ["selector"]
            }
        }),
        json!({
            "name": "type",
            "description": "Type text into an input/textarea on the ACTIVE CDP tab (React-compatible events).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string" },
                    "text": { "type": "string" }
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
                    "selector": { "type": "string" }
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
                    "key": { "type": "string", "enum": ["Enter", "Tab", "Backspace", "Escape", "ArrowDown", "ArrowUp"] }
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
    ]
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
