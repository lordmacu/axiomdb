/// Bundles the three subsystem references needed by every executor function.
///
/// Inspired by DataFusion's `TaskContext` and DuckDB's `ClientContext`:
/// single allocation, no `Arc`.
///
/// ## Design rationale
///
/// Previously executor functions received `(storage, txn, bloom)` as three
/// separate parameters — 3 borrows per function call, 100+ call sites. Bundling
/// them into a single `&ExecutionContext` reference reduces the signature width
/// and enables Phase 40.11 to add `LockManager` with a single field addition and
/// zero further signature sweeps.
///
/// ## Mutability model
///
/// `StorageEngine` uses full interior mutability (Phase 40.3) — all trait
/// methods take `&self`.
///
/// `TxnManager` operations (`begin`, `commit`, `rollback`, `record_index_insert`,
/// `record_index_delete`, `active_snapshot`, `snapshot`, `savepoint`,
/// `rollback_to_savepoint`) all take `&self`.
///
/// `BloomRegistry` uses interior mutability — `add` and `mark_dirty` take `&self`.
///
/// All three subsystems support shared (`&`) access, so `ExecutionContext` stores
/// plain shared references with no unsafe required.
///
/// ## Phase 40.11 extension
///
/// Adding `lock_mgr` requires one new field here — no further signature sweeps.
pub struct ExecutionContext<'a> {
    storage: &'a dyn axiomdb_storage::StorageEngine,
    coord: &'a axiomdb_wal::TxnManager,
    bloom: &'a crate::bloom::BloomRegistry,
}

impl<'a> ExecutionContext<'a> {
    /// Constructs an execution context from shared subsystem references.
    pub fn new(
        storage: &'a dyn axiomdb_storage::StorageEngine,
        coord: &'a axiomdb_wal::TxnManager,
        bloom: &'a crate::bloom::BloomRegistry,
    ) -> Self {
        Self {
            storage,
            coord,
            bloom,
        }
    }

    /// Returns a shared reference to the storage engine.
    #[inline]
    pub fn storage(&self) -> &'a dyn axiomdb_storage::StorageEngine {
        self.storage
    }

    /// Returns a shared reference to the transaction manager.
    #[inline]
    pub fn coord(&self) -> &'a axiomdb_wal::TxnManager {
        self.coord
    }

    /// Returns a shared reference to the Bloom filter registry.
    #[inline]
    pub fn bloom(&self) -> &'a crate::bloom::BloomRegistry {
        self.bloom
    }
}
