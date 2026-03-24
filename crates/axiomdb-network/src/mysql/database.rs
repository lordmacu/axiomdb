//! Database handle — wraps MmapStorage + TxnManager for the server.
//!
//! Each server process opens exactly one `Database`. It is wrapped in
//! `Arc<tokio::sync::Mutex<Database>>` so connection handlers can lock it
//! to execute queries (single-writer constraint, Phase 4).

use std::path::Path;

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, result::QueryResult, SessionContext};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
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

        Ok(Self { storage, txn })
    }

    /// Executes a SQL string through the full pipeline:
    /// `parse → analyze → execute_with_ctx`.
    ///
    /// Uses `autocommit` semantics: each statement is wrapped in BEGIN/COMMIT
    /// unless an explicit transaction is already active.
    pub fn execute_query(
        &mut self,
        sql: &str,
        session: &mut SessionContext,
    ) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(analyzed, &mut self.storage, &mut self.txn, session)
    }

    /// Executes an already-analyzed `Stmt` — used by the prepared statement
    /// plan cache path to skip `parse()` + `analyze()` entirely.
    pub fn execute_stmt(
        &mut self,
        stmt: axiomdb_sql::ast::Stmt,
        session: &mut SessionContext,
    ) -> Result<QueryResult, DbError> {
        execute_with_ctx(stmt, &mut self.storage, &mut self.txn, session)
    }

    /// Returns the current database name (always "axiomdb" for Phase 5).
    pub fn current_database(&self) -> &str {
        "axiomdb"
    }
}
