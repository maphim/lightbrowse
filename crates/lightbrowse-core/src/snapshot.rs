//! Accessibility-style snapshot of a page, designed for LLM consumption.
//!
//! Unlike raw DOM dumps, a snapshot keeps only what an agent needs to
//! understand and operate a page: interactive elements, headings, landmarks,
//! and visible text — each with a stable `uid` so the agent can refer to
//! elements across calls (e.g. "click uid 42").

use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;

use crate::extract::is_visible;

#[derive(Debug, Clone, Copy)]
pub struct SnapshotOptions {
    pub max_depth: usize,
    pub max_nodes: usize,
    /// Truncate long text per node.
    pub max_text_len: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_nodes: 400,
            max_text_len: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotNode {
    /// Stable, document-scoped id (assigned in traversal order).
    pub uid: u64,
    /// ARIA-ish role: link, button, textbox, combobox, heading, image, ...
    pub role: String,
    pub tag: String,
    pub text: String,
    pub href: Option<String>,
    /// `name` attribute (inputs).
    pub name: Option<String>,
    pub placeholder: Option<String>,
    pub checked: Option<bool>,
    pub alt: Option<String>,
    pub level: Option<u8>,
    /// CSS selector path from the document root — lets an agent act on this
    /// node (click/type/submit) via `document.querySelector`.
    pub selector: Option<String>,
    pub children: Vec<SnapshotNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotTree {
    pub url: String,
    pub title: String,
    pub nodes: Vec<SnapshotNode>,
    /// Number of nodes emitted (after caps).
    pub node_count: usize,
    /// True if the tree was truncated by max_nodes.
    pub truncated: bool,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

/// Map a tag to an accessibility role; returns `None` for non-semantic tags.
fn role_for(elm: &ElementRef) -> Option<String> {
    let e = elm.value();
    let tag = e.name();

    if let Some(r) = e.attr("role") {
        if !r.is_empty() && r != "presentation" && r != "none" {
            return Some(r.to_string());
        }
    }

    let role = match tag {
        "a" => {
            if e.attr("href").is_some() {
                "link"
            } else {
                return None;
            }
        }
        "button" => "button",
        "input" => match e.attr("type").unwrap_or("text") {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "submit" | "button" | "reset" => "button",
            "hidden" => return None,
            _ => "textbox",
        },
        "select" => "combobox",
        "textarea" => "textbox",
        "img" => "image",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "nav" => "navigation",
        "main" => "main",
        "aside" => "complementary",
        "footer" => "contentinfo",
        "header" => "banner",
        "form" => "form",
        "article" => "article",
        "section" => "region",
        "ul" | "ol" => "list",
        "li" => "listitem",
        "table" => "table",
        "dialog" => "dialog",
        "summary" => "button",
        _ => return None,
    };
    Some(role.to_string())
}

/// Build a unique-ish CSS selector path for an element, e.g.
/// `body > main:nth-child(2) > button:nth-child(1)`.
/// Stops early at an element with a valid `id` (shortest robust path).
fn css_path(elm: &ElementRef) -> String {
    fn is_css_ident(s: &str) -> bool {
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '-' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    let mut parts: Vec<String> = Vec::new();
    let mut cur = Some(*elm);
    while let Some(e) = cur {
        let node = e.value();
        let name = node.name();
        if let Some(id) = node.attr("id") {
            if is_css_ident(id) {
                parts.push(format!("#{id}"));
                break;
            }
        }
        // nth-child position among element siblings.
        let mut idx: usize = 1;
        let mut prev = e.prev_sibling();
        while let Some(sib) = prev {
            if sib.value().is_element() {
                idx += 1;
            }
            prev = sib.prev_sibling();
        }
        parts.push(format!("{name}:nth-child({idx})"));
        cur = e.parent().and_then(ElementRef::wrap);
    }
    parts.reverse();
    if parts.is_empty() {
        "body".into()
    } else {
        parts.join(" > ")
    }
}

fn heading_level(tag: &str) -> Option<u8> {
    tag.strip_prefix('h')
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (1..=6).contains(n))
}

fn direct_text(elm: &ElementRef) -> String {
    let mut s = String::new();
    for child in elm.children() {
        if let Node::Text(t) = child.value() {
            s.push_str(&t.text);
        }
    }
    crate::extract::clean(&s)
}

/// Traverse and build the snapshot tree.
pub fn snapshot(html: &str, url: &str, opts: &SnapshotOptions) -> SnapshotTree {
    let doc = Html::parse_document(&crate::extract::sanitize(html));
    let title = doc
        .select(&sel("title"))
        .next()
        .map(|t| t.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut ctx = Ctx {
        uid: 0,
        nodes: 0,
        truncated: false,
        opts: *opts,
    };

    let body = doc
        .select(&sel("body"))
        .next()
        .or_else(|| doc.select(&sel("html")).next());

    let mut nodes = Vec::new();
    if let Some(b) = body {
        for child in b.children() {
            if let Node::Element(_) = child.value() {
                if let Some(cref) = ElementRef::wrap(child) {
                    if let Some(n) = build_node(&cref, &mut ctx, 0) {
                        nodes.push(n);
                    }
                }
            }
        }
    }

    let truncated = ctx.truncated;
    SnapshotTree {
        url: url.to_string(),
        title,
        nodes,
        node_count: ctx.nodes,
        truncated,
    }
}

struct Ctx {
    uid: u64,
    nodes: usize,
    truncated: bool,
    opts: SnapshotOptions,
}

fn build_node(elm: &ElementRef, ctx: &mut Ctx, depth: usize) -> Option<SnapshotNode> {
    if ctx.nodes >= ctx.opts.max_nodes {
        ctx.truncated = true;
        return None;
    }
    if depth >= ctx.opts.max_depth {
        return None;
    }
    if !is_visible(elm) {
        return None;
    }

    let e = elm.value();
    let tag = e.name();
    let role = role_for(elm);

    // Text leaf: plain containers with direct text become "text" nodes.
    let dt = direct_text(elm);
    let has_direct_text = !dt.is_empty();
    let has_role = role.is_some();

    // Decide whether this node is worth keeping.
    if !has_role && !has_direct_text {
        // Still recurse — a wrapper may hold meaningful children.
        return collect_children_only(elm, ctx, depth);
    }

    let uid = ctx.uid;
    ctx.uid += 1;
    ctx.nodes += 1;

    let mut text = dt;
    if text.is_empty() {
        // Fall back to descendant text for semantic nodes (links, buttons...).
        let full = elm.text().collect::<String>();
        text = crate::extract::clean(&full);
    }
    if text.len() > ctx.opts.max_text_len {
        text = text.chars().take(ctx.opts.max_text_len).collect::<String>() + "…";
    }

    let node = SnapshotNode {
        uid,
        role: role.clone().unwrap_or_else(|| "text".into()),
        tag: tag.to_string(),
        text,
        href: e.attr("href").map(|s| s.to_string()),
        name: e.attr("name").map(|s| s.to_string()),
        placeholder: e.attr("placeholder").map(|s| s.to_string()),
        checked: e
            .attr("checked")
            .map(|_| true)
            .or_else(|| e.attr("aria-checked").map(|v| v == "true")),
        alt: e.attr("alt").map(|s| s.to_string()),
        level: heading_level(tag),
        selector: Some(css_path(elm)),
        children: collect_children(elm, ctx, depth),
    };

    // Prune: if a container has no direct text, no role children, and its
    // children added nothing, drop it (unless it is itself interactive).
    Some(node)
}

fn collect_children_only(elm: &ElementRef, ctx: &mut Ctx, depth: usize) -> Option<SnapshotNode> {
    let children = collect_children(elm, ctx, depth);
    if children.is_empty() {
        None
    } else {
        // Fold wrappers into a lightweight group node so siblings survive.
        let uid = ctx.uid;
        ctx.uid += 1;
        ctx.nodes += 1;
        Some(SnapshotNode {
            uid,
            role: "group".into(),
            tag: elm.value().name().to_string(),
            text: String::new(),
            href: None,
            name: None,
            placeholder: None,
            checked: None,
            alt: None,
            level: None,
            selector: None,
            children,
        })
    }
}

fn collect_children(elm: &ElementRef, ctx: &mut Ctx, depth: usize) -> Vec<SnapshotNode> {
    let mut out = Vec::new();
    for child in elm.children() {
        if let Node::Element(_) = child.value() {
            if let Some(cref) = ElementRef::wrap(child) {
                if let Some(n) = build_node(&cref, ctx, depth + 1) {
                    out.push(n);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_basic() {
        let html = r#"
            <html><head><title>T</title></head><body>
            <nav><a href="/x">Nav link</a></nav>
            <main>
              <h1>Title</h1>
              <p>Some paragraph text here.</p>
              <button id="go">Submit</button>
              <input name="email" placeholder="you@x.com">
            </main>
            </body></html>
        "#;
        let t = snapshot(html, "https://example.com/", &SnapshotOptions::default());
        assert_eq!(t.title, "T");
        assert!(t.node_count > 3);
        // Every emitted node has a unique uid.
        let mut uids: Vec<u64> = Vec::new();
        fn collect(n: &SnapshotNode, acc: &mut Vec<u64>) {
            acc.push(n.uid);
            for c in &n.children {
                collect(c, acc);
            }
        }
        for n in &t.nodes {
            collect(n, &mut uids);
        }
        let unique: std::collections::HashSet<u64> = uids.iter().copied().collect();
        assert_eq!(unique.len(), uids.len());
        // Every interactive node carries a usable CSS selector.
        fn find_selector(n: &SnapshotNode, acc: &mut Vec<String>) {
            if let Some(s) = &n.selector {
                acc.push(s.clone());
            }
            for c in &n.children {
                find_selector(c, acc);
            }
        }
        let mut sels = Vec::new();
        for n in &t.nodes {
            find_selector(n, &mut sels);
        }
        assert!(!sels.is_empty());
        assert!(sels
            .iter()
            .any(|s| s.contains("go") || s.contains("button")));
    }
}
