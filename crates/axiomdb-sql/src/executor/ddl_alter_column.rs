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
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Result<Row, DbError>,
) -> Result<(), DbError> {
    if table_def.is_clustered() {
        return rewrite_rows_clustered(storage, txn, conn_txn, table_def, old_columns, new_columns, transform);
    }
    let snap = txn.active_snapshot(conn_txn);
    let rows = TableEngine::scan_table(storage, table_def, old_columns, snap, None)?;
    for (rid, old_values) in rows {
        let new_values = transform(old_values)?;
        TableEngine::delete_row(storage, txn, conn_txn, table_def, rid)?;
        TableEngine::insert_row(storage, txn, conn_txn, table_def, new_columns, new_values)?;
    }
    Ok(())
}

/// Rewrites all rows in a clustered table by applying `transform` to each row.
///
/// Uses `clustered_tree::update_with_relocation` (in-place rewrite; falls back to
/// physical delete+reinsert when the new row doesn't fit in the current leaf page).
/// The PK never changes — ADD COLUMN and DROP COLUMN only affect non-key columns.
/// Secondary indexes whose columns are not affected remain valid after the rewrite.
fn rewrite_rows_clustered(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Result<Row, DbError>,
) -> Result<(), DbError> {
    use std::ops::Bound;

    use axiomdb_storage::clustered_tree;
    use axiomdb_types::codec::{decode_row, encode_row};

    let snap = txn.active_snapshot(conn_txn);
    let txn_id = conn_txn.txn_id;
    let mut root_pid = txn
        .clustered_root(table_def.id)
        .unwrap_or(table_def.root_page_id);

    let old_col_types = crate::table::column_data_types(old_columns);
    let new_col_types = crate::table::column_data_types(new_columns);

    // Collect all visible rows before modifying — range iterator borrows storage.
    let all_rows: Vec<axiomdb_storage::clustered_tree::ClusteredRow> = {
        let iter = clustered_tree::range(
            storage,
            Some(root_pid),
            Bound::Unbounded,
            Bound::Unbounded,
            &snap,
        )?;
        iter.collect::<Result<_, _>>()?
    };

    for row in all_rows {
        // Decode old values from raw row bytes.
        let old_values =
            decode_row(&row.row_data, &old_col_types).map_err(|e| {
                DbError::Other(format!(
                    "ALTER TABLE rewrite: failed to decode row in '{}': {e}",
                    table_def.table_name
                ))
            })?;

        // Apply schema transform (may fail for MODIFY COLUMN type coercions).
        let new_values = transform(old_values)?;
        let new_row_data = encode_row(&new_values, &new_col_types).map_err(|e| {
            DbError::Other(format!(
                "ALTER TABLE rewrite: failed to encode new row in '{}': {e}",
                table_def.table_name
            ))
        })?;

        // Build WAL images before modifying storage.
        let old_image =
            axiomdb_wal::ClusteredRowImage::new(root_pid, row.row_header, &row.row_data);
        let new_header = axiomdb_storage::heap::RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: row.row_header.row_version.saturating_add(1),
            _flags: row.row_header._flags,
        };

        // In-place rewrite; falls back to physical relocate when page is full.
        if let Some(new_root) = clustered_tree::update_with_relocation(
            storage,
            Some(root_pid),
            &row.key,
            &new_row_data,
            txn_id,
            &snap,
        )? {
            let new_image =
                axiomdb_wal::ClusteredRowImage::new(new_root, new_header, &new_row_data);
            txn.record_clustered_update(conn_txn, table_def.id, &row.key, &old_image, &new_image)?;
            root_pid = new_root;
        }
        // None = row not found or no longer visible — skip.
    }

    Ok(())
}

