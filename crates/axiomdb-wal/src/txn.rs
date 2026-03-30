//! Transaction manager — coordinates BEGIN / COMMIT / ROLLBACK.
//!
//! ## Responsibilities
//!
//! - Assigns globally monotonic [`TxnId`]s.
//! - Buffers WAL entries for the active transaction (fsynced only on COMMIT).
//! - Maintains an **undo log** per transaction: each DML records the inverse
//!   operation needed to restore the heap pages if the transaction is rolled back.
//! - Tracks `max_committed` — the TxnId of the last committed transaction.
//!   Used to construct [`TransactionSnapshot`]s for MVCC visibility checks.
//!
//! ## Single-writer constraint (Phase 3)
//!
//! At most one explicit transaction can be active at a time.
//! Concurrent readers use [`TxnManager::snapshot`] — which requires no locking
//! because `max_committed` only advances on commit, which requires `&mut self`.
//!
//! ## Autocommit
//!
//! Use [`TxnManager::autocommit`] to wrap a single operation in an implicit
//! BEGIN / COMMIT (with automatic ROLLBACK on error).

use std::path::Path;

use axiomdb_core::{error::DbError, TransactionSnapshot, TxnId};
use axiomdb_storage::{
    clear_deletion, heap_chain::HeapChain, mark_slot_dead, restore_tuple_image, Page,
    StorageEngine, WalDurabilityPolicy,
};

use crate::{
    checkpoint::Checkpointer,
    entry::{EntryType, WalEntry},
    reader::WalReader,
    recovery::{CrashRecovery, RecoveryResult},
    writer::WalWriter,
};

// ── Savepoint ─────────────────────────────────────────────────────────────────

/// An in-memory statement-level savepoint.
///
/// Created by [`TxnManager::savepoint`] before executing a statement inside an
/// explicit transaction. Passing it to [`TxnManager::rollback_to_savepoint`]
/// undoes only that statement's writes, leaving the transaction active.
///
/// Savepoints are **not persisted** to the WAL — they are valid only within the
/// lifetime of the current `TxnManager` instance. Crash recovery handles
/// transactions at transaction granularity (full redo/undo), not statement level.
///
/// In Phase 5.16 the savepoint also records the length of the deferred-free
/// page queue so that bulk-empty pages allocated after the savepoint are
/// discarded (not freed) on `rollback_to_savepoint`.
#[derive(Debug, Clone, Copy)]
pub struct Savepoint {
    /// Index into `ActiveTxn::undo_ops` at savepoint creation time.
    pub(crate) undo_len: usize,
    /// Length of `ActiveTxn::deferred_free_pages` at savepoint creation time.
    pub(crate) deferred_free_len: usize,
}

// ── UndoOp ───────────────────────────────────────────────────────────────────

/// A single undo operation recorded for each DML within a transaction.
///
/// Applied in **reverse chronological order** on ROLLBACK to restore the
/// heap pages to their pre-transaction state.
#[derive(Debug, Clone)]
pub enum UndoOp {
    /// Undo an INSERT: zero out the slot entry so the row becomes dead.
    UndoInsert { page_id: u64, slot_id: u16 },
    /// Undo a DELETE: clear `txn_id_deleted` in the RowHeader (row is live again).
    UndoDelete { page_id: u64, slot_id: u16 },
    /// Undo a stable-RID in-place update by restoring the previous tuple image.
    UndoUpdateInPlace {
        page_id: u64,
        slot_id: u16,
        old_image: Vec<u8>,
    },
    // UPDATE is recorded as UndoInsert(new_slot) + UndoDelete(old_slot).
    // Reversed: UndoDelete(old_slot) runs first (restores old), then
    // UndoInsert(new_slot) (kills the replacement). Correct MVCC undo.
    /// Undo a full-table delete: scan the heap chain and clear txn_id_deleted
    /// for every slot deleted by this transaction.
    UndoTruncate { root_page_id: u64 },
    /// Undo an index INSERT: remove the entry from the B-Tree (Phase 7.3b).
    ///
    /// Recorded when INSERT or UPDATE adds a new secondary index entry.
    /// On ROLLBACK, the entry is deleted from the B-Tree so the index
    /// returns to its pre-transaction state. The `root_page_id` is captured
    /// at recording time to avoid catalog lookups during undo.
    UndoIndexInsert {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    },
}

// ── ActiveTxn ────────────────────────────────────────────────────────────────

struct ActiveTxn {
    txn_id: TxnId,
    /// Snapshot id captured at BEGIN: used for read-your-own-writes during the txn.
    snapshot_id_at_begin: u64,
    /// Isolation level for this transaction (Phase 7.1).
    /// Controls whether `active_snapshot()` returns the frozen BEGIN snapshot
    /// (REPEATABLE READ / SERIALIZABLE) or a fresh per-statement snapshot
    /// (READ COMMITTED).
    isolation_level: axiomdb_core::IsolationLevel,
    /// Undo ops in chronological order; applied last-to-first on rollback.
    undo_ops: Vec<UndoOp>,
    /// Pages to free **after** this transaction is durably committed.
    ///
    /// Populated by `defer_free_pages(...)` during bulk-empty operations (Phase 5.16).
    /// On `rollback` or `rollback_to_savepoint`, these pages are NOT freed — the
    /// catalog undo already restored the old roots, so the old pages remain live.
    /// On `commit`, this list moves to `TxnManager::committed_free_batches` keyed
    /// by `txn_id` and is freed only after `release_committed_frees(...)` is called.
    deferred_free_pages: Vec<u64>,
}

// ── TxnManager ───────────────────────────────────────────────────────────────

/// Coordinates the transaction lifecycle over the WAL and heap pages.
pub struct TxnManager {
    wal: WalWriter,
    next_txn_id: u64,
    max_committed: u64,
    active: Option<ActiveTxn>,
    /// Reusable scratch buffer for WAL entry serialization.
    ///
    /// Passed to `WalWriter::append_with_buf()` to avoid a fresh Vec allocation
    /// per DML operation. Capacity grows to the largest entry seen and is
    /// retained across operations — inspired by LMDB's approach of reusing
    /// a single write buffer for all modifications in a batch.
    wal_scratch: Vec<u8>,
    /// When `true`, DML `commit()` skips inline flush+fsync and stores the
    /// committed `txn_id` in `pending_deferred_txn_id` for the caller to hand
    /// off to the leader-based WAL fsync pipeline. Read-only transactions still
    /// use the lightweight `flush_no_sync` path.
    deferred_commit_mode: bool,
    /// Set by `commit()` when `deferred_commit_mode` is true and the transaction
    /// contained DML. Cleared by `take_pending_deferred_commit()`.
    pending_deferred_txn_id: Option<TxnId>,
    /// Pages waiting to be freed after their transaction is durably committed.
    ///
    /// Each entry is `(txn_id, pages)`. Populated by `commit()` from
    /// `ActiveTxn::deferred_free_pages`. Released by `release_committed_frees(...)`
    /// after WAL fsync confirms durability, in both immediate and group-commit modes.
    committed_free_batches: Vec<(TxnId, Vec<u64>)>,
    /// WAL durability policy for committed DML.
    ///
    /// - `Strict` (default): full flush+sync before OK.
    /// - `Normal`: flush to OS page cache, no durable sync per commit.
    /// - `Off`: no per-commit barrier; benchmark/dev only.
    ///
    /// Set via [`set_durability_policy`]. Orthogonal to `deferred_commit_mode`.
    durability_policy: WalDurabilityPolicy,
}

