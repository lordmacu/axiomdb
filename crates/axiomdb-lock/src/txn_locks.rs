use axiomdb_core::TxnId;

/// Reference to a held lock — stored per-transaction for bulk release.
///
/// On COMMIT/ROLLBACK, the executor iterates the `TxnLockTracker`'s held
/// list and calls `LockManager::release_all_locks` with these refs. Each
/// ref tells the manager which shard to access without scanning all 80.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockRef {
    Record { page_id: u64, slot_id: u16 },
    Table { table_id: u32 },
}

/// Tracks all locks held by a single transaction.
///
/// Embedded in the per-connection transaction state (`ConnectionTxn` in
/// 40.11). Cheap to construct (empty Vec) and cheap to iterate on release.
#[derive(Debug)]
pub struct TxnLockTracker {
    txn_id: TxnId,
    held: Vec<LockRef>,
}

impl TxnLockTracker {
    pub fn new(txn_id: TxnId) -> Self {
        Self {
            txn_id,
            held: Vec::new(),
        }
    }

    /// Records a newly acquired lock.
    pub fn track(&mut self, lock_ref: LockRef) {
        self.held.push(lock_ref);
    }

    /// Drains all held lock refs for release. Resets the tracker.
    pub fn drain(&mut self) -> Vec<LockRef> {
        std::mem::take(&mut self.held)
    }

    pub fn txn_id(&self) -> TxnId {
        self.txn_id
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Returns a slice of held lock refs (for inspection/deadlock detection).
    pub fn held(&self) -> &[LockRef] {
        &self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_and_drain() {
        let mut t = TxnLockTracker::new(42);
        assert!(t.is_empty());
        t.track(LockRef::Table { table_id: 1 });
        t.track(LockRef::Record {
            page_id: 100,
            slot_id: 5,
        });
        assert_eq!(t.len(), 2);
        let drained = t.drain();
        assert_eq!(drained.len(), 2);
        assert!(t.is_empty());
    }
}
