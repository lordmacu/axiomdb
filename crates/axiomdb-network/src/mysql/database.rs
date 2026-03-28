//! Database handle — wraps MmapStorage + TxnManager for the server.
//!
//! Each server process opens exactly one `Database`. It is wrapped in
//! `Arc<tokio::sync::Mutex<Database>>` so connection handlers can lock it
//! to execute queries (single-writer constraint, Phase 4).
//!
//! ## Fsync Pipeline (Phase 6.19)
//!
//! DML commits use a leader-based fsync pipeline (inspired by MariaDB's
//! `group_commit_lock`). Instead of one `sync_all()` per transaction:
//!
//! 1. The executor writes the Commit WAL entry to the BufWriter (no fsync).
//! 2. `take_commit_rx()` calls `pipeline.acquire(commit_lsn, txn_id)`:
//!    - **Expired**: another leader already fsynced past this LSN → immediate return.
//!    - **Acquired**: this connection becomes leader → flush+fsync+advance, return.
//!    - **Queued**: another leader is active → return receiver; handler awaits it
//!      after releasing the Database lock.
//!
//! This amortises fsyncs across pipelined commits (even single-connection) and
//! supersedes the timer-based group commit from Phase 3.19.
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

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_cached,
    ast::Stmt,
    bloom::BloomRegistry,
    execute_with_ctx, parse,
    result::{ColumnMeta, QueryResult},
    session::{is_ignorable_on_error, OnErrorMode},
    verify_and_repair_indexes_on_open, SchemaCache, SessionContext,
};
use axiomdb_storage::{DbConfig, MmapStorage, WalDurabilityPolicy};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::{AcquireResult, FsyncPipeline, TxnManager};

use super::error::dberror_to_mysql_warning;
use super::status::StatusRegistry;

/// Receiver for fsync pipeline confirmation.
///
/// Resolves to `Ok(())` when the fsync covering the DML commit completes,
/// or `Err(WalGroupCommitFailed)` / `Err(DiskFull)` if the fsync fails.
pub type CommitRx = axiomdb_wal::CommitRx;

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
    /// Leader-based fsync pipeline (Phase 6.19). Always active — replaces the
    /// timer-based group commit from Phase 3.19.
    pub pipeline: FsyncPipeline,
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
    /// holding the `Database` lock.
    pub runtime_mode: Arc<AtomicU8>,
}

impl Database {
    /// Opens or creates a database at `data_dir`.
    ///
    /// Creates the directory and initializes the catalog if not already present.
    pub fn open(data_dir: &Path) -> Result<Self, DbError> {
        Self::open_with_config(data_dir, &DbConfig::default())
    }

    /// Opens or creates a database at `data_dir` with explicit configuration.
    ///
    /// The resolved `WalDurabilityPolicy` controls whether the fsync pipeline
    /// is used (`Strict`) or bypassed (`Normal` / `Off`).
    pub fn open_with_config(data_dir: &Path, config: &DbConfig) -> Result<Self, DbError> {
        std::fs::create_dir_all(data_dir).map_err(DbError::Io)?;

        let db_path = data_dir.join("axiomdb.db");
        let wal_path = data_dir.join("axiomdb.wal");

        let (storage, mut txn) = if db_path.exists() {
            let mut storage = MmapStorage::open(&db_path)?;
            let (mut txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
            verify_and_repair_indexes_on_open(&mut storage, &mut txn)?;
            (storage, txn)
        } else {
            let mut storage = MmapStorage::create(&db_path)?;
            CatalogBootstrap::init(&mut storage)?;
            let txn = TxnManager::create(&wal_path)?;
            (storage, txn)
        };

        let durability = config.resolved_wal_durability();
        txn.set_durability_policy(durability);

        // Strict mode: enable pipeline commit so `TxnManager::commit()` writes
        // the Commit WAL entry to the BufWriter without inline fsync. The fsync
        // pipeline handles durability via leader-based coalescing.
        //
        // Normal / Off: bypass the pipeline — relaxed modes flush or skip
        // inline and advance max_committed immediately in `commit()`.
        if durability == WalDurabilityPolicy::Strict {
            txn.set_deferred_commit_mode(true);
        }
        let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

        Ok(Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            pipeline,
            schema_version: Arc::new(AtomicU64::new(0)),
            status: Arc::new(StatusRegistry::new()),
            runtime_mode: Arc::new(AtomicU8::new(RUNTIME_MODE_READ_WRITE)),
        })
    }

