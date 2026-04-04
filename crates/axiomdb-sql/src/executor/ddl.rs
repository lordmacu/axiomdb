fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let primary_key = collect_create_table_primary_key(&stmt)?;
    let unique_indexes = collect_create_table_unique_indexes(&stmt)?;
    let primary_key_cols: std::collections::HashSet<u16> = primary_key
        .as_ref()
        .map(|pk| pk.columns.iter().map(|c| c.col_idx).collect())
        .unwrap_or_default();
    let storage_layout = if primary_key.is_some() {
        axiomdb_catalog::schema::TableStorageLayout::Clustered
    } else {
        axiomdb_catalog::schema::TableStorageLayout::Heap
    };

    // Check existence before constructing CatalogWriter (avoids double mutable borrow).
    {
        let mut resolver = make_resolver_with_database(storage, txn, database)?;
        if resolver.table_exists(Some(schema), &stmt.table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: schema.to_string(),
                name: stmt.table.name.clone(),
            });
        }
    } // resolver dropped here — releases immutable borrow on storage

    let mut writer = CatalogWriter::new(storage, txn)?;
    let table_def = writer.create_table_with_layout(schema, &stmt.table.name, storage_layout)?;
    let table_id = table_def.id;
    if database != DEFAULT_DATABASE_NAME {
        writer.bind_table_to_database(table_id, database)?;
    }

    // Collect inline REFERENCES constraints for processing after all columns are created.
    // We must create all columns first so col_idx values are stable.
    let mut inline_fk_specs: Vec<InlineFkSpec> = Vec::new();

    for (i, col_def) in stmt.columns.iter().enumerate() {
        let col_type = datatype_to_column_type(&col_def.data_type)?;
        let nullable = !col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull))
            && !primary_key_cols.contains(&(i as u16));
        let auto_increment = col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::AutoIncrement));
        if let Some(refs) = col_def.constraints.iter().find_map(|c| {
            if let ColumnConstraint::References {
                table,
                column,
                on_delete,
                on_update,
            } = c
            {
                Some((table.clone(), column.clone(), *on_delete, *on_update))
            } else {
                None
            }
        }) {
            inline_fk_specs.push((i as u16, col_def.name.clone(), refs));
        }

        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
            auto_increment,
        })?;
    }

    {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};

        let create_empty_index = |index_name: String,
                                  columns: Vec<IndexColumnDef>,
                                  is_unique: bool,
                                  is_primary: bool,
                                  root_override: Option<u64>,
                                  storage: &mut dyn StorageEngine,
                                  txn: &mut TxnManager|
         -> Result<u32, DbError> {
            let root_page_id = match root_override {
                Some(root_page_id) => root_page_id,
                None => {
                    let root_page_id = storage.alloc_page(PageType::Index)?;
                    let mut page = Page::new(PageType::Index, root_page_id);
                    let leaf = cast_leaf_mut(&mut page);
                    leaf.is_leaf = 1;
                    leaf.set_num_keys(0);
                    leaf.set_next_leaf(NULL_PAGE);
                    page.update_checksum();
                    storage.write_page(root_page_id, &page)?;
                    root_page_id
                }
            };

            CatalogWriter::new(storage, txn)?.create_index(IndexDef {
                index_id: 0,
                table_id,
                name: index_name,
                root_page_id,
                is_unique,
                fillfactor: 90,
                is_primary,
                columns,
                predicate: None,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
        };

        if let Some(pk_spec) = primary_key {
            let idx_id = create_empty_index(
                pk_spec.name,
                pk_spec.columns,
                true,
                true,
                Some(table_def.root_page_id),
                storage,
                txn,
            )?;
            let _ = idx_id;
        }

        for unique_spec in unique_indexes {
            let idx_id = create_empty_index(
                unique_spec.name,
                unique_spec.columns,
                true,
                false,
                None,
                storage,
                txn,
            )?;
            let _ = idx_id;
        }
    }

    for (child_col_idx, child_col_name, (ref_table, ref_col, on_delete, on_update)) in
        inline_fk_specs
    {
        persist_fk_constraint(
            table_id,
            &stmt.table.name,
            database,
            child_col_idx,
            &child_col_name,
            &ref_table,
            ref_col.as_deref(),
            ast_fk_action_to_catalog(on_delete),
            ast_fk_action_to_catalog(on_update),
            None,
            storage,
            txn,
        )?;
    }

    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } = tc
        {
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            let snap = txn.active_snapshot()?;
            let child_col_idx = {
                let mut reader = CatalogReader::new(storage, snap)?;
                let cols = reader.list_columns(table_id)?;
                cols.iter()
                    .find(|c| &c.name == child_col_name)
                    .map(|c| c.col_idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: child_col_name.clone(),
                        table: stmt.table.name.clone(),
                    })?
            };
            let ref_col = ref_columns.first().map(|s| s.as_str());
            persist_fk_constraint(
                table_id,
                &stmt.table.name,
                database,
                child_col_idx,
                child_col_name,
                ref_table,
                ref_col,
                ast_fk_action_to_catalog(*on_delete),
                ast_fk_action_to_catalog(*on_update),
                name.as_deref(),
                storage,
                txn,
            )?;
        }
    }

    Ok(QueryResult::Empty)
}

#[derive(Debug, Clone)]
struct CreateTableIndexSpec {
    name: String,
    columns: Vec<IndexColumnDef>,
}

fn resolve_create_table_index_columns(
    stmt: &CreateTableStmt,
    columns: &[String],
) -> Result<Vec<IndexColumnDef>, DbError> {
    if columns.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "PRIMARY KEY / UNIQUE requires at least one column".into(),
        });
    }

    columns
        .iter()
        .map(|col_name| {
            let (col_idx, _) = stmt
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == *col_name)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: stmt.table.name.clone(),
                })?;
            Ok(IndexColumnDef {
                col_idx: col_idx as u16,
                order: CatalogSortOrder::Asc,
            })
        })
        .collect()
}

