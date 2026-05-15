use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use crate::action::Action;
use crate::buffer::Buffer;
use crate::git::{self, LineStatus};
use crate::input;
use crate::language::Language;
use crate::lsp::{self, LspClient};
use crate::syntax::Syntax;
use crate::protocol;
use crate::tree::{self, Activation, FileTree};
use crate::tx::TxManager;
use crate::ui;
use crate::view::View;

/// TUI input mode. The agent path (`ProtocolState`) ignores this — it's
/// purely a guard for the keyboard so a stray keystroke can't mutate the
/// buffer while the human is reading code that an MCP agent may also be
/// editing. See CLAUDE.md "symmetric clients" invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Read-only. Movement, modals, save, and quit work; insert / delete
    /// / clear-line / LSP-rename are blocked and surface a status hint.
    View,
    /// Default editing behavior — every key behaves as it did pre-mode.
    Edit,
}

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
    /// LSP clients keyed by language. The first buffer in a supported
    /// language lazy-spawns its server; subsequent buffers in the same
    /// language reuse the client. Empty until a supported file opens.
    pub lsp_clients: HashMap<Language, LspClient>,
    /// Cached language of the focused buffer. Drives which entry in
    /// `lsp_clients` receives `didChange` and which server backs hover /
    /// goto-def / rename requests.
    pub language: Option<Language>,
    pub lsp_uri: Option<String>,
    /// True when LSP spawn was attempted for the focused buffer (i.e.,
    /// the file was in a supported language), regardless of outcome.
    /// Combined with `active_lsp().is_some()` this lets the UI render
    /// an Active / Failed / Hidden indicator without re-checking the
    /// file extension.
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
    /// Toggle for autosave (Ctrl-W). When on, the buffer is written
    /// ~500ms after the last edit — see `autosave_debounce` and
    /// `maybe_autosave`. Scratch buffers (no path) are skipped.
    pub autosave: bool,
    /// Timestamp of the most recent buffer mutation. `maybe_autosave`
    /// compares this against `autosave_debounce` to decide whether
    /// enough idle time has passed to flush the file.
    last_edit: Option<Instant>,
    /// One-line input prompt overlaid on the status bar. `Some`
    /// while we're collecting a filename (or any future prompted
    /// input); routing in `apply` captures every key into the
    /// buffer until the user hits Enter or Esc.
    pub prompt: Option<Prompt>,
    /// Commit-history overlay (Ctrl-L). Independent from `diff`;
    /// has its own routing in `apply`.
    pub history: Option<HistoryView>,
    /// Ctrl-P keybinding-reference overlay. Simple visibility flag —
    /// the view's content is fully static so no other state needed.
    pub keys_help: bool,
    /// Ctrl-X fuzzy file-open dialog. `Some` while the dialog is
    /// visible; holds the (full) candidate list, the user's query,
    /// and the filtered match indices that drive rendering.
    pub open_file: Option<OpenFileView>,
    /// Alt-T workspace type-search dialog. `Some` while the dialog is
    /// visible; holds the user's query and the LSP-returned type hits
    /// for the current query (refreshed on each keystroke).
    pub type_search: Option<TypeSearchView>,
    /// Ctrl-G f project-wide text-search dialog. `Some` while the
    /// dialog is visible; holds the user's query and the per-line
    /// matches found by walking files under the tree root.
    pub text_search: Option<TextSearchView>,
    /// Git overlay (Ctrl-R). `Some` while the overlay is visible.
    /// Holds the change-list, the diff for the currently-selected
    /// file, and the repo root we resolved when the overlay opened
    /// (so navigation doesn't re-shell `rev-parse` for every key).
    pub diff: Option<GitView>,
    /// Pending chord prefix. `Some(Chord::CtrlG)` after the user
    /// presses Ctrl-G; the next keystroke is consumed by
    /// `resolve_chord` to pick a destination. Cleared on any
    /// keystroke whether it matches a chord arm or not.
    pub chord: Option<Chord>,
    /// Current input mode. Starts in `View` when launched on a file
    /// (the user's reading workflow is the default), `Edit` for a
    /// scratch buffer (no path yet, so the user is necessarily about
    /// to create content). `open_file` resets back to `View` on every
    /// cross-file navigation. Toggle with `Ctrl-V`.
    pub mode: Mode,
}

#[derive(Clone, Copy)]
pub enum Chord {
    /// Ctrl-G prefix: g/d = go-to-def, t = find type, l/v = go to line,
    /// b = back. Anything else (incl. Esc) silently cancels.
    CtrlG,
}

pub struct Prompt {
    pub label: &'static str,
    pub buffer: String,
    pub kind: PromptKind,
}

pub enum PromptKind {
    /// Create a new file rooted at `parent`. The prompt buffer is
    /// treated as a path relative to `parent`; intermediate
    /// directories are created on confirm.
    NewFile { parent: PathBuf },
    /// Commit currently-staged changes in `repo_root` with the
    /// prompt buffer as the commit message.
    CommitMessage { repo_root: PathBuf },
    /// LSP rename. The cursor position captured here pins the request
    /// to the symbol the user actually wanted to rename, even if they
    /// move the cursor while typing into the prompt.
    RenameSymbol { line: u32, character: u32 },
    /// Jump to a 1-based line number typed by the user.
    GoToLine,
}

pub struct OpenFileView {
    /// Root the dialog was opened against. New buffers resolve names
    /// relative to this so we can show short relative paths.
    pub root: PathBuf,
    /// All non-hidden, non-junk files under `root`, sorted. Captured
    /// once at open time — refresh = close + reopen.
    pub files: Vec<PathBuf>,
    /// What the user has typed so far. Replaces the buffer cursor
    /// while the dialog is open; routing in `apply` funnels keys
    /// straight in.
    pub query: String,
    /// Indices into `files` matching the current query, score-sorted
    /// (prefix matches before mid-string matches).
    pub matches: Vec<usize>,
    /// Position within `matches`; Up/Down moves this.
    pub cursor: usize,
    pub top: usize,
}

pub struct TypeSearchView {
    /// What the user has typed so far. Each non-control char or
    /// Backspace re-issues a `workspace/symbol` request.
    pub query: String,
    /// Server-returned type candidates for the current query, filtered
    /// to type-like `SymbolKind`s (struct/enum/trait/etc). Order is
    /// whatever rust-analyzer ranked.
    pub results: Vec<TypeHit>,
    /// Selection within `results`.
    pub cursor: usize,
    pub top: usize,
    /// Empty-state message ("Type to search types…",
    /// "rust-analyzer still indexing…", "No matches"). Shown only when
    /// `results` is empty.
    pub status: String,
}

#[derive(Clone)]
pub struct TypeHit {
    pub name: String,
    pub container: Option<String>,
    pub uri: String,
    pub line: u32,
    pub character: u32,
}

pub struct TextSearchView {
    /// Root the search was scoped to — the tree's project root at the
    /// moment the dialog was opened. Cached so we don't re-resolve it
    /// on every keystroke.
    pub root: PathBuf,
    /// What the user has typed so far. Each non-control char or
    /// Backspace re-walks the tree under `root`.
    pub query: String,
    /// Per-line hits for the current query, ordered file-then-line.
    /// Capped by `TEXT_SEARCH_MAX_HITS` so a 2-char query against a
    /// huge tree can't freeze the UI.
    pub results: Vec<TextHit>,
    pub cursor: usize,
    pub top: usize,
    /// Empty-state line shown when `results` is empty
    /// ("Type to search…", "No matches for …", or a max-hits notice).
    pub status: String,
}

#[derive(Clone)]
pub struct TextHit {
    /// Path of the matching file relative to `root`. Joined back with
    /// `root` on jump so the buffer opens at an absolute path.
    pub path: PathBuf,
    /// 0-based line number within the file.
    pub line: usize,
    /// 0-based char column of the first match on the line.
    pub col: usize,
    /// Full line text with leading/trailing whitespace trimmed, for
    /// rendering. Capped at `TEXT_SEARCH_PREVIEW_CHARS` to keep one row.
    pub preview: String,
}

pub struct HistoryView {
    pub entries: Vec<git::LogEntry>,
    pub cursor: usize,
    pub commit_lines: Vec<String>,
    pub commit_scroll: usize,
    pub repo_root: PathBuf,
}

pub struct GitView {
    pub files: Vec<GitFile>,
    /// Index into `files` of the currently-selected entry. Changes on
    /// Up/Down and triggers a fresh `diff_for_path` load.
    pub cursor: usize,
    pub diff_lines: Vec<String>,
    pub diff_scroll: usize,
    pub repo_root: PathBuf,
}

pub struct GitFile {
    /// Path relative to `repo_root`.
    pub path: PathBuf,
    pub staged: char,
    pub unstaged: char,
    pub group: GitGroup,
}

/// Sections rendered in the change-list pane. Order matters: this is
/// also the sort key (Staged first, Untracked last) so navigating
/// Up/Down feels like scanning a `git status` from top to bottom.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitGroup {
    Staged,
    Both,
    Modified,
    Untracked,
}

impl GitGroup {
    pub fn header(self) -> &'static str {
        match self {
            GitGroup::Staged => "Staged",
            GitGroup::Both => "Staged + Modified",
            GitGroup::Modified => "Modified",
            GitGroup::Untracked => "Untracked",
        }
    }
}

