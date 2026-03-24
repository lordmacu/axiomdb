//! Database handle — wraps MmapStorage + TxnManager for the server.
//!
//! Each server process opens exactly one `Database`. It is wrapped in
//! `Arc<tokio::sync::Mutex<Database>>` so connection handlers can lock it
//! to execute queries (single-writer constraint, Phase 4).
//!
//! ## Group Commit (Phase 3.19)
//!
//! When `CommitCoordinator` is attached via `set_coordinator()`:
//! - `execute_query` / `execute_stmt` use `TxnManager::commit()` in deferred
//!   mode: DML commits write the Commit WAL entry to the `BufWriter` but do
//!   not fsync. The txn_id is registered with the coordinator.
//! - The caller receives `(QueryResult, Some(CommitRx))` and must **release
//!   the Database lock** before awaiting the receiver.
//! - A background task (see `group_commit.rs`) batches the fsync and notifies
//!   all waiting connections.
//! - Read-only transactions and disabled mode return `(result, None)` and
//!   behave identically to pre-3.19 behavior.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_cached, execute_with_ctx, parse, result::QueryResult, SchemaCache, SessionContext,
};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;

use super::commit_coordinator::CommitCoordinator;
use super::group_commit::spawn_group_commit_task;

/// Receiver for group commit fsync confirmation.
///
/// Resolves to `Ok(())` when the fsync covering the DML commit completes,
/// or `Err(WalGroupCommitFailed)` if the fsync fails.
pub type CommitRx = oneshot::Receiver<Result<(), DbError>>;

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    /// Group commit coordinator. `None` when group commit is disabled
    /// (`group_commit_interval_ms = 0`), in which case every DML commit
    /// fsyncs inline as in pre-3.19 behavior.
    pub coordinator: Option<CommitCoordinator>,
}

impl Database {
    /// Opens or creates a database at `data_dir`.
    ///
    /// Creates the directory and initializes the catalog if not already present.
    pub fn open(data_dir: &Path) -> Result<Self, DbError> {
        std::fs::create_dir_all(data_dir).map_err(DbError::Io)?;

        let db_path = data_dir.join("axiomdb.db");
        let wal_path = data_dir.join("axiomdb.wal");

        let (storage, txn) = if db_path.exists() {
            let storage = MmapStorage::open(&db_path)?;
            let txn = TxnManager::open(&wal_path)?;
            (storage, txn)
        } else {
            let mut storage = MmapStorage::create(&db_path)?;
            CatalogBootstrap::init(&mut storage)?;
            let txn = TxnManager::create(&wal_path)?;
            (storage, txn)
        };

        Ok(Self {
            storage,
            txn,
            coordinator: None,
        })
    }

    /// Attaches a `CommitCoordinator` and enables deferred commit mode.
    ///
    /// Spawns the group commit background task using `db` (the `Arc` wrapping
    /// this `Database`). Must be called once after `open()` when
    /// `group_commit_interval_ms > 0`.
    ///
    /// Returns the `JoinHandle` of the background task. The caller is
    /// responsible for aborting it on shutdown (or it exits on its own when
    /// the `Arc<Mutex<Database>>` is dropped).
    pub fn enable_group_commit(
        db: Arc<Mutex<Database>>,
        interval_ms: u64,
        max_batch: usize,
    ) -> tokio::task::JoinHandle<()> {
        let coordinator = CommitCoordinator::new(max_batch);
        {
            // Brief synchronous lock to set coordinator + enable deferred mode.
            // Safe: called at startup before any connections are accepted.
            let mut guard = db.blocking_lock();
            guard.txn.set_deferred_commit_mode(true);
            guard.coordinator = Some(coordinator);
        }
        spawn_group_commit_task(db, interval_ms)
    }

    /// Executes a SQL string through the full pipeline:
    /// `parse → analyze_cached → execute_with_ctx`.
    ///
    /// Returns `(QueryResult, Option<CommitRx>)`.
    ///
    /// When group commit is enabled and the statement was DML, `CommitRx`
    /// is `Some`. The caller **must release the Database lock** before
    /// awaiting the receiver to avoid blocking all other connections for
    /// the duration of the fsync.
    ///
    /// When group commit is disabled or the statement was read-only,
    /// `CommitRx` is `None` and the commit has already been fsynced before
    /// this function returned.
    pub fn execute_query(
        &mut self,
        sql: &str,
        session: &mut SessionContext,
        schema_cache: &mut SchemaCache,
    ) -> Result<(QueryResult, Option<CommitRx>), DbError> {
        let stmt = parse(sql, None)?;
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze_cached(stmt, &self.storage, snap, schema_cache)?;
        let result = execute_with_ctx(analyzed, &mut self.storage, &mut self.txn, session)?;
        Ok((result, self.take_commit_rx()))
    }

    /// Executes an already-analyzed `Stmt` — used by the prepared statement
    /// plan cache path to skip `parse()` + `analyze()` entirely.
    pub fn execute_stmt(
        &mut self,
        stmt: axiomdb_sql::ast::Stmt,
        session: &mut SessionContext,
    ) -> Result<(QueryResult, Option<CommitRx>), DbError> {
        let result = execute_with_ctx(stmt, &mut self.storage, &mut self.txn, session)?;
        Ok((result, self.take_commit_rx()))
    }

    /// Returns the current database name (always "axiomdb" for Phase 5).
    pub fn current_database(&self) -> &str {
        "axiomdb"
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Checks if the last `commit()` left a pending deferred txn (group commit
    /// mode + DML), and if so, registers it with the coordinator and returns
    /// the confirmation receiver. Returns `None` in all other cases.
    fn take_commit_rx(&mut self) -> Option<CommitRx> {
        let txn_id = self.txn.take_pending_deferred_commit()?;
        let coordinator = self.coordinator.as_ref()?;
        Some(coordinator.register_pending(txn_id))
    }
}
