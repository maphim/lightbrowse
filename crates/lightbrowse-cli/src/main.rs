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
    /// Attach to an EXISTING Chrome/Chromium DevTools endpoint instead of
    /// spawning a fresh headless instance — reuse the user's logged-in
    /// browser. Accepts http://host:port or ws://.../devtools/browser/...
    /// Also settable via LIGHTBROWSE_CDP_URL.
    #[arg(long, global = true)]
    cdp_url: Option<String>,
    /// Directory for browser downloads (programmatic `download` tool).
    /// Also settable via LIGHTBROWSE_DOWNLOAD_DIR. Default: ~/Downloads.
    #[arg(long, global = true)]
    download_dir: Option<std::path::PathBuf>,
    /// Path to a JS file injected into EVERY page before app scripts run
    /// (fetch/XHR hooks, network spies). Also settable via LIGHTBROWSE_PRELOAD.
    #[arg(long, global = true)]
    preload: Option<std::path::PathBuf>,
    /// Disable fingerprint-masking (navigator.webdriver, hardwareConcurrency,
    /// languages, chrome, …). On by default; disable if a site misbehaves.
    #[arg(long, global = true)]
    no_stealth: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Store (or update) credentials for a website.
    Set {
        name: String,
        url: String,
        username: String,
        password: String,
    },
    /// List entry names + urls (never secrets).
    List,
    /// Print a full entry (username + password).
    Get { name: String },
    /// Delete an entry.
    Delete { name: String },
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
    /// Human-like look: screenshot the ACTIVE tab with numbered SoM frames
    /// over interactive elements + a number→uid map. Vision LLMs pick numbers;
    /// non-vision LLMs can still act from the map JSON.
    VisualSnapshot {
        /// Optional URL — when given, navigates first (engine=cdp). When
        /// omitted, looks at the current active tab.
        url: Option<String>,
        #[arg(long, default_value_t = 40)]
        max_marks: usize,
        #[arg(long, default_value_t = 400)]
        max_nodes: usize,
        #[arg(long, default_value = "visual-snapshot.png")]
        output: String,
        /// Settle wait (ms) after navigate before looking — lets bot
        /// challenges / heavy JS finish. Default 3000.
        #[arg(long, default_value_t = 3000)]
        settle_ms: u64,
    },
    /// Click at raw viewport coordinates (CSS px) — pairs with visual-snapshot.
    ClickAt {
        x: f64,
        y: f64,
        #[arg(long)]
        session: Option<String>,
    },
    /// LLM-less extractive summary of a page (top sentences, no API key).
    Summarize {
        url: String,
        #[arg(long, default_value_t = 5)]
        max_sentences: usize,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
    },
    /// Diff two pages (or the same URL twice) line-by-line — what changed?
    Diff {
        url_a: String,
        url_b: String,
        #[arg(long, default_value = "auto", value_parser = parse_engine)]
        engine: Engine,
        /// Context lines around each change (0 = only changes).
        #[arg(long, default_value_t = 2)]
        context: usize,
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
    /// Manage the encrypted credential vault.
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
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
    // Attach to an external Chrome (--cdp-url / LIGHTBROWSE_CDP_URL) instead
    // of spawning our own headless instance.
    config.cdp_url = cli.cdp_url.clone().or_else(|| {
        std::env::var("LIGHTBROWSE_CDP_URL")
            .ok()
            .filter(|s| !s.is_empty())
    });
    // Download directory (programmatic `download` tool + setDownloadBehavior).
    config.download_dir = cli.download_dir.clone().or_else(|| {
        std::env::var("LIGHTBROWSE_DOWNLOAD_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    });
    // Per-page preload hook script.
    config.preload_script = cli.preload.clone().or_else(|| {
        std::env::var("LIGHTBROWSE_PRELOAD")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    });
    if cli.no_stealth {
        config.stealth = false;
    }
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
    // Note: an explicit --idle-timeout or LIGHTBROWSE_IDLE_TIMEOUT is honored
    // as-is for serve/MCP too; the 60s default lives in Config::default().

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
        Cmd::VisualSnapshot {
            url,
            max_marks,
            max_nodes,
            output,
            settle_ms,
        } => {
            if let Some(u) = url {
                // Live tab needed — bypass the memory cache so the CDP
                // backend actually registers the tab.
                lightbrowse_core::service::navigate(
                    &*fetch,
                    Some(&*cdp_trait),
                    &session,
                    &u,
                    Engine::Cdp,
                )
                .await?;
                if settle_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
                }
            }
            let (html, title, url) = cdp.current_dom(None).await?;
            let opts = SnapshotOptions {
                max_nodes: max_nodes.clamp(10, 2000),
                max_depth: 12,
                ..SnapshotOptions::default()
            };
            let mut tree = snapshot::snapshot(&html, &url, &opts);
            let sels = snapshot::collect_selectors(&tree);
            let rects = cdp.element_rects(&sels, None).await?;
            snapshot::attach_rects(&mut tree, &rects);

            let shot = std::env::temp_dir().join(format!("lb-shot-{}.png", std::process::id()));
            let shot_path = cdp.screenshot(&shot, false, None).await?;
            let png = std::fs::read(&shot_path)?;
            let marks = lightbrowse_core::vision::select_marks(&tree, max_marks.clamp(1, 200));
            let som_marks: Vec<lightbrowse_core::vision::Mark> = marks
                .iter()
                .map(|(label, _, _, b)| lightbrowse_core::vision::Mark {
                    label: *label,
                    bbox: *b,
                })
                .collect();
            let overlaid = lightbrowse_core::vision::overlay(&png, &som_marks)?;
            std::fs::write(&output, &overlaid)?;

            let mut map = serde_json::Map::new();
            for (label, uid, text, bbox) in &marks {
                map.insert(
                    label.to_string(),
                    json!({
                        "uid": uid,
                        "text": text,
                        "bbox": [bbox.x, bbox.y, bbox.w, bbox.h]
                    }),
                );
            }
            print_json(&json!({
                "url": url,
                "title": title,
                "count": marks.len(),
                "overlay": output,
                "map": map,
                "note": "Open the overlay image, pick the number that matches your goal, then: lightbrowse click-at <x> <y>"
            }));
        }
        Cmd::ClickAt { x, y, session } => {
            let res = cdp.click_at(x, y, session.as_deref()).await?;
            print_json(&res);
        }
        Cmd::Summarize {
            url,
            max_sentences,
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
            let s = lightbrowse_core::summarize::summarize(
                &page.html,
                max_sentences.clamp(1, 20),
            );
            print_json(&json!({
                "url": page.url,
                "title": s.title,
                "total_sentences": s.total_sentences,
                "summary": s.sentences,
            }));
        }
        Cmd::Diff {
            url_a,
            url_b,
            engine,
            context,
        } => {
            let (pa, _) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url_a,
                engine,
                ttl,
            )
            .await?;
            let (pb, _) = nav_cached(
                &memory,
                &*fetch,
                Some(&*cdp_trait),
                &session,
                &url_b,
                engine,
                ttl,
            )
            .await?;
            let ta = extract::extract_text(&pa.html).text;
            let tb = extract::extract_text(&pb.html).text;
            let full = lightbrowse_core::diff::diff_texts(&ta, &tb);
            let compacted = lightbrowse_core::diff::compact(full.clone(), context);
            let (same, added, removed) = lightbrowse_core::diff::diff_stats(&full);
            let lines: Vec<Value> = compacted
                .iter()
                .map(|l| {
                    json!({
                        "kind": match l.kind {
                            lightbrowse_core::diff::DiffKind::Same => "same",
                            lightbrowse_core::diff::DiffKind::Added => "added",
                            lightbrowse_core::diff::DiffKind::Removed => "removed",
                        },
                        "text": l.text,
                    })
                })
                .collect();
            print_json(&json!({
                "url_a": pa.url,
                "url_b": pb.url,
                "stats": { "same": same, "added": added, "removed": removed },
                "lines": lines,
            }));
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
        Cmd::Vault { cmd } => {
            use lightbrowse_core::vault::{Vault as VaultStore, VaultEntry};
            let vault =
                VaultStore::open(Default::default()).map_err(lightbrowse_core::Error::Parse)?;
            match cmd {
                VaultCmd::Set {
                    name,
                    url,
                    username,
                    password,
                } => {
                    vault
                        .set(
                            &name,
                            VaultEntry {
                                url,
                                username,
                                password,
                                extra: Default::default(),
                                updated_at: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0),
                            },
                        )
                        .map_err(lightbrowse_core::Error::Parse)?;
                    print_json(&json!({"ok": true, "name": name}));
                }
                VaultCmd::List => {
                    let items: Vec<Value> = vault
                        .list()
                        .into_iter()
                        .map(|(n, url, updated)| json!({"name": n, "url": url, "updated_at": updated}))
                        .collect();
                    print_json(&json!({"count": items.len(), "entries": items}));
                }
                VaultCmd::Get { name } => {
                    let e = vault.get(&name).ok_or_else(|| {
                        lightbrowse_core::Error::Parse(format!("vault entry '{name}' not found"))
                    })?;
                    print_json(
                        &json!({"name": name, "url": e.url, "username": e.username, "password": e.password}),
                    );
                }
                VaultCmd::Delete { name } => {
                    if !vault
                        .delete(&name)
                        .map_err(lightbrowse_core::Error::Parse)?
                    {
                        return Err(lightbrowse_core::Error::NotInitialized(format!(
                            "vault entry '{name}' not found"
                        )));
                    }
                    print_json(&json!({"ok": true, "name": name}));
                }
            }
        }
        Cmd::Mcp { engine } => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            // Encrypted credential vault (auto-creates key + vault file).
            let vault = match lightbrowse_core::vault::Vault::open(Default::default()) {
                Ok(v) => {
                    tracing::info!("vault: unlocked ({} entries)", v.list().len());
                    Some(Arc::new(v))
                }
                Err(e) => {
                    tracing::warn!("vault: unavailable — {e}");
                    None
                }
            };
            let server = lightbrowse_mcp::McpServer::new(
                fetch,
                Some(cdp_trait),
                session,
                engine,
                Some(Arc::new(memory)),
                vault,
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
