//! Phase 4–8 — the protocol layer.
//!
//! `ProtocolState` is the agent-facing *view* over a shared
//! [`EditorCore`] (in `core.rs`). It holds an `Arc<Mutex<EditorCore>>`
//! plus this client's stable id and forwards each DESIGN.md verb to the
//! locked core; the editor-as-runtime state (buffers, `TxManager`,
//! `LspClient`s, the proposal queue) lives in the core so that the TUI
//! (`App`) can become a second view over the same instance — the Phase 4
//! daemon split (`PLAN.md`). `mcp.rs` is one transport over this surface
//! and tests call the methods directly.
//!
//! Edits go through transactions. With no explicit `tx.begin` open, an
//! edit auto-opens / auto-commits a one-shot transaction so the flat
//! history still gets an entry. Phase 8's `edit_rename_symbol` runs a
//! *per-buffer* auto-tx for each affected buffer — true cross-buffer
//! atomicity is deferred until cross-buffer transactions exist.
//!
//! The protocol DTOs and the free helper functions (`apply_text_edits`,
//! `lsp_pos_to_char`, the context-pack geometry, …) stay in this module
//! — they're the protocol's public vocabulary, shared by `core.rs`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ropey::Rope;
use serde::{Deserialize, Serialize};

use crate::buffer::Buffer;
use crate::core::EditorCore;
use crate::git;
use crate::lsp::{self, Diagnostic, Location, SymbolInformation, TextEdit};
use crate::proposals::{Proposal, ProposalId};
use crate::syntax::AstMatch;
use crate::test_runner::TestResults;
use crate::tx::{Change, ChangeId, TxId};

/// The first buffer always gets this id, so back-compat with the
/// pre-Phase-8 single-buffer tests holds.
pub const SOLE_BUFFER_ID: u64 = 1;

/// The agent-facing view over a shared [`EditorCore`]. Holds the
/// `Arc<Mutex<EditorCore>>` plus this client's stable id, and forwards
/// every protocol verb to the locked core. One transport (`mcp.rs`)
/// drives this today; the TUI will become a second view over the same
/// core in a later step of Phase 4.
pub struct ProtocolState {
    core: Arc<Mutex<EditorCore>>,
    /// Stable identifier for this client (the current MCP session).
    /// Surfaced via `clients.list`.
    client_id: String,
}

