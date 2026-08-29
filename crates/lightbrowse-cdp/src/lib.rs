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

use std::collections::{HashMap, VecDeque};
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

/// Per-session tab bookkeeping for the resource manager.
#[derive(Clone)]
pub struct TabInfo {
    pub page: ActivePage,
    /// Last activity (navigate/action) — used for LRU eviction.
    pub last_used: Instant,
    /// When the tab was created — for age reporting.
    pub created: Instant,
    /// URL the tab was created/navigated with — used to re-create the tab
    /// after a renderer crash (with_recovery).
    pub url: String,
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
    /// Open tabs, keyed by session id. Each session gets its own tab so
    /// concurrent agents never step on each other's page state.
    tabs: tokio::sync::Mutex<HashMap<String, TabInfo>>,
    trail: Mutex<Vec<TrailStep>>,
    last_used: Mutex<Instant>,
    shutdown: CancellationToken,
    /// Runtime proxy override (set via `set_proxy`). Applied at Chromium
    /// launch; takes precedence over `config.proxy`.
    proxy_override: Mutex<Option<String>>,
    /// Highest Chromium RAM ever observed (MB) — for monitoring.
    peak_ram: std::sync::atomic::AtomicU64,
    /// Navigations performed since start.
    navigations: std::sync::atomic::AtomicU64,
    /// Recent programmatic downloads (last 200): url, saved path, bytes, ts.
    /// Lets agents confirm a download finished and see the final filename
    /// (after Chromium dedupe) instead of polling ~/Downloads (#25).
    recent_downloads: Arc<Mutex<VecDeque<Value>>>,
    /// Bounded capture buffer for the network request log (#29/#31).
    network_log: Arc<Mutex<VecDeque<Value>>>,
    /// Cancellation handle for the active network-capture background task.
    network_capture_token: Mutex<Option<CancellationToken>>,
    /// True while the capture task is running (toggled off on task exit).
    network_capturing: Arc<std::sync::atomic::AtomicBool>,
}

impl CdpBackend {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            inner: tokio::sync::Mutex::new(None),
            tabs: tokio::sync::Mutex::new(HashMap::new()),
            trail: Mutex::new(Vec::new()),
            last_used: Mutex::new(Instant::now()),
            shutdown: CancellationToken::new(),
            proxy_override: Mutex::new(None),
            peak_ram: std::sync::atomic::AtomicU64::new(0),
            navigations: std::sync::atomic::AtomicU64::new(0),
            recent_downloads: Arc::new(Mutex::new(VecDeque::new())),
            network_log: Arc::new(Mutex::new(VecDeque::new())),
            network_capture_token: Mutex::new(None),
            network_capturing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Change the proxy at runtime. The new proxy is applied the next time
    /// Chromium launches; if a browser is already running it is restarted so
    /// the change takes effect immediately. Pass `None` to go direct.
    ///
    /// The URL is validated up front (http/https/socks5/socks5h).
    pub async fn set_proxy(&self, proxy: Option<String>) -> lightbrowse_core::Result<()> {
        if let Some(p) = &proxy {
            lightbrowse_core::proxy::parse_proxy(p)?;
        }
        {
            let mut guard = self
                .proxy_override
                .lock()
                .map_err(|_| lightbrowse_core::Error::Transport("proxy lock poisoned".into()))?;
            if *guard == proxy {
                return Ok(());
            }
            *guard = proxy;
        }
        if self.is_running().await {
            tracing::info!("cdp: proxy changed — restarting Chromium");
            self.reset_browser().await;
        }
        Ok(())
    }

