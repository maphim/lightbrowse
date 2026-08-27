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

/// One recorded action in the session trail (feed for runbook/save).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrailStep {
    pub action: String,
    pub selector: Option<String>,
    /// Alternative ways to find the same element (id/name/placeholder/...).
    #[serde(default)]
    pub fallbacks: Vec<String>,
    pub text: Option<String>,
    pub key: Option<String>,
    pub ms: Option<u64>,
}

/// A replayable runbook step.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunbookStep {
    pub action: String,
    pub selector: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    pub text: Option<String>,
    pub key: Option<String>,
    pub ms: Option<u64>,
}

/// CDP backend with lazy Chromium spawn + idle suspension.
pub struct CdpBackend {
    config: Config,
    inner: tokio::sync::Mutex<Option<CdpBrowser>>,
    active: tokio::sync::Mutex<Option<ActivePage>>,
    trail: Mutex<Vec<TrailStep>>,
    last_used: Mutex<Instant>,
    shutdown: CancellationToken,
}

impl CdpBackend {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            inner: tokio::sync::Mutex::new(None),
            active: tokio::sync::Mutex::new(None),
            trail: Mutex::new(Vec::new()),
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

    /// Close Chromium gracefully (cookies flushed to the profile) and
    /// release its RAM. Called on idle timeout / shutdown.
    pub async fn suspend(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(b) = guard.take() {
            b.close_gracefully().await;
            *self.active.lock().await = None;
            tracing::info!("cdp: Chromium suspended (RAM released, cookies flushed)");
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

    /// Session action trail (feed for `runbook/save`).
    pub fn trail(&self) -> Vec<TrailStep> {
        self.trail.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clear_trail(&self) {
        if let Ok(mut g) = self.trail.lock() {
            g.clear();
        }
    }

    fn record_step(&self, step: TrailStep) {
        if let Ok(mut g) = self.trail.lock() {
            g.push(step);
        }
    }

    /// Collect alternative selectors for an element (id, name, placeholder,
    /// aria-label) so replays survive small DOM changes.
    async fn collect_fallbacks(&self, selector: &str) -> Vec<String> {
        let sel = serde_json::to_string(selector).unwrap_or_default();
        self.evaluate(&format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return [];              const out = [];              if (el.id) out.push('#' + CSS.escape(el.id));              if (el.name) out.push(`[name=\"${{el.name}}\"]`);              if (el.placeholder) out.push(`[placeholder=\"${{el.placeholder}}\"]`);              if (el.getAttribute('aria-label')) out.push(`[aria-label=\"${{el.getAttribute('aria-label')}}\"]`);              return out.slice(0,3); }})()"
        ))
        .await
        .and_then(|v| serde_json::from_value(v).map_err(|e| Error::Parse(e.to_string())))
        .unwrap_or_default()
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