impl ProtocolState {
    pub fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            core: Arc::new(Mutex::new(EditorCore::open(path)?)),
            client_id: format!("mcp-{}", std::process::id()),
        })
    }

    /// Lock the shared core. Every forwarding method goes through here;
    /// the guard is dropped at the end of the statement, so methods that
    /// call other `ProtocolState` methods (e.g. `proposal_accept`) never
    /// hold the lock across a nested call.
    fn core(&self) -> MutexGuard<'_, EditorCore> {
        self.core.lock().expect("editor core lock poisoned")
    }

    // ---------- Buffers ----------

    pub fn buffer_open(&mut self, path: PathBuf) -> Result<u64> {
        self.core().buffer_open(path)
    }

    pub fn buffer_close(&mut self, buffer_id: u64) -> Result<()> {
        self.core().buffer_close(buffer_id)
    }

    pub fn buffer_list(&self) -> Vec<BufferListEntry> {
        self.core().buffer_list()
    }

    pub fn buffer_read(
        &self,
        buffer_id: u64,
        range: Option<CharRange>,
    ) -> Result<BufferReadResponse> {
        self.core().buffer_read(buffer_id, range)
    }

    // ---------- AST ----------

    pub fn ast_query(&self, buffer_id: u64, query: &str) -> Result<Vec<AstMatch>> {
        self.core().ast_query(buffer_id, query)
    }

    // ---------- Edits ----------

    pub fn edit_replace_range(
        &mut self,
        buffer_id: u64,
        version: u64,
        range: CharRange,
        text: &str,
    ) -> Result<u64> {
        self.core().edit_replace_range(buffer_id, version, range, text)
    }

    pub fn edit_replace_node(
        &mut self,
        buffer_id: u64,
        version: u64,
        byte_range: ByteRange,
        text: &str,
    ) -> Result<u64> {
        self.core()
            .edit_replace_node(buffer_id, version, byte_range, text)
    }

    // ---------- Transactions ----------

    pub fn tx_begin(
        &mut self,
        buffer_id: u64,
        intent: String,
        conversation_id: Option<String>,
    ) -> Result<TxId> {
        self.core().tx_begin(buffer_id, intent, conversation_id)
    }

    pub fn tx_commit(&mut self, tx_id: TxId) -> Result<ChangeId> {
        self.core().tx_commit(tx_id)
    }

    pub fn tx_rollback(&mut self, tx_id: TxId) -> Result<()> {
        self.core().tx_rollback(tx_id)
    }

    // ---------- History ----------

    pub fn history_recent(&self, limit: usize) -> Vec<Change> {
        self.core().history_recent(limit)
    }

    // ---------- Clients ----------

    pub fn clients_list(&self) -> Vec<ClientInfo> {
        // Phase 8 baseline: just the current MCP session. Awareness of a
        // concurrent TUI client comes after the planned daemon split.
        vec![ClientInfo {
            id: self.client_id.clone(),
            kind: "agent".into(),
            focus: self.core().focus(),
        }]
    }

    // ---------- Semantic (LSP) ----------

    pub fn symbol_definition(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        self.core().symbol_definition(buffer_id, line, character)
    }

    pub fn symbol_references(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        self.core()
            .symbol_references(buffer_id, line, character, include_declaration)
    }

    pub fn symbol_hover(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<Option<String>> {
        self.core().symbol_hover(buffer_id, line, character)
    }

    pub fn symbol_workspace_search(
        &self,
        buffer_id: u64,
        query: &str,
    ) -> Result<Vec<SymbolInformation>> {
        self.core().symbol_workspace_search(buffer_id, query)
    }

    pub fn diag_current(&self, buffer_id: u64) -> Result<Vec<Diagnostic>> {
        self.core().diag_current(buffer_id)
    }

    pub fn diag_wait_until_idle(
        &self,
        buffer_id: u64,
        timeout: Duration,
    ) -> Result<DiagWaitResult> {
        self.core().diag_wait_until_idle(buffer_id, timeout)
    }

    pub fn edit_rename_symbol(
        &mut self,
        buffer_id: u64,
        version: u64,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        self.core()
            .edit_rename_symbol(buffer_id, version, line, character, new_name)
    }

    // ---------- Proposals (Phase 10) ----------

    pub fn propose_replace_range(
        &mut self,
        buffer_id: u64,
        version: u64,
        range: CharRange,
        text: String,
        intent: String,
    ) -> Result<ProposalId> {
        self.core()
            .propose_replace_range(buffer_id, version, range, text, intent)
    }

    pub fn proposals_list(&self) -> Vec<Proposal> {
        self.core().proposals_list()
    }

    pub fn proposal_accept(&mut self, id: ProposalId) -> Result<u64> {
        self.core().proposal_accept(id)
    }

    pub fn proposal_reject(&mut self, id: ProposalId) -> Result<()> {
        self.core().proposal_reject(id)
    }

    pub fn proposals_accept_all(&mut self) -> BulkAcceptResult {
        self.core().proposals_accept_all()
    }

    pub fn proposals_reject_all(&mut self) -> usize {
        self.core().proposals_reject_all()
    }

    pub fn proposals_count(&self) -> usize {
        self.core().proposals_count()
    }

    // ---------- Git (Phase 9) ----------

    pub fn git_diff(&self, buffer_id: u64) -> Result<String> {
        self.core().git_diff(buffer_id)
    }

    pub fn git_status(&self, buffer_id: u64) -> Result<Vec<git::StatusEntry>> {
        self.core().git_status(buffer_id)
    }

    pub fn git_log(&self, buffer_id: u64, limit: usize) -> Result<Vec<git::LogEntry>> {
        self.core().git_log(buffer_id, limit)
    }

    pub fn git_show(&self, buffer_id: u64, sha: &str) -> Result<String> {
        self.core().git_show(buffer_id, sha)
    }

    pub fn git_stage(&self, buffer_id: u64, path: Option<&str>) -> Result<()> {
        self.core().git_stage(buffer_id, path)
    }

    pub fn git_unstage(&self, buffer_id: u64, path: Option<&str>) -> Result<()> {
        self.core().git_unstage(buffer_id, path)
    }

    pub fn git_commit(&self, buffer_id: u64, message: &str) -> Result<String> {
        self.core().git_commit(buffer_id, message)
    }

    // ---------- Tests ----------

    pub fn test_run(&mut self, buffer_id: u64, target: Option<&str>) -> Result<TestResults> {
        self.core().test_run(buffer_id, target)
    }

    pub fn test_last_results(&self) -> Option<TestResults> {
        self.core().test_last_results()
    }

    // ---------- Scope / Context ----------

    pub fn scope_imports(&self, buffer_id: u64) -> Result<Vec<ImportEntry>> {
        self.core().scope_imports(buffer_id)
    }

    pub fn scope_in_scope(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<ScopeReport> {
        self.core().scope_in_scope(buffer_id, line, character)
    }

    pub fn context_pack(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
        token_budget: usize,
    ) -> Result<PackedContext> {
        self.core()
            .context_pack(buffer_id, line, character, token_budget)
    }

    // ---------- Inline agent tasks ----------

    pub fn tasks_list(&self, buffer_id: u64) -> Result<Vec<InlineTask>> {
        self.core().tasks_list(buffer_id)
    }

    // ---------- Read-only accessors ----------

    pub fn buffer_version(&self, buffer_id: u64) -> Result<u64> {
        self.core().buffer_version(buffer_id)
    }

    #[allow(dead_code)] // surfaced through clients_list; kept on the view for parity with the core.
    pub fn focus(&self) -> Option<u64> {
        self.core().focus()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BufferListEntry {
    pub id: u64,
    pub path: Option<String>,
    pub dirty: bool,
    pub version: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BufferReadResponse {
    pub text: String,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct CharRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RenameApplied {
    pub buffer_id: u64,
    pub uri: String,
    pub edits: usize,
    pub new_version: u64,
}

/// Outcome of `edit_rename_symbol`. `applied` lists every loaded
/// buffer whose changes we wrote (with the per-buffer edit count and
/// new version). `skipped_files` are URIs the LSP server wanted to
/// touch but which aren't currently loaded as buffers — the agent
/// must `buffer.open` them and re-target (Phase 8 keeps rename
/// per-buffer; cross-buffer atomic txes are a follow-up).
#[derive(Clone, Debug, Serialize)]
pub struct RenameResult {
    pub applied: Vec<RenameApplied>,
    pub skipped_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientInfo {
    pub id: String,
    /// "agent" for an MCP session, "human" once a TUI client lands.
    pub kind: String,
    pub focus: Option<u64>,
}

/// Outcome of `diag.wait_until_idle`. `caught_up` is `false` when the
/// wait timed out before the server published a fresh diagnostics frame
/// (or — for languages that report it — finished indexing); the
/// `diagnostics` payload is still the most recent the server gave us.
#[derive(Clone, Debug, Serialize)]
pub struct DiagWaitResult {
    pub caught_up: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// One occurrence of an inline agent-task marker (`// CLAUDE: ...` or
/// `// TODO(claude): ...`) discovered by `tasks.list`. The path is
/// relative to the scan root so it can be rendered without leaking
/// absolute filesystem prefixes.
#[derive(Clone, Debug, Serialize)]
pub struct InlineTask {
    pub path: String,
    pub line: usize,
    pub kind: String,
    pub text: String,
}

/// One import declaration found by `scope.imports`. `text` is the
/// declaration's source verbatim; `line` is its 0-indexed start line.
/// LSP-free — sourced from a Tree-sitter query against the cached tree.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ImportEntry {
    pub text: String,
    pub line: usize,
}

/// A symbol referenced by `scope.in_scope` — a flattened
/// [`lsp::DocumentSymbol`] without its children. `kind` is the LSP
/// `SymbolKind` integer (5 = Class, 12 = Function, …).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SymbolRef {
    pub name: String,
    pub kind: u32,
    pub range: lsp::Range,
}

/// What's visible at a point, per `scope.in_scope`. `enclosing` runs
/// outer→inner (e.g. `mod` → `impl` → `fn`); `siblings` are the other
/// top-level symbols in the file; `imports` are the file's import
/// declarations. `locals` (params / `let` bindings via a Tree-sitter
/// scope walk) is deferred — see `PLAN.md` Phase 2.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScopeReport {
    pub enclosing: Vec<SymbolRef>,
    pub imports: Vec<ImportEntry>,
    pub siblings: Vec<SymbolRef>,
}

/// One slice of packed context. `range` is in the buffer's native
/// coordinates (line + char offset within the line, *not* UTF-16 — that
/// convention is confined to the LSP layer). `reason` says why it was
/// included (`"anchor: enclosing function"`, `"import"`,
/// `"sibling signature"`); `estimated_tokens` is the cheap `chars / 4`
/// heuristic the packer budgeted against.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ContextSlice {
    pub path: Option<String>,
    pub range: lsp::Range,
    pub reason: String,
    pub text: String,
    pub estimated_tokens: usize,
}

/// Result of `context.pack`: a budget-bounded bundle of source slices
/// assembled around a point, anchored on the enclosing function.
/// `anchor` is that function's range (or `None` if the point isn't
/// inside one). `truncated` is **always** reported — `true` means the
/// budget stopped us before every candidate slice was included, so the
/// agent knows the pack is partial (never a silent drop). `estimated_
/// tokens` is the summed `chars / 4` estimate of the included slices.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PackedContext {
    pub anchor: Option<lsp::Range>,
    pub slices: Vec<ContextSlice>,
    pub estimated_tokens: usize,
    pub truncated: bool,
}

impl SymbolRef {
    pub(crate) fn from_doc_symbol(sym: &lsp::DocumentSymbol) -> Self {
        Self {
            name: sym.name.clone(),
            kind: sym.kind,
            range: sym.range,
        }
    }
}

/// Per-proposal failure surfaced by `proposals.accept_all`. The
/// proposal is re-queued under a fresh id (the same way a single
/// `proposal_accept` re-queues), so the agent can `proposals.list` to
/// find it.
#[derive(Clone, Debug, Serialize)]
pub struct BulkAcceptError {
    pub proposal_id: ProposalId,
    pub message: String,
}

/// Outcome of `proposals.accept_all`. `accepted` is the count that
/// landed cleanly; `errors` lists the ones that didn't.
#[derive(Clone, Debug, Serialize)]
pub struct BulkAcceptResult {
    pub accepted: usize,
    pub errors: Vec<BulkAcceptError>,
}


/// Apply a list of LSP `TextEdit`s to a buffer. Caller is responsible
/// for sorting the edits end-to-start so earlier indices stay valid.
pub(crate) fn apply_text_edits(buffer: &mut Buffer, edits: &[TextEdit]) -> Result<()> {
    for edit in edits {
        let start = lsp_pos_to_char(buffer, edit.range.start.line, edit.range.start.character)?;
        let end = lsp_pos_to_char(buffer, edit.range.end.line, edit.range.end.character)?;
        if start < end {
            buffer.delete_range(start..end);
        }
        if !edit.new_text.is_empty() {
            buffer.insert_str(start, &edit.new_text);
        }
    }
    Ok(())
}

/// Per-scan upper bound on inline-task hits. A repo with thousands of
/// vendored TODOs shouldn't be able to balloon a single MCP response —
/// agents that need more granularity can scan a subdirectory.
const TASKS_MAX_HITS: usize = 1000;
/// Per-file byte cap. Files bigger than this are skipped; they're
/// usually generated or vendored and not where inline agent intent
/// lives. Matches the spirit of the TUI's text-search cap.
const TASKS_MAX_FILE_BYTES: u64 = 1_000_000;

/// Walk `root` recursively (skipping dotfiles and the usual vendored /
/// build directories) and collect lines that contain either `CLAUDE:`
/// or `TODO(claude)` (case-insensitive on the keyword). Results are
/// sorted by path, then line. Capped at `TASKS_MAX_HITS`.
pub(crate) fn scan_inline_tasks(root: &Path) -> Vec<InlineTask> {
    let mut hits: Vec<InlineTask> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
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
            let meta = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(p);
                continue;
            }
            if !meta.is_file() || meta.len() > TASKS_MAX_FILE_BYTES {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&p) else {
                continue;
            };
            let rel = p
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| p.clone());
            for (idx, line) in contents.lines().enumerate() {
                let Some(parsed) = parse_inline_task(line) else {
                    continue;
                };
                hits.push(InlineTask {
                    path: rel.display().to_string(),
                    line: idx,
                    kind: parsed.0,
                    text: parsed.1,
                });
                if hits.len() >= TASKS_MAX_HITS {
                    break 'walk;
                }
            }
        }
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    hits
}

