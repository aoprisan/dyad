//! Phase 4 — the protocol layer.
//!
//! `ProtocolState` owns the editor-as-runtime state (one buffer + syntax +
//! transaction manager) and exposes each DESIGN.md operation as a typed
//! Rust method. `mcp.rs` is one transport over this surface; tests call
//! the methods directly.
//!
//! Every edit goes through a transaction. If a caller hasn't opened an
//! explicit `tx.begin`, the edit auto-opens / auto-commits a one-shot
//! transaction so the flat history still gets an entry — matching the
//! "every edit happens inside a transaction" requirement from DESIGN.md
//! §Transactions & intent.

use std::ops::Range;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::buffer::Buffer;
use crate::lsp::{self, Diagnostic, Location, LspClient, TextEdit};
use crate::syntax::{AstMatch, Syntax};
use crate::tx::{Change, ChangeId, TxId, TxManager};

pub struct ProtocolState {
    buffer: Buffer,
    syntax: Option<Syntax>,
    tx_manager: TxManager,
    /// Currently open explicit transaction, if any. Edits join this tx
    /// rather than auto-wrapping themselves.
    explicit_tx: Option<TxId>,
    /// Optional rust-analyzer connection. `None` when the file isn't
    /// Rust, doesn't live in a Cargo workspace, or rust-analyzer
    /// couldn't be spawned — the protocol still works, just without
    /// semantic tools.
    lsp: Option<LspClient>,
    /// `file://...` URI for the buffer. Set whenever the buffer has a
    /// path; LSP tools require this to be `Some`.
    buffer_uri: Option<String>,
    /// Monotonic LSP document version (separate from `Buffer::version`
    /// because LSP needs an i32 starting at 0).
    lsp_version: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct BufferEntry {
    pub id: u64,
    pub path: Option<String>,
    pub dirty: bool,
    pub version: u64,
}

/// Outcome of `edit_rename_symbol`. `applied` counts the in-buffer
/// edits we successfully wrote; `skipped_files` are the other URIs the
/// LSP server wanted to touch but which aren't loaded as `dyad`'s
/// active buffer. The agent must re-target those separately.
#[derive(Clone, Debug, Serialize)]
pub struct RenameResult {
    pub applied: usize,
    pub new_version: u64,
    pub skipped_files: Vec<String>,
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

/// We only host one buffer for now (Phase 8 brings multi-buffer +
/// awareness). The MCP layer accepts a `buffer_id` field for forward
/// compatibility but currently rejects anything but `SOLE_BUFFER_ID`.
pub const SOLE_BUFFER_ID: u64 = 1;

impl ProtocolState {
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut buffer = Buffer::open(path)?;
        let mut syntax = Syntax::for_path(buffer.path());
        if let Some(syn) = syntax.as_mut() {
            syn.refresh(&mut buffer);
        }
        let (lsp, buffer_uri) = try_spawn_lsp(&buffer);
        Ok(Self {
            buffer,
            syntax,
            tx_manager: TxManager::new(),
            explicit_tx: None,
            lsp,
            buffer_uri,
            lsp_version: 0,
        })
    }

    // ---------- Buffers ----------

    pub fn buffer_list(&self) -> Vec<BufferEntry> {
        vec![BufferEntry {
            id: SOLE_BUFFER_ID,
            path: self
                .buffer
                .path()
                .map(|p| p.display().to_string()),
            dirty: self.buffer.is_dirty(),
            version: self.buffer.version(),
        }]
    }

