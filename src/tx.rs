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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::buffer::{Buffer, BufferSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TxId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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
#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub change_id: ChangeId,
    pub tx_id: TxId,
    pub intent: String,
    pub conversation_id: Option<String>,
    /// Unix seconds; `serde` doesn't ship a SystemTime impl by default,
    /// and an unsigned-seconds wire format is what the agent wants anyway.
    #[serde(rename = "timestamp_unix")]
    #[serde(serialize_with = "serialize_systemtime_unix")]
    pub timestamp: SystemTime,
    pub files: Vec<PathBuf>,
}

fn serialize_systemtime_unix<S>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    s.serialize_u64(secs)
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

    #[test]
    fn recent_limit_respected() {
        let mut buf = scratch_buffer("limit");
        let mut tx = TxManager::new();
        for i in 0..5 {
            let id = tx.begin(format!("step {i}"), None, &buf);
            buf.insert_str(buf.len_chars(), "x");
            tx.commit(id, &buf).unwrap();
        }
        let recent = tx.recent(3);
        assert_eq!(recent.len(), 3);
        // Most-recent slice: steps 2, 3, 4.
        assert_eq!(recent[0].intent, "step 2");
        assert_eq!(recent[2].intent, "step 4");
    }

    #[test]
    fn commit_assigns_ascending_change_ids() {
        let mut buf = scratch_buffer("ids");
        let mut tx = TxManager::new();
        let id1 = tx.begin("a", None, &buf);
        buf.insert_str(0, "a");
        let c1 = tx.commit(id1, &buf).unwrap();
        let id2 = tx.begin("b", None, &buf);
        buf.insert_str(buf.len_chars(), "b");
        let c2 = tx.commit(id2, &buf).unwrap();
        // ChangeId is a transparent newtype; serialize to compare.
        let s1 = serde_json::to_value(c1).unwrap();
        let s2 = serde_json::to_value(c2).unwrap();
        assert_eq!(s1.as_u64().unwrap() + 1, s2.as_u64().unwrap());
    }

    #[test]
    fn pre_version_returns_buffer_version_at_begin() {
        let mut buf = scratch_buffer("preversion");
        buf.insert_str(0, "x"); // bumps version away from 0
        let snapshot_version = buf.version();
        let mut tx = TxManager::new();
        let id = tx.begin("snap", None, &buf);
        // Mutate further; pre_version should still reflect snapshot.
        buf.insert_str(buf.len_chars(), "y");
        assert_eq!(tx.pre_version(id), Some(snapshot_version));
        // After commit it disappears from the active list.
        tx.commit(id, &buf).unwrap();
        assert_eq!(tx.pre_version(id), None);
    }

    #[test]
    fn commit_with_unknown_id_errors() {
        let buf = scratch_buffer("badcommit");
        let mut tx = TxManager::new();
        let real = tx.begin("real", None, &buf);
        // Discard so `real` is no longer active.
        tx.discard(real).unwrap();
        let err = tx.commit(real, &buf).unwrap_err();
        assert!(err.to_string().contains("unknown tx_id"));
    }

    #[test]
    fn rollback_with_unknown_id_errors() {
        let mut buf = scratch_buffer("badroll");
        let mut tx = TxManager::new();
        let real = tx.begin("real", None, &buf);
        tx.discard(real).unwrap();
        let err = tx.rollback(real, &mut buf).unwrap_err();
        assert!(err.to_string().contains("unknown tx_id"));
    }

    #[test]
    fn rollback_clears_dirty_when_snapshot_was_clean() {
        let mut buf = scratch_buffer("clean_rollback");
        // Snapshot taken before any edits — clean state.
        assert!(!buf.is_dirty());
        let mut tx = TxManager::new();
        let id = tx.begin("rollback", None, &buf);
        buf.insert_str(0, "edit");
        assert!(buf.is_dirty());
        tx.rollback(id, &mut buf).unwrap();
        assert!(!buf.is_dirty());
        assert_eq!(buf.rope().to_string(), "");
    }
}