/// Cap on sibling-signature candidates `context.pack` assembles, so a
/// large file can't balloon candidate construction. The token budget
/// bounds what's *included*; this bounds what's *considered*.
pub(crate) const CONTEXT_MAX_SIBLINGS: usize = 50;

/// Cheap token estimate — roughly 4 chars per token. A real tokenizer
/// is out of scope for v0; `context.pack` returns this estimate so the
/// agent can recalibrate against its own model.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Convert a byte offset to a buffer-native [`lsp::Position`] (line +
/// char offset within the line — not UTF-16). Clamps past-EOF offsets.
fn byte_to_position(rope: &Rope, byte: usize) -> lsp::Position {
    let byte = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(byte);
    let line_start = rope.byte_to_char(rope.line_to_byte(line));
    let character = rope.byte_to_char(byte) - line_start;
    lsp::Position {
        line: line as u32,
        character: character as u32,
    }
}

pub(crate) fn byte_span_to_range(rope: &Rope, start: usize, end: usize) -> lsp::Range {
    lsp::Range {
        start: byte_to_position(rope, start),
        end: byte_to_position(rope, end),
    }
}

/// Byte offset of the first newline in `[start, end)`, or `end` if the
/// span is single-line. Used to trim a top-level item down to its
/// signature line.
pub(crate) fn first_line_end_byte(rope: &Rope, start: usize, end: usize) -> usize {
    let text = rope
        .slice(rope.byte_to_char(start)..rope.byte_to_char(end))
        .to_string();
    match text.find('\n') {
        Some(idx) => start + idx,
        None => end,
    }
}