struct NavPoint {
    path: PathBuf,
    line: usize,
    col: usize,
}

const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(500);

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
        let language = buffer.path().and_then(Language::for_path);
        let (lsp_clients, lsp_uri, lsp_attempted) = spawn_lsp(&buffer);
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
            lsp_clients,
            language,
            lsp_uri,
            lsp_attempted,
            lsp_version: 0,
            nav_stack: Vec::new(),
            tx_manager: TxManager::new(),
            quit_pending: false,
            tree: FileTree::new(tree_root),
            diff: None,
            chord: None,
            history: None,
            keys_help: false,
            open_file: None,
            type_search: None,
            text_search: None,
            prompt: None,
            autosave: false,
            last_edit: None,
            mode: Mode::View,
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
            lsp_clients: HashMap::new(),
            language: None,
            lsp_uri: None,
            lsp_attempted: false,
            lsp_version: 0,
            nav_stack: Vec::new(),
            tx_manager: TxManager::new(),
            quit_pending: false,
            tree,
            diff: None,
            chord: None,
            history: None,
            keys_help: false,
            open_file: None,
            type_search: None,
            text_search: None,
            prompt: None,
            autosave: false,
            last_edit: None,
            mode: Mode::Edit,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while self.running {
            terminal.draw(|frame| ui::render(frame, self))?;
            self.handle_events()?;
            self.maybe_autosave();
        }
        Ok(())
    }

    /// Idle-debounced autosave. Runs after every event-loop tick:
    /// when the buffer is dirty, has a path, and the user hasn't
    /// typed for at least `AUTOSAVE_DEBOUNCE`, write to disk.
    /// Failures are silently dropped — surfacing them on every tick
    /// would just spam the status bar; the next manual save will
    /// produce the same error.
    fn maybe_autosave(&mut self) {
        if !self.autosave || !self.buffer.is_dirty() || self.buffer.path().is_none() {
            return;
        }
        let Some(t) = self.last_edit else {
            return;
        };
        if t.elapsed() < AUTOSAVE_DEBOUNCE {
            return;
        }
        if let Ok(bytes) = self.buffer.save() {
            self.git_status = compute_git_status(self.buffer.path());
            self.status = format!("Autosaved {} bytes", bytes);
            self.last_edit = None;
        }
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
        // Chord prefix consumes the very next keystroke. Resolution
        // happens before every other routing — while a chord is
        // pending, no modal, prompt, or edit can fire. resolve_chord
        // clears the chord and may dispatch a follow-up action via a
        // recursive apply call.
        if let Some(chord) = self.chord {
            return self.resolve_chord(chord, action);
        }

        // Any non-Quit input clears the pending-quit confirmation.
        if !matches!(action, Action::Quit) {
            self.quit_pending = false;
        }

        // Prompt takes precedence over every other mode: while the
        // user is typing into the status-bar input we route every key
        // there. Returns early so no buffer mutation can leak through.
        if self.prompt.is_some() {
            return self.drive_prompt(action);
        }

        // Modal routing. Toggles and Escape are mode-independent;
        // anything else either drives the active modal or is swallowed
        // while one is open. Save/Quit are explicitly passed through so
        // the user is never locked in.
        match action {
            Action::ToggleTree => {
                self.tree.focused = !self.tree.focused;
                // On open: expand the tree down to the file the user
                // is currently editing and select it. Skipped for
                // scratch buffers (no path) and when the buffer's
                // file lives outside the tree root.
                if self.tree.focused
                    && let Some(path) = self.buffer.path()
                {
                    self.tree.reveal(path);
                }
                return Ok(());
            }
            Action::ToggleGitDiff => {
                self.toggle_git_diff();
                return Ok(());
            }
            Action::ToggleHistory => {
                self.toggle_history();
                return Ok(());
            }
            Action::ToggleKeysHelp => {
                self.keys_help = !self.keys_help;
                return Ok(());
            }
            Action::OpenFile => {
                self.start_open_file_dialog();
                return Ok(());
            }
            Action::OpenTypeSearch => {
                self.start_type_search_dialog();
                return Ok(());
            }
            Action::OpenTextSearch => {
                self.start_text_search_dialog();
                return Ok(());
            }
            Action::NewFile => {
                self.start_new_file_prompt();
                return Ok(());
            }
            Action::GoToLine => {
                self.start_goto_line_prompt();
                return Ok(());
            }
            Action::CtrlGPrefix => {
                self.chord = Some(Chord::CtrlG);
                self.status =
                    "Ctrl-G: g/d=def · t=type · f=find · l/v=line · b=back · Esc cancels".into();
                return Ok(());
            }
            Action::ToggleAutosave => {
                self.autosave = !self.autosave;
                self.status = if self.autosave {
                    "Autosave on".into()
                } else {
                    "Autosave off".into()
                };
                return Ok(());
            }
            Action::ToggleMode => {
                self.mode = match self.mode {
                    Mode::View => Mode::Edit,
                    Mode::Edit => Mode::View,
                };
                self.status = match self.mode {
                    Mode::View => "View mode".into(),
                    Mode::Edit => "Edit mode".into(),
                };
                return Ok(());
            }
            Action::Escape => {
                if self.keys_help {
                    self.keys_help = false;
                } else if self.open_file.is_some() {
                    self.open_file = None;
                } else if self.type_search.is_some() {
                    self.type_search = None;
                } else if self.text_search.is_some() {
                    self.text_search = None;
                } else if self.history.is_some() {
                    self.history = None;
                } else if self.diff.is_some() {
                    self.diff = None;
                } else if self.tree.focused {
                    self.tree.focused = false;
                } else {
                    // Nothing modal to close — fall through to clear
                    // any transient status (Ctrl-K type hint, save
                    // confirmation, error message) so the status bar
                    // returns to its default keymap hint.
                    self.status.clear();
                    self.quit_pending = false;
                }
                return Ok(());
            }
            Action::Save | Action::Quit => {}
            // Help overlay swallows everything else — nothing should
            // mutate state while the user is reading the keymap.
            _ if self.keys_help => return Ok(()),
            _ if self.open_file.is_some() => {
                self.drive_open_file(action)?;
                return Ok(());
            }
            _ if self.type_search.is_some() => {
                self.drive_type_search(action)?;
                return Ok(());
            }
            _ if self.text_search.is_some() => {
                self.drive_text_search(action)?;
                return Ok(());
            }
            _ if self.history.is_some() => {
                self.drive_history(action);
                return Ok(());
            }
            _ if self.diff.is_some() => {
                self.drive_diff(action)?;
                return Ok(());
            }
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

        // View-mode gate. Block buffer-mutating actions before the auto-tx
        // opens so a stray keystroke can't even start a transaction. The
        // agent path (`ProtocolState`) is independent of this — MCP edits
        // keep landing in either mode, preserving the symmetric-clients
        // invariant.
        if matches!(self.mode, Mode::View)
            && matches!(
                action,
                Action::Insert(_)
                    | Action::DeletePrev
                    | Action::DeleteNext
                    | Action::ClearLine
                    | Action::Rename
            )
        {
            self.status = "View mode — Ctrl-V to edit".into();
            return Ok(());
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
            Action::ClearLine => {
                let line = self.view.cursor_line();
                let start = self.buffer.line_to_char(line);
                let len = self.buffer.line_len_chars(line);
                if len > 0 {
                    self.buffer.delete_range(start..start + len);
                }
                self.view.goto(&self.buffer, line, 0);
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
            Action::ShowType => {
                self.show_type();
            }
            Action::Rename => {
                self.start_rename_prompt();
            }
            // Handled in the modal-routing block above; listed here so the
            // compiler stays happy about exhaustiveness.
            Action::ToggleTree
            | Action::ToggleGitDiff
            | Action::ToggleHistory
            | Action::ToggleKeysHelp
            | Action::OpenFile
            | Action::OpenTypeSearch
            | Action::OpenTextSearch
            | Action::NewFile
            | Action::GoToLine
            | Action::CtrlGPrefix
            | Action::ToggleAutosave
            | Action::ToggleMode
            | Action::Escape => {}
            // Listed for exhaustiveness — `ShowType`/`Rename` are
            // handled in their own arms above.
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
                self.last_edit = Some(Instant::now());
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

    /// LSP client for the currently-focused buffer, if any.
    pub fn active_lsp(&self) -> Option<&LspClient> {
        self.language.and_then(|lang| self.lsp_clients.get(&lang))
    }

    fn notify_lsp_changed(&mut self) {
        let Some(lang) = self.language else { return };
        let Some(uri) = self.lsp_uri.clone() else { return };
        self.lsp_version += 1;
        let version = self.lsp_version;
        let text = self.buffer.rope().to_string();
        if let Some(lsp) = self.lsp_clients.get(&lang) {
            let _ = lsp.did_change(&uri, version, &text);
        }
    }

    /// Resolve `textDocument/definition` at the cursor. Returns the
    /// status-bar message — empty string means "moved cursor, no
    /// message needed."
    fn go_to_definition(&mut self) -> String {
        let (Some(lsp), Some(uri)) = (self.active_lsp(), self.lsp_uri.as_ref()) else {
            return "LSP not available".into();
        };
        let line = self.view.cursor_line() as u32;
        let character = self.view.cursor_col() as u32;
        let result = lsp.definition(uri, line, character);
        match result {
            Ok(locs) if locs.is_empty() => {
                if lsp.is_indexing() {
                    format!("{} still indexing — try again in a moment", lsp.language().lsp_binary())
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

    /// Ctrl-X — pop up the fuzzy file-open dialog rooted at the
    /// project root (tree.root). Files are gathered on open (one
    /// walk, then filter-in-memory on each keystroke) so the dialog
    /// stays responsive without any background indexing.
    fn start_open_file_dialog(&mut self) {
        if self.open_file.is_some() {
            return;
        }
        let root = self.tree.root.clone();
        let files = walk_files(&root);
        if files.is_empty() {
            self.status = "No files under project root".into();
            return;
        }
        let matches: Vec<usize> = (0..files.len()).collect();
        self.open_file = Some(OpenFileView {
            root,
            files,
            query: String::new(),
            matches,
            cursor: 0,
            top: 0,
        });
    }

    /// Route a key into the open-file dialog. Letters/Backspace edit
    /// the query (which re-filters), arrows navigate matches, Enter
    /// opens the selected entry, Esc cancels.
    fn drive_open_file(&mut self, action: Action) -> Result<()> {
        let Some(view) = self.open_file.as_mut() else {
            return Ok(());
        };
        let mut refilter = false;
        match action {
            Action::Insert('\n') => {
                let selection = view
                    .matches
                    .get(view.cursor)
                    .and_then(|&i| view.files.get(i))
                    .cloned();
                let root = view.root.clone();
                self.open_file = None;
                if let Some(rel) = selection {
                    let full = root.join(rel);
                    if self.buffer.is_dirty() {
                        self.status =
                            "Save first — current buffer has unsaved changes".into();
                        return Ok(());
                    }
                    self.push_nav_stack();
                    match self.open_file(&full) {
                        Ok(()) => self.status = String::new(),
                        Err(e) => {
                            self.nav_stack.pop();
                            self.status =
                                format!("Could not open {}: {e}", full.display());
                        }
                    }
                }
                return Ok(());
            }
            Action::Insert(c) if !c.is_control() => {
                view.query.push(c);
                refilter = true;
            }
            Action::DeletePrev => {
                if view.query.pop().is_some() {
                    refilter = true;
                }
            }
            Action::MoveUp => {
                if view.cursor > 0 {
                    view.cursor -= 1;
                }
            }
            Action::MoveDown => {
                if view.cursor + 1 < view.matches.len() {
                    view.cursor += 1;
                }
            }
            Action::MoveHome => view.cursor = 0,
            Action::MoveEnd => {
                if !view.matches.is_empty() {
                    view.cursor = view.matches.len() - 1;
                }
            }
            // Other input (arrows in-line, etc.) is swallowed — the
            // dialog is its own little world.
            _ => {}
        }
        if refilter {
            view.matches = filter_files(&view.files, &view.query);
            view.cursor = 0;
            view.top = 0;
        }
        Ok(())
    }

    /// Alt-T — pop up the workspace type-search dialog. Refuses to
    /// open when no LSP is active for the focused buffer; the dialog
    /// is useless without `workspace/symbol`.
    fn start_type_search_dialog(&mut self) {
        if self.type_search.is_some() {
            return;
        }
        let Some(lsp) = self.active_lsp() else {
            self.status = "LSP not available".into();
            return;
        };
        let status = if lsp.is_indexing() {
            format!(
                "{} still indexing — try again in a moment",
                lsp.language().lsp_binary()
            )
        } else {
            "Type to search workspace types".to_string()
        };
        self.type_search = Some(TypeSearchView {
            query: String::new(),
            results: Vec::new(),
            cursor: 0,
            top: 0,
            status,
        });
    }

    /// Route a key into the type-search dialog. Letters/Backspace
    /// re-issue `workspace/symbol`, arrows navigate, Enter jumps to
    /// the selected type, Esc cancels.
    fn drive_type_search(&mut self, action: Action) -> Result<()> {
        let Some(view) = self.type_search.as_mut() else {
            return Ok(());
        };
        let mut requery = false;
        match action {
            Action::Insert('\n') => {
                let hit = view.results.get(view.cursor).cloned();
                self.type_search = None;
                if let Some(hit) = hit {
                    self.jump_to_type_hit(hit);
                }
                return Ok(());
            }
            Action::Insert(c) if !c.is_control() => {
                view.query.push(c);
                requery = true;
            }
            Action::DeletePrev => {
                if view.query.pop().is_some() {
                    requery = true;
                }
            }
            Action::MoveUp => {
                if view.cursor > 0 {
                    view.cursor -= 1;
                }
            }
            Action::MoveDown => {
                if view.cursor + 1 < view.results.len() {
                    view.cursor += 1;
                }
            }
            Action::MoveHome => view.cursor = 0,
            Action::MoveEnd => {
                if !view.results.is_empty() {
                    view.cursor = view.results.len() - 1;
                }
            }
            _ => {}
        }
        if requery {
            self.refresh_type_search_results();
        }
        Ok(())
    }

    /// Re-run `workspace/symbol` against the dialog's current query
    /// and store the type-filtered hits back into the view. Skips the
    /// LSP round-trip for queries under 2 chars (rust-analyzer returns
    /// the entire workspace at that point — useless and slow).
    fn refresh_type_search_results(&mut self) {
        // Snapshot what we need from the view, then drop the borrow
        // so we can call `active_lsp` (immutable self) without overlap.
        let Some(view) = self.type_search.as_ref() else {
            return;
        };
        let query = view.query.clone();
        if query.chars().count() < 2 {
            if let Some(v) = self.type_search.as_mut() {
                v.results.clear();
                v.cursor = 0;
                v.top = 0;
                v.status = "Type at least 2 characters".into();
            }
            return;
        }
        let Some(lsp) = self.active_lsp() else {
            return;
        };
        let indexing = lsp.is_indexing();
        let lang_binary = lsp.language().lsp_binary();
        let outcome = lsp.workspace_symbol(&query);
        let Some(v) = self.type_search.as_mut() else {
            return;
        };
        match outcome {
            Ok(hits) => {
                let types: Vec<TypeHit> = hits
                    .into_iter()
                    .filter(|s| is_type_kind(s.kind))
                    .map(|s| TypeHit {
                        name: s.name,
                        container: s.container_name,
                        uri: s.location.uri,
                        line: s.location.range.start.line,
                        character: s.location.range.start.character,
                    })
                    .collect();
                v.cursor = 0;
                v.top = 0;
                v.status = if types.is_empty() {
                    if indexing {
                        format!("{lang_binary} still indexing — try again in a moment")
                    } else {
                        format!("No type matches for \"{query}\"")
                    }
                } else {
                    String::new()
                };
                v.results = types;
            }
            Err(e) => {
                v.results.clear();
                v.cursor = 0;
                v.top = 0;
                v.status = format!("workspace/symbol failed: {e}");
            }
        }
    }

    /// Ctrl-G f — pop up the project-wide text-search dialog rooted
    /// at `tree.root`. The dialog walks files lazily on each query
    /// (kept short by the 2-char minimum + result cap) rather than
    /// pre-indexing, so opening it is instant even on large trees.
    fn start_text_search_dialog(&mut self) {
        if self.text_search.is_some() {
            return;
        }
        self.text_search = Some(TextSearchView {
            root: self.tree.root.clone(),
            query: String::new(),
            results: Vec::new(),
            cursor: 0,
            top: 0,
            status: "Type at least 2 characters".into(),
        });
    }

    /// Route a key into the text-search dialog. Letters/Backspace
    /// re-walk the tree, arrows navigate matches, Enter opens the
    /// selected file at the matching line/column, Esc cancels.
    fn drive_text_search(&mut self, action: Action) -> Result<()> {
        let Some(view) = self.text_search.as_mut() else {
            return Ok(());
        };
        let mut refilter = false;
        match action {
            Action::Insert('\n') => {
                let hit = view.results.get(view.cursor).cloned();
                self.text_search = None;
                if let Some(hit) = hit {
                    self.jump_to_text_hit(hit);
                }
                return Ok(());
            }
            Action::Insert(c) if !c.is_control() => {
                view.query.push(c);
                refilter = true;
            }
            Action::DeletePrev => {
                if view.query.pop().is_some() {
                    refilter = true;
                }
            }
            Action::MoveUp => {
                if view.cursor > 0 {
                    view.cursor -= 1;
                }
            }
            Action::MoveDown => {
                if view.cursor + 1 < view.results.len() {
                    view.cursor += 1;
                }
            }
            Action::MoveHome => view.cursor = 0,
            Action::MoveEnd => {
                if !view.results.is_empty() {
                    view.cursor = view.results.len() - 1;
                }
            }
            _ => {}
        }
        if refilter {
            self.refresh_text_search_results();
        }
        Ok(())
    }

    /// Re-walk the tree under the dialog's `root` and store hits for
    /// the current query. Skips the walk for queries under 2 chars
    /// (matches the type-search idiom — single letters would just
    /// return everything).
    fn refresh_text_search_results(&mut self) {
        let Some(view) = self.text_search.as_mut() else {
            return;
        };
        view.cursor = 0;
        view.top = 0;
        if view.query.chars().count() < 2 {
            view.results.clear();
            view.status = "Type at least 2 characters".into();
            return;
        }
        let root = view.root.clone();
        let query = view.query.clone();
        let (hits, hit_capped) = grep_under_root(&root, &query);
        let v = self.text_search.as_mut().expect("set above");
        v.results = hits;
        v.status = if v.results.is_empty() {
            format!("No matches for \"{}\"", query)
        } else if hit_capped {
            format!(
                "{} match(es) — capped at {}",
                v.results.len(),
                TEXT_SEARCH_MAX_HITS
            )
        } else {
            String::new()
        };
    }

    /// Open the file backing a text-search hit and place the cursor
    /// on the matching column. Mirrors `jump_to_type_hit` — same
    /// dirty-buffer guard, same nav-stack push, same rollback.
    fn jump_to_text_hit(&mut self, hit: TextHit) {
        let target = hit.path.clone();
        let absolute = if target.is_absolute() {
            target
        } else {
            self.tree.root.join(target)
        };
        if self.buffer.path() == Some(absolute.as_path()) {
            self.view.goto(&self.buffer, hit.line, hit.col);
            self.status = String::new();
            return;
        }
        if self.buffer.is_dirty() {
            self.status = "Save first — current buffer has unsaved changes".into();
            return;
        }
        self.push_nav_stack();
        match self.open_file(&absolute) {
            Ok(()) => {
                self.view.goto(&self.buffer, hit.line, hit.col);
                self.status = String::new();
            }
            Err(e) => {
                self.nav_stack.pop();
                self.status = format!("Could not open {}: {e}", absolute.display());
            }
        }
    }

    /// Navigate to the location of a type-search hit. Mirrors the
    /// cross-file branch of `go_to_definition` — same dirty-buffer
    /// guard, same nav-stack push, same rollback on open failure.
    fn jump_to_type_hit(&mut self, hit: TypeHit) {
        let target_line = hit.line as usize;
        let target_col = hit.character as usize;
        if Some(&hit.uri) == self.lsp_uri.as_ref() {
            self.view.goto(&self.buffer, target_line, target_col);
            self.status = String::new();
            return;
        }
        let Some(target_path) = uri_to_path(&hit.uri) else {
            self.status = format!("Type has unparseable URI {}", hit.uri);
            return;
        };
        if self.buffer.is_dirty() {
            self.status = "Save first — current buffer has unsaved changes".into();
            return;
        }
        self.push_nav_stack();
        match self.open_file(&target_path) {
            Ok(()) => {
                self.view.goto(&self.buffer, target_line, target_col);
                self.status = String::new();
            }
            Err(e) => {
                self.nav_stack.pop();
                self.status = format!("Could not open {}: {e}", target_path.display());
            }
        }
    }

    /// Open the New-File prompt anchored at the user's current point
    /// in the tree (selected directory, the parent of a selected file,
    /// or the tree root). Idempotent — pressing Ctrl-N twice doesn't
    /// reset what the user has typed.
    fn start_new_file_prompt(&mut self) {
        if self.prompt.is_some() {
            return;
        }
        let parent = self.new_file_parent();
        self.prompt = Some(Prompt {
            label: "New file:",
            buffer: String::new(),
            kind: PromptKind::NewFile { parent },
        });
    }

    fn new_file_parent(&self) -> PathBuf {
        if self.tree.focused
            && let Some(entry) = self.tree.entries.get(self.tree.cursor)
        {
            if entry.is_parent_link {
                return self.tree.root.clone();
            }
            if entry.is_dir {
                return entry.path.clone();
            }
            if let Some(p) = entry.path.parent() {
                return p.to_path_buf();
            }
        }
        self.tree.root.clone()
    }

    fn drive_prompt(&mut self, action: Action) -> Result<()> {
        let Some(prompt) = self.prompt.as_mut() else {
            return Ok(());
        };
        match action {
            Action::Escape => {
                self.prompt = None;
            }
            Action::Insert('\n') => {
                let confirmed = self.prompt.take();
                if let Some(p) = confirmed {
                    self.confirm_prompt(p)?;
                }
            }
            Action::Insert(c) if !c.is_control() => {
                prompt.buffer.push(c);
            }
            Action::DeletePrev => {
                prompt.buffer.pop();
            }
            // Movement / save / quit etc. are deliberately ignored while
            // a prompt is open — keeps the input box behavior obvious.
            _ => {}
        }
        Ok(())
    }

    fn confirm_prompt(&mut self, prompt: Prompt) -> Result<()> {
        match prompt.kind {
            PromptKind::NewFile { parent } => self.create_new_file(parent, prompt.buffer),
            PromptKind::CommitMessage { repo_root } => self.do_commit(repo_root, prompt.buffer),
            PromptKind::RenameSymbol { line, character } => {
                self.do_rename(line, character, prompt.buffer)
            }
            PromptKind::GoToLine => {
                self.do_goto_line(prompt.buffer);
                Ok(())
            }
        }
    }

    /// Resolve the keystroke that follows a chord prefix. The chord
    /// state is cleared up-front so a recursive `apply` call lands in
    /// the normal routing path. Any arm we don't recognise (including
    /// Esc and any other modifier-bearing key) silently cancels — we
    /// deliberately do not let the cancelling key fall through and
    /// fire its normal action, since the user pressed Ctrl-G to
    /// disambiguate and we'd rather they retry than accidentally
    /// trigger a mutation.
    fn resolve_chord(&mut self, chord: Chord, action: Action) -> Result<()> {
        self.chord = None;
        self.status.clear();
        let resolved = match chord {
            Chord::CtrlG => match action {
                // Vim-style "gg" (Ctrl-G Ctrl-G) and the explicit "d"
                // arm both fire go-to-definition. The plain letter
                // forms come through as `Insert` because chord arms
                // are pressed without a modifier.
                Action::CtrlGPrefix => Some(Action::GoToDefinition),
                Action::Insert('g' | 'G' | 'd' | 'D') => Some(Action::GoToDefinition),
                Action::Insert('t' | 'T') => Some(Action::OpenTypeSearch),
                // "f" for find: project-wide text search dialog rooted
                // at the tree's project root. "s" reads as "search" but
                // is also accepted for muscle-memory.
                Action::Insert('f' | 'F' | 's' | 'S') => Some(Action::OpenTextSearch),
                Action::Insert('l' | 'L' | 'v' | 'V') => Some(Action::GoToLine),
                Action::Insert('b' | 'B') => Some(Action::GoBack),
                _ => None,
            },
        };
        if let Some(a) = resolved {
            self.apply(a)
        } else {
            Ok(())
        }
    }

    /// Ctrl-V ("visit line") — also reachable via the Ctrl-G prefix
    /// (Ctrl-G then L or V). Prompts for a 1-based line number and
    /// jumps to it. Alt-G and Ctrl-Shift-G are silent fallbacks in
    /// `input::map` for terminals that deliver them.
    fn start_goto_line_prompt(&mut self) {
        if self.prompt.is_some() {
            return;
        }
        self.prompt = Some(Prompt {
            label: "Go to line:",
            buffer: String::new(),
            kind: PromptKind::GoToLine,
        });
    }

    fn do_goto_line(&mut self, input: String) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }
        let parsed: Option<usize> = trimmed.parse().ok();
        let Some(n) = parsed else {
            self.status = format!("Not a line number: {trimmed}");
            return;
        };
        if n == 0 {
            self.status = "Line numbers start at 1".into();
            return;
        }
        let last_line = self.buffer.line_count().saturating_sub(1);
        let target = (n - 1).min(last_line);
        self.view.goto(&self.buffer, target, 0);
        if target + 1 != n {
            self.status = format!("Clamped to last line ({})", target + 1);
        }
    }

    /// Ctrl-K — ask the LSP for the type/signature at the cursor and
    /// surface it in the status bar.
    ///
    /// rust-analyzer's hover payload starts with the enclosing
    /// path/namespace as a code block ("crate::module::Item") and
    /// puts the actual `let x: T` / `fn foo() -> U` in a *later*
    /// block. `extract_signature` walks the fenced blocks and returns
    /// the last non-empty one, which is the type the user is asking
    /// about — picking the first block would surface the surrounding
    /// scope instead.
    ///
    /// Field-definition special case: when the cursor sits on a
    /// struct field name (e.g. `pub history: Option<…>`), the hover
    /// payload is just the parent struct's path with no field info.
    /// `looks_like_path_only` detects that shape and falls back to
    /// extracting `: Type` from the source line so the user gets
    /// `Option<…>` instead of `dyad::app::App`.
    fn show_type(&mut self) {
        let (Some(lsp), Some(uri)) = (self.active_lsp(), self.lsp_uri.as_ref()) else {
            self.status = "LSP not available".into();
            return;
        };
        let line_idx = self.view.cursor_line();
        let line = line_idx as u32;
        let character = self.view.cursor_col() as u32;
        // The source-line `: Type` fallback is a Rust-specific heuristic
        // (`looks_like_path_only` keys on `::`-separated paths); only
        // engage it when the active language opts in.
        let source_fallback_ok = self
            .language
            .map(Language::supports_type_from_source_line)
            .unwrap_or(false);
        match lsp.hover(uri, line, character) {
            Ok(Some(text)) => match extract_signature(&text) {
                Some(sig) => {
                    let resolved = if source_fallback_ok && looks_like_path_only(&sig) {
                        type_from_source_line(&self.buffer, line_idx).unwrap_or(sig)
                    } else {
                        sig
                    };
                    self.status = format!("type: {resolved}");
                }
                None => self.status = "No type info".into(),
            },
            Ok(None) => {
                self.status = if lsp.is_indexing() {
                    format!("{} still indexing — try again in a moment", lsp.language().lsp_binary())
                } else {
                    "No type info".into()
                };
            }
            Err(e) => self.status = format!("Hover failed: {e}"),
        }
    }

    /// Ctrl-Y — open the rename prompt prefilled with the word under
    /// the cursor. The cursor's `(line, character)` is captured into
    /// the prompt so a stray Up/Down inside the prompt buffer doesn't
    /// repoint the rename request.
    fn start_rename_prompt(&mut self) {
        if self.active_lsp().is_none() {
            self.status = "LSP not available".into();
            return;
        }
        let line = self.view.cursor_line() as u32;
        let character = self.view.cursor_col() as u32;
        let current = word_at_cursor(&self.buffer, &self.view);
        self.prompt = Some(Prompt {
            label: "Rename to:",
            buffer: current,
            kind: PromptKind::RenameSymbol { line, character },
        });
    }

    fn do_rename(&mut self, line: u32, character: u32, new_name: String) -> Result<()> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            self.status = "Empty name — rename aborted".into();
            return Ok(());
        }
        let (Some(lsp), Some(uri)) = (self.active_lsp(), self.lsp_uri.as_ref()) else {
            self.status = "LSP not available".into();
            return Ok(());
        };
        let workspace_edit = match lsp.rename(uri, line, character, trimmed) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("Rename failed: {e}");
                return Ok(());
            }
        };
        let current_uri = self.lsp_uri.clone();
        // Wrap the in-buffer edits in an auto-tx so the rename lands
        // in the flat history with a human-readable intent (same idiom
        // as the MCP protocol layer's `edit_rename_symbol`).
        let intent = format!("rename -> {trimmed}");
        let tx_id = self.tx_manager.begin(intent, None, &self.buffer);
        let pre_version = self.tx_manager.pre_version(tx_id);

        let mut applied = 0;
        let mut other_files = 0;
        for (edit_uri, edits) in &workspace_edit.changes {
            if Some(edit_uri) == current_uri.as_ref() {
                // Sort end-to-start so each successive edit's offsets
                // are still correct under the previous one's mutation.
                let mut sorted = edits.clone();
                sorted.sort_by(|a, b| {
                    (
                        b.range.start.line,
                        b.range.start.character,
                        b.range.end.line,
                        b.range.end.character,
                    )
                        .cmp(&(
                            a.range.start.line,
                            a.range.start.character,
                            a.range.end.line,
                            a.range.end.character,
                        ))
                });
                if let Err(e) = protocol::apply_text_edits(&mut self.buffer, &sorted) {
                    self.tx_manager.discard(tx_id)?;
                    self.status = format!("Rename apply failed: {e}");
                    return Ok(());
                }
                applied += sorted.len();
            } else {
                other_files += 1;
            }
        }

        if Some(self.buffer.version()) == pre_version {
            self.tx_manager.discard(tx_id)?;
        } else {
            self.tx_manager.commit(tx_id, &self.buffer)?;
            self.notify_lsp_changed();
            self.last_edit = Some(Instant::now());
            if let Some(syn) = self.syntax.as_mut() {
                syn.refresh(&mut self.buffer);
            }
        }

        self.status = if other_files > 0 {
            format!(
                "Renamed {applied} occurrence(s); {other_files} other file(s) need manual edit"
            )
        } else if applied == 0 {
            "Nothing to rename".into()
        } else {
            format!("Renamed {applied} occurrence(s)")
        };
        Ok(())
    }

    fn create_new_file(&mut self, parent: PathBuf, name: String) -> Result<()> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let candidate = Path::new(trimmed);
        let target = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            parent.join(candidate)
        };
        if target.exists() {
            self.status = format!("{} already exists", target.display());
            return Ok(());
        }
        if self.buffer.is_dirty() {
            self.status = "Save first — current buffer has unsaved changes".into();
            return Ok(());
        }
        if let Some(dir) = target.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            self.status = format!("Could not create {}: {e}", dir.display());
            return Ok(());
        }
        if let Err(e) = std::fs::File::create(&target) {
            self.status = format!("Could not create {}: {e}", target.display());
            return Ok(());
        }
        self.push_nav_stack();
        if let Err(e) = self.open_file(&target) {
            self.nav_stack.pop();
            self.status = format!("Created but failed to open: {e}");
            return Ok(());
        }
        self.tree.focused = false;
        // Rebuild the tree from its current root so the new file is
        // visible the next time the user opens the sidebar. Drops
        // expansion state — acceptable cost for a clean view.
        let root = self.tree.root.clone();
        self.tree = FileTree::new(root);
        self.status = format!("Created {}", target.display());
        Ok(())
    }

    /// Toggle the Ctrl-R git overlay. Opening it shells out for the
    /// repo root + `git status`, groups the change list, and loads
    /// the diff for the first entry. An empty status (clean tree) or
    /// missing repo surface as a status-bar message instead of an
    /// empty overlay.
    fn toggle_git_diff(&mut self) {
        if self.diff.is_some() {
            self.diff = None;
            return;
        }
        // Resolve the repo root from the current buffer's file when
        // we have one, otherwise from the tree root — either covers
        // both `dyad <file>` and `dyad <dir>` launch modes.
        let probe = self
            .buffer
            .path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.tree.root.clone());
        let repo_root = match git::repo_root_for(&probe) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("git: {e}");
                return;
            }
        };
        let entries = match git::status_at(&repo_root) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("git: {e}");
                return;
            }
        };
        let mut files = entries
            .into_iter()
            .map(|s| {
                let group = classify(s.staged, s.unstaged);
                GitFile {
                    path: s.path,
                    staged: s.staged,
                    unstaged: s.unstaged,
                    group,
                }
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| match a.group.cmp(&b.group) {
            std::cmp::Ordering::Equal => a.path.cmp(&b.path),
            other => other,
        });
        if files.is_empty() {
            self.status = "Working tree clean".into();
            return;
        }
        let mut view = GitView {
            files,
            cursor: 0,
            diff_lines: Vec::new(),
            diff_scroll: 0,
            repo_root,
        };
        load_diff_for_cursor(&mut view);
        self.diff = Some(view);
    }

    /// Drive the git overlay. Up/Down moves between files (and
    /// refreshes the diff pane); Ctrl-U/D (PageUp/PageDown) scrolls
    /// the diff; Home/End jump within the file list. Letter keys
    /// 's' / 'u' / 'c' run git commands.
    fn drive_diff(&mut self, action: Action) -> Result<()> {
        let Some(view) = self.diff.as_mut() else {
            return Ok(());
        };
        let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let page = rows.saturating_sub(2).max(1) as usize;
        let last_file = view.files.len().saturating_sub(1);
        let last_diff = view.diff_lines.len().saturating_sub(1);
        let mut file_changed = false;
        match action {
            Action::MoveUp => {
                if view.cursor > 0 {
                    view.cursor -= 1;
                    file_changed = true;
                }
            }
            Action::MoveDown => {
                if view.cursor < last_file {
                    view.cursor += 1;
                    file_changed = true;
                }
            }
            Action::MoveHome => {
                if view.cursor != 0 {
                    view.cursor = 0;
                    file_changed = true;
                }
            }
            Action::MoveEnd => {
                if view.cursor != last_file {
                    view.cursor = last_file;
                    file_changed = true;
                }
            }
            Action::PageUp => {
                view.diff_scroll = view.diff_scroll.saturating_sub(page);
            }
            Action::PageDown => {
                view.diff_scroll = (view.diff_scroll + page).min(last_diff);
            }
            // Single-letter commands inside the overlay. They arrive
            // as Insert(c) because input.rs has no special tree mode;
            // the modal-routing block above only sends Insert here
            // when the overlay is open.
            Action::Insert('s') => {
                self.stage_at_cursor();
                return Ok(());
            }
            Action::Insert('u') => {
                self.unstage_at_cursor();
                return Ok(());
            }
            Action::Insert('c') => {
                self.start_commit_prompt();
                return Ok(());
            }
            _ => {}
        }
        if file_changed {
            view.diff_scroll = 0;
            load_diff_for_cursor(view);
        }
        Ok(())
    }

    fn stage_at_cursor(&mut self) {
        let Some(view) = self.diff.as_mut() else { return };
        let Some(file) = view.files.get(view.cursor) else {
            return;
        };
        let path = file.path.clone();
        let repo_root = view.repo_root.clone();
        match git::stage(&repo_root, &path) {
            Ok(()) => {
                self.status = format!("Staged {}", path.display());
                self.refresh_git_view();
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    fn unstage_at_cursor(&mut self) {
        let Some(view) = self.diff.as_mut() else { return };
        let Some(file) = view.files.get(view.cursor) else {
            return;
        };
        let path = file.path.clone();
        let repo_root = view.repo_root.clone();
        match git::unstage(&repo_root, &path) {
            Ok(()) => {
                self.status = format!("Unstaged {}", path.display());
                self.refresh_git_view();
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    fn start_commit_prompt(&mut self) {
        let Some(view) = self.diff.as_ref() else { return };
        let repo_root = view.repo_root.clone();
        self.prompt = Some(Prompt {
            label: "Commit message:",
            buffer: String::new(),
            kind: PromptKind::CommitMessage { repo_root },
        });
    }

    fn do_commit(&mut self, repo_root: PathBuf, message: String) -> Result<()> {
        let msg = message.trim();
        if msg.is_empty() {
            self.status = "Empty commit message — aborted".into();
            return Ok(());
        }
        match git::commit(&repo_root, msg) {
            Ok(_summary) => {
                self.status = format!("Committed: {msg}");
                if self.diff.is_some() {
                    self.refresh_git_view();
                }
                // Refresh the per-line gutter for the current file — a
                // commit may have moved its HEAD baseline.
                self.git_status = compute_git_status(self.buffer.path());
            }
            Err(e) => self.status = format!("{e}"),
        }
        Ok(())
    }

    /// Reload `status` after stage/unstage/commit. Keeps the cursor
    /// pinned to the previously-selected file when it still appears
    /// in the list; otherwise clamps to the end (so removing the last
    /// changed file lands on what's now the last entry).
    fn refresh_git_view(&mut self) {
        let Some(view) = self.diff.as_mut() else { return };
        let prev_path = view.files.get(view.cursor).map(|f| f.path.clone());
        let repo_root = view.repo_root.clone();
        let entries = match git::status_at(&repo_root) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("git: {e}");
                self.diff = None;
                return;
            }
        };
        if entries.is_empty() {
            self.status = "Working tree clean".into();
            self.diff = None;
            return;
        }
        let mut files: Vec<GitFile> = entries
            .into_iter()
            .map(|s| GitFile {
                group: classify(s.staged, s.unstaged),
                path: s.path,
                staged: s.staged,
                unstaged: s.unstaged,
            })
            .collect();
        files.sort_by(|a, b| match a.group.cmp(&b.group) {
            std::cmp::Ordering::Equal => a.path.cmp(&b.path),
            other => other,
        });
        // Try to preserve the selection by path; if the file's gone,
        // clamp the cursor to the new tail.
        let new_cursor = prev_path
            .as_ref()
            .and_then(|p| files.iter().position(|f| &f.path == p))
            .unwrap_or_else(|| files.len().saturating_sub(1));
        view.files = files;
        view.cursor = new_cursor;
        view.diff_scroll = 0;
        load_diff_for_cursor(view);
    }

    /// Toggle the Ctrl-L history view. Closing it is fast; opening
    /// shells out to `git log` for the last 200 commits and loads
    /// `git show` for the newest one into the right pane.
    fn toggle_history(&mut self) {
        if self.history.is_some() {
            self.history = None;
            return;
        }
        let probe = self
            .buffer
            .path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.tree.root.clone());
        let repo_root = match git::repo_root_for(&probe) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("git: {e}");
                return;
            }
        };
        let entries = match git::log(&repo_root, 200) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("git: {e}");
                return;
            }
        };
        if entries.is_empty() {
            self.status = "No commits yet".into();
            return;
        }
        let mut view = HistoryView {
            entries,
            cursor: 0,
            commit_lines: Vec::new(),
            commit_scroll: 0,
            repo_root,
        };
        load_commit_for_cursor(&mut view);
        self.history = Some(view);
    }

    /// Drive the history overlay. Up/Down between commits refreshes
    /// the show pane; Ctrl-U/D scrolls the show pane.
    fn drive_history(&mut self, action: Action) {
        let Some(view) = self.history.as_mut() else {
            return;
        };
        let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let page = rows.saturating_sub(2).max(1) as usize;
        let last_entry = view.entries.len().saturating_sub(1);
        let last_show = view.commit_lines.len().saturating_sub(1);
        let mut commit_changed = false;
        match action {
            Action::MoveUp => {
                if view.cursor > 0 {
                    view.cursor -= 1;
                    commit_changed = true;
                }
            }
            Action::MoveDown => {
                if view.cursor < last_entry {
                    view.cursor += 1;
                    commit_changed = true;
                }
            }
            Action::MoveHome => {
                if view.cursor != 0 {
                    view.cursor = 0;
                    commit_changed = true;
                }
            }
            Action::MoveEnd => {
                if view.cursor != last_entry {
                    view.cursor = last_entry;
                    commit_changed = true;
                }
            }
            Action::PageUp => {
                view.commit_scroll = view.commit_scroll.saturating_sub(page);
            }
            Action::PageDown => {
                view.commit_scroll = (view.commit_scroll + page).min(last_show);
            }
            _ => {}
        }
        if commit_changed {
            view.commit_scroll = 0;
            load_commit_for_cursor(view);
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
    /// matching LSP client about the new file. Each language's server is
    /// spawned at most once per session and reused across buffers.
    fn open_file(&mut self, path: &Path) -> Result<()> {
        let mut new_buffer = Buffer::open(path.to_path_buf())?;
        let mut new_syntax = Syntax::for_path(new_buffer.path());
        if let Some(syn) = new_syntax.as_mut() {
            syn.refresh(&mut new_buffer);
        }
        let new_git_status = compute_git_status(new_buffer.path());
        let new_uri = new_buffer.path().map(lsp::path_to_uri);
        let new_language = new_buffer.path().and_then(Language::for_path);

        if let Some(lang) = new_language
            && !self.lsp_clients.contains_key(&lang)
        {
            let (clients, _spawned_uri, attempted) = spawn_lsp(&new_buffer);
            self.lsp_clients.extend(clients);
            self.lsp_attempted = attempted;
            // spawn already issued didOpen with the file's content;
            // the else-branch's didOpen call would be redundant.
        } else if let (Some(lang), Some(uri)) = (new_language, new_uri.as_ref())
            && let Some(lsp) = self.lsp_clients.get(&lang)
        {
            let _ = lsp.did_open(uri, lang.lsp_language_id(), &new_buffer.rope().to_string());
        }

        self.buffer = new_buffer;
        self.syntax = new_syntax;
        self.git_status = new_git_status;
        self.lsp_uri = new_uri;
        self.language = new_language;
        self.lsp_version = 0;
        self.view = View::new();
        // Opening a path-bearing file lands in View, matching the
        // `App::new(path)` constructor. Without this, starting from
        // `dyad <dir>` (Edit mode for the empty scratch buffer) and
        // then picking a file in the tree leaves the user in Edit.
        self.mode = Mode::View;
        // Keep the tree's cursor in sync with whatever file is now
        // loaded so a subsequent Ctrl-T lands on it without a second
        // navigation step. No-op when the file is outside the tree
        // root or hidden by the dot-prefix filter.
        if let Some(p) = self.buffer.path() {
            self.tree.reveal(p);
        }
        Ok(())
    }
}

/// Map a porcelain (X, Y) status pair to a display group. `?` in
/// column X means untracked (it always pairs with `?` in Y in v1).
fn classify(staged: char, unstaged: char) -> GitGroup {
    if staged == '?' {
        return GitGroup::Untracked;
    }
    let staged_changed = staged != ' ';
    let unstaged_changed = unstaged != ' ';
    match (staged_changed, unstaged_changed) {
        (true, true) => GitGroup::Both,
        (true, false) => GitGroup::Staged,
        (false, true) => GitGroup::Modified,
        // Shouldn't reach here in porcelain output (rows with both
        // columns ' ' are unchanged and omitted), but classify as
        // Modified to keep the entry visible if it ever does.
        (false, false) => GitGroup::Modified,
    }
}

/// Return the word at the view's cursor — the maximal run of
/// alphanumerics + `_` straddling the cursor's char index. Returns
/// an empty string if the cursor isn't touching a word.
fn word_at_cursor(buffer: &Buffer, view: &View) -> String {
    let idx = view.char_idx(buffer);
    let total = buffer.len_chars();
    let rope = buffer.rope();
    let mut start = idx;
    while start > 0 {
        let c = rope.char(start - 1);
        if !is_word_char(c) {
            break;
        }
        start -= 1;
    }
    let mut end = idx;
    while end < total {
        let c = rope.char(end);
        if !is_word_char(c) {
            break;
        }
        end += 1;
    }
    if start == end {
        return String::new();
    }
    rope.slice(start..end).chars().collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Walk `root` recursively and collect non-hidden source files,
/// skipping common build / vendor directories. Returns paths relative
/// to `root`, sorted, and capped at 20k entries so a monorepo scan
/// can't blow up memory.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= 20_000 {
            break;
        }
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in reader.filter_map(|r| r.ok()) {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if matches!(
                name.as_str(),
                "target" | "node_modules" | "dist" | "build" | "vendor" | "venv" | "__pycache__"
            ) {
                continue;
            }
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Per-query upper bound on text-search results. A 2-char query
/// against a vendored monorepo could hit hundreds of thousands of
/// lines; we cap so the dialog stays usable and the walk returns in
/// bounded time.
pub const TEXT_SEARCH_MAX_HITS: usize = 500;
/// Per-file byte cap. Files bigger than this are skipped entirely —
/// they're usually generated, vendored, or binary, and not what the
/// user is grepping for. Matches the spirit of `walk_files` skipping
/// `target/` and `node_modules/`.
const TEXT_SEARCH_MAX_FILE_BYTES: u64 = 1_000_000;
/// Width budget for the line preview shown in each match row. Long
/// lines truncate so each result still fits on a single row.
const TEXT_SEARCH_PREVIEW_CHARS: usize = 200;

/// Walk `root` recursively (same skip-list as `walk_files`) and
/// collect case-insensitive substring matches of `query`, one entry
/// per matching line. Returns `(hits, capped)`; `capped` is `true`
/// when we hit `TEXT_SEARCH_MAX_HITS` and stopped scanning.
fn grep_under_root(root: &Path, query: &str) -> (Vec<TextHit>, bool) {
    let mut hits: Vec<TextHit> = Vec::new();
    if query.is_empty() {
        return (hits, false);
    }
    let needle = query.to_lowercase();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut capped = false;
    'walk: while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in reader.filter_map(|r| r.ok()) {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if matches!(
                name.as_str(),
                "target" | "node_modules" | "dist" | "build" | "vendor" | "venv" | "__pycache__"
            ) {
                continue;
            }
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // Cheap skip: stat once, drop oversized / unstattable
            // entries before opening them.
            let meta = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() || meta.len() > TEXT_SEARCH_MAX_FILE_BYTES {
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| p.clone());
            let Ok(contents) = std::fs::read_to_string(&p) else {
                // Non-UTF-8: skip silently rather than surface a per-file
                // error in the dialog.
                continue;
            };
            for (line_idx, line) in contents.lines().enumerate() {
                let Some(byte_pos) = find_ignore_ascii_case(line, &needle) else {
                    continue;
                };
                let col = line[..byte_pos].chars().count();
                hits.push(TextHit {
                    path: rel.clone(),
                    line: line_idx,
                    col,
                    preview: preview_line(line),
                });
                if hits.len() >= TEXT_SEARCH_MAX_HITS {
                    capped = true;
                    break 'walk;
                }
            }
        }
    }
    // File-then-line is the order users expect in a grep result list.
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    (hits, capped)
}