    async fn navigate(&self, session: &Session, url: &str) -> Result<Page> {
        let active = self.ensure_page(url).await?;
        let result = navigate_and_render(
            &active.ws_url,
            url,
            &session.user_agent,
            self.config.js_wait_ms,
        )
        .await;
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

    /// Click an element by CSS selector using REAL mouse events (moved →
    /// pressed → released at the element's center), which is far harder for
    /// anti-bot systems to flag than `el.click()`.
    pub async fn click(&self, selector: &str) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let point = self
            .evaluate(&format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
                 el.scrollIntoView({{block:'center'}}); const r = el.getBoundingClientRect(); \
                 return {{x: r.x + r.width/2, y: r.y + r.height/2, tag: el.tagName.toLowerCase()}}; }})()"
            ))
            .await?;
        let x = point.get("x").and_then(|v| v.as_f64());
        let y = point.get("y").and_then(|v| v.as_f64());
        let Some((x, y)) = x.zip(y) else {
            return Ok(json!({"ok": false, "reason": "element not found"}));
        };
        let tag = point
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let active = self
            .active
            .lock()
            .await
            .clone()
            .ok_or_else(|| Error::NotInitialized("no active page — navigate first".into()))?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        for (typ, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            rpc_expect(
                &mut ws,
                "Input.dispatchMouseEvent",
                json!({
                    "type": typ,
                    "x": x.round() as u64,
                    "y": y.round() as u64,
                    "button": "left",
                    "buttons": buttons,
                    "clickCount": 1
                }),
            )
            .await?;
        }
        let _ = ws.close(None).await;
        self.touch();
        let fallbacks = self.collect_fallbacks(selector).await;
        self.record_step(TrailStep {
            action: "click".into(),
            selector: Some(selector.to_string()),
            fallbacks,
            text: None,
            key: None,
            ms: None,
        });
        Ok(json!({ "ok": true, "tag": tag, "x": x.round() as u64, "y": y.round() as u64 }))
    }

    /// Type text like a real keyboard (CDP `Input.insertText` — fires genuine
    /// input events; React and anti-bot both see a human typing).
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        // Focus the field first with a real click.
        let clicked = self.click(selector).await?;
        if clicked.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Ok(clicked);
        }
        let active = self
            .active
            .lock()
            .await
            .clone()
            .ok_or_else(|| Error::NotInitialized("no active page — navigate first".into()))?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        rpc_expect(&mut ws, "Input.insertText", json!({ "text": text })).await?;
        let _ = ws.close(None).await;
        self.touch();
        // Confirm what landed in the field.
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let value = self
            .evaluate(&format!(
                "(() => {{ const el = document.querySelector({sel}); return el ? el.value : null; }})()"
            ))
            .await?;
        let fallbacks = self.collect_fallbacks(selector).await;
        self.record_step(TrailStep {
            action: "type".into(),
            selector: Some(selector.to_string()),
            fallbacks,
            text: Some(text.to_string()),
            key: None,
            ms: None,
        });
        Ok(json!({ "ok": true, "value": value }))
    }

    /// Press a physical key on the focused element (Enter, Tab, Backspace...).
    pub async fn press_key(&self, key: &str) -> Result<Value> {
        let (key_code, text) = match key {
            "Enter" => (13, "\r"),
            "Tab" => (9, "\t"),
            "Backspace" => (8, "\u{8}"),
            "Escape" => (27, "\u{1b}"),
            "ArrowDown" => (40, ""),
            "ArrowUp" => (38, ""),
            other => return Err(Error::Unsupported(format!("unsupported key '{other}'"))),
        };
        let active = self
            .active
            .lock()
            .await
            .clone()
            .ok_or_else(|| Error::NotInitialized("no active page — navigate first".into()))?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        for typ in ["keyDown", "keyUp"] {
            rpc_expect(
                &mut ws,
                "Input.dispatchKeyEvent",
                json!({
                    "type": typ,
                    "key": key,
                    "code": key,
                    "text": if typ == "keyDown" { text } else { "" },
                    "windowsVirtualKeyCode": key_code,
                    "nativeVirtualKeyCode": key_code
                }),
            )
            .await?;
        }
        let _ = ws.close(None).await;
        self.touch();
        self.record_step(TrailStep {
            action: "press".into(),
            selector: None,
            fallbacks: Vec::new(),
            text: None,
            key: Some(key.to_string()),
            ms: None,
        });
        Ok(json!({ "ok": true, "key": key }))
    }

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
    browser_ws: String,
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
        // Persistent profile keeps logins alive across restarts; without one
        // we use a throwaway temp profile (stateless browsing).
        let user_data = match &config.profile_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).ok();
                tracing::info!("cdp: using persistent profile {}", dir.display());
                dir.clone()
            }
            None => std::env::temp_dir().join(format!(
                "lightbrowse-chrome-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )),
        };
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
        let browser_ws = wait_for_devtools(&http, port).await?;

        Ok(Self {
            child,
            port,
            browser_ws,
            _user_data_dir: user_data,
        })
    }

    /// Graceful CDP `Browser.close` first (flushes cookies to the profile),
    /// then SIGKILL as a fallback. Without this, logins are lost.
    async fn close_gracefully(mut self) {
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&self.browser_ws).await {
            let _ = ws
                .send(WsMessage::Text(
                    json!({"id": next_id(), "method": "Browser.close"}).to_string(),
                ))
                .await;
            // Give Chrome a moment to flush and exit.
            for _ in 0..20 {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let _ = ws.close(None).await;
        }
        tracing::warn!("cdp: Browser.close timed out — killing Chromium");
        self.kill();
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
    user_agent: &str,
    js_wait_ms: u64,
) -> Result<(String, String, String)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;

    rpc_expect(&mut ws, "Page.enable", json!({})).await?;
    stealth_setup(&mut ws, user_agent).await?;

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

    // Cloudflare / Turnstile / PerimeterX challenges self-solve after a few
    // seconds of JS execution. Poll until the title stops being a challenge
    // page (max 15s), so we read the real content instead of a captcha.
    let mut challenge_waits = 0u32;
    while challenge_waits < 15 {
        let title = rpc_expect(
            &mut ws,
            "Runtime.evaluate",
            json!({ "expression": "document.title", "returnByValue": true }),
        )
        .await
        .ok()
        .and_then(|r| {
            r.get("result")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
        let is_challenge = title.contains("Just a moment")
            || title.contains("Attention Required")
            || title.contains("cf-chl")
            || title.contains("Checking your browser")
            || title.contains("Access Denied");
        if !is_challenge {
            break;
        }
        challenge_waits += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if challenge_waits > 0 {
        tracing::info!("cdp: challenge page detected, waited {challenge_waits}s");
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

/// Hide automation fingerprints before the page runs any script:
/// - override the User-Agent (removes the `HeadlessChrome` marker)
/// - neuter `navigator.webdriver`
/// - restore `window.chrome` / `navigator.plugins` shape (headless lacks them)
async fn stealth_setup(ws: &mut CdpWs, user_agent: &str) -> Result<()> {
    if let Err(e) = rpc_expect(
        ws,
        "Emulation.setUserAgentOverride",
        json!({
            "userAgent": user_agent,
            "acceptLanguage": "en-US,en;q=0.9,vi;q=0.8",
            "platform": "Linux x86_64"
        }),
    )
    .await
    {
        tracing::warn!("cdp: UA override failed: {e}");
    }
    let stealth_js = format!(
        r#"(() => {{
    Object.defineProperty(navigator, 'webdriver', {{ get: () => undefined }});
    Object.defineProperty(navigator, 'userAgent', {{ get: () => {ua_json} }});
    if (!window.chrome) {{
        window.chrome = {{ runtime: {{}} }};
    }}
    Object.defineProperty(navigator, 'plugins', {{
        get: () => [1, 2, 3, 4, 5],
    }});
    Object.defineProperty(navigator, 'languages', {{
        get: () => ['en-US', 'en', 'vi'],
    }});
}})();
"#,
        ua_json = serde_json::to_string(user_agent).map_err(|e| Error::Parse(e.to_string()))?
    );
    let _ = rpc_expect(
        ws,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": stealth_js }),
    )
    .await;
    Ok(())
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

// ---------------------------------------------------------------------------
// Runbook execution (replay a recorded action recipe)
// ---------------------------------------------------------------------------

/// Result of running one step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StepOutcome {
    pub step: usize,
    pub action: String,
    pub ok: bool,
    pub detail: String,
}

/// Result of a full runbook run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunbookOutcome {
    pub url: String,
    pub ok: bool,
    pub steps: Vec<StepOutcome>,
    /// Final page state after the run (title + text preview) so callers can
    /// confirm the run actually achieved its goal (e.g. logged in).
    pub final_title: String,
    pub final_text_preview: String,
}

