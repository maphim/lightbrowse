use async_trait::async_trait;

use crate::error::Result;
use crate::page::Page;
use crate::session::Session;

/// A pluggable browsing engine.
///
/// The default implementation is the pure-Rust fetch backend
/// (`lightbrowse-fetch`, zero GUI, ~1-2 MB of deps). Future backends:
/// - **cdp**: drive a real Chrome/Chromium via the DevTools Protocol for
///   JavaScript-heavy sites (planned)
/// - **webview**: embed a native webview for rendering + screenshots (planned)
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Backend identifier, e.g. `"fetch"`.
    fn name(&self) -> &'static str;

    /// Navigate to `url` and return the fetched page.
    ///
    /// Implementations must apply `session` cookies, user-agent and limits.
    async fn navigate(&self, session: &Session, url: &str) -> Result<Page>;

    /// Downcast hook for backend-specific introspection (e.g. RAM stats).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}
