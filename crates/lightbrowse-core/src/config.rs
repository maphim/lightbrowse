//! Runtime configuration — headless-first, RAM-budgeted.

use serde::{Deserialize, Serialize};

/// Browsing engine to use for a navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// Try the featherweight fetch backend first; fall back to the CDP
    /// (Chromium) backend when a page looks JavaScript-rendered.
    #[default]
    Auto,
    /// Pure-Rust fetch only. ~5 MB RAM, no JS.
    Fetch,
    /// Drive a real headless Chromium via the DevTools Protocol.
    /// ~200 MB RAM per tab; spawned lazily, suspended when idle.
    Cdp,
}

impl Engine {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "fetch" => Some(Self::Fetch),
            "cdp" | "chrome" | "chromium" => Some(Self::Cdp),
            _ => None,
        }
    }
}

/// Global lightbrowse configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Render a GUI window. **Default: false** — lightbrowse is headless-first;
    /// the UI is an opt-in feature for humans who want to watch.
    pub ui: bool,
    /// Engine selection policy.
    pub engine: Engine,
    /// Hard memory budget in MB. Used to size the JS heap, cap concurrent
    /// tabs, and decide when to suspend engines.
    pub memory_budget_mb: usize,
    /// Seconds a tab can sit unused before its engine is suspended
    /// (network dropped, Chromium process killed).
    pub idle_timeout_secs: u64,
    /// Maximum concurrently active tabs.
    pub max_tabs: usize,
    /// Path to the Chrome/Chromium binary (auto-detected when `None`).
    pub chrome_path: Option<String>,
    /// Extra wait after the load event, for lazy-JS pages (milliseconds).
    pub js_wait_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: false,
            engine: Engine::Auto,
            memory_budget_mb: 1024,
            idle_timeout_secs: 60,
            max_tabs: 4,
            chrome_path: None,
            js_wait_ms: 800,
        }
    }
}

impl Config {
    /// Rough estimate: how many Chromium tabs fit in the budget?
    pub fn max_tabs_for_budget(&self) -> usize {
        // Headless Chromium ≈ 190-250 MB per tab in practice.
        (self.memory_budget_mb / 250).clamp(1, 8)
    }
}