fn collect_create_table_primary_key(
    stmt: &CreateTableStmt,
) -> Result<Option<CreateTableIndexSpec>, DbError> {
    let inline_pk_cols: Vec<(u16, String)> = stmt
        .columns
        .iter()
        .enumerate()
        .filter(|(_, col_def)| {
            col_def
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::PrimaryKey))
        })
        .map(|(idx, col_def)| (idx as u16, col_def.name.clone()))
        .collect();

    let mut table_pk = None;
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::PrimaryKey { name, columns } = tc {
            if table_pk.is_some() || !inline_pk_cols.is_empty() {
                return Err(DbError::InvalidValue {
                    reason: "multiple PRIMARY KEY constraints are not allowed".into(),
                });
            }
            table_pk = Some(CreateTableIndexSpec {
                name: name
                    .clone()
                    .unwrap_or_else(|| format!("{}_pkey", stmt.table.name)),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }

    if !inline_pk_cols.is_empty() {
        if inline_pk_cols.len() > 1 {
            return Err(DbError::InvalidValue {
                reason: "multiple inline PRIMARY KEY columns are not allowed; use PRIMARY KEY (...)"
                    .into(),
            });
        }
        return Ok(Some(CreateTableIndexSpec {
            name: format!("{}_pkey", stmt.table.name),
            columns: vec![IndexColumnDef {
                col_idx: inline_pk_cols[0].0,
                order: CatalogSortOrder::Asc,
            }],
        }));
    }

    Ok(table_pk)
}

fn collect_create_table_unique_indexes(
    stmt: &CreateTableStmt,
) -> Result<Vec<CreateTableIndexSpec>, DbError> {
    let mut unique_indexes = Vec::new();

    for (idx, col_def) in stmt.columns.iter().enumerate() {
        if col_def
            .constraints
            .iter()
            .any(|c| matches!(c, crate::ast::ColumnConstraint::Unique))
        {
            unique_indexes.push(CreateTableIndexSpec {
                name: format!("{}_{}_unique", stmt.table.name, col_def.name),
                columns: vec![IndexColumnDef {
                    col_idx: idx as u16,
                    order: CatalogSortOrder::Asc,
                }],
            });
        }
    }

    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Unique { name, columns } = tc {
            let generated_name = if columns.len() == 1 {
                format!("{}_{}_unique", stmt.table.name, columns[0])
            } else {
                format!("{}_{}_unique", stmt.table.name, columns.join("_"))
            };
            unique_indexes.push(CreateTableIndexSpec {
                name: name.clone().unwrap_or(generated_name),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }

    Ok(unique_indexes)
}

// ── FK helpers ────────────────────────────────────────────────────────────────

/// Converts an AST [`ForeignKeyAction`] to the catalog [`FkAction`] used in `FkDef`.
fn ast_fk_action_to_catalog(action: crate::ast::ForeignKeyAction) -> axiomdb_catalog::FkAction {
    use crate::ast::ForeignKeyAction;
    use axiomdb_catalog::FkAction;
    match action {
        ForeignKeyAction::NoAction => FkAction::NoAction,
        ForeignKeyAction::Restrict => FkAction::Restrict,
        ForeignKeyAction::Cascade => FkAction::Cascade,
        ForeignKeyAction::SetNull => FkAction::SetNull,
        ForeignKeyAction::SetDefault => FkAction::SetDefault,
    }
}

/// Validates and persists a single FK constraint definition.
///
/// Called from `execute_create_table` (inline `REFERENCES` and table-level
/// `FOREIGN KEY`) and from `alter_add_constraint`.
///
/// # Steps
/// 1. Resolve parent table and referenced column (defaults to PK if unspecified).
/// 2. Verify parent column has a PRIMARY KEY or UNIQUE index.
/// 3. Auto-generate constraint name if not provided.
/// 4. Check uniqueness of constraint name on this child table.
/// 5. Create an index on the FK column in the child table if none exists.
/// 6. Persist `FkDef` in `axiom_foreign_keys`.
#[allow(clippy::too_many_arguments)]
fn persist_fk_constraint(
    child_table_id: u32,
    child_table_name: &str,
    database: &str,
    child_col_idx: u16,
    child_col_name: &str,
    ref_table: &str,
    ref_col: Option<&str>,
    on_delete: axiomdb_catalog::FkAction,
    on_update: axiomdb_catalog::FkAction,
    fk_name: Option<&str>,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot()?;

    // 1. Resolve parent table.
    let parent_def = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader
            .get_table_in_database(database, "public", ref_table)?
            .ok_or_else(|| DbError::TableNotFound {
                name: ref_table.to_string(),
            })?
    };

    // 2. Find the referenced column in the parent table.
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.list_columns(parent_def.id)?
    };
    let parent_col_idx: u16 = if let Some(col_name) = ref_col {
        parent_cols
            .iter()
            .find(|c| c.name == col_name)
            .map(|c| c.col_idx)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: col_name.to_string(),
                table: ref_table.to_string(),
            })?
    } else {
        // Default: use the leading column of the primary key index.
        let parent_indexes = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_indexes(parent_def.id)?
        };
        let pk_idx = parent_indexes
            .iter()
            .find(|i| i.is_primary && !i.columns.is_empty())
            .ok_or_else(|| DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: "<primary key>".to_string(),
            })?;
        pk_idx.columns[0].col_idx
    };

    // 3. Verify the parent column has a PRIMARY KEY or UNIQUE index covering it.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        let parent_indexes = reader.list_indexes(parent_def.id)?;
        let has_unique = parent_indexes.iter().any(|i| {
            (i.is_primary || i.is_unique)
                && i.columns.len() == 1
                && i.columns[0].col_idx == parent_col_idx
        });
        if !has_unique {
            let col_name = parent_cols
                .iter()
                .find(|c| c.col_idx == parent_col_idx)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col_{parent_col_idx}"));
            return Err(DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: col_name,
            });
        }
    }

    // 4. Auto-generate FK name if not provided.
    let constraint_name: String = fk_name
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("fk_{child_table_name}_{child_col_name}_{ref_table}"));

    // 5. Check FK name uniqueness on this child table.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        if reader
            .get_fk_by_name(child_table_id, &constraint_name)?
            .is_some()
        {
            return Err(DbError::Other(format!(
                "foreign key constraint '{constraint_name}' already exists on table \
                 '{child_table_name}'"
            )));
        }
    }

    // 6. FK auto-index on child table (Phase 6.9).
    use axiomdb_catalog::{IndexColumnDef as CatIndexColumnDef, SortOrder as CatSortOrder};
    //
    // Uses composite keys: encode_index_key(&[fk_val]) ++ encode_rid(rid) (10 bytes).
    // Every entry is globally unique even when multiple rows share the same FK value —
    // the InnoDB approach (appending PK as tiebreaker). This enables O(log n)
    // range scans for RESTRICT/CASCADE/SET NULL enforcement.
    let fk_index_id: u32 = {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
        use std::sync::atomic::{AtomicU64, Ordering};

        // Read child table def once to check if it is clustered.
        let child_table_def_for_fk = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader
                .get_table_by_id(child_table_id)?
                .ok_or(DbError::CatalogTableNotFound {
                    table_id: child_table_id,
                })?
        };

        if child_table_def_for_fk.is_clustered() {
            // Clustered child table: FK auto-index (composite heap RID key) is
            // incompatible with the clustered layout. Enforcement always falls back
            // to a full scan via scan_clustered_table (fk_index_id = 0 path).
            0
        } else {
            // Check if child already has a suitable covering index on child_col_idx
            // (user-provided, not an FK auto-index).
            let existing_covers = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader.list_indexes(child_table_id)?.into_iter().any(|i| {
                    !i.is_fk_index && !i.columns.is_empty() && i.columns[0].col_idx == child_col_idx
                })
            };

            if existing_covers {
                0 // reuse existing user-provided index; will not be dropped with FK
            } else {
                // Build FK auto-index with composite keys from existing child rows.
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

                let child_cols = {
                    let mut reader = CatalogReader::new(storage, snap)?;
                    reader.list_columns(child_table_id)?
                };

                // Insert composite key entry for every existing child row.
                let rows = TableEngine::scan_table(storage, &child_table_def_for_fk, &child_cols, snap, None)?;
                for (rid, row_vals) in rows {
                    let fk_val = row_vals.get(child_col_idx as usize).unwrap_or(&Value::Null);
                    if matches!(fk_val, Value::Null) {
                        continue;
                    }
                    if let Ok(key) = crate::index_maintenance::fk_composite_key(fk_val, rid) {
                        BTree::insert_in(storage, &root_pid, &key, rid, 90)?;
                    }
                }

                let final_root = root_pid.load(Ordering::Acquire);
                let new_idx_id = CatalogWriter::new(storage, txn)?.create_index(IndexDef {
                    index_id: 0,
                    table_id: child_table_id,
                    name: format!("_fk_{constraint_name}"),
                    root_page_id: final_root,
                    is_unique: false,
                    is_primary: false,
                    is_fk_index: true, // marks composite-key FK auto-index
                    columns: vec![CatIndexColumnDef {
                        col_idx: child_col_idx,
                        order: CatSortOrder::Asc,
                    }],
                    predicate: None,
                    fillfactor: 90,
                    include_columns: vec![],
                    index_type: 0,
                    pages_per_range: 128,
                })?;
                new_idx_id
            }
        }
    };

    // 7. Persist FkDef in axiom_foreign_keys.
    CatalogWriter::new(storage, txn)?.create_foreign_key(FkDef {
        fk_id: 0, // allocated by CatalogWriter::create_foreign_key
        child_table_id,
        child_col_idx,
        parent_table_id: parent_def.id,
        parent_col_idx,
        on_delete,
        on_update,
        fk_index_id,
        name: constraint_name,
    })?;

    Ok(())
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn execute_drop_table(
    stmt: DropTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    for table_ref in stmt.tables {
        let schema = table_ref.schema.as_deref().unwrap_or("public");
        let snap = txn.active_snapshot()?;

        let table_id = {
            let mut reader = CatalogReader::new(storage, snap)?;
            match reader.get_table_in_database(database, schema, &table_ref.name)? {
                Some(def) => def.id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        }; // reader dropped — immutable borrow released

        // Bump schema_version before dropping so cross-connection plan caches
        // that hold a dep on this table_id see a version mismatch on next lookup
        // (belt-and-suspenders: `is_stale()` also detects the table being gone).
        // Ignore errors — bump is advisory; the drop itself is authoritative.
        let _ = CatalogWriter::new(storage, txn)?.bump_table_schema_version(table_id);

        drop_table_fully(storage, txn, table_id)?;
    }

    Ok(QueryResult::Empty)
}

fn drop_table_fully(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_id: u32,
) -> Result<(), DbError> {
    use std::collections::HashSet;

    let snap = txn.active_snapshot()?;
    let (constraints, child_fks, parent_fks) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        (
            reader.list_constraints(table_id)?,
            reader.list_fk_constraints(table_id)?,
            reader.list_fk_constraints_referencing(table_id)?,
        )
    };

    let mut seen_fk_ids = HashSet::new();
    let mut noop_bloom = crate::bloom::BloomRegistry::new();
    for fk in child_fks.into_iter().chain(parent_fks.into_iter()) {
        if !seen_fk_ids.insert(fk.fk_id) {
            continue;
        }
        CatalogWriter::new(storage, txn)?.drop_foreign_key(fk.fk_id)?;
        if fk.fk_index_id != 0 {
            let _ = execute_drop_index_by_id(fk.fk_index_id, storage, txn, &mut noop_bloom);
        }
    }

    for constraint in constraints {
        CatalogWriter::new(storage, txn)?.drop_constraint(constraint.constraint_id)?;
    }
    CatalogWriter::new(storage, txn)?.delete_stats_for_table(table_id)?;
    CatalogWriter::new(storage, txn)?.delete_table(table_id)?;
    Ok(())
}

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
    bloom: &mut crate::bloom::BloomRegistry,
    database: &str,
) -> Result<QueryResult, DbError> {
    use crate::key_encoding::{encode_index_key, MAX_INDEX_KEY};
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use std::sync::atomic::{AtomicU64, Ordering};

    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;

    // 1. Resolve table definition + column list.
    let (table_def, col_defs) = {
        let mut resolver = make_resolver_with_database(storage, txn, database)?;
        let resolved = resolver.resolve_table(Some(schema), &stmt.table.name)?;
        (resolved.def.clone(), resolved.columns.clone())
    };
    // 2. Check for a duplicate index name on this table.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
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
        crate::table::scan_clustered_table(storage, &table_def, &col_defs, snap)?
    } else {
        TableEngine::scan_table(storage, &table_def, &col_defs, snap, None)?
    };

    if table_def.is_clustered() {
        // ── Clustered secondary index build ──────────────────────────────
        // Derive the physical key layout from the primary index so that every
        // secondary entry encodes as: secondary_cols ++ suffix_primary_cols.
        // This is identical to what execute_clustered_insert uses for runtime inserts.
        let primary_idx = {
            let mut reader = CatalogReader::new(storage, snap)?;
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
    let mut writer = CatalogWriter::new(storage, txn)?;
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
        let _ = CatalogWriter::new(storage, txn)?.upsert_stats(axiomdb_catalog::StatsDef {
            table_id: table_def.id,
            col_idx: idx_col.col_idx,
            row_count: rows.len() as u64,
            ndv,
        });
    }

    // 9. Bump per-table schema_version so plan caches referencing this table
    //    detect staleness on next lookup (OID-based invalidation, Phase 40.2).
    let _ = CatalogWriter::new(storage, txn)?.bump_table_schema_version(table_def.id);

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

// ── DROP INDEX ────────────────────────────────────────────────────────────────

fn execute_drop_index(
    stmt: DropIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;

    // MySQL requires `DROP INDEX name ON table`. If no table is provided, we cannot
    // efficiently search all indexes for Phase 4.5.
    let table_ref = stmt.table.as_ref().ok_or_else(|| DbError::NotImplemented {
        feature: "DROP INDEX without ON table — Phase 4.20".into(),
    })?;

    let schema = table_ref.schema.as_deref().unwrap_or("public");

    // Capture index_id, root_page_id, clustered flag, and table_id for bump.
    let (index_id, root_page_id, clustered_primary, table_id_for_bump) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        let table_def = match reader.get_table_in_database(database, schema, &table_ref.name)? {
            Some(d) => d,
            None if stmt.if_exists => return Ok(QueryResult::Empty),
            None => {
                return Err(DbError::TableNotFound {
                    name: table_ref.name.clone(),
                })
            }
        };
        let indexes = reader.list_indexes(table_def.id)?;
        match indexes.into_iter().find(|i| i.name == stmt.name) {
            Some(i) => (
                Some(i.index_id),
                Some(i.root_page_id),
                table_def.is_clustered() && i.is_primary,
                table_def.id,
            ),
            None => (None, None, false, table_def.id),
        }
    }; // reader dropped

    if clustered_primary {
        return Err(DbError::NotImplemented {
            feature: "DROP PRIMARY KEY on clustered table — Phase 39.19".into(),
        });
    }

    match index_id {
        None if stmt.if_exists => Ok(QueryResult::Empty),
        None => Err(DbError::NotImplemented {
            feature: format!("DROP INDEX — index '{}' not found", stmt.name),
        }),
        Some(id) => {
            // Delete catalog entry first.
            CatalogWriter::new(storage, txn)?.delete_index(id)?;
            bloom.remove(id);
            // Then free all B-Tree pages to avoid leaks.
            if let Some(root) = root_page_id {
                free_btree_pages(storage, root)?;
            }
            // Bump per-table schema_version for OID-based plan cache invalidation
            // (Phase 40.2). Cached plans that chose this index must be recompiled.
            let _ = CatalogWriter::new(storage, txn)?.bump_table_schema_version(table_id_for_bump);
            Ok(QueryResult::Empty)
        }
    }
}

