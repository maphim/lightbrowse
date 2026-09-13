//! Model-free reduction of tool output before it reaches an LLM.
//!
//! Ported from the "context surface" mechanism in CodeLocal
//! (`internal/contextsurface/reducer.go`, Go, Apache-2.0): score the lines (or
//! tree nodes) of an observation, keep the highest-signal ones inside a token
//! budget, preserve local context around the signal, and fall back to a
//! head+tail projection when the input carries no semantic signal at all.
//!
//! Design invariants:
//! 1. **Deterministic and model-free** — no LLM call, same input → same output.
//! 2. **Never silently lossy** — every projection reports `strategy`,
//!    `original_tokens` and `reduced_tokens`; kept lines stay in document order.
//! 3. **Never empty** — a signal-free observation falls back to head+tail.
//! 4. **Context is never orphaned** — a kept interactive node keeps its
//!    ancestors (and nearby siblings), so an agent never sees "button Submit"
//!    without the form it belongs to.
//!
//! Token counting is an estimate (~4 Unicode chars per token), matching the
//! heuristic used by the source implementation. It is deliberately *not* a
//! model billing tokenizer.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::snapshot::{SnapshotNode, SnapshotTree};

/// Default budget for a single reduced observation (tokens).
pub const DEFAULT_MAX_TOKENS: usize = 512;
/// Lower clamp applied to any non-zero budget.
pub const MIN_MAX_TOKENS: usize = 256;
/// Upper clamp applied to any non-zero budget.
pub const MAX_MAX_TOKENS: usize = 4096;
/// A line/node scoring at or above this is considered a semantic signal.
pub const SIGNAL_SCORE: i32 = 50;
/// Marker inserted where the syntactic fallback dropped content.
pub const PRUNE_MARKER: &str = "… output pruned; full content available via the source URL …";

const ERROR_TOKENS: &[&str] = &[
    "error",
    "failed",
    "failure",
    "panic",
    "exception",
    "fatal",
    "conflict",
    "denied",
    "timeout",
    "assert",
];

const WARNING_TOKENS: &[&str] = &["warning", "warn:", "exit code", "exit="];

const TREE_INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "listbox",
    "checkbox",
    "radio",
    "menuitem",
    "tab",
    "switch",
    "slider",
    "spinbutton",
];

const TREE_PROBLEM_TOKENS: &[&str] = &[
    "not found",
    "invalid",
    "required",
    "disabled",
    "stale",
    "blocked",
    "forbidden",
    "captcha",
    "expired",
];

const CONSOLE_TOKENS: &[&str] = &[
    "uncaught",
    "failed to load",
    "refused",
    "cors",
    "mixed content",
    "deprecated",
    "403",
    "404",
    "429",
    "500",
];

const NETWORK_TOKENS: &[&str] = &[
    "set-cookie",
    "location:",
    "redirect",
    "403",
    "404",
    "429",
    "500",
    "502",
    "503",
    "failed",
];

/// What kind of observation is being reduced — drives the keyword weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    PageText,
    BrowserTree,
    ConsoleLog,
    NetworkTrace,
    Evaluate,
    Generic,
}

impl ObservationKind {
    /// Short stable name (used in fingerprints and JSON output).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PageText => "page_text",
            Self::BrowserTree => "browser_tree",
            Self::ConsoleLog => "console_log",
            Self::NetworkTrace => "network_trace",
            Self::Evaluate => "evaluate",
            Self::Generic => "generic",
        }
    }

    /// Classify an MCP/CLI operation into an observation kind.
    pub fn classify(tool: &str, operation: &str) -> Self {
        let tool = tool.to_ascii_lowercase();
        let operation = operation.to_ascii_lowercase();
        let hay = format!("{tool} {operation}");
        if hay.contains("snapshot") || hay.contains("visual") || hay.contains("ui_tree") {
            Self::BrowserTree
        } else if hay.contains("console") {
            Self::ConsoleLog
        } else if hay.contains("network") || hay.contains("requests") {
            Self::NetworkTrace
        } else if hay.contains("evaluate") || hay.contains("script") {
            Self::Evaluate
        } else if hay.contains("extract")
            || hay.contains("fetch")
            || hay.contains("navigate")
            || hay.contains("page")
            || hay.contains("text")
            || hay.contains("ask")
        {
            Self::PageText
        } else {
            Self::Generic
        }
    }
}

