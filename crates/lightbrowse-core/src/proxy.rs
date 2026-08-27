//! Proxy support — parse and normalize proxy URLs for both backends.
//!
//! Supported schemes:
//! - `http://host:port` — HTTP (CONNECT) proxy
//! - `https://host:port` — HTTPS proxy (TLS to the proxy itself)
//! - `socks5://host:port` — SOCKS5 proxy (DNS resolved client-side)
//! - `socks5h://host:port` — SOCKS5 proxy with DNS resolved *through* the
//!   proxy (no DNS leak; recommended for privacy / geo-bypass)
//!
//! A missing port defaults to `1080` for SOCKS and `8080` for HTTP(S),
//! matching common conventions.

use crate::error::{Error, Result};

/// Proxy kind, normalized from the URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// Plain HTTP CONNECT proxy (`http://`).
    Http,
    /// TLS-encrypted HTTP proxy (`https://`).
    Https,
    /// SOCKS5 (`socks5://` or `socks5h://`).
    Socks5,
}

/// A validated proxy target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySpec {
    /// Original URL as given by the caller (with the scheme normalized).
    pub url: String,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// True when the original scheme was `socks5h://` (DNS via proxy).
    pub dns_via_proxy: bool,
}

impl ProxySpec {
    /// Value for reqwest's `Proxy::all` (fetch backend).
    pub fn reqwest_url(&self) -> String {
        match self.kind {
            ProxyKind::Socks5 => {
                // reqwest understands socks5h for DNS-through-proxy.
                if self.dns_via_proxy {
                    format!("socks5h://{}:{}", self.host, self.port)
                } else {
                    format!("socks5://{}:{}", self.host, self.port)
                }
            }
            ProxyKind::Http => format!("http://{}:{}", self.host, self.port),
            ProxyKind::Https => format!("https://{}:{}", self.host, self.port),
        }
    }

    /// Value for Chromium's `--proxy-server` flag (CDP backend).
    ///
    /// Chromium does not accept a `socks5h` scheme; for SOCKS5 it always
    /// resolves hostnames through the proxy when the connection is made via
    /// the proxy, so `socks5://` is the correct spelling either way.
    pub fn chrome_arg(&self) -> String {
        match self.kind {
            ProxyKind::Socks5 => format!("socks5://{}:{}", self.host, self.port),
            ProxyKind::Http => format!("http://{}:{}", self.host, self.port),
            ProxyKind::Https => format!("https://{}:{}", self.host, self.port),
        }
    }

    /// Human-readable summary (used by `/v1/proxy` and MCP responses).
    pub fn describe(&self) -> String {
        match self.kind {
            ProxyKind::Socks5 if self.dns_via_proxy => {
                format!("socks5h://{}:{} (DNS via proxy)", self.host, self.port)
            }
            ProxyKind::Socks5 => format!("socks5://{}:{}", self.host, self.port),
            ProxyKind::Http => format!("http://{}:{}", self.host, self.port),
            ProxyKind::Https => format!("https://{}:{}", self.host, self.port),
        }
    }
}

/// Parse a proxy URL string into a validated [`ProxySpec`].
///
/// Accepts `http://`, `https://`, `socks5://`, `socks5h://`. A missing port
/// defaults to 1080 (SOCKS) or 8080 (HTTP/HTTPS).
pub fn parse_proxy(input: &str) -> Result<ProxySpec> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::InvalidUrl("empty proxy URL".into()));
    }

    let (scheme, rest) = match input.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => {
            return Err(Error::InvalidUrl(format!(
            "proxy URL '{input}' must include a scheme (http://, https://, socks5://, socks5h://)"
        )))
        }
    };

    let (kind, dns_via_proxy) = match scheme.as_str() {
        "http" => (ProxyKind::Http, false),
        "https" => (ProxyKind::Https, false),
        "socks5" => (ProxyKind::Socks5, false),
        "socks5h" => (ProxyKind::Socks5, true),
        other => {
            return Err(Error::InvalidUrl(format!(
                "unsupported proxy scheme '{other}' (expected http://, https://, socks5://, socks5h://)"
            )))
        }
    };

    // Strip any userinfo (proxy auth is out of scope for v1; reject clearly).
    let (rest, _userinfo) = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => (hostport, Some(userinfo)),
        None => (rest, None),
    };

    // host:port — split on the LAST ':' so IPv6 literals like [::1]:1080 work.
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => {
            let p: u16 = p
                .parse()
                .map_err(|_| Error::InvalidUrl(format!("invalid proxy port in '{input}'")))?;
            if p == 0 {
                return Err(Error::InvalidUrl("proxy port must be non-zero".into()));
            }
            (h.trim_matches(['[', ']']).to_string(), p)
        }
        None => {
            let p = if kind == ProxyKind::Socks5 {
                1080
            } else {
                8080
            };
            (rest.to_string(), p)
        }
    };

    if host.is_empty() {
        return Err(Error::InvalidUrl(format!(
            "missing proxy host in '{input}'"
        )));
    }

    Ok(ProxySpec {
        url: format!("{scheme}://{host}:{port}"),
        kind,
        host,
        port,
        dns_via_proxy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_with_port() {
        let p = parse_proxy("http://127.0.0.1:8888").unwrap();
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 8888);
        assert_eq!(p.reqwest_url(), "http://127.0.0.1:8888");
        assert_eq!(p.chrome_arg(), "http://127.0.0.1:8888");
    }

    #[test]
    fn parses_socks5h_dns_via_proxy() {
        let p = parse_proxy("socks5h://my-proxy.example:1080").unwrap();
        assert_eq!(p.kind, ProxyKind::Socks5);
        assert!(p.dns_via_proxy);
        assert_eq!(p.reqwest_url(), "socks5h://my-proxy.example:1080");
        // Chromium has no socks5h scheme — normalized to socks5.
        assert_eq!(p.chrome_arg(), "socks5://my-proxy.example:1080");
    }

    #[test]
    fn defaults_ports() {
        let socks = parse_proxy("socks5://proxy.local").unwrap();
        assert_eq!(socks.port, 1080);
        let http = parse_proxy("http://proxy.local").unwrap();
        assert_eq!(http.port, 8080);
    }

    #[test]
    fn handles_ipv6_literal() {
        let p = parse_proxy("socks5://[::1]:9050").unwrap();
        assert_eq!(p.host, "::1");
        assert_eq!(p.port, 9050);
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(parse_proxy("").is_err());
        assert!(parse_proxy("nonsense").is_err());
        assert!(parse_proxy("ftp://host:21").is_err());
        assert!(parse_proxy("http://host:notaport").is_err());
        assert!(parse_proxy("http://:8080").is_err());
        assert!(parse_proxy("socks4://host:1080").is_err());
    }

    #[test]
    fn case_insensitive_scheme() {
        let p = parse_proxy("SOCKS5H://host:1080").unwrap();
        assert_eq!(p.kind, ProxyKind::Socks5);
        assert!(p.dns_via_proxy);
    }
}