/// Drops an index by its catalog `index_id`, without requiring the index name.
///
/// Used by `alter_drop_constraint` to remove the auto-created FK index when a
/// FK constraint is dropped.
fn execute_drop_index_by_id(
    index_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    // Find the root page ID so we can free the B-Tree pages.
    let root_page_id = {
        // Scan all indexes looking for this index_id.
        // We scan axiom_indexes; CatalogReader::list_indexes requires a table_id,
        // so we use a raw catalog reader to get the TableDef first, but since we
        // only have index_id, we scan all tables. For Phase 6.5 this is acceptable
        // (index count is small). A direct get_index_by_id is deferred.
        // Scan axiom_indexes heap directly to find root by index_id (no table filter needed).
        let page_ids = axiomdb_catalog::bootstrap::CatalogBootstrap::page_ids(storage)?;
        let rows = axiomdb_storage::heap_chain::HeapChain::scan_visible_ro(
            storage,
            page_ids.indexes,
            snap,
        )?;
        let mut found_root = None;
        for (_, _, data) in rows {
            if let Ok((def, _)) = axiomdb_catalog::schema::IndexDef::from_bytes(&data) {
                if def.index_id == index_id {
                    found_root = Some(def.root_page_id);
                    break;
                }
            }
        }
        found_root
    };

    CatalogWriter::new(storage, txn)?.delete_index(index_id)?;
    bloom.remove(index_id);
    if let Some(root) = root_page_id {
        free_btree_pages(storage, root)?;
    }
    Ok(())
}

