//! `lightbrowse-fetch` — the default, featherweight backend.
//!
//! Fetches pages with `reqwest` (HTTP/2, gzip/brotli, rustls TLS), applies
//! the session cookie jar and Chrome-like headers, follows redirects
//! manually (so `Set-Cookie` from every hop is captured), then hands the raw
//! HTML to the core extractors. No browser engine, no GUI.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::cookie::CookieJar;
use lightbrowse_core::error::{Error, Result};
use lightbrowse_core::page::Page;
use lightbrowse_core::proxy::parse_proxy;
use lightbrowse_core::session::{Session, SessionOptions};
use reqwest::cookie::CookieStore;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, LOCATION, SET_COOKIE,
    USER_AGENT,
};
use reqwest::redirect::Policy;

/// Fetch-only backend. Stateless beyond the reqwest connection pool.
pub struct FetchBackend {
    client: RwLock<reqwest::Client>,
    /// Proxy URL currently in effect (`None` = direct). Kept so the value
    /// survives client rebuilds and can be reported by `/v1/proxy`.
    proxy: RwLock<Option<String>>,
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
        Self::with_proxy(None)
    }

    /// Build a backend that routes every request through `proxy`
    /// (http/https/socks5/socks5h URL, or `None` for direct connections).
    pub fn with_proxy(proxy: Option<&str>) -> Result<Self> {
        let client = build_client(proxy)?;
        Ok(Self {
            client: RwLock::new(client),
            proxy: RwLock::new(proxy.map(|p| p.to_string())),
        })
    }

    /// Change the proxy at runtime (rebuilds the connection pool).
    /// Pass `None` to go back to direct connections. Returns the previous
    /// value, or an error when the URL is malformed.
    pub fn set_proxy(&self, proxy: Option<&str>) -> Result<Option<String>> {
        let client = build_client(proxy)?;
        let mut prev = self
            .proxy
            .write()
            .map_err(|_| Error::Transport("proxy lock poisoned".into()))?;
        let old = prev.clone();
        *prev = proxy.map(|p| p.to_string());
        drop(prev);
        *self
            .client
            .write()
            .map_err(|_| Error::Transport("client lock poisoned".into()))? = client;
        Ok(old)
    }

    /// Currently active proxy URL (or `None` when direct).
    pub fn proxy(&self) -> Option<String> {
        self.proxy.read().ok().and_then(|g| g.clone())
    }

    /// Create a session wired to a real cookie jar.
    pub fn new_session(options: SessionOptions) -> Session {
        let jar = Arc::new(ReqwestCookieJar::default());
        Session::with_cookie_jar(options, jar)
    }
}

/// Build a reqwest client, optionally routing through a proxy.
fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        // Redirects are handled manually so every Set-Cookie hop is seen.
        .redirect(Policy::none())
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .http1_only()
        .pool_idle_timeout(std::time::Duration::from_secs(90));
    if let Some(p) = proxy {
        // Parse early so a malformed URL fails fast (before any request).
        let spec = parse_proxy(p)?;
        let prox = reqwest::Proxy::all(spec.reqwest_url()).map_err(|e| {
            Error::InvalidUrl(format!("bad proxy URL '{}': {e}", spec.reqwest_url()))
        })?;
        builder = builder.proxy(prox);
        tracing::info!("fetch: routing all traffic via {}", spec.describe());
    }
    builder.build().map_err(|e| Error::Transport(e.to_string()))
}

impl Default for FetchBackend {
    fn default() -> Self {
        Self::new().expect("fetch backend construction is infallible")
    }
}

#[async_trait]
impl lightbrowse_core::backend::ProxyControl for FetchBackend {
    async fn set_proxy(&self, proxy: Option<String>) -> Result<()> {
        self.set_proxy(proxy.as_deref()).map(|_| ())
    }

    fn proxy(&self) -> Option<String> {
        self.proxy()
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
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
    headers
}

#[async_trait]
impl BrowserBackend for FetchBackend {
    fn name(&self) -> &'static str {
        "fetch"
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
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

            let resp = {
                let client = self
                    .client
                    .read()
                    .map_err(|_| Error::Transport("client lock poisoned".into()))?
                    .clone();
                client
                    .get(current.clone())
                    .headers(headers)
                    .timeout(timeout)
                    .send()
                    .await
                    .map_err(|e| Error::Transport(e.to_string()))?
            };

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

