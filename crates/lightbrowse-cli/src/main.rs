//! lightbrowse — a featherweight, AI-native browser in Rust.
//!
//! ```text
//! lightbrowse fetch    https://example.com
//! lightbrowse fetch    https://spa.example --engine cdp      # JS-rendered sites
//! lightbrowse extract  https://example.com --mode links
//! lightbrowse snapshot https://example.com
//! lightbrowse search   "rust async runtime"
//! lightbrowse ask      https://example.com "what is this page about"
//! lightbrowse memory-search "tokio async"
//! lightbrowse serve    --port 8787 --engine auto
//! lightbrowse mcp
//! ```

use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use lightbrowse_cdp::CdpBackend;
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::{Config, Engine};
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use lightbrowse_fetch::FetchBackend;
use lightbrowse_memory::{navigate_cached, MemoryStore};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "lightbrowse",
    version,
    about = "A featherweight, AI-native browser in Rust (headless-first)"
)]
struct Cli {
    /// Browsing-memory database path (default: ~/.cache/lightbrowse/memory.db).
    /// Read pages are cached + indexed here; `ask` and `memory-*` use it.
    #[arg(long, global = true)]
    memory: Option<std::path::PathBuf>,
    /// Cache TTL in seconds for repeated fetches (default 300).
    #[arg(long, global = true, default_value_t = 300)]
    cache_ttl: i64,
    /// Persistent Chrome profile directory. Set this to keep logins
    /// (cookies, localStorage) alive across runs — e.g. for Gmail or
    /// brokerage accounts. Default: temp profile (stateless).
    #[arg(long, global = true)]
    profile: Option<std::path::PathBuf>,
    /// Route all traffic through a proxy — http://host:port,
    /// https://host:port, socks5://host:port or socks5h://host:port
    /// (SOCKS5 with DNS via proxy, recommended for geo-bypass).
    /// Also settable via LIGHTBROWSE_PROXY.
    #[arg(long, global = true)]
    proxy: Option<String>,
    /// Max concurrent CDP tabs (per-session). Beyond this, the least
    /// recently used tab is evicted. Also settable via LIGHTBROWSE_MAX_TABS.
    #[arg(long, global = true)]
    max_tabs: Option<usize>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch a URL and print a page summary (title, status, text preview).
    Fetch {
        url: String,
        /// Print the raw HTML body instead of the summary.
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Extract structured data from a page.
    Extract {
        url: String,
        /// text | links | forms | meta | headings
        #[arg(long, default_value = "text")]
        mode: String,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Produce an accessibility-style snapshot tree for agents.
    Snapshot {
        url: String,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Web search via DuckDuckGo (no API key).
    Search {
        query: String,
        #[arg(long)]
        max_results: Option<usize>,
    },
    /// Start the HTTP/REST API.
    Serve {
        #[arg(long, default_value_t = 8787)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
    /// Serve the MCP (Model Context Protocol) server over stdio.
    Mcp {
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Ask a question about a page — fetch (or reuse cache), then return the
    /// most relevant text blocks with scores (intent-aware reading).
    Ask {
        url: String,
        question: String,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Search everything this browser has read (BM25 over page blocks).
    MemorySearch {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// List the most recently read pages.
    MemoryRecent {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Browsing-memory stats.
    MemoryStats,
    /// Multi-page research: read several URLs about one topic and aggregate
    /// the most relevant blocks from each (pairs with the memory cache).
    Research {
        topic: String,
        /// One or more URLs to read.
        urls: Vec<String>,
        #[arg(long, default_value_t = 4)]
        per_page: usize,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
}

#[tokio::main]
async fn main() -> lightbrowse_core::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let mut config = Config::default();
    // Headless-first: UI stays off unless explicitly requested (no GUI yet).
    if std::env::var("LIGHTBROWSE_UI").is_ok() {
        config.ui = true;
    }
    if let Ok(v) = std::env::var("LIGHTBROWSE_MEMORY_MB") {
        config.memory_budget_mb = v.parse().unwrap_or(1024);
    }
    if let Ok(v) = std::env::var("LIGHTBROWSE_IDLE_TIMEOUT") {
        config.idle_timeout_secs = v.parse().unwrap_or(60);
    }
    if let Ok(v) = std::env::var("LIGHTBROWSE_JS_WAIT_MS") {
        config.js_wait_ms = v.parse().unwrap_or(800);
    }
    if let Ok(v) = std::env::var("LIGHTBROWSE_ENGINE") {
        if let Some(e) = Engine::parse(&v) {
            config.engine = e;
        }
    }
    config.profile_dir = cli.profile.clone().or_else(|| {
        std::env::var("LIGHTBROWSE_PROFILE")
            .ok()
            .map(std::path::PathBuf::from)
    });
    // Proxy: CLI flag wins over the environment; validate early so a typo
    // fails before any request is made.
    config.proxy = cli
        .proxy
        .clone()
        .or_else(|| std::env::var("LIGHTBROWSE_PROXY").ok());
    if let Some(p) = &config.proxy {
        lightbrowse_core::parse_proxy(p)?;
    }
    // Per-tab budget: explicit flag > env > default (derived from memory
    // budget, ~250 MB per headless tab).
    config.max_tabs = cli
        .max_tabs
        .or_else(|| {
            std::env::var("LIGHTBROWSE_MAX_TABS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or_else(|| config.max_tabs_for_budget());
    // Per-command overrides must land in config BEFORE backends are built.
    if let Cmd::Serve {
        idle_timeout: Some(t),
        ..
    } = &cli.cmd
    {
        config.idle_timeout_secs = *t;
    }
    if matches!(cli.cmd, Cmd::Serve { .. } | Cmd::Mcp { .. }) {
        config.idle_timeout_secs = config.idle_timeout_secs.min(60);
    }

    let fetch: Arc<dyn BrowserBackend> =
        Arc::new(FetchBackend::with_proxy(config.proxy.as_deref())?);
    let cdp = Arc::new(CdpBackend::new(config.clone()));
    cdp.spawn_idle_watcher();
    let cdp_trait: Arc<dyn BrowserBackend> = cdp.clone();

    let session = FetchBackend::new_session(Default::default());
    let memory = open_memory(cli.memory.as_deref());
    let ttl = cli.cache_ttl;

    match cli.cmd {
        Cmd::Fetch { url, raw, engine } => {
            let (page, cached) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url,
                engine,
                ttl,
            )
            .await?;
            if raw {
                print!("{}", page.html);
                return Ok(());
            }
            let t = extract::extract_text(&page.html);
            let out = json!({
                "url": page.url,
                "title": t.title,
                "status": page.status,
                "engine": engine_name(engine),
                "cached": cached,
                "mime": page.mime,
                "body_bytes": page.body_len(),
                "truncated": page.truncated,
                "word_count": t.word_count,
                "reading_time_secs": t.reading_time_secs,
                "text_preview": t.text.chars().take(4000).collect::<String>(),
            });
            print_json(&out);
        }
        Cmd::Extract { url, mode, engine } => {
            let (page, _) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url,
                engine,
                ttl,
            )
            .await?;
            let mode = parse_mode(&mode)?;
            let data = extract::extract(&page.html, &page.url, mode);
            print_json(&json!({
                "url": page.url,
                "engine": engine_name(engine),
                "mode": mode_str(mode),
                "data": data
            }));
        }
        Cmd::Snapshot {
            url,
            max_nodes,
            engine,
        } => {
            let (page, _) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url,
                engine,
                ttl,
            )
            .await?;
            let opts = SnapshotOptions {
                max_nodes: max_nodes.unwrap_or(400).clamp(10, 2000),
                ..SnapshotOptions::default()
            };
            let tree = snapshot::snapshot(&page.html, &page.url, &opts);
            print_json(&serde_json::to_value(tree).unwrap());
        }
        Cmd::Search { query, max_results } => {
            let ddg = format!(
                "https://html.duckduckgo.com/html/?q={}",
                urlencoding(&query)
            );
            let page = fetch.navigate(&session, &ddg).await?;
            let mut results = extract::extract_search_results(&page.html);
            results.truncate(max_results.unwrap_or(8).clamp(1, 20));
            print_json(&json!({ "query": query, "results": results }));
        }
        Cmd::Ask {
            url,
            question,
            engine,
        } => {
            let (page, cached) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url,
                engine,
                ttl,
            )
            .await?;
            memory.store_page(&page).ok();
            let mut hits: Vec<serde_json::Value> = memory
                .search(&question, 6, None)
                .unwrap_or_default()
                .into_iter()
                .map(|h| {
                    let mut v = serde_json::to_value(h).unwrap_or(serde_json::Value::Null);
                    if let Some(obj) = v.as_object_mut() {
                        if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                            let clipped: String = text.chars().take(300).collect();
                            obj.insert("text".into(), serde_json::Value::String(clipped));
                        }
                    }
                    v
                })
                .collect();
            let _ = &mut hits;
            let meta = extract::extract_meta(&page.html);
            print_json(&json!({
                "url": page.url,
                "title": meta.title,
                "cached": cached,
                "question": question,
                "hits": hits,
            }));
        }
        Cmd::MemorySearch { query, limit } => {
            let hits = memory.search(&query, limit, None)?;
            print_json(&json!({ "query": query, "hits": hits }));
        }
        Cmd::MemoryRecent { limit } => {
            let pages = memory.recent(limit)?;
            print_json(&json!({ "pages": pages }));
        }
        Cmd::MemoryStats => {
            print_json(&json!({
                "pages": memory.page_count().unwrap_or(0),
                "memory_db": memory_db_path(cli.memory.as_deref()).display().to_string(),
            }));
        }
        Cmd::Research {
            topic,
            urls,
            per_page,
            engine,
        } => {
            let mut results = Vec::new();
            for url in &urls {
                let (page, _) = nav_cached(
                    &memory,
                    &*fetch,
                    Some(&*cdp_trait),
                    &session,
                    url,
                    engine,
                    ttl,
                )
                .await?;
                memory.store_page(&page).ok();
                let hits = memory
                    .search(&topic, per_page, Some(&page.url))
                    .unwrap_or_default();
                let title = extract::extract_meta(&page.html).title;
                results.push(json!({
                    "url": page.url,
                    "title": title,
                    "hits": hits,
                }));
            }
            print_json(&json!({ "topic": topic, "pages": results.len(), "results": results }));
        }
        Cmd::Serve {
            port,
            host,
            engine,
            idle_timeout: _,
        } => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let state = lightbrowse_http::AppState {
                backend: fetch,
                cdp: Some(cdp),
                session,
                sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
                engine,
                config: Arc::new(config),
                memory: Arc::new(memory),
            };
            lightbrowse_http::serve(&format!("{host}:{port}"), state).await?;
        }
        Cmd::Mcp { engine } => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let server = lightbrowse_mcp::McpServer::new(
                fetch,
                Some(cdp_trait),
                session,
                engine,
                Some(Arc::new(memory)),
            );
            server.run().await?;
        }
    }
    Ok(())
}

/// Open the browsing-memory store (file-backed by default).
fn open_memory(path: Option<&std::path::Path>) -> MemoryStore {
    let p = memory_db_path(path);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    MemoryStore::open(Some(p.to_str().unwrap_or(":memory:")))
        .unwrap_or_else(|_| MemoryStore::open(None).expect("in-memory store"))
}

fn memory_db_path(path: Option<&std::path::Path>) -> std::path::PathBuf {
    match path {
        Some(p) => p.to_path_buf(),
        None => {
            let base = std::env::var("XDG_CACHE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| std::path::PathBuf::from(h).join(".cache"))
                        .unwrap_or_else(|_| std::env::temp_dir())
                });
            base.join("lightbrowse").join("memory.db")
        }
    }
}

/// navigate with memory cache; store failures degrade to direct fetch.
async fn nav_cached(
    memory: &MemoryStore,
    fetch: &dyn BrowserBackend,
    cdp: Option<&dyn BrowserBackend>,
    session: &lightbrowse_core::session::Session,
    url: &str,
    engine: Engine,
    ttl: i64,
) -> lightbrowse_core::Result<(lightbrowse_core::Page, bool)> {
    match navigate_cached(memory, fetch, cdp, session, url, engine, ttl).await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!("memory store degraded, direct fetch: {e}");
            let page =
                lightbrowse_core::service::navigate(fetch, cdp, session, url, engine).await?;
            Ok((page, false))
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,hyper=warn"));
    // Logs go to stderr — stdout is reserved for data (MCP JSON-RPC, raw HTML).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn parse_engine(s: &str) -> Result<Engine, String> {
    Engine::parse(s).ok_or_else(|| format!("invalid engine '{s}' (expected auto|fetch|cdp)"))
}

fn engine_name(e: Engine) -> &'static str {
    match e {
        Engine::Auto => "auto",
        Engine::Fetch => "fetch",
        Engine::Cdp => "cdp",
    }
}

fn parse_mode(m: &str) -> lightbrowse_core::Result<ExtractMode> {
    match m {
        "text" => Ok(ExtractMode::Text),
        "links" => Ok(ExtractMode::Links),
        "forms" => Ok(ExtractMode::Forms),
        "meta" => Ok(ExtractMode::Meta),
        "headings" => Ok(ExtractMode::Headings),
        _ => Err(lightbrowse_core::Error::Unsupported(format!(
            "unknown mode '{m}' (expected text|links|forms|meta|headings)"
        ))),
    }
}

fn mode_str(m: ExtractMode) -> &'static str {
    match m {
        ExtractMode::Text => "text",
        ExtractMode::Links => "links",
        ExtractMode::Forms => "forms",
        ExtractMode::Meta => "meta",
        ExtractMode::Headings => "headings",
    }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes();
            bytes
                .iter()
                .map(|&b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{:02X}", b),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