// ── Bulk table-empty machinery (Phase 5.16) ──────────────────────────────────

/// Everything needed to swap a table (and all its indexes) to empty roots.
fn execute_analyze(
    stmt: crate::ast::AnalyzeStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let schema = "public";
    let database = ctx.effective_database().to_string();
    let snap = txn.active_snapshot()?;

    // Collect target tables.
    let target_tables: Vec<String> = if let Some(table_name) = stmt.table {
        vec![table_name]
    } else {
        // ANALYZE without TABLE — all tables in schema.
        let mut reader = CatalogReader::new(storage, snap)?;
        reader
            .list_tables_in_database(&database, schema)?
            .into_iter()
            .map(|t| t.table_name)
            .collect()
    };

    for table_name in target_tables {
        let resolved = {
            let mut resolver = make_resolver_with_database(storage, txn, &database)?;
            match resolver.resolve_table(Some(schema), &table_name) {
                Ok(r) => r,
                Err(_) => continue, // table may not exist — skip
            }
        };
        // Scan the full table once (clustered or heap).
        let rows = if resolved.def.is_clustered() {
            crate::table::scan_clustered_table(storage, &resolved.def, &resolved.columns, snap)?
        } else {
            TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
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
            let _ = CatalogWriter::new(storage, txn)?.upsert_stats(axiomdb_catalog::StatsDef {
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver_with_database(storage, txn, database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let snap = txn.active_snapshot()?;

    // TRUNCATE TABLE must fail if child FKs reference this table as the parent.
    // AxiomDB does not implement TRUNCATE ... CASCADE; the caller must DELETE
    // or TRUNCATE child tables first (same as PostgreSQL's behavior).
    {
        let mut reader = CatalogReader::new(storage, snap)?;
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
    let mut noop_bloom = crate::bloom::BloomRegistry::new();
    let plan = if resolved.def.is_clustered() {
        plan_bulk_empty_clustered_table(storage, &resolved.def, &all_indexes, snap)?
    } else {
        plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?
    };
    apply_bulk_empty_table(storage, txn, &mut noop_bloom, &resolved.def, plan)?;

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

// ── SHOW TABLES / SHOW COLUMNS / DESCRIBE (4.20) ─────────────────────────────

fn execute_show_databases(
    _stmt: ShowDatabasesStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    let databases = reader.list_databases()?;
    let rows: Vec<Row> = databases
        .into_iter()
        .map(|db| vec![Value::Text(db.name)])
        .collect();
    Ok(QueryResult::Rows {
        columns: vec![ColumnMeta::computed("Database", DataType::Text)],
        rows,
    })
}

fn execute_use_database(
    stmt: UseDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    if !reader.database_exists(&stmt.name)? {
        return Err(DbError::DatabaseNotFound { name: stmt.name });
    }
    ctx.set_current_database(stmt.name);
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_database(
    stmt: CreateDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.database_exists(&stmt.name)? {
        return Err(DbError::DatabaseAlreadyExists { name: stmt.name });
    }
    let mut writer = CatalogWriter::new(storage, txn)?;
    writer.create_database(&stmt.name)?;
    // Every new database gets a `public` schema.
    writer.create_schema(&stmt.name, "public")?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_schema(
    stmt: crate::ast::CreateSchemaStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.schema_exists(database, &stmt.name)? {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::SchemaAlreadyExists {
            name: stmt.name,
        });
    }
    CatalogWriter::new(storage, txn)?.create_schema(database, &stmt.name)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_drop_database(
    stmt: DropDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if ctx.selected_database() == Some(stmt.name.as_str()) {
        return Err(DbError::ActiveDatabaseDrop { name: stmt.name });
    }

    let snap = txn.active_snapshot()?;
    let tables = {
        let mut reader = CatalogReader::new(storage, snap)?;
        if !reader.database_exists(&stmt.name)? {
            if stmt.if_exists {
                return Ok(QueryResult::Affected {
                    count: 0,
                    last_insert_id: None,
                });
            }
            return Err(DbError::DatabaseNotFound { name: stmt.name });
        }
        reader.list_tables_owned_by_database(&stmt.name)?
    };

    for table in tables {
        drop_table_fully(storage, txn, table.id)?;
    }
    CatalogWriter::new(storage, txn)?.drop_table_database_bindings_for_database(&stmt.name)?;
    let _ = CatalogWriter::new(storage, txn)?.drop_database(&stmt.name)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_show_tables(
    stmt: crate::ast::ShowTablesStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    let tables = reader.list_tables_in_database(database, schema)?;

    let col_name = format!("Tables_in_{schema}");
    let out_cols = vec![ColumnMeta::computed(col_name, DataType::Text)];
    let rows: Vec<Row> = tables
        .into_iter()
        .map(|t| vec![Value::Text(t.table_name)])
        .collect();

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

fn execute_show_columns(
    stmt: crate::ast::ShowColumnsStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;

    let table_def =
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?;
    let columns = reader.list_columns(table_def.id)?;

    let out_cols = vec![
        ColumnMeta::computed("Field", DataType::Text),
        ColumnMeta::computed("Type", DataType::Text),
        ColumnMeta::computed("Null", DataType::Text),
        ColumnMeta::computed("Key", DataType::Text),
        ColumnMeta::computed("Default", DataType::Text),
        ColumnMeta::computed("Extra", DataType::Text),
    ];

    let rows: Vec<Row> = columns
        .iter()
        .map(|c| {
            let type_str = column_type_to_sql_name(c.col_type);
            let null_str = if c.nullable { "YES" } else { "NO" };
            let extra = if c.auto_increment {
                "auto_increment"
            } else {
                ""
            };
            vec![
                Value::Text(c.name.clone()),
                Value::Text(type_str.into()),
                Value::Text(null_str.into()),
                Value::Text("".into()), // Key — deferred
                Value::Null,            // Default — deferred
                Value::Text(extra.into()),
            ]
        })
        .collect();

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Returns the SQL type name string for display in SHOW COLUMNS / DESCRIBE.
fn column_type_to_sql_name(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "BOOL",
        ColumnType::Int => "INT",
        ColumnType::BigInt => "BIGINT",
        ColumnType::Float => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Bytes => "BYTES",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Uuid => "UUID",
    }
}

// ── ALTER TABLE (4.22) ────────────────────────────────────────────────────────

/// Rewrites all rows in `table_def` by applying `transform` to each row.
///
/// The row is decoded using `old_columns`, transformed, then encoded and
/// reinserted using `new_columns`. Used by ADD COLUMN and DROP COLUMN.
///
/// **Ordering for ADD COLUMN**: call this AFTER updating the catalog so that
/// the new rows match the new schema.
/// **Ordering for DROP COLUMN**: call this BEFORE updating the catalog so that
/// if the rewrite fails the catalog is still consistent with the existing rows.
fn rewrite_rows(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Row,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, table_def, old_columns, snap, None)?;
    for (rid, old_values) in rows {
        let new_values = transform(old_values);
        TableEngine::delete_row(storage, txn, table_def, rid)?;
        TableEngine::insert_row(storage, txn, table_def, new_columns, new_values)?;
    }
    Ok(())
}

fn execute_alter_table(
    stmt: AlterTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Resolve the table once upfront.
    let table_def = {
        let mut resolver = make_resolver_with_database(storage, txn, database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    // Keep the current column list; update it as we apply operations.
    let mut columns = table_def.columns.clone();

    for op in stmt.operations {
        match op {
            AlterTableOp::AddColumn(col_def) => {
                alter_add_column(storage, txn, &table_def.def, &mut columns, col_def, schema)?;
            }
            AlterTableOp::DropColumn { name, if_exists } => {
                alter_drop_column(storage, txn, &table_def.def, &mut columns, &name, if_exists)?;
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                alter_rename_column(
                    storage,
                    txn,
                    &table_def.def,
                    &columns,
                    &old_name,
                    &new_name,
                    schema,
                )?;
                // Refresh: catalog was updated, re-read column list.
                let snap2 = txn.active_snapshot()?;
                columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.def.id)?;
            }
            AlterTableOp::RenameTable(new_name) => {
                alter_rename_table(storage, txn, &table_def.def, &new_name, database, schema)?;
                // After RENAME TABLE further operations would need the new table_def;
                // for simplicity, only one op per statement is expected for RENAME TO.
                break;
            }
            AlterTableOp::AddConstraint(tc) => {
                alter_add_constraint(storage, txn, &table_def, &columns, tc, database, schema)?;
            }
            AlterTableOp::DropConstraint { name, if_exists } => {
                alter_drop_constraint(storage, txn, &table_def, &name, if_exists)?;
            }
            AlterTableOp::Rebuild => {
                // Bump before returning so the plan cache detects the schema change.
                let _ = CatalogWriter::new(storage, txn)?
                    .bump_table_schema_version(table_def.def.id);
                return alter_rebuild_to_clustered(
                    storage,
                    txn,
                    &table_def,
                    database,
                    schema,
                );
            }
            _ => {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE MODIFY COLUMN — Phase N".into(),
                })
            }
        }
    }

    // Bump per-table schema_version so plan caches referencing this table
    // detect staleness on next lookup (Phase 40.2 OID-based invalidation).
    let _ = CatalogWriter::new(storage, txn)?.bump_table_schema_version(table_def.def.id);

    Ok(QueryResult::Empty)
}

fn alter_add_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    col_def: crate::ast::ColumnDef,
    schema: &str,
) -> Result<(), DbError> {
    // Check for duplicate column name.
    let table_name = &table_def.table_name;
    if columns.iter().any(|c| c.name == col_def.name) {
        return Err(DbError::ColumnAlreadyExists {
            name: col_def.name.clone(),
            table: table_name.clone(),
        });
    }

    // Evaluate DEFAULT expression (or NULL if no default).
    let default_value = col_def
        .constraints
        .iter()
        .find_map(|c| match c {
            crate::ast::ColumnConstraint::Default(expr) => {
                Some(eval(expr, &[]).unwrap_or(Value::Null))
            }
            _ => None,
        })
        .unwrap_or(Value::Null);

    let col_type = datatype_to_column_type(&col_def.data_type)?;
    let nullable = !col_def
        .constraints
        .iter()
        .any(|c| matches!(c, crate::ast::ColumnConstraint::NotNull));
    let auto_increment = col_def
        .constraints
        .iter()
        .any(|c| matches!(c, crate::ast::ColumnConstraint::AutoIncrement));

    let new_col_idx = columns
        .iter()
        .map(|c| c.col_idx)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    let new_catalog_col = CatalogColumnDef {
        table_id: table_def.id,
        col_idx: new_col_idx,
        name: col_def.name.clone(),
        col_type,
        nullable,
        auto_increment,
    };

    // 1. Add column to catalog.
    CatalogWriter::new(storage, txn)?.create_column(new_catalog_col.clone())?;

    // 2. Rewrite rows (AFTER catalog update — new rows must include the new column).
    let old_columns = columns.clone();
    let mut new_columns = columns.clone();
    new_columns.push(new_catalog_col.clone());

    let dv = default_value;
    rewrite_rows(
        storage,
        txn,
        table_def,
        &old_columns,
        &new_columns,
        &|mut row| {
            row.push(dv.clone());
            row
        },
    )?;

    columns.push(new_catalog_col);
    let _ = schema; // schema already encoded in table_def
    Ok(())
}

fn alter_drop_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    name: &str,
    if_exists: bool,
) -> Result<(), DbError> {
    // Find the column by name.
    let drop_pos = match columns.iter().position(|c| c.name == name) {
        Some(pos) => pos,
        None if if_exists => return Ok(()),
        None => {
            return Err(DbError::ColumnNotFound {
                name: name.to_string(),
                table: table_def.table_name.clone(),
            })
        }
    };

    let dropped_col_idx = columns[drop_pos].col_idx;
    let old_columns = columns.clone();

    // Build new column list (without the dropped column).
    let mut new_columns = columns.clone();
    new_columns.remove(drop_pos);

    // 1. Rewrite rows BEFORE updating catalog (if rewrite fails, catalog is still consistent).
    rewrite_rows(
        storage,
        txn,
        table_def,
        &old_columns,
        &new_columns,
        &move |mut row| {
            if drop_pos < row.len() {
                row.remove(drop_pos);
            }
            row
        },
    )?;

    // 2. Delete column from catalog.
    CatalogWriter::new(storage, txn)?.delete_column(table_def.id, dropped_col_idx)?;

    *columns = new_columns;
    Ok(())
}

fn alter_rename_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &[axiomdb_catalog::schema::ColumnDef],
    old_name: &str,
    new_name: &str,
    _schema: &str,
) -> Result<(), DbError> {
    // Find old column.
    let col =
        columns
            .iter()
            .find(|c| c.name == old_name)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: old_name.to_string(),
                table: table_def.table_name.clone(),
            })?;

    // Check new name is not already in use.
    if columns.iter().any(|c| c.name == new_name) {
        return Err(DbError::ColumnAlreadyExists {
            name: new_name.to_string(),
            table: table_def.table_name.clone(),
        });
    }

    CatalogWriter::new(storage, txn)?.rename_column(
        table_def.id,
        col.col_idx,
        new_name.to_string(),
    )?;
    Ok(())
}

fn alter_rename_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    new_name: &str,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    // Check new name not already in use.
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.get_table_in_database(database, schema, new_name)?.is_some() {
        return Err(DbError::TableAlreadyExists {
            schema: schema.to_string(),
            name: new_name.to_string(),
        });
    }

    CatalogWriter::new(storage, txn)?.rename_table(table_def.id, new_name.to_string(), schema)?;
    Ok(())
}

// ── CHECK constraint enforcement (Phase 4.22b) ────────────────────────────────

/// Evaluates active CHECK constraints for a row about to be inserted/updated.
fn alter_add_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::ResolvedTable,
    columns_arg: &[axiomdb_catalog::schema::ColumnDef],
    tc: crate::ast::TableConstraint,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    use crate::ast::TableConstraint;
    use axiomdb_catalog::schema::ConstraintDef;

    match tc {
        TableConstraint::Unique {
            name,
            columns: col_names,
        } => {
            // ADD CONSTRAINT name UNIQUE (cols) → create a unique index named `name`.
            let idx_name = name.unwrap_or_else(|| {
                format!(
                    "axiom_uq_{}_{}",
                    table_def.def.table_name,
                    col_names.join("_")
                )
            });
            let stmt = crate::ast::CreateIndexStmt {
                name: idx_name,
                table: crate::ast::TableRef {
                    database: None,
                    schema: Some(schema.to_string()),
                    name: table_def.def.table_name.clone(),
                    alias: None,
                },
                columns: col_names
                    .into_iter()
                    .map(|c| crate::ast::IndexColumn {
                        name: c,
                        order: crate::ast::SortOrder::Asc,
                    })
                    .collect(),
                unique: true,
                if_not_exists: false,
                predicate: None,
                fillfactor: None,
                include_columns: vec![],
                index_type: crate::ast::IndexType::BTree,
                pages_per_range: None,
            };
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(stmt, storage, txn, &mut noop_bloom, database)?;
            Ok(())
        }

        TableConstraint::Check { name, expr } => {
            let cname = name.ok_or_else(|| DbError::ParseError {
                message: "ADD CONSTRAINT CHECK requires an explicit constraint name".into(),
                position: None,
            })?;

            // Check for duplicate constraint name.
            let snap = txn.active_snapshot()?;
            {
                let mut reader = CatalogReader::new(storage, snap)?;
                if reader
                    .get_constraint_by_name(table_def.def.id, &cname)?
                    .is_some()
                {
                    return Err(DbError::Other(format!(
                        "constraint '{cname}' already exists on table '{}'",
                        table_def.def.table_name
                    )));
                }
            }

            // Validate all existing rows.
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            for (_rid, row_values) in &existing_rows {
                let result = eval(&expr, row_values)?;
                if !crate::eval::is_truthy(&result) {
                    return Err(DbError::CheckViolation {
                        table: table_def.def.table_name.clone(),
                        constraint: cname.clone(),
                    });
                }
            }

            // Serialize the expression to SQL string for persistence.
            let check_expr = expr_to_sql_string(&expr);

            // Persist in axiom_constraints.
            CatalogWriter::new(storage, txn)?.create_constraint(ConstraintDef {
                constraint_id: 0, // allocated by writer
                table_id: table_def.def.id,
                name: cname,
                check_expr,
            })?;
            Ok(())
        }

        TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } => {
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            let child_col_idx = columns_arg
                .iter()
                .find(|c| &c.name == child_col_name)
                .map(|c| c.col_idx)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: child_col_name.clone(),
                    table: table_def.def.table_name.clone(),
                })?;
            let ref_col = ref_columns.first().map(|s| s.as_str());

            // Persist the FK definition (validates parent, creates auto-index if needed).
            persist_fk_constraint(
                table_def.def.id,
                &table_def.def.table_name,
                database,
                child_col_idx,
                child_col_name,
                &ref_table,
                ref_col,
                ast_fk_action_to_catalog(on_delete),
                ast_fk_action_to_catalog(on_update),
                name.as_deref(),
                storage,
                txn,
            )?;

            // Validate existing data: every non-NULL FK value must reference a parent row.
            let snap = txn.active_snapshot()?;
            let default_constraint_name = format!(
                "fk_{}_{}_{ref_table}",
                table_def.def.table_name, child_col_name
            );
            let constraint_name = name.as_deref().unwrap_or(&default_constraint_name);
            let new_fk = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader
                    .get_fk_by_name(table_def.def.id, constraint_name)?
                    .ok_or_else(|| DbError::Internal {
                        message: "FK just created not found in catalog".into(),
                    })?
            };
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            for (_, row) in &existing_rows {
                if let Err(e) = crate::fk_enforcement::check_fk_child_insert(
                    row,
                    std::slice::from_ref(&new_fk),
                    storage,
                    txn,
                    &mut noop_bloom,
                ) {
                    // Roll back: drop the FK definition (and its auto-created index).
                    let snap2 = txn.active_snapshot()?;
                    if let Ok(Some(fk)) = CatalogReader::new(storage, snap2)?
                        .get_fk_by_name(table_def.def.id, &new_fk.name)
                    {
                        let fk_index_id = fk.fk_index_id;
                        CatalogWriter::new(storage, txn)?.drop_foreign_key(fk.fk_id)?;
                        if fk_index_id != 0 {
                            let _ = execute_drop_index_by_id(
                                fk_index_id,
                                storage,
                                txn,
                                &mut noop_bloom,
                            );
                        }
                    }
                    return Err(e);
                }
            }

            Ok(())
        }

        TableConstraint::PrimaryKey { .. } => Err(DbError::NotImplemented {
            feature: "ADD CONSTRAINT PRIMARY KEY — requires full table rewrite".into(),
        }),
    }
}