/// Case-insensitive substring search returning the byte offset of
/// the first occurrence. `needle` is assumed already lowercased so
/// we don't re-allocate it per line. ASCII-folded only — full Unicode
/// case folding can wait until users start grepping non-ASCII source.
fn find_ignore_ascii_case(haystack: &str, lowercase_needle: &str) -> Option<usize> {
    if lowercase_needle.is_empty() {
        return Some(0);
    }
    let n = lowercase_needle.len();
    if haystack.len() < n {
        return None;
    }
    let needle_bytes = lowercase_needle.as_bytes();
    let bytes = haystack.as_bytes();
    let last = bytes.len() - n;
    for i in 0..=last {
        let mut matched = true;
        for j in 0..n {
            if bytes[i + j].eq_ignore_ascii_case(&needle_bytes[j]) {
                continue;
            }
            matched = false;
            break;
        }
        if matched && haystack.is_char_boundary(i) {
            return Some(i);
        }
    }
    None
}

/// Trim leading whitespace and cap the line length so each match
/// renders on a single row. Trailing whitespace is also trimmed.
fn preview_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() > TEXT_SEARCH_PREVIEW_CHARS {
        trimmed.chars().take(TEXT_SEARCH_PREVIEW_CHARS).collect()
    } else {
        trimmed.to_string()
    }
}

