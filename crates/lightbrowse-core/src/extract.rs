//! AI-oriented extraction: turn raw HTML into compact structured data.

use std::collections::HashMap;

use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use url::Url;

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector should be valid")
}

pub fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text belonging directly to this element (not its descendants).
fn direct_text(elm: &ElementRef) -> String {
    let mut s = String::new();
    for child in elm.children() {
        if let Node::Text(t) = child.value() {
            s.push_str(&t.text);
        }
    }
    clean(&s)
}

/// Element that should never contribute visible text.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "head", "title", "meta", "link", "iframe",
    "canvas", "audio", "video",
];

pub fn is_visible(elm: &ElementRef) -> bool {
    let e = elm.value();
    if SKIP_TAGS.contains(&e.name()) {
        return false;
    }
    if e.attr("hidden").is_some() {
        return false;
    }
    if e.attr("aria-hidden") == Some("true") {
        return false;
    }
    // CSS-hidden via class (mdBook popups, mobile menus, etc.).
    if e.classes()
        .any(|c| c == "hidden" || c == "d-none" || c == "visually-hidden")
    {
        return false;
    }
    if let Some(style) = e.attr("style") {
        let s = style.to_ascii_lowercase();
        if s.contains("display:none") || s.contains("visibility:hidden") {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
/// Structural chrome detected via CSS classes (sidebars, menus, popups).
/// Matches whole class tokens only — substring matching wrongly flags
/// containers like `p-body-main--withSidebar` (which is the MAIN content).
fn is_chrome_class(elm: &ElementRef) -> bool {
    elm.value().classes().any(|c| {
        let c = c.to_ascii_lowercase();
        matches!(
            c.as_str(),
            "sidebar"
                | "menu"
                | "popup"
                | "theme"
                | "theme-list"
                | "toc"
                | "breadcrumb"
                | "toolbar"
                | "search"
                | "advert"
                | "advertisement"
                | "navbar"
                | "nav-menu"
        ) || c.starts_with("sidebar-")
            || c.starts_with("menu-")
            || c.starts_with("popup-")
            || c.starts_with("theme-list-")
    })
}

// Meta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct Meta {
    pub title: String,
    pub description: Option<String>,
    pub canonical: Option<String>,
    /// All `og:*` / `twitter:*` properties.
    pub og: HashMap<String, String>,
    pub robots: Option<String>,
    pub lang: Option<String>,
    pub charset: Option<String>,
}

pub fn extract_meta(html: &str) -> Meta {
    let doc = Html::parse_document(html);
    let mut m = Meta::default();

    if let Some(t) = doc.select(&sel("title")).next() {
        m.title = clean(&t.text().collect::<String>());
    }
    for meta in doc.select(&sel("meta")) {
        let e = meta.value();
        let name = e
            .attr("name")
            .or_else(|| e.attr("property"))
            .map(|s| s.to_ascii_lowercase());
        let content = e.attr("content").map(|s| s.to_string());
        match (name, content) {
            (Some(n), Some(c)) if n == "description" => m.description = Some(c),
            (Some(n), Some(c)) if n == "robots" => m.robots = Some(c),
            (Some(n), Some(c)) if n.starts_with("og:") || n.starts_with("twitter:") => {
                m.og.insert(n, c);
            }
            _ => {}
        }
        if let Some(cs) = e.attr("charset") {
            m.charset = Some(cs.to_string());
        }
    }
    if let Some(c) = doc.select(&sel("link[rel=canonical]")).next() {
        m.canonical = c.value().attr("href").map(|s| s.to_string());
    }
    if let Some(h) = doc.select(&sel("html")).next() {
        m.lang = h.value().attr("lang").map(|s| s.to_string());
    }
    m
}

// ---------------------------------------------------------------------------
// Headings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub id: Option<String>,
}

pub fn extract_headings(html: &str) -> Vec<Heading> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for level in 1..=6 {
        for h in doc.select(&sel(&format!("h{level}"))) {
            if !is_visible(&h) {
                continue;
            }
            let text = clean(&h.text().collect::<String>());
            if text.is_empty() {
                continue;
            }
            out.push(Heading {
                level,
                text,
                id: h.value().attr("id").map(|s| s.to_string()),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub text: String,
    pub href: String,
    pub title: Option<String>,
    /// True when `href` is a relative URL resolved against the page URL.
    pub relative: bool,
}

pub fn extract_links(html: &str, base_url: &str) -> Vec<Link> {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    let mut out = Vec::new();
    for a in doc.select(&sel("a[href]")) {
        if !is_visible(&a) {
            continue;
        }
        let raw = a.value().attr("href").unwrap_or_default();
        let href = match (&base, raw) {
            (Some(b), r) if r.starts_with('/') || r.starts_with("./") || r.starts_with("../") => b
                .join(r)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| r.to_string()),
            _ => raw.to_string(),
        };
        let text = clean(&a.text().collect::<String>());
        out.push(Link {
            text,
            href,
            title: a.value().attr("title").map(|s| s.to_string()),
            relative: false,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FormField {
    pub name: Option<String>,
    pub field_type: String,
    pub placeholder: Option<String>,
    pub required: bool,
    pub value: Option<String>,
    /// Select options: (value, label).
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Form {
    pub action: Option<String>,
    pub method: String,
    pub fields: Vec<FormField>,
}

pub fn extract_forms(html: &str, base_url: &str) -> Vec<Form> {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    let mut out = Vec::new();
    for f in doc.select(&sel("form")) {
        if !is_visible(&f) {
            continue;
        }
        let mut action = f.value().attr("action").map(|s| s.to_string());
        if let (Some(a), Some(b)) = (&action, &base) {
            if a.starts_with('/') {
                action = b.join(a).ok().map(|u| u.to_string());
            }
        }
        let method = f
            .value()
            .attr("method")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "get".into());

        let mut fields = Vec::new();
        for input in f.select(&sel("input, select, textarea")) {
            let e = input.value();
            let tag = e.name();
            let field_type = match tag {
                "select" => "select".into(),
                "textarea" => "textarea".into(),
                _ => e
                    .attr("type")
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "text".into()),
            };
            let mut options = Vec::new();
            if tag == "select" {
                for opt in input.select(&sel("option")) {
                    options.push((
                        opt.value()
                            .attr("value")
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                        clean(&opt.text().collect::<String>()),
                    ));
                }
            }
            fields.push(FormField {
                name: e.attr("name").map(|s| s.to_string()),
                field_type,
                placeholder: e.attr("placeholder").map(|s| s.to_string()),
                required: e.attr("required").is_some() || e.attr("aria-required") == Some("true"),
                value: e.attr("value").map(|s| s.to_string()),
                options,
            });
        }
        out.push(Form {
            action,
            method,
            fields,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Text (mini-readability)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TextBlock {
    /// 0 = plain paragraph, 1..=6 = heading level.
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextExtract {
    pub title: String,
    pub blocks: Vec<TextBlock>,
    /// Plain concatenated text (headings prefixed with `#`).
    pub text: String,
    pub word_count: usize,
    /// Estimated reading time in seconds (200 wpm).
    pub reading_time_secs: u64,
}

const TEXT_TAGS: &[&str] = &[
    "p",
    "li",
    "blockquote",
    "pre",
    "td",
    "th",
    "figcaption",
    "dt",
    "dd",
];

/// Score an element as a potential main-content container.
fn content_score(elm: &ElementRef) -> (f64, usize) {
    let e = elm.value();
    let tag = e.name();
    // Skip leaf text-ish tags; we score *containers*.
    if TEXT_TAGS.contains(&tag) || matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "a") {
        return (f64::NEG_INFINITY, 0);
    }
    let all_text = elm.text().collect::<String>();
    let link_text: usize = elm
        .select(&sel("a"))
        .map(|a| a.text().collect::<String>().len())
        .sum();
    let total = all_text.len();
    if total < 60 {
        return (f64::NEG_INFINITY, 0);
    }
    let non_link = total.saturating_sub(link_text) as f64;
    // Hard-exclude chrome regions outright — a nav/header/footer can contain
    // a lot of text (e.g. mdBook's keyboard-help overlay) and would otherwise
    // beat the real content on raw length. Body/html are also excluded as
    // candidates: their raw text dwarfs every child, so they'd always win;
    // we only fall back to body when nothing else qualifies.
    if matches!(
        tag,
        "nav" | "header" | "footer" | "aside" | "form" | "body" | "html"
    ) {
        return (f64::NEG_INFINITY, 0);
    }
    let mut score = non_link;
    // Main/article containers get a strong structural bonus so they beat
    // link-heavy content or text-heavy popups (mdBook help, cookie banners).
    let bonus = match tag {
        "article" => 300.0,
        "main" => 280.0,
        "section" => 80.0,
        "pre" => 20.0,
        "table" => 10.0,
        "ul" | "ol" => 10.0,
        "div" => -20.0,
        _ => -5.0,
    };
    score += bonus;
    // Density bonus: prefer nodes with little link text relative to size.
    if total > 0 {
        score += non_link / total as f64 * 20.0;
    }
    (score, total)
}

fn block_text(elm: &ElementRef) -> String {
    clean(&elm.text().collect::<String>())
}

pub fn extract_text(html: &str) -> TextExtract {
    let doc = Html::parse_document(html);
    let title = doc
        .select(&sel("title"))
        .next()
        .map(|t| clean(&t.text().collect::<String>()))
        .unwrap_or_default();

    // Pick the best main-content container.
    let mut best: Option<(f64, ElementRef)> = None;
    for elm in doc.select(&sel("*")) {
        if !is_visible(&elm) {
            continue;
        }
        let (score, _) = content_score(&elm);
        if score.is_finite() && best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, elm));
        }
    }

    let root = best.map(|(_, e)| e);

    let mut blocks: Vec<TextBlock> = Vec::new();
    if let Some(root) = root {
        collect_blocks(&root, &mut blocks, 0, 512);
    }

    // Pass 2 fallback: when scoring picked a weak container (e.g. a thread
    // header on XenForo), fall back to the element holding the most
    // non-link text — the real content on link-dense pages.
    if blocks
        .iter()
        .map(|b| b.text.split_whitespace().count())
        .sum::<usize>()
        < 300
    {
        let mut max_node: Option<ElementRef> = None;
        let mut max_len = 0usize;
        for elm in doc.select(&sel("*")) {
            if !is_visible(&elm) {
                continue;
            }
            let (score, _) = content_score(&elm);
            if score == f64::NEG_INFINITY {
                continue;
            }
            let text = elm.text().collect::<String>();
            let link_text: usize = elm
                .select(&sel("a"))
                .map(|a| a.text().collect::<String>().len())
                .sum();
            let non_link = text.len().saturating_sub(link_text);
            if non_link > max_len {
                max_len = non_link;
                max_node = Some(elm);
            }
        }
        if let Some(node) = max_node {
            let mut alt = Vec::new();
            collect_blocks(&node, &mut alt, 0, 1024);
            if !alt.is_empty() {
                blocks = alt;
            }
        }
    }

    // Fallback for tiny pages: grab body text directly.
    if blocks.is_empty() {
        if let Some(body) = doc.select(&sel("body")).next() {
            let t = block_text(&body);
            if !t.is_empty() {
                blocks.push(TextBlock { level: 0, text: t });
            }
        }
    }

    let mut text = String::new();
    for b in &blocks {
        if b.level > 0 {
            text.push_str(&"#".repeat(b.level as usize));
            text.push(' ');
        }
        text.push_str(&b.text);
        text.push('\n');
    }
    let word_count = text.split_whitespace().count();

    TextExtract {
        title,
        blocks,
        text,
        word_count,
        reading_time_secs: (word_count as u64 * 60 / 200).max(1),
    }
}

/// Depth-first collection of text blocks under `root`.
fn collect_blocks(elm: &ElementRef, blocks: &mut Vec<TextBlock>, depth: usize, max_blocks: usize) {
    if blocks.len() >= max_blocks || depth > 64 {
        return;
    }
    let e = elm.value();
    let tag = e.name();
    if !is_visible(elm) {
        return;
    }
    if let Some(level) = heading_level(tag) {
        let t = block_text(elm);
        if !t.is_empty() {
            blocks.push(TextBlock { level, text: t });
        }
    } else if TEXT_TAGS.contains(&tag) {
        let t = block_text(elm);
        if !t.is_empty() {
            blocks.push(TextBlock { level: 0, text: t });
        }
    } else if matches!(tag, "div" | "span" | "section") {
        // Div-heavy pages (XenForo, many forums) put paragraphs in <div>.
        // Only push when the element has direct text of its own.
        let dt = direct_text(elm);
        if dt.chars().count() > 3 {
            blocks.push(TextBlock { level: 0, text: dt });
        } else {
            descend(elm, blocks, depth, max_blocks);
        }
    } else {
        descend(elm, blocks, depth, max_blocks);
    }
}

fn descend(elm: &ElementRef, blocks: &mut Vec<TextBlock>, depth: usize, max_blocks: usize) {
    for child in elm.children() {
        if let Node::Element(e2) = child.value() {
            if let Some(cref) = ElementRef::wrap(child) {
                // Don't descend into nav/footer/aside when inside the main root.
                if matches!(e2.name(), "nav" | "header" | "footer" | "aside") && depth > 0 {
                    continue;
                }
                if depth > 0 && is_chrome_class(&cref) {
                    continue;
                }
                collect_blocks(&cref, blocks, depth + 1, max_blocks);
            }
        }
    }
}

fn heading_level(tag: &str) -> Option<u8> {
    tag.strip_prefix('h')
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (1..=6).contains(n))
}

// ---------------------------------------------------------------------------
// Search results (DuckDuckGo lite HTML)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub fn extract_search_results(html: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();

    // DDG lite markup: .result > .result__a (title link), .result__snippet
    for r in doc.select(&sel(".result, .web-result")) {
        let mut title = String::new();
        let mut url = String::new();
        if let Some(a) = r.select(&sel("a")).next() {
            title = clean(&a.text().collect::<String>());
            let href = a.value().attr("href").unwrap_or_default().to_string();
            url = decode_uddg(&href);
        }
        if url.is_empty() {
            continue;
        }
        let snippet = r
            .select(&sel(".result__snippet, .snippet"))
            .next()
            .map(|s| clean(&s.text().collect::<String>()))
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

/// DDG wraps target URLs in `uddg=<urlencoded>`.
fn decode_uddg(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let enc = &href[idx + 5..];
        if let Some(amp) = enc.find('&') {
            return percent_decode(&enc[..amp]);
        }
        return percent_decode(enc);
    }
    if href.starts_with("//") {
        return format!("https:{}", href);
    }
    href.to_string()
}

fn percent_decode(s: &str) -> String {
    // Minimal percent-decoding (enough for typical URLs).
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Convenience: one-call extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractMode {
    Text,
    Links,
    Forms,
    Meta,
    Headings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ExtractOutput {
    Text(TextExtract),
    Links(Vec<Link>),
    Forms(Vec<Form>),
    Meta(Meta),
    Headings(Vec<Heading>),
}

pub fn extract(html: &str, base_url: &str, mode: ExtractMode) -> ExtractOutput {
    match mode {
        ExtractMode::Text => ExtractOutput::Text(extract_text(html)),
        ExtractMode::Links => ExtractOutput::Links(extract_links(html, base_url)),
        ExtractMode::Forms => ExtractOutput::Forms(extract_forms(html, base_url)),
        ExtractMode::Meta => ExtractOutput::Meta(extract_meta(html)),
        ExtractMode::Headings => ExtractOutput::Headings(extract_headings(html)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        <!DOCTYPE html>
        <html lang="vi">
        <head>
          <title>Test Page</title>
          <meta name="description" content="a description">
          <meta property="og:title" content="OG">
          <link rel="canonical" href="https://example.com/canon">
        </head>
        <body>
          <nav><a href="/home">Home</a></nav>
          <main>
            <h1>Hello World</h1>
            <p>This is a paragraph with some text.</p>
            <a href="/about">About us</a>
            <form action="/submit" method="post">
              <input name="q" placeholder="query" required>
              <select name="kind"><option value="a">A</option></select>
            </form>
          </main>
          <footer>© 2026</footer>
        </body>
        </html>
    "#;

    #[test]
    fn meta() {
        let m = extract_meta(SAMPLE);
        assert_eq!(m.title, "Test Page");
        assert_eq!(m.description.as_deref(), Some("a description"));
        assert_eq!(m.canonical.as_deref(), Some("https://example.com/canon"));
        assert_eq!(m.og.get("og:title").map(String::as_str), Some("OG"));
        assert_eq!(m.lang.as_deref(), Some("vi"));
    }

    #[test]
    fn headings() {
        let h = extract_headings(SAMPLE);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].level, 1);
        assert_eq!(h[0].text, "Hello World");
    }

    #[test]
    fn links() {
        let l = extract_links(SAMPLE, "https://example.com/");
        assert!(l.iter().any(|x| x.href == "https://example.com/home"));
        assert!(l.iter().any(|x| x.href == "https://example.com/about"));
    }

    #[test]
    fn forms() {
        let f = extract_forms(SAMPLE, "https://example.com/");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].method, "post");
        assert_eq!(f[0].action.as_deref(), Some("https://example.com/submit"));
        assert_eq!(f[0].fields.len(), 2);
        assert!(f[0].fields[0].required);
        assert_eq!(f[0].fields[1].options[0].0, "a");
    }

    #[test]
    fn text() {
        let t = extract_text(SAMPLE);
        assert!(t.text.contains("Hello World"));
        assert!(t.text.contains("This is a paragraph"));
        assert!(!t.text.contains("© 2026")); // footer excluded
        assert!(t.word_count > 5);
    }

    #[test]
    fn uddg_decode() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fx%3Fa%3D1%26b%3D2&rut=x";
        assert_eq!(decode_uddg(href), "https://example.com/x?a=1&b=2");
    }
}