/// Reduction budget and context-guard settings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReduceConfig {
    /// Token budget for one observation. `0` disables reduction entirely.
    /// Non-zero values are clamped to `[MIN_MAX_TOKENS, MAX_MAX_TOKENS]`.
    pub max_tokens: usize,
    /// Floor the score of the lines surrounding a high-signal line.
    pub preserve_neighbors: bool,
    /// How many ancestor levels of a kept tree node are mandatory context.
    pub tree_ancestor_depth: usize,
    /// How many siblings on each side of a kept node are kept.
    pub tree_sibling_window: usize,
}

impl Default for ReduceConfig {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            preserve_neighbors: true,
            tree_ancestor_depth: 2,
            tree_sibling_window: 1,
        }
    }
}

impl ReduceConfig {
    /// A config with an explicit budget (`0` disables reduction).
    pub fn with_max_tokens(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            ..Self::default()
        }
    }

    /// Enable/disable the neighbour score floor.
    pub fn preserving_neighbors(mut self, preserve: bool) -> Self {
        self.preserve_neighbors = preserve;
        self
    }

    fn budget(&self) -> usize {
        self.max_tokens.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS)
    }
}

/// Which strategy produced a [`Reduction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReduceStrategy {
    /// Input already fit the budget; returned untouched.
    Passthrough,
    /// Semantic signal found; highest-scoring lines kept in document order.
    Ranked,
    /// No signal; head (3/4) + tail (1/4) around a prune marker.
    HeadTail,
    /// Tree: highest-scoring nodes kept, plus mandatory ancestor context.
    TreeRanked,
    /// Tree: no signal; kept document-order nodes until the budget filled.
    TreeFirstN,
}

/// A bounded projection of one tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reduction {
    /// Stable content-addressed id: `obs_<fnv64 hex>`.
    pub id: String,
    pub kind: ObservationKind,
    /// The text that should be shown to the model.
    pub text: String,
    pub original_tokens: usize,
    pub reduced_tokens: usize,
    pub strategy: ReduceStrategy,
    /// True when content was dropped from `text`.
    pub truncated: bool,
}

impl Reduction {
    /// Tokens removed by the projection.
    pub fn saved_tokens(&self) -> usize {
        self.original_tokens.saturating_sub(self.reduced_tokens)
    }

    /// One-line audit marker for the model, e.g.
    /// `[reduced 4210→380 tokens · ranked · full content: re-fetch the URL]`.
    pub fn marker(&self) -> String {
        let strategy = match self.strategy {
            ReduceStrategy::Passthrough => "passthrough",
            ReduceStrategy::Ranked => "ranked",
            ReduceStrategy::HeadTail => "head_tail",
            ReduceStrategy::TreeRanked => "tree_ranked",
            ReduceStrategy::TreeFirstN => "tree_first_n",
        };
        format!(
            "[reduced {}→{} tokens · {} · id={}]",
            self.original_tokens, self.reduced_tokens, strategy, self.id
        )
    }
}

/// Stats for a [`prune_snapshot`] call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PruneStats {
    pub strategy: ReduceStrategy,
    pub original_nodes: usize,
    pub kept_nodes: usize,
    pub removed_nodes: usize,
    pub original_tokens: usize,
    pub reduced_tokens: usize,
    pub truncated: bool,
}

/// Estimate tokens for `text` (~4 Unicode chars per token, `ceil`).
pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(4)
}

