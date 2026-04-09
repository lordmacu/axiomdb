use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use axiomdb_core::TxnId;
use tokio::sync::Notify;

use crate::bitmap::SlotBitmap;
use crate::mode::{conflicts, LockFlags, LockMode};

/// One granted or pending lock held by a single transaction.
///
/// For record locks, the `bitmap` tracks which slot_ids on the page are
/// covered — one `LockEntry` per (txn, page, mode) covers all locked slots.
/// This is the InnoDB bitmap optimization: avoids creating one struct per row.
#[derive(Debug, Clone)]
pub struct LockEntry {
    pub txn_id: TxnId,
    pub mode: LockMode,
    pub flags: LockFlags,
    pub requested_at: Instant,
    /// Slot bitmap for record locks. `None` for table locks.
    pub bitmap: Option<SlotBitmap>,
}

/// Reason a waiter was aborted (set externally by deadlock detector or timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitAbortReason {
    Deadlock,
    Timeout,
}

/// A transaction waiting to acquire a lock.
pub struct LockWaiter {
    pub entry: LockEntry,
    pub notify: Arc<Notify>,
    /// Set by the deadlock detector (40.6) or timeout handler before notifying.
    pub abort_reason: Option<WaitAbortReason>,
}

impl std::fmt::Debug for LockWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockWaiter")
            .field("entry", &self.entry)
            .field("abort_reason", &self.abort_reason)
            .finish()
    }
}

/// Queue of granted locks and FIFO waiters for a single lockable object
/// (one page_id for record locks, or one table_id for table locks).
///
/// Inspired by PostgreSQL's `LOCK` struct which has separate `procLocks`
/// (granted) and `waitProcs` (waiting) lists, and InnoDB's per-cell
/// hash chain with granted-before-waiters ordering.
#[derive(Debug, Default)]
pub struct LockQueue {
    pub granted: Vec<LockEntry>,
    pub waiting: VecDeque<LockWaiter>,
}

