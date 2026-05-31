//! Phase 4 — `EditorCore`: the shared editor runtime.
//!
//! Owns the editor-as-runtime state — the collection of buffers (each
//! with its own `Syntax` + LSP URI + LSP version), the shared
//! `TxManager`, the per-language `LspClient`s, the proposal queue, and
//! the last test results. This is the single owner that both the TUI
//! (`App`) and the agent surface (`ProtocolState`) are meant to become
//! *views* over — the daemon split tracked in `ROADMAP.md` (Phase 4).
//!
//! It's wrapped in `Arc<Mutex<…>>` by its holders. Today exactly one
//! client drives a given core at a time (an MCP session *or* the TUI),
//! so the lock is uncontended; concurrent TUI+MCP access — and lifting
//! the per-client `focus`/`explicit_tx` out of here — lands with the
//! later steps of the phase.
//!
//! `ProtocolState` (in `protocol.rs`) is the thin agent-facing view: it
//! holds an `Arc<Mutex<EditorCore>>` plus the per-client `client_id` and
//! forwards each protocol verb to the locked core. The protocol DTOs and
//! the free helper functions stay in `protocol.rs`; the editing logic
//! lives here.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::buffer::Buffer;
use crate::git;
use crate::language::Language;
use crate::lsp::{self, Diagnostic, Location, LspClient, SymbolInformation};
use crate::proposals::{PendingProposal, Proposal, ProposalId, ProposalKind, ProposalQueue};
use crate::protocol::{
    BufferListEntry, BufferReadResponse, BulkAcceptError, BulkAcceptResult, ByteRange,
    CONTEXT_MAX_SIBLINGS, CharRange, ContextSlice, DiagWaitResult, ImportEntry, InlineTask,
    PackedContext, RenameApplied, RenameResult, SOLE_BUFFER_ID, ScopeReport, SymbolRef,
    apply_text_edits, byte_span_to_range, first_line_end_byte, lsp_pos_to_char, make_context_slice,
    range_contains, scan_inline_tasks,
};
use crate::syntax::{AstMatch, HighlightSpan, Syntax};
use crate::test_runner::{self, TestResults};
use crate::tx::{Change, ChangeId, TxId, TxManager};

pub(crate) struct EditorCore {
    next_buffer_id: u64,
    buffers: HashMap<u64, BufferEntry>,
    /// Which buffer the current client considers "focused". Surfaced via
    /// `clients.list` (through `ProtocolState`). Updates whenever a
    /// buffer is opened or the focused buffer is closed. Per-client in
    /// the eventual multi-client design; shared here while exactly one
    /// client drives the core.
    focus: Option<u64>,
    tx_manager: TxManager,
    /// At most one explicit transaction at a time; the buffer it targets
    /// is recorded so per-buffer edits know whether to auto-tx.
    explicit_tx: Option<(TxId, u64)>,
    /// One LSP client per language. The first buffer opened in a
    /// supported language spawns it lazily; subsequent buffers in the
    /// same language reuse the client via additional `didOpen`
    /// notifications. Polyglot sessions can host rust-analyzer and
    /// Metals side-by-side.
    lsp_clients: HashMap<Language, LspClient>,
    /// Workspace root resolved at spawn time, kept per language so the
    /// same dyad session can host (e.g.) a Rust workspace at `repo/`
    /// and a Scala workspace at `repo/scala/`.
    workspace_roots: HashMap<Language, PathBuf>,
    /// Phase 10 — agent-submitted edits awaiting accept/reject.
    proposals: ProposalQueue,
    /// Most recent `test.run` outcome, served by `test.last_results` so
    /// an agent (or a second client) can read the last verification
    /// without re-running the suite.
    last_test_results: Option<TestResults>,
}

struct BufferEntry {
    id: u64,
    buffer: Buffer,
    syntax: Option<Syntax>,
    uri: Option<String>,
    /// Cached at open time — used to route LSP traffic to the right
    /// client. `None` for scratch buffers and unrecognized extensions.
    language: Option<Language>,
    /// Per-buffer monotonic LSP document version (LSP needs an i32
    /// starting at 0 and incrementing on each `didChange`).
    lsp_version: i32,
}

/// One screen row's worth of rendered text for the TUI (Phase 4 step
/// 1b scaffolding). Owned — text collected and highlight spans cloned
/// — so the renderer can drop the core lock before walking ratatui
/// widgets. `App` becomes the production caller when it turns into a
/// view over the shared core; today this module's tests exercise it.
#[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
#[derive(Default)]
pub(crate) struct RenderLine {
    pub text: String,
    pub spans: Vec<HighlightSpan>,
}

/// Per-buffer metadata for the TUI status line and gutter geometry,
/// snapshotted so the renderer needn't hold the lock (Phase 4 step 1b).
#[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
pub(crate) struct BufferMeta {
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub line_count: usize,
    pub version: u64,
}