fn alter_drop_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::ResolvedTable,
    name: &str,
    if_exists: bool,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    let table_id = table_def.def.id;

    // 1. Search in axiom_indexes (UNIQUE constraints stored as indexes).
    let (idx_id, idx_root) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        let indexes = reader.list_indexes(table_id)?;
        match indexes.into_iter().find(|i| i.name == name) {
            Some(i) => (Some(i.index_id), Some(i.root_page_id)),
            None => (None, None),
        }
    };

    if let Some(index_id) = idx_id {
        CatalogWriter::new(storage, txn)?.delete_index(index_id)?;
        if let Some(root) = idx_root {
            free_btree_pages(storage, root)?;
        }
        return Ok(());
    }

    // 2. Search in axiom_constraints (CHECK constraints).
    let constraint = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_constraint_by_name(table_id, name)?
    };

    if let Some(c) = constraint {
        CatalogWriter::new(storage, txn)?.drop_constraint(c.constraint_id)?;
        return Ok(());
    }

    // 3. Search in axiom_foreign_keys (FK constraints — Phase 6.5).
    let fk = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_fk_by_name(table_id, name)?
    };

    if let Some(fk_def) = fk {
        let fk_index_id = fk_def.fk_index_id;
        CatalogWriter::new(storage, txn)?.drop_foreign_key(fk_def.fk_id)?;
        // Drop the auto-created FK index (fk_index_id != 0 means we created it).
        if fk_index_id != 0 {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index_by_id(fk_index_id, storage, txn, &mut noop_bloom)?;
        }
        return Ok(());
    }

    if if_exists {
        Ok(())
    } else {
        Err(DbError::Other(format!(
            "constraint '{name}' not found on table '{}'",
            table_def.def.table_name
        )))
    }
}

