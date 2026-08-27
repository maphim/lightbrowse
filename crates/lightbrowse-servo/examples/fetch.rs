//! PoC driver: `cargo run --example fetch -- <url>`
//! Fetches a URL with the Servo backend and prints title/words/RAM.

use lightbrowse_core::extract::extract_text;
use lightbrowse_core::session::Session;
use lightbrowse_servo::ServoBackend;

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "https://example.com".into());
    let backend = ServoBackend::new();

    let start = std::time::Instant::now();
    match backend.navigate(&Session::new(), &url).await {
        Ok(page) => {
            let t = extract_text(&page.html);
            let elapsed = start.elapsed();
            let rss_kb = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|st| {
                    st.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|kb| kb.parse::<u64>().ok())
                })
                .unwrap_or(0);
            println!("=== SERVO PoC ===");
            println!("url:        {}", page.url);
            println!("title:      {}", page.title);
            println!("html bytes: {}", page.body_len());
            println!("words:      {}", t.word_count);
            println!("blocks:     {}", t.blocks.len());
            println!("elapsed:    {:.2}s", elapsed.as_secs_f32());
            println!("process RSS: {:.0} MB", rss_kb as f64 / 1024.0);
            println!("--- first blocks ---");
            for b in t.blocks.iter().take(5) {
                println!("  [{}] {}", b.level, b.text.chars().take(90).collect::<String>());
            }
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}