/// Substring match (case-insensitive) against the full relative
/// path. Score = position of the first match (earlier = better);
/// stable-sort by score, so an empty query keeps the source order
/// from `walk_files`.
fn filter_files(files: &[PathBuf], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len()).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(usize, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let s = p.to_string_lossy().to_lowercase();
            s.find(&q).map(|pos| (i, pos))
        })
        .collect();
    scored.sort_by_key(|&(_, pos)| pos);
    scored.into_iter().map(|(i, _)| i).collect()
}

/// True when `s` looks like a bare `crate::module::Item` path — used
/// to detect rust-analyzer's "I only know the enclosing scope" hover
/// reply for struct-field definitions. Any whitespace or type-syntax
/// punctuation (`<`, `(`, `{`) means we already have something real.
fn looks_like_path_only(s: &str) -> bool {
    s.contains("::")
        && !s.contains(' ')
        && !s.contains('<')
        && !s.contains('(')
        && !s.contains('{')
}

/// Pull a Rust-style `: Type` annotation out of the source `line`,
/// stopping at the first `,`, `=`, or `{` that terminates the type.
/// Returns `None` when the line has no colon at all (typical for
/// expression lines that don't declare anything).
fn type_from_source_line(buffer: &Buffer, line: usize) -> Option<String> {
    if line >= buffer.line_count() {
        return None;
    }
    let mut text: String = buffer.line(line).chars().collect();
    if text.ends_with('\n') {
        text.pop();
    }
    if text.ends_with('\r') {
        text.pop();
    }
    let colon = text.find(':')?;
    // `::` is a path separator, not a type annotation. Skip past
    // runs of consecutive colons (the first `:` in `Vec::new`)
    // before we start carving up the right side.
    let after_colon = text[colon..]
        .find(|c: char| c != ':')
        .map(|off| colon + off)?;
    let after = text[after_colon..].trim_start();
    let end = after.find([',', '=', '{']).unwrap_or(after.len());
    let ty = after[..end].trim_end();
    if ty.is_empty() {
        None
    } else {
        Some(ty.to_string())
    }
}

