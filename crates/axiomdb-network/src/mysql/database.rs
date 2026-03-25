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
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_cached,
    ast::Stmt,
    bloom::BloomRegistry,
    execute_with_ctx, parse,
    result::{ColumnMeta, QueryResult},
    SchemaCache, SessionContext,
};
use axiomdb_storage::MmapStorage;
use axiomdb_types::{DataType, Value};
use axiomdb_wal::TxnManager;

use super::commit_coordinator::CommitCoordinator;
use super::group_commit::spawn_group_commit_task;
use super::status::StatusRegistry;

/// Receiver for group commit fsync confirmation.
///
/// Resolves to `Ok(())` when the fsync covering the DML commit completes,
/// or `Err(WalGroupCommitFailed)` / `Err(DiskFull)` if the fsync fails.
pub type CommitRx = oneshot::Receiver<Result<(), DbError>>;

// ── RuntimeMode ───────────────────────────────────────────────────────────────

/// Shared runtime mode for the whole opened database process.
///
/// Stored as an `AtomicU8` so the background group-commit task and all
/// connection handlers can read it without holding the `Database` lock.
///
/// The transition is one-way: `ReadWrite → ReadOnlyDegraded`. Once degraded
/// the mode persists until the process restarts.
pub const RUNTIME_MODE_READ_WRITE: u8 = 0;
pub const RUNTIME_MODE_DEGRADED: u8 = 1;

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    /// Bloom filter registry for secondary index lookups.
    pub bloom: BloomRegistry,
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
    /// Server-wide status counters (Phase 5.9c).
    ///
    /// Connections clone this `Arc` once at connect time. Counter updates
    /// use atomics — no lock needed after the initial clone.
    pub status: Arc<StatusRegistry>,
    /// Shared runtime mode (`RUNTIME_MODE_READ_WRITE` / `RUNTIME_MODE_DEGRADED`).
    ///
    /// Connections clone this `Arc` at connect time and poll it without
    /// holding the `Database` lock. The group-commit background task also
    /// holds a clone so it can flip the mode on a disk-full fsync failure.
    pub runtime_mode: Arc<AtomicU8>,
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
            bloom: BloomRegistry::new(),
            coordinator: None,
            schema_version: Arc::new(AtomicU64::new(0)),
            status: Arc::new(StatusRegistry::new()),
            runtime_mode: Arc::new(AtomicU8::new(RUNTIME_MODE_READ_WRITE)),
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
        // Clear warnings from the previous statement (MySQL clears on each new statement),
        // except for SHOW WARNINGS itself — it must see the warnings it's reporting.
        let lower_trim = sql.trim().to_ascii_lowercase();
        if !lower_trim.starts_with("show warnings") {
            session.clear_warnings();
        }

        let lower = sql.trim().to_ascii_lowercase();

        // ── @@in_transaction ──────────────────────────────────────────────────
        // Returns 1 when inside an active transaction, 0 otherwise.
        // Handled here (not in the executor) because it requires txn state.
        if lower.contains("@@in_transaction") && lower.starts_with("select") {
            let val = if self.txn.active_txn_id().is_some() {
                Value::Int(1)
            } else {
                Value::Int(0)
            };
            let result = QueryResult::Rows {
                columns: vec![ColumnMeta {
                    name: "@@in_transaction".into(),
                    data_type: DataType::Int,
                    nullable: false,
                    table_name: None,
                }],
                rows: vec![vec![val]],
            };
            return Ok((result, None));
        }

        // ── SHOW WARNINGS ─────────────────────────────────────────────────────
        // Returns the warnings accumulated by the previous statement.
        if lower == "show warnings" || lower == "show warnings;" {
            let rows = session
                .warnings
                .iter()
                .map(|w| {
                    vec![
                        Value::Text(w.level.to_string()),
                        Value::Int(w.code as i32),
                        Value::Text(w.message.clone()),
                    ]
                })
                .collect();
            let result = QueryResult::Rows {
                columns: vec![
                    ColumnMeta {
                        name: "Level".into(),
                        data_type: DataType::Text,
                        nullable: false,
                        table_name: None,
                    },
                    ColumnMeta {
                        name: "Code".into(),
                        data_type: DataType::Int,
                        nullable: false,
                        table_name: None,
                    },
                    ColumnMeta {
                        name: "Message".into(),
                        data_type: DataType::Text,
                        nullable: false,
                        table_name: None,
                    },
                ],
                rows,
            };
            return Ok((result, None));
        }

        if self.is_degraded() && sql_may_mutate(sql) {
            return Err(DbError::DiskFull {
                operation: "database is in read-only degraded mode",
            });
        }

        let stmt = parse(sql, None)?;
        let is_ddl = is_schema_changing(&stmt);
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze_cached(stmt, &self.storage, snap, schema_cache)?;
        let result = execute_with_ctx(
            analyzed,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            session,
        );
        if let Err(ref e) = result {
            if matches!(e, DbError::DiskFull { .. }) {
                self.enter_degraded_mode();
            }
        }
        let result = result?;
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
        if self.is_degraded() && stmt_may_mutate(&stmt) {
            return Err(DbError::DiskFull {
                operation: "database is in read-only degraded mode",
            });
        }
        let is_ddl = is_schema_changing(&stmt);
        let result = execute_with_ctx(
            stmt,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            session,
        );
        if let Err(ref e) = result {
            if matches!(e, DbError::DiskFull { .. }) {
                self.enter_degraded_mode();
            }
        }
        let result = result?;
        if is_ddl {
            self.schema_version.fetch_add(1, Ordering::Release);
        }
        Ok((result, self.take_commit_rx()))
    }

    /// Returns `true` if the database has entered read-only degraded mode due
    /// to a previous disk-full error.
    pub fn is_degraded(&self) -> bool {
        self.runtime_mode.load(Ordering::Acquire) == RUNTIME_MODE_DEGRADED
    }

    /// Transitions the database to read-only degraded mode (one-way).
    ///
    /// Safe to call from any thread. Has no effect if already degraded.
    pub fn enter_degraded_mode(&self) {
        self.runtime_mode
            .store(RUNTIME_MODE_DEGRADED, Ordering::Release);
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

/// Quick keyword-level check: does this SQL string look like it may mutate
/// durable state?
///
/// Used to gate statements before they reach WAL/storage when the database
/// is in read-only degraded mode. Conservative — prefers false positives
/// (blocking a SELECT that starts with "INSERT" is fine; never allow a real
/// INSERT to slip through).
///
/// Not a substitute for the parser — only used as a fast pre-check.
pub fn sql_may_mutate(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    // DML
    lower.starts_with("insert")
        || lower.starts_with("update")
        || lower.starts_with("delete")
        || lower.starts_with("truncate")
        // DDL
        || lower.starts_with("create")
        || lower.starts_with("drop")
        || lower.starts_with("alter")
        // Transaction control
        || lower.starts_with("begin")
        || lower.starts_with("start transaction")
        || lower.starts_with("commit")
        || lower.starts_with("rollback")
        || lower.starts_with("savepoint")
        || lower.starts_with("release")
}

/// Returns `true` if the parsed `Stmt` may mutate durable state.
///
/// Used for the prepared-statement execute path where we already have
/// the parsed AST — avoids re-parsing just for the gate check.
fn stmt_may_mutate(stmt: &Stmt) -> bool {
    !matches!(
        stmt,
        Stmt::Select(_) | Stmt::ShowTables(_) | Stmt::ShowColumns(_) | Stmt::Set(_)
    )
}
