fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let primary_key = collect_create_table_primary_key(&stmt)?;
    let unique_indexes = collect_create_table_unique_indexes(&stmt)?;
    let non_unique_indexes = collect_create_table_non_unique_indexes(&stmt)?;
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
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
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

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
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
        let type_len = col_def.type_len;
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

        let default_expr = col_def
            .constraints
            .iter()
            .find_map(|c| match c {
                ColumnConstraint::Default(expr) => {
                    Some(crate::expr_to_sql::expr_to_sql_string(expr))
                }
                _ => None,
            });

        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
            auto_increment,
            type_len,
            is_fixed_len: col_def.is_char,
            default_expr,
        })?;
    }

    {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};

        let mut create_empty_index = |index_name: String,
                                  columns: Vec<IndexColumnDef>,
                                  is_unique: bool,
                                  is_primary: bool,
                                  root_override: Option<u64>,
                                  storage: &dyn StorageEngine,
                                  txn: &TxnManager|
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

            CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
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

        // Non-unique inline INDEX/KEY constraints (MySQL extension).
        for idx_spec in non_unique_indexes {
            let idx_id = create_empty_index(
                idx_spec.name,
                idx_spec.columns,
                false,
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
            conn_txn,
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
            let snap = txn.active_snapshot(conn_txn);
            let child_col_idxs: Vec<u16> = {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                let cols = reader.list_columns(table_id)?;
                columns
                    .iter()
                    .map(|name| {
                        cols.iter()
                            .find(|c| &c.name == name)
                            .map(|c| c.col_idx)
                            .ok_or_else(|| DbError::ColumnNotFound {
                                name: name.clone(),
                                table: stmt.table.name.clone(),
                            })
                    })
                    .collect::<Result<_, _>>()?
            };
            if columns.len() == 1 {
                let ref_col = ref_columns.first().map(|s| s.as_str());
                persist_fk_constraint(
                    table_id,
                    &stmt.table.name,
                    database,
                    child_col_idxs[0],
                    &columns[0],
                    ref_table,
                    ref_col,
                    ast_fk_action_to_catalog(*on_delete),
                    ast_fk_action_to_catalog(*on_update),
                    name.as_deref(),
                    storage,
                    txn,
                    conn_txn,
                )?;
            } else {
                persist_composite_fk_constraint(
                    table_id,
                    &stmt.table.name,
                    database,
                    &child_col_idxs,
                    columns,
                    ref_table,
                    ref_columns,
                    ast_fk_action_to_catalog(*on_delete),
                    ast_fk_action_to_catalog(*on_update),
                    name.as_deref(),
                    storage,
                    txn,
                    conn_txn,
                )?;
            }
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

fn collect_create_table_non_unique_indexes(
    stmt: &CreateTableStmt,
) -> Result<Vec<CreateTableIndexSpec>, DbError> {
    let mut indexes = Vec::new();
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Index { name, columns } = tc {
            let generated_name = if columns.len() == 1 {
                format!("{}_{}_idx", stmt.table.name, columns[0])
            } else {
                format!("{}_{}_idx", stmt.table.name, columns.join("_"))
            };
            indexes.push(CreateTableIndexSpec {
                name: name.clone().unwrap_or(generated_name),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }
    Ok(indexes)
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot(conn_txn);

    // 1. Resolve parent table.
    let parent_def = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader
            .get_table_in_database(database, "public", ref_table)?
            .ok_or_else(|| DbError::TableNotFound {
                name: ref_table.to_string(),
            })?
    };

    // 2. Find the referenced column in the parent table.
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
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
            let mut reader = CatalogReader::new(storage, snap.clone())?;
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
        let mut reader = CatalogReader::new(storage, snap.clone())?;
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
        let mut reader = CatalogReader::new(storage, snap.clone())?;
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
            let mut reader = CatalogReader::new(storage, snap.clone())?;
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
                let mut reader = CatalogReader::new(storage, snap.clone())?;
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
                    let mut reader = CatalogReader::new(storage, snap.clone())?;
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
                let new_idx_id = CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
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
    CatalogWriter::new(storage, txn, conn_txn)?.create_foreign_key(FkDef {
        fk_id: 0, // allocated by CatalogWriter::create_foreign_key
        child_table_id,
        child_col_idx,
        parent_table_id: parent_def.id,
        parent_col_idx,
        on_delete,
        on_update,
        fk_index_id,
        name: constraint_name,
        child_col_idxs: vec![child_col_idx],
        parent_col_idxs: vec![parent_col_idx],
    })?;

    Ok(())
}

// ── CREATE TABLE LIKE ─────────────────────────────────────────────────────────

/// Implements `CREATE TABLE new_table LIKE source_table`.
///
/// Copies the full schema (columns + indexes) from `source_table` into a new
/// empty table. No data is copied. FK constraints are intentionally not copied
/// (MySQL behaviour: `CREATE TABLE … LIKE` does not inherit FK constraints).
fn execute_create_table_like(
    stmt: crate::ast::CreateTableLikeStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};

    let new_schema = stmt.new_table.schema.as_deref().unwrap_or("public");
    let src_schema = stmt.source_table.schema.as_deref().unwrap_or("public");
    let src_db = stmt.source_table.database.as_deref().unwrap_or(database);

    // 1. Resolve source table (read-only snapshot).
    let source = {
        let snap = txn.active_snapshot(conn_txn);
        let mut resolver = SchemaResolver::new(storage, snap, src_db, src_schema)?;
        resolver.resolve_table(Some(src_schema), &stmt.source_table.name)?
    };

    // 2. Check the destination does not already exist.
    {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        if resolver.table_exists(Some(new_schema), &stmt.new_table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: new_schema.to_string(),
                name: stmt.new_table.name.clone(),
            });
        }
    }

    // 3. Create the new table with the same storage layout.
    let new_def = {
        let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
        let def = writer.create_table_with_layout(
            new_schema,
            &stmt.new_table.name,
            source.def.storage_layout,
        )?;
        if database != DEFAULT_DATABASE_NAME {
            writer.bind_table_to_database(def.id, database)?;
        }
        def
    };
    let new_table_id = new_def.id;

    // 4. Copy columns (same col_idx, same type/nullable/auto_increment flags).
    for col in &source.columns {
        CatalogWriter::new(storage, txn, conn_txn)?.create_column(CatalogColumnDef {
            table_id: new_table_id,
            col_idx: col.col_idx,
            name: col.name.clone(),
            col_type: col.col_type,
            nullable: col.nullable,
            auto_increment: col.auto_increment,
            type_len: col.type_len,
            is_fixed_len: col.is_fixed_len,
            default_expr: col.default_expr.clone(),
        })?;
    }

    // 5. Copy indexes with fresh empty B-tree roots.
    //    For clustered tables the primary index root IS the table root page.
    for idx in &source.indexes {
        let root_page_id = if idx.is_primary && source.def.is_clustered() {
            new_def.root_page_id
        } else {
            let pid = storage.alloc_page(PageType::Index)?;
            let mut page = Page::new(PageType::Index, pid);
            let leaf = cast_leaf_mut(&mut page);
            leaf.is_leaf = 1;
            leaf.set_num_keys(0);
            leaf.set_next_leaf(NULL_PAGE);
            page.update_checksum();
            storage.write_page(pid, &page)?;
            pid
        };

        CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
            index_id: 0,
            table_id: new_table_id,
            name: idx.name.clone(),
            root_page_id,
            is_unique: idx.is_unique,
            is_primary: idx.is_primary,
            // FK-backing indexes are not preserved — FK constraints are not copied.
            is_fk_index: false,
            columns: idx.columns.clone(),
            predicate: idx.predicate.clone(),
            fillfactor: idx.fillfactor,
            include_columns: idx.include_columns.clone(),
            index_type: idx.index_type,
            pages_per_range: idx.pages_per_range,
        })?;
    }

    Ok(QueryResult::Empty)
}