/// Collapse a rust-analyzer hover markdown payload to the signature
/// line we want in the status bar. The hover layout puts namespace /
/// owning-impl info in the first code block and the actual type or
/// fn signature in a later block; we return the last non-empty
/// fenced block (joined to a single line). If the payload has no
/// fenced blocks at all — some hovers are plain text — we fall back
/// to the first non-empty line.
fn extract_signature(text: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if let Some(buf) = current.take() {
                blocks.push(buf);
            } else {
                current = Some(String::new());
            }
            continue;
        }
        if let Some(buf) = current.as_mut() {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(line.trim());
        }
    }
    if let Some(buf) = current.take() {
        blocks.push(buf);
    }
    if let Some(last) = blocks
        .into_iter()
        .rev()
        .map(|b| b.trim().to_string())
        .find(|b| !b.is_empty())
    {
        return Some(last);
    }
    text.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// Mirror of `load_diff_for_cursor` for the history view.
fn load_commit_for_cursor(view: &mut HistoryView) {
    let Some(entry) = view.entries.get(view.cursor) else {
        view.commit_lines = Vec::new();
        return;
    };
    match git::show_commit(&view.repo_root, &entry.sha) {
        Ok(text) => {
            view.commit_lines = text.lines().map(|l| l.to_string()).collect();
        }
        Err(e) => {
            view.commit_lines = vec![format!("git show failed: {e}")];
        }
    }
}

