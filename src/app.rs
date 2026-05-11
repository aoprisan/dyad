use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use crate::action::Action;
use crate::buffer::Buffer;
use crate::git::{self, LineStatus};
use crate::input;
use crate::lsp::{self, LspClient};
use crate::syntax::Syntax;
use crate::tree::{self, Activation, FileTree};
use crate::tx::TxManager;
use crate::ui;
use crate::view::View;

pub struct App {
    pub buffer: Buffer,
    pub view: View,
    pub syntax: Option<Syntax>,
    pub running: bool,
    pub status: String,
    /// Per-line git status against HEAD, keyed by 0-indexed line in the
    /// working-tree file. Refreshed on save and at startup; empty when
    /// the file isn't in a git repo (or git isn't installed).
    pub git_status: HashMap<usize, LineStatus>,
    /// `rust-analyzer` client when the seed file is Rust and the binary
    /// is on PATH. `None` everywhere else (including non-Rust files and
    /// failed spawns) — every consumer must tolerate absence.
    pub lsp: Option<LspClient>,
    pub lsp_uri: Option<String>,
    /// True when LSP spawn was attempted (i.e., the file looked like Rust),
    /// regardless of outcome. Combined with `lsp.is_some()` this lets the
    /// UI render an Active / Failed / Hidden indicator without re-checking
    /// the file extension.
    pub lsp_attempted: bool,
    lsp_version: i32,
    /// Locations we navigated *from* via cross-file go-to-definition.
    /// Ctrl-O pops this and re-opens the previous file at its prior
    /// cursor — vim's `Ctrl-O` jumplist, single-direction.
    nav_stack: Vec<NavPoint>,
    tx_manager: TxManager,
    quit_pending: bool,
    /// Left-sidebar file tree. Always present so Ctrl-T can show it
    /// without the App needing to know about lazy initialization.
    /// `tree.focused` doubles as the visibility flag.
    pub tree: FileTree,
}

struct NavPoint {
    path: PathBuf,
    line: usize,
    col: usize,
}

impl App {
    pub fn new(path: PathBuf) -> Result<Self> {
        if path.is_dir() {
            return Self::new_for_dir(path);
        }
        let mut buffer = Buffer::open(path)?;
        let mut syntax = Syntax::for_path(buffer.path());
        if let Some(syn) = syntax.as_mut() {
            syn.refresh(&mut buffer);
        }
        let git_status = compute_git_status(buffer.path());
        let (lsp, lsp_uri, lsp_attempted) = spawn_lsp(&buffer);
        let tree_root = buffer
            .path()
            .and_then(|p| p.parent())
            .map(tree::project_root_for)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Ok(Self {
            buffer,
            view: View::new(),
            syntax,
            running: true,
            status: String::new(),
            git_status,
            lsp,
            lsp_uri,
            lsp_attempted,
            lsp_version: 0,
            nav_stack: Vec::new(),
            tx_manager: TxManager::new(),
            quit_pending: false,
            tree: FileTree::new(tree_root),
        })
    }

