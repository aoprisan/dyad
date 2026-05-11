//! Phase 3 — transactions + flat history.
//!
//! Every buffer mutation runs inside a transaction with a stated intent
//! string. `commit` records a `Change` in a flat history log; `rollback`
//! restores the buffer to the snapshot taken at `begin`. Maps to
//! DESIGN.md §Transactions & intent and §History.
//!
//! Today's only client is `App::apply`, which auto-wraps each mutating
//! keystroke with a one-line intent ("insert 'a'", "delete backward").
//! Phase 4 (MCP) will pass real intent strings from agent calls.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Result, anyhow};

use crate::buffer::{Buffer, BufferSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TxId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChangeId(u64);

struct ActiveTx {
    tx_id: TxId,
    intent: String,
    conversation_id: Option<String>,
    started_at: SystemTime,
    pre_version: u64,
    snapshot: BufferSnapshot,
}

#[allow(dead_code)] // Phase 4: fields are consumed by the MCP `history.recent` handler.
#[derive(Clone, Debug)]
pub struct Change {
    pub change_id: ChangeId,
    pub tx_id: TxId,
    pub intent: String,
    pub conversation_id: Option<String>,
    pub timestamp: SystemTime,
    pub files: Vec<PathBuf>,
}

pub struct TxManager {
    next_tx: u64,
    next_change: u64,
    active: Vec<ActiveTx>,
    history: Vec<Change>,
}

impl TxManager {
    pub fn new() -> Self {
        Self {
            next_tx: 1,
            next_change: 1,
            active: Vec::new(),
            history: Vec::new(),
        }
    }

    pub fn begin(
        &mut self,
        intent: impl Into<String>,
        conversation_id: Option<String>,
        buffer: &Buffer,
    ) -> TxId {
        let tx_id = TxId(self.next_tx);
        self.next_tx += 1;
        self.active.push(ActiveTx {
            tx_id,
            intent: intent.into(),
            conversation_id,
            started_at: SystemTime::now(),
            pre_version: buffer.version(),
            snapshot: buffer.snapshot(),
        });
        tx_id
    }

    pub fn commit(&mut self, tx_id: TxId, buffer: &Buffer) -> Result<ChangeId> {
        let tx = self.take(tx_id)?;
        let change_id = ChangeId(self.next_change);
        self.next_change += 1;
        let files = buffer
            .path()
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        self.history.push(Change {
            change_id,
            tx_id: tx.tx_id,
            intent: tx.intent,
            conversation_id: tx.conversation_id,
            timestamp: SystemTime::now(),
            files,
        });
        let _ = tx.started_at; // recorded for future telemetry / history.diff
        Ok(change_id)
    }

    /// Restore the buffer to the snapshot taken at `begin` and discard
    /// the transaction without recording a `Change`.
    #[allow(dead_code)] // Phase 4: exposed as `tx.rollback` over MCP.
    pub fn rollback(&mut self, tx_id: TxId, buffer: &mut Buffer) -> Result<()> {
        let tx = self.take(tx_id)?;
        buffer.restore(tx.snapshot);
        Ok(())
    }

    /// Drop the transaction without committing or restoring. Used when
    /// the wrapped operation turned out to be a no-op (e.g., DeletePrev
    /// at the start of the buffer) and we don't want history clutter.
    pub fn discard(&mut self, tx_id: TxId) -> Result<()> {
        self.take(tx_id).map(|_| ())
    }

    /// Return the most-recent `limit` history entries (oldest first
    /// within the returned slice).
    #[allow(dead_code)] // Phase 4: exposed as `history.recent` over MCP.
    pub fn recent(&self, limit: usize) -> &[Change] {
        let n = self.history.len();
        let start = n.saturating_sub(limit);
        &self.history[start..]
    }

    /// `pre_version` of an active transaction — i.e. the buffer's
    /// version() at the moment `begin` was called. Callers compare it to
    /// `buffer.version()` to decide between commit and discard.
    pub fn pre_version(&self, tx_id: TxId) -> Option<u64> {
        self.active
            .iter()
            .find(|tx| tx.tx_id == tx_id)
            .map(|tx| tx.pre_version)
    }

    fn take(&mut self, tx_id: TxId) -> Result<ActiveTx> {
        let idx = self
            .active
            .iter()
            .position(|tx| tx.tx_id == tx_id)
            .ok_or_else(|| anyhow!("unknown tx_id {:?}", tx_id))?;
        Ok(self.active.remove(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn scratch_buffer(name: &str) -> Buffer {
        let path = std::env::temp_dir()
            .join(format!("dyad_tx_test_{}_{}.rs", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        Buffer::open(path).unwrap()
    }

    #[test]
    fn commit_records_change_in_history() {
        let mut buf = scratch_buffer("commit");
        let mut tx = TxManager::new();
        let tx_id = tx.begin("test insert", None, &buf);
        buf.insert_str(0, "hello");
        let change_id = tx.commit(tx_id, &buf).unwrap();

        let recent = tx.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].change_id, change_id);
        assert_eq!(recent[0].tx_id, tx_id);
        assert_eq!(recent[0].intent, "test insert");
        assert_eq!(recent[0].files.len(), 1);
    }

    #[test]
    fn rollback_restores_buffer_state() {
        let mut buf = scratch_buffer("rollback");
        buf.insert_str(0, "initial");
        let pre_text = buf.rope().to_string();

        let mut tx = TxManager::new();
        let tx_id = tx.begin("test", None, &buf);
        let end = buf.len_chars();
        buf.insert_str(end, " appended");
        assert_eq!(buf.rope().to_string(), "initial appended");

        tx.rollback(tx_id, &mut buf).unwrap();
        assert_eq!(buf.rope().to_string(), pre_text);
        assert!(tx.recent(10).is_empty());
    }

    #[test]
    fn discard_leaves_history_empty() {
        let buf = scratch_buffer("discard");
        let mut tx = TxManager::new();
        let tx_id = tx.begin("noop", None, &buf);
        tx.discard(tx_id).unwrap();
        assert!(tx.recent(10).is_empty());
    }

    #[test]
    fn recent_returns_entries_in_chronological_order() {
        let mut buf = scratch_buffer("recent");
        let mut tx = TxManager::new();

        let id1 = tx.begin("one", None, &buf);
        buf.insert_str(0, "a");
        tx.commit(id1, &buf).unwrap();

        let id2 = tx.begin("two", None, &buf);
        buf.insert_str(buf.len_chars(), "b");
        tx.commit(id2, &buf).unwrap();

        let recent = tx.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].intent, "one");
        assert_eq!(recent[1].intent, "two");
    }
}