    // NOTE: `enable_group_commit()` removed in Phase 6.19.
    // The fsync pipeline is always active — initialized in `open()`.

    /// Executes a SQL string through the full pipeline:
    /// `parse → analyze_cached → execute_with_ctx`.
    ///
    /// Returns `(QueryResult, Option<CommitRx>)`.
    ///
    /// Increments `schema_version` after any successful DDL statement so that
    /// connections can detect stale prepared statement plans (Phase 5.13).
    ///
    /// For DML statements that queue behind an active fsync leader, `CommitRx`
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

        // ── parse ─────────────────────────────────────────────────────────────
        let stmt = match parse(sql, None) {
            Ok(s) => s,
            Err(e) => return self.apply_on_error_pipeline_failure(sql, session, e),
        };
        let is_ddl = is_schema_changing(&stmt);

        // ── analyze ───────────────────────────────────────────────────────────
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = match analyze_cached(stmt, &self.storage, snap, schema_cache) {
            Ok(a) => a,
            Err(e) => return self.apply_on_error_pipeline_failure(sql, session, e),
        };

        // ── execute ───────────────────────────────────────────────────────────
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
        // For on_error = 'ignore', executor already rolled back the statement and
        // returned Err — we intercept here to convert it to a warning + success.
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                return self.apply_on_error_pipeline_failure(sql, session, e);
            }
        };
        if is_ddl {
            self.schema_version.fetch_add(1, Ordering::Release);
        }
        Ok((result, self.take_commit_rx()))
    }

    /// Applies the session `on_error` policy to a pipeline failure from
    /// parse, analyze, or execute.
    ///
    /// - `RollbackStatement` / `Savepoint`: keep any active txn open, return ERR.
    /// - `RollbackTransaction`: roll back the whole active txn, return ERR.
    /// - `Ignore` (ignorable error): add warning, return `QueryResult::Empty`.
    /// - `Ignore` (non-ignorable): roll back active txn, return ERR.
    fn apply_on_error_pipeline_failure(
        &mut self,
        sql: &str,
        session: &mut SessionContext,
        err: DbError,
    ) -> Result<(QueryResult, Option<CommitRx>), DbError> {
        if matches!(err, DbError::DiskFull { .. }) {
            self.enter_degraded_mode();
        }
        match session.on_error {
            OnErrorMode::RollbackTransaction => {
                if self.txn.active_txn_id().is_some() {
                    let _ = self.txn.rollback(&mut self.storage);
                }
                Err(err)
            }
            OnErrorMode::Ignore if is_ignorable_on_error(&err) => {
                let (code, message) = dberror_to_mysql_warning(&err, Some(sql));
                session.warn(code, message);
                Ok((QueryResult::Empty, None))
            }
            OnErrorMode::Ignore => {
                if self.txn.active_txn_id().is_some() {
                    let _ = self.txn.rollback(&mut self.storage);
                }
                Err(err)
            }
            _ => {
                // RollbackStatement / Savepoint / Ignore non-ignorable:
                // executor already handled statement-level rollback where applicable.
                Err(err)
            }
        }
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

    /// Releases deferred-free pages for the given committed transaction IDs.
    ///
    /// Wraps `TxnManager::release_committed_frees` to avoid split-borrow issues
    /// when both `txn` and `storage` are fields of the same `Database`.
    pub fn release_deferred_frees(
        &mut self,
        txn_ids: &[axiomdb_core::TxnId],
    ) -> Result<(), axiomdb_core::error::DbError> {
        self.txn.release_committed_frees(&mut self.storage, txn_ids)
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

    /// Drives the fsync pipeline for the last deferred DML commit.
    ///
    /// If the last `commit()` left a pending deferred txn_id, calls
    /// `pipeline.acquire(commit_lsn, txn_id)`:
    ///
    /// - **Expired**: another leader already fsynced past this LSN.
    ///   Advance `max_committed` and release deferred frees. Return `None`.
    /// - **Acquired**: this connection is the leader. Perform flush+fsync,
    ///   advance `max_committed` for this txn + all woken followers,
    ///   release deferred frees. Return `None`.
    /// - **Queued**: another leader is running. Return `Some(rx)` — the
    ///   handler must release the Database lock before awaiting it.
    ///
    /// Returns `None` for read-only commits (no pending deferred txn).
    fn take_commit_rx(&mut self) -> Option<CommitRx> {
        let txn_id = self.txn.take_pending_deferred_commit()?;
        let commit_lsn = self.txn.wal_current_lsn();

        match self.pipeline.acquire(commit_lsn, txn_id) {
            AcquireResult::Expired => {
                // Another leader already fsynced past our LSN.
                self.txn.advance_committed_single(txn_id);
                let _ = self
                    .txn
                    .release_committed_frees(&mut self.storage, &[txn_id]);
                None
            }
            AcquireResult::Acquired => {
                // We are the leader — flush + fsync.
                let fsync_result = self.txn.wal_flush_and_fsync();
                match fsync_result {
                    Ok(()) => {
                        let flushed_lsn = self.txn.wal_current_lsn();
                        // Advance our own txn.
                        self.txn.advance_committed_single(txn_id);
                        let _ = self
                            .txn
                            .release_committed_frees(&mut self.storage, &[txn_id]);
                        // Wake followers and advance their txns.
                        let woken_ids = self.pipeline.release_ok(flushed_lsn);
                        if !woken_ids.is_empty() {
                            self.txn.advance_committed(&woken_ids);
                            let _ = self
                                .txn
                                .release_committed_frees(&mut self.storage, &woken_ids);
                        }
                        None
                    }
                    Err(ref e) => {
                        let is_disk_full = matches!(e, DbError::DiskFull { .. });
                        if is_disk_full {
                            self.enter_degraded_mode();
                        }
                        let msg = e.to_string();
                        self.pipeline.release_err(&msg, is_disk_full);
                        // Return the error as a failed commit for this connection.
                        // We create a oneshot and send the error immediately.
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let err = if is_disk_full {
                            DbError::DiskFull {
                                operation: "wal pipeline fsync",
                            }
                        } else {
                            DbError::WalGroupCommitFailed { message: msg }
                        };
                        let _ = tx.send(Err(err));
                        Some(rx)
                    }
                }
            }
            AcquireResult::Queued(rx) => {
                // Another leader is running — we must await the fsync.
                // NOTE: max_committed for this txn will be advanced by the
                // leader via release_ok → advance_committed on the woken_ids.
                // However, the leader only knows about woken_ids from the
                // pipeline, not from TxnManager. We need the leader to
                // advance max_committed for queued followers.
                //
                // Problem: the leader calls self.txn.advance_committed(&woken_ids)
                // but those woken_ids come from the pipeline's Waiter.txn_id.
                // This works because we stored txn_id in the pipeline.
                //
                // Deferred frees: the handler will call release_deferred_frees
                // after receiving Ok from the pipeline. But the handler doesn't
                // have access to Database... Actually, the leader already calls
                // release_committed_frees for the woken_ids. So we're covered.
                Some(rx)
            }
        }
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use axiomdb_core::error::DbError;
    use axiomdb_sql::{SchemaCache, SessionContext};

    use super::{Database, OnErrorMode, QueryResult};

    fn open_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().expect("temp dir");
        let db = Database::open(dir.path()).expect("open db");
        (dir, db)
    }

    #[test]
    fn test_ignore_parse_error_becomes_warning_and_keeps_txn_open() {
        let (_dir, mut db) = open_db();
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::default();

        session.on_error = OnErrorMode::Ignore;
        db.txn.begin().expect("begin txn");

        let result = db
            .execute_query("SELEC 1", &mut session, &mut cache)
            .expect("ignore parse error returns success");

        assert!(
            matches!(result.0, QueryResult::Empty),
            "ignored parse error must serialize as Empty/OK"
        );
        assert!(
            db.txn.active_txn_id().is_some(),
            "ignorable parse error must keep active txn open in ignore mode"
        );
        assert_eq!(session.warning_count(), 1);
        assert_eq!(session.warnings[0].code, 1064);
        assert!(
            session.warnings[0]
                .message
                .to_ascii_lowercase()
                .contains("error in your sql syntax"),
            "warning should preserve original error text: {}",
            session.warnings[0].message
        );
    }

    #[test]
    fn test_ignore_non_ignorable_error_rolls_back_active_txn() {
        let (_dir, mut db) = open_db();
        let mut session = SessionContext::new();

        session.on_error = OnErrorMode::Ignore;
        db.txn.begin().expect("begin txn");

        let err = db
            .apply_on_error_pipeline_failure(
                "INSERT INTO t VALUES (1)",
                &mut session,
                DbError::DiskFull { operation: "test" },
            )
            .expect_err("disk full must still return ERR");

        assert!(matches!(err, DbError::DiskFull { .. }));
        assert!(
            db.txn.active_txn_id().is_none(),
            "non-ignorable ignore error must eagerly roll back the txn"
        );
        assert_eq!(session.warning_count(), 0);
    }
}
