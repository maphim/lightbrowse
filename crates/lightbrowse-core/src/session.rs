use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cookie::{CookieJar, NoopCookieJar};

/// Options that tune a browsing session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    /// Maximum HTML body size in bytes (larger bodies are truncated).
    pub max_html_bytes: usize,
    /// Maximum number of redirects to follow.
    pub max_redirects: usize,
    /// Accept-Language header value.
    pub accept_language: String,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            max_html_bytes: 2 * 1024 * 1024,
            max_redirects: 10,
            accept_language: "en-US,en;q=0.9,vi;q=0.8".into(),
        }
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A browsing session: shared cookies + history + tunables.
///
/// A session is what makes lightbrowse feel like a *browser* instead of a
/// bare HTTP client — cookies persist across navigations so logins and
/// session state carry over between AI tool calls.
#[derive(Clone)]
pub struct Session {
    pub id: String,
    /// Shared cookie jar (thread-safe, persists across requests).
    pub cookie_jar: Arc<dyn CookieJar>,
    pub user_agent: String,
    pub history: Vec<String>,
    pub options: SessionOptions,
}

impl Session {
    pub fn new() -> Self {
        Self::with_options(SessionOptions::default())
    }

    pub fn with_options(options: SessionOptions) -> Self {
        Self::with_cookie_jar(options, Arc::new(NoopCookieJar))
    }

    pub fn with_cookie_jar(options: SessionOptions, cookie_jar: Arc<dyn CookieJar>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = format!(
            "sess-{:x}-{}",
            now,
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            id,
            cookie_jar,
            user_agent: DEFAULT_UA.into(),
            history: Vec::new(),
            options,
        }
    }

    pub fn push_history(&mut self, url: String) {
        self.history.push(url);
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// A modern Chrome UA so that most sites treat us like a real browser.
pub const DEFAULT_UA: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/131.0.0.0 Safari/537.36 lightbrowse/",
    env!("CARGO_PKG_VERSION"),
);