/// Refresh `view.diff_lines` for the file currently under
/// `view.cursor`. Errors stash a single one-line message into the
/// diff so the user always sees *something* in the right pane.
fn load_diff_for_cursor(view: &mut GitView) {
    let Some(file) = view.files.get(view.cursor) else {
        view.diff_lines = Vec::new();
        return;
    };
    let untracked = file.group == GitGroup::Untracked;
    match git::diff_for_path(&view.repo_root, &file.path, untracked) {
        Ok(text) => {
            view.diff_lines = text.lines().map(|l| l.to_string()).collect();
        }
        Err(e) => {
            view.diff_lines = vec![format!("git diff failed: {e}")];
        }
    }
}

/// Strip the `file://` scheme and return the underlying filesystem path,
/// `None` for any URI we can't parse trivially. (Full RFC 8089 handling
/// can wait — rust-analyzer only emits plain `file://` URIs.)
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// LSP `SymbolKind` values that represent a Rust-style type the user
/// would expect in a "go to type" dialog: struct, enum, trait,
/// type-alias, and class-like wrappers other servers use. Source:
/// LSP spec SymbolKind enum.
fn is_type_kind(kind: u32) -> bool {
    matches!(
        kind,
        5  // Class
        | 10 // Enum
        | 11 // Interface (rust-analyzer maps trait → Interface)
        | 19 // Object (used for type aliases by some servers)
        | 23 // Struct
        | 26 // TypeParameter
    )
}