    /// Launch with `dyad <dir>`: no buffer is loaded yet; the tree is
    /// visible and focused so the user can pick a file from disk.
    fn new_for_dir(dir: PathBuf) -> Result<Self> {
        let mut tree = FileTree::new(tree::project_root_for(&dir));
        tree.focused = true;
        Ok(Self {
            buffer: Buffer::scratch(),
            view: View::new(),
            syntax: None,
            running: true,
            status: String::new(),
            git_status: HashMap::new(),
            lsp: None,
            lsp_uri: None,
            lsp_attempted: false,
            lsp_version: 0,
            nav_stack: Vec::new(),
            tx_manager: TxManager::new(),
            quit_pending: false,
            tree,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while self.running {
            terminal.draw(|frame| ui::render(frame, self))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn handle_events(&mut self) -> Result<()> {
        if !event::poll(Duration::from_millis(250))? {
            return Ok(());
        }
        match event::read()? {
            Event::Key(key) => {
                if let Some(action) = input::map(key) {
                    self.apply(action)?;
                }
            }
            Event::Resize(_, _) => {
                // The next draw call will re-layout against the new size.
            }
            _ => {}
        }
        Ok(())
    }

    fn apply(&mut self, action: Action) -> Result<()> {
        // Any non-Quit input clears the pending-quit confirmation.
        if !matches!(action, Action::Quit) {
            self.quit_pending = false;
        }

        // Tree-mode routing. Toggle / Escape are mode-independent;
        // everything else either drives the tree or is swallowed
        // while the sidebar holds focus, with Save/Quit explicitly
        // passed through so the user is never locked in.
        match action {
            Action::ToggleTree => {
                self.tree.focused = !self.tree.focused;
                return Ok(());
            }
            Action::Escape => {
                if self.tree.focused {
                    self.tree.focused = false;
                }
                return Ok(());
            }
            Action::Save | Action::Quit => {}
            _ if self.tree.focused => {
                match action {
                    Action::MoveUp => self.tree.move_up(),
                    Action::MoveDown => self.tree.move_down(),
                    Action::Insert('\n') => self.tree_activate()?,
                    _ => {}
                }
                return Ok(());
            }
            _ => {}
        }

        // Open an auto-tx for buffer-mutating actions so every edit lands
        // in the flat history with a human-readable intent (DESIGN.md
        // §Transactions & intent). Movement, save, and quit aren't edits
        // and don't get wrapped.
        let tx_id = action_intent(&action)
            .map(|intent| self.tx_manager.begin(intent, None, &self.buffer));
        let pre_version = tx_id.and_then(|id| self.tx_manager.pre_version(id));

        match action {
            Action::Insert(c) => {
                let idx = self.view.char_idx(&self.buffer);
                self.buffer.insert_char(idx, c);
                let mut tmp = [0u8; 4];
                let s: &str = c.encode_utf8(&mut tmp);
                self.view.after_insert(&self.buffer, s);
            }
            Action::DeletePrev => {
                let end = self.view.char_idx(&self.buffer);
                if end > 0 {
                    let start = end - 1;
                    self.buffer.delete_range(start..end);
                    self.view.after_delete_prev(&self.buffer);
                }
            }
            Action::DeleteNext => {
                let start = self.view.char_idx(&self.buffer);
                if start < self.buffer.len_chars() {
                    self.buffer.delete_range(start..start + 1);
                    // Cursor position stays the same (chars shift left).
                }
            }
            Action::MoveLeft => self.view.move_left(&self.buffer),
            Action::MoveRight => self.view.move_right(&self.buffer),
            Action::MoveUp => self.view.move_up(&self.buffer),
            Action::MoveDown => self.view.move_down(&self.buffer),
            Action::MoveWordLeft => self.view.move_word_left(&self.buffer),
            Action::MoveWordRight => self.view.move_word_right(&self.buffer),
            Action::MoveHome => self.view.move_home(),
            Action::MoveEnd => self.view.move_end(&self.buffer),
            Action::PageUp | Action::PageDown => {
                // Use the most recent terminal size; ratatui exposes it via the next draw,
                // but for paging we ask crossterm directly.
                let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
                let viewport = ui::text_viewport_rows(ratatui::layout::Rect::new(0, 0, 1, rows));
                if matches!(action, Action::PageUp) {
                    self.view.page_up(&self.buffer, viewport);
                } else {
                    self.view.page_down(&self.buffer, viewport);
                }
            }
            Action::Save => match self.buffer.save() {
                Ok(bytes) => {
                    self.status = format!("Saved {} bytes", bytes);
                    self.git_status = compute_git_status(self.buffer.path());
                }
                Err(e) => self.status = format!("Save failed: {}", e),
            },
            Action::Quit => {
                if self.buffer.is_dirty() && !self.quit_pending {
                    self.quit_pending = true;
                    self.status = "Unsaved changes — Ctrl-Q again to quit, Ctrl-S to save".into();
                } else {
                    self.running = false;
                }
            }
            Action::GoToDefinition => {
                self.status = self.go_to_definition();
            }
            Action::GoBack => {
                self.status = self.go_back();
            }
            // Handled in the tree-routing block above; listed here so the
            // compiler stays happy about exhaustiveness.
            Action::ToggleTree | Action::Escape => {}
        }

        // Close out the auto-tx. If the mutation didn't actually change
        // the rope (e.g., DeletePrev at the start of the buffer), drop
        // it without recording a history entry — pre_version comparison
        // is the test of record because Buffer::touch bumps version on
        // every real mutation.
        if let Some(tx_id) = tx_id {
            if Some(self.buffer.version()) == pre_version {
                self.tx_manager.discard(tx_id)?;
            } else {
                self.tx_manager.commit(tx_id, &self.buffer)?;
                self.notify_lsp_changed();
            }
        }

        // Scroll-into-view after every action. We re-query the terminal height; the next draw
        // will adjust if it changes.
        let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let viewport = rows.saturating_sub(1); // minus status row
        self.view.scroll_into_view(viewport);

        if let Some(syn) = self.syntax.as_mut() {
            syn.refresh(&mut self.buffer);
        }

        Ok(())
    }

    fn notify_lsp_changed(&mut self) {
        let Some(lsp) = self.lsp.as_ref() else { return };
        let Some(uri) = self.lsp_uri.as_ref() else { return };
        self.lsp_version += 1;
        let text = self.buffer.rope().to_string();
        let _ = lsp.did_change(uri, self.lsp_version, &text);
    }

    /// Resolve `textDocument/definition` at the cursor. Returns the
    /// status-bar message — empty string means "moved cursor, no
    /// message needed."
    fn go_to_definition(&mut self) -> String {
        let (Some(lsp), Some(uri)) = (self.lsp.as_ref(), self.lsp_uri.as_ref()) else {
            return "LSP not available".into();
        };
        let line = self.view.cursor_line() as u32;
        let character = self.view.cursor_col() as u32;
        let result = lsp.definition(uri, line, character);
        match result {
            Ok(locs) if locs.is_empty() => {
                if self.lsp.as_ref().map(|c| c.is_indexing()).unwrap_or(false) {
                    "rust-analyzer still indexing — try again in a moment".into()
                } else {
                    "No definition found".into()
                }
            }
            Ok(locs) => {
                let loc = locs[0].clone();
                let target_line = loc.range.start.line as usize;
                let target_col = loc.range.start.character as usize;
                if Some(&loc.uri) == self.lsp_uri.as_ref() {
                    self.view.goto(&self.buffer, target_line, target_col);
                    return String::new();
                }
                let Some(target_path) = uri_to_path(&loc.uri) else {
                    return format!("Definition has unparseable URI {}", loc.uri);
                };
                if self.buffer.is_dirty() {
                    return "Save first — current buffer has unsaved changes".into();
                }
                self.push_nav_stack();
                match self.open_file(&target_path) {
                    Ok(()) => {
                        self.view.goto(&self.buffer, target_line, target_col);
                        // The previous status would otherwise stick around;
                        // a clean jump is its own feedback.
                        String::new()
                    }
                    Err(e) => {
                        // Roll back the stack push since we didn't actually
                        // navigate anywhere.
                        self.nav_stack.pop();
                        format!("Could not open {}: {e}", target_path.display())
                    }
                }
            }
            Err(e) => format!("Definition lookup failed: {e}"),
        }
    }

    /// Pop the navigation stack and re-open the previous file at its
    /// stored cursor. No-op (with status) when the stack is empty.
    fn go_back(&mut self) -> String {
        let Some(point) = self.nav_stack.pop() else {
            return "Nothing to go back to".into();
        };
        if self.buffer.is_dirty() {
            // Restore the stack — refusing the jump means the back-pointer
            // is still valid for the next try.
            self.nav_stack.push(point);
            return "Save first — current buffer has unsaved changes".into();
        }
        let target_line = point.line;
        let target_col = point.col;
        match self.open_file(&point.path) {
            Ok(()) => {
                self.view.goto(&self.buffer, target_line, target_col);
                String::new()
            }
            Err(e) => format!("Could not reopen {}: {e}", point.path.display()),
        }
    }

    fn push_nav_stack(&mut self) {
        if let Some(path) = self.buffer.path() {
            self.nav_stack.push(NavPoint {
                path: path.to_path_buf(),
                line: self.view.cursor_line(),
                col: self.view.cursor_col(),
            });
        }
    }

    /// Enter on the tree's selected entry. Directories toggle open/closed
    /// in place; the `..` row re-roots one level up; files load into the
    /// buffer and return focus to the editor. Refuses with a status
    /// message when the current buffer is dirty — same policy as
    /// cross-file go-to-definition.
    fn tree_activate(&mut self) -> Result<()> {
        let path = match self.tree.activate() {
            Activation::None => return Ok(()),
            Activation::Ascend => {
                self.tree.ascend();
                return Ok(());
            }
            Activation::Open(p) => p,
        };
        if self.buffer.is_dirty() {
            self.status = "Save first — current buffer has unsaved changes".into();
            return Ok(());
        }
        self.push_nav_stack();
        match self.open_file(&path) {
            Ok(()) => {
                self.tree.focused = false;
                self.status = String::new();
            }
            Err(e) => {
                self.nav_stack.pop();
                self.status = format!("Could not open {}: {e}", path.display());
            }
        }
        Ok(())
    }

    /// Swap the current buffer for one rooted at `path`. Resets the view,
    /// re-runs the tree-sitter parse, refreshes git status, and tells the
    /// existing LSP client about the new file. The LSP client itself is
    /// not respawned — rust-analyzer already knows the workspace.
    fn open_file(&mut self, path: &Path) -> Result<()> {
        let mut new_buffer = Buffer::open(path.to_path_buf())?;
        let mut new_syntax = Syntax::for_path(new_buffer.path());
        if let Some(syn) = new_syntax.as_mut() {
            syn.refresh(&mut new_buffer);
        }
        let new_git_status = compute_git_status(new_buffer.path());
        let new_uri = new_buffer.path().map(lsp::path_to_uri);
        let is_rust = new_buffer
            .path()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            == Some("rs");

        // rust-analyzer is a single workspace-wide instance. Two paths:
        //   - if it's already running, send didOpen so it picks up the
        //     new doc (it tolerates didOpen on a known doc — replaces
        //     its view);
        //   - if it isn't, this is the first Rust file we've seen, so
        //     lazy-spawn it. This is what makes `dyad <dir>` followed
        //     by picking a `.rs` file from the tree behave the same as
        //     `dyad <file.rs>` from the command line.
        if self.lsp.is_none() && is_rust {
            let (lsp, _spawned_uri, attempted) = spawn_lsp(&new_buffer);
            self.lsp = lsp;
            self.lsp_attempted = attempted;
            // spawn_rust already issued didOpen with the file's content;
            // the else-branch's didOpen call would be redundant.
        } else if let (Some(lsp), Some(uri), Some(p)) = (
            self.lsp.as_ref(),
            new_uri.as_ref(),
            new_buffer.path(),
        ) && p.extension().and_then(|e| e.to_str()) == Some("rs")
        {
            let _ = lsp.did_open(uri, "rust", &new_buffer.rope().to_string());
        }

        self.buffer = new_buffer;
        self.syntax = new_syntax;
        self.git_status = new_git_status;
        self.lsp_uri = new_uri;
        self.lsp_version = 0;
        self.view = View::new();
        Ok(())
    }
}

/// Strip the `file://` scheme and return the underlying filesystem path,
/// `None` for any URI we can't parse trivially. (Full RFC 8089 handling
/// can wait — rust-analyzer only emits plain `file://` URIs.)
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Returns `(client, uri, attempted)`. `attempted` is `true` whenever
/// the file looked like Rust, so the UI can distinguish "spawn failed"
/// (red badge) from "we didn't try" (no badge).
fn spawn_lsp(buffer: &Buffer) -> (Option<LspClient>, Option<String>, bool) {
    let Some(path) = buffer.path() else {
        return (None, None, false);
    };
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return (None, None, false);
    }
    let uri = lsp::path_to_uri(path);
    let workspace = lsp::workspace_root_for(path);
    match LspClient::spawn_rust(&workspace, &uri, &buffer.rope().to_string()) {
        Ok(client) => (Some(client), Some(uri), true),
        // Fail-graceful: the TUI runs without LSP-backed features.
        Err(_) => (None, None, true),
    }
}

fn action_intent(action: &Action) -> Option<String> {
    match action {
        Action::Insert(c) => Some(format!("insert {}", describe_char(*c))),
        Action::DeletePrev => Some("delete backward".into()),
        Action::DeleteNext => Some("delete forward".into()),
        Action::MoveLeft
        | Action::MoveRight
        | Action::MoveUp
        | Action::MoveDown
        | Action::MoveWordLeft
        | Action::MoveWordRight
        | Action::MoveHome
        | Action::MoveEnd
        | Action::PageUp
        | Action::PageDown
        | Action::Save
        | Action::Quit
        | Action::GoToDefinition
        | Action::GoBack
        | Action::ToggleTree
        | Action::Escape => None,
    }
}

fn compute_git_status(path: Option<&std::path::Path>) -> HashMap<usize, LineStatus> {
    let Some(path) = path else {
        return HashMap::new();
    };
    git::diff_against_head(path)
        .map(|changes| changes.into_iter().map(|c| (c.line, c.status)).collect())
        .unwrap_or_default()
}

fn describe_char(c: char) -> String {
    match c {
        '\n' => "newline".into(),
        '\t' => "tab".into(),
        ' ' => "space".into(),
        c if c.is_ascii_graphic() => format!("'{c}'"),
        c => format!("U+{:04X}", c as u32),
    }
}
