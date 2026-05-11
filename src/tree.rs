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

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique temp dir with the given subtree structure.
    fn make_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dyad_tree_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested").join("c.txt"), "c").unwrap();
        // Hidden file should not show up.
        std::fs::write(root.join(".hidden"), "h").unwrap();
        root
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn new_lists_top_level_entries_with_parent_link() {
        let root = make_tree("new");
        let tree = FileTree::new(root.clone());
        // Sorted by directory-first then name, prefixed by the synthetic ".." row.
        assert!(tree.entries.first().unwrap().is_parent_link);
        let visible: Vec<&str> = tree
            .entries
            .iter()
            .filter(|e| !e.is_parent_link)
            .map(|e| e.name.as_str())
            .collect();
        // Hidden file omitted; "nested" before files since dirs go first.
        assert_eq!(visible, vec!["nested", "a.txt", "b.txt"]);
        cleanup(&root);
    }

    #[test]
    fn move_up_and_down_clamp_at_boundaries() {
        let root = make_tree("nav");
        let mut tree = FileTree::new(root.clone());
        let last = tree.entries.len() - 1;

        tree.move_up(); // already at 0, no-op
        assert_eq!(tree.cursor, 0);

        for _ in 0..tree.entries.len() + 5 {
            tree.move_down();
        }
        assert_eq!(tree.cursor, last);

        for _ in 0..tree.entries.len() + 5 {
            tree.move_up();
        }
        assert_eq!(tree.cursor, 0);
        cleanup(&root);
    }

    #[test]
    fn activate_on_file_yields_open() {
        let root = make_tree("activate_file");
        let mut tree = FileTree::new(root.clone());
        // Move to "a.txt" — find its index.
        let idx = tree
            .entries
            .iter()
            .position(|e| !e.is_parent_link && e.name == "a.txt")
            .unwrap();
        tree.cursor = idx;
        match tree.activate() {
            Activation::Open(path) => {
                assert_eq!(path.file_name().unwrap(), "a.txt");
            }
            _ => panic!("expected Activation::Open"),
        }
        cleanup(&root);
    }

    #[test]
    fn activate_on_directory_toggles_expansion() {
        let root = make_tree("activate_dir");
        let mut tree = FileTree::new(root.clone());
        let idx = tree
            .entries
            .iter()
            .position(|e| !e.is_parent_link && e.is_dir && e.name == "nested")
            .unwrap();
        tree.cursor = idx;
        let before = tree.entries.len();
        // First activate expands.
        match tree.activate() {
            Activation::None => {}
            _ => panic!("expected Activation::None when toggling expand"),
        }
        assert!(tree.entries[idx].expanded);
        assert!(tree.entries.len() > before);
        // Second activate collapses again.
        tree.activate();
        assert!(!tree.entries[idx].expanded);
        assert_eq!(tree.entries.len(), before);
        cleanup(&root);
    }

    #[test]
    fn activate_on_parent_link_returns_ascend() {
        let root = make_tree("ascend");
        let mut tree = FileTree::new(root.clone());
        tree.cursor = 0; // synthetic ".."
        match tree.activate() {
            Activation::Ascend => {}
            _ => panic!("expected Activation::Ascend"),
        }
        cleanup(&root);
    }

    #[test]
    fn reveal_returns_false_for_path_outside_root() {
        let root = make_tree("outside");
        let other = std::env::temp_dir().join("dyad_tree_outside_other.txt");
        let _ = std::fs::write(&other, "x");
        let mut tree = FileTree::new(root.clone());
        assert!(!tree.reveal(&other));
        let _ = std::fs::remove_file(&other);
        cleanup(&root);
    }

    #[test]
    fn reveal_expands_path_and_positions_cursor() {
        let root = make_tree("reveal");
        let mut tree = FileTree::new(root.clone());
        let nested_file = root.join("nested").join("c.txt");
        assert!(tree.reveal(&nested_file));
        // Cursor should be on the c.txt entry.
        let entry = &tree.entries[tree.cursor];
        assert_eq!(entry.name, "c.txt");
        cleanup(&root);
    }

    #[test]
    fn scroll_into_view_brings_cursor_into_viewport() {
        let root = make_tree("scroll");
        let mut tree = FileTree::new(root.clone());
        tree.cursor = tree.entries.len() - 1;
        tree.scroll_into_view(2);
        assert!(tree.top >= tree.cursor + 1 - 2);
        // Reset to 0; scroll_into_view should walk top back down to 0.
        tree.cursor = 0;
        tree.scroll_into_view(2);
        assert_eq!(tree.top, 0);
        cleanup(&root);
    }

    #[test]
    fn scroll_into_view_zero_viewport_is_noop() {
        let root = make_tree("noop_scroll");
        let mut tree = FileTree::new(root.clone());
        let original_top = tree.top;
        tree.cursor = 99;
        tree.scroll_into_view(0);
        assert_eq!(tree.top, original_top);
        cleanup(&root);
    }

    #[test]
    fn ascend_resets_to_parent_directory() {
        let root = make_tree("ascend_root");
        let mut tree = FileTree::new(root.clone());
        let parent_dir = tree.root.parent().unwrap().to_path_buf();
        tree.ascend();
        assert_eq!(tree.root, parent_dir);
        // After ascending, the new root list is re-derived (cursor at 0).
        assert_eq!(tree.cursor, 0);
        cleanup(&root);
    }

    #[test]
    fn project_root_for_finds_git_dir() {
        let parent = std::env::temp_dir().join(format!(
            "dyad_tree_proj_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(parent.join(".git")).unwrap();
        std::fs::create_dir_all(parent.join("src")).unwrap();
        let canonical_parent = parent.canonicalize().unwrap();
        let resolved = project_root_for(&parent.join("src"));
        assert_eq!(resolved, canonical_parent);
        let _ = std::fs::remove_dir_all(&parent);
    }
}
