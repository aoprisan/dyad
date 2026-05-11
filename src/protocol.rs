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
use crate::syntax::{AstMatch, Syntax};
use crate::tx::{Change, ChangeId, TxId, TxManager};

pub struct ProtocolState {
    buffer: Buffer,
    syntax: Option<Syntax>,
    tx_manager: TxManager,
    /// Currently open explicit transaction, if any. Edits join this tx
    /// rather than auto-wrapping themselves.
    explicit_tx: Option<TxId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BufferEntry {
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
        Ok(Self {
            buffer,
            syntax,
            tx_manager: TxManager::new(),
            explicit_tx: None,
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
        Ok(())
    }

    // ---------- History ----------

    pub fn history_recent(&self, limit: usize) -> Vec<Change> {
        self.tx_manager.recent(limit).to_vec()
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
