// ── CREATE INDEX ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexBuildResult {
    pub root_page_id: u64,
    pub skipped_key_too_long: usize,
}

pub(crate) fn build_index_root_from_heap(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    col_defs: &[CatalogColumnDef],
    idx: &IndexDef,
    snap: TransactionSnapshot,
) -> Result<IndexBuildResult, DbError> {
    table_def
        .ensure_heap_runtime("CREATE INDEX / heap rebuild on clustered table — Phase 39.13+")?;
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

pub(crate) fn build_index_root_from_existing_def(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    col_defs: &[CatalogColumnDef],
    idx: &IndexDef,
    snap: TransactionSnapshot,
) -> Result<IndexBuildResult, DbError> {
    if idx.index_type != 0 {
        // BRIN indexes don't need rebuild via ALTER TABLE — they track summaries.
        return Ok(IndexBuildResult {
            root_page_id: idx.root_page_id,
            skipped_key_too_long: 0,
        });
    }
    if table_def.is_clustered() {
        build_index_root_from_clustered(storage, table_def, col_defs, idx, snap)
    } else {
        build_index_root_from_heap(storage, table_def, col_defs, idx, snap)
    }
}

fn build_index_root_from_clustered(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    col_defs: &[CatalogColumnDef],
    idx: &IndexDef,
    snap: TransactionSnapshot,
) -> Result<IndexBuildResult, DbError> {
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
    let primary_idx = CatalogReader::new(storage, snap.clone())?
        .list_indexes(table_def.id)?
        .into_iter()
        .find(|i| i.is_primary)
        .ok_or_else(|| {
            DbError::Other(format!(
                "clustered table '{}' has no primary index — catalog inconsistency",
                table_def.table_name
            ))
        })?;
    let layout = {
        let base =
            crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx)?;
        // Phase 21.8: attach compiled expression-index expressions so the
        // layout builds logical keys from LOWER(col) etc. when evaluating
        // existing rows (e.g. CREATE UNIQUE INDEX after data is already loaded).
        let idx_exprs =
            crate::partial_index::compile_index_exprs(std::slice::from_ref(idx), col_defs)?
                .pop()
                .unwrap_or_default();
        base.with_exprs(idx_exprs)?
    };
    let rows = crate::table::scan_clustered_table(storage, table_def, col_defs, snap)?;
    let mut skipped_key_too_long = 0usize;

    for (_rid, row_vals) in &rows {
        if let Some(pred) = &pred_expr {
            if !crate::eval::is_truthy(&crate::eval::eval(pred, row_vals)?) {
                continue;
            }
        }
        match layout.insert_row(storage, &root_pid, row_vals) {
            Ok(_) => {}
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    database: &str,
) -> Result<QueryResult, DbError> {
    use crate::key_encoding::{encode_index_key, MAX_INDEX_KEY};
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use std::sync::atomic::{AtomicU64, Ordering};

    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot(conn_txn);

    // 1. Resolve table definition + column list.
    let (table_def, col_defs) = {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
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
    //    Also compile expression-index expressions (Phase 21.8).
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
                expr: ic
                    .expr
                    .as_ref()
                    .map(|e| crate::expr_to_sql::expr_to_sql_string(e)),
            }
        })
        .collect();

    // 4. Allocate root: B-Tree leaf page, BRIN metapage, or Trigram B-Tree.
    let is_brin = matches!(stmt.index_type, crate::ast::IndexType::Brin);
    let is_trigram = matches!(stmt.index_type, crate::ast::IndexType::Trigram);
    let is_fts = matches!(stmt.index_type, crate::ast::IndexType::FullText);
    let is_gin = matches!(stmt.index_type, crate::ast::IndexType::Gin);
    let root_page_id = if is_brin {
        let pages_per_range = stmt.pages_per_range.unwrap_or(128);
        let brin_col_idx = index_columns.first().map(|c| c.col_idx).unwrap_or(0);
        axiomdb_storage::brin::init_metapage(storage, pages_per_range, brin_col_idx)?
    } else {
        let pid = storage.alloc_page(PageType::Index)?;
        {
            let mut page = Page::new(PageType::Index, pid);
            let leaf = cast_leaf_mut(&mut page);
            leaf.is_leaf = 1;
            leaf.set_num_keys(0);
            leaf.set_next_leaf(NULL_PAGE);
            page.update_checksum();
            storage.write_page(pid, &page)?;
        }
        pid
    };
    let root_pid = AtomicU64::new(root_page_id);

    // 5. Scan the table and insert existing rows into the B-Tree.
    //    For partial indexes, compile the predicate once and skip non-matching rows.
    //    For expression indexes, compile the per-column expressions once (Phase 21.8).
    let index_fillfactor = stmt.fillfactor.unwrap_or(90);
    let pred_expr: Option<crate::expr::Expr> = match &stmt.predicate {
        Some(pred) => {
            let sql = crate::expr_to_sql::expr_to_sql_string(pred);
            Some(crate::partial_index::compile_predicate_sql(
                &sql, &col_defs,
            )?)
        }
        None => None,
    };

    // Compile expression-index expressions (parallel to index_columns).
    // Results in Vec<Option<Expr>> aligned with index_columns.
    let compiled_exprs: Vec<Option<crate::expr::Expr>> = index_columns
        .iter()
        .map(|ic| {
            ic.expr
                .as_ref()
                .map(|sql| crate::partial_index::compile_predicate_sql(sql, &col_defs))
                .transpose()
        })
        .collect::<Result<_, _>>()?;

    let mut bloom_keys: Vec<Vec<u8>> = Vec::new();
    let mut skipped = 0usize;

    // Scan existing rows — same Vec<(RecordId, Vec<Value>)> format for both paths,
    // reused by stats bootstrap at step 8 without extra I/O.
    let rows = if table_def.is_clustered() {
        crate::table::scan_clustered_table(storage, &table_def, &col_defs, snap.clone())?
    } else {
        TableEngine::scan_table(storage, &table_def, &col_defs, snap.clone(), None)?
    };

    // ── BRIN index build path (Phase 11.1b Step 4) ─────────────────────────
    if is_brin {
        let pages_per_range = stmt.pages_per_range.unwrap_or(128);
        let brin_col_idx = index_columns
            .first()
            .map(|c| c.col_idx as usize)
            .unwrap_or(0);

        // Compute per-range min/max from existing rows.
        let mut range_map: std::collections::BTreeMap<u32, axiomdb_storage::brin::BrinSummary> =
            std::collections::BTreeMap::new();
        for (rid, row_vals) in &rows {
            let range_id = (rid.page_id / pages_per_range as u64) as u32;
            let summary = range_map
                .entry(range_id)
                .or_insert_with(axiomdb_storage::brin::BrinSummary::empty);
            let val = &row_vals[brin_col_idx];
            if matches!(val, Value::Null) {
                summary.add_null();
            } else {
                // Encode value as i64 for BRIN comparison.
                let encoded = match val {
                    Value::Int(v) => *v as i64,
                    Value::BigInt(v) => *v,
                    Value::Real(v) => v.to_bits() as i64,
                    Value::Date(d) => *d as i64,
                    Value::Timestamp(t) => *t,
                    Value::Bool(b) => *b as i64,
                    _ => continue, // non-numeric columns skip
                };
                summary.add_value(encoded);
            }
        }

        // Write summaries to metapage.
        let max_range = range_map.keys().copied().max().unwrap_or(0);
        let mut summaries =
            vec![axiomdb_storage::brin::BrinSummary::empty(); max_range as usize + 1];
        for (range_id, summary) in range_map {
            summaries[range_id as usize] = summary;
        }
        axiomdb_storage::brin::write_summaries(storage, root_page_id, &summaries)?;

        // Skip B-Tree build + bloom — BRIN doesn't use either.
    } else if is_trigram {
        // ── Trigram index build (Phase 11.4b) ────────────────────────────
        // Extract 3-char n-grams from each row's text value and insert
        // (trigram || RecordId) keys into the B-Tree.
        let trgm_col_idx = index_columns
            .first()
            .map(|c| c.col_idx as usize)
            .unwrap_or(0);
        for (rid, row_vals) in &rows {
            let text = match row_vals.get(trgm_col_idx) {
                Some(Value::Text(s)) | Some(Value::Json(s)) => s,
                _ => continue,
            };
            let trigrams = crate::trigram::extract_trigrams(text);
            for trgm in &trigrams {
                let mut key = trgm.to_vec();
                key.extend_from_slice(&encode_rid(*rid));
                BTree::insert_in(storage, &root_pid, &key, *rid, index_fillfactor)?;
            }
        }
        // Skip regular bloom for trigram.
    } else if is_fts {
        // ── FTS inverted index build (Phase 11.6) ────────────────────────
        // Tokenize each row's text value, insert (term || docid || position)
        // keys into B-Tree. Also track document lengths for BM25.
        let fts_col_idx = index_columns
            .first()
            .map(|c| c.col_idx as usize)
            .unwrap_or(0);
        let mut total_tokens: u64 = 0;
        let mut doc_count: u64 = 0;
        for (rid, row_vals) in &rows {
            let text = match row_vals.get(fts_col_idx) {
                Some(Value::Text(s)) | Some(Value::Json(s)) => s,
                _ => continue,
            };
            let tokens = crate::tokenizer::tokenize(text);
            if tokens.is_empty() {
                continue;
            }
            total_tokens += tokens.len() as u64;
            doc_count += 1;
            for tok in &tokens {
                // Key: term_bytes + docid(page_id:slot_id) + position
                let mut key = tok.term.as_bytes().to_vec();
                key.push(0x00); // null separator between term and docid
                key.extend_from_slice(&rid.page_id.to_le_bytes());
                key.extend_from_slice(&rid.slot_id.to_le_bytes());
                key.extend_from_slice(&tok.position.to_le_bytes());
                BTree::insert_in(storage, &root_pid, &key, *rid, index_fillfactor)?;
            }
        }
        // TODO: store doc_count + total_tokens in index metadata for BM25 avgdl.
        let _ = (doc_count, total_tokens);
        // Skip regular bloom for FTS.
    } else if is_gin {
        // ── GIN inverted index build (Phase 11.17) ────────────────────────────
        // Extract all JSONB terms from each row and insert term postings into the
        // B-Tree. Heap tables use RID bookmarks; clustered tables use the encoded
        // primary key as the bookmark suffix. No bloom: term keys use a specialised
        // encoding.
        let gin_col_idx = index_columns
            .first()
            .map(|c| c.col_idx as usize)
            .unwrap_or(0);
        let clustered_primary_cols = if table_def.is_clustered() {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            let primary_idx = reader
                .list_indexes(table_def.id)?
                .into_iter()
                .find(|i| i.is_primary && !i.columns.is_empty())
                .ok_or_else(|| DbError::Internal {
                    message: format!(
                        "clustered table '{}' has no primary index — catalog inconsistency",
                        table_def.table_name
                    ),
                })?;
            Some(
                primary_idx
                    .columns
                    .iter()
                    .map(|col| col.col_idx)
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        for (rid, row_vals) in &rows {
            let Some(terms) =
                crate::index_maintenance::gin_terms_for_row_value(row_vals.get(gin_col_idx))?
            else {
                continue;
            };
            let clustered_pk_key = clustered_primary_cols
                .as_ref()
                .map(|primary_cols| {
                    crate::index_maintenance::encode_clustered_pk_key_from_row(
                        &stmt.name,
                        primary_cols,
                        row_vals,
                    )
                })
                .transpose()?;
            for term in &terms {
                let (key, payload_rid) = if let Some(pk_key) = clustered_pk_key.as_ref() {
                    (
                        crate::index_maintenance::gin_clustered_key(term, pk_key),
                        crate::index_maintenance::GIN_CLUSTERED_DUMMY_RID,
                    )
                } else {
                    (crate::index_maintenance::gin_heap_key(term, *rid), *rid)
                };
                BTree::insert_in(storage, &root_pid, &key, payload_rid, index_fillfactor)?;
            }
        }
        // Skip bloom for GIN — term keys are specialised; cannot be checked with
        // simple key-presence bloom used by regular B-Tree indexes.
    } else if table_def.is_clustered() {
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
        let layout = {
            let base = crate::clustered_secondary::ClusteredSecondaryLayout::derive(
                &preview_index_def,
                &primary_idx,
            )?;
            // Phase 21.8: attach the already-compiled per-column expressions
            // so logical keys use LOWER(col) etc. when indexing existing rows.
            base.with_exprs(compiled_exprs.clone())?
        };

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

            // Collect key values — for expression columns, evaluate the compiled expression;
            // for regular columns, read the raw column value (Phase 21.8).
            let key_vals: Vec<Value> = index_columns
                .iter()
                .enumerate()
                .filter_map(|(i, ic)| {
                    let compiled = compiled_exprs.get(i).and_then(|e| e.as_ref());
                    crate::index_maintenance::index_key_value_for_column(ic, compiled, row_vals)
                        .unwrap_or(None)
                })
                .collect();
            // Skip rows with NULL key values — NULLs are not indexed.
            if key_vals.len() != index_columns.len() {
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
    let predicate_sql: Option<String> = stmt
        .predicate
        .as_ref()
        .map(crate::expr_to_sql::expr_to_sql_string);

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
            crate::ast::IndexType::Trigram => 2,
            crate::ast::IndexType::FullText => 3,
            crate::ast::IndexType::Gin => 4,
        },
        pages_per_range: stmt.pages_per_range.unwrap_or(128),
    })?;

    // 7. Populate bloom filter for the newly created index (B-Tree only).
    // Skip BRIN (no B-Tree), FTS, Trigram, and GIN (specialised key encodings).
    if !is_brin && !is_fts && !is_trigram && !is_gin {
        bloom.create(new_index_id, bloom_keys.len().max(1));
        for key in &bloom_keys {
            bloom.add(new_index_id, key);
        }
    }

    // 8. Bootstrap per-column statistics (Phase 6.10).
    // Reuses the `rows` scan from step 5 — no extra I/O.
    for idx_col in &index_columns {
        let ndv = compute_ndv_exact(idx_col.col_idx, &rows);
        // Ignore stats write errors — stats are advisory, not correctness-critical.
        let _ =
            CatalogWriter::new(storage, txn, conn_txn)?.upsert_stats(axiomdb_catalog::StatsDef {
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