fn execute_alter_table(
    stmt: AlterTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Resolve the table once upfront.
    let table_def = {
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    // Keep the current column list; update it as we apply operations.
    let mut columns = table_def.columns.clone();

    for op in stmt.operations {
        match op {
            AlterTableOp::AddColumn(col_def) => {
                alter_add_column(storage, txn, conn_txn, &table_def.def, &mut columns, col_def, schema)?;
            }
            AlterTableOp::DropColumn { name, if_exists } => {
                alter_drop_column(storage, txn, conn_txn, &table_def.def, &mut columns, &name, if_exists)?;
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                alter_rename_column(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &columns,
                    &old_name,
                    &new_name,
                    schema,
                )?;
                // Refresh: catalog was updated, re-read column list.
                let snap2 = txn.active_snapshot(conn_txn);
                columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.def.id)?;
            }
            AlterTableOp::RenameTable(new_name) => {
                alter_rename_table(storage, txn, conn_txn, &table_def.def, &new_name, database, schema)?;
                // After RENAME TABLE further operations would need the new table_def;
                // for simplicity, only one op per statement is expected for RENAME TO.
                break;
            }
            AlterTableOp::AddConstraint(tc) => {
                alter_add_constraint(storage, txn, conn_txn, &table_def, &columns, tc, database, schema)?;
            }
            AlterTableOp::DropConstraint { name, if_exists } => {
                alter_drop_constraint(storage, txn, conn_txn, &table_def, &name, if_exists)?;
            }
            AlterTableOp::Rebuild => {
                // Bump before returning so the plan cache detects the schema change.
                let _ = CatalogWriter::new(storage, txn, conn_txn)?
                    .bump_table_schema_version(table_def.def.id);
                return alter_rebuild_to_clustered(
                    storage,
                    txn,
                    conn_txn,
                    &table_def,
                    database,
                    schema,
                );
            }
            AlterTableOp::ModifyColumn(col_def) => {
                alter_modify_column(storage, txn, conn_txn, &table_def.def, &mut columns, col_def, schema)?;
            }
            AlterTableOp::RenameIndex { old_name, new_name } => {
                alter_rename_index(storage, txn, conn_txn, table_def.def.id, &old_name, &new_name)?;
            }
            AlterTableOp::ConvertCharset | AlterTableOp::SetEngine => {
                // Accepted and ignored — charset/engine are compat metadata only.
            }
            AlterTableOp::SetAutoIncrement(_) => {
                // AUTO_INCREMENT counter reset accepted; not yet persisted (4.18e).
            }
            AlterTableOp::AddIndex { unique, name, columns } => {
                alter_add_index(
                    storage, txn, conn_txn, &table_def, &columns, unique, name, database,
                )?;
                // Refresh columns after index creation (schema version bumped by create_index).
            }
            AlterTableOp::DropIndex { name } => {
                alter_drop_index(storage, txn, conn_txn, table_def.def.id, &name, table_def.def.is_clustered())?;
            }
            AlterTableOp::ChangeColumn { old_name, new_def } => {
                // CHANGE COLUMN = rename + retype in one op.
                // Strategy: run MODIFY with a temp def named old_name (retype),
                // then rename old_name → new_def.name if they differ.
                let rename_needed = old_name != new_def.name;
                let new_name = new_def.name.clone();
                // Step 1: Modify type/nullability under the OLD name.
                let modify_def = crate::ast::ColumnDef {
                    name: old_name.clone(),
                    ..new_def
                };
                alter_modify_column(storage, txn, conn_txn, &table_def.def, &mut columns, modify_def, schema)?;
                // Step 2: Rename if needed.
                if rename_needed {
                    let snap2 = txn.active_snapshot(conn_txn);
                    columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.def.id)?;
                    alter_rename_column(
                        storage, txn, conn_txn, &table_def.def, &columns,
                        &old_name, &new_name, schema,
                    )?;
                    let snap3 = txn.active_snapshot(conn_txn);
                    columns = CatalogReader::new(storage, snap3)?.list_columns(table_def.def.id)?;
                }
            }
        }
    }

    // Bump per-table schema_version so plan caches referencing this table
    // detect staleness on next lookup (Phase 40.2 OID-based invalidation).
    let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_def.def.id);

    Ok(QueryResult::Empty)
}

fn alter_add_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
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
        type_len: col_def.type_len,
        is_fixed_len: col_def.is_char,
    };

    // 1. Add column to catalog.
    CatalogWriter::new(storage, txn, conn_txn)?.create_column(new_catalog_col.clone())?;

    // 2. Rewrite rows (AFTER catalog update — new rows must include the new column).
    let old_columns = columns.clone();
    let mut new_columns = columns.clone();
    new_columns.push(new_catalog_col.clone());

    let dv = default_value;
    rewrite_rows(
        storage,
        txn,
        conn_txn,
        table_def,
        &old_columns,
        &new_columns,
        &|mut row| {
            row.push(dv.clone());
            Ok(row)
        },
    )?;

    columns.push(new_catalog_col);
    let _ = schema; // schema already encoded in table_def
    Ok(())
}

