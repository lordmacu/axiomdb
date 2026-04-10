impl TxnManager {
    // ── Construction ─────────────────────────────────────────────────────────

    /// Creates a fresh WAL file and a new TxnManager.
    ///
    /// Fails if the WAL file already exists.
    pub fn create(wal_path: &Path) -> Result<Self, DbError> {
        let wal = ConcurrentWalWriter::create(wal_path)?;
        Ok(Self {
            wal,
            next_txn_id: 1,
            max_committed: AtomicU64::new(0),
            active_set: RwLock::new(HashSet::new()),
            lowest_active_id: AtomicU64::new(0),
            deferred_commit_mode: false,
            committed_free_batches: Vec::new(),
            committed_steal_protection: Vec::new(),
            committed_recycle_pages: Vec::new(),
            durability_policy: WalDurabilityPolicy::Strict,
            last_clustered_roots: HashMap::new(),
            pending_deferred_txn_id: None,
        })
    }

    /// Opens an existing WAL file, scanning it to recover `max_committed` and
    /// the latest committed clustered root per table.
    ///
    /// Does not replay DML entries — full crash recovery is handled in Phase 3.8.
    /// Only the highest committed TxnId is restored so that new transactions
    /// receive monotonically increasing IDs and snapshots are correct.
    pub fn open(wal_path: &Path) -> Result<Self, DbError> {
        let (max_committed, clustered_roots) = scan_committed_state(wal_path)?;
        let wal = ConcurrentWalWriter::open(wal_path)?;
        Ok(Self {
            wal,
            next_txn_id: max_committed + 1,
            max_committed: AtomicU64::new(max_committed),
            active_set: RwLock::new(HashSet::new()),
            lowest_active_id: AtomicU64::new(0),
            deferred_commit_mode: false,
            committed_free_batches: Vec::new(),
            committed_steal_protection: Vec::new(),
            committed_recycle_pages: Vec::new(),
            durability_policy: WalDurabilityPolicy::Strict,
            last_clustered_roots: clustered_roots,
            pending_deferred_txn_id: None,
        })
    }

}
