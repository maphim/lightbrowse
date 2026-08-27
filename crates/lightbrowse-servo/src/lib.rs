//! `lightbrowse-servo` — PoC backend using the **Servo** engine (Rust-native).
//!
//! Goal: replace Chromium/CDP for most JS-heavy pages, cutting RAM from
//! ~450 MB to ~50-150 MB. This is a *proof of concept* — minimal, no caching,
//! builds a fresh Servo per navigation (Servo is !Send, so each fetch lives
//! on one thread).
//!
//! Pattern learned from [servo-fetch](https://github.com/konippi/servo-fetch):
//! - `ServoBuilder` + `SoftwareRenderingContext` (no GPU)
//! - `WebViewBuilder` -> `WebView`, drive with `servo.spin_event_loop()`
//! - wait for `document.readyState == "complete"`, then eval JS to harvest DOM

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::error::{Error, Result};
use lightbrowse_core::page::Page;
use lightbrowse_core::session::Session;
use servo::preferences::Preferences;
use servo::{
    Opts, Servo, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};

const VIEWPORT_W: i32 = 1280;
const VIEWPORT_H: i32 = 900;

/// Minimal delegate: we drive everything via JS eval + readyState polling.
struct DummyDelegate;
impl WebViewDelegate for DummyDelegate {}

/// Stateless backend — every navigation builds and drops its own Servo.
#[derive(Default)]
pub struct ServoBackend {
    timeout_secs: u64,
}

impl ServoBackend {
    pub fn new() -> Self {
        Self { timeout_secs: 30 }
    }

    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }

    fn fetch_once(&self, url: &str) -> Result<(String, String, String)> {
        let parsed = url::Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;

        // Software rendering context (no GPU).
        let size = servo::PhysicalSize::new(VIEWPORT_W, VIEWPORT_H);
        let ctx = Rc::new(
            SoftwareRenderingContext::new(size)
                .map_err(|e| Error::Transport(format!("servo context: {e:?}")))?,
        );
        ctx.make_current()
            .map_err(|e| Error::Transport(format!("servo context current: {e:?}")))?;

        let prefs = Preferences {
            user_agent: lightbrowse_core::session::DEFAULT_UA.to_owned(),
            ..Preferences::default()
        };
        let opts = Opts {
            ..Opts::default()
        };

        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<()>();
        let waker = Box::new(move || {
            let _ = wake_tx.try_send(());
        });

        let servo = ServoBuilder::default()
            .opts(opts)
            .preferences(prefs)
            .event_loop_waker(waker)
            .build();

        let delegate: Rc<dyn WebViewDelegate> = Rc::new(DummyDelegate);
        let rc_dyn: Rc<dyn servo::RenderingContext> = ctx;
        let webview = WebViewBuilder::new(&servo, rc_dyn)
            .delegate(delegate)
            .url(parsed)
            .build();

        // Drive the event loop until the document finishes loading (or timeout).
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            servo.spin_event_loop();
            if matches!(eval_js(&servo, &webview, "document.readyState"), Ok(s) if s.trim() == "complete") {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("servo: document did not reach readyState=complete within {}s", self.timeout_secs);
                break;
            }
            // Sleep a tick; the waker unblocks early on activity.
            let _ = wake_rx.recv_timeout(Duration::from_millis(50));
        }

        // Give lazy JS a moment (PoC keeps it simple).
        std::thread::sleep(Duration::from_millis(500));

        let html = eval_js(&servo, &webview, "document.documentElement.outerHTML")?;
        let title = eval_js(&servo, &webview, "document.title").unwrap_or_default();
        let final_url = eval_js(&servo, &webview, "location.href").unwrap_or_else(|_| url.to_string());

        Ok((html, title.trim().to_string(), final_url.trim().to_string()))
    }
}

fn eval_js(servo: &Servo, webview: &WebView, script: &str) -> Result<String> {
    let result: Rc<RefCell<Option<Result<String>>>> = Rc::new(RefCell::new(None));
    let cb_result = result.clone();

    webview.evaluate_javascript(script, move |js_result| {
        let val = match js_result {
            Ok(servo::JSValue::String(s)) => Ok(s),
            Ok(servo::JSValue::Undefined | servo::JSValue::Null) => Ok(String::new()),
            Ok(servo::JSValue::Boolean(b)) => Ok(b.to_string()),
            Ok(servo::JSValue::Number(n)) => Ok(n.to_string()),
            Ok(other) => Err(Error::Transport(format!("servo JS value: {other:?}"))),
            Err(e) => Err(Error::Transport(format!("servo JS error: {e:?}"))),
        };
        *cb_result.borrow_mut() = Some(val);
    });

    // Spin until the async callback fires (bounded by a sane deadline).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        servo.spin_event_loop();
        if let Some(v) = result.borrow_mut().take() {
            return v;
        }
        if Instant::now() >= deadline {
            return Err(Error::Transport("servo: JS eval timed out".into()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[async_trait::async_trait]
impl BrowserBackend for ServoBackend {
    fn name(&self) -> &'static str {
        "servo"
    }

    async fn navigate(&self, _session: &Session, url: &str) -> Result<Page> {
        // Servo is single-threaded (!Send) — run the whole fetch on a
        // blocking thread, then hand the serialized HTML back to core.
        let timeout = self.timeout_secs;
        let url = url.to_string();
        let (html, title, final_url) = tokio::task::spawn_blocking(move || {
            let backend = ServoBackend::with_timeout(timeout);
            backend.fetch_once(&url)
        })
        .await
        .map_err(|e| Error::Transport(format!("servo thread: {e}")))??;

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
