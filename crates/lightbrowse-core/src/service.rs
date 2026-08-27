//! High-level navigation service: engine selection + auto-fallback.

use crate::backend::BrowserBackend;
use crate::config::Engine;
use crate::error::Result;
use crate::page::Page;
use crate::session::Session;

/// A page that came out too empty to trust — a heuristic for "this site
/// renders its content with JavaScript" or threw a bot-challenge.
pub fn looks_js_rendered(page: &Page) -> bool {
    let has_script = page.html.to_ascii_lowercase().contains("<script");
    let text_len = crate::extract::extract_text(&page.html).word_count;
    let challenge = page.title.contains("Just a moment")
        || page.title.contains("Attention Required")
        || page.title.contains("Access Denied")
        || page.status >= 400;
    has_script && (text_len < 40 || challenge)
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
            match fetch.navigate(session, url).await {
                // Fetch worked and the page looks server-rendered — done.
                Ok(page) if !looks_js_rendered(&page) => Ok(page),
                // Fetch worked but the page is JS-heavy/challenged — the
                // real browser can render it.
                Ok(page) => match cdp {
                    Some(c) => c.navigate(session, url).await,
                    None => Ok(page),
                },
                // Transient fetch failure (DNS/TCP/TLS — e.g. a network blip
                // or a site that rejects the fetch UA). Let the real browser
                // try before giving up.
                Err(e) => match cdp {
                    Some(c) => match c.navigate(session, url).await {
                        Ok(p) => Ok(p),
                        Err(ce) => Err(ce),
                    },
                    None => Err(e),
                },
            }
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
