fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
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

        let mut create_empty_index = |index_name: String,
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
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            let snap = txn.active_snapshot(conn_txn);
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
                conn_txn,
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
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
    })?;

    Ok(())
}