/// Stable 64-bit FNV-1a fingerprint (hex, 16 chars).
pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Content-addressed observation id.
pub fn observation_id(kind: ObservationKind, payload: &str) -> String {
    let mut key = String::with_capacity(kind.as_str().len() + payload.len() + 1);
    key.push_str(kind.as_str());
    key.push('\u{1f}');
    key.push_str(payload);
    format!("obs_{}", fingerprint(key.as_bytes()))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Score a single line of text output. Higher = more likely to matter.
pub fn score_line(line: &str, kind: ObservationKind) -> i32 {
    let text = line.trim().to_ascii_lowercase();
    if text.is_empty() {
        return 0;
    }
    let mut score = 1;
    if contains_any(&text, ERROR_TOKENS) {
        score += 100;
    }
    if contains_any(&text, WARNING_TOKENS) {
        score += 45;
    }
    match kind {
        ObservationKind::ConsoleLog => {
            if contains_any(&text, CONSOLE_TOKENS) {
                score += 60;
            }
        }
        ObservationKind::NetworkTrace => {
            if contains_any(&text, NETWORK_TOKENS) {
                score += 60;
            }
        }
        ObservationKind::BrowserTree | ObservationKind::PageText => {
            if contains_any(
                &text,
                &[
                    "uid=",
                    "selector",
                    "dialog",
                    "modal",
                    "accessibility",
                    "focus",
                ],
            ) {
                score += 40;
            }
        }
        ObservationKind::Evaluate => {
            if contains_any(&text, &["undefined", "null", "true", "false"]) {
                score += 30;
            }
        }
        ObservationKind::Generic => {}
    }
    score
}

fn bound_single_line(line: &str, budget: usize) -> String {
    let max_chars = budget.saturating_mul(4).max(256);
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Clip a single line so it can never exceed `budget` tokens (~4 chars/token).
/// Used as the guaranteed-non-empty last resort: a long line is cut, never dropped.
fn hard_clip(line: &str, budget: usize) -> String {
    let budget = budget.max(1);
    let max_chars = budget.saturating_mul(4).saturating_sub(1).max(1);
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn split_lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn select_ranked(lines: &[&str], kind: ObservationKind, cfg: &ReduceConfig) -> Option<Vec<String>> {
    let budget = cfg.budget();
    let mut scores: Vec<i32> = lines.iter().map(|line| score_line(line, kind)).collect();
    let last = lines.len().saturating_sub(1);
    for (index, score) in scores.iter_mut().enumerate() {
        if index < 2 || index + 2 > last {
            *score += 8;
        }
    }
    if !scores.iter().any(|score| *score >= SIGNAL_SCORE) {
        return None;
    }
    if cfg.preserve_neighbors {
        let snapshot = scores.clone();
        for (index, score) in snapshot.iter().enumerate() {
            if *score < SIGNAL_SCORE {
                continue;
            }
            if index > 0 && scores[index - 1] < 35 {
                scores[index - 1] = 35;
            }
            if index + 1 < scores.len() && scores[index + 1] < 30 {
                scores[index + 1] = 30;
            }
        }
    }
    let mut order: Vec<usize> = (0..lines.len()).filter(|i| scores[*i] > 0).collect();
    order.sort_by(|a, b| scores[*b].cmp(&scores[*a]).then(a.cmp(b)));

    // Greedy fill in score order, accounting for the newline separator.
    let mut chosen: Vec<(usize, String)> = Vec::new();
    let mut used = 0usize;
    for index in order {
        let line = bound_single_line(lines[index], budget);
        let tokens = estimate_tokens(&line).max(1) + 1;
        if used + tokens > budget {
            continue;
        }
        used += tokens;
        chosen.push((index, line));
        if used >= budget {
            break;
        }
    }
    if chosen.is_empty() {
        return None;
    }
    // Joining adds the separators the greedy pass could only approximate; drop
    // the lowest-scoring lines (pushed last) until the real payload fits.
    while chosen.len() > 1 {
        let joined = chosen
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if estimate_tokens(&joined) <= budget {
            break;
        }
        chosen.pop();
    }
    chosen.sort_by_key(|(index, _)| *index);
    Some(chosen.into_iter().map(|(_, line)| line).collect())
}

fn select_head_tail(lines: &[&str], budget: usize) -> String {
    let head_budget = (budget * 3 / 4).max(1);
    let tail_budget = budget.saturating_sub(head_budget).max(1);
    let mut head: Vec<String> = Vec::new();
    let mut head_used = 0usize;
    for line in lines {
        // Always keep the first line (clipped) so the projection is never empty.
        let line = hard_clip(line, head_budget);
        let tokens = estimate_tokens(&line).max(1);
        if !head.is_empty() && head_used + tokens > head_budget {
            break;
        }
        head_used += tokens;
        head.push(line);
    }
    let mut tail: Vec<String> = Vec::new();
    let mut tail_used = 0usize;
    for line in lines.iter().rev() {
        let line = hard_clip(line, tail_budget);
        let tokens = estimate_tokens(&line).max(1);
        if !tail.is_empty() && tail_used + tokens > tail_budget {
            break;
        }
        tail_used += tokens;
        tail.push(line);
    }
    tail.reverse();
    if tail.is_empty() {
        return head.join("\n");
    }
    // The per-section budgets only approximate the joined size; shrink the
    // projection until the measured payload respects the real budget.
    loop {
        let mut combined = head.clone();
        combined.push(PRUNE_MARKER.to_string());
        combined.extend(tail.iter().cloned());
        let joined = combined.join("\n");
        if estimate_tokens(&joined) <= budget {
            return joined;
        }
        if !tail.is_empty() {
            tail.remove(0);
        } else if head.len() > 1 {
            head.pop();
        } else {
            return hard_clip(&joined, budget);
        }
    }
}

/// Reduce a text observation to `cfg.max_tokens` tokens.
///
/// A budget of `0` disables reduction and returns the input untouched.
pub fn reduce_text(text: &str, kind: ObservationKind, cfg: &ReduceConfig) -> Reduction {
    let trimmed = text.trim();
    let original_tokens = estimate_tokens(trimmed);
    let id = observation_id(kind, trimmed);

    let passthrough = |strategy: ReduceStrategy| Reduction {
        id: id.clone(),
        kind,
        text: trimmed.to_string(),
        original_tokens,
        reduced_tokens: original_tokens,
        strategy,
        truncated: false,
    };

    if cfg.max_tokens == 0 || trimmed.is_empty() || original_tokens <= cfg.budget() {
        return passthrough(ReduceStrategy::Passthrough);
    }

    let lines = split_lines(trimmed);
    if lines.is_empty() {
        return passthrough(ReduceStrategy::Passthrough);
    }

    let (payload, strategy) = match select_ranked(&lines, kind, cfg) {
        Some(selected) if !selected.is_empty() => (selected.join("\n"), ReduceStrategy::Ranked),
        _ => (
            select_head_tail(&lines, cfg.budget()),
            ReduceStrategy::HeadTail,
        ),
    };
    // Never hand the model an empty observation: a single over-long line is
    // clipped to the budget instead of dropped.
    let payload = if payload.trim().is_empty() {
        hard_clip(trimmed, cfg.budget())
    } else {
        payload
    };
    let reduced_tokens = estimate_tokens(&payload);
    Reduction {
        id,
        kind,
        text: payload,
        original_tokens,
        reduced_tokens,
        strategy,
        truncated: true,
    }
}

struct ScoredNode {
    path: Vec<usize>,
    order: usize,
    score: i32,
    tokens: usize,
}

fn score_node(node: &SnapshotNode, depth: usize) -> (i32, bool) {
    let role = node.role.to_ascii_lowercase();
    let text = node.text.to_ascii_lowercase();
    let mut score = 1;
    let interactive = TREE_INTERACTIVE_ROLES.contains(&role.as_str());
    if interactive {
        score += 120;
    }
    if role == "heading" {
        score += 60;
    }
    if contains_any(&text, ERROR_TOKENS) {
        score += 100;
    }
    if contains_any(&text, TREE_PROBLEM_TOKENS) {
        score += 60;
    }
    if depth == 0 {
        score += 8;
    }
    (score, interactive)
}

fn node_tokens(node: &SnapshotNode) -> usize {
    let mut rendered = String::with_capacity(node.role.len() + node.text.len() + 32);
    rendered.push_str(&node.role);
    rendered.push(' ');
    rendered.push_str(&node.text);
    if let Some(selector) = &node.selector {
        rendered.push(' ');
        rendered.push_str(selector);
    }
    estimate_tokens(&rendered).max(1)
}

fn collect_nodes(
    nodes: &[SnapshotNode],
    prefix: &mut Vec<usize>,
    order: &mut usize,
    flat: &mut Vec<ScoredNode>,
) {
    for (index, node) in nodes.iter().enumerate() {
        prefix.push(index);
        let (score, _interactive) = score_node(node, prefix.len() - 1);
        flat.push(ScoredNode {
            path: prefix.clone(),
            order: *order,
            score,
            tokens: node_tokens(node),
        });
        *order += 1;
        collect_nodes(&node.children, prefix, order, flat);
        prefix.pop();
    }
}

fn retain_paths(
    nodes: &mut Vec<SnapshotNode>,
    keep: &BTreeSet<Vec<usize>>,
    prefix: &mut Vec<usize>,
) {
    let mut index = 0usize;
    nodes.retain_mut(|node| {
        prefix.push(index);
        index += 1;
        let kept = keep.contains(prefix);
        if kept {
            retain_paths(&mut node.children, keep, prefix);
        }
        prefix.pop();
        kept
    });
}

/// Reduce a snapshot tree to `cfg.max_tokens` tokens, in place.
///
/// Keeps the highest-signal nodes (`interactive`, error/heading text, document
/// roots) and then re-adds **mandatory context**: up to
/// `tree_ancestor_depth` ancestors and `tree_sibling_window` siblings per kept
/// node. Mandatory context is never dropped to fit the budget — an agent must
/// never receive a button without the form around it.
pub fn prune_snapshot(tree: &mut SnapshotTree, cfg: &ReduceConfig) -> PruneStats {
    let mut flat: Vec<ScoredNode> = Vec::new();
    let mut prefix: Vec<usize> = Vec::new();
    let mut order = 0usize;
    collect_nodes(&tree.nodes, &mut prefix, &mut order, &mut flat);

    let original_nodes = flat.len();
    let original_tokens: usize = flat.iter().map(|node| node.tokens).sum();
    let unchanged = PruneStats {
        strategy: ReduceStrategy::Passthrough,
        original_nodes,
        kept_nodes: original_nodes,
        removed_nodes: 0,
        original_tokens,
        reduced_tokens: original_tokens,
        truncated: false,
    };
    if cfg.max_tokens == 0 || original_nodes == 0 || original_tokens <= cfg.budget() {
        return unchanged;
    }

    let mut ranking: Vec<usize> = (0..flat.len()).collect();
    ranking.sort_by(|a, b| {
        flat[*b]
            .score
            .cmp(&flat[*a].score)
            .then(flat[*a].order.cmp(&flat[*b].order))
    });

    let mut keep: BTreeSet<Vec<usize>> = BTreeSet::new();
    let mut used = 0usize;
    let mut has_signal = false;
    for index in &ranking {
        let node = &flat[*index];
        if node.score < SIGNAL_SCORE {
            continue;
        }
        has_signal = true;
        if used + node.tokens > cfg.budget() {
            continue;
        }
        used += node.tokens;
        keep.insert(node.path.clone());
    }

    let strategy = if has_signal {
        ReduceStrategy::TreeRanked
    } else {
        ReduceStrategy::TreeFirstN
    };
    if !has_signal {
        for node in &flat {
            if used + node.tokens > cfg.budget() {
                break;
            }
            used += node.tokens;
            keep.insert(node.path.clone());
        }
    }
    if keep.is_empty() {
        return unchanged;
    }

    // Mandatory context: ancestors and neighbouring siblings are never dropped.
    let selected: Vec<Vec<usize>> = keep.iter().cloned().collect();
    for path in &selected {
        for depth in 1..=cfg.tree_ancestor_depth {
            if path.len() > depth {
                keep.insert(path[..path.len() - depth].to_vec());
            }
        }
        if cfg.tree_sibling_window == 0 || path.len() < 2 {
            continue;
        }
        let (parent, index) = path.split_at(path.len() - 1);
        let index = index[0];
        for delta in 1..=cfg.tree_sibling_window {
            if index >= delta {
                let mut sibling = parent.to_vec();
                sibling.push(index - delta);
                keep.insert(sibling);
            }
            let mut sibling = parent.to_vec();
            sibling.push(index + delta);
            keep.insert(sibling);
        }
    }

    let reduced_tokens: usize = flat
        .iter()
        .filter(|node| keep.contains(&node.path))
        .map(|node| node.tokens)
        .sum();
    let kept_nodes = flat.iter().filter(|node| keep.contains(&node.path)).count();
    let removed_nodes = original_nodes - kept_nodes;

    retain_paths(&mut tree.nodes, &keep, &mut Vec::new());
    tree.node_count = kept_nodes;
    tree.truncated = tree.truncated || removed_nodes > 0;

    PruneStats {
        strategy,
        original_nodes,
        kept_nodes,
        removed_nodes,
        original_tokens,
        reduced_tokens,
        truncated: removed_nodes > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Bbox, SnapshotNode, SnapshotTree};

    fn node(uid: u64, role: &str, text: &str, children: Vec<SnapshotNode>) -> SnapshotNode {
        SnapshotNode {
            uid,
            role: role.to_string(),
            tag: "div".to_string(),
            text: text.to_string(),
            href: None,
            name: None,
            input_type: None,
            placeholder: None,
            checked: None,
            alt: None,
            level: None,
            selector: Some(format!("#n{uid}")),
            bbox: None,
            children,
        }
    }

    fn tree(nodes: Vec<SnapshotNode>) -> SnapshotTree {
        let count = nodes.len();
        SnapshotTree {
            url: "https://example.test/".to_string(),
            title: "Example".to_string(),
            nodes,
            node_count: count,
            truncated: false,
        }
    }

    #[test]
    fn small_text_passes_through_untouched() {
        let text = "hello world\nsecond line";
        let reduction = reduce_text(text, ObservationKind::PageText, &ReduceConfig::default());
        assert_eq!(reduction.strategy, ReduceStrategy::Passthrough);
        assert_eq!(reduction.text, text);
        assert!(!reduction.truncated);
        assert_eq!(reduction.saved_tokens(), 0);
    }

    #[test]
    fn zero_budget_disables_reduction() {
        let text = "error: boom\n".repeat(400);
        let cfg = ReduceConfig::with_max_tokens(0);
        let reduction = reduce_text(&text, ObservationKind::ConsoleLog, &cfg);
        assert_eq!(reduction.strategy, ReduceStrategy::Passthrough);
        assert!(!reduction.truncated);
    }

    #[test]
    fn verbose_test_output_is_bounded_and_keeps_errors() {
        let mut raw = String::new();
        for index in 0..400 {
            raw.push_str(&format!("thread {index}: noise line {index}\n"));
        }
        raw.push_str("error: could not compile `lightbrowse-core`\n");
        raw.push_str("assertion failed: left == right\n");
        let cfg = ReduceConfig::with_max_tokens(256);
        let reduction = reduce_text(&raw, ObservationKind::ConsoleLog, &cfg);
        assert!(reduction.reduced_tokens <= 256, "{reduction:?}");
        assert!(reduction.reduced_tokens < reduction.original_tokens);
        assert!(reduction.truncated);
        assert!(reduction.text.contains("could not compile"));
        assert!(reduction.text.contains("assertion failed"));
    }

    #[test]
    fn single_over_long_line_is_clipped_not_dropped() {
        let raw = "x".repeat(60_000);
        let cfg = ReduceConfig::with_max_tokens(256);
        let reduction = reduce_text(&raw, ObservationKind::PageText, &cfg);
        assert!(!reduction.text.is_empty());
        assert!(reduction.reduced_tokens <= 256, "{reduction:?}");
        assert!(reduction.truncated);
    }

    #[test]
    fn many_over_long_lines_still_fill_the_budget() {
        let paragraph = "the quick brown fox jumps over the lazy dog. ".repeat(20);
        let raw = (0..40)
            .map(|index| format!("{paragraph} (block {index})"))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = ReduceConfig::with_max_tokens(512);
        let reduction = reduce_text(&raw, ObservationKind::PageText, &cfg);
        assert!(reduction.reduced_tokens <= 512, "{reduction:?}");
        assert!(
            reduction.reduced_tokens >= 256,
            "budget under-used: {reduction:?}"
        );
        assert!(!reduction.text.is_empty());
    }

    #[test]
    fn signal_free_text_falls_back_to_head_tail() {
        let mut raw = String::new();
        for index in 0..500 {
            raw.push_str(&format!("lorem ipsum dolor sit amet {index}\n"));
        }
        let cfg = ReduceConfig::with_max_tokens(320);
        let reduction = reduce_text(&raw, ObservationKind::PageText, &cfg);
        assert_eq!(reduction.strategy, ReduceStrategy::HeadTail);
        assert!(reduction.text.contains(PRUNE_MARKER));
        assert!(reduction.reduced_tokens <= 320);
        assert!(reduction.text.starts_with("lorem ipsum dolor sit amet 0"));
        assert!(reduction.text.contains("lorem ipsum dolor sit amet 499"));
    }

    #[test]
    fn neighbours_of_signal_lines_are_preserved() {
        let mut raw = String::new();
        raw.push_str("step one done\n");
        raw.push_str("step two done\n");
        raw.push_str("error: connection refused\n");
        raw.push_str("retrying in 5s\n");
        for index in 0..300 {
            raw.push_str(&format!("padding {index}\n"));
        }
        let cfg = ReduceConfig::with_max_tokens(256);
        let reduction = reduce_text(&raw, ObservationKind::ConsoleLog, &cfg);
        assert!(reduction.text.contains("connection refused"));
        assert!(reduction.text.contains("retrying in 5s"));
    }

    #[test]
    fn reduction_id_is_content_addressed_and_stable() {
        let cfg = ReduceConfig::with_max_tokens(256);
        let raw = "error: boom\n".repeat(200);
        let first = reduce_text(&raw, ObservationKind::ConsoleLog, &cfg);
        let second = reduce_text(&raw, ObservationKind::ConsoleLog, &cfg);
        assert_eq!(first.id, second.id);
        assert!(first.id.starts_with("obs_"));
        assert_eq!(first.id.len(), 4 + 16);
        let other = reduce_text("error: different\n", ObservationKind::ConsoleLog, &cfg);
        assert_ne!(first.id, other.id);
    }

    #[test]
    fn marker_reports_savings_and_strategy() {
        let raw = "error: boom\n".repeat(200);
        let reduction = reduce_text(
            &raw,
            ObservationKind::ConsoleLog,
            &ReduceConfig::with_max_tokens(256),
        );
        let marker = reduction.marker();
        assert!(marker.contains("ranked"));
        assert!(marker.contains(&reduction.id));
    }

    #[test]
    fn large_tree_is_pruned_but_keeps_ancestors_of_interactive_nodes() {
        let mut rows = Vec::new();
        for index in 0..300 {
            rows.push(node(
                100 + index,
                "text",
                &format!("row {index} some filler text here"),
                vec![],
            ));
        }
        let submit = node(999, "button", "Submit", vec![]);
        let form = node(900, "form", "Checkout", vec![submit]);
        let main = node(10, "main", "", vec![form]);
        let mut roots = vec![main];
        roots.extend(rows);

        let mut snapshot = tree(roots);
        let cfg = ReduceConfig::with_max_tokens(256);
        let stats = prune_snapshot(&mut snapshot, &cfg);

        assert_eq!(stats.strategy, ReduceStrategy::TreeRanked);
        assert!(stats.removed_nodes > 0);
        assert!(stats.reduced_tokens < stats.original_tokens);
        assert!(
            stats.reduced_tokens * 10 <= stats.original_tokens * 7,
            "expected at least 30% fewer tokens: {stats:?}"
        );
        assert!(snapshot.truncated);
        assert_eq!(snapshot.node_count, stats.kept_nodes);

        // The button survived, and so did its form/main ancestors.
        let kept = flatten(&snapshot.nodes);
        assert!(kept
            .iter()
            .any(|(role, text)| role == "button" && text == "Submit"));
        assert!(kept.iter().any(|(_, text)| text == "Checkout"));
        let selector = snapshot.nodes[0].children[0].children[0].selector.clone();
        assert_eq!(selector.as_deref(), Some("#n999"));
    }

    #[test]
    fn signal_free_tree_falls_back_to_document_order() {
        let roots: Vec<SnapshotNode> = (0..300)
            .map(|index| node(index as u64, "text", &format!("filler {index}"), vec![]))
            .collect();
        let mut snapshot = tree(roots);
        let cfg = ReduceConfig::with_max_tokens(256);
        let stats = prune_snapshot(&mut snapshot, &cfg);
        assert_eq!(stats.strategy, ReduceStrategy::TreeFirstN);
        assert!(stats.kept_nodes > 0);
        assert!(snapshot.nodes.len() < 300);
        assert_eq!(snapshot.nodes[0].uid, 0);
    }

    #[test]
    fn small_tree_is_untouched() {
        let mut snapshot = tree(vec![node(1, "button", "Go", vec![])]);
        let stats = prune_snapshot(&mut snapshot, &ReduceConfig::with_max_tokens(256));
        assert_eq!(stats.strategy, ReduceStrategy::Passthrough);
        assert_eq!(stats.removed_nodes, 0);
        assert_eq!(snapshot.nodes.len(), 1);
    }

    fn flatten(nodes: &[SnapshotNode]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for node in nodes {
            out.push((node.role.clone(), node.text.clone()));
            out.extend(flatten(&node.children));
        }
        out
    }

    #[test]
    fn classify_maps_tools_to_kinds() {
        assert_eq!(
            ObservationKind::classify("snapshot", ""),
            ObservationKind::BrowserTree
        );
        assert_eq!(
            ObservationKind::classify("visual_snapshot", ""),
            ObservationKind::BrowserTree
        );
        assert_eq!(
            ObservationKind::classify("console", "read"),
            ObservationKind::ConsoleLog
        );
        assert_eq!(
            ObservationKind::classify("network/capture", ""),
            ObservationKind::NetworkTrace
        );
        assert_eq!(
            ObservationKind::classify("extract", "text"),
            ObservationKind::PageText
        );
        assert_eq!(
            ObservationKind::classify("evaluate", ""),
            ObservationKind::Evaluate
        );
        assert_eq!(
            ObservationKind::classify("vault/get", ""),
            ObservationKind::Generic
        );
    }

    #[test]
    fn estimate_matches_characters_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_eq!(fingerprint(b"abc").len(), 16);
    }

    #[test]
    fn bbox_is_not_required_for_scoring() {
        let node = SnapshotNode {
            bbox: Some(Bbox {
                x: 1.0,
                y: 2.0,
                w: 3.0,
                h: 4.0,
            }),
            ..node(1, "button", "Submit", vec![])
        };
        let (score, interactive) = score_node(&node, 1);
        assert!(interactive);
        assert!(score >= SIGNAL_SCORE);
    }
}
