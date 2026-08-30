//! LLM-less extractive summarization.
//!
//! Scores sentences by content-word frequency (TF) with position bonuses —
//! the classic extractive summarization heuristic. No model, no API, ~1ms
//! on a typical page. Good enough for "what is this page about" previews.

use std::collections::HashMap;

use crate::extract::{clean, extract_text};

/// Extractive summary: top sentences in original order.
pub struct Summary {
    /// Page title.
    pub title: String,
    /// Selected sentences (original order).
    pub sentences: Vec<String>,
    /// Total sentences considered.
    pub total_sentences: usize,
}

fn split_sentences(text: &str) -> Vec<String> {
    text.split(['.', '!', '?', '\n'])
        .map(clean)
        .filter(|s| {
            let wc = s.split_whitespace().count();
            (4..=80).contains(&wc)
        })
        .filter(|s| !looks_timestamp(s))
        .collect()
}

/// Heuristic: skip fragments that are mostly timestamps/empty chrome
/// ("Yesterday at 8:57 AM", "Today at 1:22 AM", "2 minutes ago"…).
fn looks_timestamp(s: &str) -> bool {
    let low = s.to_lowercase();
    let markers = [
        "yesterday",
        "today",
        "ago",
        "minutes",
        "hours",
        "at ",
        "hôm qua",
        "hôm nay",
        "lúc ",
        "phút trước",
        "giờ trước",
    ];
    let hits = markers.iter().filter(|m| low.contains(**m)).count();
    // Mostly digits / clock tokens?
    let digits: usize = s.chars().filter(|c| c.is_ascii_digit()).count();
    let alpha: usize = s.chars().filter(|c| c.is_alphabetic()).count();
    hits >= 2 || (alpha > 0 && digits > alpha * 2)
}

fn content_words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| {
            w.len() >= 4
                && !matches!(
                    w.as_str(),
                    "the"
                        | "this"
                        | "that"
                        | "with"
                        | "from"
                        | "have"
                        | "they"
                        | "there"
                        | "their"
                        | "which"
                        | "would"
                        | "could"
                        | "about"
                        | "these"
                        | "those"
                        | "being"
                        | "been"
                        | "were"
                        | "than"
                        | "then"
                        | "when"
                        | "what"
                        | "where"
                        | "while"
                        | "after"
                        | "before"
                        | "because"
                        | "between"
                        | "through"
                        | "during"
                        | "without"
                        | "within"
                        | "across"
                        | "against"
                        | "along"
                        | "around"
                        | "under"
                        | "above"
                        | "below"
                        | "again"
                        | "further"
                        | "và"
                        | "của"
                        | "cho"
                        | "các"
                        | "được"
                        | "không"
                        | "những"
                        | "một"
                        | "với"
                        | "người"
                        | "khi"
                        | "để"
                        | "trong"
                        | "cũng"
                        | "này"
                        | "đó"
                        | "về"
                        | "như"
                        | "từ"
                        | "đã"
                        | "sẽ"
                        | "đang"
                        | "là"
                        | "có"
                )
        })
        .collect()
}

/// Summarize an HTML page extractively.
pub fn summarize(html: &str, max_sentences: usize) -> Summary {
    let ex = extract_text(html);
    let sentences = split_sentences(&ex.text);
    let total = sentences.len();
    if total == 0 {
        return Summary {
            title: ex.title,
            sentences: Vec::new(),
            total_sentences: 0,
        };
    }

    // Word frequencies across all sentences (content words only).
    let mut freq: HashMap<String, usize> = HashMap::new();
    for s in &sentences {
        for w in content_words(s) {
            *freq.entry(w).or_insert(0) += 1;
        }
    }

    // Score: avg content-word frequency × position bonus (first sentences win).
    let mut scored: Vec<(f64, usize)> = Vec::new();
    for (i, s) in sentences.iter().enumerate() {
        let cw = content_words(s);
        if cw.is_empty() {
            scored.push((0.0, i));
            continue;
        }
        let tf: f64 = cw.iter().map(|w| *freq.get(w).unwrap_or(&1) as f64).sum();
        let avg = tf / cw.len() as f64;
        let pos = if i == 0 {
            1.6
        } else if i < 3 {
            1.3
        } else {
            1.0
        };
        scored.push((avg * pos, i));
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut picked: Vec<usize> = scored
        .iter()
        .filter(|(score, _)| *score > 0.0)
        .take(max_sentences)
        .map(|(_, i)| *i)
        .collect();
    picked.sort_unstable();

    Summary {
        title: ex.title,
        sentences: picked.iter().map(|i| sentences[*i].clone()).collect(),
        total_sentences: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_basic() {
        let html = "<html><head><title>T</title></head><body>
            <p>Rust is a systems programming language focused on safety and performance.
               Rust guarantees memory safety without a garbage collector.
               The borrow checker enforces ownership rules at compile time.
               Many large companies use Rust in production today.
               Rust tooling includes cargo, clippy and rustfmt.
               The ecosystem grows quickly with crates.io packages.</p>
            <p>Python is a high-level language with dynamic typing.
               Python is popular for data science and machine learning.
               Python scripts are easy to read and write.</p>
            </body></html>";
        let s = summarize(html, 3);
        assert_eq!(s.title, "T");
        assert!(!s.sentences.is_empty());
        assert!(s.sentences.len() <= 3);
        // Top sentence should mention the dominant topic (Rust).
        assert!(s.sentences.iter().any(|x| x.contains("Rust")));
        assert_eq!(s.total_sentences, 9);
    }
}
