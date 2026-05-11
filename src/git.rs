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

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStatus {
    Added,
    Modified,
    DeletedAbove,
}

/// One row of `git status --porcelain=v1`. `staged` is the X column
/// (index vs HEAD), `unstaged` is Y (working tree vs index). Both
/// chars use the porcelain alphabet: space = unchanged, M/A/D/R/C/U
/// for the usual states, `?` for untracked.
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub staged: char,
    pub unstaged: char,
}

/// One entry from `git log --pretty=format:...`. SHA in full and
/// short form so the UI can render the short version while
/// `show` keeps using the unambiguous full SHA.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
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

/// Resolve the repo root for any path inside a repo via
/// `git -C <dir> rev-parse --show-toplevel`. Returns `Err` when
/// `start` isn't in a repo (or git isn't installed) so the caller
/// can surface a friendly error to the user.
pub fn repo_root_for(start: &Path) -> Result<PathBuf> {
    let workdir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(&workdir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .context("spawning `git rev-parse`")?;
    if !out.status.success() {
        anyhow::bail!("not a git repository");
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(s))
}

/// Parsed `git status --porcelain=v1 --no-renames` output for
/// `repo_root`. We disable renames so every entry holds exactly one
/// path — the v1 rename form (`XY old -> new`) would otherwise need a
/// second parsing path for a feature we don't surface yet.
pub fn status_at(repo_root: &Path) -> Result<Vec<StatusEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("--no-renames")
        .output()
        .context("spawning `git status`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git status failed: {stderr}");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        // Porcelain v1: bytes 0,1 are status chars, byte 2 is a space,
        // remainder is the path. Skip malformed rows defensively.
        if line.len() < 4 {
            continue;
        }
        let bytes = line.as_bytes();
        let staged = bytes[0] as char;
        let unstaged = bytes[1] as char;
        let path = PathBuf::from(&line[3..]);
        entries.push(StatusEntry {
            path,
            staged,
            unstaged,
        });
    }
    Ok(entries)
}

/// Combined HEAD diff for a single `rel_path` inside `repo_root`.
/// Shows both staged and unstaged changes in one patch. For untracked
/// files (which `git diff HEAD` ignores) we synthesize a "+"-only
/// patch by reading the file directly — the renderer doesn't need to
/// know the difference.
pub fn diff_for_path(repo_root: &Path, rel_path: &Path, untracked: bool) -> Result<String> {
    if untracked {
        let full = repo_root.join(rel_path);
        let content = std::fs::read_to_string(&full)
            .unwrap_or_else(|_| String::from("[binary or unreadable]"));
        let mut s = String::new();
        s.push_str(&format!("+++ b/{}\n", rel_path.display()));
        s.push_str("@@ untracked file @@\n");
        for line in content.lines() {
            s.push('+');
            s.push_str(line);
            s.push('\n');
        }
        return Ok(s);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("diff")
        .arg("HEAD")
        .arg("--no-color")
        .arg("--")
        .arg(rel_path)
        .output()
        .context("spawning `git diff`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git diff failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Stage `rel_path` (any state) via `git add -- <path>`. Untracked
/// files, modifications, and deletions all work — `git add` treats
/// the new index entry as "match the working tree" for each.
pub fn stage(repo_root: &Path, rel_path: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("add")
        .arg("--")
        .arg(rel_path)
        .output()
        .context("spawning `git add`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git add failed: {stderr}");
    }
    Ok(())
}

/// Unstage `rel_path` via `git restore --staged --`. Leaves the
/// working tree untouched.
pub fn unstage(repo_root: &Path, rel_path: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("restore")
        .arg("--staged")
        .arg("--")
        .arg(rel_path)
        .output()
        .context("spawning `git restore --staged`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git restore --staged failed: {stderr}");
    }
    Ok(())
}

/// Commit currently-staged changes with `message`. Errors include
/// the git stderr verbatim so "nothing to commit" or pre-commit hook
/// rejections reach the user as-is.
pub fn commit(repo_root: &Path, message: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("commit")
        .arg("-m")
        .arg(message)
        .output()
        .context("spawning `git commit`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // `git commit` writes "nothing to commit" to stdout, not
        // stderr — prefer whichever has content so the user always
        // gets a useful message.
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        anyhow::bail!("git commit failed: {detail}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Most recent `limit` commits. Uses `0x1f` (unit separator) as a
/// field delimiter so author names with spaces parse cleanly.
pub fn log(repo_root: &Path, limit: usize) -> Result<Vec<LogEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("log")
        .arg(format!("-n{limit}"))
        .arg("--pretty=format:%H%x1f%h%x1f%an%x1f%ad%x1f%s")
        .arg("--date=short")
        .arg("--no-color")
        .output()
        .context("spawning `git log`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git log failed: {stderr}");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\u{1f}').collect();
        if parts.len() < 5 {
            continue;
        }
        entries.push(LogEntry {
            sha: parts[0].to_string(),
            short_sha: parts[1].to_string(),
            author: parts[2].to_string(),
            date: parts[3].to_string(),
            subject: parts[4].to_string(),
        });
    }
    Ok(entries)
}

/// Full `git show` for a commit — used as the right-pane detail in
/// the history view.
pub fn show_commit(repo_root: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show")
        .arg("--no-color")
        .arg(sha)
        .output()
        .context("spawning `git show`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git show failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
