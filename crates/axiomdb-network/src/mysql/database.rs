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
//!
//! ## Prepared Statement Plan Cache (Phase 5.13)
//!
//! `schema_version` is a global monotonic counter incremented after every
//! successful DDL statement (CREATE/DROP/ALTER TABLE, CREATE/DROP INDEX,
//! TRUNCATE). Each connection clones `Arc<AtomicU64>` at connect time and
//! can poll it lock-free. When a connection's cached `compiled_at_version`
//! diverges from `schema_version`, the cached plan is stale and must be
//! re-analyzed before execution.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_cached, ast::Stmt, execute_with_ctx, parse, result::QueryResult, SchemaCache,
    SessionContext,
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
    /// Global schema version. Incremented after every successful DDL
    /// (CREATE/DROP/ALTER TABLE, CREATE/DROP INDEX, TRUNCATE).
    ///
    /// Connections clone this `Arc` at connect time. Reading it requires no
    /// lock — use `load(Ordering::Acquire)`. Writing requires holding the
    /// `Database` lock (only `execute_query`/`execute_stmt` write it).
    pub schema_version: Arc<AtomicU64>,
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
            schema_version: Arc::new(AtomicU64::new(0)),
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
    /// Increments `schema_version` after any successful DDL statement so that
    /// connections can detect stale prepared statement plans (Phase 5.13).
    ///
    /// When group commit is enabled and the statement was DML, `CommitRx`
    /// is `Some`. The caller **must release the Database lock** before
    /// awaiting the receiver to avoid blocking all other connections for
    /// the duration of the fsync.
    pub fn execute_query(
        &mut self,
        sql: &str,
        session: &mut SessionContext,
        schema_cache: &mut SchemaCache,
    ) -> Result<(QueryResult, Option<CommitRx>), DbError> {
        let stmt = parse(sql, None)?;
        let is_ddl = is_schema_changing(&stmt);
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze_cached(stmt, &self.storage, snap, schema_cache)?;
        let result = execute_with_ctx(analyzed, &mut self.storage, &mut self.txn, session)?;
        if is_ddl {
            self.schema_version.fetch_add(1, Ordering::Release);
        }
        Ok((result, self.take_commit_rx()))
    }

    /// Executes an already-analyzed `Stmt` — used by the prepared statement
    /// plan cache path to skip `parse()` + `analyze()` entirely.
    ///
    /// Also increments `schema_version` on DDL (Phase 5.13).
    pub fn execute_stmt(
        &mut self,
        stmt: Stmt,
        session: &mut SessionContext,
    ) -> Result<(QueryResult, Option<CommitRx>), DbError> {
        let is_ddl = is_schema_changing(&stmt);
        let result = execute_with_ctx(stmt, &mut self.storage, &mut self.txn, session)?;
        if is_ddl {
            self.schema_version.fetch_add(1, Ordering::Release);
        }
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

/// Returns `true` if `stmt` is a DDL statement that modifies the schema.
///
/// Used to decide whether to increment `Database::schema_version` after
/// a successful execution, signalling connections to re-validate their
/// cached prepared statement plans (Phase 5.13).
fn is_schema_changing(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::DropTable(_)
            | Stmt::AlterTable(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropIndex(_)
            | Stmt::TruncateTable(_)
    )
}
