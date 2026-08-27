//! `lightbrowse-cdp` — headless Chromium backend, driven through the Chrome
//! DevTools Protocol (CDP) with a small hand-rolled JSON-RPC client.
//!
//! Design goals (in priority order):
//! 1. **Low RAM** — Chromium is spawned lazily (only when a page actually
//!    needs JS), killed when idle, and launched with memory-conservative
//!    flags (`--disable-gpu`, capped JS heap, no background services).
//! 2. **Full web support** — real JS rendering for SPA-heavy sites, then the
//!    rendered DOM is handed to the core extractors (same pipeline as fetch).
//! 3. **Zero magic deps** — the CDP client is ~250 lines over a websocket;
//!    no chromiumoxide, no browser driver.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::Config;
use lightbrowse_core::error::{Error, Result};
use lightbrowse_core::page::Page;
use lightbrowse_core::session::Session;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

static RPC_ID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// A live browser tab kept open so actions (click/type/submit) can run
/// against the page an agent is currently looking at.
#[derive(Clone)]
pub struct ActivePage {
    pub port: u16,
    pub target_id: String,
    pub ws_url: String,
}

/// CDP backend with lazy Chromium spawn + idle suspension.
pub struct CdpBackend {
    config: Config,
    inner: tokio::sync::Mutex<Option<CdpBrowser>>,
    active: tokio::sync::Mutex<Option<ActivePage>>,
    last_used: Mutex<Instant>,
    shutdown: CancellationToken,
}

impl CdpBackend {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            inner: tokio::sync::Mutex::new(None),
            active: tokio::sync::Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
            shutdown: CancellationToken::new(),
        }
    }

    /// Start the idle watcher: after `idle_timeout_secs` without activity the
    /// Chromium process is killed and its RAM released. Call once after the
    /// backend is wrapped in an `Arc`.
    pub fn spawn_idle_watcher(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let idle = self.config.idle_timeout_secs;
        let token = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                let idle_elapsed = this
                    .last_used
                    .lock()
                    .map(|g| g.elapsed())
                    .unwrap_or_default();
                if idle_elapsed > Duration::from_secs(idle) {
                    this.suspend().await;
                }
            }
        });
    }

    /// Kill Chromium and release its RAM.
    pub async fn suspend(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(b) = guard.take() {
            b.kill();
            *self.active.lock().await = None;
            tracing::info!("cdp: Chromium suspended (RAM released)");
        }
    }

    /// Is a Chromium instance alive right now?
    pub async fn is_running(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    /// Current RAM usage of the Chromium process tree (MB).
    pub async fn memory_usage_mb(&self) -> usize {
        self.inner
            .lock()
            .await
            .as_ref()
            .map(|b| b.memory_usage_mb())
            .unwrap_or(0)
    }

    fn touch(&self) {
        if let Ok(mut g) = self.last_used.lock() {
            *g = Instant::now();
        }
    }
}

impl Drop for CdpBackend {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[async_trait]
impl BrowserBackend for CdpBackend {
    fn name(&self) -> &'static str {
        "cdp"
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    async fn navigate(&self, _session: &Session, url: &str) -> Result<Page> {
        let active = self.ensure_page(url).await?;
        let result = navigate_and_render(&active.ws_url, url, self.config.js_wait_ms).await;
        if result.is_ok() {
            let ram = self.memory_usage_mb().await;
            if ram > self.config.memory_budget_mb {
                tracing::warn!(
                    "cdp: actual Chromium RAM {ram} MB exceeds budget {} MB — consider --engine fetch for this site or raise LIGHTBROWSE_MEMORY_MB",
                    self.config.memory_budget_mb
                );
            }
        }
        let (html, title, final_url) = result?;
        self.touch();
        Ok(Page {
            url: final_url,
            title,
            status: 200,
            headers: Default::default(),
            html,
            truncated: false,
            mime: Some("text/html".into()),
        })
    }
}

impl CdpBackend {
    /// Make sure Chromium is alive and a tab exists; returns the active page.
    async fn ensure_page(&self, url: &str) -> Result<ActivePage> {
        // Reuse the open tab if we have one.
        if let Some(p) = self.active.lock().await.clone() {
            tracing::debug!("cdp: reusing active tab for {url}");
            return Ok(p);
        }
        tracing::debug!("cdp: creating new tab for {url}");
        let port = {
            let mut guard = self.inner.lock().await;
            if guard.is_none() {
                tracing::info!("cdp: spawning headless Chromium (lazy)");
                *guard = Some(CdpBrowser::spawn(&self.config).await?);
            }
            guard.as_ref().expect("just spawned").port
        };
        let http = reqwest::Client::new();
        let target = http
            .put(format!(
                "http://127.0.0.1:{port}/json/new?{}",
                urlencoding(url)
            ))
            .send()
            .await
            .map_err(|e| Error::Transport(format!("cdp create tab: {e}")))?
            .json::<Value>()
            .await
            .map_err(|e| Error::Transport(format!("cdp create tab json: {e}")))?;
        let page_ws = target
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Transport("no page ws url".into()))?
            .to_string();
        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let active = ActivePage {
            port,
            target_id,
            ws_url: page_ws,
        };
        *self.active.lock().await = Some(active.clone());
        tracing::info!("cdp: active tab set (target={})", active.target_id);
        Ok(active)
    }