/// Build a [`ContextSlice`] for the byte span `[start, end)`.
pub(crate) fn make_context_slice(
    rope: &Rope,
    path: Option<String>,
    start: usize,
    end: usize,
    reason: &str,
) -> ContextSlice {
    let text = rope
        .slice(rope.byte_to_char(start)..rope.byte_to_char(end))
        .to_string();
    ContextSlice {
        path,
        range: byte_span_to_range(rope, start, end),
        reason: reason.to_string(),
        estimated_tokens: estimate_tokens(&text),
        text,
    }
}

/// Does `range` cover the zero-based `(line, character)` point? Used by
/// `scope_in_scope` to find enclosing symbols. Inclusive at both ends
/// so a point sitting exactly on a symbol's closing brace still counts
/// as inside it.
pub(crate) fn range_contains(range: &lsp::Range, line: u32, character: u32) -> bool {
    let after_start = line > range.start.line
        || (line == range.start.line && character >= range.start.character);
    let before_end = line < range.end.line
        || (line == range.end.line && character <= range.end.character);
    after_start && before_end
}

/// Look for `CLAUDE:` or `TODO(claude)` in a single line. Returns
/// `(kind, body)` on match — `kind` is `"claude"` or `"todo"`; body is
/// the trimmed text after the marker. `TODO(claude)` wins over a bare
/// `CLAUDE:` on the same line so the more specific shape gets the
/// `todo` tag.
fn parse_inline_task(line: &str) -> Option<(String, String)> {
    let lower = line.to_ascii_lowercase();
    if let Some(start) = lower.find("todo(claude)") {
        let after = &line[start + "todo(claude)".len()..];
        let body = after
            .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
            .trim()
            .to_string();
        return Some(("todo".to_string(), body));
    }
    if let Some(start) = lower.find("claude:") {
        let after = &line[start + "claude:".len()..];
        let body = after.trim().to_string();
        return Some(("claude".to_string(), body));
    }
    None
}