impl TxnManager {
    // ── Construction ─────────────────────────────────────────────────────────

    /// Creates a fresh WAL file and a new TxnManager.
    ///
    /// Fails if the WAL file already exists.
    pub fn create(wal_path: &Path) -> Result<Self, DbError> {
        let wal = WalWriter::create(wal_path)?;
        Ok(Self {
            wal,
            next_txn_id: 1,
            max_committed: 0,
            active: None,
            wal_scratch: Vec::with_capacity(256),
            deferred_commit_mode: false,
            pending_deferred_txn_id: None,
            committed_free_batches: Vec::new(),
            durability_policy: WalDurabilityPolicy::Strict,
        })
    }

    /// Opens an existing WAL file, scanning it to recover `max_committed`.
    ///
    /// Does not replay DML entries — full crash recovery is handled in Phase 3.8.
    /// Only the highest committed TxnId is restored so that new transactions
    /// receive monotonically increasing IDs and snapshots are correct.
    pub fn open(wal_path: &Path) -> Result<Self, DbError> {
        let max_committed = scan_max_committed(wal_path)?;
        let wal = WalWriter::open(wal_path)?;
        Ok(Self {
            wal,
            next_txn_id: max_committed + 1,
            max_committed,
            active: None,
            wal_scratch: Vec::with_capacity(256),
            deferred_commit_mode: false,
            pending_deferred_txn_id: None,
            committed_free_batches: Vec::new(),
            durability_policy: WalDurabilityPolicy::Strict,
        })
    }

    // ── Transaction lifecycle ─────────────────────────────────────────────────

    /// Starts a new explicit transaction.
    ///
    /// Assigns the next monotonic [`TxnId`], writes a buffered Begin WAL entry,
    /// and initialises the undo log. Uses `RepeatableRead` isolation by default.
    ///
    /// # Errors
    /// - [`DbError::TransactionAlreadyActive`] if a transaction is already open.
    pub fn begin(&mut self) -> Result<TxnId, DbError> {
        self.begin_with_isolation(axiomdb_core::IsolationLevel::RepeatableRead)
    }

    /// Like [`begin`] but with an explicit isolation level (Phase 7.1).
    ///
    /// - `ReadCommitted`: `active_snapshot()` returns a fresh snapshot per call.
    /// - `RepeatableRead` / `Serializable`: `active_snapshot()` returns the
    ///   snapshot frozen at BEGIN.
    pub fn begin_with_isolation(
        &mut self,
        isolation_level: axiomdb_core::IsolationLevel,
    ) -> Result<TxnId, DbError> {
        if let Some(ref active) = self.active {
            return Err(DbError::TransactionAlreadyActive {
                txn_id: active.txn_id,
            });
        }

        let txn_id = self.next_txn_id;
        self.next_txn_id += 1;

        // Phase 7.15: transaction ID overflow prevention.
        // u64 gives ~1.8×10^19 IDs — at 1M txn/s this lasts 584,942 years.
        // Still, detect pathological usage early and warn before overflow.
        const TXN_ID_WARN_90: u64 = u64::MAX / 10 * 9; // 90% capacity
        const TXN_ID_WARN_50: u64 = u64::MAX / 2; // 50% capacity
        if txn_id >= TXN_ID_WARN_90 {
            tracing::error!(
                txn_id,
                "CRITICAL: transaction ID at 90% of u64 capacity — VACUUM FREEZE required"
            );
        } else if txn_id >= TXN_ID_WARN_50 {
            tracing::warn!(
                txn_id,
                "transaction ID at 50% of u64 capacity — plan VACUUM FREEZE"
            );
        }

        let mut entry = WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![]);
        self.wal.append(&mut entry)?;