/// Converts an [`Expr`] to a SQL string suitable for storing in `axiom_constraints`.
///
/// Not a perfect round-trip — whitespace and casing may differ from the original
/// input, but the output is valid SQL that can be re-parsed and evaluated.
fn expr_to_sql_string(expr: &Expr) -> String {
    use crate::expr::BinaryOp;

    match expr {
        Expr::Literal(v) => match v {
            Value::Int(n) => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Null => "NULL".to_string(),
            Value::Real(f) => f.to_string(),
            _ => format!("{v}"),
        },
        Expr::Column { name, .. } => name.clone(),
        Expr::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Concat => "||",
            };
            format!(
                "({} {op_str} {})",
                expr_to_sql_string(left),
                expr_to_sql_string(right)
            )
        }
        Expr::UnaryOp {
            op: crate::expr::UnaryOp::Not,
            operand,
        } => {
            format!("NOT {}", expr_to_sql_string(operand))
        }
        Expr::IsNull {
            expr: inner,
            negated: false,
        } => {
            format!("{} IS NULL", expr_to_sql_string(inner))
        }
        Expr::IsNull {
            expr: inner,
            negated: true,
        } => {
            format!("{} IS NOT NULL", expr_to_sql_string(inner))
        }
        // For complex expressions not yet handled, fall back to a debug representation.
        other => format!("{other:?}"),
    }
}

