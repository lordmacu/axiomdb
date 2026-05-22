impl TxnManager {
    // ── Transaction lifecycle ─────────────────────────────────────────────────

    /// Starts a new explicit transaction with `RepeatableRead` isolation (default).
    ///
    /// Returns a [`ConnectionTxn`] that holds all per-transaction state.
    /// Multiple connections can call `begin()` concurrently (Phase 40.10).
    pub fn begin(&self) -> Result<ConnectionTxn, DbError> {
        self.begin_with_isolation(axiomdb_core::IsolationLevel::RepeatableRead)
    }

    /// Like [`begin`] but with an explicit isolation level.
    ///
    /// - `ReadCommitted`: `active_snapshot()` returns a fresh snapshot per call.
    /// - `RepeatableRead` / `Serializable`: `active_snapshot()` returns the
    ///   snapshot frozen at BEGIN.
    pub fn begin_with_isolation(
        &self,
        isolation_level: axiomdb_core::IsolationLevel,
    ) -> Result<ConnectionTxn, DbError> {
        // Allocate txn_id atomically — no lock needed.
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);

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

        let clustered_roots = self.last_clustered_roots.lock().unwrap().clone();

        Ok(ConnectionTxn {
            txn_id,
            snapshot_id_at_begin,
            isolation_level,
            undo_ops: Vec::new(),
            deferred_free_pages: Vec::new(),
            savepoints: Vec::new(),
            clustered_roots,
            active_ids_at_begin,
            wal_scratch: Vec::with_capacity(256),
            deferred_commit_mode: self.deferred_commit_mode,
            pending_deferred_txn_id: None,
            local_page_batch: LocalPageBatch::new(),
            durability_override: None,
        })
    }

    /// Commits and makes the page-frame redo log durable first (write-ahead order:
    /// the txn's data frames are `fsync`'d BEFORE the logical `Commit` record, so
    /// recovery never sees a committed txn whose frames are missing). This is the
    /// entry point the executor uses; plain [`commit`](Self::commit) stays for tests
    /// and non-redo callers. `sync_frame_log` is a no-op unless the redo log is on.
    pub fn commit_durable(
        &self,
        conn_txn: ConnectionTxn,
        storage: &dyn StorageEngine,
    ) -> Result<Option<TxnId>, DbError> {
        // 6d: frame fsync is policy-aware (SQLite `synchronous` model). At Strict we fsync the
        // frame log per commit (write-ahead: data durable before the WAL Commit record). At
        // Normal/Off the fsync is DEFERRED to the checkpoint (which fsyncs the frame log before
        // applying to main — 6d step 1a), exactly like SQLite WAL+NORMAL (`wal.c`: all WAL
        // fsyncs occur in walCheckpoint under NORMAL). The frames are still written + visible
        // to readers via the wal_index; only the per-commit fsync is dropped. A power loss at
        // NORMAL may lose the last unsynced txn(s) — the documented NORMAL relaxation.
        let effective_policy = conn_txn
            .durability_override
            .unwrap_or(self.durability_policy);
        if effective_policy == WalDurabilityPolicy::Strict {
            storage.sync_frame_log()?;
        }
        self.commit(conn_txn)
    }

    /// Commits the transaction: writes the Commit WAL entry, fsyncs (or defers),
    /// advances `max_committed`, and removes the txn from the active set.
    ///
    /// Returns `Some(txn_id)` when the commit is deferred (pipeline mode) and
    /// the caller must drive the fsync pipeline. Returns `None` for immediate
    /// commits or read-only transactions.
    ///
    /// # Errors
    /// - I/O errors from WAL write or fsync.
    pub fn commit(&self, mut conn_txn: ConnectionTxn) -> Result<Option<TxnId>, DbError> {
        let txn_id = conn_txn.txn_id;

        {
            let mut roots = self.last_clustered_roots.lock().unwrap();
            for (table_id, root_pid) in &conn_txn.clustered_roots {
                roots.insert(*table_id, *root_pid);
            }
        }

        let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;

        // Determine if max_committed should advance now or later (deferred pipeline).
        // I/O (flush/fsync) happens BEFORE acquiring the lock to minimize lock hold time.
        let (advance_now, pending_deferred) = if conn_txn.undo_ops.is_empty() {
            // Read-only transaction: flush to OS page cache only (no fsync).
            self.wal.flush_no_sync()?;
            (true, None)
        } else {
            // Attack 6: per-txn `durability_override` wins over the
            // instance default when set. Lets the SQL layer honor
            // session-level `SET synchronous = '<value>'`.
            let effective_policy = conn_txn
                .durability_override
                .unwrap_or(self.durability_policy);
            match effective_policy {
                WalDurabilityPolicy::Strict => {
                    if conn_txn.deferred_commit_mode {
                        // Pipeline mode: Commit entry buffered but not fsynced yet.
                        // max_committed advances only after the pipeline confirms fsync.
                        (false, Some(txn_id))
                    } else {
                        self.wal.commit_data_sync()?;
                        (true, None)
                    }
                }
                WalDurabilityPolicy::Normal => {
                    self.wal.flush_no_sync()?;
                    (true, None)
                }
                WalDurabilityPolicy::Off => (true, None),
            }
        };

        // ATOMICALLY: advance max_committed (if non-deferred) AND remove from active_set.
        // DuckDB transaction_lock + PostgreSQL ProcArrayLock pattern:
        // no snapshot can observe a txn as both committed and still in-flight.
        {
            let mut set = self.active_set.write().unwrap();
            if advance_now {
                // fetch_max: only advance, never regress. A lower txn_id
                // committing after a higher one must not overwrite.
                self.max_committed.fetch_max(txn_id, Ordering::Release);
            }
            set.remove(&txn_id);
            let new_lowest = set.iter().copied().min().unwrap_or(0);
            self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
        }

        // Post-commit housekeeping under a brief Mutex.
        {
            let mut batches = self.post_commit.lock().unwrap();
            if !conn_txn.deferred_free_pages.is_empty() {
                batches
                    .committed_free_batches
                    .push((txn_id, conn_txn.deferred_free_pages));
            }

            // Phase 40.9: stash the LocalPageBatch data for later drain.
            let (avail, freed) = conn_txn.local_page_batch.take_for_commit();
            if !avail.is_empty() {
                batches.committed_steal_protection.push(avail);
            }
            if !freed.is_empty() {
                batches.committed_recycle_pages.push(freed);
            }
        }

        Ok(pending_deferred)
    }

    /// Returns the WAL durability policy.
    pub fn durability_policy(&self) -> WalDurabilityPolicy {
        self.durability_policy
    }

    /// Advances `max_committed` to the maximum of the given txn_ids.
    ///
    /// Called after a successful pipeline-driven `wal_flush_and_fsync()`.
    /// Makes all transactions in the batch visible to future snapshots.
    pub fn advance_committed(&self, txn_ids: &[TxnId]) {
        if let Some(&max) = txn_ids.iter().max() {
            self.max_committed.fetch_max(max, Ordering::Release);
        }
    }

    /// Advances `max_committed` to `txn_id` if it is greater than the current
    /// value. Used by the fsync pipeline leader to make a single transaction
    /// visible after confirming WAL durability.
    pub fn advance_committed_single(&self, txn_id: TxnId) {
        self.max_committed.fetch_max(txn_id, Ordering::Release);
    }

    /// Returns the WAL writer's current LSN (the last assigned LSN).
    pub fn wal_current_lsn(&self) -> u64 {
        self.wal.current_lsn()
    }

    /// Enqueues `pages` for deferred reclamation after the current transaction
    /// is durably committed.
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
    pub fn release_committed_frees(
        &self,
        storage: &dyn StorageEngine,
        txn_ids: &[TxnId],
    ) -> Result<(), DbError> {
        let mut batches = self.post_commit.lock().unwrap();
        if txn_ids.is_empty() || batches.committed_free_batches.is_empty() {
            return Ok(());
        }
        let id_set: std::collections::HashSet<TxnId> = txn_ids.iter().copied().collect();
        let mut remaining = Vec::with_capacity(batches.committed_free_batches.len());
        for (txn_id, pages) in batches.committed_free_batches.drain(..) {
            if id_set.contains(&txn_id) {
                for pid in pages {
                    let _ = storage.free_page(pid);
                }
            } else {
                remaining.push((txn_id, pages));
            }
        }
        batches.committed_free_batches = remaining;
        Ok(())
    }

    /// Releases deferred-free pages for `txn_id` only in immediate-commit mode.
    pub fn release_immediate_committed_frees(
        &self,
        storage: &dyn StorageEngine,
        txn_id: TxnId,
    ) -> Result<(), DbError> {
        if !self.deferred_commit_mode {
            self.release_committed_frees(storage, &[txn_id])?;
        }
        Ok(())
    }

    /// Flushes the WAL BufWriter to the OS and performs the steady-state
    /// durable data sync.
    pub fn wal_flush_and_fsync(&self) -> Result<(), DbError> {
        self.wal.commit_data_sync()
    }

    /// Phase 40.9: drains page-batch data stashed during commit.
    pub fn drain_committed_page_batches(
        &self,
        storage: &dyn StorageEngine,
    ) -> Result<(), DbError> {
        let mut batches = self.post_commit.lock().unwrap();
        for batch in batches.committed_steal_protection.drain(..) {
            if !batch.is_empty() {
                storage.free_page_batch(&batch)?;
            }
        }
        for batch in batches.committed_recycle_pages.drain(..) {
            for page_id in batch {
                storage.recycle_page(page_id)?;
            }
        }
        Ok(())
    }

}
