//! Line-level diff between two texts (LCS) — for "what changed on this page".

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Diff two texts line-by-line using classic DP-LCS. Returns lines in order
/// with kind markers; `Same` lines are context (kept so the reader can
/// anchor added/removed content).
pub fn diff_texts(a: &str, b: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let (n, m) = (a.len(), b.len());

    // DP table: lcs[i][j] = LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine {
                kind: DiffKind::Same,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Added,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            kind: DiffKind::Removed,
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            kind: DiffKind::Added,
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

/// Counts of each kind in a diff result.
pub fn diff_stats(diff: &[DiffLine]) -> (usize, usize, usize) {
    let (mut same, mut added, mut removed) = (0, 0, 0);
    for l in diff {
        match l.kind {
            DiffKind::Same => same += 1,
            DiffKind::Added => added += 1,
            DiffKind::Removed => removed += 1,
        }
    }
    (same, added, removed)
}

/// Keep only lines within `context` of a change (compress long unchanged runs).
pub fn compact(diff: Vec<DiffLine>, context: usize) -> Vec<DiffLine> {
    let mut out: VecDeque<DiffLine> = VecDeque::new();
    let mut unchanged_run = 0usize;
    for line in diff {
        if line.kind != DiffKind::Same {
            // Emit any pending context (context lines before this change).
            let emit = unchanged_run.min(context);
            out.push_back(DiffLine {
                kind: DiffKind::Same,
                text: format!("⋯ {emit} unchanged ⋯"),
            });
            out.push_back(line);
            unchanged_run = 0;
        } else {
            unchanged_run += 1;
            if unchanged_run <= context {
                out.push_back(line);
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_basic() {
        let a = "line1\nline2\nline3\nline4";
        let b = "line1\nlineX\nline3\nline4\nline5";
        let d = diff_texts(a, b);
        let (_, added, removed) = diff_stats(&d);
        assert_eq!(added, 2); // lineX + line5
        assert_eq!(removed, 1); // line2
        let kinds: Vec<DiffKind> = d.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&DiffKind::Removed));
        assert!(kinds.contains(&DiffKind::Added));
        assert!(kinds.contains(&DiffKind::Same));
    }

    #[test]
    fn diff_identical() {
        let d = diff_texts("a\nb\nc", "a\nb\nc");
        let (same, added, removed) = diff_stats(&d);
        assert_eq!(same, 3);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }
}