impl LockQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if any granted lock from a **different** transaction conflicts
    /// with the requested mode on the given slot.
    ///
    /// For record locks: only conflicts if the other entry's bitmap overlaps
    /// on the specific slot AND the modes are incompatible.
    ///
    /// For table locks: `slot_id` is `None`, all granted entries are checked.
    ///
    /// Returns the `TxnId` of the first conflicting holder, or `None`.
    pub fn find_conflict(
        &self,
        request_txn: TxnId,
        request_mode: LockMode,
        slot_id: Option<u16>,
    ) -> Option<TxnId> {
        for entry in &self.granted {
            if entry.txn_id == request_txn {
                continue; // same transaction — no self-conflict
            }
            if !conflicts(entry.mode, request_mode) {
                continue; // compatible modes
            }
            // For record locks, check bitmap overlap on the specific slot.
            if let (Some(sid), Some(ref bm)) = (slot_id, &entry.bitmap) {
                if !bm.overlaps_slot(sid) {
                    continue; // different slots on same page — no conflict
                }
            }
            return Some(entry.txn_id);
        }
        None
    }

    /// Finds the index of an existing granted entry for the given transaction
    /// and mode. Used for bitmap extension (adding a slot to an existing lock)
    /// and lock upgrade detection.
    pub fn find_granted_idx(&self, txn_id: TxnId, mode: LockMode) -> Option<usize> {
        self.granted
            .iter()
            .position(|e| e.txn_id == txn_id && e.mode == mode)
    }

    /// Finds any granted entry for the given transaction (any mode).
    pub fn find_any_granted_idx(&self, txn_id: TxnId) -> Option<usize> {
        self.granted.iter().position(|e| e.txn_id == txn_id)
    }

    /// After releasing a lock, scans the waiting queue and promotes compatible
    /// waiters to the granted list. Returns the `Notify` handles to wake them.
    ///
    /// Multiple compatible waiters can be granted in one pass (e.g., multiple
    /// S locks after an X release). FIFO order is respected: we scan front to
    /// back and skip (leave in queue) any waiter that conflicts with the
    /// current granted set.
    ///
    /// **Important**: caller must drop the shard mutex BEFORE calling
    /// `notify.notify_one()` on the returned handles to avoid holding the
    /// mutex during task wake-up.
    pub fn try_grant_waiters(&mut self) -> Vec<Arc<Notify>> {
        let mut notifies = Vec::new();
        let mut i = 0;
        while i < self.waiting.len() {
            let waiter = &self.waiting[i];
            let slot_id = waiter
                .entry
                .bitmap
                .as_ref()
                .and_then(|bm| bm.first_set_bit());

            let has_conflict = self
                .find_conflict(waiter.entry.txn_id, waiter.entry.mode, slot_id)
                .is_some();

            if has_conflict {
                // Can't grant this waiter yet — skip but keep in queue.
                // Don't break: a later waiter with a different slot might
                // be grantable. (PostgreSQL does this too.)
                i += 1;
            } else {
                // Grant this waiter.
                let mut waiter = self.waiting.remove(i).unwrap();
                let notify = Arc::clone(&waiter.notify);
                waiter.entry.flags = waiter.entry.flags.difference(LockFlags::WAITING);
                self.granted.push(waiter.entry);
                notifies.push(notify);
                // Don't increment i — next waiter shifted into current position.
            }
        }
        notifies
    }

    /// Removes all granted entries for the given transaction.
    /// Returns the removed entries (for bitmap cleanup / stats).
    pub fn remove_granted_by_txn(&mut self, txn_id: TxnId) -> Vec<LockEntry> {
        let mut removed = Vec::new();
        self.granted.retain(|e| {
            if e.txn_id == txn_id {
                removed.push(e.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Removes a specific waiter for the given transaction.
    /// Used by timeout/deadlock abort.
    pub fn remove_waiter_by_txn(&mut self, txn_id: TxnId) -> Option<LockWaiter> {
        let pos = self.waiting.iter().position(|w| w.entry.txn_id == txn_id)?;
        self.waiting.remove(pos)
    }

    /// Returns `true` if both granted and waiting lists are empty.
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty() && self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(txn: TxnId, mode: LockMode, slot: u16) -> LockEntry {
        LockEntry {
            txn_id: txn,
            mode,
            flags: LockFlags::NONE,
            requested_at: Instant::now(),
            bitmap: Some(SlotBitmap::with_slot(slot)),
        }
    }

    fn table_entry(txn: TxnId, mode: LockMode) -> LockEntry {
        LockEntry {
            txn_id: txn,
            mode,
            flags: LockFlags::TABLE_LOCK,
            requested_at: Instant::now(),
            bitmap: None,
        }
    }

    #[test]
    fn no_self_conflict() {
        let mut q = LockQueue::new();
        q.granted.push(entry(1, LockMode::Exclusive, 5));
        // Same txn requesting X on same slot — no conflict.
        assert!(q.find_conflict(1, LockMode::Exclusive, Some(5)).is_none());
    }

    #[test]
    fn s_s_no_conflict() {
        let mut q = LockQueue::new();
        q.granted.push(entry(1, LockMode::Shared, 5));
        assert!(q.find_conflict(2, LockMode::Shared, Some(5)).is_none());
    }

    #[test]
    fn s_x_conflict() {
        let mut q = LockQueue::new();
        q.granted.push(entry(1, LockMode::Shared, 5));
        assert_eq!(q.find_conflict(2, LockMode::Exclusive, Some(5)), Some(1));
    }

    #[test]
    fn different_slots_no_conflict() {
        let mut q = LockQueue::new();
        q.granted.push(entry(1, LockMode::Exclusive, 5));
        // Different slot on same page — no conflict.
        assert!(q.find_conflict(2, LockMode::Exclusive, Some(10)).is_none());
    }

    #[test]
    fn table_lock_conflict() {
        let mut q = LockQueue::new();
        q.granted.push(table_entry(1, LockMode::IntentionExclusive));
        // IX + S conflict at table level.
        assert_eq!(q.find_conflict(2, LockMode::Shared, None), Some(1));
    }

    #[test]
    fn try_grant_waiters_promotes_compatible() {
        let mut q = LockQueue::new();
        // Txn 1 holds X on slot 5.
        q.granted.push(entry(1, LockMode::Exclusive, 5));
        // Txn 2 waits for S on slot 5.
        let notify2 = Arc::new(Notify::new());
        q.waiting.push_back(LockWaiter {
            entry: {
                let mut e = entry(2, LockMode::Shared, 5);
                e.flags = LockFlags::WAITING;
                e
            },
            notify: Arc::clone(&notify2),
            abort_reason: None,
        });
        // Txn 3 waits for S on slot 10 (different slot — should be grantable
        // even while X on slot 5 is held, but our conflict check uses
        // find_conflict which checks bitmap overlap).
        let notify3 = Arc::new(Notify::new());
        q.waiting.push_back(LockWaiter {
            entry: {
                let mut e = entry(3, LockMode::Shared, 10);
                e.flags = LockFlags::WAITING;
                e
            },
            notify: Arc::clone(&notify3),
            abort_reason: None,
        });

        // With X on slot 5 still held: waiter on slot 5 can't be granted,
        // waiter on slot 10 can.
        let notified = q.try_grant_waiters();
        assert_eq!(notified.len(), 1); // only slot 10 granted
        assert_eq!(q.waiting.len(), 1); // slot 5 still waiting
        assert_eq!(q.granted.len(), 2); // original X + new S on slot 10

        // Now release X on slot 5.
        q.remove_granted_by_txn(1);
        let notified = q.try_grant_waiters();
        assert_eq!(notified.len(), 1); // slot 5 S now granted
        assert!(q.waiting.is_empty());
        assert_eq!(q.granted.len(), 2); // S on slot 10 + S on slot 5
    }

    #[test]
    fn remove_granted_by_txn() {
        let mut q = LockQueue::new();
        q.granted.push(entry(1, LockMode::Shared, 5));
        q.granted.push(entry(1, LockMode::Shared, 10));
        q.granted.push(entry(2, LockMode::Shared, 5));
        let removed = q.remove_granted_by_txn(1);
        assert_eq!(removed.len(), 2);
        assert_eq!(q.granted.len(), 1);
        assert_eq!(q.granted[0].txn_id, 2);
    }
}
