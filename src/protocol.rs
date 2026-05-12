//! Phase 4–8 — the protocol layer.
//!
//! `ProtocolState` owns the editor-as-runtime state — a collection of
//! buffers (Phase 8), each with its own `Syntax` + LSP URI + LSP version
//! — plus a shared `TxManager` and an optional shared `LspClient`.
//! Methods are one-per-DESIGN.md-verb; `mcp.rs` is one transport over
//! this surface and tests call the methods directly.
//!
//! Edits go through transactions. With no explicit `tx.begin` open, an
//! edit auto-opens / auto-commits a one-shot transaction so the flat
//! history still gets an entry. Phase 8's `edit_rename_symbol` runs a
//! *per-buffer* auto-tx for each affected buffer — true cross-buffer
//! atomicity is deferred until cross-buffer transactions exist.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::buffer::Buffer;
use crate::git;
use crate::language::Language;
use crate::lsp::{self, Diagnostic, Location, LspClient, SymbolInformation, TextEdit};
use crate::proposals::{PendingProposal, Proposal, ProposalId, ProposalKind, ProposalQueue};
use crate::syntax::{AstMatch, Syntax};
use crate::tx::{Change, ChangeId, TxId, TxManager};

/// The first buffer always gets this id, so back-compat with the
/// pre-Phase-8 single-buffer tests holds.
pub const SOLE_BUFFER_ID: u64 = 1;

pub struct ProtocolState {
    next_buffer_id: u64,
    buffers: HashMap<u64, BufferEntry>,
    /// Which buffer the agent currently considers "focused" — surfaced
    /// via `clients.list`. Updates whenever a new buffer is opened or
    /// the focused buffer is closed.
    focus: Option<u64>,
    tx_manager: TxManager,
    /// At most one explicit transaction at a time; the buffer it
    /// targets is recorded so per-buffer edits know whether to auto-tx.
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
    /// Stable identifier for this client (the current MCP session).
    /// Surfaced via `clients.list`.
    client_id: String,
    /// Phase 10 — agent-submitted edits awaiting accept/reject.
    proposals: ProposalQueue,
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

impl ProtocolState {
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut state = Self {
            next_buffer_id: SOLE_BUFFER_ID,
            buffers: HashMap::new(),
            focus: None,
            tx_manager: TxManager::new(),
            explicit_tx: None,
            lsp_clients: HashMap::new(),
            workspace_roots: HashMap::new(),
            client_id: format!("mcp-{}", std::process::id()),
            proposals: ProposalQueue::new(),
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

    // ---------- Clients ----------

    pub fn clients_list(&self) -> Vec<ClientInfo> {
        // Phase 8 baseline: just the current MCP session. Awareness of
        // a concurrent TUI client comes after the planned daemon split.
        vec![ClientInfo {
            id: self.client_id.clone(),
            kind: "agent".into(),
            focus: self.focus,
        }]
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