// ── ALTER TABLE ... REBUILD (Phase 39.19) ────────────────────────────────────

/// Converts a heap table with a PRIMARY KEY into clustered format:
/// scans rows in PK order → bulk-inserts into clustered B-tree → rebuilds
/// secondary indexes with PK bookmarks → swaps root in catalog → frees old pages.
fn alter_rebuild_to_clustered(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    resolved: &axiomdb_catalog::ResolvedTable,
    _database: &str,
    _schema: &str,
) -> Result<QueryResult, DbError> {
    use axiomdb_storage::{clustered_tree, page::PageType};

    // ── Validate preconditions ───────────────────────────────────────────
    if resolved.def.is_clustered() {
        return Err(DbError::Other(format!(
            "table '{}' is already clustered",
            resolved.def.table_name
        )));
    }

    let pk_idx = resolved
        .indexes
        .iter()
        .find(|i| i.is_primary)
        .ok_or_else(|| DbError::Other(format!(
            "ALTER TABLE REBUILD requires a PRIMARY KEY on '{}'",
            resolved.def.table_name
        )))?;

    let pk_col_positions: Vec<usize> = pk_idx
        .columns
        .iter()
        .map(|c| c.col_idx as usize)
        .collect();

    let columns = &resolved.columns;
    let col_types: Vec<axiomdb_types::DataType> = columns
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let _snap = txn.active_snapshot()?;
    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let old_root = resolved.def.root_page_id;

    // ── Phase 1: Scan heap rows in PK order ──────────────────────────────
    // Use PK B-tree to iterate in key order → batch heap read → decode.
    let pairs = axiomdb_index::BTree::range_in(storage, pk_idx.root_page_id, None, None)?;
    let rids: Vec<axiomdb_core::RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();
    let batch_rows = crate::table::TableEngine::read_rows_batch(storage, columns, &rids)?;

    let mut sorted_rows: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(rids.len());
    for (idx, values) in batch_rows.into_iter().enumerate() {
        match values {
            Some(values) => sorted_rows.push(values),
            None => {
                return Err(DbError::IndexIntegrityFailure {
                    table: format!("{}.{}", resolved.def.schema_name, resolved.def.table_name),
                    index: pk_idx.name.clone(),
                    reason: format!(
                        "PRIMARY KEY index points at dead heap slot {:?} during ALTER TABLE ... REBUILD",
                        rids[idx]
                    ),
                })
            }
        }
    }

    // ── Phase 2: Bulk-insert into new clustered B-tree ───────────────────
    // Rows arrive in PK order → inserts are append-only → no rebalancing.
    let mut new_root_pid: Option<u64> = None;

    for values in &sorted_rows {
        let pk_values: Vec<axiomdb_types::Value> = pk_col_positions
            .iter()
            .map(|&pos| values[pos].clone())
            .collect();
        let pk_key = crate::key_encoding::encode_index_key(&pk_values)?;
        let row_data = axiomdb_types::codec::encode_row(values, &col_types)?;
        let header = axiomdb_storage::heap::RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };

        new_root_pid = Some(clustered_tree::insert(
            storage,
            new_root_pid,
            &pk_key,
            &header,
            &row_data,
        )?);
    }

    let clustered_root = match new_root_pid {
        Some(root) => root,
        None => alloc_empty_clustered_root(storage)?,
    };

    // ── Phase 3: Rebuild secondary indexes with PK bookmarks ─────────────
    let secondary_indexes: Vec<&axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .collect();

    let mut new_sec_roots: Vec<(u32, u64)> = Vec::new(); // (index_id, new_root)
    let rebuild_result = (|| -> Result<QueryResult, DbError> {
        for sec_idx in &secondary_indexes {
            let layout =
                crate::clustered_secondary::ClusteredSecondaryLayout::derive(sec_idx, pk_idx)?;

            // Allocate fresh B-tree root for the secondary index.
            let new_sec_root_pid = storage.alloc_page(PageType::Index)?;
            {
                let mut page = axiomdb_storage::Page::new(PageType::Index, new_sec_root_pid);
                let leaf = axiomdb_index::page_layout::cast_leaf_mut(&mut page);
                leaf.is_leaf = 1;
                leaf.set_num_keys(0);
                leaf.set_next_leaf(axiomdb_index::page_layout::NULL_PAGE);
                page.update_checksum();
                storage.write_page(new_sec_root_pid, &page)?;
            }

            let sec_root_atomic = std::sync::atomic::AtomicU64::new(new_sec_root_pid);

            // Scan clustered tree and insert secondary entries.
            for values in &sorted_rows {
                layout.insert_row(storage, &sec_root_atomic, values)?;
            }

            new_sec_roots.push((
                sec_idx.index_id,
                sec_root_atomic.load(std::sync::atomic::Ordering::Acquire),
            ));
        }

        let mut old_pages = collect_heap_chain_pages(storage, old_root)?;
        old_pages.extend(collect_btree_pages(storage, pk_idx.root_page_id)?);
        for sec_idx in &secondary_indexes {
            old_pages.extend(collect_btree_pages(storage, sec_idx.root_page_id)?);
        }
        old_pages.sort_unstable();
        old_pages.dedup();

        storage.flush()?;

        // ── Phase 4: Atomic catalog swap + deferred reclamation ──────────
        {
            let mut writer = axiomdb_catalog::CatalogWriter::new(storage, txn)?;

            // Update table: root → clustered root, layout → Clustered.
            writer.update_table_root(resolved.def.id, clustered_root)?;
            writer.update_storage_layout(
                resolved.def.id,
                axiomdb_catalog::TableStorageLayout::Clustered,
            )?;

            // Update PK index root to match clustered root.
            writer.update_index_root(pk_idx.index_id, clustered_root)?;

            // Update secondary index roots.
            for (idx_id, new_root) in &new_sec_roots {
                writer.update_index_root(*idx_id, *new_root)?;
            }
        }

        txn.defer_free_pages(old_pages)?;

        Ok(QueryResult::Affected {
            count: sorted_rows.len() as u64,
            last_insert_id: None,
        })
    })();

    if let Err(err) = rebuild_result {
        cleanup_rebuild_artifacts(
            storage,
            Some(clustered_root),
            new_sec_roots.iter().map(|(_, root)| *root),
        );
        return Err(err);
    }

    rebuild_result
}