/// Convert an LSP `(line, character)` position into a rope char index.
/// LSP characters are UTF-16 code units in the spec; we treat them as
/// rope chars, which is exact for BMP-only source and off-by-one per
/// non-BMP code point otherwise.
pub(crate) fn lsp_pos_to_char(buffer: &Buffer, line: u32, character: u32) -> Result<usize> {
    let line = line as usize;
    let character = character as usize;
    if line >= buffer.line_count() {
        return Err(anyhow!(
            "lsp position line {} out of bounds (line_count = {})",
            line,
            buffer.line_count()
        ));
    }
    let line_start = buffer.line_to_char(line);
    let line_len = buffer.line_len_chars(line);
    if character > line_len {
        return Err(anyhow!(
            "lsp position character {} past end of line {} (len = {})",
            character,
            line,
            line_len
        ));
    }
    Ok(line_start + character)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_state(name: &str) -> ProtocolState {
        let path = std::env::temp_dir()
            .join(format!("dyad_proto_{}_{}.rs", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut state = ProtocolState::open(path).unwrap();
        let v0 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v0,
                CharRange { start: 0, end: 0 },
                "fn hello() {}\n",
            )
            .unwrap();
        state
    }

    fn text_of(state: &ProtocolState, id: u64) -> String {
        state.buffer_read(id, None).unwrap().text
    }

    #[test]
    fn buffer_list_reports_the_sole_buffer() {
        let state = scratch_state("list");
        let list = state.buffer_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, SOLE_BUFFER_ID);
        assert!(list[0].dirty);
    }

    #[test]
    fn buffer_read_returns_full_or_partial_text() {
        let state = scratch_state("read");
        let full = state.buffer_read(SOLE_BUFFER_ID, None).unwrap();
        assert_eq!(full.text, "fn hello() {}\n");
        let part = state
            .buffer_read(SOLE_BUFFER_ID, Some(CharRange { start: 3, end: 8 }))
            .unwrap();
        assert_eq!(part.text, "hello");
    }

    #[test]
    fn edit_replace_range_requires_matching_version() {
        let mut state = scratch_state("version_check");
        let stale = state.buffer_version(SOLE_BUFFER_ID).unwrap() + 1;
        let err = state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                stale,
                CharRange { start: 0, end: 0 },
                "x",
            )
            .unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn ast_query_returns_function_names() {
        let state = scratch_state("ast_query");
        let matches = state
            .ast_query(
                SOLE_BUFFER_ID,
                "(function_item name: (identifier) @name)",
            )
            .unwrap();
        let names: Vec<&str> = matches
            .iter()
            .filter(|m| m.capture == "name")
            .map(|m| m.kind.as_str())
            .collect();
        assert!(!names.is_empty(), "expected at least one name match");
    }

    #[test]
    fn edit_replace_node_via_protocol_renames_function() {
        let mut state = scratch_state("replace_node");
        let matches = state
            .ast_query(
                SOLE_BUFFER_ID,
                "(function_item name: (identifier) @name)",
            )
            .unwrap();
        let target = matches.into_iter().find(|m| m.capture == "name").unwrap();
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_node(
                SOLE_BUFFER_ID,
                v,
                ByteRange {
                    start: target.byte_start,
                    end: target.byte_end,
                },
                "farewell",
            )
            .unwrap();
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), "fn farewell() {}\n");
    }

    #[test]
    fn explicit_tx_commit_creates_one_history_entry_for_two_edits() {
        let mut state = scratch_state("explicit_tx");
        let tx_id = state
            .tx_begin(SOLE_BUFFER_ID, "multi-edit refactor".to_string(), None)
            .unwrap();
        let v1 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v1,
                CharRange { start: 0, end: 0 },
                "// intro\n",
            )
            .unwrap();
        let v2 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let end = text_of(&state, SOLE_BUFFER_ID).chars().count();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v2,
                CharRange { start: end, end },
                "// outro\n",
            )
            .unwrap();
        let change_id = state.tx_commit(tx_id).unwrap();
        let history = state.history_recent(10);
        // The seed edit + this multi-edit tx = 2 entries.
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].change_id, change_id);
        assert_eq!(history[1].intent, "multi-edit refactor");
    }

    #[test]
    fn apply_text_edits_rewrites_multiple_ranges_end_to_start() {
        use crate::lsp::{Position, Range, TextEdit};

        let path = std::env::temp_dir()
            .join(format!("dyad_apply_edits_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut buf = Buffer::open(path).unwrap();
        buf.insert_str(0, "fn old() { let old = 1; }\n");

        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position { line: 0, character: 15 },
                    end:   Position { line: 0, character: 18 },
                },
                new_text: "fresh".into(),
            },
            TextEdit {
                range: Range {
                    start: Position { line: 0, character: 3 },
                    end:   Position { line: 0, character: 6 },
                },
                new_text: "fresh".into(),
            },
        ];
        apply_text_edits(&mut buf, &edits).unwrap();
        assert_eq!(buf.rope().to_string(), "fn fresh() { let fresh = 1; }\n");
    }

    #[test]
    fn lsp_pos_to_char_maps_line_and_column() {
        let path = std::env::temp_dir()
            .join(format!("dyad_lsp_pos_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut buf = Buffer::open(path).unwrap();
        buf.insert_str(0, "ab\ncde\n");
        assert_eq!(lsp_pos_to_char(&buf, 0, 1).unwrap(), 1);
        assert_eq!(lsp_pos_to_char(&buf, 1, 2).unwrap(), 5);
        assert!(lsp_pos_to_char(&buf, 9, 0).is_err());
    }

    #[test]
    fn tx_rollback_restores_buffer_and_invalidates_syntax() {
        let mut state = scratch_state("rollback");
        let pre_text = text_of(&state, SOLE_BUFFER_ID);
        let tx_id = state
            .tx_begin(SOLE_BUFFER_ID, "doomed".to_string(), None)
            .unwrap();
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 0, end: 0 },
                "garbage ",
            )
            .unwrap();
        state.tx_rollback(tx_id).unwrap();
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), pre_text);
        let matches = state
            .ast_query(
                SOLE_BUFFER_ID,
                "(function_item name: (identifier) @name)",
            )
            .unwrap();
        assert!(matches.iter().any(|m| m.capture == "name"));
    }

    // ---------- Phase 8 multi-buffer ----------

    #[test]
    fn buffer_open_returns_ascending_ids_and_list_reflects_them() {
        let mut state = scratch_state("multi_open");
        let path_b = std::env::temp_dir()
            .join(format!("dyad_proto_multi_open_b_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path_b);
        let id_b = state.buffer_open(path_b).unwrap();
        let path_c = std::env::temp_dir()
            .join(format!("dyad_proto_multi_open_c_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path_c);
        let id_c = state.buffer_open(path_c).unwrap();

        assert_eq!(id_b, 2);
        assert_eq!(id_c, 3);
        let ids: Vec<u64> = state.buffer_list().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(state.focus(), Some(id_c));
    }

    #[test]
    fn buffer_close_removes_entry_and_shifts_focus() {
        let mut state = scratch_state("multi_close");
        let path_b = std::env::temp_dir()
            .join(format!("dyad_proto_multi_close_b_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path_b);
        let id_b = state.buffer_open(path_b).unwrap();
        assert_eq!(state.focus(), Some(id_b));

        state.buffer_close(id_b).unwrap();
        assert_eq!(state.focus(), Some(SOLE_BUFFER_ID));
        assert_eq!(state.buffer_list().len(), 1);

        // Closing the last buffer leaves focus None.
        state.buffer_close(SOLE_BUFFER_ID).unwrap();
        assert_eq!(state.focus(), None);
        assert!(state.buffer_list().is_empty());
    }

    #[test]
    fn clients_list_returns_the_mcp_session() {
        let state = scratch_state("clients");
        let clients = state.clients_list();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].kind, "agent");
        assert_eq!(clients[0].focus, Some(SOLE_BUFFER_ID));
    }

    // ---------- Phase 10 proposals ----------

    #[test]
    fn proposal_accept_applies_the_edit_and_carries_intent_into_history() {
        let mut state = scratch_state("propose_accept");
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let id = state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 3, end: 8 },
                "farewell".into(),
                "rename hello -> farewell".into(),
            )
            .unwrap();
        // Queue has one entry.
        assert_eq!(state.proposals_list().len(), 1);

        state.proposal_accept(id).unwrap();
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), "fn farewell() {}\n");
        // Queue drains after accept.
        assert!(state.proposals_list().is_empty());

        let history = state.history_recent(10);
        let last = history.last().unwrap();
        assert_eq!(last.intent, "rename hello -> farewell");
    }

    #[test]
    fn proposal_reject_discards_without_applying() {
        let mut state = scratch_state("propose_reject");
        let pre = text_of(&state, SOLE_BUFFER_ID);
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let id = state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 0, end: 0 },
                "// proposed\n".into(),
                "add a doc comment".into(),
            )
            .unwrap();
        state.proposal_reject(id).unwrap();
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), pre);
        assert!(state.proposals_list().is_empty());
    }

    #[test]
    fn proposal_accept_on_stale_version_errors_and_requeues() {
        let mut state = scratch_state("propose_stale");
        let stale = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let id = state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                stale,
                CharRange { start: 0, end: 0 },
                "x".into(),
                "stale insert".into(),
            )
            .unwrap();
        // Move the buffer forward, invalidating the proposal's version.
        let v_now = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(SOLE_BUFFER_ID, v_now, CharRange { start: 0, end: 0 }, "y")
            .unwrap();

        let err = state.proposal_accept(id).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
        // The proposal stays in the queue under a NEW id so the agent
        // can `proposals.list` and decide what to do.
        let still_queued = state.proposals_list();
        assert_eq!(still_queued.len(), 1);
        assert_ne!(still_queued[0].id, id);
    }

    #[test]
    fn scope_imports_lists_use_declarations() {
        let mut state = scratch_state("scope_imports");
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let len = text_of(&state, SOLE_BUFFER_ID).chars().count();
        // Replace the seeded `fn hello() {}` with a file that has two
        // top-level `use`s and a function.
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 0, end: len },
                "use std::fmt;\nuse std::io::Read;\nfn main() {}\n",
            )
            .unwrap();
        let imports = state.scope_imports(SOLE_BUFFER_ID).unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].text, "use std::fmt;");
        assert_eq!(imports[0].line, 0);
        assert_eq!(imports[1].text, "use std::io::Read;");
        assert_eq!(imports[1].line, 1);
    }

    #[test]
    fn scope_imports_errors_for_unrecognized_language() {
        let path = std::env::temp_dir()
            .join(format!("dyad_scope_unknown_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = ProtocolState::open(path).unwrap();
        let err = state.scope_imports(SOLE_BUFFER_ID).unwrap_err();
        assert!(
            err.to_string().contains("recognized language"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn range_contains_is_inclusive_and_line_aware() {
        let r = lsp::Range {
            start: lsp::Position { line: 1, character: 4 },
            end: lsp::Position { line: 3, character: 2 },
        };
        // Inside on an interior line, regardless of column.
        assert!(range_contains(&r, 2, 0));
        // On the boundary lines, the column gates membership.
        assert!(range_contains(&r, 1, 4));
        assert!(!range_contains(&r, 1, 3));
        assert!(range_contains(&r, 3, 2));
        assert!(!range_contains(&r, 3, 3));
        // Outside entirely.
        assert!(!range_contains(&r, 0, 9));
        assert!(!range_contains(&r, 4, 0));
    }

    // Live `scope.in_scope` against rust-analyzer — ignored by default
    // (needs the binary on PATH and a workspace index). Exercises the
    // documentSymbol enclosing/sibling walk; the LSP-free `imports` side
    // is covered hermetically above.
    #[test]
    #[ignore = "requires rust-analyzer + workspace index; run explicitly"]
    fn scope_in_scope_reports_enclosing_function() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        let state = ProtocolState::open(manifest).unwrap();
        // Give the server a moment to index before asking for symbols.
        let _ = state.diag_wait_until_idle(SOLE_BUFFER_ID, Duration::from_secs(30));
        let report = state.scope_in_scope(SOLE_BUFFER_ID, 0, 0).unwrap();
        // main.rs has top-level items; at the very top we should see them
        // as siblings (or one as enclosing if it spans line 0).
        assert!(
            !report.siblings.is_empty() || !report.enclosing.is_empty(),
            "expected some document symbols from main.rs",
        );
    }

    /// Build a fresh single-buffer state whose sole buffer holds `src`,
    /// with syntax refreshed. LSP-free — fine for the tree-sitter paths.
    fn rust_state_with(name: &str, src: &str) -> ProtocolState {
        let mut state = scratch_state(name);
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let len = text_of(&state, SOLE_BUFFER_ID).chars().count();
        state
            .edit_replace_range(SOLE_BUFFER_ID, v, CharRange { start: 0, end: len }, src)
            .unwrap();
        state
    }

    const PACK_FIXTURE: &str = "\
use std::fmt;
use std::io::Read;

struct Helper;

fn target() {
    let x = 1;
    let y = 2;
}

fn neighbor() {}
";

    #[test]
    fn context_pack_anchors_on_enclosing_function_first() {
        let state = rust_state_with("pack_anchor", PACK_FIXTURE);
        // Point inside `target` (line 6 is `let x = 1;`).
        let packed = state.context_pack(SOLE_BUFFER_ID, 6, 8, 1000).unwrap();
        assert!(!packed.truncated, "1000 tokens easily fits this fixture");
        // Anchor is `target`'s range and is the first slice.
        assert!(packed.anchor.is_some());
        assert_eq!(packed.slices[0].reason, "anchor: enclosing function");
        assert!(packed.slices[0].text.contains("fn target()"));
        assert!(packed.slices[0].text.contains("let y = 2;"));

        // The two imports and the sibling signatures (struct Helper, fn
        // neighbor) come after the anchor, never duplicated.
        let reasons: Vec<&str> = packed.slices.iter().map(|s| s.reason.as_str()).collect();
        assert_eq!(reasons.iter().filter(|r| **r == "import").count(), 2);
        let sib_text: Vec<&str> = packed
            .slices
            .iter()
            .filter(|s| s.reason == "sibling signature")
            .map(|s| s.text.as_str())
            .collect();
        assert!(sib_text.iter().any(|t| t.contains("struct Helper")));
        assert!(sib_text.iter().any(|t| t.contains("fn neighbor()")));
        // Sibling signatures are single-line (no body).
        assert!(sib_text.iter().all(|t| !t.contains('\n')));
        // estimated_tokens is the sum of included slices.
        let summed: usize = packed.slices.iter().map(|s| s.estimated_tokens).sum();
        assert_eq!(packed.estimated_tokens, summed);
    }

    #[test]
    fn context_pack_flags_truncation_on_a_tiny_budget() {
        let state = rust_state_with("pack_trunc", PACK_FIXTURE);
        // A budget of 1 token can't fit anything beyond the always-kept
        // anchor, so packing stops after it and flags truncation.
        let packed = state.context_pack(SOLE_BUFFER_ID, 6, 8, 1).unwrap();
        assert!(packed.truncated);
        assert_eq!(packed.slices.len(), 1);
        assert_eq!(packed.slices[0].reason, "anchor: enclosing function");
    }

    #[test]
    fn context_pack_without_enclosing_function_packs_imports() {
        let state = rust_state_with("pack_toplevel", PACK_FIXTURE);
        // Line 0 is `use std::fmt;` — not inside any function.
        let packed = state.context_pack(SOLE_BUFFER_ID, 0, 0, 1000).unwrap();
        assert!(packed.anchor.is_none());
        assert!(!packed.slices.is_empty());
        // First slice is an import (no anchor to lead with).
        assert_eq!(packed.slices[0].reason, "import");
    }

    #[test]
    fn estimate_tokens_is_chars_over_four_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn diag_wait_until_idle_errors_for_buffer_without_lsp() {
        // The scratch state opens a .rs path that never went through
        // rust-analyzer (no binary on test PATH most of the time), so
        // there's no client to wait on. The protocol method should
        // surface that as an error instead of hanging.
        let path = std::env::temp_dir()
            .join(format!("dyad_diag_wait_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = ProtocolState::open(path).unwrap();
        let err = state
            .diag_wait_until_idle(SOLE_BUFFER_ID, Duration::from_millis(50))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("recognized language")
                || msg.contains("not running")
                || msg.contains("no recognized"),
            "unexpected error: {msg}",
        );
    }

    #[test]
    fn test_last_results_empty_until_a_run_happens() {
        let state = scratch_state("test_cache");
        assert!(state.test_last_results().is_none());
    }

    #[test]
    fn test_run_errors_for_unrecognized_language() {
        // A .txt buffer has no `Language`, so there's no runner to pick.
        let path = std::env::temp_dir()
            .join(format!("dyad_test_run_unknown_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut state = ProtocolState::open(path).unwrap();
        let err = state.test_run(SOLE_BUFFER_ID, None).unwrap_err();
        assert!(
            err.to_string().contains("language is unknown"),
            "unexpected error: {err}",
        );
    }

    // Live `cargo test` run — ignored by default because it shells out
    // to cargo (and would recurse if pointed at this very crate). The
    // parser is covered hermetically in `test_runner`; this just proves
    // the protocol plumbing and the result cache against a real run.
    // Exercised by `scripts/mcp-smoke.sh`.
    #[test]
    #[ignore = "spawns a live `cargo test`; run explicitly"]
    fn test_run_populates_the_cache() {
        // Point at this crate's manifest dir so cargo has a workspace.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/main.rs");
        let mut state = ProtocolState::open(manifest).unwrap();
        assert!(state.test_last_results().is_none());
        let results = state
            .test_run(SOLE_BUFFER_ID, Some("parse_summary_line_unreachable_filter"))
            .unwrap();
        // The filter matches nothing, so zero tests run but the call
        // still succeeds and caches.
        assert_eq!(results.failed, 0);
        assert!(state.test_last_results().is_some());
    }

    #[test]
    fn parse_inline_task_recognizes_todo_and_claude_markers() {
        assert_eq!(
            parse_inline_task("// CLAUDE: rename this to Foo"),
            Some(("claude".into(), "rename this to Foo".into())),
        );
        assert_eq!(
            parse_inline_task("# claude: drop the prefix"),
            Some(("claude".into(), "drop the prefix".into())),
        );
        assert_eq!(
            parse_inline_task("// TODO(claude): refactor"),
            Some(("todo".into(), "refactor".into())),
        );
        assert_eq!(
            parse_inline_task("/* todo(Claude) plain body */"),
            Some(("todo".into(), "plain body */".into())),
        );
        assert_eq!(parse_inline_task("nothing here"), None);
        // `TODO(claude)` wins over a coincidental `claude:` later in the line.
        assert_eq!(
            parse_inline_task("// TODO(claude): mention claude: in body"),
            Some(("todo".into(), "mention claude: in body".into())),
        );
    }

    #[test]
    fn scan_inline_tasks_finds_markers_recursively_and_sorts_results() {
        let root = std::env::temp_dir().join(format!(
            "dyad_inline_tasks_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(
            root.join("a.rs"),
            "fn a() {}\n// CLAUDE: rename a -> b\nfn b() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("nested/b.rs"),
            "// TODO(claude): refactor this\nfn x() {}\n",
        )
        .unwrap();
        // Should be skipped (dotfile + ignored dir).
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/skipme.rs"), "// CLAUDE: ignored").unwrap();
        std::fs::write(root.join(".hidden"), "// CLAUDE: also ignored").unwrap();

        let hits = scan_inline_tasks(&root);
        let labels: Vec<(String, usize, String)> = hits
            .into_iter()
            .map(|h| (h.path, h.line, h.kind))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("a.rs".to_string(), 1, "claude".to_string()),
                ("nested/b.rs".to_string(), 0, "todo".to_string()),
            ]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn proposals_accept_all_drains_queue_and_applies_in_id_order() {
        let mut state = scratch_state("accept_all");
        // Two proposals, both valid against the current version.
        let v0 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let _p1 = state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                v0,
                CharRange { start: 0, end: 0 },
                "// first\n".into(),
                "first".into(),
            )
            .unwrap();
        // p2 targets a version we don't yet have — accept_all should
        // re-queue this one and keep going with the rest.
        let _p2 = state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                v0 + 999,
                CharRange { start: 0, end: 0 },
                "// second\n".into(),
                "second".into(),
            )
            .unwrap();

        let result = state.proposals_accept_all();
        // p1 lands; p2 fails with a version error and is re-queued.
        assert_eq!(result.accepted, 1);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].message.contains("version mismatch"));
        let remaining = state.proposals_list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].intent, "second");
        assert!(text_of(&state, SOLE_BUFFER_ID).starts_with("// first\n"));
    }

    #[test]
    fn proposals_reject_all_drains_queue_without_applying() {
        let mut state = scratch_state("reject_all");
        let pre = text_of(&state, SOLE_BUFFER_ID);
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        for i in 0..3 {
            state
                .propose_replace_range(
                    SOLE_BUFFER_ID,
                    v,
                    CharRange { start: 0, end: 0 },
                    format!("// {i}\n"),
                    format!("noise {i}"),
                )
                .unwrap();
        }
        assert_eq!(state.proposals_count(), 3);
        let dropped = state.proposals_reject_all();
        assert_eq!(dropped, 3);
        assert_eq!(state.proposals_count(), 0);
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), pre);
    }

    #[test]
    fn edits_are_isolated_per_buffer() {
        let mut state = scratch_state("isolated");
        let path_b = std::env::temp_dir()
            .join(format!("dyad_proto_isolated_b_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path_b);
        let id_b = state.buffer_open(path_b).unwrap();
        let v_b = state.buffer_version(id_b).unwrap();
        state
            .edit_replace_range(id_b, v_b, CharRange { start: 0, end: 0 }, "fn other() {}\n")
            .unwrap();

        // Buffer 1 still has its original text; buffer id_b has its own.
        assert_eq!(text_of(&state, SOLE_BUFFER_ID), "fn hello() {}\n");
        assert_eq!(text_of(&state, id_b), "fn other() {}\n");
    }
}