/// Returns `(clients, uri, attempted)`. `attempted` is `true` whenever
/// the file was in a language we know how to spawn an LSP for, so the
/// UI can distinguish "spawn failed" (red badge) from "we didn't try"
/// (no badge). The map is empty unless spawn succeeded.
///
/// Step 3 still gates spawning to `Language::Rust` only — step 5 lifts
/// this so Metals fires for `.scala` files too.
fn spawn_lsp(buffer: &Buffer) -> (HashMap<Language, LspClient>, Option<String>, bool) {
    let mut clients = HashMap::new();
    let Some(path) = buffer.path() else {
        return (clients, None, false);
    };
    let Some(language) = Language::for_path(path) else {
        return (clients, None, false);
    };
    let uri = lsp::path_to_uri(path);
    let workspace = lsp::workspace_root_for(path, language);
    match LspClient::spawn(language, &workspace, &uri, &buffer.rope().to_string()) {
        Ok(client) => {
            clients.insert(language, client);
            (clients, Some(uri), true)
        }
        // Fail-graceful: the TUI runs without LSP-backed features.
        Err(_) => (clients, None, true),
    }
}

fn action_intent(action: &Action) -> Option<String> {
    match action {
        Action::Insert(c) => Some(format!("insert {}", describe_char(*c))),
        Action::DeletePrev => Some("delete backward".into()),
        Action::DeleteNext => Some("delete forward".into()),
        Action::ClearLine => Some("clear line".into()),
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
        | Action::GoToLine
        | Action::GoBack
        | Action::CtrlGPrefix
        | Action::ToggleTree
        | Action::ToggleGitDiff
        | Action::ToggleHistory
        | Action::ToggleKeysHelp
        | Action::OpenFile
        | Action::OpenTypeSearch
        | Action::OpenTextSearch
        | Action::NewFile
        | Action::ToggleAutosave
        | Action::ToggleMode
        | Action::ShowType
        | Action::Rename
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_signature_picks_last_code_block() {
        // rust-analyzer hover payload: first block is the path,
        // second block is the actual signature.
        let hover = "\
```rust
test_crate::main
```

```rust
let x: i32
```
";
        assert_eq!(extract_signature(hover).as_deref(), Some("let x: i32"));
    }

    #[test]
    fn extract_signature_handles_multi_line_blocks() {
        let hover = "\
```rust
fn foo(
    a: i32,
) -> i32
```
";
        assert_eq!(
            extract_signature(hover).as_deref(),
            Some("fn foo( a: i32, ) -> i32")
        );
    }

    #[test]
    fn extract_signature_falls_back_to_plain_text() {
        assert_eq!(
            extract_signature("just a plain line").as_deref(),
            Some("just a plain line")
        );
    }

    #[test]
    fn looks_like_path_only_flags_bare_paths() {
        assert!(looks_like_path_only("dyad::app::App"));
        assert!(looks_like_path_only("core::option::Option"));
        // Real signatures with structure should not match.
        assert!(!looks_like_path_only("Option<HistoryView>"));
        assert!(!looks_like_path_only("fn foo() -> i32"));
        assert!(!looks_like_path_only("let x: i32"));
        // Plain types without `::` shouldn't trigger the fallback —
        // they're already useful.
        assert!(!looks_like_path_only("i32"));
        assert!(!looks_like_path_only("App"));
    }

    #[test]
    fn type_from_source_line_extracts_field_type() {
        let buffer = buffer_with("    pub history: Option<HistoryView>,\n");
        assert_eq!(
            type_from_source_line(&buffer, 0).as_deref(),
            Some("Option<HistoryView>")
        );
    }

    #[test]
    fn type_from_source_line_extracts_let_binding() {
        let buffer = buffer_with("let x: i32 = 5;\n");
        assert_eq!(
            type_from_source_line(&buffer, 0).as_deref(),
            Some("i32")
        );
    }

    #[test]
    fn type_from_source_line_returns_none_for_expression_only() {
        let buffer = buffer_with("foo(bar);\n");
        assert_eq!(type_from_source_line(&buffer, 0), None);
    }

    fn buffer_with(text: &str) -> Buffer {
        let mut b = Buffer::scratch();
        b.insert_str(0, text);
        b
    }

    #[test]
    fn find_ignore_ascii_case_matches_mixed_case() {
        assert_eq!(find_ignore_ascii_case("Hello World", "world"), Some(6));
        assert_eq!(find_ignore_ascii_case("HELLO world", "hello"), Some(0));
        assert_eq!(find_ignore_ascii_case("nope", "yes"), None);
    }

    #[test]
    fn preview_line_trims_and_caps() {
        assert_eq!(preview_line("    let x = 1;   "), "let x = 1;");
        let long: String = "a".repeat(TEXT_SEARCH_PREVIEW_CHARS + 50);
        let prev = preview_line(&long);
        assert_eq!(prev.chars().count(), TEXT_SEARCH_PREVIEW_CHARS);
    }

    /// Make a fresh test root under `std::env::temp_dir()` and return
    /// its path. The label keeps collisions from concurrent tests
    /// readable; PID + nanos make it actually unique.
    fn fresh_test_root(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "dyad-grep-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn grep_under_root_finds_hits_and_orders_by_path() {
        let root = fresh_test_root("hits");
        std::fs::write(root.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("b.rs"), "// alpha lives here\nfn other() {}\n")
            .unwrap();
        // Should be skipped: hidden dir, vendored dir, hidden file.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "alpha").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target").join("junk.rs"), "fn alpha() {}").unwrap();
        std::fs::write(root.join(".secret"), "alpha").unwrap();

        let (hits, capped) = grep_under_root(&root, "alpha");
        assert!(!capped);
        // file-then-line ordering, no skipped-dir entries.
        let paths: Vec<PathBuf> = hits.iter().map(|h| h.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("a.rs"), PathBuf::from("sub").join("b.rs")]);
        assert_eq!(hits[0].line, 0);
        assert_eq!(hits[0].col, 3);
        assert!(hits[0].preview.contains("alpha"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn grep_under_root_is_case_insensitive() {
        let root = fresh_test_root("case");
        std::fs::write(root.join("a.rs"), "Hello World\n").unwrap();
        let (hits, _) = grep_under_root(&root, "WORLD");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].col, 6);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Build an App on a temp file with a `.txt` extension so we
    /// sidestep LSP spawn and language detection — just a buffer with
    /// a real path.
    fn app_on_temp_txt(label: &str, contents: &str) -> (App, PathBuf) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "dyad-mode-{label}-{}-{nanos}.txt",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        let app = App::new(path.clone()).unwrap();
        (app, path)
    }

    #[test]
    fn opens_file_in_view_mode_by_default() {
        let (app, path) = app_on_temp_txt("default-view", "hello\n");
        assert_eq!(app.mode, Mode::View);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn view_mode_blocks_insert_and_preserves_buffer() {
        let (mut app, path) = app_on_temp_txt("block-insert", "hello\n");
        assert_eq!(app.mode, Mode::View);
        let before = app.buffer.rope().to_string();
        let before_version = app.buffer.version();
        app.apply(Action::Insert('x')).unwrap();
        // Buffer text and version untouched — the gate fired before any
        // mutation could open a transaction.
        assert_eq!(app.buffer.rope().to_string(), before);
        assert_eq!(app.buffer.version(), before_version);
        assert!(!app.buffer.is_dirty());
        assert!(app.status.contains("View mode"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn view_mode_blocks_delete_and_clear_line() {
        let (mut app, path) = app_on_temp_txt("block-del", "hello\nworld\n");
        let before = app.buffer.rope().to_string();
        app.apply(Action::DeleteNext).unwrap();
        app.apply(Action::DeletePrev).unwrap();
        app.apply(Action::ClearLine).unwrap();
        assert_eq!(app.buffer.rope().to_string(), before);
        assert!(!app.buffer.is_dirty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn toggle_mode_flips_view_and_edit() {
        let (mut app, path) = app_on_temp_txt("toggle", "hello\n");
        assert_eq!(app.mode, Mode::View);
        app.apply(Action::ToggleMode).unwrap();
        assert_eq!(app.mode, Mode::Edit);
        app.apply(Action::ToggleMode).unwrap();
        assert_eq!(app.mode, Mode::View);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn edit_mode_allows_insert() {
        let (mut app, path) = app_on_temp_txt("edit-allows", "hello\n");
        app.apply(Action::ToggleMode).unwrap();
        assert_eq!(app.mode, Mode::Edit);
        app.apply(Action::Insert('X')).unwrap();
        // Cursor starts at (0, 0); 'X' inserts at the very beginning.
        assert!(app.buffer.rope().to_string().starts_with('X'));
        assert!(app.buffer.is_dirty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn view_mode_allows_movement_and_save() {
        let (mut app, path) = app_on_temp_txt("view-nav", "ab\ncd\n");
        // Movement should work freely in View mode.
        app.apply(Action::MoveDown).unwrap();
        app.apply(Action::MoveRight).unwrap();
        assert_eq!(app.view.cursor_line(), 1);
        assert_eq!(app.view.cursor_col(), 1);
        // Save passes through — the buffer wasn't dirty, but the action
        // shouldn't be blocked.
        app.apply(Action::Save).unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn grep_under_root_skips_files_over_size_cap() {
        let root = fresh_test_root("size");
        let mut big = String::with_capacity(TEXT_SEARCH_MAX_FILE_BYTES as usize + 100);
        big.push_str("needle\n");
        big.push_str(&"x".repeat(TEXT_SEARCH_MAX_FILE_BYTES as usize + 50));
        std::fs::write(root.join("big.bin"), big).unwrap();
        std::fs::write(root.join("small.txt"), "needle here\n").unwrap();
        let (hits, _) = grep_under_root(&root, "needle");
        let paths: Vec<PathBuf> = hits.iter().map(|h| h.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("small.txt")]);
        std::fs::remove_dir_all(&root).ok();
    }
}