fn alloc_empty_clustered_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
    let pid = storage.alloc_page(PageType::ClusteredLeaf)?;
    let mut page = axiomdb_storage::Page::new(PageType::ClusteredLeaf, pid);
    axiomdb_storage::clustered_leaf::init_clustered_leaf(&mut page);
    page.update_checksum();
    storage.write_page(pid, &page)?;
    Ok(pid)
}

fn cleanup_rebuild_artifacts<I>(
    storage: &mut dyn StorageEngine,
    clustered_root: Option<u64>,
    secondary_roots: I,
) where
    I: IntoIterator<Item = u64>,
{
    if let Some(root) = clustered_root {
        let _ = free_clustered_tree_pages(storage, root);
    }
    for root in secondary_roots {
        let _ = free_btree_pages(storage, root);
    }
}

fn free_clustered_tree_pages(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
) -> Result<(), DbError> {
    let mut stack = vec![root_pid];
    let mut visited = std::collections::HashSet::new();

    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }

        let page = storage.read_page(pid)?;
        let page_type = PageType::try_from(page.header().page_type).map_err(|err| {
            DbError::BTreeCorrupted {
                msg: format!("clustered rebuild cleanup saw invalid page type at {pid}: {err}"),
            }
        })?;

        match page_type {
            PageType::ClusteredLeaf => {
                let n = axiomdb_storage::clustered_leaf::num_cells(&page);
                for idx in 0..n {
                    let cell = axiomdb_storage::clustered_leaf::read_cell(&page, idx)?;
                    if let Some(first_overflow) = cell.overflow_first_page {
                        axiomdb_storage::clustered_overflow::free_chain(storage, first_overflow)?;
                    }
                }
                storage.free_page(pid)?;
            }
            PageType::ClusteredInternal => {
                let n = axiomdb_storage::clustered_internal::num_cells(&page);
                for idx in 0..=n {
                    let child = axiomdb_storage::clustered_internal::child_at(&page, idx)?;
                    if child != axiomdb_storage::clustered_internal::NULL_PAGE {
                        stack.push(child);
                    }
                }
                storage.free_page(pid)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered rebuild cleanup expected clustered page at {pid}, found {other:?}"
                    ),
                })
            }
        }
    }

    Ok(())
}
