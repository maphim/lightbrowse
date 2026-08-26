//! High-level navigation service: engine selection + auto-fallback.

use crate::backend::BrowserBackend;
use crate::config::Engine;
use crate::error::Result;
use crate::page::Page;
use crate::session::Session;

/// A page that came out too empty to trust — a heuristic for "this site
/// renders its content with JavaScript".
pub fn looks_js_rendered(page: &Page) -> bool {
    let has_script = page.html.to_ascii_lowercase().contains("<script");
    let text_len = crate::extract::extract_text(&page.html).word_count;
    has_script && text_len < 40
}

/// Navigate using the requested engine.
///
/// `cdp` may be `None`; `Engine::Auto` and `Engine::Cdp` then degrade to
/// fetch with a warning-free, explicit fallback.
pub async fn navigate(
    fetch: &dyn BrowserBackend,
    cdp: Option<&dyn BrowserBackend>,
    session: &Session,
    url: &str,
    engine: Engine,
) -> Result<Page> {
    match engine {
        Engine::Fetch => fetch.navigate(session, url).await,
        Engine::Cdp => match cdp {
            Some(c) => c.navigate(session, url).await,
            None => fetch.navigate(session, url).await,
        },
        Engine::Auto => {
            let page = fetch.navigate(session, url).await?;
            if looks_js_rendered(&page) {
                if let Some(c) = cdp {
                    return c.navigate(session, url).await;
                }
            }
            Ok(page)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;

    fn page(html: &str) -> Page {
        Page {
            url: "https://x.test/".into(),
            title: String::new(),
            status: 200,
            headers: Default::default(),
            html: html.into(),
            truncated: false,
            mime: Some("text/html".into()),
        }
    }

    #[test]
    fn js_heuristic() {
        // Empty shell + script = JS-rendered.
        assert!(looks_js_rendered(&page(
            r#"<html><body><div id="root"></div><script src="app.js"></script></body></html>"#
        )));
        // Tiny SSR content + script still trips the heuristic (false positive
        // is fine: CDP re-render is correct, just slower).
        assert!(looks_js_rendered(&page(
            "<html><body><h1>Real content here</h1><script>analytics()</script></body></html>"
        )));
        // Substantial server-rendered content + script = not JS-dependent.
        let long_text = "word ".repeat(60);
        assert!(!looks_js_rendered(&page(&format!(
            "<html><body><p>{long_text}</p><script>analytics()</script></body></html>"
        ))));
        // Static page = no.
        assert!(!looks_js_rendered(&page(
            "<html><body><p>Hello world</p></body></html>"
        )));
    }
}
