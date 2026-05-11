//! Phase 10 — hunk-by-hunk review.
//!
//! When an agent submits an edit via `edit.propose_range` it lands in
//! the queue instead of being applied immediately. A reviewer
//! (eventually a human at the TUI; today another MCP caller) walks the
//! queue via `proposals.list` and decides each one with
//! `proposals.accept(id)` or `proposals.reject(id)`. Accept runs the
//! deferred edit through the same tx machinery as a direct edit, so
//! it lands in the flat history with the proposal's intent string.
//!
//! Phase 10 ships only the protocol surface; the TUI hunk panel and
//! accept/reject keybindings need the TUI+MCP coexistence work that's
//! deferred from Phase 8.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProposalId(pub u64);

/// The wire shape for a queued proposal.
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    pub id: ProposalId,
    pub buffer_id: u64,
    pub intent: String,
    pub kind: ProposalKind,
}

/// Currently just `ReplaceRange`; future variants can carry node
/// replacements, renames, or full workspace edits.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalKind {
    ReplaceRange {
        /// Buffer version the proposal was authored against. Accept
        /// fails if the buffer has moved on (the agent must re-propose).
        version: u64,
        start: usize,
        end: usize,
        text: String,
    },
}

/// Caller-facing shape for `propose_*` calls. The queue assigns the id.
pub struct PendingProposal {
    pub buffer_id: u64,
    pub intent: String,
    pub kind: ProposalKind,
}

pub struct ProposalQueue {
    next_id: u64,
    proposals: HashMap<ProposalId, Proposal>,
}

impl ProposalQueue {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            proposals: HashMap::new(),
        }
    }

    pub fn enqueue(&mut self, pending: PendingProposal) -> ProposalId {
        let id = ProposalId(self.next_id);
        self.next_id += 1;
        self.proposals.insert(
            id,
            Proposal {
                id,
                buffer_id: pending.buffer_id,
                intent: pending.intent,
                kind: pending.kind,
            },
        );
        id
    }

    pub fn list(&self) -> Vec<Proposal> {
        let mut v: Vec<Proposal> = self.proposals.values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    /// Remove and return the proposal, or `None` if no such id.
    pub fn take(&mut self, id: ProposalId) -> Option<Proposal> {
        self.proposals.remove(&id)
    }

    #[allow(dead_code)] // surfaced once a TUI status indicator wires up.
    pub fn count(&self) -> usize {
        self.proposals.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_assigns_ascending_ids() {
        let mut q = ProposalQueue::new();
        let a = q.enqueue(PendingProposal {
            buffer_id: 1,
            intent: "a".into(),
            kind: ProposalKind::ReplaceRange { version: 0, start: 0, end: 0, text: "x".into() },
        });
        let b = q.enqueue(PendingProposal {
            buffer_id: 1,
            intent: "b".into(),
            kind: ProposalKind::ReplaceRange { version: 0, start: 0, end: 0, text: "y".into() },
        });
        assert_eq!(a.0, 1);
        assert_eq!(b.0, 2);
        let list = q.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a);
        assert_eq!(list[1].id, b);
    }

    #[test]
    fn take_removes_the_proposal() {
        let mut q = ProposalQueue::new();
        let id = q.enqueue(PendingProposal {
            buffer_id: 1,
            intent: "x".into(),
            kind: ProposalKind::ReplaceRange { version: 0, start: 0, end: 0, text: "".into() },
        });
        assert_eq!(q.count(), 1);
        let taken = q.take(id).expect("present");
        assert_eq!(taken.id, id);
        assert!(q.take(id).is_none());
        assert_eq!(q.count(), 0);
    }
}
