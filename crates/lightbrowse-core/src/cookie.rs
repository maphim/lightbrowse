//! Cookie jar abstraction.
//!
//! `lightbrowse-core` deliberately avoids depending on `reqwest`, so cookie
//! storage is defined as a small trait here and implemented by backends
//! (e.g. `lightbrowse-fetch` wraps `reqwest::cookie::Jar`).

/// A thread-safe cookie jar.
pub trait CookieJar: Send + Sync {
    /// The `Cookie` request header value for `url`, if any cookies match.
    fn cookie_header(&self, url: &str) -> Option<String>;
    /// Store a `Set-Cookie` header value received from `url`.
    fn store_set_cookie(&self, url: &str, set_cookie: &str);
}

/// A jar that stores nothing — used for sessions without cookie support.
pub struct NoopCookieJar;

impl CookieJar for NoopCookieJar {
    fn cookie_header(&self, _url: &str) -> Option<String> {
        None
    }
    fn store_set_cookie(&self, _url: &str, _set_cookie: &str) {}
}