fn alter_drop_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
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

    // Reject if the column is referenced by any secondary index — dropping it
    // would leave the index pointing at a non-existent column.
    {
        let snap = txn.active_snapshot(conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        let indexes = reader.list_indexes(table_def.id)?;
        for idx in &indexes {
            if idx.is_primary {
                continue;
            }
            if idx.columns.iter().any(|c| c.col_idx == dropped_col_idx) {
                return Err(DbError::NotImplemented {
                    feature: format!(
                        "Cannot drop column '{}': it is part of index '{}'. Drop the index first.",
                        name, idx.name
                    ),
                });
            }
        }
    }
    let old_columns = columns.clone();

    // Build new column list (without the dropped column).
    let mut new_columns = columns.clone();
    new_columns.remove(drop_pos);

    // 1. Rewrite rows BEFORE updating catalog (if rewrite fails, catalog is still consistent).
    rewrite_rows(
        storage,
        txn,
        conn_txn,
        table_def,
        &old_columns,
        &new_columns,
        &move |mut row| {
            if drop_pos < row.len() {
                row.remove(drop_pos);
            }
            Ok(row)
        },
    )?;

    // 2. Delete column from catalog.
    CatalogWriter::new(storage, txn, conn_txn)?.delete_column(table_def.id, dropped_col_idx)?;

    *columns = new_columns;
    Ok(())
}

/// `MODIFY [COLUMN] col_name new_type [NOT NULL | NULL]`
///
/// Rewrites all rows in the table to coerce the target column to the new type.
/// If the column type changes and the column is part of any secondary index the
/// operation is rejected — the caller must `DROP INDEX`, `MODIFY`, then
/// `CREATE INDEX` to avoid stale index key encodings.
fn alter_modify_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    col_def: crate::ast::ColumnDef,
    _schema: &str,
) -> Result<(), DbError> {
    use axiomdb_types::coerce::{coerce, CoercionMode};

    // Find the column to modify.
    let col_pos = columns
        .iter()
        .position(|c| c.name == col_def.name)
        .ok_or_else(|| DbError::ColumnNotFound {
            name: col_def.name.clone(),
            table: table_def.table_name.clone(),
        })?;

    let new_col_type = datatype_to_column_type(&col_def.data_type)?;
    let new_nullable = !col_def
        .constraints
        .iter()
        .any(|c| matches!(c, crate::ast::ColumnConstraint::NotNull));

    let old_col = &columns[col_pos];
    let old_col_type = old_col.col_type;
    let col_idx = old_col.col_idx;
    let type_changed = old_col_type != new_col_type;

    // Reject PK column nullability change — PK columns must be NOT NULL.
    {
        let snap = txn.active_snapshot(conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        let indexes = reader.list_indexes(table_def.id)?;

        // If the type changes and the column is in a secondary index, reject.
        if type_changed {
            for idx in &indexes {
                if idx.is_primary {
                    continue;
                }
                if idx.columns.iter().any(|c| c.col_idx == col_idx) {
                    return Err(DbError::NotImplemented {
                        feature: format!(
                            "Cannot change type of column '{}': it is part of index '{}'. \
                             Drop the index first.",
                            col_def.name, idx.name
                        ),
                    });
                }
            }
        }

        // PK column must stay NOT NULL.
        let is_pk_col = indexes
            .iter()
            .find(|i| i.is_primary)
            .map(|pk| pk.columns.iter().any(|c| c.col_idx == col_idx))
            .unwrap_or(false);
        if is_pk_col && new_nullable {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "PRIMARY KEY column '{}' cannot be changed to NULL",
                    col_def.name
                ),
            });
        }
    }

    // Build old and new column lists for rewrite_rows.
    let old_columns = columns.clone();
    let mut new_columns = columns.clone();
    new_columns[col_pos].col_type = new_col_type;
    new_columns[col_pos].nullable = new_nullable;

    let new_data_type = crate::table::column_type_to_data_type(new_col_type);

    // Rewrite rows: coerce the target column to the new type.
    // The coercion uses Strict mode — any value that cannot be converted fails
    // the entire statement atomically (no partial writes).
    rewrite_rows(
        storage,
        txn,
        conn_txn,
        table_def,
        &old_columns,
        &new_columns,
        &move |mut row| {
            if let Some(val) = row.get_mut(col_pos) {
                // Strict coercion: propagate error on conversion failure.
                // The whole statement rolls back atomically.
                *val = coerce(val.clone(), new_data_type, CoercionMode::Strict)?;
            }
            Ok(row)
        },
    )?;

    // Update catalog: replace the column definition with the new type/nullability.
    CatalogWriter::new(storage, txn, conn_txn)?.delete_column(table_def.id, col_idx)?;
    let new_catalog_col = axiomdb_catalog::ColumnDef {
        table_id: table_def.id,
        col_idx,
        name: col_def.name.clone(),
        col_type: new_col_type,
        nullable: new_nullable,
        auto_increment: old_columns[col_pos].auto_increment,
        type_len: col_def.type_len,
        is_fixed_len: col_def.is_char,
    };
    CatalogWriter::new(storage, txn, conn_txn)?.create_column(new_catalog_col.clone())?;

    *columns = new_columns;
    Ok(())
}

