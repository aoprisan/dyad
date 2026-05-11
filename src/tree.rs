//! Left-sidebar file tree.
//!
//! Lazy: each directory is read from disk only when the user expands it,
//! and the children are spliced into a single flat `entries` vector so
//! rendering is a straight slice over the visible viewport (no recursive
//! walk per frame).
//!
//! Hidden files (anything starting with `.`) are filtered out; revealing
//! them is a future toggle, not a Phase 1 concern.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// True for the synthetic `..` row that sits at the top of the
    /// entry list when the root has a parent. Activating it re-roots
    /// the tree one level up instead of toggling expansion.
    pub is_parent_link: bool,
}

pub enum Activation {
    /// Cursor sat on a non-actionable row, or activate toggled an
    /// expand in place — caller has nothing to do.
    None,
    Open(PathBuf),
    /// Cursor sat on the `..` row — caller should re-root the tree to
    /// the parent directory.
    Ascend,
}

pub struct FileTree {
    pub root: PathBuf,
    /// Flat ordered list of currently-visible entries. When a directory
    /// is expanded, its children are inserted right after the directory
    /// entry; collapsing pops them back out.
    pub entries: Vec<TreeEntry>,
    pub cursor: usize,
    pub top: usize,
    /// True while the sidebar is rendered and stealing key input.
    /// Doubles as the visibility flag — there is no "visible but not
    /// focused" state in Phase 1.
    pub focused: bool,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        // Always store an absolute root. Relative inputs like `.` would
        // otherwise give an empty `parent()` and break ascend, which
        // showed up as a blank tree after pressing Enter on `..`.
        let root = absolutize(&root);
        let entries = entries_for(&root);
        Self {
            root,
            entries,
            cursor: 0,
            top: 0,
            focused: false,
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
        }
    }

    /// Enter on the selected entry. Directories toggle expansion in
    /// place; files return `Open(path)`; the synthetic `..` row asks
    /// the caller to re-root via `ascend`.
    pub fn activate(&mut self) -> Activation {
        let Some(entry) = self.entries.get(self.cursor) else {
            return Activation::None;
        };
        if entry.is_parent_link {
            return Activation::Ascend;
        }
        if !entry.is_dir {
            return Activation::Open(entry.path.clone());
        }
        if entry.expanded {
            self.collapse_at(self.cursor);
        } else {
            self.expand_at(self.cursor);
        }
        Activation::None
    }

    /// Expand directories down to `target` and place the cursor on
    /// the corresponding entry. Returns `true` when the file was
    /// found; `false` when it's outside the tree root, hidden by the
    /// `.`-filter, or otherwise not listed. Canonicalizes `target`
    /// first so callers can pass either absolute or relative paths.
    pub fn reveal(&mut self, target: &Path) -> bool {
        let abs = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        let Ok(rel) = abs.strip_prefix(&self.root) else {
            return false;
        };
        let mut walking = self.root.clone();
        let mut components: Vec<_> = rel.components().collect();
        let Some(last) = components.pop() else {
            return false;
        };
        for comp in components {
            walking.push(comp);
            let Some(idx) = self
                .entries
                .iter()
                .position(|e| !e.is_parent_link && e.is_dir && e.path == walking)
            else {
                return false;
            };
            if !self.entries[idx].expanded {
                self.expand_at(idx);
            }
        }
        walking.push(last);
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| !e.is_parent_link && e.path == walking)
        {
            self.cursor = idx;
            return true;
        }
        false
    }

    /// Move the tree root one level up. Existing expansion state is
    /// discarded — the new root re-lists from disk with everything
    /// collapsed, which is the least-surprising default for "go up".
    pub fn ascend(&mut self) {
        let Some(parent) = self.root.parent() else {
            return;
        };
        self.root = parent.to_path_buf();
        self.entries = entries_for(&self.root);
        self.cursor = 0;
        self.top = 0;
    }

    fn expand_at(&mut self, idx: usize) {
        let depth = self.entries[idx].depth;
        let path = self.entries[idx].path.clone();
        let children = list_dir(&path, depth + 1);
        self.entries[idx].expanded = true;
        let mut insert_at = idx + 1;
        for child in children {
            self.entries.insert(insert_at, child);
            insert_at += 1;
        }
    }

    fn collapse_at(&mut self, idx: usize) {
        let depth = self.entries[idx].depth;
        self.entries[idx].expanded = false;
        // Drop every entry until we hit one at the same or shallower depth
        // (the next sibling, or end-of-list). Descendants' `expanded`
        // flags reset to false on the next expand — simplest and least
        // surprising.
        let j = idx + 1;
        while j < self.entries.len() && self.entries[j].depth > depth {
            self.entries.remove(j);
        }
    }

    pub fn scroll_into_view(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + viewport {
            self.top = self.cursor + 1 - viewport;
        }
    }
}

fn list_dir(path: &Path, depth: usize) -> Vec<TreeEntry> {
    let Ok(reader) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut entries: Vec<TreeEntry> = reader
        .filter_map(|r| r.ok())
        .filter_map(|ent| {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let path = ent.path();
            let is_dir = path.is_dir();
            Some(TreeEntry {
                path,
                name,
                depth,
                is_dir,
                expanded: false,
                is_parent_link: false,
            })
        })
        .collect();
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    entries
}

/// Build the top-level entry list for `root`: a synthetic `..` row at
/// the top when the root has a parent, followed by the directory's
/// children. Pulled into its own helper so `new` and `ascend` share it.
fn entries_for(root: &Path) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    if let Some(parent) = root.parent() {
        entries.push(TreeEntry {
            path: parent.to_path_buf(),
            name: "..".into(),
            depth: 0,
            is_dir: true,
            expanded: false,
            is_parent_link: true,
        });
    }
    entries.extend(list_dir(root, 0));
    entries
}

/// Walk up from `start` looking for a `.git` directory. Falls back to
/// `start` itself when no marker is found, so the tree still works
/// outside a repo. The result is always absolute — passing in a
/// relative `start` like `.` is fine.
pub fn project_root_for(start: &Path) -> PathBuf {
    let abs = absolutize(start);
    let mut cur = abs.clone();
    loop {
        if cur.join(".git").exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return abs,
        }
    }
}

/// Best-effort absolute form of `path`. Prefers `canonicalize` (which
/// resolves symlinks and `..`); falls back to joining onto the current
/// working directory for paths that don't exist on disk. Mirrors the
/// `lsp::absolutize` helper — kept private here to avoid leaking an
/// implementation detail of one module into another.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(abs) = path.canonicalize() {
        return abs;
    }
    if path.is_absolute() {
        return PathBuf::from(path);
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => PathBuf::from(path),
    }
}