    /// The URL the active tab is currently showing (via JS).
    pub async fn current_url(&self) -> Option<String> {
        let ws = self.active.lock().await.clone()?.ws_url;
        let v = evaluate_js(&ws, "location.href").await.ok()?;
        v.as_str().map(|s| s.to_string())
    }

    /// Evaluate JS on the active tab; returns the result value.
    pub async fn evaluate(&self, expr: &str) -> Result<Value> {
        let active = self.active.lock().await.clone().ok_or_else(|| {
            tracing::warn!("cdp: evaluate called with no active page");
            Error::NotInitialized("no active page — navigate first".into())
        })?;
        let v = evaluate_js(&active.ws_url, expr).await?;
        self.touch();
        Ok(v)
    }

    /// Click an element by CSS selector (from a snapshot's `selector` field).
    pub async fn click(&self, selector: &str) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        self.evaluate(&format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'element not found'}};              el.scrollIntoView({{block:'center'}}); el.focus(); el.click(); return {{ok:true, tag: el.tagName.toLowerCase()}}; }})()"
        ))
        .await
    }

    /// Type text into an input/textarea (React-compatible value setter).
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let txt = serde_json::to_string(text).map_err(|e| Error::Parse(e.to_string()))?;
        self.evaluate(&format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'element not found'}};              el.focus(); const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;              const setter = Object.getOwnPropertyDescriptor(proto, 'value').set; setter.call(el, {txt});              el.dispatchEvent(new Event('input', {{bubbles:true}})); el.dispatchEvent(new Event('change', {{bubbles:true}}));              return {{ok:true, value: el.value}}; }})()"
        ))
        .await
    }

    /// Submit the form containing `selector` (or the form itself).
    pub async fn submit(&self, selector: &str) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        self.evaluate(&format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'element not found'}};              const form = el.tagName === 'FORM' ? el : el.form; if (!form) return {{ok:false, reason:'no parent form'}};              form.requestSubmit(); return {{ok:true, action: form.action || null}}; }})()"
        ))
        .await
    }

    /// Serialize the active tab's rendered DOM (html, title, url).
    pub async fn current_dom(&self) -> Result<(String, String, String)> {
        let active = self
            .active
            .lock()
            .await
            .clone()
            .ok_or_else(|| Error::NotInitialized("no active page — navigate first".into()))?;
        let expr = "JSON.stringify({title: document.title, url: location.href, html: document.documentElement.outerHTML})";
        let v = evaluate_js(&active.ws_url, expr).await?;
        let parsed: Value = serde_json::from_str(v.as_str().unwrap_or("{}"))
            .map_err(|e| Error::Parse(e.to_string()))?;
        Ok((
            parsed
                .get("html")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            parsed
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            parsed
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Browser process
// ---------------------------------------------------------------------------

struct CdpBrowser {
    child: Child,
    port: u16,
    _user_data_dir: std::path::PathBuf,
}

impl CdpBrowser {
    async fn spawn(config: &Config) -> Result<Self> {
        let chrome = config
            .chrome_path
            .clone()
            .or_else(detect_chrome)
            .ok_or_else(|| {
                Error::NotInitialized(
                    "no Chrome/Chromium binary found (set --chrome <path> or CHROME_PATH)".into(),
                )
            })?;

        let port = pick_free_port().await?;
        let user_data = std::env::temp_dir().join(format!(
            "lightbrowse-chrome-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&user_data).ok();

        // JS heap cap from the memory budget: keep ~250 MB for the browser
        // itself, give the rest to the page.
        let js_heap_mb = config.memory_budget_mb.saturating_sub(250).clamp(64, 2048);

        // Low-memory mode: when the budget is tight, shed every optional
        // process and cache so a single tab fits in ~200 MB.
        let low_mem = config.memory_budget_mb < 350;

        let mut cmd = Command::new(&chrome);
        cmd.args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-default-apps",
            "--metrics-recording-only",
            "--disable-dev-shm-usage",
            "--mute-audio",
            "--window-size=1280,800",
            &format!("--js-flags=--max-old-space-size={js_heap_mb}"),
            &format!("--remote-debugging-port={port}"),
            &format!("--user-data-dir={}", user_data.display()),
            "about:blank",
        ]);
        if low_mem {
            tracing::info!(
                "cdp: low-memory mode (budget {} MB) — single renderer, no cache",
                config.memory_budget_mb
            );
            // Note: --no-zygote crashes Chrome on some systems — do not add.
            cmd.args([
                "--renderer-process-limit=1",
                "--disable-software-rasterizer",
                "--disk-cache-size=1",
                "--disable-features=Translate,MediaRouter,OptimizationHints",
            ]);
        }
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("failed to launch Chrome ({chrome}): {e}")))?;

        // Wait for the DevTools endpoint to come up.
        let http = reqwest::Client::new();
        wait_for_devtools(&http, port).await?;

        Ok(Self {
            child,
            port,
            _user_data_dir: user_data,
        })
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn memory_usage_mb(&self) -> usize {
        let pid = self.child.id();
        let children = std::process::Command::new("ps")
            .args(["--ppid", &pid.to_string(), "-o", "rss="])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| {
                s.lines()
                    .filter_map(|l| l.trim().parse::<usize>().ok())
                    .sum()
            })
            .unwrap_or(0);
        let main = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        (main + children) / 1024
    }
}

fn detect_chrome() -> Option<String> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
    ] {
        if let Ok(out) = std::process::Command::new("which").arg(name).output() {
            if out.status.success() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    let p = s.trim().to_string();
                    if !p.is_empty() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

async fn pick_free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(Error::Io)?;
    let port = listener.local_addr().map_err(Error::Io)?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_devtools(http: &reqwest::Client, port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(resp) = http.get(&url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(ws) = v.get("webSocketDebuggerUrl").and_then(|s| s.as_str()) {
                    return Ok(ws.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(Error::Transport(
        "Chrome DevTools endpoint did not come up in 20s".into(),
    ))
}

// ---------------------------------------------------------------------------
// CDP session: navigate + wait + render
// ---------------------------------------------------------------------------

type CdpWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a websocket, navigate, wait for the load event, then serialize the
/// rendered DOM. Returns `(html, title, final_url)`.
async fn navigate_and_render(
    ws_url: &str,
    url: &str,
    js_wait_ms: u64,
) -> Result<(String, String, String)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;

    rpc_expect(&mut ws, "Page.enable", json!({})).await?;

    // Navigate and wait for the load event (or timeout).
    let nav_id = next_id();
    ws.send(WsMessage::Text(
        json!({ "id": nav_id, "method": "Page.navigate", "params": { "url": url } }).to_string(),
    ))
    .await
    .map_err(|e| Error::Transport(format!("cdp send: {e}")))?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut loaded = false;
    while Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .ok()
            .flatten()
            .ok_or_else(|| Error::Transport("cdp: connection closed during load".into()))?;
        let text = match msg {
            Ok(WsMessage::Text(t)) => t,
            Ok(WsMessage::Ping(p)) => {
                let _ = ws.send(WsMessage::Pong(p)).await;
                continue;
            }
            Ok(_) => continue,
            Err(e) => return Err(Error::Transport(format!("cdp recv: {e}"))),
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if v.get("method").and_then(|m| m.as_str()) == Some("Page.loadEventFired") {
            loaded = true;
            break;
        }
    }

    // Extra grace for lazy JS frameworks.
    if js_wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(js_wait_ms)).await;
    }

    if !loaded {
        tracing::warn!("cdp: load event not fired within 30s, reading DOM anyway");
    }

    // Serialize the rendered document.
    let expr = "JSON.stringify({title: document.title, url: location.href, html: document.documentElement.outerHTML})";
    let result = rpc_expect(
        &mut ws,
        "Runtime.evaluate",
        json!({
            "expression": expr,
            "returnByValue": true,
            "awaitPromise": true
        }),
    )
    .await?;

    let value = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Parse("cdp evaluate: no value returned".into()))?;
    let parsed: Value = serde_json::from_str(value)
        .map_err(|e| Error::Parse(format!("cdp evaluate payload: {e}")))?;

    let html = parsed
        .get("html")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let final_url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or(url)
        .to_string();

    let _ = ws.close(None).await;
    Ok((html, title, final_url))
}

/// One-shot JS evaluation on a page websocket (opens + closes its own ws).
async fn evaluate_js(ws_url: &str, expression: &str) -> Result<Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
    let result = rpc_expect(
        &mut ws,
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
    )
    .await?;
    let _ = ws.close(None).await;
    result
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .ok_or_else(|| Error::Parse("evaluate: no value returned".into()))
}

/// Send a command and wait for its response (ignoring events in between).
async fn rpc_expect(ws: &mut CdpWs, method: &str, params: Value) -> Result<Value> {
    let id = next_id();
    ws.send(WsMessage::Text(
        json!({ "id": id, "method": method, "params": params }).to_string(),
    ))
    .await
    .map_err(|e| Error::Transport(format!("cdp send {method}: {e}")))?;

    let _deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .ok()
            .flatten()
            .ok_or_else(|| Error::Transport(format!("cdp: {method} timed out")))?;
        match msg {
            Ok(WsMessage::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(Error::Transport(format!("cdp {method} error: {err}")));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            Ok(WsMessage::Ping(p)) => {
                let _ = ws.send(WsMessage::Pong(p)).await;
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Transport(format!("cdp recv: {e}"))),
        }
    }
}

fn next_id() -> u64 {
    RPC_ID.fetch_add(1, Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlenc() {
        assert_eq!(
            urlencoding("https://a.com/?x=1&y=2"),
            "https%3A%2F%2Fa.com%2F%3Fx%3D1%26y%3D2"
        );
    }
}
