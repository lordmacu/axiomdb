//! Embedded fast-path INSERT builder.
//!
//! See `specs/fase-perf-sqlite-gap/spec-embedded-appender.md` and
//! `docs/perf-sqlite-gap.md` "Attack 7" for design rationale.
//!
//! The Appender skips the SQL parser, analyzer, dispatcher, and the
//! `execute_with_ctx` per-statement scaffolding (~154µs/row on Lima),
//! and writes typed [`Value`]s directly through the existing batched
//! heap-insert path (`TableEngine::insert_rows_batch_with_ctx`).
//!
//! Mirrors DuckDB's Appender API and SQLite's `sqlite3_bind_*` +
//! `sqlite3_step` pattern.

use axiomdb_catalog::schema::{ColumnDef, TableDef};
use axiomdb_catalog::SchemaResolver;
use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use axiomdb_wal::ConnectionTxn;

use crate::Db;

/// Auto-flush threshold. After this many buffered rows, the next
/// `append_row` triggers a `flush()` to keep memory bounded. Wired in
/// Step 6 of plan-embedded-appender.md.
#[allow(dead_code)] // used from Step 6 onward
pub(crate) const APPENDER_BATCH_FLUSH: usize = 1024;

/// A fast-path INSERT builder for the embedded API.
///
/// Created via [`Db::appender`]; consumed by [`Appender::finish`] (commit)
/// or dropped (rollback). See the crate docs for the full design.
pub struct Appender<'db> {
    pub(crate) db: &'db mut Db,
    pub(crate) table_def: TableDef,
    pub(crate) columns: Vec<ColumnDef>,
    /// The transaction held for the Appender's lifetime. `Some` while
    /// alive, `None` after `finish()` consumes it or Drop rolls it back.
    pub(crate) conn_txn: Option<ConnectionTxn>,
    /// In-memory row buffer. Drained on `flush()`; cleared on Drop.
    pub(crate) buffer: Vec<Vec<Value>>,
    /// Total rows successfully written across all `flush()` calls.
    pub(crate) rows_inserted: u64,
}

impl<'db> std::fmt::Debug for Appender<'db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Appender")
            .field("table", &self.table_def.table_name)
            .field("pending", &self.buffer.len())
            .field("rows_inserted", &self.rows_inserted)
            .finish_non_exhaustive()
    }
}

impl<'db> Appender<'db> {
    pub(crate) fn open(db: &'db mut Db, table_name: &str) -> Result<Self, DbError> {
        // Reject if a SQL transaction is already open — Appender owns its
        // own transaction in v1; sharing semantics with user txns is
        // deferred. See spec "Open questions" → resolved.
        if let Some(ref existing) = db.session.conn_txn {
            return Err(DbError::TransactionAlreadyActive {
                txn_id: existing.txn_id,
            });
        }

        // Resolve via the catalog directly — bypasses the SQL-layer
        // resolve_table_cached (which takes a TableRef and is crate-private
        // to axiomdb-sql). For v1 we always look in `public`; multi-schema
        // resolution can come later if needed.
        let snap = db.txn.snapshot();
        let mut resolver = SchemaResolver::new(
            &db.storage,
            snap,
            axiomdb_catalog::schema::DEFAULT_DATABASE_NAME,
            "public",
        )?;
        let resolved = resolver.resolve_table(None, table_name)?;
        let table_def = resolved.def.clone();
        let columns = resolved.columns.clone();

        if table_def.is_clustered() {
            return Err(DbError::NotImplemented {
                feature: "Appender on clustered tables — use SQL INSERT \
                          (deferred to a follow-up Attack)"
                    .to_string(),
            });
        }
        if !table_def.triggers.is_empty() {
            return Err(DbError::NotImplemented {
                feature: "Appender on tables with triggers — use SQL INSERT \
                          (deferred to a follow-up Attack)"
                    .to_string(),
            });
        }

        // Open the appender's transaction and stamp the session's current
        // durability override (Attack 6).
        let mut conn_txn = db.txn.begin()?;
        conn_txn.durability_override = Some(db.session.synchronous().to_wal_policy());

        Ok(Self {
            db,
            table_def,
            columns,
            conn_txn: Some(conn_txn),
            buffer: Vec::with_capacity(APPENDER_BATCH_FLUSH),
            rows_inserted: 0,
        })
    }

    /// Number of rows currently buffered (not yet written to heap).
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}
