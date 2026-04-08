impl TxnManager {
    // ── Transaction lifecycle ─────────────────────────────────────────────────

    /// Starts a new explicit transaction with `RepeatableRead` isolation (default).
    ///
    /// Returns a [`ConnectionTxn`] that holds all per-transaction state.
    ///
    /// # Errors
    /// - [`DbError::TransactionAlreadyActive`] if a transaction is already open
    ///   (single-writer constraint until Phase 40.10).
    pub fn begin(&mut self) -> Result<ConnectionTxn, DbError> {
        self.begin_with_isolation(axiomdb_core::IsolationLevel::RepeatableRead)
    }

    /// Like [`begin`] but with an explicit isolation level.
    ///
    /// - `ReadCommitted`: `active_snapshot()` returns a fresh snapshot per call.
    /// - `RepeatableRead` / `Serializable`: `active_snapshot()` returns the
    ///   snapshot frozen at BEGIN.
    pub fn begin_with_isolation(
        &mut self,
        isolation_level: axiomdb_core::IsolationLevel,
    ) -> Result<ConnectionTxn, DbError> {
        // Single-writer check: enforce at most one active txn.
        {
            let set = self.active_set.read().unwrap();
            if !set.is_empty() {
                let first_id = *set.iter().next().unwrap();
                return Err(DbError::TransactionAlreadyActive { txn_id: first_id });
            }
        }

        let txn_id = self.next_txn_id;
        self.next_txn_id = self
            .next_txn_id
            .checked_add(1)
            .ok_or_else(|| DbError::Other("transaction ID overflow".into()))?;

        // Phase 7.15: transaction ID overflow prevention.
        const TXN_ID_WARN_90: u64 = u64::MAX / 10 * 9;
        const TXN_ID_WARN_50: u64 = u64::MAX / 2;
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

        // Atomically: read max_committed, capture active set (BEFORE inserting self),
        // then insert self. Single write lock covers all three operations.
        // PostgreSQL ProcArrayLock + DuckDB transaction_lock pattern.
        let (snapshot_id_at_begin, active_ids_at_begin) = {
            let mut set = self.active_set.write().unwrap();
            let mc = self.max_committed.load(Ordering::Acquire);
            // Capture set BEFORE inserting self — own writes visible via current_txn_id,
            // not via active_ids. Self must be excluded from the frozen snapshot.
            let active_ids = if isolation_level.uses_frozen_snapshot() {
                Some(Arc::new(set.clone()))
            } else {
                None
            };
            set.insert(txn_id);
            let prev = self.lowest_active_id.load(Ordering::Relaxed);
            if prev == 0 || txn_id < prev {
                self.lowest_active_id.store(txn_id, Ordering::Relaxed);
            }
            (mc + 1, active_ids)
        };

        Ok(ConnectionTxn {
            txn_id,
            snapshot_id_at_begin,
            isolation_level,
            undo_ops: Vec::new(),
            deferred_free_pages: Vec::new(),
            savepoints: Vec::new(),
            clustered_roots: self.last_clustered_roots.clone(),
            active_ids_at_begin,
            wal_scratch: Vec::with_capacity(256),
            deferred_commit_mode: self.deferred_commit_mode,
            pending_deferred_txn_id: None,
        })
    }

    /// Commits the transaction: writes the Commit WAL entry, fsyncs (or defers),
    /// advances `max_committed`, and removes the txn from the active set.
    ///
    /// # Errors
    /// - I/O errors from WAL write or fsync.
    pub fn commit(&mut self, mut conn_txn: ConnectionTxn) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;
        self.last_clustered_roots = conn_txn.clustered_roots.clone();

        let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;

        // Determine if max_committed should advance now or later (deferred pipeline).
        // I/O (flush/fsync) happens BEFORE acquiring the lock to minimize lock hold time.
        let advance_now = if conn_txn.undo_ops.is_empty() {
            // Read-only transaction: flush to OS page cache only (no fsync).
            self.wal.flush_no_sync()?;
            true
        } else {
            match self.durability_policy {
                WalDurabilityPolicy::Strict => {
                    if conn_txn.deferred_commit_mode {
                        // Pipeline mode: Commit entry buffered but not fsynced yet.
                        // max_committed advances only after the pipeline confirms fsync
                        // via advance_committed() / advance_committed_single().
                        conn_txn.pending_deferred_txn_id = Some(txn_id);
                        // Mirror on TxnManager so callers can retrieve after conn_txn is consumed.
                        self.pending_deferred_txn_id = Some(txn_id);
                        false
                    } else {
                        self.wal.commit_data_sync()?;
                        true
                    }
                }
                WalDurabilityPolicy::Normal => {
                    self.wal.flush_no_sync()?;
                    true
                }
                WalDurabilityPolicy::Off => true,
            }
        };

        // ATOMICALLY: advance max_committed (if non-deferred) AND remove from active_set.
        // DuckDB transaction_lock + PostgreSQL ProcArrayLock pattern:
        // no snapshot can observe a txn as both committed and still in-flight.
        {
            let mut set = self.active_set.write().unwrap();
            if advance_now {
                self.max_committed.store(txn_id, Ordering::Release);
            }
            set.remove(&txn_id);
            let new_lowest = set.iter().copied().min().unwrap_or(0);
            self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
        }

        if !conn_txn.deferred_free_pages.is_empty() {
            self.committed_free_batches
                .push((txn_id, conn_txn.deferred_free_pages));
        }

        Ok(())
    }

    /// Enables or disables deferred commit mode for the server-side fsync pipeline.
    pub fn set_deferred_commit_mode(&mut self, enabled: bool) {
        self.deferred_commit_mode = enabled;
    }

    /// Takes the pending deferred commit txn_id set by the last `commit()` call
    /// in deferred mode, if any.
    ///
    /// Returns `Some(txn_id)` exactly once after a deferred DML commit.
    /// Returns `None` for read-only commits or non-deferred commits.
    ///
    /// This mirrors `ConnectionTxn::take_pending_deferred_commit()` for callers
    /// who no longer hold the `ConnectionTxn` after passing it to `commit()`.
    pub fn take_pending_deferred_commit(&mut self) -> Option<TxnId> {
        self.pending_deferred_txn_id.take()
    }

    /// Sets the WAL durability policy for committed DML.
    pub fn set_durability_policy(&mut self, policy: WalDurabilityPolicy) {
        self.durability_policy = policy;
    }

    /// Returns the current WAL durability policy.
    pub fn durability_policy(&self) -> WalDurabilityPolicy {
        self.durability_policy
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
            // fetch_max: only advances, never regresses. Ordering::Release so
            // subsequent Acquire loads see committed rows. active_set was already
            // updated in commit() — no lock needed here.
            self.max_committed.fetch_max(max, Ordering::Release);
        }
    }

    /// Advances `max_committed` to `txn_id` if it is greater than the current
    /// value. Used by the fsync pipeline leader to make a single transaction
    /// visible after confirming WAL durability.
    pub fn advance_committed_single(&mut self, txn_id: TxnId) {
        self.max_committed.fetch_max(txn_id, Ordering::Release);
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
    /// On `rollback` or `rollback_to_savepoint`, deferred pages are simply
    /// discarded — the catalog undo restores the old roots so old pages remain live.
    pub fn defer_free_pages(
        &self,
        conn_txn: &mut ConnectionTxn,
        pages: impl IntoIterator<Item = u64>,
    ) {
        conn_txn.deferred_free_pages.extend(pages);
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

}
