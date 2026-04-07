// ── CREATE INDEX ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexBuildResult {
    pub root_page_id: u64,
    pub skipped_key_too_long: usize,
}

pub(crate) fn build_index_root_from_heap(
    storage: &mut dyn StorageEngine,
    table_def: &TableDef,
    col_defs: &[CatalogColumnDef],
    idx: &IndexDef,
    snap: TransactionSnapshot,
) -> Result<IndexBuildResult, DbError> {
    table_def.ensure_heap_runtime("CREATE INDEX / heap rebuild on clustered table — Phase 39.13+")?;
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use std::sync::atomic::{AtomicU64, Ordering};

    let root_page_id = storage.alloc_page(PageType::Index)?;
    {
        let mut page = Page::new(PageType::Index, root_page_id);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(root_page_id, &page)?;
    }
    let root_pid = AtomicU64::new(root_page_id);
    let pred_expr = match &idx.predicate {
        Some(sql) => Some(crate::partial_index::compile_predicate_sql(sql, col_defs)?),
        None => None,
    };
    let rows = TableEngine::scan_table(storage, table_def, col_defs, snap, None)?;
    let mut skipped_key_too_long = 0usize;

    for (rid, row_vals) in &rows {
        let Some(key_vals) = crate::index_maintenance::index_key_values_if_indexed(
            idx,
            row_vals,
            pred_expr.as_ref(),
        )?
        else {
            continue;
        };

        match crate::index_maintenance::encode_index_entry_key(idx, &key_vals, *rid) {
            Ok(key) => BTree::insert_in(storage, &root_pid, &key, *rid, idx.fillfactor)?,
            Err(DbError::IndexKeyTooLong { .. }) => skipped_key_too_long += 1,
            Err(err) => return Err(err),
        }
    }

    Ok(IndexBuildResult {
        root_page_id: root_pid.load(Ordering::Acquire),
        skipped_key_too_long,
    })
}

