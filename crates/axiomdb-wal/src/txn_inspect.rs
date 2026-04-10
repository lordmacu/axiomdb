impl TxnManager {
    // ── Autocommit ───────────────────────────────────────────────────────────

    /// Wraps `f` in an implicit BEGIN / COMMIT, rolling back automatically on error.
    pub fn autocommit<F, T>(&self, storage: &dyn StorageEngine, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Self, &mut ConnectionTxn) -> Result<T, DbError>,
    {
        let mut conn = self.begin()?;
        match f(self, &mut conn) {
            Ok(v) => {
                self.commit(conn)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.rollback(conn, storage);
                Err(e)
            }
        }
    }

    // ── MVCC snapshots ────────────────────────────────────────────────────────

    /// Returns a snapshot that sees only committed data.
    ///
    /// `snapshot_id = max_committed + 1`. Safe to call at any time.
    pub fn snapshot(&self) -> TransactionSnapshot {
        // Read max_committed AND active_set under the same read lock.
        // PostgreSQL ProcArrayLock (shared) pattern.
        let (mc, active_ids) = {
            let set = self.active_set.read().unwrap();
            let mc = self.max_committed.load(Ordering::Acquire);
            (mc, Arc::new(set.clone()))
        };
        TransactionSnapshot {
            snapshot_id: mc + 1,
            current_txn_id: 0,
            active_ids,
        }
    }

    /// Returns a snapshot for an active transaction.
    ///
    /// - **REPEATABLE READ / SERIALIZABLE**: returns the frozen snapshot captured at BEGIN.
    /// - **READ COMMITTED**: returns a fresh snapshot per call.
    pub fn active_snapshot(&self, conn_txn: &ConnectionTxn) -> TransactionSnapshot {
        if conn_txn.isolation_level.uses_frozen_snapshot() {
            TransactionSnapshot {
                snapshot_id: conn_txn.snapshot_id_at_begin,
                current_txn_id: conn_txn.txn_id,
                active_ids: conn_txn
                    .active_ids_at_begin
                    .clone()
                    .unwrap_or_else(|| Arc::new(HashSet::new())),
            }
        } else {
            let (mc, active_ids) = {
                let set = self.active_set.read().unwrap();
                let mc = self.max_committed.load(Ordering::Acquire);
                (mc, Arc::new(set.clone()))
            };
            TransactionSnapshot {
                snapshot_id: mc + 1,
                current_txn_id: conn_txn.txn_id,
                active_ids,
            }
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// TxnId of the last committed transaction. `0` if none has committed yet.
    pub fn max_committed(&self) -> TxnId {
        self.max_committed.load(Ordering::Acquire)
    }

    /// LSN of the last WAL entry written (0 if none).
    pub fn current_lsn(&self) -> u64 {
        self.wal.current_lsn()
    }

    /// Returns the lowest in-flight txn_id (GC horizon), or 0 if none are active.
    pub fn lowest_active_id(&self) -> u64 {
        self.lowest_active_id.load(Ordering::Relaxed)
    }

    /// Returns `true` if any transaction is currently active.
    pub fn has_active_txn(&self) -> bool {
        !self.active_set.read().unwrap().is_empty()
    }

    /// TxnId of the currently active transaction, if any.
    pub fn active_txn_id(&self) -> Option<TxnId> {
        let set = self.active_set.read().unwrap();
        set.iter().copied().next()
    }

    /// Returns a READ-COMMITTED-style snapshot using the active txn_id from
    /// `active_txn_id()`.
    pub fn snapshot_for_active(&self) -> Result<TransactionSnapshot, DbError> {
        let (txn_id, mc, active_ids) = {
            let set = self.active_set.read().unwrap();
            let txn_id = set.iter().copied().next().ok_or(DbError::NoActiveTransaction)?;
            let mc = self.max_committed.load(Ordering::Acquire);
            (txn_id, mc, Arc::new(set.clone()))
        };
        Ok(TransactionSnapshot {
            snapshot_id: mc + 1,
            current_txn_id: txn_id,
            active_ids,
        })
    }

    /// Latest known clustered root for `table_id`.
    pub fn clustered_root(&self, table_id: u32) -> Option<u64> {
        self.last_clustered_roots
            .lock()
            .unwrap()
            .get(&table_id)
            .copied()
    }

    /// Clustered root for an active transaction (transaction-local then fallback).
    pub fn clustered_root_for_conn(&self, conn_txn: &ConnectionTxn, table_id: u32) -> Option<u64> {
        conn_txn
            .clustered_roots
            .get(&table_id)
            .copied()
            .or_else(|| {
                self.last_clustered_roots
                    .lock()
                    .unwrap()
                    .get(&table_id)
                    .copied()
            })
    }

    /// Shared access to the underlying [`ConcurrentWalWriter`].
    pub fn wal(&self) -> &ConcurrentWalWriter {
        &self.wal
    }

    // ── WAL Rotation ──────────────────────────────────────────────────────────

    /// Triggers a checkpoint and rotates the WAL file.
    ///
    /// Takes `&mut self` because replacing the internal WAL writer requires
    /// exclusive access. This is an administrative operation, not a hot path.
    ///
    /// **Must be called with no active transaction.**
    pub fn rotate_wal(
        &mut self,
        storage: &dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<u64, DbError> {
        {
            let set = self.active_set.read().unwrap();
            if !set.is_empty() {
                let first_id = *set.iter().next().unwrap();
                return Err(DbError::TransactionAlreadyActive { txn_id: first_id });
            }
        }

        let checkpoint_lsn = Checkpointer::checkpoint(storage, &self.wal)?;
        ConcurrentWalWriter::rotate_file(wal_path, checkpoint_lsn)?;
        self.wal = ConcurrentWalWriter::open(wal_path)?;

        Ok(checkpoint_lsn)
    }

    /// Opens an existing WAL and runs crash recovery, returning a ready `TxnManager`.
    pub fn open_with_recovery(
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<(Self, RecoveryResult), DbError> {
        let result = CrashRecovery::recover(storage, wal_path)?;
        let wal = ConcurrentWalWriter::open(wal_path)?;
        let mgr = Self {
            wal,
            next_txn_id: AtomicU64::new(result.max_committed + 1),
            max_committed: AtomicU64::new(result.max_committed),
            active_set: RwLock::new(HashSet::new()),
            lowest_active_id: AtomicU64::new(0),
            deferred_commit_mode: false,
            post_commit: Mutex::new(PostCommitBatches::default()),
            durability_policy: WalDurabilityPolicy::Strict,
            last_clustered_roots: Mutex::new(result.clustered_roots.clone()),
        };
        Ok((mgr, result))
    }

    /// Rotates the WAL if its current size exceeds `max_wal_size` bytes.
    pub fn check_and_rotate(
        &mut self,
        storage: &dyn StorageEngine,
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