fn alter_rename_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
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

    CatalogWriter::new(storage, txn, conn_txn)?.rename_column(
        table_def.id,
        col.col_idx,
        new_name.to_string(),
    )?;
    Ok(())
}

fn alter_rename_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    new_name: &str,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    // Check new name not already in use.
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.get_table_in_database(database, schema, new_name)?.is_some() {
        return Err(DbError::TableAlreadyExists {
            schema: schema.to_string(),
            name: new_name.to_string(),
        });
    }

    CatalogWriter::new(storage, txn, conn_txn)?.rename_table(table_def.id, new_name.to_string(), schema)?;
    Ok(())
}

/// Renames an index: update the name field in the catalog row.
fn alter_rename_index(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    old_name: &str,
    new_name: &str,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let indexes = CatalogReader::new(storage, snap)?.list_indexes(table_id)?;
    let idx = indexes.into_iter().find(|i| i.name == old_name).ok_or_else(|| {
        DbError::NotImplemented {
            feature: format!("RENAME INDEX: index '{old_name}' not found"),
        }
    })?;
    CatalogWriter::new(storage, txn, conn_txn)?.rename_index(idx.index_id, new_name.to_string())?;
    Ok(())
}

/// Creates an index for ALTER TABLE ADD INDEX / ADD UNIQUE INDEX.
fn alter_add_index(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::resolver::ResolvedTable,
    col_names: &[String],
    unique: bool,
    name: Option<String>,
    database: &str,
) -> Result<(), DbError> {
    use crate::ast::{CreateIndexStmt, IndexType, SortOrder, TableRef};
    // Build a synthetic CreateIndexStmt and delegate to execute_create_index.
    let idx_name = name.unwrap_or_else(|| {
        // Auto-generate name: col1_col2
        col_names.join("_")
    });
    let stmt = CreateIndexStmt {
        if_not_exists: false,
        unique,
        name: idx_name,
        table: TableRef {
            database: Some(database.to_string()),
            schema: Some(table_def.def.schema_name.clone()),
            name: table_def.def.table_name.clone(),
            alias: None,
        },
        columns: col_names
            .iter()
            .map(|c| crate::ast::IndexColumn {
                name: c.clone(),
                order: SortOrder::Asc,
            })
            .collect(),
        predicate: None,
        fillfactor: None,
        include_columns: vec![],
        index_type: IndexType::BTree,
        pages_per_range: None,
    };
    let mut noop_bloom = crate::bloom::BloomRegistry::new();
    execute_create_index(stmt, storage, txn, conn_txn, &mut noop_bloom, database)
        .map(|_| ())
}

/// Drops an index by name for ALTER TABLE DROP INDEX.
fn alter_drop_index(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    name: &str,
    is_clustered: bool,
) -> Result<(), DbError> {
    if name == "PRIMARY" && is_clustered {
        return Err(DbError::NotImplemented {
            feature: "DROP PRIMARY KEY on clustered table — Phase 39.19".into(),
        });
    }
    let snap = txn.active_snapshot(conn_txn);
    let indexes = CatalogReader::new(storage, snap)?.list_indexes(table_id)?;
    let idx = match indexes.into_iter().find(|i| {
        if name == "PRIMARY" { i.is_primary } else { i.name == name }
    }) {
        Some(i) => i,
        None => return Ok(()), // index not found — treat as no-op (IF EXISTS semantics)
    };
    let root = idx.root_page_id;
    CatalogWriter::new(storage, txn, conn_txn)?.delete_index(idx.index_id)?;
    free_btree_pages(storage, root)?;
    let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_id);
    Ok(())
}

