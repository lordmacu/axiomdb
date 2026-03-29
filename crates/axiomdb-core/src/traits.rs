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

// ── Isolation Level ──────────────────────────────────────────────────────────

/// Transaction isolation level (Phase 7.1).
///
/// Controls the snapshot lifetime policy:
/// - `ReadCommitted`: fresh snapshot per statement (sees latest committed data).
/// - `RepeatableRead`: frozen snapshot at `BEGIN` (sees only what was committed
///   before the transaction started).
/// - `Serializable`: accepted and stored, but uses the same snapshot policy as
///   `RepeatableRead` because single-writer already prevents write skew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationLevel {
    /// Returns the MySQL-compatible wire string for `@@transaction_isolation`.
    pub fn as_mysql_str(&self) -> &'static str {
        match self {
            Self::ReadCommitted => "READ-COMMITTED",
            Self::RepeatableRead => "REPEATABLE-READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }

    /// Parses a MySQL-compatible isolation level string.
    ///
    /// `READ UNCOMMITTED` is silently upgraded to `READ COMMITTED` (MySQL behavior).
    pub fn parse(s: &str) -> Option<Self> {
        let normalized = s.trim().replace(['-', '_'], " ").to_ascii_uppercase();
        match normalized.as_str() {
            "READ UNCOMMITTED" | "READ COMMITTED" => Some(Self::ReadCommitted),
            "REPEATABLE READ" => Some(Self::RepeatableRead),
            "SERIALIZABLE" => Some(Self::Serializable),
            _ => None,
        }
    }

    /// Returns `true` if this level uses a frozen snapshot (taken at BEGIN).
    pub fn uses_frozen_snapshot(&self) -> bool {
        matches!(self, Self::RepeatableRead | Self::Serializable)
    }
}

impl Default for IsolationLevel {
    /// MySQL default is REPEATABLE READ.
    fn default() -> Self {
        Self::RepeatableRead
    }
}

impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_mysql_str())
    }
}

/// Index trait — implementations: BTreeIndex, HashIndex, HnswIndex, FtsIndex.
/// Note: StorageEngine lives in axiomdb-storage (uses Page/PageType, avoids dep cycle).
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