fn execute_create_index(
    stmt: CreateIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &mut crate::bloom::BloomRegistry,
    database: &str,
) -> Result<QueryResult, DbError> {
    use crate::key_encoding::{encode_index_key, MAX_INDEX_KEY};
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use std::sync::atomic::{AtomicU64, Ordering};

    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot(conn_txn);

    // 1. Resolve table definition + column list.
    let (table_def, col_defs) = {
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        let resolved = resolver.resolve_table(Some(schema), &stmt.table.name)?;
        (resolved.def.clone(), resolved.columns.clone())
    };
    // 2. Check for a duplicate index name on this table.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let existing = reader.list_indexes(table_def.id)?;
        if existing.iter().any(|i| i.name == stmt.name) {
            return Err(DbError::IndexAlreadyExists {
                name: stmt.name.clone(),
                table: stmt.table.name.clone(),
            });
        }
    }

    // 3. Build IndexColumnDef list from the CREATE INDEX statement.
    let index_columns: Vec<IndexColumnDef> = stmt
        .columns
        .iter()
        .map(|ic| {
            let col = col_defs
                .iter()
                .find(|c| c.name == ic.name)
                .expect("analyzer guarantees index columns exist in the table");
            IndexColumnDef {
                col_idx: col.col_idx,
                order: match ic.order {
                    crate::ast::SortOrder::Asc => CatalogSortOrder::Asc,
                    crate::ast::SortOrder::Desc => CatalogSortOrder::Desc,
                },
            }
        })
        .collect();

    // 4. Allocate and initialize a fresh B-Tree leaf root page.
    let root_page_id = storage.alloc_page(PageType::Index)?;
    {
        let mut page = Page::new(PageType::Index, root_page_id);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(root_page_id, &page)?;
    }
    let root_pid = AtomicU64::new(root_page_id);

    // 5. Scan the table and insert existing rows into the B-Tree.
    //    For partial indexes, compile the predicate once and skip non-matching rows.
    let index_fillfactor = stmt.fillfactor.unwrap_or(90);
    let pred_expr: Option<crate::expr::Expr> = match &stmt.predicate {
        Some(pred) => {
            let sql = expr_to_sql_string(pred);
            Some(crate::partial_index::compile_predicate_sql(
                &sql, &col_defs,
            )?)
        }
        None => None,
    };

    let mut bloom_keys: Vec<Vec<u8>> = Vec::new();
    let mut skipped = 0usize;

    // Scan existing rows — same Vec<(RecordId, Vec<Value>)> format for both paths,
    // reused by stats bootstrap at step 8 without extra I/O.
    let rows = if table_def.is_clustered() {
        crate::table::scan_clustered_table(storage, &table_def, &col_defs, snap.clone())?
    } else {
        TableEngine::scan_table(storage, &table_def, &col_defs, snap.clone(), None)?
    };

    if table_def.is_clustered() {
        // ── Clustered secondary index build ──────────────────────────────
        // Derive the physical key layout from the primary index so that every
        // secondary entry encodes as: secondary_cols ++ suffix_primary_cols.
        // This is identical to what execute_clustered_insert uses for runtime inserts.
        let primary_idx = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(table_def.id)?
                .into_iter()
                .find(|i| i.is_primary)
                .ok_or_else(|| {
                    DbError::Other(format!(
                        "clustered table '{}' has no primary index — catalog inconsistency",
                        table_def.table_name
                    ))
                })?
        };
        // Build a preview IndexDef for ClusteredSecondaryLayout::derive.
        // Only name, is_unique, columns, fillfactor, and root_page_id are used.
        let preview_index_def = IndexDef {
            index_id: 0,
            table_id: table_def.id,
            name: stmt.name.clone(),
            root_page_id,
            is_unique: stmt.unique,
            is_primary: false,
            columns: index_columns.clone(),
            predicate: None,
            fillfactor: index_fillfactor,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let layout = crate::clustered_secondary::ClusteredSecondaryLayout::derive(
            &preview_index_def,
            &primary_idx,
        )?;

        for (_rid, row_vals) in &rows {
            // Partial index: skip rows that don't satisfy the predicate.
            if let Some(pred) = &pred_expr {
                if !crate::eval::is_truthy(&crate::eval::eval(pred, row_vals)?) {
                    continue;
                }
            }
            // Collect physical key for bloom (None → secondary column is NULL → skip).
            if let Some(entry) = layout.entry_from_row(row_vals)? {
                bloom_keys.push(entry.physical_key);
            }
            // insert_row enforces uniqueness + writes the physical key to the B-Tree.
            layout.insert_row(storage, &root_pid, row_vals)?;
        }
    } else {
        // ── Heap secondary index build (original path) ───────────────────
        for (rid, row_vals) in &rows {
            let (rid, row_vals) = (*rid, row_vals);
            // Partial index: skip rows that don't satisfy the predicate.
            if let Some(pred) = &pred_expr {
                if !crate::eval::is_truthy(&crate::eval::eval(pred, row_vals)?) {
                    continue;
                }
            }

            let key_vals: Vec<Value> = index_columns
                .iter()
                .map(|ic| row_vals[ic.col_idx as usize].clone())
                .collect();
            // Skip rows with NULL key values — NULLs are not indexed.
            if key_vals.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            match encode_index_key(&key_vals) {
                Ok(base_key) => {
                    // Non-unique indexes append the RecordId so that multiple rows with
                    // the same indexed value each get a unique B-Tree key (InnoDB approach).
                    let key = if !stmt.unique {
                        let mut k = base_key;
                        k.extend_from_slice(&encode_rid(rid));
                        k
                    } else {
                        base_key
                    };
                    BTree::insert_in(storage, &root_pid, &key, rid, index_fillfactor)?;
                    bloom_keys.push(key);
                }
                Err(DbError::IndexKeyTooLong { .. }) => {
                    skipped += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
    if skipped > 0 {
        eprintln!(
            "CREATE INDEX \"{}\": skipped {skipped} row(s) with index key > {MAX_INDEX_KEY} bytes",
            stmt.name
        );
    }

    // 6. Persist IndexDef with column list and final root_page_id (may have changed after splits).
    let final_root = root_pid.load(Ordering::Acquire);
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    // Serialize the predicate expression to SQL string for catalog storage.
    // Stored as a human-readable string for debuggability and backward-compat.
    let predicate_sql: Option<String> = stmt.predicate.as_ref().map(expr_to_sql_string);

    // Resolve INCLUDE column names to col_idx values for catalog storage (Phase 6.13).
    let include_col_idxs: Vec<u16> = stmt
        .include_columns
        .iter()
        .filter_map(|name| col_defs.iter().find(|c| &c.name == name).map(|c| c.col_idx))
        .collect();

    let new_index_id = writer.create_index(IndexDef {
        index_id: 0, // allocated by CatalogWriter::create_index
        table_id: table_def.id,
        name: stmt.name.clone(),
        root_page_id: final_root,
        is_unique: stmt.unique,
        is_primary: false,
        columns: index_columns.clone(), // clone kept for stats bootstrap step 8
        predicate: predicate_sql,
        fillfactor: stmt.fillfactor.unwrap_or(90),
        is_fk_index: false, // user-created indexes are never FK auto-indexes
        include_columns: include_col_idxs,
        index_type: match stmt.index_type {
            crate::ast::IndexType::BTree => 0,
            crate::ast::IndexType::Brin => 1,
        },
        pages_per_range: stmt.pages_per_range.unwrap_or(128),
    })?;

    // 7. Populate bloom filter for the newly created index.
    bloom.create(new_index_id, bloom_keys.len().max(1));
    for key in &bloom_keys {
        bloom.add(new_index_id, key);
    }

    // 8. Bootstrap per-column statistics (Phase 6.10).
    // Reuses the `rows` scan from step 5 — no extra I/O.
    for idx_col in &index_columns {
        let ndv = compute_ndv_exact(idx_col.col_idx, &rows);
        // Ignore stats write errors — stats are advisory, not correctness-critical.
        let _ = CatalogWriter::new(storage, txn, conn_txn)?.upsert_stats(axiomdb_catalog::StatsDef {
            table_id: table_def.id,
            col_idx: idx_col.col_idx,
            row_count: rows.len() as u64,
            ndv,
        });
    }

    // 9. Bump per-table schema_version so plan caches referencing this table
    //    detect staleness on next lookup (OID-based invalidation, Phase 40.2).
    let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_def.id);

    Ok(QueryResult::Empty)
}

// ── NDV helper (Phase 6.10) ───────────────────────────────────────────────────

/// Computes the exact number of distinct non-NULL values for `col_idx` in `rows`.
///
/// Uses order-preserving encoded key bytes as the hash key so that the result
/// is consistent with the B-Tree key encoding (encode_index_key).
/// Phase 6.15 will add reservoir sampling (Duj1 estimator) for large tables.
fn compute_ndv_exact(col_idx: u16, rows: &[(RecordId, Vec<Value>)]) -> i64 {
    use crate::key_encoding::encode_index_key;
    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (_, row) in rows {
        let val = row.get(col_idx as usize).unwrap_or(&Value::Null);
        if matches!(val, Value::Null) {
            continue; // NULLs are not indexed and don't count toward NDV
        }
        if let Ok(key) = encode_index_key(std::slice::from_ref(val)) {
            seen.insert(key);
        }
    }
    seen.len() as i64
}