// ── CREATE TABLE AS SELECT ────────────────────────────────────────────────────

/// Implements `CREATE TABLE new_table AS SELECT …`.
///
/// 1. Executes the inner SELECT to materialize all rows.
/// 2. Infers column types from the first non-NULL value in each output column
///    (defaults to `TEXT` for all-NULL columns).
/// 3. Creates a new Heap table with the inferred schema.
/// 4. Inserts all rows.
///
/// The resulting table has no primary key, no indexes, and no FK constraints.
fn execute_create_table_as_select(
    stmt: crate::ast::CreateTableAsSelectStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // SAFETY: see ExecutionContext::storage_mut / coord_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let new_schema = stmt
        .new_table
        .schema
        .clone()
        .unwrap_or_else(|| ctx.current_schema().to_string());
    let database = ctx.effective_database().to_string();
    let new_name = stmt.new_table.name.clone();

    // 1. Run the SELECT (read-only — borrows storage/txn immutably via coercion).
    let conn = ctx.conn_txn.take();
    let result = execute_select_ctx(stmt.select, exec_ctx, conn.as_ref(), ctx)?;
    ctx.conn_txn = conn;
    let (col_meta, rows) = match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        // SELECT without FROM (e.g. SELECT 1) returns Rows with zero or one row.
        other => {
            return Err(DbError::Other(format!(
                "CTAS inner query returned unexpected result: {other:?}"
            )))
        }
    };

    // 2. Infer column types: first non-NULL value in each column determines the type.
    let num_cols = col_meta.len();
    let mut inferred: Vec<Option<ColumnType>> = vec![None; num_cols];
    'rows: for row in &rows {
        for (i, val) in row.iter().enumerate() {
            if inferred[i].is_some() {
                continue;
            }
            inferred[i] = match val {
                Value::Null => None,
                Value::Bool(_) => Some(ColumnType::Bool),
                Value::Int(_) => Some(ColumnType::Int),
                Value::BigInt(_) => Some(ColumnType::BigInt),
                Value::Real(_) => Some(ColumnType::Float),
                Value::Text(_) => Some(ColumnType::Text),
                Value::Bytes(_) => Some(ColumnType::Bytes),
                Value::Timestamp(_) => Some(ColumnType::Timestamp),
                Value::Uuid(_) => Some(ColumnType::Uuid),
                // Decimal / Date not in ColumnType yet — store as Text.
                _ => Some(ColumnType::Text),
            };
        }
        if inferred.iter().all(|t| t.is_some()) {
            break 'rows;
        }
    }
    let col_types: Vec<ColumnType> = inferred
        .into_iter()
        .map(|t| t.unwrap_or(ColumnType::Text))
        .collect();

    // 3. Check destination table does not already exist.
    {
        let conn_txn = ctx.conn_txn.as_ref().expect("conn_txn set for DDL");
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), &database)?;
        if resolver.table_exists(Some(&new_schema), &new_name)? {
            return Err(DbError::TableAlreadyExists {
                schema: new_schema.clone(),
                name: new_name.clone(),
            });
        }
    }

    // 4. Create the table (Heap — no primary key in CTAS).
    let new_def = {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
        let def = writer
            .create_table_with_layout(&new_schema, &new_name, axiomdb_catalog::schema::TableStorageLayout::Heap)?;
        if database != DEFAULT_DATABASE_NAME {
            writer.bind_table_to_database(def.id, &database)?;
        }
        def
    };

    // 5. Create columns from inferred types + output column names.
    for (i, (meta, col_type)) in col_meta.iter().zip(col_types.iter()).enumerate() {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        CatalogWriter::new(storage, txn, conn_txn)?.create_column(CatalogColumnDef {
            table_id: new_def.id,
            col_idx: i as u16,
            name: meta.name.clone(),
            col_type: *col_type,
            nullable: true, // CTAS columns are always nullable
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
        })?;
    }

    // Build a ColumnDef slice for TableEngine::insert_row.
    let schema_cols: Vec<CatalogColumnDef> = col_meta
        .iter()
        .zip(col_types.iter())
        .enumerate()
        .map(|(i, (meta, &col_type))| CatalogColumnDef {
            table_id: new_def.id,
            col_idx: i as u16,
            name: meta.name.clone(),
            col_type,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
        })
        .collect();

    // 6. Insert all rows into the new table.
    for row in rows {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        TableEngine::insert_row(storage, txn, conn_txn, &new_def, &schema_cols, row)?;
    }

    Ok(QueryResult::Empty)
}

