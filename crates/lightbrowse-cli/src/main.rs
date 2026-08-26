//! lightbrowse — a featherweight, AI-native browser in Rust.
//!
//! ```text
//! lightbrowse fetch    https://example.com
//! lightbrowse extract  https://example.com --mode links
//! lightbrowse snapshot https://example.com
//! lightbrowse search   "rust async runtime"
//! lightbrowse serve    --port 8787
//! lightbrowse mcp
//! ```

use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use lightbrowse_core::backend::BrowserBackend;
use lightbrowse_core::extract::{self, ExtractMode};
use lightbrowse_core::snapshot::{self, SnapshotOptions};
use lightbrowse_fetch::FetchBackend;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "lightbrowse",
    version,
    about = "A featherweight, AI-native browser in Rust"
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
    },
    /// Extract structured data from a page.
    Extract {
        url: String,
        /// text | links | forms | meta | headings
        #[arg(long, default_value = "text")]
        mode: String,
    },
    /// Produce an accessibility-style snapshot tree for agents.
    Snapshot {
        url: String,
        #[arg(long)]
        max_nodes: Option<usize>,
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
    },
    /// Serve the MCP (Model Context Protocol) server over stdio.
    Mcp,
}

#[tokio::main]
async fn main() -> lightbrowse_core::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let backend: Arc<dyn BrowserBackend> = Arc::new(FetchBackend::new()?);

    match cli.cmd {
        Cmd::Fetch { url, raw } => {
            let session = FetchBackend::new_session(Default::default());
            let page = backend.navigate(&session, &url).await?;
            if raw {
                print!("{}", page.html);
                return Ok(());
            }
            let t = extract::extract_text(&page.html);
            let out = json!({
                "url": page.url,
                "title": t.title,
                "status": page.status,
                "mime": page.mime,
                "body_bytes": page.body_len(),
                "truncated": page.truncated,
                "word_count": t.word_count,
                "reading_time_secs": t.reading_time_secs,
                "text_preview": t.text.chars().take(4000).collect::<String>(),
            });
            print_json(&out);
        }
        Cmd::Extract { url, mode } => {
            let session = FetchBackend::new_session(Default::default());
            let page = backend.navigate(&session, &url).await?;
            let mode = parse_mode(&mode)?;
            let data = extract::extract(&page.html, &page.url, mode);
            print_json(&json!({ "url": page.url, "mode": mode_str(mode), "data": data }));
        }
        Cmd::Snapshot { url, max_nodes } => {
            let session = FetchBackend::new_session(Default::default());
            let page = backend.navigate(&session, &url).await?;
            let opts = SnapshotOptions {
                max_nodes: max_nodes.unwrap_or(400).clamp(10, 2000),
                ..SnapshotOptions::default()
            };
            let tree = snapshot::snapshot(&page.html, &page.url, &opts);
            print_json(&serde_json::to_value(tree).unwrap());
        }
        Cmd::Search { query, max_results } => {
            let session = FetchBackend::new_session(Default::default());
            let ddg = format!(
                "https://html.duckduckgo.com/html/?q={}",
                urlencoding(&query)
            );
            let page = backend.navigate(&session, &ddg).await?;
            let mut results = extract::extract_search_results(&page.html);
            results.truncate(max_results.unwrap_or(8).clamp(1, 20));
            print_json(&json!({ "query": query, "results": results }));
        }
        Cmd::Serve { port, host } => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let state = lightbrowse_http::AppState { backend, session };
            lightbrowse_http::serve(&format!("{host}:{port}"), state).await?;
        }
        Cmd::Mcp => {
            let session = Arc::new(Mutex::new(FetchBackend::new_session(Default::default())));
            let server = lightbrowse_mcp::McpServer::new(backend, session);
            server.run().await?;
        }
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,hyper=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
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
