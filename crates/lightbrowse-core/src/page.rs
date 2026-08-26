use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A fetched document, as seen by a [`crate::BrowserBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    /// Final URL after redirects.
    pub url: String,
    /// `<title>` tag, if any.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Response headers (lower-cased keys).
    pub headers: HashMap<String, String>,
    /// Raw HTML body (possibly truncated per session limits).
    pub html: String,
    /// Whether the body was truncated to respect `max_html_bytes`.
    pub truncated: bool,
    /// Content-Type header value.
    pub mime: Option<String>,
}

impl Page {
    /// Body size in bytes.
    pub fn body_len(&self) -> usize {
        self.html.len()
    }

    /// Cheap check: is this an HTML document?
    pub fn is_html(&self) -> bool {
        match &self.mime {
            Some(m) => m.contains("html") || m.starts_with("text/"),
            None => self.html.trim_start().starts_with('<'),
        }
    }
}
