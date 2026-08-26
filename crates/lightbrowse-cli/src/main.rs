//! lightbrowse — a featherweight, AI-native browser in Rust.
//!
//! ```text
//! lightbrowse fetch    https://example.com
//! lightbrowse fetch    https://spa.example --engine cdp      # JS-rendered sites
//! lightbrowse extract  https://example.com --mode links
//! lightbrowse snapshot https://example.com
//! lightbrowse search   "rust async runtime"
//! lightbrowse serve    --port 8787 --engine auto
//! lightbrowse mcp
//! ```

use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use lightbrowse_cdp::CdpBackend;
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::config::{Config, Engine};
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::service;
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use lightbrowse_fetch::FetchBackend;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "lightbrowse",
    version,
    about = "A featherweight, AI-native browser in Rust (headless-first)"
)]
struct Cli {
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
    // Per-command overrides must land in config BEFORE backends are built.
    if let Cmd::Serve {
        idle_timeout: Some(t),
        ..
    } = &cli.cmd
    {
        config.idle_timeout_secs = *t;
    }
    // A long-lived serve/MCP process should suspend idle Chromium promptly;
    // one-shot commands do not spawn Chromium unless the page needs JS.
    if matches!(cli.cmd, Cmd::Serve { .. } | Cmd::Mcp { .. }) {
        config.idle_timeout_secs = config.idle_timeout_secs.min(60);
    }

    let fetch: Arc<dyn BrowserBackend> = Arc::new(FetchBackend::new()?);
    let cdp = Arc::new(CdpBackend::new(config.clone()));
    cdp.spawn_idle_watcher();
    let cdp_trait: Arc<dyn BrowserBackend> = cdp.clone();

    let session = FetchBackend::new_session(Default::default());

    match cli.cmd {
        Cmd::Fetch { url, raw, engine } => {
            let page =
                service::navigate(&*fetch, Some(&*cdp_trait), &session, &url, engine).await?;
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
            let page =
                service::navigate(&*fetch, Some(&*cdp_trait), &session, &url, engine).await?;
            let mode = parse_mode(&mode)?;
            let data = extract::extract(&page.html, &page.url, mode);
            print_json(
                &json!({ "url": page.url, "engine": engine_name(engine), "mode": mode_str(mode), "data": data }),
            );
        }
        Cmd::Snapshot {
            url,
            max_nodes,
            engine,
        } => {
            let page =
                service::navigate(&*fetch, Some(&*cdp_trait), &session, &url, engine).await?;
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
        Cmd::Serve {
            port,
            host,
            engine,
            idle_timeout,
        } => {
            if let Some(t) = idle_timeout {
                config.idle_timeout_secs = t;
            }
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let state = lightbrowse_http::AppState {
                backend: fetch,
                cdp: Some(cdp),
                session,
                engine,
                config: Arc::new(config),
            };
            lightbrowse_http::serve(&format!("{host}:{port}"), state).await?;
        }
        Cmd::Mcp { engine } => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let server = lightbrowse_mcp::McpServer::new(fetch, Some(cdp_trait), session, engine);
            server.run().await?;
        }
    }
    Ok(())
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
