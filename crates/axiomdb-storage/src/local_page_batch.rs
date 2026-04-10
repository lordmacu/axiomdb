//! Per-connection page allocation batch (Phase 40.9 — FreeList Tier-1).
//!
//! This struct lives in `axiomdb-storage` (not `axiomdb-wal`) so that
//! hot-path functions in `axiomdb-storage` and `axiomdb-index` can accept
//! `Option<&mut LocalPageBatch>` without creating a dependency cycle.
//!
//! The `ConnectionTxn` in `axiomdb-wal` owns one instance and passes it
//! through to storage-layer operations via the executor.

use std::collections::VecDeque;

use axiomdb_core::error::DbError;

use crate::engine::StorageEngine;
use crate::page::PageType;

/// Pages allocated per batch refill from the global bitmap (InnoDB extent size).
pub const BATCH_ALLOC_SIZE: usize = 64;

/// Upper cap on the adaptive batch size.
pub const MAX_BATCH_SIZE: usize = 256;

/// Per-transaction page allocation batch.
///
/// Eliminates per-allocation contention on the global `Mutex<FreeList>` by
/// caching pre-allocated page IDs locally. The global allocator is touched
/// only once every `BATCH_ALLOC_SIZE` allocations (amortized).
///
/// ## Homogeneous batches (PostgreSQL `BulkInsertState` model)
///
/// `current_type` tracks which `PageType` the batch was last refilled for.
/// A type mismatch on `pop_or_refill` drains the existing batch back to the
/// bitmap and refills with the new type.
///
/// ## Adaptive sizing (PostgreSQL `RelationAddBlocks` model)
///
/// `last_refill_size` remembers the batch size from the last refill. When
/// other threads are blocked on the freelist mutex, the next refill scales
/// up by the waiter count, capped at `MAX_BATCH_SIZE`.
#[derive(Debug)]
pub struct LocalPageBatch {
    available: VecDeque<u64>,
    current_type: Option<PageType>,
    freed: Vec<u64>,
    last_refill_size: usize,
}

impl Default for LocalPageBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalPageBatch {
    /// Creates an empty batch. No global allocator interaction.
    pub fn new() -> Self {
        Self {
            available: VecDeque::new(),
            current_type: None,
            freed: Vec::new(),
            last_refill_size: BATCH_ALLOC_SIZE,
        }
    }

    /// Allocates one page ID from the local batch, refilling from the global
    /// allocator when needed. The caller is responsible for page initialization.
    pub fn pop_or_refill(
        &mut self,
        storage: &dyn StorageEngine,
        ty: PageType,
    ) -> Result<u64, DbError> {
        // Type mismatch — drain existing batch back to the bitmap.
        if self.current_type != Some(ty) {
            if !self.available.is_empty() {
                let drained: Vec<u64> = self.available.drain(..).collect();
                storage.free_page_batch(&drained)?;
            }
            self.current_type = Some(ty);
            self.last_refill_size = BATCH_ALLOC_SIZE;
        }

        // Fast path: pop from local batch.
        if let Some(id) = self.available.pop_front() {
            return Ok(id);
        }

        // Slow path: refill from global allocator with adaptive sizing.
        let waiters = storage.extension_waiters() as usize;
        let n = (self.last_refill_size * (waiters + 1)).clamp(BATCH_ALLOC_SIZE, MAX_BATCH_SIZE);
        let ids = storage.alloc_page_batch(n, ty)?;
        if ids.is_empty() {
            return Err(DbError::StorageFull);
        }
        self.last_refill_size = n;
        let first = ids[0];
        self.available.extend(ids.into_iter().skip(1));
        Ok(first)
    }

    /// Records a freed page for later return to the global allocator.
    pub fn push_freed(&mut self, page_id: u64) {
        self.freed.push(page_id);
    }

    /// Drains the batch for a COMMIT. Returns `(available, freed)`.
    pub fn take_for_commit(&mut self) -> (Vec<u64>, Vec<u64>) {
        let avail: Vec<u64> = self.available.drain(..).collect();
        let freed = std::mem::take(&mut self.freed);
        self.current_type = None;
        self.last_refill_size = BATCH_ALLOC_SIZE;
        (avail, freed)
    }

    /// Drains the batch for a ROLLBACK. Returns `available` only.
    pub fn take_for_rollback(&mut self) -> Vec<u64> {
        let avail: Vec<u64> = self.available.drain(..).collect();
        self.freed.clear();
        self.current_type = None;
        self.last_refill_size = BATCH_ALLOC_SIZE;
        avail
    }

    /// Number of pages in the freed list. Used by `Savepoint`.
    pub fn freed_len(&self) -> usize {
        self.freed.len()
    }

    /// Truncates `freed` to `len`, discarding entries after a savepoint.
    pub fn truncate_freed(&mut self, len: usize) {
        self.freed.truncate(len);
    }
}

/// Allocates a page from the local batch if available, falling back to
/// `storage.alloc_page(ty)` when `batch` is `None`.
///
/// This is the canonical entry point for hot-path code in `axiomdb-storage`
/// and `axiomdb-index` that wants to benefit from batched allocation without
/// a hard dependency on the WAL crate.
#[inline]
pub fn batch_alloc_page(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    ty: PageType,
) -> Result<u64, DbError> {
    match batch {
        Some(b) => b.pop_or_refill(storage, ty),
        None => storage.alloc_page(ty),
    }
}
