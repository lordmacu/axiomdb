impl TxnManager {
    // ── Construction ─────────────────────────────────────────────────────────

    /// Creates a fresh WAL file and a new TxnManager.
    ///
    /// Fails if the WAL file already exists.
    pub fn create(wal_path: &Path) -> Result<Self, DbError> {
        let wal = ConcurrentWalWriter::create(wal_path)?;
        Ok(Self {
            wal,
            next_txn_id: AtomicU64::new(1),
            max_committed: AtomicU64::new(0),
            active_set: RwLock::new(HashSet::new()),
            lowest_active_id: AtomicU64::new(0),
            write_commit_seq: AtomicU64::new(0),
            deferred_commit_mode: false,
            post_commit: Mutex::new(PostCommitBatches::default()),
            durability_policy: WalDurabilityPolicy::Strict,
            last_clustered_roots: Mutex::new(HashMap::new()),
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
            next_txn_id: AtomicU64::new(max_committed + 1),
            max_committed: AtomicU64::new(max_committed),
            active_set: RwLock::new(HashSet::new()),
            lowest_active_id: AtomicU64::new(0),
            write_commit_seq: AtomicU64::new(0),
            deferred_commit_mode: false,
            post_commit: Mutex::new(PostCommitBatches::default()),
            durability_policy: WalDurabilityPolicy::Strict,
            last_clustered_roots: Mutex::new(clustered_roots),
        })
    }

    /// Sets the WAL durability policy. Must be called before any transactions.
    pub fn set_durability_policy(&mut self, policy: WalDurabilityPolicy) {
        self.durability_policy = policy;
    }

    /// Enables or disables deferred commit mode for the server-side fsync pipeline.
    /// Must be called before any transactions.
    pub fn set_deferred_commit_mode(&mut self, enabled: bool) {
        self.deferred_commit_mode = enabled;
    }
}
