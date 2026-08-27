//! lightbrowse-core — engine-agnostic types and AI-oriented extraction.
//!
//! The core crate knows nothing about HTTP clients or browsers. It defines:
//! - [`Page`]: a fetched document
//! - [`Session`]: persistent browsing state (cookies, UA, history)
//! - [`BrowserBackend`]: the trait any real backend (fetch, CDP, webview) implements
//! - extractors: text / links / forms / meta / headings / search results
//! - [`snapshot`]: an accessibility-tree snapshot designed for LLM consumption

pub mod backend;
pub mod config;
pub mod cookie;
pub mod error;
pub mod extract;
pub mod page;
pub mod proxy;
pub mod service;
pub mod session;
pub mod snapshot;

pub use backend::BrowserBackend;
pub use backend::ProxyControl;
pub use config::{Config, Engine};
pub use error::{Error, Result};
pub use page::Page;
pub use proxy::{parse_proxy, ProxyKind, ProxySpec};
pub use session::{Session, SessionOptions};
