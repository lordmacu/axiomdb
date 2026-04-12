// ── Bulk table-empty machinery (Phase 5.16) ──────────────────────────────────

/// Everything needed to swap a table (and all its indexes) to empty roots.
fn execute_analyze(
    stmt: crate::ast::AnalyzeStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // SAFETY: see ExecutionContext::storage_mut / coord_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let schema = "public";
    let database = ctx.effective_database().to_string();
    let snap = txn.active_snapshot(ctx.conn_txn.as_ref().expect("conn_txn for analyze"));

    // Collect target tables.
    let target_tables: Vec<String> = if let Some(table_name) = stmt.table {
        vec![table_name]
    } else {
        // ANALYZE without TABLE — all tables in schema.
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader
            .list_tables_in_database(&database, schema)?
            .into_iter()
            .map(|t| t.table_name)
            .collect()
    };

    for table_name in target_tables {
        let resolved = {
            let mut resolver = make_resolver_with_database(
                storage,
                txn,
                ctx.conn_txn.as_ref(),
                &database,
            )?;
            match resolver.resolve_table(Some(schema), &table_name) {
                Ok(r) => r,
                Err(_) => continue, // table may not exist — skip
            }
        };
        // Scan the full table once (clustered or heap).
        let rows = if resolved.def.is_clustered() {
            crate::table::scan_clustered_table(storage, &resolved.def, &resolved.columns, snap.clone())?
        } else {
            TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap.clone(), None)?
        };
        let row_count = rows.len() as u64;

        // Determine target columns: all indexed columns OR a specific one.
        let target_col_idxs: Vec<u16> = if let Some(col_name) = &stmt.column {
            resolved
                .columns
                .iter()
                .filter(|c| &c.name == col_name)
                .map(|c| c.col_idx)
                .collect()
        } else {
            // All columns that appear as leading columns of any index.
            let mut seen = std::collections::HashSet::new();
            resolved
                .indexes
                .iter()
                .filter_map(|i| i.columns.first().map(|c| c.col_idx))
                .filter(|col_idx| seen.insert(*col_idx))
                .collect()
        };

        for col_idx in target_col_idxs {
            let ndv = compute_ndv_exact(col_idx, &rows);
            // Ignore write errors — stats are advisory.
            let conn = ctx.conn_txn.as_mut().expect("conn_txn for analyze stats");
            let _ = CatalogWriter::new(storage, txn, conn)?.upsert_stats(axiomdb_catalog::StatsDef {
                table_id: resolved.def.id,
                col_idx,
                row_count,
                ndv,
            });
        }

        // Clear staleness so the planner uses fresh stats immediately.
        ctx.stats.mark_fresh(resolved.def.id);
        // Invalidate schema cache so next query gets fresh resolved table.
        ctx.invalidate_table(&database, schema, &table_name);
    }

    Ok(QueryResult::Empty)
}

// ── TRUNCATE TABLE (4.21) ─────────────────────────────────────────────────────

fn execute_truncate(
    stmt: crate::ast::TruncateTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    // Phase 13.9: IMMUTABLE tables reject bulk removal via TRUNCATE as well.
    if resolved.def.immutable {
        return Err(DbError::ImmutableTable {
            table: resolved.def.table_name.clone(),
            operation: "TRUNCATE".into(),
        });
    }

    let snap = txn.active_snapshot(conn_txn);

    // TRUNCATE TABLE must fail if child FKs reference this table as the parent.
    // AxiomDB does not implement TRUNCATE ... CASCADE; the caller must DELETE
    // or TRUNCATE child tables first (same as PostgreSQL's behavior).
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let parent_fks = reader.list_fk_constraints_referencing(resolved.def.id)?;
        if !parent_fks.is_empty() {
            let fk = &parent_fks[0];
            return Err(DbError::ForeignKeyParentViolation {
                constraint: fk.name.clone(),
                child_table: format!("table_id={}", fk.child_table_id),
                child_column: format!("col_idx={}", fk.child_col_idx),
            });
        }
    }

    // Collect all indexes with columns for root rotation.
    let all_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // Bulk-empty via root rotation (Phase 5.16): correct for indexed tables.
    let noop_bloom = crate::bloom::BloomRegistry::new();
    let plan = if resolved.def.is_clustered() {
        plan_bulk_empty_clustered_table(storage, &resolved.def, &all_indexes, snap.clone())?
    } else {
        plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?
    };
    apply_bulk_empty_table(storage, txn, conn_txn, &noop_bloom, &resolved.def, plan)?;

    // Reset the AUTO_INCREMENT sequence so the next insert starts from 1.
    AUTO_INC_SEQ.with(|seq| {
        seq.borrow_mut().remove(&resolved.def.id);
    });

    // MySQL convention: TRUNCATE returns count = 0, not the actual deleted count.
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

