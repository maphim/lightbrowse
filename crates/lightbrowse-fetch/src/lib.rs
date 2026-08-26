//! `lightbrowse-fetch` — the default, featherweight backend.
//!
//! Fetches pages with `reqwest` (HTTP/2, gzip/brotli, rustls TLS), applies
//! the session cookie jar and Chrome-like headers, follows redirects
//! manually (so `Set-Cookie` from every hop is captured), then hands the raw
//! HTML to the core extractors. No browser engine, no GUI.

use std::sync::Arc;

use async_trait::async_trait;
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::cookie::CookieJar;
use lightbrowse_core::error::{Error, Result};
use lightbrowse_core::page::Page;
use lightbrowse_core::session::{Session, SessionOptions};
use reqwest::cookie::CookieStore;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, LOCATION, SET_COOKIE,
    USER_AGENT,
};
use reqwest::redirect::Policy;

/// Fetch-only backend. Stateless beyond the reqwest connection pool.
pub struct FetchBackend {
    client: reqwest::Client,
}

/// `reqwest::cookie::Jar` adapted to the core [`CookieJar`] trait.
#[derive(Clone)]
pub struct ReqwestCookieJar(pub Arc<reqwest::cookie::Jar>);

impl Default for ReqwestCookieJar {
    fn default() -> Self {
        Self(Arc::new(reqwest::cookie::Jar::default()))
    }
}

impl CookieJar for ReqwestCookieJar {
    fn cookie_header(&self, url: &str) -> Option<String> {
        let u = url::Url::parse(url).ok()?;
        self.0
            .cookies(&u)
            .and_then(|v| v.to_str().ok().map(|s| s.to_string()))
    }

    fn store_set_cookie(&self, url: &str, set_cookie: &str) {
        if let Ok(u) = url::Url::parse(url) {
            self.0.add_cookie_str(set_cookie, &u);
        }
    }
}

impl FetchBackend {
    /// Build a new backend with sensible defaults (gzip/brotli, HTTP/2, rustls).
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            // Redirects are handled manually so every Set-Cookie hop is seen.
            .redirect(Policy::none())
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    /// Create a session wired to a real cookie jar.
    pub fn new_session(options: SessionOptions) -> Session {
        let jar = Arc::new(ReqwestCookieJar::default());
        Session::with_cookie_jar(options, jar)
    }
}

impl Default for FetchBackend {
    fn default() -> Self {
        Self::new().expect("fetch backend construction is infallible")
    }
}

fn base_headers(session: &Session) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&session.user_agent).unwrap(),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_str(&session.options.accept_language).unwrap(),
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers
}

#[async_trait]
impl BrowserBackend for FetchBackend {
    fn name(&self) -> &'static str {
        "fetch"
    }

    async fn navigate(&self, session: &Session, url: &str) -> Result<Page> {
        let mut current = url::Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if !matches!(current.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl(format!(
                "unsupported scheme '{}' (only http/https)",
                current.scheme()
            )));
        }

        let timeout = std::time::Duration::from_secs(session.options.timeout_secs);
        let mut hops = 0;

        loop {
            // Attach any session cookies for this exact URL.
            let mut headers = base_headers(session);
            if let Some(cookie) = session.cookie_jar.cookie_header(current.as_str()) {
                if let Ok(v) = HeaderValue::from_str(&cookie) {
                    headers.insert(reqwest::header::COOKIE, v);
                }
            }

            let resp = self
                .client
                .get(current.clone())
                .headers(headers)
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;

            // Capture Set-Cookie from this hop.
            for sc in resp.headers().get_all(SET_COOKIE) {
                if let Ok(v) = sc.to_str() {
                    session.cookie_jar.store_set_cookie(current.as_str(), v);
                }
            }

            let status = resp.status();
            if status.is_redirection() && hops < session.options.max_redirects {
                let loc = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Error::Transport("redirect without Location".into()))?;
                let next = current
                    .join(loc)
                    .map_err(|e| Error::Transport(format!("bad redirect target: {e}")))?;
                if !matches!(next.scheme(), "http" | "https") {
                    return Err(Error::InvalidUrl(format!(
                        "redirect to unsupported scheme '{}'",
                        next.scheme()
                    )));
                }
                current = next;
                hops += 1;
                continue;
            }

            let status = resp.status().as_u16();
            let final_url = resp.url().to_string();
            let mime = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let headers: std::collections::HashMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|s| (k.as_str().to_string(), s.to_string()))
                })
                .collect();

            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Error::Transport(format!("reading body: {e}")))?;

            let max = session.options.max_html_bytes;
            let truncated = bytes.len() > max;
            let slice = if truncated { &bytes[..max] } else { &bytes[..] };
            let html = String::from_utf8_lossy(slice).into_owned();

            // Cheap title extraction for the Page header.
            let title = extract_title(&html);

            return Ok(Page {
                url: final_url,
                title,
                status,
                headers,
                html,
                truncated,
                mime,
            });
        }
    }
}

fn extract_title(html: &str) -> String {
    // Avoid a full parse for just the title; regex-free scan is enough.
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(gt) = lower[start..].find('>') else {
        return String::new();
    };
    let content_start = start + gt + 1;
    let Some(close) = lower[content_start..].find("</title>") else {
        return String::new();
    };
    html[content_start..content_start + close]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_scan() {
        assert_eq!(
            extract_title("<html><head><title>  Hello &amp; World </title></head></html>"),
            "Hello &amp; World"
        );
        assert_eq!(extract_title("<p>no title</p>"), "");
    }

    #[tokio::test]
    async fn rejects_bad_scheme() {
        let b = FetchBackend::new().unwrap();
        let s = FetchBackend::new_session(SessionOptions::default());
        let err = b.navigate(&s, "file:///etc/passwd").await.unwrap_err();
        assert!(err.to_string().contains("unsupported scheme"));
    }

    #[test]
    fn cookie_jar_roundtrip() {
        let jar = ReqwestCookieJar::default();
        jar.store_set_cookie("https://example.com/", "sid=abc123; Path=/");
        assert_eq!(
            jar.cookie_header("https://example.com/page"),
            Some("sid=abc123".into())
        );
        // Domain-scoped: different host gets nothing.
        assert_eq!(jar.cookie_header("https://other.com/"), None);
    }
}
