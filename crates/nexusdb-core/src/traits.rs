use crate::Result;

/// Page identifier in the storage.
pub type PageId = u64;

/// Row identifier (page + slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    pub page_id: PageId,
    pub slot_id: u16,
}

/// Transaction identifier.
pub type TxnId = u64;

/// Snapshot of the committed transaction state at a point in time.
///
/// Used by MVCC visibility checks to determine which rows a transaction can see.
///
/// `snapshot_id` = max_committed_txn_id + 1 at the moment this snapshot was taken.
/// A row created by txn C is visible if `C < snapshot_id` (C was committed before the snapshot).
///
/// `current_txn_id` = the txn_id of the active transaction.
/// `0` means autocommit / read-only: the snapshot sees only committed data and
/// cannot observe its own writes (no in-progress writes to see).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionSnapshot {
    pub snapshot_id: TxnId,
    pub current_txn_id: TxnId,
}

impl TransactionSnapshot {
    /// Creates a snapshot that sees all data committed up to and including
    /// `max_committed`. Used for autocommit read operations.
    pub fn committed(max_committed: TxnId) -> Self {
        Self {
            snapshot_id: max_committed.saturating_add(1),
            current_txn_id: 0,
        }
    }

    /// Creates a snapshot for an active transaction with the given txn_id.
    /// The transaction sees all data committed before it started (`max_committed`)
    /// plus its own in-progress writes.
    pub fn active(txn_id: TxnId, max_committed_at_start: TxnId) -> Self {
        Self {
            snapshot_id: max_committed_at_start.saturating_add(1),
            current_txn_id: txn_id,
        }
    }
}

/// Index trait — implementations: BTreeIndex, HashIndex, HnswIndex, FtsIndex.
/// Note: StorageEngine lives in nexusdb-storage (uses Page/PageType, avoids dep cycle).
pub trait Index: Send + Sync {
    fn insert(&self, key: &[u8], rid: RecordId) -> Result<()>;
    fn delete(&self, key: &[u8], rid: RecordId) -> Result<()>;
    fn lookup(&self, key: &[u8]) -> Result<Vec<RecordId>>;
    fn range(
        &self,
        lo: std::ops::Bound<&[u8]>,
        hi: std::ops::Bound<&[u8]>,
    ) -> Result<Vec<RecordId>>;
}