    #[test]
    fn rejects_bad_proxy_url() {
        assert!(FetchBackend::with_proxy(Some("nonsense")).is_err());
        assert!(FetchBackend::with_proxy(Some("ftp://h:21")).is_err());
        assert!(FetchBackend::with_proxy(Some("http://:8080")).is_err());
        // Valid socks5h passes validation (connection is attempted lazily).
        assert!(FetchBackend::with_proxy(Some("socks5h://127.0.0.1:1080")).is_ok());
    }

    #[test]
    fn set_proxy_updates_and_reports() {
        let b = FetchBackend::new().unwrap();
        assert_eq!(b.proxy(), None);
        let prev = b.set_proxy(Some("http://127.0.0.1:8888")).unwrap();
        assert_eq!(prev, None);
        assert_eq!(b.proxy(), Some("http://127.0.0.1:8888".into()));
        let prev = b.set_proxy(None).unwrap();
        assert_eq!(prev, Some("http://127.0.0.1:8888".into()));
        assert_eq!(b.proxy(), None);
    }

    /// Tiny in-process HTTP proxy: accepts a connection, reads the request
    /// headers, forwards them byte-for-byte to the target host, then relays
    /// the response back. Proves that `with_proxy` really routes traffic.
    async fn spawn_mini_proxy(_target_port: u16) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let mut n_total = 0usize;
                    // Read until the end of the request headers.
                    loop {
                        let Ok(n) = conn.read(&mut buf[n_total..]).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        n_total += n;
                        if buf[..n_total].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    // The request is absolute-form (GET http://host/path).
                    let Ok(req_str) = std::str::from_utf8(&buf[..n_total]) else {
                        return;
                    };
                    let Some(target) = req_str.split(' ').nth(1) else {
                        return;
                    };
                    let Ok(target_url) = url::Url::parse(target) else {
                        return;
                    };
                    let host = target_url
                        .host_str()
                        .unwrap_or("127.0.0.1")
                        .trim_matches(['[', ']'])
                        .to_string();
                    let port = target_url.port().unwrap_or(80);
                    let mut upstream =
                        match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                    let _ = upstream.write_all(&buf[..n_total]).await;
                    // Relay the response back.
                    let mut resp = [0u8; 8192];
                    let mut relayed = 0usize;
                    loop {
                        match upstream.read(&mut resp[relayed..]).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                relayed += n;
                                if conn.write_all(&resp[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = conn.flush().await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn navigate_through_http_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // In-process target server that answers with a fixed body.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = target.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf).await;
                    let body = "hello via proxy";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = conn.write_all(resp.as_bytes()).await;
                    let _ = conn.flush().await;
                });
            }
        });

        let proxy_port = spawn_mini_proxy(target_port).await;
        let backend =
            FetchBackend::with_proxy(Some(&format!("http://127.0.0.1:{proxy_port}"))).unwrap();
        let session = FetchBackend::new_session(SessionOptions::default());
        let page = backend
            .navigate(&session, &format!("http://127.0.0.1:{target_port}/"))
            .await
            .expect("fetch via proxy should succeed");
        assert_eq!(page.status, 200);
        assert!(page.html.contains("hello via proxy"), "body: {}", page.html);
    }

    #[tokio::test]
    async fn set_proxy_routes_subsequent_requests() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A live target that answers directly…
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = target.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf).await;
                    let body = "ok direct";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = conn.write_all(resp.as_bytes()).await;
                });
            }
        });

        let backend = FetchBackend::new().unwrap();
        let session = FetchBackend::new_session(SessionOptions::default());
        let url = format!("http://127.0.0.1:{target_port}/");

        // Direct: succeeds.
        let page = backend
            .navigate(&session, &url)
            .await
            .expect("direct fetch");
        assert_eq!(page.status, 200);

        // Point the backend at a dead proxy: the SAME request must now fail,
        // proving traffic is routed through the proxy.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead); // nothing listens there anymore
        backend
            .set_proxy(Some(&format!("http://127.0.0.1:{dead_port}")))
            .unwrap();
        assert!(backend.navigate(&session, &url).await.is_err());

        // Back to direct: succeeds again.
        backend.set_proxy(None).unwrap();
        let page = backend
            .navigate(&session, &url)
            .await
            .expect("direct again");
        assert_eq!(page.status, 200);
    }
}