        self.active = Some(ActiveTxn {
            txn_id,
            snapshot_id_at_begin: self.max_committed + 1,
            isolation_level,
            undo_ops: Vec::new(),
            deferred_free_pages: Vec::new(),
        });
        Ok(txn_id)
    }

    /// Commits the active transaction: writes the Commit WAL entry and either
    /// fsyncs inline or hands the commit off to the WAL fsync pipeline.
    ///
    /// Advances `max_committed` to the committed TxnId, making the transaction's
    /// writes visible to future [`TransactionSnapshot`]s.
    ///
    /// When `deferred_commit_mode` is enabled (used by the fsync pipeline in the
    /// server path), DML commits skip the inline fsync and store the txn_id in
    /// `pending_deferred_txn_id`. The caller retrieves it with
    /// `take_pending_deferred_commit()` and drives the WAL fsync pipeline, which
    /// advances `max_committed` only after durability is confirmed.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    /// - I/O errors from WAL write or fsync.
    pub fn commit(&mut self) -> Result<(), DbError> {
        let active = self.active.take().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;
        let deferred_pages = active.deferred_free_pages;

        let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;

        if active.undo_ops.is_empty() {
            // Read-only transaction: flush to OS page cache (visible to
            // readers/recovery) but skip the expensive fsync (~10-20ms).
            // No heap data was modified, so OS-level durability is sufficient.
            self.wal.flush_no_sync()?;
            self.max_committed = txn_id;
        } else {
            match self.durability_policy {
                WalDurabilityPolicy::Strict => {
                    if self.deferred_commit_mode {
                        // Pipeline mode: Commit entry is in the BufWriter but NOT
                        // flushed or fsynced. max_committed does NOT advance here —
                        // it advances only after the pipeline leader confirms fsync.
                        self.pending_deferred_txn_id = Some(txn_id);
                    } else {
                        // Immediate mode: full flush + fsync for durability.
                        self.wal.commit_data_sync()?;
                        self.max_committed = txn_id;
                    }
                }
                WalDurabilityPolicy::Normal => {
                    // Flush WAL bytes to OS page cache — visible to readers and
                    // crash recovery, but NOT durable across power loss.
                    self.wal.flush_no_sync()?;
                    self.max_committed = txn_id;
                }
                WalDurabilityPolicy::Off => {
                    // No per-commit barrier. The BufWriter holds the data in
                    // user-space; it will reach the OS on the next flush or when
                    // the buffer fills. Benchmark/dev only.
                    self.max_committed = txn_id;
                }
            }
        }

        // Register deferred-free pages (if any) for post-commit reclamation.
        // Pages are only freed after `release_committed_frees(txn_id)` confirms
        // WAL durability — never before.
        if !deferred_pages.is_empty() {
            self.committed_free_batches.push((txn_id, deferred_pages));
        }

        Ok(())
    }

    /// Enables or disables deferred commit mode for the server-side fsync pipeline.
    ///
    /// When enabled, DML `commit()` skips inline flush+fsync and stores the
    /// txn_id in `pending_deferred_txn_id` for the caller to hand off to the
    /// leader-based pipeline.
    pub fn set_deferred_commit_mode(&mut self, enabled: bool) {
        self.deferred_commit_mode = enabled;
    }

    /// Sets the WAL durability policy for committed DML.
    ///
    /// Call this once during database open, before any transactions.
    /// The policy is orthogonal to `deferred_commit_mode`.
    pub fn set_durability_policy(&mut self, policy: WalDurabilityPolicy) {
        self.durability_policy = policy;
    }

    /// Returns the current WAL durability policy.
    pub fn durability_policy(&self) -> WalDurabilityPolicy {
        self.durability_policy
    }

    /// Takes the pending deferred commit txn_id, if any.
    ///
    /// Returns `Some(txn_id)` if the last `commit()` was a DML transaction in
    /// deferred mode (the Commit entry is in the BufWriter but not fsynced).
    /// Returns `None` if the last commit was read-only or deferred mode is off.
    ///
    /// Called by `Database::execute_query` to hand the txn to the fsync
    /// pipeline after statement execution.
    pub fn take_pending_deferred_commit(&mut self) -> Option<TxnId> {
        self.pending_deferred_txn_id.take()
    }

    /// Advances `max_committed` to the maximum of the given txn_ids.
    ///
    /// Called after a successful pipeline-driven `wal_flush_and_fsync()`, while
    /// holding the Database lock. Makes all transactions in the batch visible
    /// to future snapshots.
    ///
    /// Does not regress `max_committed` — if `max(txn_ids) < self.max_committed`,
    /// no change is made (safe for out-of-order batch notification, though in
    /// practice batches are always monotone under the single-writer constraint).
    pub fn advance_committed(&mut self, txn_ids: &[TxnId]) {
        if let Some(&max) = txn_ids.iter().max() {
            if max > self.max_committed {
                self.max_committed = max;
            }
        }
    }

    /// Advances `max_committed` to `txn_id` if it is greater than the current
    /// value. Used by the fsync pipeline leader to make a single transaction
    /// visible after confirming WAL durability.
    pub fn advance_committed_single(&mut self, txn_id: TxnId) {
        if txn_id > self.max_committed {
            self.max_committed = txn_id;
        }
    }

    /// Returns the WAL writer's current LSN (the last assigned LSN).
    ///
    /// Used by the fsync pipeline to track which LSN was last fsynced.
    pub fn wal_current_lsn(&self) -> u64 {
        self.wal.current_lsn()
    }

    /// Enqueues `pages` for deferred reclamation after the current transaction
    /// is durably committed.
    ///
    /// Must be called **inside an active transaction**. The pages are moved to
    /// `committed_free_batches` on `commit()` and physically freed only when
    /// `release_committed_frees(...)` is called with a matching `txn_id`.
    ///
    /// On `rollback` or `rollback_to_savepoint`, deferred pages are simply
    /// discarded — the catalog undo restores the old roots so old pages remain live.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    pub fn defer_free_pages(
        &mut self,
        pages: impl IntoIterator<Item = u64>,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        active.deferred_free_pages.extend(pages);
        Ok(())
    }

    /// Frees pages whose transactions have been durably committed.
    ///
    /// Called after WAL fsync succeeds (immediate mode: right after `commit()`;
    /// pipeline mode: after `advance_committed(&ids)` in the fsync leader path).
    ///
    /// Pages are freed via `storage.free_page(pid)`. Any `txn_id` in `txn_ids`
    /// that has no pending batch is silently ignored.
    ///
    /// # Errors
    /// - I/O errors from `storage.free_page(...)`.
    pub fn release_committed_frees(
        &mut self,
        storage: &mut dyn StorageEngine,
        txn_ids: &[TxnId],
    ) -> Result<(), DbError> {
        if txn_ids.is_empty() || self.committed_free_batches.is_empty() {
            return Ok(());
        }
        let id_set: std::collections::HashSet<TxnId> = txn_ids.iter().copied().collect();
        let mut remaining = Vec::with_capacity(self.committed_free_batches.len());
        for (txn_id, pages) in self.committed_free_batches.drain(..) {
            if id_set.contains(&txn_id) {
                for pid in pages {
                    // Best-effort: ignore double-free errors (page already freed
                    // by earlier recovery or duplicate call).
                    let _ = storage.free_page(pid);
                }
            } else {
                remaining.push((txn_id, pages));
            }
        }
        self.committed_free_batches = remaining;
        Ok(())
    }

    /// Releases deferred-free pages for `txn_id` only in immediate-commit mode.
    ///
    /// In pipeline mode this is a no-op — the fsync leader path calls
    /// [`release_committed_frees`] after batch fsync confirms durability.
    ///
    /// Call this right after a successful `txn.commit()` in immediate-commit paths,
    /// passing the txn_id captured from `active_txn_id()` before the commit call.
    ///
    /// [`release_committed_frees`]: TxnManager::release_committed_frees
    pub fn release_immediate_committed_frees(
        &mut self,
        storage: &mut dyn StorageEngine,
        txn_id: TxnId,
    ) -> Result<(), DbError> {
        if !self.deferred_commit_mode {
            self.release_committed_frees(storage, &[txn_id])?;
        }
        Ok(())
    }

    /// Flushes the WAL BufWriter to the OS and performs the steady-state
    /// durable data sync.
    ///
    /// Called by the fsync pipeline leader while holding the Database lock,
    /// covering all Commit entries written since the last fsync.
    ///
    /// # Errors
    /// - I/O errors from flush or durable sync propagated to all batch waiters.
    pub fn wal_flush_and_fsync(&mut self) -> Result<(), DbError> {
        self.wal.commit_data_sync()
    }

    /// Rolls back the active transaction: undoes heap changes and writes a
    /// Rollback WAL entry (not fsynced — rolled-back data is intentionally ephemeral).
    ///
    /// Captures the current undo log position as a statement-level savepoint.
    ///
    /// The returned `Savepoint` can be passed to [`rollback_to_savepoint`] to undo
    /// only the operations recorded *after* this call, leaving the transaction active.
    ///
    /// Call this **before** executing each statement inside an explicit transaction
    /// to implement MySQL-style statement-level rollback on error.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if called outside an active transaction.
    ///
    /// [`rollback_to_savepoint`]: TxnManager::rollback_to_savepoint
    pub fn savepoint(&self) -> Savepoint {
        debug_assert!(
            self.active.is_some(),
            "savepoint() called outside an active transaction"
        );
        let (undo_len, deferred_free_len) = self
            .active
            .as_ref()
            .map(|a| (a.undo_ops.len(), a.deferred_free_pages.len()))
            .unwrap_or((0, 0));
        Savepoint {
            undo_len,
            deferred_free_len,
        }
    }

    /// Undoes all operations recorded **after** `sp`, leaving the transaction active.
    ///
    /// This implements MySQL's statement-level rollback semantics: when a statement
    /// errors inside an explicit transaction, only that statement's writes are
    /// undone. The transaction remains open; subsequent statements can execute.
    ///
    /// Undo ops are applied in reverse order (last write first), identical to the
    /// full `rollback()` path but scoped to `sp.0..undo_ops.len()`.
    ///
    /// # Errors
    ///
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - I/O errors from undo writes.
    pub fn rollback_to_savepoint(
        &mut self,
        sp: Savepoint,
        storage: &mut dyn StorageEngine,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Discard deferred-free pages recorded after this savepoint.
        // Catalog undo (below) restores old roots, so old pages remain live.
        active.deferred_free_pages.truncate(sp.deferred_free_len);

        // Drain only the undo ops recorded after the savepoint.
        let ops_to_undo: Vec<UndoOp> = active.undo_ops.drain(sp.undo_len..).rev().collect();
        for op in ops_to_undo {
            match op {
                UndoOp::UndoInsert { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    mark_slot_dead(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoDelete { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    clear_deletion(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoUpdateInPlace {
                    page_id,
                    slot_id,
                    old_image,
                } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    restore_tuple_image(&mut page, slot_id, &old_image)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoTruncate { root_page_id } => {
                    HeapChain::clear_deletions_by_txn(storage, root_page_id, txn_id)?;
                }
                UndoOp::UndoIndexInsert { .. } => {
                    // Handled by caller via pending_index_undos().
                    // TxnManager cannot depend on axiomdb-index.
                }
            }
        }
        Ok(())
    }

    /// Applies undo operations in **reverse chronological order**:
    /// - `UndoInsert`: marks the slot dead (row hidden from all future snapshots).
    /// - `UndoDelete`: clears `txn_id_deleted` (row is live again).
    ///
    /// Does **not** advance `max_committed`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    /// - I/O errors from undo writes or WAL append.
    pub fn rollback(&mut self, storage: &mut dyn StorageEngine) -> Result<(), DbError> {
        let active = self.active.take().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Write Rollback entry — informational for crash recovery. No fsync.
        let mut entry = WalEntry::new(0, txn_id, EntryType::Rollback, 0, vec![], vec![], vec![]);
        self.wal.append(&mut entry)?;

        // Discard deferred-free pages: catalog undo will restore old roots,
        // so the old pages remain live and must not be freed.
        // (deferred_free_pages is dropped with `active` after the loop.)

        // Apply undo ops in reverse (last DML first).
        for op in active.undo_ops.into_iter().rev() {
            match op {
                UndoOp::UndoInsert { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    mark_slot_dead(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoDelete { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    clear_deletion(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoUpdateInPlace {
                    page_id,
                    slot_id,
                    old_image,
                } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    restore_tuple_image(&mut page, slot_id, &old_image)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoTruncate { root_page_id } => {
                    HeapChain::clear_deletions_by_txn(storage, root_page_id, txn_id)?;
                }
                UndoOp::UndoIndexInsert { .. } => {
                    // Handled by caller via pending_index_undos().
                }
            }
        }
        // max_committed is unchanged — the rolled-back txn's inserts are invisible
        // to all future snapshots (txn_id_created >= snapshot_id for every future reader).
        Ok(())
    }

    // ── DML recording ────────────────────────────────────────────────────────

    // NOTE: record_* methods prepend PHYSICAL_LOC_LEN bytes to new_value (Insert/Update)
    // and old_value (Delete) so crash recovery can locate the heap slot without an
    // in-memory undo log.  See `PHYSICAL_LOC_LEN` and `decode_physical_loc`.

    /// Records an INSERT into the WAL and enqueues an undo operation.
    ///
    /// Must be called **after** the heap + index changes have been applied to storage.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_insert(
        &mut self,
        table_id: u32,
        key: &[u8],
        value: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Prepend physical location so crash recovery can undo without RAM state.
        let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
        new_value.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        new_value.extend_from_slice(value);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::Insert,
            table_id,
            key.to_vec(),
            vec![],
            new_value,
        );
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;
        active
            .undo_ops
            .push(UndoOp::UndoInsert { page_id, slot_id });
        Ok(())
    }

    /// Records N INSERTs into the WAL in a **single `write_all` call**.
    ///
    /// Equivalent to calling [`record_insert`] N times but uses
    /// [`WalWriter::reserve_lsns`] + [`WalWriter::write_batch`] to write all
    /// entries in one shot, reducing BufWriter call overhead from O(N) to O(1).
    ///
    /// `phys_locs[i]` and `values[i]` must correspond to the same row.
    /// Both slices must have the same length; a length mismatch is an internal
    /// error (caller invariant — never caused by user SQL).
    ///
    /// The entries written to disk are byte-for-byte identical to those produced
    /// by N calls to `record_insert` — crash recovery is unchanged.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_insert_batch(
        &mut self,
        table_id: u32,
        phys_locs: &[(u64, u16)], // (page_id, slot_id) per row
        values: &[Vec<u8>],       // encoded row bytes per row (same order as phys_locs)
    ) -> Result<(), DbError> {
        let n = phys_locs.len();
        debug_assert_eq!(
            n,
            values.len(),
            "record_insert_batch: phys_locs and values must have the same length"
        );
        if n == 0 {
            return Ok(());
        }

        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Reserve N consecutive LSNs atomically before serializing.
        let lsn_base = self.wal.reserve_lsns(n);

        // Accumulate all N entries into wal_scratch in one pass.
        // Clear once — do NOT clear between entries.
        self.wal_scratch.clear();

        for (i, ((page_id, slot_id), value)) in phys_locs.iter().zip(values.iter()).enumerate() {
            let key = encode_physical_loc(*page_id, *slot_id);

            // Prepend physical location to new_value (same as record_insert).
            let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
            new_value.extend_from_slice(&key);
            new_value.extend_from_slice(value);

            let entry = WalEntry::new(
                lsn_base + i as u64, // pre-assigned LSN
                txn_id,
                EntryType::Insert,
                table_id,
                key.to_vec(),
                vec![],
                new_value,
            );
            entry.serialize_into(&mut self.wal_scratch);
        }

        // Single write_all for all N entries.
        self.wal.write_batch(&self.wal_scratch)?;

        // Enqueue undo ops after the WAL write succeeds.
        for (page_id, slot_id) in phys_locs {
            active.undo_ops.push(UndoOp::UndoInsert {
                page_id: *page_id,
                slot_id: *slot_id,
            });
        }

        Ok(())
    }

    /// Records N bulk-insert pages into the WAL as compact `PageWrite` entries.
    ///
    /// Each element of `page_writes` is `(page_id, slot_ids)` where `slot_ids`
    /// lists the slots inserted by this transaction on that page.
    ///
    /// ## Compact WAL format
    ///
    /// `new_value = [num_slots: u16 LE][slot_id × num_slots: u16 LE each]`
    ///
    /// No page bytes are stored — crash recovery only needs the slot IDs to mark
    /// inserted slots dead on undo. Eliminating the 16 KB page image reduces WAL
    /// size from ~820 KB to ~20 KB per 10K-row batch (40× reduction).
    ///
    /// Inspired by MariaDB's InnoDB redo log (logical delta) and OceanBase's
    /// `ObDASWriteBuffer` (row-level buffering, not page-level snapshot).
    ///
    /// ## WAL ordering
    ///
    /// Uses `reserve_lsns + write_batch` for O(1) BufWriter calls — same
    /// pattern as `record_insert_batch`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_page_writes(
        &mut self,
        table_id: u32,
        page_writes: &[(u64, &[u16])], // (page_id, slot_ids)
    ) -> Result<(), DbError> {
        let n = page_writes.len();
        if n == 0 {
            return Ok(());
        }

        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        let lsn_base = self.wal.reserve_lsns(n);
        self.wal_scratch.clear();

        for (i, (page_id, slot_ids)) in page_writes.iter().enumerate() {
            let key = page_id.to_le_bytes();

            // Compact new_value: [num_slots: u16 LE][slot_id × N: u16 LE]
            // No page bytes — crash recovery only needs slot IDs for undo.
            let mut new_value = Vec::with_capacity(2 + slot_ids.len() * 2);
            new_value.extend_from_slice(&(slot_ids.len() as u16).to_le_bytes());
            for &slot_id in slot_ids.iter() {
                new_value.extend_from_slice(&slot_id.to_le_bytes());
            }

            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::PageWrite,
                table_id,
                key.to_vec(),
                vec![],
                new_value,
            );
            entry.serialize_into(&mut self.wal_scratch);
        }

        self.wal.write_batch(&self.wal_scratch)?;

        // Enqueue in-memory undo ops (used by ROLLBACK and group commit mode).
        for (page_id, slot_ids) in page_writes {
            for &slot_id in slot_ids.iter() {
                active.undo_ops.push(UndoOp::UndoInsert {
                    page_id: *page_id,
                    slot_id,
                });
            }
        }

        Ok(())
    }

    /// Records a DELETE into the WAL and enqueues an undo operation.
    ///
    /// Must be called **after** `txn_id_deleted` has been stamped in the RowHeader.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_delete(
        &mut self,
        table_id: u32,
        key: &[u8],
        old_value: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Prepend physical location to old_value for crash recovery.
        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_value.len());
        ov.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        ov.extend_from_slice(old_value);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::Delete,
            table_id,
            key.to_vec(),
            ov,
            vec![],
        );
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;
        active
            .undo_ops
            .push(UndoOp::UndoDelete { page_id, slot_id });
        Ok(())
    }

    /// Records an UPDATE (delete + insert) into the WAL and enqueues undo operations.
    ///
    /// Undo order: kill the new slot first, then restore the old slot's deletion mark.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_update(
        &mut self,
        table_id: u32,
        key: &[u8],
        old_value: &[u8],
        new_value: &[u8],
        page_id: u64,
        old_slot: u16,
        new_slot: u16,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        // Prepend physical locations to both sides for crash recovery.
        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_value.len());
        ov.extend_from_slice(&encode_physical_loc(page_id, old_slot));
        ov.extend_from_slice(old_value);

        let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_value.len());
        nv.extend_from_slice(&encode_physical_loc(page_id, new_slot));
        nv.extend_from_slice(new_value);

        let mut entry = WalEntry::new(0, txn_id, EntryType::Update, table_id, key.to_vec(), ov, nv);
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;
        // Push in chronological order; on rollback they are reversed:
        // UndoDelete(old_slot) runs first (restores old row), then
        // UndoInsert(new_slot) kills the replacement.
        active.undo_ops.push(UndoOp::UndoInsert {
            page_id,
            slot_id: new_slot,
        });
        active.undo_ops.push(UndoOp::UndoDelete {
            page_id,
            slot_id: old_slot,
        });
        Ok(())
    }

    /// Records a stable-RID in-place UPDATE into the WAL and enqueues tuple-image undo.
    ///
    /// Both `old_tuple_image` and `new_tuple_image` are full logical tuple images:
    /// `[RowHeader || row bytes]`, without alignment padding.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_update_in_place(
        &mut self,
        table_id: u32,
        key: &[u8],
        old_tuple_image: &[u8],
        new_tuple_image: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_tuple_image.len());
        ov.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        ov.extend_from_slice(old_tuple_image);

        let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_tuple_image.len());
        nv.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        nv.extend_from_slice(new_tuple_image);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::UpdateInPlace,
            table_id,
            key.to_vec(),
            ov,
            nv,
        );
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;
        active.undo_ops.push(UndoOp::UndoUpdateInPlace {
            page_id,
            slot_id,
            old_image: old_tuple_image.to_vec(),
        });
        Ok(())
    }

    /// Records N stable-RID in-place UPDATEs in a **single `write_all` call**.
    ///
    /// Equivalent to calling [`record_update_in_place`] N times but uses
    /// `reserve_lsns + write_batch` to emit all entries in one shot, reducing
    /// BufWriter call overhead from O(N) to O(1).
    ///
    /// Each element of `images` is `(key, old_tuple_image, new_tuple_image, page_id, slot_id)`.
    /// The WAL entries are byte-for-byte identical to those produced by N calls
    /// to `record_update_in_place` — crash recovery is unchanged.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    #[allow(clippy::type_complexity)]
    pub fn record_update_in_place_batch(
        &mut self,
        table_id: u32,
        images: &[(&[u8], &[u8], &[u8], u64, u16)], // (key, old_tuple_image, new_tuple_image, page_id, slot_id)
    ) -> Result<(), DbError> {
        let n = images.len();
        if n == 0 {
            return Ok(());
        }

        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let txn_id = active.txn_id;

        let lsn_base = self.wal.reserve_lsns(n);
        self.wal_scratch.clear();

        for (i, (key, old_tuple_image, new_tuple_image, page_id, slot_id)) in
            images.iter().enumerate()
        {
            let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_tuple_image.len());
            ov.extend_from_slice(&encode_physical_loc(*page_id, *slot_id));
            ov.extend_from_slice(old_tuple_image);

            let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_tuple_image.len());
            nv.extend_from_slice(&encode_physical_loc(*page_id, *slot_id));
            nv.extend_from_slice(new_tuple_image);

            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::UpdateInPlace,
                table_id,
                key.to_vec(),
                ov,
                nv,
            );
            entry.serialize_into(&mut self.wal_scratch);
        }

        self.wal.write_batch(&self.wal_scratch)?;

        for (_, old_tuple_image, _, page_id, slot_id) in images {
            active.undo_ops.push(UndoOp::UndoUpdateInPlace {
                page_id: *page_id,
                slot_id: *slot_id,
                old_image: old_tuple_image.to_vec(),
            });
        }

        Ok(())
    }

    /// Records a full-table delete (DELETE without WHERE / TRUNCATE) as a
    /// single WAL entry instead of N per-row entries.
    ///
    /// The physical heap pages must already have been updated by `delete_batch()`
    /// before calling this. ONE WAL entry replaces N `record_delete()` calls.
    ///
    /// The `key` field of the WAL entry holds `root_page_id` as 8 bytes LE —
    /// sufficient for crash recovery to locate the heap chain and undo the deletion.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    pub fn record_truncate(&mut self, table_id: u32, root_page_id: u64) -> Result<(), DbError> {
        let txn = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        let mut entry = WalEntry::new(
            0,
            txn.txn_id,
            EntryType::Truncate,
            table_id,
            root_page_id.to_le_bytes().to_vec(), // key = root_page_id (for recovery)
            vec![],
            vec![],
        );
        self.wal
            .append_with_buf(&mut entry, &mut self.wal_scratch)?;
        txn.undo_ops.push(UndoOp::UndoTruncate { root_page_id });
        Ok(())
    }

    // ── Index undo (Phase 7.3b) ────────────────────────────────────────────

    /// Records an index INSERT undo operation so that ROLLBACK can remove the
    /// entry from the B-Tree. Called by the executor after inserting a new
    /// secondary index entry (INSERT or UPDATE of an indexed column).
    ///
    /// No WAL entry is written — index operations are derived from heap state
    /// on crash recovery. This is purely for in-memory ROLLBACK.
    pub fn record_index_insert(
        &mut self,
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    ) -> Result<(), DbError> {
        let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
        active.undo_ops.push(UndoOp::UndoIndexInsert {
            index_id,
            root_page_id,
            key,
        });
        Ok(())
    }

    /// Returns all `UndoIndexInsert` operations from the active transaction's
    /// undo log, in reverse chronological order (last insert first).
    ///
    /// Called by the executor before `rollback()` or `rollback_to_savepoint()`
    /// to handle B-Tree deletes at the executor layer (TxnManager cannot depend
    /// on `axiomdb-index`).
    ///
    /// Returns `(index_id, root_page_id, key)` tuples.
    pub fn collect_index_undos(&self) -> Vec<(u32, u64, Vec<u8>)> {
        let Some(active) = &self.active else {
            return Vec::new();
        };
        active
            .undo_ops
            .iter()
            .rev()
            .filter_map(|op| match op {
                UndoOp::UndoIndexInsert {
                    index_id,
                    root_page_id,
                    key,
                } => Some((*index_id, *root_page_id, key.clone())),
                _ => None,
            })
            .collect()
    }

    /// Like [`collect_index_undos`] but only returns ops recorded after the
    /// given savepoint.
    pub fn collect_index_undos_since(&self, sp: &Savepoint) -> Vec<(u32, u64, Vec<u8>)> {
        let Some(active) = &self.active else {
            return Vec::new();
        };
        active
            .undo_ops
            .iter()
            .skip(sp.undo_len)
            .rev()
            .filter_map(|op| match op {
                UndoOp::UndoIndexInsert {
                    index_id,
                    root_page_id,
                    key,
                } => Some((*index_id, *root_page_id, key.clone())),
                _ => None,
            })
            .collect()
    }

    // ── Autocommit ───────────────────────────────────────────────────────────

    /// Wraps `f` in an implicit BEGIN / COMMIT, rolling back automatically on error.
    ///
    /// `f` receives `&mut Self` so it can call `record_*` methods.
    /// Storage is needed only if an error triggers rollback.
    ///
    /// ```rust,ignore
    /// let slot_id = txn_mgr.autocommit(&mut storage, |mgr| {
    ///     let slot = insert_tuple(&mut page, data, mgr.begin_txn_id())?;
    ///     mgr.record_insert(table_id, key, value, page_id, slot)?;
    ///     Ok(slot)
    /// })?;
    /// ```
    pub fn autocommit<F, T>(&mut self, storage: &mut dyn StorageEngine, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&mut Self) -> Result<T, DbError>,
    {
        self.begin()?;
        match f(self) {
            Ok(result) => {
                self.commit()?;
                Ok(result)
            }
            Err(e) => {
                // Best-effort rollback: propagate original error regardless.
                let _ = self.rollback(storage);
                Err(e)
            }
        }
    }

    // ── MVCC snapshots ────────────────────────────────────────────────────────

    /// Returns a snapshot that sees only committed data.
    ///
    /// `snapshot_id = max_committed + 1`. Safe to call at any time.
    /// Used for read operations outside an explicit transaction.
    pub fn snapshot(&self) -> TransactionSnapshot {
        TransactionSnapshot::committed(self.max_committed)
    }

    /// Returns a snapshot for the active transaction.
    ///
    /// - **REPEATABLE READ / SERIALIZABLE**: returns the frozen snapshot captured
    ///   at `BEGIN` (same `snapshot_id` for every call within the txn).
    /// - **READ COMMITTED**: returns a fresh snapshot reflecting everything
    ///   committed right now, plus the transaction's own writes.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    pub fn active_snapshot(&self) -> Result<TransactionSnapshot, DbError> {
        let active = self.active.as_ref().ok_or(DbError::NoActiveTransaction)?;
        let snapshot_id = if active.isolation_level.uses_frozen_snapshot() {
            active.snapshot_id_at_begin
        } else {
            // READ COMMITTED: fresh snapshot per statement
            self.max_committed + 1
        };
        Ok(TransactionSnapshot {
            snapshot_id,
            current_txn_id: active.txn_id,
        })
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// TxnId of the last committed transaction. `0` if none has committed yet.
    pub fn max_committed(&self) -> TxnId {
        self.max_committed
    }

    /// LSN of the last WAL entry written (0 if none).
    pub fn current_lsn(&self) -> u64 {
        self.wal.current_lsn()
    }

    /// TxnId of the currently active transaction, if any.
    pub fn active_txn_id(&self) -> Option<TxnId> {
        self.active.as_ref().map(|a| a.txn_id)
    }

    /// Mutable access to the underlying [`WalWriter`].
    ///
    /// Used by [`Checkpointer`] to append the Checkpoint entry and fsync the WAL.
    /// Callers must not write arbitrary entries through this — only Checkpointer uses it.
    pub fn wal_mut(&mut self) -> &mut WalWriter {
        &mut self.wal
    }

    // ── WAL Rotation ──────────────────────────────────────────────────────────

    /// Triggers a checkpoint and rotates the WAL file.
    ///
    /// After rotation, the WAL file is truncated to just the 24-byte v2 header
    /// with `start_lsn = checkpoint_lsn`. The next entry written will have
    /// `LSN = checkpoint_lsn + 1`, preserving global monotonicity.
    ///
    /// **Must be called with no active transaction.** Rotating mid-transaction
    /// would lose the in-progress undo log.
    ///
    /// Returns the checkpoint LSN.
    ///
    /// # Errors
    /// - [`DbError::TransactionAlreadyActive`] if a transaction is in progress.
    /// - Any I/O error from checkpoint or file truncation.
    pub fn rotate_wal(
        &mut self,
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<u64, DbError> {
        if let Some(ref active) = self.active {
            return Err(DbError::TransactionAlreadyActive {
                txn_id: active.txn_id,
            });
        }

        // 1. Checkpoint: flush pages + write Checkpoint WAL entry + fsync.
        let checkpoint_lsn = Checkpointer::checkpoint(storage, &mut self.wal)?;

        // 2. Truncate the WAL file to just the header with start_lsn.
        WalWriter::rotate_file(wal_path, checkpoint_lsn)?;

        // 3. Reopen the WAL: next_lsn = checkpoint_lsn + 1.
        self.wal = WalWriter::open(wal_path)?;

        Ok(checkpoint_lsn)
    }

    /// Opens an existing WAL and runs crash recovery, returning a ready `TxnManager`.
    ///
    /// Equivalent to `CrashRecovery::recover() + TxnManager::open()`, initialising
    /// `max_committed` from the WAL scan instead of a separate pass.
    ///
    /// Use this instead of [`TxnManager::open`] when reopening a database that
    /// may have crashed.
    pub fn open_with_recovery(
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<(Self, RecoveryResult), DbError> {
        let result = CrashRecovery::recover(storage, wal_path)?;
        let wal = WalWriter::open(wal_path)?;
        let mgr = Self {
            wal,
            next_txn_id: result.max_committed + 1,
            max_committed: result.max_committed,
            active: None,
            wal_scratch: Vec::with_capacity(256),
            deferred_commit_mode: false,
            pending_deferred_txn_id: None,
            committed_free_batches: Vec::new(),
            durability_policy: WalDurabilityPolicy::Strict,
        };
        Ok((mgr, result))
    }

    /// Rotates the WAL if its current size exceeds `max_wal_size` bytes.
    ///
    /// Returns `true` if rotation occurred, `false` if the WAL was below the threshold.
    pub fn check_and_rotate(
        &mut self,
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
        max_wal_size: u64,
    ) -> Result<bool, DbError> {
        if self.wal.file_offset() > max_wal_size {
            self.rotate_wal(storage, wal_path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// ── Physical location helpers ─────────────────────────────────────────────────

/// Bytes prepended to `new_value` (Insert/Update) and `old_value` (Delete)
/// to encode the heap physical location for crash recovery:
/// `[page_id: u64 LE][slot_id: u16 LE]` = 10 bytes.
pub const PHYSICAL_LOC_LEN: usize = 10;

/// Encodes `(page_id, slot_id)` into a 10-byte array.
pub(crate) fn encode_physical_loc(page_id: u64, slot_id: u16) -> [u8; PHYSICAL_LOC_LEN] {
    let mut loc = [0u8; PHYSICAL_LOC_LEN];
    loc[0..8].copy_from_slice(&page_id.to_le_bytes());
    loc[8..10].copy_from_slice(&slot_id.to_le_bytes());
    loc
}

/// Decodes `(page_id, slot_id)` from the first 10 bytes of a WAL payload.
/// Returns `None` if the slice is too short (e.g. legacy or control entries).
pub fn decode_physical_loc(bytes: &[u8]) -> Option<(u64, u16)> {
    if bytes.len() < PHYSICAL_LOC_LEN {
        return None;
    }
    let page_id = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let slot_id = u16::from_le_bytes([bytes[8], bytes[9]]);
    Some((page_id, slot_id))
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Scans the WAL forward and returns the highest TxnId with a Commit entry.
fn scan_max_committed(wal_path: &Path) -> Result<TxnId, DbError> {
    let reader = WalReader::open(wal_path)?;
    let mut max = 0u64;
    for result in reader.scan_forward(0)? {
        let entry = result?;
        if entry.entry_type == EntryType::Commit && entry.txn_id > max {
            max = entry.txn_id;
        }
    }
    Ok(max)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::Page;
    use axiomdb_storage::{
        insert_tuple, read_tuple, read_tuple_image, rewrite_tuple_same_slot, MemoryStorage,
        PageType,
    };

    fn temp_wal() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        (dir, path)
    }

    // ── begin / commit ────────────────────────────────────────────────────────

    #[test]
    fn test_begin_commit_advances_max_committed() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        assert_eq!(mgr.max_committed(), 0);

        let txn = mgr.begin().unwrap();
        assert_eq!(txn, 1);
        mgr.commit().unwrap();
        assert_eq!(mgr.max_committed(), 1);

        let txn2 = mgr.begin().unwrap();
        assert_eq!(txn2, 2);
        mgr.commit().unwrap();
        assert_eq!(mgr.max_committed(), 2);
    }

    #[test]
    fn test_begin_rollback_does_not_advance_max_committed() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        mgr.begin().unwrap();
        mgr.rollback(&mut storage).unwrap();
        assert_eq!(mgr.max_committed(), 0);
    }

    // ── undo INSERT ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_insert_marks_slot_dead() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let txn_id = mgr.begin().unwrap();

        // Simulate: insert on heap page, then record in txn manager.
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"hello", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"key", b"hello", page_id, slot_id)
            .unwrap();

        // Rollback should kill the slot.
        mgr.rollback(&mut storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let result = read_tuple(&page, slot_id).unwrap();
        assert!(
            result.is_none(),
            "slot must be dead after rollback of insert"
        );
    }

    // ── undo DELETE ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_delete_clears_deletion() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        // Insert row in txn 1, commit.
        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"data", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"data", page_id, slot_id)
            .unwrap();
        mgr.commit().unwrap();

        // Delete row in txn 2, then rollback.
        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            axiomdb_storage::delete_tuple(&mut p, slot_id, txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
        }
        mgr.record_delete(1, b"k", b"data", page_id, slot_id)
            .unwrap();
        mgr.rollback(&mut storage).unwrap();

        // After rollback, txn_id_deleted must be 0 (row is live again).
        let page = storage.read_page(page_id).unwrap();
        let (hdr, _) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(
            hdr.txn_id_deleted, 0,
            "txn_id_deleted must be cleared after rollback"
        );
    }

    // ── undo UPDATE ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_update() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        // Insert original row in txn 1.
        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let old_slot = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"original", page_id, old_slot)
            .unwrap();
        mgr.commit().unwrap();

        // Update in txn 2: delete old + insert new.
        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let new_slot =
                axiomdb_storage::update_tuple(&mut p, old_slot, b"updated", txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update(
                1,
                b"k",
                b"original",
                b"updated",
                page_id,
                old_slot,
                new_slot,
            )
            .unwrap();
        }
        mgr.rollback(&mut storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        // Old slot must be live again.
        let (old_hdr, old_data) = read_tuple(&page, old_slot).unwrap().unwrap();
        assert_eq!(old_data, b"original");
        assert_eq!(
            old_hdr.txn_id_deleted, 0,
            "old row must be live after update rollback"
        );
        // New slot must be dead.
        // new_slot = old_slot + 1 (inserted right after old in the page)
        let new_slot = old_slot + 1;
        assert!(
            read_tuple(&page, new_slot).unwrap().is_none(),
            "new slot must be dead after update rollback"
        );
    }

    #[test]
    fn test_rollback_undo_update_in_place_restores_old_tuple_image() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"original", page_id, slot_id)
            .unwrap();
        mgr.commit().unwrap();

        let txn2 = mgr.begin().unwrap();
        let old_image = {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let old_image = rewrite_tuple_same_slot(&mut p, slot_id, b"updated", txn2)
                .unwrap()
                .unwrap();
            let new_image = read_tuple_image(&p, slot_id).unwrap().unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update_in_place(1, b"k", &old_image, &new_image, page_id, slot_id)
                .unwrap();
            old_image
        };

        mgr.rollback(&mut storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let (hdr, data) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(data, b"original");
        assert_eq!(hdr.txn_id_created, 1);
        assert_eq!(hdr.txn_id_deleted, 0);
        assert_eq!(hdr.row_version, 0);
        assert_eq!(
            read_tuple_image(&page, slot_id).unwrap().unwrap(),
            old_image
        );
    }

    // ── snapshots ─────────────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_returns_committed_snapshot() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();

        let snap = mgr.snapshot();
        assert_eq!(snap.snapshot_id, 1); // max_committed=0 → snapshot_id=1
        assert_eq!(snap.current_txn_id, 0);

        mgr.begin().unwrap();
        mgr.commit().unwrap(); // max_committed=1

        let snap2 = mgr.snapshot();
        assert_eq!(snap2.snapshot_id, 2);
    }

    #[test]
    fn test_active_snapshot_has_current_txn_id() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();

        let txn_id = mgr.begin().unwrap();
        let snap = mgr.active_snapshot().unwrap();
        assert_eq!(snap.current_txn_id, txn_id);
        assert_eq!(snap.snapshot_id, 1); // max_committed=0 at begin
    }

    #[test]
    fn test_uncommitted_row_not_visible_via_snapshot() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let txn_id = mgr.begin().unwrap();

        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"secret", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"secret", page_id, slot_id)
            .unwrap();

        // A committed snapshot (max_committed=0) should NOT see txn_id=1's row.
        let snap = mgr.snapshot();
        let page = storage.read_page(page_id).unwrap();
        let (hdr, _) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert!(
            !hdr.is_visible(&snap),
            "uncommitted row must not be visible"
        );

        // The active snapshot (with current_txn_id=1) SHOULD see it.
        let active_snap = mgr.active_snapshot().unwrap();
        assert!(
            hdr.is_visible(&active_snap),
            "active txn must see its own writes"
        );

        mgr.rollback(&mut storage).unwrap();
    }

    // ── error cases ───────────────────────────────────────────────────────────

    #[test]
    fn test_double_begin_error() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();

        mgr.begin().unwrap();
        let err = mgr.begin().unwrap_err();
        assert!(matches!(err, DbError::TransactionAlreadyActive { .. }));
    }

    #[test]
    fn test_commit_without_begin_error() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let err = mgr.commit().unwrap_err();
        assert!(matches!(err, DbError::NoActiveTransaction));
    }

    #[test]
    fn test_rollback_without_begin_error() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();
        let err = mgr.rollback(&mut storage).unwrap_err();
        assert!(matches!(err, DbError::NoActiveTransaction));
    }

    // ── open / recovery ───────────────────────────────────────────────────────

    #[test]
    fn test_open_recovers_max_committed() {
        let (_dir, path) = temp_wal();

        // First session: two commits.
        {
            let mut mgr = TxnManager::create(&path).unwrap();
            mgr.begin().unwrap();
            mgr.commit().unwrap(); // txn 1 committed
            mgr.begin().unwrap();
            mgr.commit().unwrap(); // txn 2 committed
            mgr.begin().unwrap(); // txn 3 never committed (simulates crash)
                                  // Drop without commit
        }

        // Second session: open should recover max_committed = 2.
        let mgr = TxnManager::open(&path).unwrap();
        assert_eq!(mgr.max_committed(), 2);
        assert_eq!(mgr.active_txn_id(), None);
    }

    // ── WAL entry order ───────────────────────────────────────────────────────

    #[test]
    fn test_wal_entry_order() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();

        let txn = mgr.begin().unwrap();
        mgr.record_insert(1, b"k", b"v", 99, 0).unwrap();
        mgr.commit().unwrap();

        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap().entry_type)
            .collect();

        assert_eq!(
            entries,
            vec![EntryType::Begin, EntryType::Insert, EntryType::Commit]
        );
        let _ = txn;
    }

    #[test]
    fn test_record_update_in_place_batch_writes_parseable_entries() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();

        let txn = mgr.begin().unwrap();
        let key1 = encode_physical_loc(42, 1);
        let key2 = encode_physical_loc(42, 2);
        let old1 = b"old-row-1".to_vec();
        let new1 = b"new-row-1".to_vec();
        let old2 = b"old-row-2".to_vec();
        let new2 = b"new-row-2".to_vec();

        let batch = vec![
            (
                key1.as_slice(),
                old1.as_slice(),
                new1.as_slice(),
                42_u64,
                1_u16,
            ),
            (
                key2.as_slice(),
                old2.as_slice(),
                new2.as_slice(),
                42_u64,
                2_u16,
            ),
        ];
        mgr.record_update_in_place_batch(7, &batch).unwrap();
        mgr.commit().unwrap();

        let reader = WalReader::open(&path).unwrap();
        let txn_entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .filter(|e| e.txn_id == txn)
            .collect();

        assert_eq!(txn_entries.len(), 4);
        assert_eq!(txn_entries[0].entry_type, EntryType::Begin);
        assert_eq!(txn_entries[1].entry_type, EntryType::UpdateInPlace);
        assert_eq!(txn_entries[2].entry_type, EntryType::UpdateInPlace);
        assert_eq!(txn_entries[3].entry_type, EntryType::Commit);
        assert_eq!(txn_entries[1].table_id, 7);
        assert_eq!(txn_entries[2].table_id, 7);
        assert_eq!(
            decode_physical_loc(&txn_entries[1].old_value),
            Some((42, 1)),
            "old_value must carry the physical location prefix",
        );
        assert_eq!(
            decode_physical_loc(&txn_entries[2].new_value),
            Some((42, 2)),
            "new_value must carry the physical location prefix",
        );
    }

    // ── autocommit ────────────────────────────────────────────────────────────

    #[test]
    fn test_autocommit_commits_on_ok() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        mgr.autocommit(&mut storage, |mgr| {
            mgr.record_insert(1, b"k", b"v", 99, 0)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(mgr.max_committed(), 1);
    }

    #[test]
    fn test_autocommit_rollbacks_on_err() {
        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        let result = mgr.autocommit(&mut storage, |_mgr| {
            Err::<(), _>(DbError::Other("simulated failure".into()))
        });

        assert!(result.is_err());
        assert_eq!(mgr.max_committed(), 0); // nothing committed
        assert!(mgr.active_txn_id().is_none()); // no active txn
    }

    // ── record_truncate ───────────────────────────────────────────────────────

    /// Verifies that record_truncate writes exactly ONE WAL entry (Truncate type)
    /// instead of N Delete entries — the core WAL I/O reduction.
    #[test]
    fn test_record_truncate_single_wal_entry() {
        use crate::reader::WalReader;
        use axiomdb_storage::{heap_chain::HeapChain, PageType};

        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        // Allocate a root heap page and insert 5 rows (txn 1).
        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let init_page = axiomdb_storage::Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &init_page).unwrap();

        let txn1 = mgr.begin().unwrap();
        for i in 0u8..5 {
            HeapChain::insert(&mut storage, root_page_id, &[i; 8], txn1).unwrap();
        }
        mgr.commit().unwrap();

        // Txn 2: delete_batch + record_truncate (simulates no-WHERE DELETE).
        let txn2 = mgr.begin().unwrap();
        let snap = mgr.active_snapshot().unwrap();
        let raw_rids = HeapChain::scan_rids_visible(&mut storage, root_page_id, snap).unwrap();
        HeapChain::delete_batch(&mut storage, root_page_id, &raw_rids, txn2).unwrap();
        mgr.record_truncate(1, root_page_id).unwrap();
        mgr.commit().unwrap();

        // Scan WAL and count DML entries for txn2.
        let reader = WalReader::open(&path).unwrap();
        let txn2_dml: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.txn_id == txn2)
            .filter(|e| {
                matches!(
                    e.entry_type,
                    EntryType::Insert
                        | EntryType::Delete
                        | EntryType::Update
                        | EntryType::UpdateInPlace
                        | EntryType::Truncate
                )
            })
            .collect();

        // Must be exactly 1 Truncate entry — not 5 Delete entries.
        assert_eq!(txn2_dml.len(), 1, "expected exactly 1 WAL DML entry");
        assert_eq!(
            txn2_dml[0].entry_type,
            EntryType::Truncate,
            "DML entry must be Truncate type"
        );
        // key must encode root_page_id as 8 bytes LE.
        let encoded_root = u64::from_le_bytes(txn2_dml[0].key[..8].try_into().unwrap());
        assert_eq!(encoded_root, root_page_id, "key must contain root_page_id");
    }

    /// Verifies that rolling back a record_truncate restores all deleted rows.
    #[test]
    fn test_truncate_rollback_restores_rows() {
        use axiomdb_core::TransactionSnapshot;
        use axiomdb_storage::{heap_chain::HeapChain, PageType};

        let (_dir, path) = temp_wal();
        let mut mgr = TxnManager::create(&path).unwrap();
        let mut storage = MemoryStorage::new();

        // Insert 5 rows in txn 1 (committed).
        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let init_page = axiomdb_storage::Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &init_page).unwrap();

        let txn1 = mgr.begin().unwrap();
        for i in 0u8..5 {
            HeapChain::insert(&mut storage, root_page_id, &[i; 8], txn1).unwrap();
        }
        mgr.commit().unwrap();

        // Verify 5 rows visible after txn1 commit.
        let snap_after_insert = TransactionSnapshot::committed(mgr.max_committed());
        let before =
            HeapChain::scan_rids_visible(&mut storage, root_page_id, snap_after_insert).unwrap();
        assert_eq!(before.len(), 5, "5 rows must be visible before truncate");

        // Txn 2: delete_batch + record_truncate, then ROLLBACK.
        let txn2 = mgr.begin().unwrap();
        let snap2 = mgr.active_snapshot().unwrap();
        let raw_rids = HeapChain::scan_rids_visible(&mut storage, root_page_id, snap2).unwrap();
        HeapChain::delete_batch(&mut storage, root_page_id, &raw_rids, txn2).unwrap();
        mgr.record_truncate(1, root_page_id).unwrap();
        mgr.rollback(&mut storage).unwrap();

        // After rollback: all 5 rows must be visible again.
        let snap_after_rollback = TransactionSnapshot::committed(mgr.max_committed());
        let after =
            HeapChain::scan_rids_visible(&mut storage, root_page_id, snap_after_rollback).unwrap();
        assert_eq!(
            after.len(),
            5,
            "all 5 rows must be visible again after truncate rollback"
        );
    }
}