// ── Composite foreign key (GAP-C.2) ──────────────────────────────────────────

/// Persists a multi-column FK. Requires that both parent and child already
/// have an index whose leading columns exactly match the FK column list
/// (parent: PRIMARY KEY or UNIQUE covering `parent_col_idxs`; child: any
/// index whose prefix matches `child_col_idxs`). This avoids the complexity
/// of auto-building a composite child index on existing rows — a tradeoff
/// that mirrors MySQL's recommendation to always pre-declare the child
/// composite index explicitly.
#[allow(clippy::too_many_arguments)]
fn persist_composite_fk_constraint(
    child_table_id: u32,
    child_table_name: &str,
    database: &str,
    child_col_idxs: &[u16],
    child_col_names: &[String],
    ref_table: &str,
    ref_columns: &[String],
    on_delete: axiomdb_catalog::FkAction,
    on_update: axiomdb_catalog::FkAction,
    fk_name: Option<&str>,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot(conn_txn);

    // 1. Resolve parent table + columns.
    let parent_def = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader
            .get_table_in_database(database, "public", ref_table)?
            .ok_or_else(|| DbError::TableNotFound {
                name: ref_table.to_string(),
            })?
    };
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_columns(parent_def.id)?
    };

    // Parent column list: must match arity. If REFERENCES specifies columns
    // explicitly, use those; otherwise default to the PK leading columns.
    let parent_col_idxs: Vec<u16> = if ref_columns.is_empty() {
        let parent_indexes = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader.list_indexes(parent_def.id)?
        };
        let pk = parent_indexes
            .iter()
            .find(|i| i.is_primary && i.columns.len() >= child_col_idxs.len())
            .ok_or_else(|| DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: "<primary key>".to_string(),
            })?;
        pk.columns
            .iter()
            .take(child_col_idxs.len())
            .map(|c| c.col_idx)
            .collect()
    } else {
        if ref_columns.len() != child_col_idxs.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "composite FK arity mismatch: child has {} column(s), REFERENCES has {}",
                    child_col_idxs.len(),
                    ref_columns.len()
                ),
            });
        }
        ref_columns
            .iter()
            .map(|name| {
                parent_cols
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.col_idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: name.clone(),
                        table: ref_table.to_string(),
                    })
            })
            .collect::<Result<_, _>>()?
    };

    // 2. Verify parent has a PK or UNIQUE index covering every parent col in order.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let parent_indexes = reader.list_indexes(parent_def.id)?;
        let covered = parent_indexes.iter().any(|i| {
            (i.is_primary || i.is_unique)
                && i.columns.len() >= parent_col_idxs.len()
                && i.columns
                    .iter()
                    .take(parent_col_idxs.len())
                    .zip(parent_col_idxs.iter())
                    .all(|(ic, wanted)| ic.col_idx == *wanted)
        });
        if !covered {
            return Err(DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: format!("({})", ref_columns.join(",")),
            });
        }
    }

    // 3. Auto-generate FK name if not provided.
    let constraint_name: String = fk_name.map(|n| n.to_string()).unwrap_or_else(|| {
        format!(
            "fk_{child_table_name}_{}_{ref_table}",
            child_col_names.join("_")
        )
    });

    // 4. Name uniqueness on this child table.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
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

    // 5. Verify child already has an index whose leading columns match
    //    `child_col_idxs`. Auto-building a composite FK index on existing
    //    child rows is not yet supported — user must declare it explicitly.
    let child_has_index = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_indexes(child_table_id)?.into_iter().any(|i| {
            !i.is_fk_index
                && i.columns.len() >= child_col_idxs.len()
                && i.columns
                    .iter()
                    .take(child_col_idxs.len())
                    .zip(child_col_idxs.iter())
                    .all(|(ic, wanted)| ic.col_idx == *wanted)
        })
    };
    if !child_has_index {
        return Err(DbError::InvalidValue {
            reason: format!(
                "composite FK '{constraint_name}' requires a pre-declared index on \
                 child columns ({}); auto-creation of composite FK indexes is not \
                 supported yet",
                child_col_names.join(",")
            ),
        });
    }

    // 6. Persist FkDef with composite vectors. `fk_index_id = 0` → user-provided.
    CatalogWriter::new(storage, txn, conn_txn)?.create_foreign_key(FkDef {
        fk_id: 0,
        child_table_id,
        child_col_idx: child_col_idxs[0],
        parent_table_id: parent_def.id,
        parent_col_idx: parent_col_idxs[0],
        on_delete,
        on_update,
        fk_index_id: 0,
        name: constraint_name,
        child_col_idxs: child_col_idxs.to_vec(),
        parent_col_idxs,
    })?;

    Ok(())
}