    /// Currently effective proxy URL (`None` = direct).
    pub fn proxy(&self) -> Option<String> {
        self.proxy_override
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.config.proxy.clone())
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
                    // On shutdown (MCP process exit / Drop), kill Chromium so
                    // no headless orphan is left behind (macOS issue).
                    _ = token.cancelled() => {
                        tracing::info!("cdp: shutdown — cleaning up Chromium");
                        this.reset_browser().await;
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                let idle_elapsed = this
                    .last_used
                    .lock()
                    .map(|g| g.elapsed())
                    .unwrap_or_default();
                if idle_elapsed > Duration::from_secs(idle) {
                    this.suspend(idle).await;
                }
            }
        });
    }

    /// Reset browser state: kill any Chromium (alive or zombie), drop all
    /// tabs. Used when a connection dies so the next call starts fresh.
    pub async fn reset_browser(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(mut b) = guard.take() {
            if b.is_alive() {
                b.close_gracefully().await;
            } else {
                tracing::warn!("cdp: Chromium process was already dead — cleaned up");
            }
        }
        self.tabs.lock().await.clear();
    }

    /// Close Chromium gracefully (cookies flushed to the profile) and
    /// release its RAM. Called on idle timeout / shutdown.
    ///
    /// Re-checks `last_used` AFTER acquiring the browser lock: between the
    /// watcher's idle check and this lock, an action may have touched the
    /// backend or a fresh browser may have been spawned after a crash
    /// recovery — we must never kill that one (observed race: tab created
    /// 16:47:29 → suspended 16:47:31, i.e. a 1.8s-old browser killed).
    pub async fn suspend(&self, idle_secs: u64) {
        let mut guard = self.inner.lock().await;
        let still_idle = self
            .last_used
            .lock()
            .map(|g| g.elapsed() > Duration::from_secs(idle_secs))
            .unwrap_or(true);
        if !still_idle {
            tracing::debug!("cdp: idle suspend skipped — activity detected after check");
            return;
        }
        if let Some(b) = guard.take() {
            b.close_gracefully().await;
            self.tabs.lock().await.clear();
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

    /// Peak Chromium RAM observed (MB).
    pub fn peak_ram_mb(&self) -> u64 {
        self.peak_ram.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total navigations performed.
    pub fn navigate_count(&self) -> u64 {
        self.navigations.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn touch(&self) {
        if let Ok(mut g) = self.last_used.lock() {
            *g = Instant::now();
        }
    }

    /// Resource manager — mark a session's tab as recently used.
    async fn touch_tab(&self, session_id: &str) {
        let mut g = self.tabs.lock().await;
        if let Some(t) = g.get_mut(session_id) {
            t.last_used = Instant::now();
        }
    }

    /// Resource manager — current open tab count (per-session).
    pub async fn tab_count(&self) -> usize {
        self.tabs.lock().await.len()
    }

    /// Resource manager — snapshot of open tabs for monitoring (/health).
    pub async fn tabs_snapshot(&self) -> Vec<serde_json::Value> {
        self.tabs
            .lock()
            .await
            .iter()
            .map(|(sid, t)| {
                serde_json::json!({
                    "session": sid,
                    "age_secs": t.created.elapsed().as_secs_f64().round(),
                    "idle_secs": t.last_used.elapsed().as_secs_f64().round(),
                })
            })
            .collect()
    }

    /// Close the tab of one session (manual eviction). Returns an error when
    /// the session has no open tab.
    pub async fn close_tab(&self, session_id: &str) -> Result<()> {
        let page = {
            let mut tabs = self.tabs.lock().await;
            match tabs.remove(session_id) {
                Some(t) => t.page,
                None => {
                    return Err(Error::NotInitialized(format!(
                        "no open tab for session '{session_id}'"
                    )))
                }
            }
        };
        let url = format!(
            "http://127.0.0.1:{}/json/close/{}",
            page.port, page.target_id
        );
        if let Err(e) = reqwest::Client::new().get(&url).send().await {
            tracing::warn!("cdp: close tab {session_id}: {e}");
        }
        tracing::info!("cdp: closed tab of session {session_id}");
        Ok(())
    }

    /// Resource manager — evict the least-recently-used tab (LRU policy),
    /// never the one in `except`. Returns the evicted session id, if any.
    async fn evict_lru_tab(&self, except: Option<&str>) -> Option<String> {
        let victim = {
            let tabs = self.tabs.lock().await;
            tabs.iter()
                .filter(|(sid, _)| Some(sid.as_str()) != except)
                .min_by_key(|(_, t)| t.last_used)
                .map(|(sid, _)| sid.clone())
        };
        if let Some(ref v) = victim {
            self.close_tab(v).await.ok();
        }
        victim
    }

    /// Resource manager — RAM governor: while Chromium RAM exceeds the
    /// budget, evict idle tabs (never the one in `keep`) until we fit or
    /// only the kept tab remains.
    async fn enforce_ram_budget(&self, keep: &str) {
        let mut rounds = 0;
        while self.memory_usage_mb().await > self.config.memory_budget_mb
            && self.tab_count().await > 1
            && rounds < 8
        {
            rounds += 1;
            let ram = self.memory_usage_mb().await;
            match self.evict_lru_tab(Some(keep)).await {
                Some(victim) => tracing::warn!(
                    "cdp: RAM {ram} MB > budget {} MB — evicted LRU tab (session {victim})",
                    self.config.memory_budget_mb
                ),
                None => break,
            }
        }
    }

    /// Resolve the tab an action should target: the named session's tab, or
    /// (when `None`) the most-recently-used tab. Returns (session_id, page).
    /// Run a CDP operation with one self-healing retry. If the tab's
    /// renderer died mid-call (genuine ws reset — e.g. "Connection reset
    /// without closing handshake" after a heavy SPA like Teams OOMs its
    /// renderer), drop the dead tab, re-create it against its last URL and
    /// retry the operation once. Mirrors `navigate`'s reset-and-retry for
    /// the read-only tools (current/screenshot/evaluate).
    async fn with_recovery<T, F, Fut>(&self, session: Option<&str>, op: F) -> Result<T>
    where
        F: Fn(&ActivePage) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let (sid, active) = self.active_page(session).await?;
        match op(&active).await {
            Err(e) if is_recoverable_error(&e) => {
                let last_url = {
                    let tabs = self.tabs.lock().await;
                    tabs.get(&sid).map(|t| t.url.clone()).unwrap_or_default()
                };
                tracing::warn!(
                    "cdp: tab connection died ({e}) — re-creating tab for session {sid} and retrying once"
                );
                self.tabs.lock().await.remove(&sid);
                let active2 = self.ensure_page(&sid, &last_url).await?;
                // The fresh tab starts loading last_url asynchronously — give
                // it a moment to produce a document before the retry runs.
                wait_for_tab_ready(&active2.ws_url).await;
                op(&active2).await
            }
            other => other,
        }
    }

    async fn active_page(&self, session: Option<&str>) -> Result<(String, ActivePage)> {
        let tabs = self.tabs.lock().await;
        match session {
            Some(id) => tabs
                .get(id)
                .map(|t| (id.to_string(), t.page.clone()))
                .ok_or_else(|| {
                    Error::NotInitialized(format!(
                        "no tab for session '{id}' — navigate with engine=cdp first"
                    ))
                }),
            None => tabs
                .iter()
                .max_by_key(|(_, t)| t.last_used)
                .map(|(sid, t)| (sid.clone(), t.page.clone()))
                .ok_or_else(|| {
                    Error::NotInitialized("no active page — navigate with engine=cdp first".into())
                }),
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
    /// aria-label) so replays survive small DOM changes. Searches across
    /// frames so iframe-hosted fields get fallbacks too.
    async fn collect_fallbacks(&self, selector: &str, session: Option<&str>) -> Vec<String> {
        let sel = serde_json::to_string(selector).unwrap_or_default();
        let (_, active) = match self.active_page(session).await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        eval_across_frames_value(
            active.port,
            &active.ws_url,
            &format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null;              const out = [];              if (el.id) out.push('#' + CSS.escape(el.id));              if (el.name) out.push(`[name=\"${{el.name}}\"]`);              if (el.placeholder) out.push(`[placeholder=\"${{el.placeholder}}\"]`);              if (el.getAttribute('aria-label')) out.push(`[aria-label=\"${{el.getAttribute('aria-label')}}\"]`);              return out.slice(0,3); }})()"
            ),
        )
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
impl lightbrowse_core::backend::ProxyControl for CdpBackend {
    async fn set_proxy(&self, proxy: Option<String>) -> Result<()> {
        CdpBackend::set_proxy(self, proxy).await
    }

    fn proxy(&self) -> Option<String> {
        CdpBackend::proxy(self)
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
        let attempt = self.try_navigate(session, url).await;
        if let Err(e) = &attempt {
            // Only surface an error when the FINAL retry fails: on redirect
            // chains (1drv.ms → onedrive.live.com) the DevTools connection
            // routinely closes mid-load while the navigation itself succeeds
            // (#26). Reset Chromium and retry once; verify the page rendered.
            if is_recoverable(e) {
                tracing::warn!("cdp: navigate failed ({e}) — resetting Chromium and retrying once");
                self.reset_browser().await;
                let retried = self.try_navigate(session, url).await;
                if let Ok(p) = &retried {
                    let rendered = !p.html.trim().is_empty() || !p.title.trim().is_empty();
                    if !rendered {
                        tracing::warn!(
                            "cdp: navigate retry succeeded but page looks empty ({} bytes, title {:?})",
                            p.html.len(),
                            p.title
                        );
                    }
                }
                return retried;
            }
        }
        attempt
    }
}

impl CdpBackend {
    /// Single navigation attempt (no retry) — see `navigate`.
    async fn try_navigate(&self, session: &Session, url: &str) -> Result<Page> {
        let active = self.ensure_page(&session.id, url).await?;
        // CDP pages advertise the REAL Chrome version (chrome_version_ua) so
        // version-gated sites (Slack, Google) don't block us — not the
        // session default that may lag behind the installed binary.
        let ua = self
            .inner
            .lock()
            .await
            .as_ref()
            .map(|b| b.user_agent.clone())
            .unwrap_or_else(|| session.user_agent.clone());
        let result = navigate_and_render(
            &active.ws_url,
            url,
            &ua,
            self.config.js_wait_ms,
            self.config.stealth,
            self.config.preload_script.as_deref(),
            self.config.download_dir.as_deref(),
        )
        .await;
        if result.is_ok() {
            let ram = self.memory_usage_mb().await;
            if ram > self.config.memory_budget_mb {
                tracing::warn!(
                    "cdp: actual Chromium RAM {ram} MB exceeds budget {} MB — evicting idle tabs",
                    self.config.memory_budget_mb
                );
                self.enforce_ram_budget(&session.id).await;
            }
        }
        let (html, title, final_url) = result?;
        self.navigations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ram = self.memory_usage_mb().await;
        self.peak_ram
            .fetch_max(ram as u64, std::sync::atomic::Ordering::Relaxed);
        self.touch();
        self.touch_tab(&session.id).await;
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

    /// Make sure Chromium is alive and a tab exists for `session_id`;
    /// returns that session's tab. Enforces the per-tab budget (max_tabs)
    /// with LRU eviction when the limit is hit.
    async fn ensure_page(&self, session_id: &str, url: &str) -> Result<ActivePage> {
        // Mark activity immediately: a navigate/action is starting (or a
        // crash-recovery is re-creating the tab). This makes the idle
        // watcher's re-check in `suspend()` skip killing the browser we're
        // about to use.
        self.touch();
        // The browser struct may be alive while the process is actually dead
        // (crashed/killed) — detect zombies FIRST so we never reuse a dead
        // tab's websocket (macOS: 'Connection refused' / 'closed connection').
        {
            let mut guard = self.inner.lock().await;
            if let Some(b) = guard.as_mut() {
                if !b.is_alive() {
                    drop(guard);
                    tracing::warn!("cdp: Chromium process dead but state alive — resetting");
                    self.reset_browser().await;
                }
            }
        }
        // Reuse this session's tab if it already has one.
        {
            let mut tabs = self.tabs.lock().await;
            if let Some(t) = tabs.get_mut(session_id) {
                t.last_used = Instant::now();
                tracing::debug!("cdp: reusing tab of session {session_id} for {url}");
                return Ok(t.page.clone());
            }
            // Resource manager: cap concurrent tabs (per-tab budget).
            let max_tabs = self.config.max_tabs.clamp(1, 16);
            if tabs.len() >= max_tabs {
                let victim = tabs
                    .iter()
                    .min_by_key(|(_, t)| t.last_used)
                    .map(|(sid, _)| sid.clone());
                drop(tabs);
                if let Some(v) = victim {
                    tracing::warn!(
                        "cdp: {max_tabs} tab limit reached — evicting LRU tab (session {v})"
                    );
                    self.close_tab(&v).await.ok();
                }
            }
        }
        tracing::debug!("cdp: creating new tab for {url}");
        let port = {
            let mut guard = self.inner.lock().await;
            if guard.is_none() {
                tracing::info!("cdp: spawning headless Chromium (lazy)");
                // Runtime proxy override (if any) wins over config.
                let mut cfg = self.config.clone();
                if let Ok(p) = self.proxy_override.lock() {
                    if let Some(proxy) = p.clone() {
                        cfg.proxy = Some(proxy);
                    }
                }
                // Spawn with one internal retry: right after a hard-killed
                // server, the profile can briefly contend with the dying
                // Chromium (stale socket / port race) and the endpoint may
                // not come up in time. The failed child is already killed by
                // spawn(), so a retry is safe and virtually always succeeds.
                let mut spawned = match &self.config.cdp_url {
                    Some(endpoint) => {
                        tracing::info!("cdp: attaching to existing Chrome at {endpoint}");
                        CdpBrowser::attach(endpoint).await
                    }
                    None => CdpBrowser::spawn(&cfg, self.shutdown.clone()).await,
                };
                if spawned.is_err() && self.config.cdp_url.is_none() {
                    tracing::warn!(
                        "cdp: Chromium spawn failed — retrying once after a short pause"
                    );
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    spawned = CdpBrowser::spawn(&cfg, self.shutdown.clone()).await;
                }
                *guard = Some(spawned?);
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
        self.tabs.lock().await.insert(
            session_id.to_string(),
            TabInfo {
                page: active.clone(),
                last_used: Instant::now(),
                created: Instant::now(),
                url: url.to_string(),
            },
        );
        tracing::info!(
            "cdp: tab created for session {session_id} (target={})",
            active.target_id
        );
        Ok(active)
    }

    /// The URL the currently-targeted tab is showing (via JS).
    pub async fn current_url(&self, session: Option<&str>) -> Option<String> {
        let (sid, active) = self.active_page(session).await.ok()?;
        let v = evaluate_js(&active.ws_url, "location.href").await.ok()?;
        self.touch_tab(&sid).await;
        v.as_str().map(|s| s.to_string())
    }

    /// Evaluate JS on the targeted tab; returns the result value.
    pub async fn evaluate(&self, expr: &str, session: Option<&str>) -> Result<Value> {
        let v = self
            .with_recovery(session, |active| {
                let ws_url = active.ws_url.clone();
                let expr = expr.to_string();
                async move { evaluate_js(&ws_url, &expr).await }
            })
            .await
            .inspect_err(|_| {
                tracing::warn!("cdp: evaluate called with no active page");
            })?;
        if let Ok((sid, _)) = self.active_page(session).await {
            self.touch();
            self.touch_tab(&sid).await;
        }
        Ok(v)
    }

    /// All cookies visible to the browser session (including httpOnly and
    /// SameSite), via CDP `Network.getAllCookies` on the active tab. Lets
    /// agents export a session to replay it with curl or another tool —
    /// `document.cookie` only exposes non-httpOnly cookies.
    pub async fn cookies(&self, session: Option<&str>) -> Result<Value> {
        let (sid, active) = self.active_page(session).await?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        let result = rpc_expect(&mut ws, "Network.getAllCookies", json!({})).await;
        let _ = ws.close(None).await;
        let v = result?;
        self.touch();
        self.touch_tab(&sid).await;
        Ok(v.get("cookies").cloned().unwrap_or(Value::Null))
    }

    /// Trigger a programmatic download of `url` on the active tab and wait
    /// for the file to land in the configured download directory.
    /// Returns the saved path and size, or a descriptive error.
    pub async fn download(
        &self,
        url: &str,
        filename: Option<&str>,
        session: Option<&str>,
    ) -> Result<Value> {
        let (sid, active) = self.active_page(session).await?;
        let dir = self
            .config
            .download_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("lightbrowse-downloads"));
        std::fs::create_dir_all(&dir).ok();
        // Files present before the click — the new download is any file that
        // appears after this snapshot.
        let before: std::collections::HashSet<std::path::PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        // Set the download behavior on THIS connection, right before the
        // click — and KEEP the connection open until the file lands. Chrome
        // decides the save path asynchronously after the click; if the
        // connection that called setDownloadBehavior closes first, the
        // behavior reverts and the file silently lands in ~/Downloads
        // instead (verified live on Chrome 149).
        match rpc_expect(
            &mut ws,
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allow", "downloadPath": dir.display().to_string() }),
        )
        .await
        {
            Ok(v) => tracing::debug!("cdp: download(): setDownloadBehavior ok ({v})"),
            Err(e) => tracing::warn!("cdp: download: setDownloadBehavior failed: {e}"),
        }
        let eval_res = rpc_expect(
            &mut ws,
            "Runtime.evaluate",
            json!({
                "expression": format!(
                    "(() => {{ let a = document.getElementById('__lb_download_anchor'); \
                     if (!a) {{ a = document.createElement('a'); a.id = '__lb_download_anchor'; \
                     a.style.cssText = 'position:fixed;left:0;top:0;width:1px;height:1px;opacity:0;'; \
                     document.body.appendChild(a); }} \
                     a.href = {url_json}; a.download = {file_json}; \
                     a.dispatchEvent(new MouseEvent('click', {{bubbles: true, cancelable: true, view: window}})); \
                     return 'triggered'; }})()",
                    url_json = serde_json::to_string(url).unwrap_or_default(),
                    file_json = serde_json::to_string(filename.unwrap_or("")).unwrap_or_default()
                ),
                "returnByValue": true,
                "awaitPromise": true
            }),
        )
        .await;
        match &eval_res {
            Ok(v) => tracing::debug!("cdp: download click triggered on {}: {v}", active.ws_url),
            Err(e) => tracing::warn!("cdp: download click evaluate failed: {e}"),
        }
        let _ = eval_res?;
        self.touch();
        self.touch_tab(&sid).await;
        // Poll the download dir for a new file (up to 60s) while keeping the
        // ws open so the download behavior stays in effect.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = Err(Error::Transport(format!(
            "download: no file appeared in {} within 60s",
            dir.display()
        )));
        while Instant::now() < deadline {
            let now: std::collections::HashSet<std::path::PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
                .unwrap_or_default();
            let fresh: Vec<_> = now
                .difference(&before)
                .filter(|p| !p.extension().map(|e| e == "crdownload").unwrap_or(false))
                .collect();
            if !fresh.is_empty() {
                let path = fresh[0].clone();
                // Wait for the file to finish (size stable across 2 reads —
                // Chrome writes in chunks; a partial read would report a
                // truncated size).
                let mut stable = 0usize;
                let mut prev_size = 0u64;
                for _ in 0..8 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if size == prev_size && size > 0 {
                        stable += 1;
                        if stable >= 2 {
                            break;
                        }
                    } else {
                        stable = 0;
                        prev_size = size;
                    }
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                // Honor the requested filename by renaming after the download
                // completes — the `download` attribute is only a hint and is
                // ignored on cross-origin URLs, so renaming is deterministic.
                let final_path = match filename {
                    Some(name)
                        if !name.is_empty()
                            && name
                                != path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default() =>
                    {
                        let target = dir.join(name);
                        let target = if target.exists() {
                            // avoid overwriting an existing file with a (N) suffix
                            let stem = name.rsplit('.').next().unwrap_or(name);
                            let ext = name
                                .rsplit('.')
                                .nth(1)
                                .map(|e| format!(".{e}"))
                                .unwrap_or_default();
                            let mut n = 1usize;
                            loop {
                                let cand = dir.join(format!("{stem} ({n}){ext}"));
                                if !cand.exists() {
                                    break cand;
                                }
                                n += 1;
                            }
                        } else {
                            target
                        };
                        let _ = std::fs::rename(&path, &target);
                        target
                    }
                    _ => path,
                };
                let size = std::fs::metadata(&final_path)
                    .map(|m| m.len())
                    .unwrap_or(size);
                last = Ok(json!({
                    "ts": now_ms(),
                    "url": url,
                    "saved": final_path.display().to_string(),
                    "bytes": size,
                    "dir": dir.display().to_string()
                }));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let _ = ws.close(None).await;
        if let Ok(event) = &last {
            let mut log = self
                .recent_downloads
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if log.len() >= 200 {
                log.pop_front();
            }
            log.push_back(event.clone());
        }
        last
    }

    /// Recent programmatic downloads (last 200), newest first. Each entry:
    /// ts, url, saved (final filename after Chromium dedupe), bytes, dir.
    pub fn downloads(&self) -> Vec<Value> {
        let log = self
            .recent_downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        log.iter().rev().cloned().collect()
    }

    /// Start (`enable=true`) or stop (`enable=false`) a network capture on
    /// the active tab. While capturing, a background task holds a DevTools
    /// connection open and records requests/responses/failures (url, method,
    /// status, mime) into a bounded ring buffer — for SPA API discovery
    /// (#31) and auth-flow analysis (#29). Returns capture status + count.
    pub async fn network_capture(&self, enable: bool, session: Option<&str>) -> Result<Value> {
        let (sid, active) = self.active_page(session).await?;
        if !enable {
            let mut token_guard = self
                .network_capture_token
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(t) = token_guard.take() {
                t.cancel();
            }
            drop(token_guard);
            self.network_capturing.store(false, Ordering::Relaxed);
            let n = self
                .network_log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            return Ok(json!({ "capturing": false, "events": n }));
        }
        if self.network_capturing.load(Ordering::Relaxed) {
            let n = self
                .network_log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            return Ok(json!({ "capturing": true, "events": n }));
        }
        let token = CancellationToken::new();
        {
            let mut token_guard = self
                .network_capture_token
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *token_guard = Some(token.clone());
        }
        self.network_capturing.store(true, Ordering::Relaxed);
        let ws_url = active.ws_url.clone();
        let log: Arc<Mutex<VecDeque<Value>>> = self.network_log.clone();
        let capturing = self.network_capturing.clone();
        self.touch_tab(&sid).await;
        tokio::spawn(async move {
            let res = network_capture_loop(&ws_url, &log, &token).await;
            if let Err(e) = res {
                tracing::warn!("cdp: network capture ended: {e}");
            }
            capturing.store(false, Ordering::Relaxed);
        });
        Ok(json!({ "capturing": true, "events": 0 }))
    }

    /// Current network capture buffer (newest first) + capture status.
    pub fn network_log(&self) -> Vec<Value> {
        let log = self.network_log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter().rev().cloned().collect()
    }

    /// Whether a network capture is currently running.
    pub fn network_capturing(&self) -> bool {
        self.network_capturing.load(Ordering::Relaxed)
    }

    /// Clear the network capture buffer.
    pub fn network_log_clear(&self) {
        self.network_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    /// pressed → released at the element's center), which is far harder for
    /// anti-bot systems to flag than `el.click()`. Works across iframes
    /// (selectors are resolved in every frame; coordinates are viewport
    /// based so the CDP mouse events land correctly).
    pub async fn click(&self, selector: &str, session: Option<&str>) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let (sid, active) = self.active_page(session).await?;
        let fe = eval_across_frames(
            active.port,
            &active.ws_url,
            &format!(
                "(() => {{ const els = document.querySelectorAll({sel}); \
                 for (const el of els) {{ \
                   const r = el.getBoundingClientRect(); \
                   if (r.width < 1 || r.height < 1) continue; \
                   const cs = getComputedStyle(el); \
                   if (cs.visibility === 'hidden' || cs.display === 'none') continue; \
                   el.scrollIntoView({{block:'center'}}); \
                   const r2 = el.getBoundingClientRect(); \
                   return {{x: r2.x + r2.width/2, y: r2.y + r2.height/2, tag: el.tagName.toLowerCase()}}; \
                 }} \
                 return null; }})()"
            ),
        )
        .await?;
        let point = &fe.value;
        let x = point.get("x").and_then(|v| v.as_f64());
        let y = point.get("y").and_then(|v| v.as_f64());
        let Some((mut x, mut y)) = x.zip(y) else {
            return Ok(json!({"ok": false, "reason": "element not found"}));
        };
        // When the element lives in an iframe, its rect is frame-relative —
        // add the iframe's own offset in the top viewport so the mouse event
        // lands on the real element.
        if let Some(fid) = &fe.frame_id {
            tracing::debug!("cdp: element found in frame {fid}");
            if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&active.ws_url).await {
                if let Some((ox, oy)) = iframe_viewport_offset(&mut ws, fid).await {
                    tracing::debug!("cdp: iframe offset ({ox}, {oy})");
                    x += ox;
                    y += oy;
                } else {
                    tracing::warn!("cdp: could not compute iframe offset for frame {fid}");
                }
                let _ = ws.close(None).await;
            }
        }
        let tag = point
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
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
        self.touch_tab(&sid).await;
        let fallbacks = self.collect_fallbacks(selector, session).await;
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

    /// Click at raw viewport coordinates (CSS px, top-left origin) — the
    /// "human pointing" action for SoM/vision agents. Dispatches a real
    /// mouseMoved → pressed → released sequence at the given point.
    pub async fn click_at(&self, x: f64, y: f64, session: Option<&str>) -> Result<Value> {
        let (sid, active) = self.active_page(session).await?;
        if x < 0.0 || y < 0.0 {
            return Ok(json!({ "ok": false, "reason": "coordinates must be >= 0" }));
        }
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        let (rx, ry) = (x.round() as u64, y.round() as u64);
        for (typ, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            rpc_expect(
                &mut ws,
                "Input.dispatchMouseEvent",
                json!({
                    "type": typ,
                    "x": rx,
                    "y": ry,
                    "button": "left",
                    "buttons": buttons,
                    "clickCount": 1
                }),
            )
            .await?;
        }
        let _ = ws.close(None).await;
        self.touch();
        self.touch_tab(&sid).await;
        Ok(json!({ "ok": true, "x": rx, "y": ry }))
    }

    /// Get viewport bounding boxes for a batch of CSS selectors in ONE JS
    /// pass. Returns a map {selector: [x, y, w, h]} for elements that are
    /// visible and non-zero-sized. Selectors are top-document paths (the
    /// same ones the snapshot tree emits).
    pub async fn element_rects(
        &self,
        selectors: &[String],
        session: Option<&str>,
    ) -> Result<std::collections::HashMap<String, [f64; 4]>> {
        if selectors.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let list = serde_json::to_string(selectors)
            .map_err(|e| Error::Parse(format!("element_rects encode: {e}")))?;
        let expr = format!(
            "(() => {{ const sels = {list}; const out = {{}}; \
             for (const s of sels) {{ \
               const el = document.querySelector(s); if (!el) continue; \
               const r = el.getBoundingClientRect(); \
               if (r.width < 1 || r.height < 1) continue; \
               const cs = getComputedStyle(el); \
               if (cs.visibility === 'hidden' || cs.display === 'none') continue; \
               out[s] = [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; \
             }} \
             return out; }})()"
        );
        let v = self.evaluate(&expr, session).await?;
        let mut out = std::collections::HashMap::new();
        if let Some(obj) = v.as_object() {
            for (sel, arr) in obj {
                if let Some(a) = arr.as_array() {
                    let vals: Vec<f64> = a.iter().filter_map(|x| x.as_f64()).collect();
                    if vals.len() == 4 {
                        out.insert(sel.clone(), [vals[0], vals[1], vals[2], vals[3]]);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Type text like a real keyboard (CDP `Input.insertText` — fires genuine
    /// input events; React and anti-bot both see a human typing).
    pub async fn type_text(
        &self,
        selector: &str,
        text: &str,
        session: Option<&str>,
    ) -> Result<Value> {
        // Focus the field first with a real click.
        let clicked = self.click(selector, session).await?;
        if clicked.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Ok(clicked);
        }
        let (sid, active) = self.active_page(session).await?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&active.ws_url)
            .await
            .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
        rpc_expect(&mut ws, "Input.insertText", json!({ "text": text })).await?;
        let _ = ws.close(None).await;
        self.touch();
        self.touch_tab(&sid).await;
        // Confirm what landed in the field (may live in an iframe).
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let value = eval_across_frames_value(
            active.port,
            &active.ws_url,
            &format!(
                "(() => {{ const el = document.querySelector({sel}); return el ? el.value : null; }})()"
            ),
        )
        .await?;
        let fallbacks = self.collect_fallbacks(selector, session).await;
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
    pub async fn press_key(&self, key: &str, session: Option<&str>) -> Result<Value> {
        let (key_code, text) = match key {
            "Enter" => (13, "\r"),
            "Tab" => (9, "\t"),
            "Backspace" => (8, "\u{8}"),
            "Escape" => (27, "\u{1b}"),
            "ArrowDown" => (40, ""),
            "ArrowUp" => (38, ""),
            other => return Err(Error::Unsupported(format!("unsupported key '{other}'"))),
        };
        let (sid, active) = self.active_page(session).await?;
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
        self.touch_tab(&sid).await;
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

    pub async fn submit(&self, selector: &str, session: Option<&str>) -> Result<Value> {
        let sel = serde_json::to_string(selector).map_err(|e| Error::Parse(e.to_string()))?;
        let (_, active) = self.active_page(session).await?;
        eval_across_frames_value(
            active.port,
            &active.ws_url,
            &format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null;              const form = el.tagName === 'FORM' ? el : el.form; if (!form) return {{ok:false, reason:'no parent form'}};              form.requestSubmit(); return {{ok:true, action: form.action || null}}; }})()"
            ),
        )
        .await
    }

    /// Capture the targeted tab as a PNG. `full_page` stitches the whole
    /// scrollable document (requires `captureBeyondViewport`).
    pub async fn screenshot(
        &self,
        path: &std::path::Path,
        full_page: bool,
        session: Option<&str>,
    ) -> Result<std::path::PathBuf> {
        let out = self
            .with_recovery(session, |active| {
                let ws_url = active.ws_url.clone();
                let path = path.to_path_buf();
                async move {
                    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
                        .await
                        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;

                    // Stretch the viewport to the full content height for full-page shots.
                    if full_page {
                        let _ = rpc_expect(
                            &mut ws,
                            "Emulation.setDeviceMetricsOverride",
                            json!({
                                "width": 1280,
                                "height": 1000,
                                "deviceScaleFactor": 1,
                                "mobile": false
                            }),
                        )
                        .await;
                        let _ = rpc_expect(
                            &mut ws,
                            "Page.setDeviceMetricsOverride",
                            json!({ "width": 1280, "height": 1000 }),
                        )
                        .await;
                    }

                    let result = rpc_expect(
                        &mut ws,
                        "Page.captureScreenshot",
                        json!({
                            "format": "png",
                            "captureBeyondViewport": full_page,
                            "fromSurface": true
                        }),
                    )
                    .await?;
                    let b64 = result
                        .get("data")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| Error::Parse("screenshot: no data returned".into()))?;

                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| Error::Parse(format!("screenshot: base64: {e}")))?;

                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    std::fs::write(&path, &bytes).map_err(Error::Io)?;
                    let _ = ws.close(None).await;
                    Ok(path)
                }
            })
            .await?;
        if let Ok((sid, _)) = self.active_page(session).await {
            self.touch();
            self.touch_tab(&sid).await;
        }
        Ok(out)
    }

    /// Serialize the targeted tab's rendered DOM (html, title, url).
    /// Child-frame (iframe) HTML is appended so agents can see login forms
    /// hosted in iframes (e.g. Microsoft's fpt.live.com).
    pub async fn current_dom(&self, session: Option<&str>) -> Result<(String, String, String)> {
        let (html, title, url) = self
            .with_recovery(session, |active| {
                let ws_url = active.ws_url.clone();
                let port = active.port;
                async move {
                    let expr = "JSON.stringify({title: document.title, url: location.href, html: document.documentElement.outerHTML})";
                    let v = evaluate_js(&ws_url, expr).await?;
                    let parsed: Value = serde_json::from_str(v.as_str().unwrap_or("{}"))
                        .map_err(|e| Error::Parse(e.to_string()))?;
                    let mut html = parsed
                        .get("html")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Append iframe contents so snapshot/ask see frame-hosted fields.
                    let frames = collect_frame_htmls(port, &ws_url).await;
                    if !frames.is_empty() {
                        html.push_str(&format!("<div data-lb-frames=\"{}:\">", frames.len()));
                        for (i, fh) in frames.iter().enumerate() {
                            html.push_str(&format!("<div data-lb-frame=\"{i}\">{fh}</div>"));
                        }
                        html.push_str("</div>");
                    }
                    Ok((
                        html,
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
            })
            .await?;
        if let Ok((sid, _)) = self.active_page(session).await {
            self.touch();
            self.touch_tab(&sid).await;
        }
        Ok((html, title, url))
    }
}

// ---------------------------------------------------------------------------
// Browser process
// ---------------------------------------------------------------------------

struct CdpBrowser {
    /// The spawned Chrome process. `None` in attach mode (external Chrome
    /// via `--cdp-url`) — we must never kill the user's browser.
    child: Option<Child>,
    port: u16,
    browser_ws: String,
    /// True when attached to an external Chrome instead of spawning one.
    attached: bool,
    /// Effective User-Agent: matches the actual Chrome binary version so
    /// sites (Slack, Google) don't reject us as an "old browser" — see
    /// `chrome_version_ua`. Overridable via LIGHTBROWSE_UA.
    user_agent: String,
    _user_data_dir: std::path::PathBuf,
}

impl CdpBrowser {
    async fn spawn(config: &Config, shutdown: CancellationToken) -> Result<Self> {
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
        // A hard-killed previous server (SIGKILL) leaves its Chromium orphaned
        // and holding the profile's SingletonLock — the next spawn then hangs
        // ("Chrome DevTools endpoint did not come up in 20s"). Reap orphans
        // and clear stale locks before launching.
        clean_stale_profile_locks(&user_data);
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
        // Route all Chromium traffic through the configured proxy. The URL is
        // normalized (e.g. socks5h → socks5, default ports applied) so Chrome
        // always gets a well-formed `--proxy-server` value.
        if let Some(proxy) = &config.proxy {
            match lightbrowse_core::proxy::parse_proxy(proxy) {
                Ok(spec) => {
                    tracing::info!("cdp: routing via proxy {}", spec.describe());
                    cmd.arg(format!("--proxy-server={}", spec.chrome_arg()));
                }
                Err(e) => {
                    tracing::warn!("cdp: ignoring invalid proxy '{proxy}': {e}");
                }
            }
        }
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
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("failed to launch Chrome ({chrome}): {e}")))?;

        // Wait for the DevTools endpoint to come up.
        let http = reqwest::Client::new();
        let browser_ws = match wait_for_devtools(&http, port).await {
            Ok(ws) => ws,
            // Do NOT leak the freshly launched Chrome: a spawn failure (e.g.
            // endpoint timeout after a profile-lock conflict) must kill the
            // child, otherwise every retry accumulates another Chromium that
            // keeps poisoning the profile and times out forever.
            Err(e) => {
                tracing::error!("cdp: spawn failed ({e}) — killing freshly launched Chrome");
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };

        // Log renderer crashes (Target.targetCrashed) so "Connection reset
        // without closing handshake" failures can be attributed to a real
        // renderer death (e.g. heavy SPA OOM) instead of guessed network
        // issues. Runs until the backend shuts down.
        spawn_crash_listener(browser_ws.clone(), shutdown);

        Ok(Self {
            child: Some(child),
            port,
            browser_ws,
            attached: false,
            // Prefer an explicit override, else match the real binary version.
            user_agent: std::env::var("LIGHTBROWSE_UA")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| chrome_version_ua(&chrome))
                .unwrap_or_else(|| lightbrowse_core::session::DEFAULT_UA.to_string()),
            _user_data_dir: user_data,
        })
    }

    /// Attach to an EXISTING Chrome/Chromium DevTools endpoint instead of
    /// spawning our own headless instance. The user's browser may already
    /// have logins (cookies/localStorage) — reusing it skips every SSO flow.
    /// We never kill or close the external browser.
    ///
    /// Accepts `http://host:port` (probed via `/json/version`) or a direct
    /// `ws://.../devtools/browser/...` URL.
    async fn attach(endpoint: &str) -> Result<Self> {
        let endpoint = endpoint.trim();
        let (http_base, browser_ws) =
            if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
                // Derive an http base from the ws URL for /json/new calls.
                let http_base = endpoint
                    .replace("ws://", "http://")
                    .replace("wss://", "https://")
                    .trim_end_matches("/devtools/browser")
                    .trim_end_matches("/devtools/page")
                    .to_string();
                (http_base, endpoint.to_string())
            } else {
                let base = endpoint
                    .trim_end_matches('/')
                    .strip_prefix("http://")
                    .or_else(|| endpoint.strip_prefix("https://"))
                    .map(|s| format!("http://{}", s))
                    .unwrap_or_else(|| format!("http://{}", endpoint));
                let http = reqwest::Client::new();
                let version: Value = http
                    .get(format!("{base}/json/version"))
                    .send()
                    .await
                    .map_err(|e| Error::Transport(format!("cdp attach: cannot reach {base}: {e}")))?
                    .json()
                    .await
                    .map_err(|e| {
                        Error::Transport(format!("cdp attach: {base} not a DevTools endpoint: {e}"))
                    })?;
                let ws = version
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        Error::Transport(format!(
                            "cdp attach: no webSocketDebuggerUrl in {base}/json/version"
                        ))
                    })?
                    .to_string();
                (base, ws)
            };

        // Verify the endpoint is actually reachable before adopting it.
        let (mut ws, _) = tokio_tungstenite::connect_async(&browser_ws)
            .await
            .map_err(|e| Error::Transport(format!("cdp attach: connect {browser_ws}: {e}")))?;
        let ver = rpc_expect(&mut ws, "Browser.getVersion", json!({})).await;
        let _ = ws.close(None).await;
        let version = match ver {
            Ok(v) => v
                .get("product")
                .and_then(|p| p.as_str())
                .unwrap_or("external")
                .to_string(),
            Err(_) => "external".to_string(),
        };
        tracing::info!("cdp: attached to external Chrome ({version})");

        let port = http_base
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        Ok(Self {
            child: None,
            port,
            browser_ws,
            attached: true,
            user_agent: std::env::var("LIGHTBROWSE_UA")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| lightbrowse_core::session::DEFAULT_UA.to_string()),
            _user_data_dir: std::path::PathBuf::new(),
        })
    }

    /// Graceful CDP `Browser.close` first (flushes cookies to the profile),
    /// then SIGKILL as a fallback. Without this, logins are lost.
    /// In attach mode the external browser is NEVER closed or killed — we
    /// simply drop our connection.
    async fn close_gracefully(mut self) {
        if self.attached {
            tracing::debug!("cdp: attached mode — not closing external Chrome");
            return;
        }
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&self.browser_ws).await {
            let _ = ws
                .send(WsMessage::Text(
                    json!({"id": next_id(), "method": "Browser.close"}).to_string(),
                ))
                .await;
            // Give Chrome a moment to flush and exit.
            for _ in 0..20 {
                if let Some(child) = self.child.as_mut() {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let _ = ws.close(None).await;
        }
        tracing::warn!("cdp: Browser.close timed out — killing Chromium");
        self.kill();
    }

    /// Is the Chromium process still running? (try_wait needs &mut Child)
    /// In attach mode we can't see the external process — assume alive.
    fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_none(),
            None => true, // attached: external browser state is not ours to check
        }
    }

    fn kill(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn memory_usage_mb(&self) -> usize {
        let Some(pid) = self.child.as_ref().map(|c| c.id()) else {
            return 0; // attached: external process RAM is not ours to measure
        };
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

/// Poll a tab until it has a document (readyState != "loading", max ~5s).
/// Used after re-creating a tab in `with_recovery` so the retried
/// read-only op sees real content instead of an about:blank mid-load page.
async fn wait_for_tab_ready(ws_url: &str) {
    for _ in 0..10 {
        let ready = evaluate_js(
            ws_url,
            "document.readyState === 'loading' ? null : document.readyState",
        )
        .await
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()));
        if ready.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Build a stock-Chrome UA whose major version matches the installed binary
/// (e.g. `google-chrome --version` → "Google Chrome 149.0.7827.200" →
/// Chrome/149.0.0.0). Sites like Slack reject UAs that are too old, even
/// though the real browser is recent — this keeps the advertised version in
/// lockstep with reality (and consistent with UA-CH brands).
fn chrome_version_ua(chrome: &str) -> Option<String> {
    let out = std::process::Command::new(chrome)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // "Google Chrome 149.0.7827.200" | "Chromium 149.0.0.0" | "Mozilla ... Chrome/149.0"
    let major = s
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?
        .split('.')
        .next()?
        .parse::<u32>()
        .ok()?;
    if !(70..=999).contains(&major) {
        return None;
    }
    let platform = if cfg!(windows) {
        "(Windows NT 10.0; Win64; x64)"
    } else if cfg!(target_os = "macos") {
        "(Macintosh; Intel Mac OS X 10_15_7)"
    } else {
        "(X11; Linux x86_64)"
    };
    Some(format!(
        "Mozilla/5.0 {platform} AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

fn detect_chrome() -> Option<String> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    // Unix: look up browser names via `which`.
    #[cfg(not(windows))]
    {
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
    }
    // Windows: `where.exe` + well-known install paths (Chrome/Edge).
    #[cfg(windows)]
    {
        for name in ["chrome", "msedge", "chromium"] {
            if let Ok(out) = std::process::Command::new("where.exe").arg(name).output() {
                if out.status.success() {
                    if let Ok(s) = String::from_utf8(out.stdout) {
                        let p = s.lines().next().unwrap_or("").trim().to_string();
                        if !p.is_empty() {
                            return Some(p);
                        }
                    }
                }
            }
        }
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
        let pfx86 = std::env::var("ProgramFiles(x86)")
            .unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
        for p in [
            format!("{pf}\\Google\\Chrome\\Application\\chrome.exe"),
            format!("{pfx86}\\Google\\Chrome\\Application\\chrome.exe"),
            format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe"),
            format!("{pfx86}\\Microsoft\\Edge\\Application\\msedge.exe"),
        ] {
            if std::path::Path::new(&p).exists() {
                return Some(p);
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
    Err(Error::Transport(format!(
        "Chrome DevTools endpoint did not come up in 20s (port {port})"
    )))
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
    stealth: bool,
    preload_script: Option<&std::path::Path>,
    download_dir: Option<&std::path::Path>,
) -> Result<(String, String, String)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;

    rpc_expect(&mut ws, "Page.enable", json!({})).await?;
    stealth_setup(&mut ws, user_agent, stealth, preload_script).await?;

    // Allow programmatic downloads (anchor clicks / the `download` tool)
    // without Chromium's "multiple automatic downloads" gate, and route
    // them to a deterministic directory. Env: LIGHTBROWSE_DOWNLOAD_DIR.
    // NOTE: use behavior "allow", NOT "allowAndName" — on Chrome 149
    // allowAndName silently renames every file to a GUID (verified live).
    if let Some(dir) = download_dir {
        match rpc_expect(
            &mut ws,
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allow", "downloadPath": dir.display().to_string() }),
        )
        .await
        {
            Ok(v) => tracing::debug!(
                "cdp: setDownloadBehavior allow -> {} (result {v})",
                dir.display()
            ),
            Err(e) => tracing::warn!("cdp: setDownloadBehavior failed: {e}"),
        }
    } else {
        tracing::debug!("cdp: no download dir configured — downloads go to Chrome default");
    }

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
        // 1s idle is normal mid-load (redirect chains, TLS, silent-auth
        // round-trips) — keep waiting; only a real close aborts. Previously
        // any 1s gap errored out as "connection closed during load" and
        // triggered a full Chromium restart on healthy sessions.
        match tokio::time::timeout(Duration::from_secs(1), ws.next()).await {
            Err(_) => continue,
            Ok(None) => {
                return Err(Error::Transport(
                    "cdp: connection closed during load".into(),
                ))
            }
            Ok(Some(Err(e))) => return Err(Error::Transport(format!("cdp recv: {e}"))),
            Ok(Some(Ok(WsMessage::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("method").and_then(|m| m.as_str()) == Some("Page.loadEventFired") {
                    loaded = true;
                    break;
                }
            }
            Ok(Some(Ok(WsMessage::Ping(p)))) => {
                let _ = ws.send(WsMessage::Pong(p)).await;
            }
            Ok(Some(Ok(_))) => {}
        }
    }

    // Extra grace for lazy JS frameworks.
    if js_wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(js_wait_ms)).await;
    }

    // SPA settle: client-rendered frameworks (React/Vue/Next) mount their
    // DOM AFTER Page.loadEventFired, so serializing right away yields an
    // empty shell (see #27). Poll body-text length: if it grew between two
    // samples the page is still rendering — keep waiting until stable (2
    // equal reads) or the cap expires. Static pages are stable on the first
    // two samples (~0.8s extra) and skip the wait.
    let settle_cap = Duration::from_secs(12);
    let settle_start = Instant::now();
    let mut prev_len: Option<usize> = None;
    let mut stable_rounds = 0u32;
    loop {
        if settle_start.elapsed() >= settle_cap {
            break;
        }
        let len = rpc_expect(
            &mut ws,
            "Runtime.evaluate",
            json!({
                "expression": "document.body ? document.body.innerText.length : 0",
                "returnByValue": true
            }),
        )
        .await
        .ok()
        .and_then(|r| {
            r.get("result")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_u64())
        })
        .map(|n| n as usize)
        .unwrap_or(0);
        match prev_len {
            Some(p) if p == len => {
                stable_rounds += 1;
                if stable_rounds >= 2 {
                    break;
                }
            }
            _ => {
                stable_rounds = 0;
                prev_len = Some(len);
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
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
async fn stealth_setup(
    ws: &mut CdpWs,
    user_agent: &str,
    stealth: bool,
    preload_script: Option<&std::path::Path>,
) -> Result<()> {
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
    Object.defineProperty(navigator, 'hardwareConcurrency', {{
        get: () => 8,
    }});
    Object.defineProperty(navigator, 'deviceMemory', {{
        get: () => 8,
    }});
}})();
"#,
        ua_json = serde_json::to_string(user_agent).map_err(|e| Error::Parse(e.to_string()))?
    );
    if stealth {
        let _ = rpc_expect(
            ws,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": stealth_js }),
        )
        .await;
    }
    // Optional user preload hook (--preload / LIGHTBROWSE_PRELOAD): runs
    // before the page's own scripts, e.g. fetch/XHR wrappers for API
    // discovery (see #31).
    if let Some(path) = preload_script {
        match std::fs::read_to_string(path) {
            Ok(source) => {
                let _ = rpc_expect(
                    ws,
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": source }),
                )
                .await;
            }
            Err(e) => {
                tracing::warn!("cdp: preload script {} unreadable: {e}", path.display());
            }
        }
    }
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

/// Result of an across-frames evaluation: the value plus the id of the
/// frame that produced it (`None` = main frame).
struct FrameEval {
    frame_id: Option<String>,
    value: Value,
}

/// Execute `expression` in the main frame first, then in every child frame
/// (iframe), returning the first non-null value and its frame. Lets
/// selectors reach elements inside iframes — e.g. Microsoft's
/// `fpt.live.com` login fields, invisible to the top-level document.
/// Cross-origin iframes (OOPIFs) are discovered via Target.getTargets as a
/// last resort — they never appear in Page.getFrameTree.
async fn eval_across_frames(port: u16, ws_url: &str, expression: &str) -> Result<FrameEval> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;

    // Main frame first (no contextId = default context).
    if let Ok(v) = rpc_expect(
        &mut ws,
        "Runtime.evaluate",
        json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
    )
    .await
    {
        if let Some(val) = v.pointer("/result/value") {
            if !val.is_null() && val.as_str() != Some("undefined") {
                let _ = ws.close(None).await;
                return Ok(FrameEval {
                    frame_id: None,
                    value: val.clone(),
                });
            }
        }
    }

    // Child frames via isolated worlds.
    // CDP returns { frameTree: { frame: {...}, childFrames: [...] } }.
    let tree = rpc_frame_tree(&mut ws).await?;
    let main_id = tree
        .pointer("/frameTree/frame/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut frames: Vec<String> = Vec::new();
    fn walk_frames(node: &Value, out: &mut Vec<String>, main: &str) {
        if let Some(id) = node.pointer("/frame/id").and_then(|v| v.as_str()) {
            if id != main {
                out.push(id.to_string());
            }
        }
        if let Some(children) = node.get("childFrames").and_then(|c| c.as_array()) {
            for c in children {
                walk_frames(c, out, main);
            }
        }
    }
    if let Some(ft) = tree.get("frameTree") {
        walk_frames(ft, &mut frames, &main_id);
    }

    for fid in frames {
        // NOTE: "grantUniveralAccess" is CDP's historical spelling.
        let world = rpc_expect(
            &mut ws,
            "Page.createIsolatedWorld",
            json!({"frameId": fid, "worldName": "lb-frame", "grantUniveralAccess": true}),
        )
        .await;
        let Ok(world) = world else {
            continue;
        };
        let Some(ctx) = world.get("executionContextId").and_then(|v| v.as_u64()) else {
            continue;
        };
        if let Ok(v) = rpc_expect(
            &mut ws,
            "Runtime.evaluate",
            json!({
                "contextId": ctx,
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true
            }),
        )
        .await
        {
            if let Some(val) = v.pointer("/result/value") {
                if !val.is_null() && val.as_str() != Some("undefined") {
                    let _ = ws.close(None).await;
                    return Ok(FrameEval {
                        frame_id: Some(fid.clone()),
                        value: val.clone(),
                    });
                }
            }
        }
    }
    // OOPIF fallback: cross-site iframes are separate browser targets and
    // never show up in Page.getFrameTree — reach them directly.
    if let Some((tid, val)) = eval_oopif_targets(port, expression)
        .await
        .into_iter()
        .next()
    {
        let _ = ws.close(None).await;
        return Ok(FrameEval {
            frame_id: Some(tid),
            value: val,
        });
    }
    let _ = ws.close(None).await;
    Err(Error::Parse("no frame returned a value".into()))
}

/// Value-only wrapper (frame id discarded) — for callers that don't need
/// frame-aware coordinates.
async fn eval_across_frames_value(port: u16, ws_url: &str, expression: &str) -> Result<Value> {
    eval_across_frames(port, ws_url, expression)
        .await
        .map(|fe| fe.value)
}

/// Discover cross-origin iframes (OOPIFs) and evaluate `expression` in each
/// one's own session. Out-of-process frames are invisible to the embedding
/// page's `Page.getFrameTree` (verified on Chrome 149: an embedded
/// login.live.com/Me.htm iframe never appears in the tree, even after
/// `load`), so they must be reached through the browser-level target list.
/// Returns (target_id, value) for every non-null result — target_id doubles
/// as the frameId for DOM.getFrameOwner, so clicks/offset still work.
async fn eval_oopif_targets(port: u16, expression: &str) -> Vec<(String, Value)> {
    // Browser-level ws — Target.getTargets is browser-scoped.
    let Ok(version) = reqwest::get(format!("http://127.0.0.1:{port}/json/version")).await else {
        return Vec::new();
    };
    let Ok(version) = version.json::<Value>().await else {
        return Vec::new();
    };
    let Some(browser_ws) = version.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Ok((mut ws, _)) = tokio_tungstenite::connect_async(browser_ws).await else {
        return Vec::new();
    };
    let Ok(targets) = rpc_expect(&mut ws, "Target.getTargets", json!({})).await else {
        let _ = ws.close(None).await;
        return Vec::new();
    };
    let _ = ws.close(None).await;

    let mut out = Vec::new();
    let infos = targets
        .pointer("/targetInfos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for t in infos {
        if t.get("type").and_then(|v| v.as_str()) != Some("iframe") {
            continue;
        }
        let Some(tid) = t.get("targetId").and_then(|v| v.as_str()) else {
            continue;
        };
        let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() || url == "about:blank" {
            continue;
        }
        let iframe_ws = format!("ws://127.0.0.1:{port}/devtools/page/{tid}");
        let Ok((mut iws, _)) = tokio_tungstenite::connect_async(&iframe_ws).await else {
            continue;
        };
        if let Ok(v) = rpc_expect(
            &mut iws,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true
            }),
        )
        .await
        {
            if let Some(val) = v.pointer("/result/value") {
                if !val.is_null() && val.as_str() != Some("undefined") {
                    out.push((tid.to_string(), val.clone()));
                }
            }
        }
        let _ = iws.close(None).await;
    }
    out
}

/// Top-viewport offset (x, y) of an iframe element inside its parent
/// document, via the CDP DOM domain. Used to convert frame-local element
/// coordinates (getBoundingClientRect) into page-viewport coordinates that
/// `Input.dispatchMouseEvent` understands.
async fn iframe_viewport_offset(ws: &mut CdpWs, frame_id: &str) -> Option<(f64, f64)> {
    let _ = rpc_expect(ws, "DOM.enable", json!({})).await;
    let owner = rpc_expect(ws, "DOM.getFrameOwner", json!({ "frameId": frame_id }))
        .await
        .ok()?;
    tracing::debug!("cdp: getFrameOwner -> {owner}");
    // Modern Chrome returns backendNodeId; older returns nodeId. getBoxModel
    // accepts either.
    let params = if let Some(bid) = owner.get("backendNodeId").and_then(|v| v.as_u64()) {
        json!({ "backendNodeId": bid })
    } else {
        let nid = owner.get("nodeId").and_then(|v| v.as_u64())?;
        json!({ "nodeId": nid })
    };
    let boxed = rpc_expect(ws, "DOM.getBoxModel", params).await.ok()?;
    tracing::debug!("cdp: getBoxModel -> {boxed}");
    let border = boxed.pointer("/model/border").and_then(|b| b.as_array())?;
    // Flat list: [x1, y1, x2, y2, x3, y3, x4, y4] (top-left first).
    let x = border.first()?.as_f64()?;
    let y = border.get(1)?.as_f64()?;
    Some((x, y))
}

/// Collect the rendered HTML of every child frame (iframe) of the page, so
/// snapshot extraction sees login forms and other frame-hosted content.
async fn collect_frame_htmls(port: u16, ws_url: &str) -> Vec<String> {
    let Ok((mut ws, _)) = tokio_tungstenite::connect_async(ws_url).await else {
        return Vec::new();
    };
    let Ok(tree) = rpc_frame_tree(&mut ws).await else {
        let _ = ws.close(None).await;
        return Vec::new();
    };
    let main_id = tree
        .pointer("/frameTree/frame/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut frames: Vec<String> = Vec::new();
    fn walk_frames(node: &Value, out: &mut Vec<String>, main: &str) {
        if let Some(id) = node.pointer("/frame/id").and_then(|v| v.as_str()) {
            if id != main {
                out.push(id.to_string());
            }
        }
        if let Some(children) = node.get("childFrames").and_then(|c| c.as_array()) {
            for c in children {
                walk_frames(c, out, main);
            }
        }
    }
    walk_frames(&tree, &mut frames, &main_id);

    if let Some(ft) = tree.get("frameTree") {
        walk_frames(ft, &mut frames, &main_id);
    }

    let mut htmls = Vec::new();
    for fid in frames {
        let Ok(world) = rpc_expect(
            &mut ws,
            "Page.createIsolatedWorld",
            json!({"frameId": fid, "worldName": "lb-frame", "grantUniveralAccess": true}),
        )
        .await
        else {
            continue;
        };
        let Some(ctx) = world.get("executionContextId").and_then(|v| v.as_u64()) else {
            continue;
        };
        if let Ok(v) = rpc_expect(
            &mut ws,
            "Runtime.evaluate",
            json!({
                "contextId": ctx,
                "expression": "document.documentElement.outerHTML",
                "returnByValue": true
            }),
        )
        .await
        {
            if let Some(h) = v.pointer("/result/value").and_then(|x| x.as_str()) {
                if !h.is_empty() {
                    htmls.push(h.to_string());
                }
            }
        }
    }
    // OOPIF fallback: cross-site iframes don't show up in the frame tree.
    for (_, val) in eval_oopif_targets(port, "document.documentElement.outerHTML").await {
        if let Some(h) = val.as_str() {
            if !h.is_empty() {
                htmls.push(h.to_string());
            }
        }
    }
    let _ = ws.close(None).await;
    htmls
}

/// Send a command and wait for its response (ignoring events in between).
/// The 20s deadline is the OVERALL cap; short 1s read timeouts (a busy page
/// can stall longer between CDP messages) just keep waiting — only a real
/// websocket close or protocol error aborts the call.
async fn rpc_expect(ws: &mut CdpWs, method: &str, params: Value) -> Result<Value> {
    let id = next_id();
    ws.send(WsMessage::Text(
        json!({ "id": id, "method": method, "params": params }).to_string(),
    ))
    .await
    .map_err(|e| Error::Transport(format!("cdp send {method}: {e}")))?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Transport(format!("cdp: {method} timed out")));
        }
        match tokio::time::timeout(Duration::from_secs(1), ws.next()).await {
            // 1s idle — page is busy; keep waiting until the deadline.
            Err(_) => continue,
            // Real websocket close → abort ("Trying to work with closed connection").
            Ok(None) => return Err(Error::Transport(format!("cdp: {method} connection closed"))),
            Ok(Some(Err(e))) => return Err(Error::Transport(format!("cdp recv {method}: {e}"))),
            Ok(Some(Ok(WsMessage::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(Error::Transport(format!("cdp {method} error: {err}")));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            Ok(Some(Ok(WsMessage::Ping(p)))) => {
                let _ = ws.send(WsMessage::Pong(p)).await;
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// Reap orphaned Chromium processes that hold OUR profile dir and clear
/// their stale profile locks, so a fresh spawn never hangs on a
/// SingletonLock left by a hard-killed previous server.
///
/// Unix-only via /proc. A Chrome whose parent is NOT a live lightbrowse
/// server (parent died → reparented to init or a subreaper, PPID may be
/// anything) is safe to SIGKILL; a live owner is left untouched, and locks
/// are only removed when no live owner remains.
fn clean_stale_profile_locks(user_data: &std::path::Path) {
    #[cfg(unix)]
    {
        let needle = user_data.display().to_string();
        let mut live_owner = false;
        let mut found = 0usize;
        let mut killed = 0usize;
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let pid = e.file_name().to_string_lossy().to_string();
                if !pid.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let args: Vec<&str> = std::str::from_utf8(&cmdline)
                    .unwrap_or("")
                    .split('\0')
                    .collect();
                if !args
                    .iter()
                    .any(|a| a.starts_with("--user-data-dir=") && a.contains(&needle))
                {
                    continue;
                }
                // Decide ownership: the browser MAIN process is directly
                // parented to the lightbrowse server; its children (renderers,
                // zygotes — same cmdline profile) are parented to chrome.
                let ppid: i32 = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .unwrap_or_default()
                    .split_whitespace()
                    .nth(3)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(-1);
                let parent_cmd = std::fs::read(format!("/proc/{ppid}/cmdline"))
                    .map(|c| String::from_utf8_lossy(&c).to_string())
                    .unwrap_or_default();
                let parent_is_lightbrowse = parent_cmd.contains("lightbrowse");
                let parent_is_chrome = parent_cmd.contains("chrome");
                if parent_is_lightbrowse {
                    live_owner = true;
                } else if !parent_is_chrome {
                    // Parent is init, a subreaper, or dead — an orphaned
                    // browser main. Kill it; children die with it.
                    killed += 1;
                    tracing::warn!(
                        "cdp: killing orphaned Chromium (pid {pid}) holding profile {needle}"
                    );
                    let _ = std::process::Command::new("kill")
                        .arg("-9")
                        .arg(&pid)
                        .status();
                } else {
                    found += 1;
                }
            }
        }
        tracing::debug!(
            "cdp: profile-lock scan for {needle}: {found} chrome procs, {killed} orphaned killed, live_owner={live_owner}"
        );
        if !live_owner {
            for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
                let p = user_data.join(name);
                if p.exists() {
                    tracing::info!("cdp: removing stale profile lock {}", p.display());
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

/// Heuristic: does this error mean the CDP websocket / browser died?
/// (e.g. "Trying to work with closed connection" from tokio-tungstenite)
fn is_connection_error(e: &Error) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("closed connection")
        || msg.contains("connection closed")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("unexpected eof")
        || msg.contains("broken pipe")
        || msg.contains("i/o error: connection")
}

/// A CDP error worth retrying at a higher level: transport failures and
/// internal RPC timeouts (a busy renderer can stall a single call past its
/// deadline). The caller decides how many retries are worth it.
fn is_recoverable(e: &Error) -> bool {
    is_connection_error(e) || e.to_string().to_ascii_lowercase().contains("timed out")
}

/// Epoch milliseconds — timestamps for download events / network capture.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `Page.getFrameTree` with internal retry + backoff: a single stalled RPC
/// (busy renderer, page mid-navigation) used to fail the whole call with a
/// "Page.getFrameTree timed out" noise error even though the connection is
/// healthy (see #26). Retries twice with short backoff, then gives up.
async fn rpc_frame_tree(ws: &mut CdpWs) -> Result<Value> {
    let mut last = None;
    for attempt in 0..3 {
        match rpc_expect(ws, "Page.getFrameTree", json!({})).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                if !is_recoverable(last.as_ref().unwrap()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300 * (attempt + 1))).await;
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Transport("cdp: Page.getFrameTree failed".into())))
}

/// Background loop for network capture (#29/#31): holds a DevTools
/// connection to the active tab open, listens for Network events and stores
/// compact request/response/failure records into a bounded ring buffer.
/// Exits when the tab's websocket closes (tab recreated / browser reset) or
/// the caller cancels the token; always clears `capturing` on exit.
async fn network_capture_loop(
    ws_url: &str,
    log: &Arc<Mutex<VecDeque<Value>>>,
    token: &CancellationToken,
) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| Error::Transport(format!("cdp connect: {e}")))?;
    rpc_expect(&mut ws, "Network.enable", json!({})).await?;
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = ws.close(None).await;
                return Ok(());
            }
            msg = ws.next() => {
                match msg {
                    None => return Ok(()), // ws closed → tab gone; capture ends
                    Some(Err(e)) => return Err(Error::Transport(format!("cdp recv: {e}"))),
                    Some(Ok(WsMessage::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                            let entry = match m {
                                "Network.requestWillBeSent" => Some(json!({
                                    "ts": now_ms(),
                                    "kind": "request",
                                    "request_id": v.pointer("/params/requestId"),
                                    "url": v.pointer("/params/request/url").and_then(|u| u.as_str()).unwrap_or(""),
                                    "method": v.pointer("/params/request/method").and_then(|u| u.as_str()).unwrap_or(""),
                                    "type": v.pointer("/params/type"),
                                })),
                                "Network.responseReceived" => Some(json!({
                                    "ts": now_ms(),
                                    "kind": "response",
                                    "request_id": v.pointer("/params/requestId"),
                                    "url": v.pointer("/params/response/url").and_then(|u| u.as_str()).unwrap_or(""),
                                    "status": v.pointer("/params/response/status"),
                                    "mime": v.pointer("/params/response/mimeType").and_then(|u| u.as_str()).unwrap_or(""),
                                })),
                                "Network.loadingFailed" => Some(json!({
                                    "ts": now_ms(),
                                    "kind": "failed",
                                    "request_id": v.pointer("/params/requestId"),
                                    "error": v.pointer("/params/errorText"),
                                })),
                                _ => None,
                            };
                            if let Some(entry) = entry {
                                let mut guard = log.lock().unwrap_or_else(|e| e.into_inner());
                                if guard.len() >= 500 {
                                    guard.pop_front();
                                }
                                guard.push_back(entry);
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Errors a read-only op should self-heal from: a dead/restarted renderer
/// surfaces either as a ws reset (`is_connection_error`) or — when the
/// browser keeps the socket open but the renderer never answers (SIGKILL /
/// OOM-killed renderer) — as a 20s `timed out`. In both cases the tab is
/// gone and retrying on a re-created tab is the only useful outcome.
fn is_recoverable_error(e: &Error) -> bool {
    is_connection_error(e) || e.to_string().to_ascii_lowercase().contains("timed out")
}

/// Background listener on the browser-level CDP socket that logs renderer
/// crashes (`Target.targetCrashed`). A crashed renderer closes its tab's
/// DevTools socket with "Connection reset without closing handshake" — this
/// event is what tells us it was a crash (e.g. a heavy SPA like Teams
/// exceeding the V8 heap cap), not a network problem. Reconnects on socket
/// death (browser restart) and exits on backend shutdown.
fn spawn_crash_listener(browser_ws: String, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&browser_ws).await else {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            // Snapshot targetId → URL for crash diagnostics.
            let mut urls: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            if let Ok(t) = rpc_expect(&mut ws, "Target.getTargets", json!({})).await {
                if let Some(infos) = t.pointer("/targetInfos").and_then(|v| v.as_array()) {
                    for i in infos {
                        if let (Some(tid), Some(u)) = (
                            i.get("targetId").and_then(|v| v.as_str()),
                            i.get("url").and_then(|v| v.as_str()),
                        ) {
                            urls.insert(tid.to_string(), u.to_string());
                        }
                    }
                }
            }
            loop {
                if shutdown.is_cancelled() {
                    let _ = ws.close(None).await;
                    return;
                }
                match tokio::time::timeout(Duration::from_secs(1), ws.next()).await {
                    // Idle is fine for a listener — keep waiting.
                    Err(_) => continue,
                    Ok(None) => break,
                    Ok(Some(Err(e))) => {
                        tracing::debug!("cdp: crash listener ws error: {e}");
                        break;
                    }
                    Ok(Some(Ok(WsMessage::Ping(p)))) => {
                        let _ = ws.send(WsMessage::Pong(p)).await;
                    }
                    Ok(Some(Ok(WsMessage::Text(t)))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if v.get("method").and_then(|m| m.as_str())
                                == Some("Target.targetCrashed")
                            {
                                let tid = v
                                    .pointer("/params/targetId")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("?");
                                let status = v
                                    .pointer("/params/status")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("?");
                                let code = v
                                    .pointer("/params/errorCode")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(-1);
                                let url = urls.get(tid).cloned().unwrap_or_else(|| "?".into());
                                tracing::warn!(
                                    "cdp: TARGET CRASHED target={tid} url={url} status={status} errorCode={code} — this tab's DevTools socket will reset (Connection reset without closing handshake)"
                                );
                            }
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                }
            }
            let _ = ws.close(None).await;
        }
    });
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
                    match cdp.click(sel, None).await {
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
                    match cdp.type_text(sel, &text, None).await {
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
                match cdp.press_key(&key, None).await {
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
                        .evaluate(
                            &format!(
                                "(() => !!document.querySelector({}))",
                                serde_json::to_string(sel).unwrap_or_default()
                            ),
                            None,
                        )
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
    let (html, title, _) = cdp.current_dom(None).await.unwrap_or_default();
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

/// Integration test: CDP must self-heal when the Chromium process is killed
/// mid-session (macOS report: "Trying to work with closed connection").
/// Requires a Chrome/Chromium binary + network; runs in CI via --include-ignored.
/// Uses unix-only `pkill`/`kill` — ignored on Windows.
#[tokio::test]
#[ignore = "requires Chrome + network"]
#[cfg_attr(windows, ignore = "uses pkill/kill (unix-only)")]
async fn recovers_after_chromium_killed() {
    let config = lightbrowse_core::config::Config {
        idle_timeout_secs: 300,
        ..Default::default()
    };
    let backend = std::sync::Arc::new(CdpBackend::new(config));
    let session = Session::new();

    // 1) First navigation works.
    let p1 = backend
        .navigate(&session, "https://example.com/")
        .await
        .expect("first navigate");
    assert!(!p1.html.is_empty());

    // 2) Kill the Chromium process tree (simulate silent death).
    let pid = backend
        .inner
        .lock()
        .await
        .as_ref()
        .expect("browser running")
        .child
        .as_ref()
        .expect("spawned browser")
        .id();
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg("--user-data-dir=.*lightbrowse-chrome")
        .status();
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 3) Next navigation must recover: kill old state, spawn fresh Chromium.
    let p2 = backend
        .navigate(&session, "https://example.com/")
        .await
        .expect("navigate after kill");
    assert!(!p2.html.is_empty(), "recovered page must have content");
    assert!(
        backend.is_running().await,
        "fresh Chromium should be running"
    );
    assert!(backend.navigate_count() >= 2, "both navigations counted");

    backend.reset_browser().await;
}