impl EditorCore {
    /// Open `path` as the sole buffer and return the populated core.
    /// `ProtocolState::open` / `App::new` wrap the result in an
    /// `Arc<Mutex<…>>`.
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut state = Self {
            next_buffer_id: SOLE_BUFFER_ID,
            buffers: HashMap::new(),
            focus: None,
            tx_manager: TxManager::new(),
            explicit_tx: None,
            lsp_clients: HashMap::new(),
            workspace_roots: HashMap::new(),
            proposals: ProposalQueue::new(),
            last_test_results: None,
        };
        state.buffer_open(path)?;
        Ok(state)
    }

    // ---------- Buffers ----------

    /// Add a buffer to the protocol state. The first call returns
    /// `SOLE_BUFFER_ID` (=1); subsequent calls allocate fresh ids.
    /// For .rs files this also lazily spawns rust-analyzer for the
    /// workspace and forwards a `didOpen`.
    pub fn buffer_open(&mut self, path: PathBuf) -> Result<u64> {
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;

        let mut buffer = Buffer::open(path)?;
        let mut syntax = Syntax::for_path(buffer.path());
        if let Some(syn) = syntax.as_mut() {
            syn.refresh(&mut buffer);
        }
        let uri = buffer.path().map(lsp::path_to_uri);
        let language = buffer.path().and_then(Language::for_path);

        if let (Some(p), Some(u), Some(lang)) = (buffer.path(), uri.as_ref(), language) {
            self.ensure_lsp_for(lang, p, u, &buffer.rope().to_string());
        }

        self.buffers.insert(
            id,
            BufferEntry {
                id,
                buffer,
                syntax,
                uri,
                language,
                lsp_version: 0,
            },
        );
        self.focus = Some(id);
        Ok(id)
    }

    /// Remove a buffer. If the focused buffer is closed, focus moves
    /// to the lowest-id remaining buffer (or `None` if no buffers
    /// remain). LSP gets a best-effort `didClose`.
    pub fn buffer_close(&mut self, buffer_id: u64) -> Result<()> {
        let entry = self
            .buffers
            .remove(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
        if let (Some(lang), Some(uri)) = (entry.language, entry.uri.as_ref())
            && let Some(lsp) = self.lsp_clients.get(&lang)
        {
            let _ = lsp.did_close(uri);
        }
        if self.focus == Some(buffer_id) {
            self.focus = self.buffers.keys().min().copied();
        }
        Ok(())
    }

    pub fn buffer_list(&self) -> Vec<BufferListEntry> {
        let mut list: Vec<_> = self
            .buffers
            .values()
            .map(|e| BufferListEntry {
                id: e.id,
                path: e.buffer.path().map(|p| p.display().to_string()),
                dirty: e.buffer.is_dirty(),
                version: e.buffer.version(),
            })
            .collect();
        list.sort_by_key(|e| e.id);
        list
    }

    pub fn buffer_read(
        &self,
        buffer_id: u64,
        range: Option<CharRange>,
    ) -> Result<BufferReadResponse> {
        let entry = self.buffer_entry(buffer_id)?;
        let rope = entry.buffer.rope();
        let text = match range {
            None => rope.to_string(),
            Some(r) => {
                if r.start > r.end || r.end > rope.len_chars() {
                    return Err(anyhow!(
                        "range {}..{} outside buffer (len_chars = {})",
                        r.start,
                        r.end,
                        rope.len_chars()
                    ));
                }
                rope.slice(r.start..r.end).to_string()
            }
        };
        Ok(BufferReadResponse {
            text,
            version: entry.buffer.version(),
        })
    }

    // ---------- AST ----------

    pub fn ast_query(&self, buffer_id: u64, query: &str) -> Result<Vec<AstMatch>> {
        let entry = self.buffer_entry(buffer_id)?;
        let syn = entry
            .syntax
            .as_ref()
            .context("buffer has no syntax (unsupported language)")?;
        syn.ast_query(entry.buffer.rope(), query)
    }

    // ---------- Edits ----------

    pub fn edit_replace_range(
        &mut self,
        buffer_id: u64,
        version: u64,
        range: CharRange,
        text: &str,
    ) -> Result<u64> {
        self.check_version(buffer_id, version)?;
        {
            let entry = self.buffer_entry(buffer_id)?;
            if range.start > range.end || range.end > entry.buffer.len_chars() {
                return Err(anyhow!(
                    "range {}..{} outside buffer (len_chars = {})",
                    range.start,
                    range.end,
                    entry.buffer.len_chars()
                ));
            }
        }
        let intent = format!("edit.replace_range {}..{}", range.start, range.end);
        let text_owned = text.to_string();
        self.with_auto_tx_on(buffer_id, intent, move |entry| {
            if range.start < range.end {
                entry.buffer.delete_range(range.start..range.end);
            }
            if !text_owned.is_empty() {
                entry.buffer.insert_str(range.start, &text_owned);
            }
            Ok(())
        })?;
        self.refresh_syntax(buffer_id);
        self.notify_lsp_changed(buffer_id);
        Ok(self.buffer_entry(buffer_id)?.buffer.version())
    }

    pub fn edit_replace_node(
        &mut self,
        buffer_id: u64,
        version: u64,
        byte_range: ByteRange,
        text: &str,
    ) -> Result<u64> {
        self.check_version(buffer_id, version)?;
        {
            let entry = self.buffer_entry(buffer_id)?;
            if byte_range.start > byte_range.end
                || byte_range.end > entry.buffer.rope().len_bytes()
            {
                return Err(anyhow!(
                    "byte range {}..{} outside buffer (len_bytes = {})",
                    byte_range.start,
                    byte_range.end,
                    entry.buffer.rope().len_bytes()
                ));
            }
        }
        let range = Range {
            start: byte_range.start,
            end: byte_range.end,
        };
        let intent = format!("edit.replace_node {}..{}", byte_range.start, byte_range.end);
        let text_owned = text.to_string();
        self.with_auto_tx_on(buffer_id, intent, move |entry| {
            entry.buffer.replace_node(range, &text_owned);
            Ok(())
        })?;
        self.refresh_syntax(buffer_id);
        self.notify_lsp_changed(buffer_id);
        Ok(self.buffer_entry(buffer_id)?.buffer.version())
    }

    // ---------- Transactions ----------

    pub fn tx_begin(
        &mut self,
        buffer_id: u64,
        intent: String,
        conversation_id: Option<String>,
    ) -> Result<TxId> {
        if self.explicit_tx.is_some() {
            return Err(anyhow!(
                "a transaction is already open; commit or rollback it first"
            ));
        }
        let entry = self
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
        let tx_id = self
            .tx_manager
            .begin(intent, conversation_id, &entry.buffer);
        self.explicit_tx = Some((tx_id, buffer_id));
        Ok(tx_id)
    }

    pub fn tx_commit(&mut self, tx_id: TxId) -> Result<ChangeId> {
        let buffer_id = self.tx_buffer_for(tx_id)?;
        let entry = self
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
        let change_id = self.tx_manager.commit(tx_id, &entry.buffer)?;
        self.explicit_tx = None;
        Ok(change_id)
    }

    pub fn tx_rollback(&mut self, tx_id: TxId) -> Result<()> {
        let buffer_id = self.tx_buffer_for(tx_id)?;
        {
            let entry = self
                .buffers
                .get_mut(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            self.tx_manager.rollback(tx_id, &mut entry.buffer)?;
            if let Some(syn) = entry.syntax.as_mut() {
                syn.invalidate();
            }
        }
        self.explicit_tx = None;
        self.refresh_syntax(buffer_id);
        self.notify_lsp_changed(buffer_id);
        Ok(())
    }

    // ---------- History ----------

    pub fn history_recent(&self, limit: usize) -> Vec<Change> {
        self.tx_manager.recent(limit).to_vec()
    }

    // ---------- Semantic (LSP) ----------

    pub fn symbol_definition(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        lsp.definition(uri, line, character)
    }

    /// All references to the symbol at `(line, character)`. The pair
    /// with `symbol_definition` — most agents call definition first,
    /// then references when they need to scope a rename / impact-analyze
    /// a change. `include_declaration` defaults to `true` so the
    /// definition shows up in the result alongside uses.
    pub fn symbol_references(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        lsp.references(uri, line, character, include_declaration)
    }

    /// Hover text for the symbol at `(line, character)`. Backs both the
    /// `symbol.hover` and `symbol.signature` MCP tools — LSP exposes one
    /// endpoint and the agent slices what it wants from the body.
    /// Returns `None` when the server has nothing to say.
    pub fn symbol_hover(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<Option<String>> {
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        lsp.hover(uri, line, character)
    }

    /// Run `workspace/symbol` against the LSP client serving
    /// `buffer_id`. `buffer_id` only picks the language server (the
    /// query itself is workspace-wide), so any buffer in a supported
    /// language works.
    pub fn symbol_workspace_search(
        &self,
        buffer_id: u64,
        query: &str,
    ) -> Result<Vec<SymbolInformation>> {
        let (lsp, _uri) = self.lsp_for_buffer(buffer_id)?;
        lsp.workspace_symbol(query)
    }

    pub fn diag_current(&self, buffer_id: u64) -> Result<Vec<Diagnostic>> {
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        Ok(lsp.diagnostics(uri))
    }

    /// Block until the LSP serving `buffer_id` has acknowledged the most
    /// recent sync for the buffer's URI with a `publishDiagnostics`, and
    /// (for languages that report indexing status) is no longer
    /// indexing. Returns `(caught_up, diagnostics)` — `caught_up` is
    /// `false` if the timeout fired first, in which case the cached
    /// diagnostics may still be stale.
    ///
    /// This is the edit-then-verify primitive: after an `edit.*` call,
    /// agents that want to know "did my edit introduce errors?" can
    /// call this instead of polling `diag.current` in a loop.
    pub fn diag_wait_until_idle(
        &self,
        buffer_id: u64,
        timeout: Duration,
    ) -> Result<DiagWaitResult> {
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        let caught_up = lsp.wait_until_idle(uri, timeout);
        let diagnostics = lsp.diagnostics(uri);
        Ok(DiagWaitResult {
            caught_up,
            diagnostics,
        })
    }

    /// Phase 7/8 tier-3 edit: ask rust-analyzer for the workspace edits
    /// required to rename the symbol at `(line, character)` to
    /// `new_name`, then apply the changes to every loaded buffer the
    /// server wants to touch (one per-buffer auto-tx each). URIs the
    /// server names but which aren't loaded come back in
    /// `skipped_files` — the agent can `buffer.open` them and re-run.
    ///
    /// LSP positions are line + UTF-16 code units. dyad's rope is char
    /// indexed (Unicode scalar values), so the conversion is exact for
    /// BMP-only source. Files with non-BMP characters will mis-position;
    /// that's a known limitation.
    pub fn edit_rename_symbol(
        &mut self,
        buffer_id: u64,
        version: u64,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        self.check_version(buffer_id, version)?;
        let (lsp, request_uri) = self.lsp_for_buffer(buffer_id)?;
        let request_uri = request_uri.to_string();
        let workspace_edit = lsp.rename(&request_uri, line, character, &new_name)?;

        let uri_to_bid: HashMap<String, u64> = self
            .buffers
            .values()
            .filter_map(|e| e.uri.as_ref().map(|u| (u.clone(), e.id)))
            .collect();

        let mut applied = Vec::new();
        let mut skipped_files = Vec::new();
        let mut affected_ids: Vec<u64> = Vec::new();

        for (edit_uri, edits) in &workspace_edit.changes {
            match uri_to_bid.get(edit_uri) {
                Some(&bid) => {
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
                    let intent = format!("edit.rename_symbol -> {new_name}");
                    let sorted_for_body = sorted.clone();
                    self.with_auto_tx_on(bid, intent, move |entry| {
                        apply_text_edits(&mut entry.buffer, &sorted_for_body)
                    })?;
                    affected_ids.push(bid);
                    let entry = self.buffer_entry(bid)?;
                    applied.push(RenameApplied {
                        buffer_id: bid,
                        uri: edit_uri.clone(),
                        edits: sorted.len(),
                        new_version: entry.buffer.version(),
                    });
                }
                None => skipped_files.push(edit_uri.clone()),
            }
        }

        for bid in affected_ids {
            self.refresh_syntax(bid);
            self.notify_lsp_changed(bid);
        }
        // Sort for stable output.
        applied.sort_by_key(|a| a.buffer_id);
        skipped_files.sort();
        Ok(RenameResult {
            applied,
            skipped_files,
        })
    }

    // ---------- Proposals (Phase 10) ----------

    /// Queue a deferred `edit.replace_range` against `buffer_id`. The
    /// version is *recorded*, not checked here — staleness is detected
    /// at `proposal_accept` time so the agent can author proposals
    /// against a snapshot the buffer may have moved past.
    pub fn propose_replace_range(
        &mut self,
        buffer_id: u64,
        version: u64,
        range: CharRange,
        text: String,
        intent: String,
    ) -> Result<ProposalId> {
        if !self.buffers.contains_key(&buffer_id) {
            return Err(anyhow!("unknown buffer_id {}", buffer_id));
        }
        Ok(self.proposals.enqueue(PendingProposal {
            buffer_id,
            intent,
            kind: ProposalKind::ReplaceRange {
                version,
                start: range.start,
                end: range.end,
                text,
            },
        }))
    }

    pub fn proposals_list(&self) -> Vec<Proposal> {
        self.proposals.list()
    }

    /// Pull the proposal out of the queue and run it through the same
    /// tx machinery a direct edit would use — the proposal's intent is
    /// the tx intent, so it lands in flat history with that string.
    /// Errors with the proposal *put back* if the buffer version moved.
    pub fn proposal_accept(&mut self, id: ProposalId) -> Result<u64> {
        let proposal = self
            .proposals
            .take(id)
            .ok_or_else(|| anyhow!("unknown proposal_id {:?}", id))?;
        match proposal.kind.clone() {
            ProposalKind::ReplaceRange {
                version,
                start,
                end,
                text,
            } => {
                // Open an explicit tx with the proposal's intent so the
                // history entry shows what the agent said, not the
                // synthetic auto-tx string.
                let tx_id = self
                    .tx_begin(proposal.buffer_id, proposal.intent.clone(), None)
                    .inspect_err(|_| {
                        // Re-queue so the caller can retry / reject.
                        self.proposals.enqueue(PendingProposal {
                            buffer_id: proposal.buffer_id,
                            intent: proposal.intent.clone(),
                            kind: proposal.kind.clone(),
                        });
                    })?;
                match self.edit_replace_range(
                    proposal.buffer_id,
                    version,
                    CharRange { start, end },
                    &text,
                ) {
                    Ok(new_version) => {
                        self.tx_commit(tx_id)?;
                        Ok(new_version)
                    }
                    Err(e) => {
                        let _ = self.tx_rollback(tx_id);
                        // Put the proposal back so a future retry sees it.
                        self.proposals.enqueue(PendingProposal {
                            buffer_id: proposal.buffer_id,
                            intent: proposal.intent.clone(),
                            kind: proposal.kind.clone(),
                        });
                        Err(e)
                    }
                }
            }
        }
    }

    pub fn proposal_reject(&mut self, id: ProposalId) -> Result<()> {
        self.proposals
            .take(id)
            .ok_or_else(|| anyhow!("unknown proposal_id {:?}", id))?;
        Ok(())
    }

    /// Accept every queued proposal in id order. Each accept goes
    /// through the same tx machinery as a single `proposal_accept`, so
    /// per-proposal failures (typically version mismatches) re-queue
    /// the offender and continue. Returns the number of successful
    /// accepts plus the list of `(id, error)` for any that failed —
    /// useful for "OK everything Claude proposed, tell me what didn't
    /// fit" review flows.
    pub fn proposals_accept_all(&mut self) -> BulkAcceptResult {
        let ids: Vec<ProposalId> = self.proposals.list().into_iter().map(|p| p.id).collect();
        let mut accepted = 0;
        let mut errors = Vec::new();
        for id in ids {
            match self.proposal_accept(id) {
                Ok(_) => accepted += 1,
                Err(e) => errors.push(BulkAcceptError {
                    proposal_id: id,
                    message: e.to_string(),
                }),
            }
        }
        BulkAcceptResult { accepted, errors }
    }

    /// Discard every queued proposal. Returns the number dropped.
    pub fn proposals_reject_all(&mut self) -> usize {
        let ids: Vec<ProposalId> = self.proposals.list().into_iter().map(|p| p.id).collect();
        let mut rejected = 0;
        for id in ids {
            if self.proposal_reject(id).is_ok() {
                rejected += 1;
            }
        }
        rejected
    }

    /// Number of proposals currently in the queue. Cheap status check
    /// for agents that don't want to pay the cost of a full
    /// `proposals.list` just to know whether anything is pending.
    pub fn proposals_count(&self) -> usize {
        self.proposals.count()
    }

    // ---------- Git (Phase 9) ----------

    /// Raw `git diff HEAD --no-color -- <path>` for the buffer's file.
    /// Returns `Err` when the file isn't tracked or git isn't usable.
    pub fn git_diff(&self, buffer_id: u64) -> Result<String> {
        let entry = self
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
        let path = entry
            .buffer
            .path()
            .context("buffer has no path; cannot diff against HEAD")?;
        git::diff_text(path)
    }

    /// `git status --porcelain=v1` for the repo containing the buffer's
    /// file. Returns all entries — caller filters if needed.
    pub fn git_status(&self, buffer_id: u64) -> Result<Vec<git::StatusEntry>> {
        let repo_root = self.repo_root_for_buffer(buffer_id)?;
        git::status_at(&repo_root)
    }

    /// Most recent `limit` commits in the repo containing the buffer's
    /// file. Errors when the repo has no commits yet — consistent with
    /// `git log`'s own behavior.
    pub fn git_log(&self, buffer_id: u64, limit: usize) -> Result<Vec<git::LogEntry>> {
        let repo_root = self.repo_root_for_buffer(buffer_id)?;
        git::log(&repo_root, limit)
    }

    /// Full `git show` output for a commit (SHA, ref, or short SHA —
    /// anything `git` itself accepts).
    pub fn git_show(&self, buffer_id: u64, sha: &str) -> Result<String> {
        let repo_root = self.repo_root_for_buffer(buffer_id)?;
        git::show_commit(&repo_root, sha)
    }

    /// Stage a file in the repo containing the buffer. When `path` is
    /// `None`, the buffer's own file is staged. When `Some`, the string
    /// is passed to `git add` as a path relative to the repo root.
    pub fn git_stage(&self, buffer_id: u64, path: Option<&str>) -> Result<()> {
        let (repo_root, rel) = self.stage_target(buffer_id, path)?;
        git::stage(&repo_root, &rel)
    }

    /// Unstage a file. Same path semantics as `git_stage`.
    pub fn git_unstage(&self, buffer_id: u64, path: Option<&str>) -> Result<()> {
        let (repo_root, rel) = self.stage_target(buffer_id, path)?;
        git::unstage(&repo_root, &rel)
    }

    /// Commit currently-staged changes in the repo containing the
    /// buffer. Returns `git commit`'s stdout (typically the summary
    /// line). Pre-commit hook failures and "nothing to commit" reach
    /// the caller as the error string.
    pub fn git_commit(&self, buffer_id: u64, message: &str) -> Result<String> {
        let repo_root = self.repo_root_for_buffer(buffer_id)?;
        git::commit(&repo_root, message)
    }

    // ---------- Tests ----------

    /// Run the buffer language's test suite (Rust → `cargo test`) with
    /// the language workspace root as the working directory, optionally
    /// filtered by `target` (a libtest name substring for cargo). The
    /// result is cached for `test_last_results`. Returns `Err` only when
    /// the language has no runner wired up, the buffer has no path, or
    /// the process can't be spawned — a test *failure* comes back as a
    /// successful call with `exit_ok: false`.
    pub fn test_run(&mut self, buffer_id: u64, target: Option<&str>) -> Result<TestResults> {
        let entry = self.buffer_entry(buffer_id)?;
        let path = entry
            .buffer
            .path()
            .context("buffer has no path; cannot locate a test workspace")?;
        let language = entry
            .language
            .context("buffer language is unknown; cannot run tests")?;
        let cmd = language.test_command();
        let kind = language.test_runner_kind();
        let (Some(cmd), Some(kind)) = (cmd, kind) else {
            return Err(anyhow!(
                "no test runner configured for {}",
                language.display_name()
            ));
        };
        let root = lsp::workspace_root_for(path, language);
        let results = test_runner::run(&root, cmd, kind, target)?;
        self.last_test_results = Some(results.clone());
        Ok(results)
    }

    /// The most recent `test_run` outcome, or `None` if no run has
    /// happened this session. Lets a second client read the last
    /// verification without paying to re-run the suite.
    pub fn test_last_results(&self) -> Option<TestResults> {
        self.last_test_results.clone()
    }

    // ---------- Scope ----------

    /// The import declarations in `buffer_id`, via a Tree-sitter query
    /// against the cached parse tree (LSP-free, so it works the instant
    /// a file is open). Matches all import declarations in the file —
    /// for Rust that includes `use`s nested inside items, which is the
    /// honest answer to "what names are imported here". Returns `Err`
    /// for languages with no grammar / import query.
    pub fn scope_imports(&self, buffer_id: u64) -> Result<Vec<ImportEntry>> {
        let entry = self.buffer_entry(buffer_id)?;
        let language = entry
            .language
            .context("buffer has no recognized language; cannot list imports")?;
        let query = language.import_query().with_context(|| {
            format!("no import query for {}", language.display_name())
        })?;
        let syn = entry
            .syntax
            .as_ref()
            .context("buffer has no syntax (unsupported language)")?;
        let rope = entry.buffer.rope();
        let mut out = Vec::new();
        for m in syn.ast_query(rope, query)? {
            if m.capture != "import" {
                continue;
            }
            let start_char = rope.byte_to_char(m.byte_start);
            let end_char = rope.byte_to_char(m.byte_end);
            out.push(ImportEntry {
                text: rope.slice(start_char..end_char).to_string(),
                line: rope.byte_to_line(m.byte_start),
            });
        }
        Ok(out)
    }

    /// What's in scope at `(line, character)`: the enclosing symbols
    /// (outer→inner), the file's imports, and the other top-level
    /// symbols (`siblings`). Enclosing + siblings come from LSP
    /// `documentSymbol` (so this needs a running server); imports are
    /// the LSP-free `scope_imports`. `locals` is deferred (see
    /// `ROADMAP.md`).
    pub fn scope_in_scope(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
    ) -> Result<ScopeReport> {
        // Imports first — LSP-free, and it validates the buffer/language
        // before we pay for an LSP round-trip.
        let imports = self.scope_imports(buffer_id)?;
        let (lsp, uri) = self.lsp_for_buffer(buffer_id)?;
        let symbols = lsp.document_symbols(uri)?;

        let mut enclosing = Vec::new();
        let mut siblings = Vec::new();
        let mut level: &[lsp::DocumentSymbol] = &symbols;
        let mut top = true;
        loop {
            // The deepest symbol on this level whose range covers the
            // point becomes the next enclosing frame; everything else at
            // the top level is a sibling.
            let mut enclosing_here: Option<&lsp::DocumentSymbol> = None;
            for sym in level {
                if range_contains(&sym.range, line, character) {
                    enclosing_here = Some(sym);
                } else if top {
                    siblings.push(SymbolRef::from_doc_symbol(sym));
                }
            }
            match enclosing_here {
                Some(sym) => {
                    enclosing.push(SymbolRef::from_doc_symbol(sym));
                    level = &sym.children;
                    top = false;
                }
                None => break,
            }
        }

        Ok(ScopeReport {
            enclosing,
            imports,
            siblings,
        })
    }

    // ---------- Context ----------

    /// Pack source context around `(line, character)` into at most
    /// `token_budget` estimated tokens. v0 is a deterministic,
    /// LSP-free, priority-ordered greedy packer (DESIGN.md §Dogfooding):
    ///
    /// 1. **anchor** — the innermost enclosing function (Tree-sitter),
    ///    always kept even if it alone exceeds the budget (it's the
    ///    point of the pack);
    /// 2. **imports** — the file's import declarations;
    /// 3. **sibling signatures** — the first line of each other
    ///    top-level item.
    ///
    /// Candidates are added in that order until one would exceed the
    /// budget, at which point packing stops and `truncated` is set.
    /// Token cost is the cheap `chars / 4` heuristic, surfaced per slice
    /// and in total so the agent can recalibrate. The richer rungs from
    /// the design (referenced type defs, callee signatures, docstrings —
    /// all LSP-backed) are deferred to v1; see `ROADMAP.md`.
    pub fn context_pack(
        &self,
        buffer_id: u64,
        line: u32,
        character: u32,
        token_budget: usize,
    ) -> Result<PackedContext> {
        let entry = self.buffer_entry(buffer_id)?;
        let language = entry
            .language
            .context("buffer has no recognized language; cannot pack context")?;
        let syn = entry
            .syntax
            .as_ref()
            .context("buffer has no syntax (unsupported language)")?;
        let rope = entry.buffer.rope();
        let path = entry.buffer.path().map(|p| p.display().to_string());

        let point_char = lsp_pos_to_char(&entry.buffer, line, character)?;
        let point_byte = rope.char_to_byte(point_char);

        let mut candidates: Vec<ContextSlice> = Vec::new();
        // Spans already represented, so a top-level item that is also an
        // import (or the anchor itself) isn't packed twice.
        let mut covered: Vec<(usize, usize)> = Vec::new();
        let mut anchor_range = None;
        let mut anchor_span: Option<(usize, usize)> = None;

        // 1. Anchor: innermost enclosing function.
        if let Some(fq) = language.function_query() {
            let fns = syn.ast_query(rope, fq)?;
            if let Some(m) = fns
                .iter()
                .filter(|m| {
                    m.capture == "fn" && m.byte_start <= point_byte && point_byte < m.byte_end
                })
                .min_by_key(|m| m.byte_end - m.byte_start)
            {
                anchor_span = Some((m.byte_start, m.byte_end));
                anchor_range = Some(byte_span_to_range(rope, m.byte_start, m.byte_end));
                covered.push((m.byte_start, m.byte_end));
                candidates.push(make_context_slice(
                    rope,
                    path.clone(),
                    m.byte_start,
                    m.byte_end,
                    "anchor: enclosing function",
                ));
            }
        }

        // 2. Imports (LSP-free).
        if let Some(iq) = language.import_query() {
            for m in syn.ast_query(rope, iq)? {
                if m.capture != "import" {
                    continue;
                }
                covered.push((m.byte_start, m.byte_end));
                candidates.push(make_context_slice(
                    rope,
                    path.clone(),
                    m.byte_start,
                    m.byte_end,
                    "import",
                ));
            }
        }

        // 3. Sibling signatures: first line of each other top-level item.
        if let Some(mq) = language.module_items_query() {
            let mut count = 0;
            for m in syn.ast_query(rope, mq)? {
                if m.capture != "item" || m.kind.contains("comment") {
                    continue;
                }
                let span = (m.byte_start, m.byte_end);
                if covered.contains(&span) {
                    continue;
                }
                // Skip items nested inside the anchor — already in its body.
                if anchor_span.is_some_and(|(bs, be)| m.byte_start >= bs && m.byte_end <= be) {
                    continue;
                }
                let sig_end = first_line_end_byte(rope, m.byte_start, m.byte_end);
                candidates.push(make_context_slice(
                    rope,
                    path.clone(),
                    m.byte_start,
                    sig_end,
                    "sibling signature",
                ));
                count += 1;
                if count >= CONTEXT_MAX_SIBLINGS {
                    break;
                }
            }
        }

        // Greedy pack in priority order. Index 0 (the anchor, or the
        // first import if there's no enclosing function) is always kept;
        // the first later candidate that would overflow the budget stops
        // the pack and flips `truncated` — never a silent drop.
        let mut slices = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        for (i, cand) in candidates.into_iter().enumerate() {
            if i == 0 || total + cand.estimated_tokens <= token_budget {
                total += cand.estimated_tokens;
                slices.push(cand);
            } else {
                truncated = true;
                break;
            }
        }

        Ok(PackedContext {
            anchor: anchor_range,
            slices,
            estimated_tokens: total,
            truncated,
        })
    }

    // ---------- Inline agent tasks ----------

    /// Walk the workspace beneath `buffer_id` for inline agent task
    /// markers (`// CLAUDE: ...`, `// TODO(claude): ...`, also `# ...`
    /// for hash-comment languages — the match is on the keyword, not
    /// the comment prefix). Lets agents drop intent into the file where
    /// it belongs and pick it up on the next pass without copy-paste.
    ///
    /// Scan root: the git repo containing the buffer if one exists,
    /// otherwise the buffer's parent directory. Result paths are
    /// relative to that root.
    pub fn tasks_list(&self, buffer_id: u64) -> Result<Vec<InlineTask>> {
        let root = self.tasks_scan_root_for_buffer(buffer_id)?;
        Ok(scan_inline_tasks(&root))
    }

    fn tasks_scan_root_for_buffer(&self, buffer_id: u64) -> Result<PathBuf> {
        let entry = self.buffer_entry(buffer_id)?;
        let path = entry
            .buffer
            .path()
            .context("buffer has no path; cannot locate a scan root")?;
        if let Ok(repo) = git::repo_root_for(path) {
            return Ok(repo);
        }
        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(parent)
    }

    fn repo_root_for_buffer(&self, buffer_id: u64) -> Result<PathBuf> {
        let entry = self.buffer_entry(buffer_id)?;
        let path = entry
            .buffer
            .path()
            .context("buffer has no path; cannot locate a git repo")?;
        git::repo_root_for(path)
    }

    /// Resolve `(repo_root, rel_path)` for stage/unstage. When `path`
    /// is `None`, the buffer's own file (relative to the repo root) is
    /// the target. When `path` is `Some`, the string is taken as a
    /// repo-root-relative path verbatim — `git` itself rejects anything
    /// outside the worktree.
    fn stage_target(
        &self,
        buffer_id: u64,
        path: Option<&str>,
    ) -> Result<(PathBuf, PathBuf)> {
        let entry = self.buffer_entry(buffer_id)?;
        let buf_path = entry
            .buffer
            .path()
            .context("buffer has no path; cannot locate a git repo")?;
        let repo_root = git::repo_root_for(buf_path)?;
        let rel = match path {
            Some(p) => PathBuf::from(p),
            None => buf_path
                .strip_prefix(&repo_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| buf_path.to_path_buf()),
        };
        Ok((repo_root, rel))
    }

    // ---------- Read-only accessors (for tests + transport) ----------

    /// Current buffer version (the optimistic-concurrency token edits
    /// must reference). Cheaper than a full `buffer.read` when the
    /// agent only wants to check whether something has moved.
    pub fn buffer_version(&self, buffer_id: u64) -> Result<u64> {
        Ok(self.buffer_entry(buffer_id)?.buffer.version())
    }

    #[allow(dead_code)]
    pub fn focus(&self) -> Option<u64> {
        self.focus
    }

    // ---------- TUI lens (Phase 4 step 1b scaffolding) ----------
    //
    // Read/edit helpers the TUI (`App`) will call once it becomes a
    // *view* over this shared core instead of owning its own `Buffer`
    // + `TxManager`. They mirror the agent-facing methods above but are
    // shaped for the per-keystroke render/edit loop: rendering pulls an
    // owned line snapshot (lock dropped before drawing), editing runs
    // through one auto-tx that discards a no-op so movement at a buffer
    // boundary doesn't litter flat history. Exercised by this module's
    // tests today; `App` is the production caller in the next step.
    //
    // `#[allow(dead_code)]` here is the same forward-scaffolding idiom
    // as `focus` above — these land with the App-on-core flip.

    /// Borrow a buffer for read-only View math (`char_idx`, `move_*`,
    /// `word_at_cursor`). Read-only, so it doesn't break the "every
    /// mutation goes through a transaction" invariant.
    #[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
    pub fn buffer_ref(&self, buffer_id: u64) -> Result<&Buffer> {
        Ok(&self.buffer_entry(buffer_id)?.buffer)
    }

    /// Snapshot lightweight per-buffer metadata for the status line and
    /// gutter geometry without holding the lock across the render.
    #[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
    pub fn buffer_meta(&self, buffer_id: u64) -> Result<BufferMeta> {
        let entry = self.buffer_entry(buffer_id)?;
        Ok(BufferMeta {
            path: entry.buffer.path().map(|p| p.to_path_buf()),
            dirty: entry.buffer.is_dirty(),
            line_count: entry.buffer.line_count(),
            version: entry.buffer.version(),
        })
    }

    /// Snapshot `rows` visible lines starting at `top_line`: trailing
    /// newline stripped, highlight spans cloned out so the caller can
    /// drop the lock before drawing. Out-of-range rows come back empty,
    /// matching the renderer's blank padding past end-of-buffer.
    #[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
    pub fn render_lines(
        &self,
        buffer_id: u64,
        top_line: usize,
        rows: usize,
    ) -> Result<Vec<RenderLine>> {
        let entry = self.buffer_entry(buffer_id)?;
        let total = entry.buffer.line_count();
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            let line_idx = top_line + r;
            if line_idx >= total {
                out.push(RenderLine::default());
                continue;
            }
            let mut text: String = entry.buffer.line(line_idx).chars().collect();
            // Strip the trailing newline so it doesn't render as a
            // control character (mirrors the old `ui::render_text`).
            if text.ends_with('\n') {
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
            }
            let spans = entry
                .syntax
                .as_ref()
                .map(|s| s.line_spans(line_idx).to_vec())
                .unwrap_or_default();
            out.push(RenderLine { text, spans });
        }
        Ok(out)
    }

    /// Run a single TUI edit inside an auto-tx. Mirrors `App::apply`'s
    /// old inline logic: a mutation that doesn't move the version (a
    /// boundary no-op like `DeletePrev` at offset 0) is discarded so it
    /// never reaches flat history; a real change commits, refreshes
    /// syntax, and notifies the LSP. Returns whether the buffer changed.
    #[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
    pub fn tui_apply_edit<F>(&mut self, buffer_id: u64, intent: String, edit: F) -> Result<bool>
    where
        F: FnOnce(&mut Buffer),
    {
        let tx_id = {
            let entry = self
                .buffers
                .get(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            self.tx_manager.begin(intent, None, &entry.buffer)
        };
        let pre = self.tx_manager.pre_version(tx_id);
        {
            let entry = self
                .buffers
                .get_mut(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            edit(&mut entry.buffer);
        }
        let changed = {
            let entry = self
                .buffers
                .get(&buffer_id)
                .expect("buffer existed at the start of the edit");
            Some(entry.buffer.version()) != pre
        };
        if !changed {
            self.tx_manager.discard(tx_id)?;
            return Ok(false);
        }
        {
            let entry = self
                .buffers
                .get(&buffer_id)
                .expect("buffer existed at the start of the edit");
            self.tx_manager.commit(tx_id, &entry.buffer)?;
        }
        self.refresh_syntax(buffer_id);
        self.notify_lsp_changed(buffer_id);
        Ok(true)
    }

    /// Persist a buffer to disk (TUI `Ctrl-S` / autosave). The caller
    /// owns refreshing git status — `App` keeps that TUI-local.
    #[allow(dead_code)] // consumed by the TUI in Phase 4 step 1b (App-on-core flip).
    pub fn tui_save(&mut self, buffer_id: u64) -> Result<usize> {
        let entry = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
        entry.buffer.save()
    }

    // ---------- Internals ----------

    fn buffer_entry(&self, buffer_id: u64) -> Result<&BufferEntry> {
        self.buffers
            .get(&buffer_id)
            .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))
    }

    fn check_version(&self, buffer_id: u64, version: u64) -> Result<()> {
        let entry = self.buffer_entry(buffer_id)?;
        if entry.buffer.version() != version {
            return Err(anyhow!(
                "version mismatch: buffer {} is at {}, caller sent {}",
                buffer_id,
                entry.buffer.version(),
                version
            ));
        }
        Ok(())
    }

    fn tx_buffer_for(&self, tx_id: TxId) -> Result<u64> {
        match self.explicit_tx {
            Some((open_tx, bid)) if open_tx == tx_id => Ok(bid),
            _ => Err(anyhow!(
                "tx_id {:?} is not the currently open transaction",
                tx_id
            )),
        }
    }

    /// Run `body` inside a per-buffer transaction. If an explicit
    /// transaction is open *for the same buffer*, the body joins it;
    /// otherwise an auto-tx wraps the body and commits / rolls back
    /// based on the body's result.
    fn with_auto_tx_on<F>(
        &mut self,
        buffer_id: u64,
        intent: String,
        body: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut BufferEntry) -> Result<()>,
    {
        let join_explicit = matches!(self.explicit_tx, Some((_, b)) if b == buffer_id);
        if join_explicit {
            let entry = self
                .buffers
                .get_mut(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            return body(entry);
        }
        let tx_id = {
            let entry = self
                .buffers
                .get(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            self.tx_manager.begin(intent, None, &entry.buffer)
        };
        let body_result = {
            let entry = self
                .buffers
                .get_mut(&buffer_id)
                .ok_or_else(|| anyhow!("unknown buffer_id {}", buffer_id))?;
            body(entry)
        };
        match body_result {
            Ok(()) => {
                let entry = self
                    .buffers
                    .get(&buffer_id)
                    .expect("buffer was just modified inside the tx");
                self.tx_manager.commit(tx_id, &entry.buffer)?;
                Ok(())
            }
            Err(e) => {
                if let Some(entry) = self.buffers.get_mut(&buffer_id) {
                    let _ = self.tx_manager.rollback(tx_id, &mut entry.buffer);
                }
                Err(e)
            }
        }
    }

    fn refresh_syntax(&mut self, buffer_id: u64) {
        if let Some(entry) = self.buffers.get_mut(&buffer_id)
            && let Some(syn) = entry.syntax.as_mut()
        {
            syn.refresh(&mut entry.buffer);
        }
    }

    fn notify_lsp_changed(&mut self, buffer_id: u64) {
        let Some(entry) = self.buffers.get_mut(&buffer_id) else {
            return;
        };
        let Some(lang) = entry.language else {
            return;
        };
        let Some(lsp) = self.lsp_clients.get(&lang) else {
            return;
        };
        let Some(uri) = entry.uri.as_ref() else {
            return;
        };
        entry.lsp_version += 1;
        let text = entry.buffer.rope().to_string();
        let _ = lsp.did_change(uri, entry.lsp_version, &text);
    }

    fn ensure_lsp_for(&mut self, language: Language, path: &Path, uri: &str, text: &str) {
        if let Some(lsp) = self.lsp_clients.get(&language) {
            // Already spawned — just register the new file.
            let _ = lsp.did_open(uri, language.lsp_language_id(), text);
            return;
        }
        let workspace = lsp::workspace_root_for(path, language);
        match LspClient::spawn(language, &workspace, uri, text) {
            Ok(client) => {
                self.lsp_clients.insert(language, client);
                self.workspace_roots.insert(language, workspace);
            }
            Err(e) => {
                eprintln!("dyad: {} LSP disabled ({e})", language.display_name());
            }
        }
    }

    /// Look up the LSP client for `buffer_id`, returning the client +
    /// the buffer's URI. The error message names the language's binary
    /// and install hint so it stays accurate as we add more languages.
    fn lsp_for_buffer(&self, buffer_id: u64) -> Result<(&LspClient, &str)> {
        let entry = self.buffer_entry(buffer_id)?;
        let lang = entry
            .language
            .context("buffer has no recognized language; cannot query LSP")?;
        let uri = entry
            .uri
            .as_deref()
            .context("buffer has no file URI; cannot query LSP")?;
        let lsp = self.lsp_clients.get(&lang).with_context(|| {
            format!(
                "{} not running (see `{}`)",
                lang.lsp_binary(),
                lang.install_hint()
            )
        })?;
        Ok((lsp, uri))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SOLE_BUFFER_ID;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dyad-core-{}-{}.txt", std::process::id(), name))
    }

    fn core_with(name: &str, contents: &str) -> (EditorCore, PathBuf) {
        let path = temp_path(name);
        std::fs::write(&path, contents).unwrap();
        let core = EditorCore::open(path.clone()).unwrap();
        (core, path)
    }

    #[test]
    fn open_focuses_sole_buffer() {
        let (core, path) = core_with("focus", "hello\nworld\n");
        assert_eq!(core.focus(), Some(SOLE_BUFFER_ID));
        let meta = core.buffer_meta(SOLE_BUFFER_ID).unwrap();
        // "hello\nworld\n" => three rope lines (the trailing empty one).
        assert_eq!(meta.line_count, 3);
        assert!(!meta.dirty);
        assert_eq!(meta.version, 0);
        assert_eq!(meta.path.as_deref(), Some(path.as_path()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn render_lines_strips_newline_and_pads_past_eof() {
        let (core, path) = core_with("render", "ab\ncd\n");
        let lines = core.render_lines(SOLE_BUFFER_ID, 0, 4).unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "ab");
        assert_eq!(lines[1].text, "cd");
        assert_eq!(lines[2].text, ""); // real trailing empty rope line
        assert_eq!(lines[3].text, ""); // padded past end-of-buffer
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn render_lines_honors_top_line_offset() {
        let (core, path) = core_with("offset", "l0\nl1\nl2\nl3\n");
        let lines = core.render_lines(SOLE_BUFFER_ID, 2, 2).unwrap();
        assert_eq!(lines[0].text, "l2");
        assert_eq!(lines[1].text, "l3");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tui_apply_edit_commits_real_change_and_discards_noop() {
        let (mut core, path) = core_with("edit", "x\n");
        let changed = core
            .tui_apply_edit(SOLE_BUFFER_ID, "insert".into(), |b| b.insert_char(0, 'Z'))
            .unwrap();
        assert!(changed);
        assert!(
            core.buffer_ref(SOLE_BUFFER_ID)
                .unwrap()
                .rope()
                .to_string()
                .starts_with("Zx")
        );
        assert_eq!(core.history_recent(10).len(), 1);

        // A closure that doesn't touch the rope must roll back without
        // recording a history entry.
        let again = core
            .tui_apply_edit(SOLE_BUFFER_ID, "noop".into(), |_b| {})
            .unwrap();
        assert!(!again);
        assert_eq!(
            core.history_recent(10).len(),
            1,
            "a no-op edit must not record flat history"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tui_save_persists_and_clears_dirty() {
        let (mut core, path) = core_with("save", "a\n");
        core.tui_apply_edit(SOLE_BUFFER_ID, "insert".into(), |b| b.insert_char(0, 'q'))
            .unwrap();
        assert!(core.buffer_meta(SOLE_BUFFER_ID).unwrap().dirty);
        let bytes = core.tui_save(SOLE_BUFFER_ID).unwrap();
        assert!(bytes > 0);
        assert!(!core.buffer_meta(SOLE_BUFFER_ID).unwrap().dirty);
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("qa"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn lens_rejects_unknown_buffer_id() {
        let (mut core, path) = core_with("unknown", "z\n");
        assert!(core.buffer_meta(999).is_err());
        assert!(core.render_lines(999, 0, 1).is_err());
        assert!(core.buffer_ref(999).is_err());
        assert!(core.tui_save(999).is_err());
        assert!(core.tui_apply_edit(999, "x".into(), |_b| {}).is_err());
        std::fs::remove_file(&path).ok();
    }
}