/// Substitute `{{VAR}}` placeholders in a string.
fn fill_vars(template: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// Replay a runbook against the CDP backend.
///
/// `steps` come from `runbook/save` (recorded trail). Each element step tries
/// its primary selector, then fallbacks (id/name/placeholder), so replays
/// survive small DOM changes.
pub async fn run_runbook(
    cdp: &CdpBackend,
    url: &str,
    steps: &[RunbookStep],
    vars: &std::collections::HashMap<String, String>,
) -> Result<RunbookOutcome> {
    let mut outcomes = Vec::new();
    let mut ok_all = true;

    cdp.navigate(&Session::new(), url).await?;

    for (i, step) in steps.iter().enumerate() {
        let mut candidates: Vec<String> = Vec::new();
        if let Some(sel) = &step.selector {
            candidates.push(fill_vars(sel, vars));
        }
        for fb in &step.fallbacks {
            candidates.push(fill_vars(fb, vars));
        }

        let outcome = match step.action.as_str() {
            "click" => {
                let mut detail = String::new();
                let mut done = false;
                for sel in &candidates {
                    match cdp.click(sel).await {
                        Ok(v) if v.get("ok").and_then(|x| x.as_bool()) == Some(true) => {
                            detail = format!("clicked {sel}");
                            done = true;
                            break;
                        }
                        Ok(v) => {
                            detail = v
                                .get("reason")
                                .and_then(|r| r.as_str())
                                .unwrap_or("failed")
                                .to_string();
                        }
                        Err(e) => detail = e.to_string(),
                    }
                }
                StepOutcome {
                    step: i,
                    action: "click".into(),
                    ok: done,
                    detail,
                }
            }
            "type" => {
                let text = fill_vars(step.text.as_deref().unwrap_or(""), vars);
                let mut detail = String::new();
                let mut done = false;
                for sel in &candidates {
                    match cdp.type_text(sel, &text).await {
                        Ok(v) if v.get("ok").and_then(|x| x.as_bool()) == Some(true) => {
                            detail = format!("typed into {sel}");
                            done = true;
                            break;
                        }
                        Ok(v) => {
                            detail = v
                                .get("reason")
                                .and_then(|r| r.as_str())
                                .unwrap_or("failed")
                                .to_string();
                        }
                        Err(e) => detail = e.to_string(),
                    }
                }
                StepOutcome {
                    step: i,
                    action: "type".into(),
                    ok: done,
                    detail,
                }
            }
            "press" => {
                let key = fill_vars(step.key.as_deref().unwrap_or("Enter"), vars);
                match cdp.press_key(&key).await {
                    Ok(v) if v.get("ok").and_then(|x| x.as_bool()) == Some(true) => StepOutcome {
                        step: i,
                        action: "press".into(),
                        ok: true,
                        detail: format!("pressed {key}"),
                    },
                    Ok(v) => StepOutcome {
                        step: i,
                        action: "press".into(),
                        ok: false,
                        detail: v.to_string(),
                    },
                    Err(e) => StepOutcome {
                        step: i,
                        action: "press".into(),
                        ok: false,
                        detail: e.to_string(),
                    },
                }
            }
            "wait" => {
                let ms = step.ms.unwrap_or(1000).min(60_000);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                StepOutcome {
                    step: i,
                    action: "wait".into(),
                    ok: true,
                    detail: format!("waited {ms}ms"),
                }
            }
            "assert" => {
                let mut found = false;
                for sel in &candidates {
                    if let Ok(v) = cdp
                        .evaluate(&format!(
                            "(() => !!document.querySelector({}))",
                            serde_json::to_string(sel).unwrap_or_default()
                        ))
                        .await
                    {
                        if v.as_bool() == Some(true) {
                            found = true;
                            break;
                        }
                    }
                }
                StepOutcome {
                    step: i,
                    action: "assert".into(),
                    ok: found,
                    detail: if found {
                        format!("found {candidates:?}")
                    } else {
                        "assertion failed".into()
                    },
                }
            }
            other => StepOutcome {
                step: i,
                action: other.into(),
                ok: false,
                detail: format!("unknown action '{other}'"),
            },
        };
        if !outcome.ok {
            ok_all = false;
        }
        outcomes.push(outcome);
    }

    // Capture the final page state so callers can verify the goal.
    let (html, title, _) = cdp.current_dom().await.unwrap_or_default();
    let text = lightbrowse_core::extract::extract_text(&html);

    Ok(RunbookOutcome {
        url: url.to_string(),
        ok: ok_all,
        steps: outcomes,
        final_title: title,
        final_text_preview: text.text.chars().take(2000).collect(),
    })
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