    pub fn buffer_read(
        &self,
        buffer_id: u64,
        range: Option<CharRange>,
    ) -> Result<BufferReadResponse> {
        self.check_buffer(buffer_id)?;
        let rope = self.buffer.rope();
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
            version: self.buffer.version(),
        })
    }

    // ---------- AST ----------

    pub fn ast_query(&self, buffer_id: u64, query: &str) -> Result<Vec<AstMatch>> {
        self.check_buffer(buffer_id)?;
        let syn = self
            .syntax
            .as_ref()
            .context("buffer has no syntax (unsupported language)")?;
        syn.ast_query(self.buffer.rope(), query)
    }

    // ---------- Edits ----------

    pub fn edit_replace_range(
        &mut self,
        buffer_id: u64,
        version: u64,
        range: CharRange,
        text: &str,
    ) -> Result<u64> {
        self.check_buffer(buffer_id)?;
        self.check_version(version)?;
        if range.start > range.end || range.end > self.buffer.len_chars() {
            return Err(anyhow!(
                "range {}..{} outside buffer (len_chars = {})",
                range.start,
                range.end,
                self.buffer.len_chars()
            ));
        }
        self.with_auto_tx(
            || format!("edit.replace_range {}..{}", range.start, range.end),
            |s| {
                if range.start < range.end {
                    s.buffer.delete_range(range.start..range.end);
                }
                if !text.is_empty() {
                    s.buffer.insert_str(range.start, text);
                }
                Ok(())
            },
        )?;
        self.refresh_syntax();
        self.notify_lsp_changed();
        Ok(self.buffer.version())
    }

    pub fn edit_replace_node(
        &mut self,
        buffer_id: u64,
        version: u64,
        byte_range: ByteRange,
        text: &str,
    ) -> Result<u64> {
        self.check_buffer(buffer_id)?;
        self.check_version(version)?;
        if byte_range.start > byte_range.end || byte_range.end > self.buffer.rope().len_bytes() {
            return Err(anyhow!(
                "byte range {}..{} outside buffer (len_bytes = {})",
                byte_range.start,
                byte_range.end,
                self.buffer.rope().len_bytes()
            ));
        }
        let range = Range {
            start: byte_range.start,
            end: byte_range.end,
        };
        self.with_auto_tx(
            || format!("edit.replace_node {}..{}", byte_range.start, byte_range.end),
            |s| {
                s.buffer.replace_node(range, text);
                Ok(())
            },
        )?;
        self.refresh_syntax();
        self.notify_lsp_changed();
        Ok(self.buffer.version())
    }

    // ---------- Transactions ----------

    pub fn tx_begin(
        &mut self,
        intent: String,
        conversation_id: Option<String>,
    ) -> Result<TxId> {
        if self.explicit_tx.is_some() {
            return Err(anyhow!(
                "a transaction is already open; commit or rollback it first"
            ));
        }
        let tx_id = self.tx_manager.begin(intent, conversation_id, &self.buffer);
        self.explicit_tx = Some(tx_id);
        Ok(tx_id)
    }

    pub fn tx_commit(&mut self, tx_id: TxId) -> Result<ChangeId> {
        if self.explicit_tx != Some(tx_id) {
            return Err(anyhow!(
                "tx_id {:?} is not the currently open transaction",
                tx_id
            ));
        }
        let change_id = self.tx_manager.commit(tx_id, &self.buffer)?;
        self.explicit_tx = None;
        Ok(change_id)
    }

    pub fn tx_rollback(&mut self, tx_id: TxId) -> Result<()> {
        if self.explicit_tx != Some(tx_id) {
            return Err(anyhow!(
                "tx_id {:?} is not the currently open transaction",
                tx_id
            ));
        }
        self.tx_manager.rollback(tx_id, &mut self.buffer)?;
        self.explicit_tx = None;
        // The cached syntax tree is now stale relative to the restored
        // rope — drop it so the next refresh does a full reparse.
        if let Some(syn) = self.syntax.as_mut() {
            syn.invalidate();
        }
        self.refresh_syntax();
        self.notify_lsp_changed();
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
        self.check_buffer(buffer_id)?;
        let lsp = self
            .lsp
            .as_ref()
            .context("rust-analyzer not running (see `rustup component add rust-analyzer`)")?;
        let uri = self
            .buffer_uri
            .as_ref()
            .context("buffer has no file URI; cannot query LSP")?;
        lsp.definition(uri, line, character)
    }

    pub fn diag_current(&self, buffer_id: u64) -> Result<Vec<Diagnostic>> {
        self.check_buffer(buffer_id)?;
        let lsp = self
            .lsp
            .as_ref()
            .context("rust-analyzer not running (see `rustup component add rust-analyzer`)")?;
        let uri = self
            .buffer_uri
            .as_ref()
            .context("buffer has no file URI; cannot query LSP")?;
        Ok(lsp.diagnostics(uri))
    }

    /// Phase 7 tier-3 edit: ask the LSP server for the workspace edits
    /// required to rename the symbol at `(line, character)` to
    /// `new_name`, then apply the in-buffer subset as a single
    /// transaction. Cross-file edits aren't applied here — they come
    /// back in `skipped_files` so the caller can re-target via another
    /// `--mcp` invocation (Phase 8 brings real multi-buffer).
    ///
    /// LSP positions are line + UTF-16 code units. dyad's rope is char
    /// indexed (Unicode scalar values), so this conversion is exact
    /// for BMP-only source — which covers nearly all Rust code. Files
    /// with non-BMP characters (rare emoji-in-string-literal cases)
    /// will mis-position; that's a known limitation here.
    pub fn edit_rename_symbol(
        &mut self,
        buffer_id: u64,
        version: u64,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        self.check_buffer(buffer_id)?;
        self.check_version(version)?;
        let lsp = self
            .lsp
            .as_ref()
            .context("rust-analyzer not running (see `rustup component add rust-analyzer`)")?;
        let uri = self
            .buffer_uri
            .as_ref()
            .context("buffer has no file URI; cannot query LSP")?
            .clone();

        let workspace_edit = lsp.rename(&uri, line, character, &new_name)?;
        let mut in_buffer = workspace_edit
            .changes
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        let skipped_files: Vec<String> = workspace_edit
            .changes
            .keys()
            .filter(|k| **k != uri)
            .cloned()
            .collect();

        if in_buffer.is_empty() {
            return Ok(RenameResult {
                applied: 0,
                new_version: self.buffer.version(),
                skipped_files,
            });
        }

        // Apply end-to-start so earlier edits don't shift later ranges.
        in_buffer.sort_by(|a, b| {
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

        let edits_for_intent = in_buffer.clone();
        self.with_auto_tx(
            || format!("edit.rename_symbol -> {new_name}"),
            move |s| apply_text_edits(&mut s.buffer, &edits_for_intent),
        )?;
        self.refresh_syntax();
        self.notify_lsp_changed();

        Ok(RenameResult {
            applied: in_buffer.len(),
            new_version: self.buffer.version(),
            skipped_files,
        })
    }

    // ---------- Read-only accessors (for tests + transport) ----------

    #[allow(dead_code)] // Used by tests; an MCP `buffer.version` tool can wrap it later.
    pub fn buffer_version(&self) -> u64 {
        self.buffer.version()
    }

    // ---------- Internals ----------

    fn check_buffer(&self, buffer_id: u64) -> Result<()> {
        if buffer_id != SOLE_BUFFER_ID {
            return Err(anyhow!(
                "unknown buffer_id {} (only {} is hosted)",
                buffer_id,
                SOLE_BUFFER_ID
            ));
        }
        Ok(())
    }

    fn check_version(&self, version: u64) -> Result<()> {
        if self.buffer.version() != version {
            return Err(anyhow!(
                "version mismatch: buffer is at {}, caller sent {}",
                self.buffer.version(),
                version
            ));
        }
        Ok(())
    }

    /// Run `body` under either the caller's explicit transaction or an
    /// auto-tx with `intent_fn` as its intent. On error the auto-tx is
    /// rolled back; on success it commits. The explicit-tx path is left
    /// to the caller to close.
    fn with_auto_tx<F, I>(&mut self, intent_fn: I, body: F) -> Result<()>
    where
        F: FnOnce(&mut Self) -> Result<()>,
        I: FnOnce() -> String,
    {
        if self.explicit_tx.is_some() {
            return body(self);
        }
        let tx_id = self
            .tx_manager
            .begin(intent_fn(), None, &self.buffer);
        match body(self) {
            Ok(()) => {
                self.tx_manager.commit(tx_id, &self.buffer)?;
                Ok(())
            }
            Err(e) => {
                let _ = self.tx_manager.rollback(tx_id, &mut self.buffer);
                Err(e)
            }
        }
    }

    fn refresh_syntax(&mut self) {
        if let Some(syn) = self.syntax.as_mut() {
            syn.refresh(&mut self.buffer);
        }
    }

    fn notify_lsp_changed(&mut self) {
        if let (Some(lsp), Some(uri)) = (self.lsp.as_ref(), self.buffer_uri.as_ref()) {
            self.lsp_version += 1;
            let text = self.buffer.rope().to_string();
            // Best-effort: an LSP I/O error doesn't fail the edit.
            let _ = lsp.did_change(uri, self.lsp_version, &text);
        }
    }
}

/// Apply a list of LSP `TextEdit`s to a buffer. Caller is responsible
/// for sorting the edits end-to-start so earlier indices stay valid
/// (we don't sort here so the caller's intent — including pre-sorted
/// inputs from tests — is preserved).
fn apply_text_edits(buffer: &mut Buffer, edits: &[TextEdit]) -> Result<()> {
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

/// Convert an LSP `(line, character)` position into a rope char index.
/// LSP characters are UTF-16 code units in the spec; we treat them as
/// rope chars, which is exact for BMP-only source and off-by-one per
/// non-BMP code point otherwise (see `edit_rename_symbol`'s doc).
fn lsp_pos_to_char(buffer: &Buffer, line: u32, character: u32) -> Result<usize> {
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

/// Try to spawn rust-analyzer for the buffer's file. Returns
/// `(Some(client), Some(uri))` on success, `(None, Some(uri))` if the
/// file exists but rust-analyzer couldn't start, and `(None, None)`
/// when the buffer is unsaved / pathless. Failures log to stderr and
/// degrade gracefully — the protocol still serves non-LSP tools.
fn try_spawn_lsp(buffer: &Buffer) -> (Option<LspClient>, Option<String>) {
    let Some(path) = buffer.path() else {
        return (None, None);
    };
    let uri = lsp::path_to_uri(path);
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return (None, Some(uri));
    }
    let workspace = lsp::workspace_root_for(path);
    let text = buffer.rope().to_string();
    match LspClient::spawn_rust(&workspace, &uri, &text) {
        Ok(client) => (Some(client), Some(uri)),
        Err(e) => {
            eprintln!("dyad: LSP disabled ({e})");
            (None, Some(uri))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_state(name: &str) -> ProtocolState {
        let path = std::env::temp_dir()
            .join(format!("dyad_proto_{}_{}.rs", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut state = ProtocolState::open(path).unwrap();
        // Seed with one function so ast.query has something to chew on.
        let v0 = state.buffer.version();
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
        let stale = state.buffer_version() + 1;
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
        let v = state.buffer_version();
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
        let read = state.buffer_read(SOLE_BUFFER_ID, None).unwrap();
        assert_eq!(read.text, "fn farewell() {}\n");
    }

    #[test]
    fn explicit_tx_commit_creates_one_history_entry_for_two_edits() {
        let mut state = scratch_state("explicit_tx");
        let tx_id = state
            .tx_begin("multi-edit refactor".to_string(), None)
            .unwrap();
        let v1 = state.buffer_version();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v1,
                CharRange { start: 0, end: 0 },
                "// intro\n",
            )
            .unwrap();
        let v2 = state.buffer_version();
        let end = state.buffer.rope().len_chars();
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

        // Two occurrences of `old`: at chars 3..6 and chars 15..18.
        // LSP positions on line 0: characters 3..6 and 15..18.
        // We pass them already sorted end-to-start the way
        // edit_rename_symbol would.
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
        // line 0, char 1 -> rope char 1 ('b')
        assert_eq!(lsp_pos_to_char(&buf, 0, 1).unwrap(), 1);
        // line 1, char 2 -> rope char 5 ('e')
        assert_eq!(lsp_pos_to_char(&buf, 1, 2).unwrap(), 5);
        // out of range line errors.
        assert!(lsp_pos_to_char(&buf, 9, 0).is_err());
    }

    #[test]
    fn tx_rollback_restores_buffer_and_invalidates_syntax() {
        let mut state = scratch_state("rollback");
        let pre_text = state.buffer_read(SOLE_BUFFER_ID, None).unwrap().text;
        let tx_id = state.tx_begin("doomed".to_string(), None).unwrap();
        let v = state.buffer_version();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 0, end: 0 },
                "garbage ",
            )
            .unwrap();
        state.tx_rollback(tx_id).unwrap();
        let read = state.buffer_read(SOLE_BUFFER_ID, None).unwrap();
        assert_eq!(read.text, pre_text);
        // ast.query must still work after the rollback — proves the
        // syntax tree was invalidated and re-parsed against the restored
        // rope.
        let matches = state
            .ast_query(
                SOLE_BUFFER_ID,
                "(function_item name: (identifier) @name)",
            )
            .unwrap();
        assert!(matches.iter().any(|m| m.capture == "name"));
    }
}
