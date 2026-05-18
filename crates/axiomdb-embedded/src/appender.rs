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

use axiomdb_catalog::schema::{ColumnDef, ConstraintDef, FkDef, IndexDef, TableDef};
use axiomdb_catalog::SchemaResolver;
use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use axiomdb_wal::ConnectionTxn;

use crate::Db;

/// Auto-flush threshold. After this many buffered rows, `append_row`
/// triggers a `flush()` to keep memory bounded even for very large
/// loads.
pub(crate) const APPENDER_BATCH_FLUSH: usize = 1024;

/// A fast-path INSERT builder for the embedded API.
///
/// Created via [`Db::appender`]; consumed by [`Appender::finish`] (commit)
/// or dropped (rollback). See the crate docs for the full design.
pub struct Appender<'db> {
    pub(crate) db: &'db mut Db,
    pub(crate) table_def: TableDef,
    pub(crate) columns: Vec<ColumnDef>,
    /// Secondary indexes maintained on flush (Step 5). Captured at open
    /// time so we don't re-resolve per flush.
    #[allow(dead_code)] // wired in Step 5 (secondary index maintenance)
    pub(crate) indexes: Vec<IndexDef>,
    /// Named CHECK constraints (v1.1 Step 4). Captured at open; checked
    /// against every appended row.
    pub(crate) constraints: Vec<ConstraintDef>,
    /// FOREIGN KEYs where THIS table is the child (v1.1 Step 5).
    /// Captured at open; immediate FKs validated per-row in append_row;
    /// deferred FKs queued in `session.deferred_fk_constraint_ids`
    /// and resolved at finish() (commit triggers deferred FK check).
    pub(crate) foreign_keys: Vec<FkDef>,
    /// Column index of the AUTO_INCREMENT column, if any (v1.1 Step 2).
    /// At most one per table; resolved once at open.
    pub(crate) auto_inc_col: Option<usize>,
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
        let indexes = resolved.indexes.clone();

        if table_def.is_clustered() {
            return Err(DbError::NotImplemented {
                feature: "Appender on clustered tables — use SQL INSERT \
                          (deferred to a follow-up Attack)"
                    .to_string(),
            });
        }
        if !table_def.triggers.is_empty() {
            return Err(DbError::NotImplemented {
                feature: "Appender on tables with triggers — use SQL INSERT".to_string(),
            });
        }
        // Open the appender's transaction and stamp the session's current
        // durability override (Attack 6).
        let mut conn_txn = db.txn.begin()?;
        conn_txn.durability_override = Some(db.session.synchronous().to_wal_policy());

        let auto_inc_col = columns.iter().position(|c| c.auto_increment);
        let constraints = resolved.constraints.clone();
        let foreign_keys = resolved.foreign_keys.clone();

        Ok(Self {
            db,
            table_def,
            columns,
            indexes,
            constraints,
            foreign_keys,
            auto_inc_col,
            conn_txn: Some(conn_txn),
            buffer: Vec::with_capacity(APPENDER_BATCH_FLUSH),
            rows_inserted: 0,
        })
    }

    /// Number of rows currently buffered (not yet written to heap).
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Append one row.
    ///
    /// `values.len()` must equal the table's column count. NULL columns
    /// must be passed explicitly as `Value::Null`. Type coercion uses
    /// the session's current `strict_mode` (mirrors SQL INSERT).
    ///
    /// Errors are returned immediately — on error the row is NOT added
    /// to the batch and subsequent calls remain valid.
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] if `values.len() != n_columns`.
    /// - [`DbError::TypeMismatch`] if strict coercion fails.
    /// - [`DbError::NotNullViolation`] if a `Value::Null` is appended for
    ///   a `NOT NULL` column.
    pub fn append_row(&mut self, values: &[Value]) -> Result<(), DbError> {
        self.append_row_owned(values.to_vec())
    }

    /// Owned variant — consumes the `Vec` to skip the per-row clone of
    /// `append_row(&[Value])`. Use this when the caller is producing
    /// `Vec<Value>` natively (e.g. building rows in a loop).
    pub fn append_row_owned(&mut self, mut values: Vec<Value>) -> Result<(), DbError> {
        if values.len() != self.columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", self.columns.len()),
                got: format!("{} values", values.len()),
            });
        }
        // v1.1 Step 2: AUTO_INCREMENT auto-assign.
        // SQL semantics: if the AUTO_INC column is Null, assign the next
        // value; if an explicit value is provided, respect it. The cache
        // seeds from MAX(col) on first call so explicit values inserted
        // earlier (or by a parallel session) are honored.
        if let Some(col_idx) = self.auto_inc_col {
            if matches!(values[col_idx], Value::Null) {
                let conn_txn = self
                    .conn_txn
                    .as_ref()
                    .expect("Appender has active txn while alive");
                let next = axiomdb_sql::next_auto_increment_value(
                    &self.db.storage,
                    &self.db.txn,
                    conn_txn,
                    &self.table_def,
                    &self.columns,
                    col_idx,
                )?;
                // Match the column's declared type — Int or BigInt
                use axiomdb_catalog::schema::ColumnType;
                values[col_idx] = match self.columns[col_idx].col_type {
                    ColumnType::BigInt => Value::BigInt(next as i64),
                    _ => Value::Int(next as i32),
                };
            }
        }
        // v1.1 Step 3: GENERATED ALWAYS STORED columns.
        // SQL semantics: explicit non-NULL value for a generated column
        // is rejected. The `materialize_generated_columns` helper
        // unconditionally overwrites the slot — it doesn't enforce this
        // check (the SQL pipeline does it earlier via
        // `validate_generated_insert_source_values`, which works on
        // col_positions). We inline the check here for the Appender.
        for (idx, col) in self.columns.iter().enumerate() {
            if col.generated_expr.is_some() && !matches!(values[idx], Value::Null) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "generated column '{}.{}' cannot be assigned explicitly",
                        self.table_def.table_name, col.name
                    ),
                });
            }
        }
        axiomdb_sql::materialize_generated_columns(&self.columns, &mut values)?;
        // v1.1 Step 4: text constraints (CHAR(N) right-pad, VARCHAR(N)
        // length check). Mutates values in place. SQL INSERT runs this
        // BEFORE coercion / NOT NULL — same here.
        axiomdb_sql::enforce_text_constraints(&self.columns, &mut values)?;
        // Coerce + emit warnings if permissive. row_num is 1-based per
        // the SQL convention so the warning text reads correctly.
        let coerced = axiomdb_sql::coerce_values_with_ctx(
            values,
            &self.columns,
            &mut self.db.session,
            self.buffer.len() + 1,
        )?;
        // NOT NULL check — same semantic as the SQL INSERT path.
        for (col, v) in self.columns.iter().zip(coerced.iter()) {
            if matches!(v, Value::Null) && !col.nullable {
                return Err(DbError::NotNullViolation {
                    table: self.table_def.table_name.clone(),
                    column: col.name.clone(),
                });
            }
        }
        // v1.1 Step 4: CHECK constraints — `expr` must be TRUE or
        // UNKNOWN (NULL); only FALSE rejects.
        if !self.constraints.is_empty() {
            axiomdb_sql::check_row_constraints_with_cols(
                &self.constraints,
                &coerced,
                &self.table_def.table_name,
                &self.columns,
            )?;
        }
        // v1.1 Step 5: FOREIGN KEY constraints.
        // - Immediate FKs validated against parent index using the
        //   active snapshot (sees own writes via conn_txn).
        // - Deferred FKs queued in session; commit machinery resolves
        //   them at finish() (txn.commit triggers
        //   validate_deferred_fks_on_commit in the WAL path).
        if !self.foreign_keys.is_empty() {
            let conn_txn = self.conn_txn.as_ref().expect("appender txn alive");
            let (immediate, deferred) =
                axiomdb_sql::fk_enforcement::split_child_insert_foreign_keys(
                    &coerced,
                    &self.foreign_keys,
                );
            if !immediate.is_empty() {
                axiomdb_sql::fk_enforcement::check_fk_child_insert(
                    &coerced,
                    &immediate,
                    &self.db.storage,
                    &self.db.txn,
                    conn_txn,
                    &self.db.bloom,
                )?;
            }
            for fk_id in deferred {
                self.db.session.deferred_fk_constraint_ids.push(fk_id);
            }
        }
        self.buffer.push(coerced);
        // Auto-flush keeps memory bounded — even a million-row load
        // never holds more than APPENDER_BATCH_FLUSH rows in memory at
        // once. The flush call inside append_row writes to heap+WAL but
        // keeps the txn open, so the appender is still usable after.
        if self.buffer.len() >= APPENDER_BATCH_FLUSH {
            self.flush()?;
        }
        Ok(())
    }

    /// Write all buffered rows to the heap + WAL in a single batched
    /// pass via [`axiomdb_sql::TableEngine::insert_rows_batch_with_ctx`].
    /// Keeps the transaction open so the Appender can continue to be
    /// used after the call.
    ///
    /// Atomic: if any row fails (e.g. unique-index violation in Step 5),
    /// the whole batch reverts within the txn.
    ///
    /// # Errors
    /// - I/O errors from heap insert + WAL append
    /// - Index/constraint violations from any row in the buffer
    pub fn flush(&mut self) -> Result<(), DbError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let conn_txn = self
            .conn_txn
            .as_mut()
            .expect("Appender::flush called after finish or rollback");
        // Drain the buffer — the helper takes &[Vec<Value>] so we move
        // the rows out and pass a slice. On error, we don't restore the
        // buffer (the rows are unrecoverable from the caller's POV;
        // append semantics are write-and-forget).
        let batch = std::mem::take(&mut self.buffer);
        let rids = axiomdb_sql::TableEngine::insert_rows_batch_with_ctx(
            &self.db.storage,
            &self.db.txn,
            &self.table_def,
            &self.columns,
            &mut self.db.session,
            conn_txn,
            &batch,
        )?;
        self.rows_inserted += rids.len() as u64;

        // Step 5: secondary index maintenance. v1 supports regular B-Tree,
        // BRIN, FTS, GIN, and Trigram indexes — partial-index predicates
        // and expression indexes are NOT supported (we pass empty Vecs;
        // the SQL INSERT path computes those at analyze time which the
        // Appender bypasses by design). Index roots that grow during
        // insert are written back via CatalogWriter and the in-memory
        // `self.indexes` is updated so subsequent flushes see the new
        // root.
        if !self.indexes.is_empty() {
            let n_idx = self.indexes.len();
            let empty_preds: Vec<Option<axiomdb_sql::Expr>> = vec![None; n_idx];
            let empty_idx_exprs: Vec<Vec<Option<axiomdb_sql::Expr>>> = vec![Vec::new(); n_idx];
            let snap = self.db.txn.active_snapshot(conn_txn);
            for (rid, values) in rids.iter().zip(batch.iter()) {
                let updated = axiomdb_sql::index_maintenance::insert_into_indexes_with_undo(
                    &self.indexes,
                    values,
                    *rid,
                    &self.db.storage,
                    &self.db.bloom,
                    &empty_preds,
                    &empty_idx_exprs,
                    snap.clone(),
                    Some(&self.db.txn),
                    Some(conn_txn),
                )?;
                // Persist root changes (B-Tree splits) to the catalog so
                // subsequent reads find the new root. Mirrors
                // insert_heap_ctx.rs:366-377.
                for (index_id, new_root) in updated {
                    axiomdb_catalog::CatalogWriter::new(&self.db.storage, &self.db.txn, conn_txn)?
                        .update_index_root(index_id, new_root)?;
                    if let Some(idx) = self.indexes.iter_mut().find(|i| i.index_id == index_id) {
                        idx.root_page_id = new_root;
                    }
                    // Schema cache invalidation — the catalog version
                    // bumped; future SQL operations need a fresh resolve.
                    self.db.session.invalidate_all();
                }
            }
        }
        Ok(())
    }

    /// Flush remaining rows, commit the transaction, and consume the
    /// Appender. Returns the total number of rows inserted across all
    /// flushes during the Appender's lifetime.
    ///
    /// On error, the transaction is rolled back and the error returned —
    /// the caller does NOT need to retry rollback separately.
    pub fn finish(mut self) -> Result<u64, DbError> {
        // Flush any remaining rows. On flush error, roll back the txn
        // (we still own conn_txn) before propagating.
        if let Err(e) = self.flush() {
            if let Some(conn_txn) = self.conn_txn.take() {
                let _ = self.db.txn.rollback(conn_txn, &self.db.storage);
            }
            return Err(e);
        }
        let conn_txn = self.conn_txn.take().expect("Appender::finish called twice");
        // Immediate commit (we don't drive the fsync pipeline from the
        // embedded fast path). The Attack 6 durability override stamped
        // at open time controls fsync vs flush-only via WAL.
        self.db.txn.commit(conn_txn)?;
        // Drain post-commit page-batch state (mirrors
        // commit_active_txn in exec_with_ctx.rs).
        self.db.txn.drain_committed_page_batches(&self.db.storage)?;
        Ok(self.rows_inserted)
    }
}

impl Drop for Appender<'_> {
    fn drop(&mut self) {
        if let Some(conn_txn) = self.conn_txn.take() {
            // Silent rollback. Errors here are unrecoverable from a Drop
            // context (panicking would be worse). If the caller wanted
            // commit semantics they should have called finish().
            let _ = self.db.txn.rollback(conn_txn, &self.db.storage);
        }
    }
}
