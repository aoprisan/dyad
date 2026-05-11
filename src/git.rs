//! Phase 9 — git gutter + diff view.
//!
//! Shells out to `git diff HEAD --no-color -- <path>` and parses the
//! unified diff into a per-line status table the renderer can plant
//! in the gutter. We also expose the raw diff text so the MCP layer's
//! `git.diff` tool can hand a patch directly to an agent.
//!
//! DESIGN.md was explicit about shelling out for git ops in early
//! phases ("shell out to `git` initially; consider `git2` later"), so
//! that's what this does. Anywhere git isn't available — not a repo,
//! file not tracked, binary missing — the calls return `Err`; the
//! caller falls back to "no gutter markers" cleanly.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStatus {
    Added,
    Modified,
    DeletedAbove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineChange {
    /// Zero-indexed line number in the working-tree version of the file.
    pub line: usize,
    pub status: LineStatus,
}

/// Run `git diff HEAD --no-color -- <path>` and parse the result.
/// Returns `Err` when git can't run or the file isn't in a repo. An
/// *empty* diff (file matches HEAD) is `Ok(vec![])`.
pub fn diff_against_head(path: &Path) -> Result<Vec<LineChange>> {
    let text = diff_text(path)?;
    Ok(parse_diff(&text))
}

/// Raw `git diff HEAD --no-color -- <path>` output. Used by the MCP
/// `git.diff` tool — agents see the same patch a human would.
pub fn diff_text(path: &Path) -> Result<String> {
    let workdir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let output = Command::new("git")
        .arg("-C")
        .arg(&workdir)
        .arg("diff")
        .arg("HEAD")
        .arg("--no-color")
        .arg("--")
        .arg(path)
        .output()
        .context("spawning `git diff` (is git installed?)")?;
    if !output.status.success() {
        // Most common case: not a repo, or file isn't tracked.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("git diff failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a unified diff into per-line statuses for the *new* side.
///
/// Heuristic for distinguishing Added vs Modified: when `-` lines are
/// immediately followed by `+` lines in the same hunk, pair them up —
/// each paired `+` is Modified, extras are Added. Trailing unmatched
/// `-` (a pure deletion) is recorded as a `DeletedAbove` marker on
/// the next context line.
pub fn parse_diff(diff_text: &str) -> Vec<LineChange> {
    let mut out = Vec::new();
    let mut new_line: usize = 0;
    let mut pending_deletes: usize = 0;

    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(new_start) = parse_hunk_new_start(rest) {
                new_line = new_start.saturating_sub(1); // 0-indexed
            }
            pending_deletes = 0;
            continue;
        }
        if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("\\ ")
        {
            continue;
        }
        match line.as_bytes().first().copied() {
            Some(b'+') => {
                let status = if pending_deletes > 0 {
                    pending_deletes -= 1;
                    LineStatus::Modified
                } else {
                    LineStatus::Added
                };
                out.push(LineChange {
                    line: new_line,
                    status,
                });
                new_line += 1;
            }
            Some(b'-') => {
                pending_deletes += 1;
            }
            _ => {
                // Context (' ' prefix) or anything else closes the
                // pending-deletes window. Any leftover deletions in
                // this hunk get attributed to the current line.
                if pending_deletes > 0 {
                    out.push(LineChange {
                        line: new_line,
                        status: LineStatus::DeletedAbove,
                    });
                    pending_deletes = 0;
                }
                new_line += 1;
            }
        }
    }
    out
}

/// Pull `<n>` out of `@@ -a,b +n,m @@ optional-section-name`.
fn parse_hunk_new_start(after_at_at: &str) -> Option<usize> {
    let plus = after_at_at.find('+')?;
    let head = &after_at_at[plus + 1..];
    let end = head
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(head.len());
    head[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_handles_pure_insertion() {
        let diff = "\
@@ -1,2 +1,4 @@
 unchanged_a
+new_a
+new_b
 unchanged_b
";
        let changes = parse_diff(diff);
        assert_eq!(
            changes,
            vec![
                LineChange { line: 1, status: LineStatus::Added },
                LineChange { line: 2, status: LineStatus::Added },
            ]
        );
    }

    #[test]
    fn parse_diff_pairs_minuses_with_pluses_as_modifications() {
        let diff = "\
@@ -1,3 +1,3 @@
 unchanged_a
-old_x
+new_x
 unchanged_b
";
        let changes = parse_diff(diff);
        assert_eq!(
            changes,
            vec![LineChange { line: 1, status: LineStatus::Modified }]
        );
    }

    #[test]
    fn parse_diff_records_pure_deletion_as_deleted_above() {
        let diff = "\
@@ -1,3 +1,2 @@
 unchanged_a
-deleted
 unchanged_b
";
        let changes = parse_diff(diff);
        // `unchanged_b` is at new line 1 (0-indexed) and has a deletion
        // immediately above it.
        assert_eq!(
            changes,
            vec![LineChange { line: 1, status: LineStatus::DeletedAbove }]
        );
    }

    #[test]
    fn parse_diff_mixes_modified_and_added_in_one_hunk() {
        let diff = "\
@@ -1,3 +1,5 @@
 unchanged
-old1
-old2
+new1
+new2
+new3
 ctx
";
        let changes = parse_diff(diff);
        // new_line=1 starts after the first context. Two old lines pair
        // with the first two `+`s as Modified; the third `+` is Added.
        assert_eq!(
            changes,
            vec![
                LineChange { line: 1, status: LineStatus::Modified },
                LineChange { line: 2, status: LineStatus::Modified },
                LineChange { line: 3, status: LineStatus::Added },
            ]
        );
    }

    #[test]
    fn parse_diff_ignores_diff_envelope_lines() {
        let diff = "\
diff --git a/foo b/foo
index 1111111..2222222 100644
--- a/foo
+++ b/foo
@@ -1 +1,2 @@
 unchanged
+added
";
        let changes = parse_diff(diff);
        assert_eq!(
            changes,
            vec![LineChange { line: 1, status: LineStatus::Added }]
        );
    }

    #[test]
    fn parse_hunk_new_start_handles_single_line_form() {
        // Single-line ranges in unified diff omit the comma+count.
        assert_eq!(parse_hunk_new_start("-1 +1 @@"), Some(1));
        assert_eq!(parse_hunk_new_start("-1,5 +1,5 @@ fn foo()"), Some(1));
    }
}
