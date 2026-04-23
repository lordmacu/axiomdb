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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Result<Row, DbError>,
) -> Result<(), DbError> {
    if table_def.is_clustered() {
        return rewrite_rows_clustered(
            storage,
            txn,
            conn_txn,
            table_def,
            old_columns,
            new_columns,
            transform,
        );
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
        let old_values = decode_row(&row.row_data, &old_col_types).map_err(|e| {
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

    if root_pid != table_def.root_page_id {
        CatalogWriter::new(storage, txn, conn_txn)?.update_table_root(table_def.id, root_pid)?;
    }

    Ok(())
}

fn current_table_def_for_alter(
    table_def: &axiomdb_catalog::schema::TableDef,
    txn: &TxnManager,
) -> axiomdb_catalog::schema::TableDef {
    let mut current = table_def.clone();
    if table_def.is_clustered() {
        if let Some(root) = txn.clustered_root(table_def.id) {
            current.root_page_id = root;
        }
    }
    current
}

type AlterMetadata = (
    Vec<axiomdb_catalog::schema::IndexDef>,
    Vec<axiomdb_catalog::schema::ConstraintDef>,
    Vec<axiomdb_catalog::schema::FkDef>,
    Vec<axiomdb_catalog::schema::FkDef>,
);

fn load_alter_metadata(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
) -> Result<AlterMetadata, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    Ok((
        reader.list_indexes(table_id)?,
        reader.list_constraints(table_id)?,
        reader.list_fk_constraints(table_id)?,
        reader.list_fk_constraints_referencing(table_id)?,
    ))
}

fn replace_table_columns(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<(), DbError> {
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    for col in old_columns {
        writer.delete_column(table_id, col.col_idx)?;
    }
    for col in new_columns {
        writer.create_column(col.clone())?;
    }
    Ok(())
}

fn expr_mentions_column_name(expr: &crate::expr::Expr, target_name: &str) -> bool {
    use crate::expr::Expr;

    match expr {
        Expr::Column { name, .. }
        | Expr::OuterColumn { name, .. }
        | Expr::InsertValue { name, .. }
        | Expr::ExcludedValue { name, .. } => name.eq_ignore_ascii_case(target_name),
        Expr::Literal(_) | Expr::Param { .. } | Expr::Default | Expr::SqlJsonQuery { .. } => false,
        Expr::UnaryOp { operand, .. }
        | Expr::IsNull { expr: operand, .. }
        | Expr::IsBoolean { expr: operand, .. }
        | Expr::Cast { expr: operand, .. } => expr_mentions_column_name(operand, target_name),
        Expr::BinaryOp { left, right, .. } => {
            expr_mentions_column_name(left, target_name)
                || expr_mentions_column_name(right, target_name)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_mentions_column_name(expr, target_name)
                || expr_mentions_column_name(low, target_name)
                || expr_mentions_column_name(high, target_name)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_mentions_column_name(expr, target_name)
                || expr_mentions_column_name(pattern, target_name)
                || escape
                    .as_deref()
                    .map(|e| expr_mentions_column_name(e, target_name))
                    .unwrap_or(false)
        }
        Expr::In { expr, list, .. } => {
            expr_mentions_column_name(expr, target_name)
                || list
                    .iter()
                    .any(|e| expr_mentions_column_name(e, target_name))
        }
        Expr::Function { args, .. } => args
            .iter()
            .any(|e| expr_mentions_column_name(e, target_name)),
        Expr::Window { spec, .. } => {
            spec.partition_by
                .iter()
                .any(|e| expr_mentions_column_name(e, target_name))
                || spec
                    .order_by
                    .iter()
                    .any(|item| expr_mentions_column_name(&item.expr, target_name))
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand
                .as_deref()
                .map(|e| expr_mentions_column_name(e, target_name))
                .unwrap_or(false)
                || when_thens.iter().any(|(when, then)| {
                    expr_mentions_column_name(when, target_name)
                        || expr_mentions_column_name(then, target_name)
                })
                || else_result
                    .as_deref()
                    .map(|e| expr_mentions_column_name(e, target_name))
                    .unwrap_or(false)
        }
        Expr::Subquery(_) | Expr::Exists { .. } => false,
        Expr::InSubquery { expr, .. } => expr_mentions_column_name(expr, target_name),
        Expr::GroupConcat { expr, order_by, .. } => {
            expr_mentions_column_name(expr, target_name)
                || order_by
                    .iter()
                    .any(|(e, _)| expr_mentions_column_name(e, target_name))
        }
        Expr::Grouping { args, .. } => args.iter().any(|a| expr_mentions_column_name(a, target_name)),
    }
}

fn stored_expr_mentions_column_name(expr_sql: &str, target_name: &str) -> Result<bool, DbError> {
    let expr = crate::parser::parse_expr_only(expr_sql)?;
    Ok(expr_mentions_column_name(&expr, target_name))
}

fn index_depends_on_column(
    idx: &axiomdb_catalog::schema::IndexDef,
    target_name: &str,
    target_col_idx: u16,
) -> Result<bool, DbError> {
    if idx.columns.iter().any(|c| c.col_idx == target_col_idx) {
        return Ok(true);
    }
    if idx.include_columns.contains(&target_col_idx) {
        return Ok(true);
    }
    if let Some(pred_sql) = &idx.predicate {
        return stored_expr_mentions_column_name(pred_sql, target_name);
    }
    Ok(false)
}

fn shift_col_idx_after_drop(col_idx: u16, dropped_col_idx: u16) -> u16 {
    if col_idx > dropped_col_idx {
        col_idx - 1
    } else {
        col_idx
    }
}

fn remap_index_after_drop(
    idx: &axiomdb_catalog::schema::IndexDef,
    dropped_col_idx: u16,
) -> axiomdb_catalog::schema::IndexDef {
    let mut updated = idx.clone();
    for col in &mut updated.columns {
        col.col_idx = shift_col_idx_after_drop(col.col_idx, dropped_col_idx);
    }
    for col_idx in &mut updated.include_columns {
        *col_idx = shift_col_idx_after_drop(*col_idx, dropped_col_idx);
    }
    updated
}

fn remap_child_fk_after_drop(
    fk: &axiomdb_catalog::schema::FkDef,
    dropped_col_idx: u16,
) -> Option<axiomdb_catalog::schema::FkDef> {
    if fk.child_col_idx > dropped_col_idx {
        let mut updated = fk.clone();
        updated.child_col_idx = shift_col_idx_after_drop(fk.child_col_idx, dropped_col_idx);
        Some(updated)
    } else {
        None
    }
}

fn remap_parent_fk_after_drop(
    fk: &axiomdb_catalog::schema::FkDef,
    dropped_col_idx: u16,
) -> Option<axiomdb_catalog::schema::FkDef> {
    if fk.parent_col_idx > dropped_col_idx {
        let mut updated = fk.clone();
        updated.parent_col_idx = shift_col_idx_after_drop(fk.parent_col_idx, dropped_col_idx);
        Some(updated)
    } else {
        None
    }
}

fn cleanup_rebuilt_index_roots(storage: &dyn StorageEngine, roots: &[u64]) {
    let mut seen = std::collections::HashSet::new();
    for &root in roots {
        if seen.insert(root) {
            let _ = free_btree_pages(storage, root);
        }
    }
}

#[derive(Debug, Clone)]
enum NonBlockingHeapAlterOp {
    Add,
    Drop {
        dropped_indexes: Vec<axiomdb_catalog::schema::IndexDef>,
        updated_child_fks: Vec<axiomdb_catalog::schema::FkDef>,
        updated_parent_fks: Vec<axiomdb_catalog::schema::FkDef>,
    },
    Modify,
}

#[derive(Debug, Clone)]
pub struct NonBlockingHeapAlterPlan {
    table_id: u32,
    old_columns: Vec<axiomdb_catalog::schema::ColumnDef>,
    new_columns: Vec<axiomdb_catalog::schema::ColumnDef>,
    old_indexes: Vec<axiomdb_catalog::schema::IndexDef>,
    new_indexes: Vec<axiomdb_catalog::schema::IndexDef>,
    shadow_root_page_id: u64,
    old_pages_to_defer: Vec<u64>,
    op: NonBlockingHeapAlterOp,
}

fn cleanup_nonblocking_heap_artifacts(
    storage: &dyn StorageEngine,
    shadow_root_page_id: u64,
    index_roots: &[u64],
) {
    let _ = free_heap_chain_pages(storage, shadow_root_page_id);
    cleanup_rebuilt_index_roots(storage, index_roots);
}

fn free_heap_chain_pages(storage: &dyn StorageEngine, root_page_id: u64) -> Result<(), DbError> {
    let pages = collect_heap_chain_pages(storage, root_page_id)?;
    for pid in pages {
        storage.free_page(pid)?;
    }
    Ok(())
}

fn build_shadow_heap_rows(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Result<Row, DbError>,
) -> Result<u64, DbError> {
    table_def.ensure_heap_runtime("non-blocking ALTER TABLE on clustered table")?;
    let shadow_root_page_id = alloc_empty_heap_root(storage)?;
    let mut shadow_table_def = table_def.clone();
    shadow_table_def.root_page_id = shadow_root_page_id;

    let snap = txn.active_snapshot(conn_txn);
    let rows = TableEngine::scan_table(storage, table_def, old_columns, snap, None)?;
    for (_rid, old_values) in rows {
        let new_values = transform(old_values)?;
        TableEngine::insert_row(
            storage,
            txn,
            conn_txn,
            &shadow_table_def,
            new_columns,
            new_values,
        )?;
    }

    Ok(shadow_root_page_id)
}

fn collect_old_heap_and_index_pages(
    storage: &dyn StorageEngine,
    table_def: &axiomdb_catalog::schema::TableDef,
    indexes: &[axiomdb_catalog::schema::IndexDef],
) -> Result<Vec<u64>, DbError> {
    let mut pages = collect_heap_chain_pages(storage, table_def.root_page_id)?;
    for idx in indexes {
        pages.extend(collect_btree_pages(storage, idx.root_page_id)?);
    }
    pages.sort_unstable();
    pages.dedup();
    Ok(pages)
}

fn validate_nonblocking_alter_table_stmt(
    table_def: &axiomdb_catalog::schema::TableDef,
    stmt: &AlterTableStmt,
) -> Result<AlterTableOp, DbError> {
    table_def.ensure_heap_runtime("non-blocking ALTER TABLE on clustered table")?;
    if stmt.operations.len() != 1 {
        return Err(DbError::NotImplemented {
            feature: "non-blocking ALTER TABLE with multiple operations".into(),
        });
    }

    match &stmt.operations[0] {
        AlterTableOp::AddColumn(_)
        | AlterTableOp::DropColumn { .. }
        | AlterTableOp::ModifyColumn(_) => Ok(stmt.operations[0].clone()),
        _ => Err(DbError::NotImplemented {
            feature: "non-blocking ALTER TABLE for this operation".into(),
        }),
    }
}

pub fn prepare_nonblocking_heap_alter(
    stmt: AlterTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<NonBlockingHeapAlterPlan, DbError> {
    let table_def = {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    let op = validate_nonblocking_alter_table_stmt(&table_def.def, &stmt)?;
    let columns = table_def.columns.clone();
    let indexes = table_def.indexes.clone();

    match op {
        AlterTableOp::AddColumn(col_def) => {
            if generated_column_constraint(&col_def)?.is_some() {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE generated columns".into(),
                });
            }
            if columns.iter().any(|c| c.name == col_def.name) {
                return Err(DbError::ColumnAlreadyExists {
                    name: col_def.name.clone(),
                    table: table_def.def.table_name.clone(),
                });
            }

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

            let new_col_idx = columns
                .iter()
                .map(|c| c.col_idx)
                .max()
                .map(|m| m + 1)
                .unwrap_or(0);
            let new_catalog_col = CatalogColumnDef {
                table_id: table_def.def.id,
                col_idx: new_col_idx,
                name: col_def.name.clone(),
                col_type: datatype_to_column_type(&col_def.data_type)?,
                nullable: !col_def
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::ast::ColumnConstraint::NotNull)),
                auto_increment: col_def
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::ast::ColumnConstraint::AutoIncrement)),
                type_len: col_def.type_len,
                is_fixed_len: col_def.is_char,
                default_expr: col_def.constraints.iter().find_map(|c| match c {
                    crate::ast::ColumnConstraint::Default(expr) => {
                        Some(crate::expr_to_sql::expr_to_sql_string(expr))
                    }
                    _ => None,
                }),
                on_update_expr: col_def.constraints.iter().find_map(|c| match c {
                    crate::ast::ColumnConstraint::OnUpdate(expr) => {
                        Some(crate::expr_to_sql::expr_to_sql_string(expr))
                    }
                    _ => None,
                }),
                generated_expr: None,
                generated_stored: false,
            };
            let mut new_columns = columns.clone();
            new_columns.push(new_catalog_col);

            let dv = default_value;
            let shadow_root_page_id = build_shadow_heap_rows(
                storage,
                txn,
                conn_txn,
                &table_def.def,
                &columns,
                &new_columns,
                &|mut row| {
                    row.push(dv.clone());
                    Ok(row)
                },
            )?;
            let mut shadow_table_def = table_def.def.clone();
            shadow_table_def.root_page_id = shadow_root_page_id;
            let snap = txn.active_snapshot(conn_txn);
            let mut new_indexes = Vec::with_capacity(indexes.len());
            let mut built_roots = Vec::new();
            for idx in &indexes {
                let mut updated = idx.clone();
                let build = match build_index_root_from_existing_def(
                    storage,
                    &shadow_table_def,
                    &new_columns,
                    &updated,
                    snap.clone(),
                ) {
                    Ok(build) => build,
                    Err(err) => {
                        cleanup_nonblocking_heap_artifacts(
                            storage,
                            shadow_root_page_id,
                            &built_roots,
                        );
                        return Err(err);
                    }
                };
                updated.root_page_id = build.root_page_id;
                built_roots.push(build.root_page_id);
                new_indexes.push(updated);
            }
            Ok(NonBlockingHeapAlterPlan {
                table_id: table_def.def.id,
                old_columns: columns,
                new_columns,
                old_indexes: indexes.clone(),
                new_indexes,
                shadow_root_page_id,
                old_pages_to_defer: collect_old_heap_and_index_pages(
                    storage,
                    &table_def.def,
                    &indexes,
                )?,
                op: NonBlockingHeapAlterOp::Add,
            })
        }
        AlterTableOp::DropColumn { name, if_exists } => {
            let drop_pos = match columns.iter().position(|c| c.name == name) {
                Some(pos) => pos,
                None if if_exists => {
                    return Ok(NonBlockingHeapAlterPlan {
                        table_id: table_def.def.id,
                        old_columns: columns.clone(),
                        new_columns: columns,
                        old_indexes: indexes.clone(),
                        new_indexes: indexes,
                        shadow_root_page_id: 0,
                        old_pages_to_defer: Vec::new(),
                        op: NonBlockingHeapAlterOp::Drop {
                            dropped_indexes: Vec::new(),
                            updated_child_fks: Vec::new(),
                            updated_parent_fks: Vec::new(),
                        },
                    })
                }
                None => {
                    return Err(DbError::ColumnNotFound {
                        name,
                        table: table_def.def.table_name.clone(),
                    })
                }
            };

            let dropped_col = columns[drop_pos].clone();
            let dropped_col_idx = dropped_col.col_idx;
            let dropped_col_name = dropped_col.name.clone();
            let (_loaded_indexes, constraints, child_fks, parent_fks) =
                load_alter_metadata(storage, txn, conn_txn, table_def.def.id)?;

            if indexes
                .iter()
                .any(|idx| idx.is_primary && idx.columns.iter().any(|c| c.col_idx == dropped_col_idx))
            {
                return Err(DbError::InvalidValue {
                    reason: format!("PRIMARY KEY column '{}' cannot be dropped", dropped_col_name),
                });
            }
            if let Some(fk) = child_fks.iter().find(|fk| fk.child_col_idx == dropped_col_idx) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "Cannot drop column '{}': it is referenced by foreign key '{}'",
                        dropped_col_name, fk.name
                    ),
                });
            }
            if let Some(fk) = parent_fks.iter().find(|fk| fk.parent_col_idx == dropped_col_idx) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "Cannot drop column '{}': it is referenced by foreign key '{}'",
                        dropped_col_name, fk.name
                    ),
                });
            }
            for constraint in &constraints {
                if !constraint.check_expr.is_empty()
                    && stored_expr_mentions_column_name(&constraint.check_expr, &dropped_col_name)?
                {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "Cannot drop column '{}': it is referenced by CHECK constraint '{}'",
                            dropped_col_name, constraint.name
                        ),
                    });
                }
            }

            let mut new_columns = columns.clone();
            new_columns.remove(drop_pos);
            for (new_pos, col) in new_columns.iter_mut().enumerate() {
                col.col_idx = new_pos as u16;
            }

            let mut dropped_indexes = Vec::new();
            let mut surviving_indexes = Vec::new();
            for idx in &indexes {
                if index_depends_on_column(idx, &dropped_col_name, dropped_col_idx)? {
                    dropped_indexes.push(idx.clone());
                } else {
                    surviving_indexes.push(remap_index_after_drop(idx, dropped_col_idx));
                }
            }
            let updated_child_fks: Vec<_> = child_fks
                .iter()
                .filter_map(|fk| remap_child_fk_after_drop(fk, dropped_col_idx))
                .collect();
            let updated_parent_fks: Vec<_> = parent_fks
                .iter()
                .filter_map(|fk| remap_parent_fk_after_drop(fk, dropped_col_idx))
                .collect();

            let shadow_root_page_id = build_shadow_heap_rows(
                storage,
                txn,
                conn_txn,
                &table_def.def,
                &columns,
                &new_columns,
                &move |mut row| {
                    if drop_pos < row.len() {
                        row.remove(drop_pos);
                    }
                    Ok(row)
                },
            )?;
            let mut shadow_table_def = table_def.def.clone();
            shadow_table_def.root_page_id = shadow_root_page_id;
            let snap = txn.active_snapshot(conn_txn);
            let mut new_indexes = Vec::with_capacity(surviving_indexes.len());
            let mut built_roots = Vec::new();
            for idx in surviving_indexes {
                let mut updated = idx.clone();
                let build = match build_index_root_from_existing_def(
                    storage,
                    &shadow_table_def,
                    &new_columns,
                    &updated,
                    snap.clone(),
                ) {
                    Ok(build) => build,
                    Err(err) => {
                        cleanup_nonblocking_heap_artifacts(
                            storage,
                            shadow_root_page_id,
                            &built_roots,
                        );
                        return Err(err);
                    }
                };
                updated.root_page_id = build.root_page_id;
                built_roots.push(build.root_page_id);
                new_indexes.push(updated);
            }

            Ok(NonBlockingHeapAlterPlan {
                table_id: table_def.def.id,
                old_columns: columns,
                new_columns,
                old_indexes: indexes,
                new_indexes,
                shadow_root_page_id,
                old_pages_to_defer: collect_old_heap_and_index_pages(
                    storage,
                    &table_def.def,
                    &table_def.indexes,
                )?,
                op: NonBlockingHeapAlterOp::Drop {
                    dropped_indexes,
                    updated_child_fks,
                    updated_parent_fks,
                },
            })
        }
        AlterTableOp::ModifyColumn(col_def) => {
            use axiomdb_types::coerce::{coerce, CoercionMode};

            if generated_column_constraint(&col_def)?.is_some() {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE generated columns".into(),
                });
            }

            let col_pos = columns
                .iter()
                .position(|c| c.name == col_def.name)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: col_def.name.clone(),
                    table: table_def.def.table_name.clone(),
                })?;
            let old_col = &columns[col_pos];
            let col_idx = old_col.col_idx;
            let (_loaded_indexes, constraints, child_fks, parent_fks) =
                load_alter_metadata(storage, txn, conn_txn, table_def.def.id)?;
            let is_pk_col = indexes
                .iter()
                .find(|i| i.is_primary)
                .map(|pk| pk.columns.iter().any(|c| c.col_idx == col_idx))
                .unwrap_or(false);
            if is_pk_col {
                return Err(DbError::InvalidValue {
                    reason: format!("PRIMARY KEY column '{}' cannot be modified", col_def.name),
                });
            }
            if let Some(fk) = child_fks.iter().find(|fk| fk.child_col_idx == col_idx) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "Cannot modify column '{}': it is referenced by foreign key '{}'",
                        col_def.name, fk.name
                    ),
                });
            }
            if let Some(fk) = parent_fks.iter().find(|fk| fk.parent_col_idx == col_idx) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "Cannot modify column '{}': it is referenced by foreign key '{}'",
                        col_def.name, fk.name
                    ),
                });
            }
            for constraint in &constraints {
                if !constraint.check_expr.is_empty()
                    && stored_expr_mentions_column_name(&constraint.check_expr, &col_def.name)?
                {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "Cannot modify column '{}': it is referenced by CHECK constraint '{}'",
                            col_def.name, constraint.name
                        ),
                    });
                }
            }

            let new_col_type = datatype_to_column_type(&col_def.data_type)?;
            let new_nullable = !col_def
                .constraints
                .iter()
                .any(|c| matches!(c, crate::ast::ColumnConstraint::NotNull));
            let new_data_type = crate::table::column_type_to_data_type(new_col_type);

            let old_columns = columns.clone();
            let mut new_columns = columns.clone();
            new_columns[col_pos].col_type = new_col_type;
            new_columns[col_pos].nullable = new_nullable;
            new_columns[col_pos].type_len = col_def.type_len;
            new_columns[col_pos].is_fixed_len = col_def.is_char;
            new_columns[col_pos].default_expr = col_def
                .constraints
                .iter()
                .find_map(|c| match c {
                    crate::ast::ColumnConstraint::Default(expr) => {
                        Some(crate::expr_to_sql::expr_to_sql_string(expr))
                    }
                    _ => None,
                })
                .or_else(|| old_columns[col_pos].default_expr.clone());
            new_columns[col_pos].on_update_expr = col_def
                .constraints
                .iter()
                .find_map(|c| match c {
                    crate::ast::ColumnConstraint::OnUpdate(expr) => {
                        Some(crate::expr_to_sql::expr_to_sql_string(expr))
                    }
                    _ => None,
                })
                .or_else(|| old_columns[col_pos].on_update_expr.clone());

            let shadow_root_page_id = build_shadow_heap_rows(
                storage,
                txn,
                conn_txn,
                &table_def.def,
                &old_columns,
                &new_columns,
                &move |mut row| {
                    if let Some(val) = row.get_mut(col_pos) {
                        *val = coerce(val.clone(), new_data_type, CoercionMode::Strict)?;
                    }
                    Ok(row)
                },
            )?;
            let mut shadow_table_def = table_def.def.clone();
            shadow_table_def.root_page_id = shadow_root_page_id;
            let snap = txn.active_snapshot(conn_txn);
            let mut new_indexes = Vec::with_capacity(indexes.len());
            let mut built_roots = Vec::new();
            for idx in &indexes {
                let mut updated = idx.clone();
                let build = match build_index_root_from_existing_def(
                    storage,
                    &shadow_table_def,
                    &new_columns,
                    &updated,
                    snap.clone(),
                ) {
                    Ok(build) => build,
                    Err(err) => {
                        cleanup_nonblocking_heap_artifacts(
                            storage,
                            shadow_root_page_id,
                            &built_roots,
                        );
                        return Err(err);
                    }
                };
                updated.root_page_id = build.root_page_id;
                built_roots.push(build.root_page_id);
                new_indexes.push(updated);
            }

            Ok(NonBlockingHeapAlterPlan {
                table_id: table_def.def.id,
                old_columns,
                new_columns,
                old_indexes: indexes,
                new_indexes,
                shadow_root_page_id,
                old_pages_to_defer: collect_old_heap_and_index_pages(
                    storage,
                    &table_def.def,
                    &table_def.indexes,
                )?,
                op: NonBlockingHeapAlterOp::Modify,
            })
        }
        _ => unreachable!(),
    }
}

pub fn cleanup_nonblocking_heap_alter_plan(
    storage: &dyn StorageEngine,
    plan: &NonBlockingHeapAlterPlan,
) {
    let index_roots: Vec<u64> = plan.new_indexes.iter().map(|idx| idx.root_page_id).collect();
    cleanup_nonblocking_heap_artifacts(storage, plan.shadow_root_page_id, &index_roots);
}

pub fn commit_nonblocking_heap_alter(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    plan: NonBlockingHeapAlterPlan,
) -> Result<QueryResult, DbError> {
    if plan.old_pages_to_defer.is_empty() {
        return Ok(QueryResult::Empty);
    }

    let NonBlockingHeapAlterPlan {
        table_id,
        old_columns,
        new_columns,
        old_indexes: _old_indexes,
        new_indexes,
        shadow_root_page_id,
        old_pages_to_defer,
        op,
    } = plan;

    CatalogWriter::new(storage, txn, conn_txn)?.update_table_root(table_id, shadow_root_page_id)?;
    replace_table_columns(storage, txn, conn_txn, table_id, &old_columns, &new_columns)?;

    match op {
        NonBlockingHeapAlterOp::Add | NonBlockingHeapAlterOp::Modify => {
            for idx in new_indexes {
                CatalogWriter::new(storage, txn, conn_txn)?.replace_index_def(idx)?;
            }
        }
        NonBlockingHeapAlterOp::Drop {
            dropped_indexes,
            updated_child_fks,
            updated_parent_fks,
        } => {
            for fk in updated_child_fks {
                CatalogWriter::new(storage, txn, conn_txn)?.replace_foreign_key(fk)?;
            }
            for fk in updated_parent_fks {
                CatalogWriter::new(storage, txn, conn_txn)?.replace_foreign_key(fk)?;
            }
            for idx in dropped_indexes {
                CatalogWriter::new(storage, txn, conn_txn)?.delete_index(idx.index_id)?;
            }
            for idx in new_indexes {
                CatalogWriter::new(storage, txn, conn_txn)?.replace_index_def(idx)?;
            }
        }
    }

    txn.defer_free_pages(conn_txn, old_pages_to_defer);
    let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_id);
    Ok(QueryResult::Empty)
}

fn execute_alter_table(
    stmt: AlterTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Resolve the table once upfront.
    let table_def = {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    // Keep the current column list; update it as we apply operations.
    let mut columns = table_def.columns.clone();
    let total_ops = stmt.operations.len();
    let mut alter_result = QueryResult::Empty;

    for (idx, op) in stmt.operations.into_iter().enumerate() {
        match op {
            AlterTableOp::AddColumn(col_def) => {
                alter_add_column(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &mut columns,
                    col_def,
                    schema,
                )?;
            }
            AlterTableOp::DropColumn { name, if_exists } => {
                alter_drop_column(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &mut columns,
                    &name,
                    if_exists,
                )?;
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
                alter_rename_table(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &new_name,
                    database,
                    schema,
                )?;
                // After RENAME TABLE further operations would need the new table_def;
                // for simplicity, only one op per statement is expected for RENAME TO.
                break;
            }
            AlterTableOp::AddConstraint(tc) => {
                let is_add_pk = matches!(&tc, crate::ast::TableConstraint::PrimaryKey { .. });
                if is_add_pk && idx + 1 < total_ops {
                    return Err(DbError::NotImplemented {
                        feature: "ALTER TABLE ... ADD PRIMARY KEY followed by additional operations in the same statement".into(),
                    });
                }
                if let Some(result) = alter_add_constraint(
                    storage, txn, conn_txn, &table_def, &columns, tc, database, schema,
                )? {
                    alter_result = result;
                }
            }
            AlterTableOp::DropConstraint { name, if_exists } => {
                alter_drop_constraint(storage, txn, conn_txn, &table_def, &name, if_exists)?;
            }
            AlterTableOp::Rebuild => {
                // Bump before returning so the plan cache detects the schema change.
                let _ = CatalogWriter::new(storage, txn, conn_txn)?
                    .bump_table_schema_version(table_def.def.id);
                return alter_rebuild_to_clustered(
                    storage, txn, conn_txn, &table_def, database, schema,
                );
            }
            AlterTableOp::ModifyColumn(col_def) => {
                alter_modify_column(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &mut columns,
                    col_def,
                    schema,
                )?;
            }
            AlterTableOp::RenameIndex { old_name, new_name } => {
                alter_rename_index(
                    storage,
                    txn,
                    conn_txn,
                    table_def.def.id,
                    &old_name,
                    &new_name,
                )?;
            }
            AlterTableOp::ConvertCharset | AlterTableOp::SetEngine => {
                // Accepted and ignored — charset/engine are compat metadata only.
            }
            AlterTableOp::SetAutoIncrement(n) => {
                // MySQL semantics: only honors N if greater than the current
                // max value on the AUTO_INCREMENT column; otherwise silently
                // ignored. Persistence lives in the per-process `AUTO_INC_SEQ`
                // cache — future inserts on this table use `next >= N`.
                //
                // NOTE: full cross-restart persistence requires a catalog field
                // (`auto_increment_next` on TableDef) and is tracked separately.
                if let Some(ai_col) = columns.iter().position(|c| c.auto_increment) {
                    let snap = txn.active_snapshot(conn_txn);
                    let max_existing = if table_def.def.is_clustered() {
                        crate::clustered_table::scan_max_numeric_column(
                            storage,
                            txn.clustered_root(table_def.def.id)
                                .or(Some(table_def.def.root_page_id)),
                            &columns,
                            ai_col,
                            &snap,
                        )?
                    } else {
                        let rows =
                            TableEngine::scan_table(storage, &table_def.def, &columns, snap, None)?;
                        rows.iter()
                            .filter_map(|(_, vals)| vals.get(ai_col))
                            .filter_map(|v| match v {
                                Value::Int(m) => Some(*m as u64),
                                Value::BigInt(m) => Some(*m as u64),
                                _ => None,
                            })
                            .max()
                            .unwrap_or(0)
                    };
                    let desired = n.max(max_existing + 1);
                    AUTO_INC_SEQ.with(|seq| {
                        seq.borrow_mut().insert(table_def.def.id, desired);
                    });
                }
            }
            AlterTableOp::AddIndex {
                unique,
                name,
                columns,
            } => {
                alter_add_index(
                    storage, txn, conn_txn, &table_def, &columns, unique, name, database,
                )?;
                // Refresh columns after index creation (schema version bumped by create_index).
            }
            AlterTableOp::DropIndex { name } => {
                alter_drop_index(
                    storage,
                    txn,
                    conn_txn,
                    table_def.def.id,
                    &name,
                    table_def.def.is_clustered(),
                )?;
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
                alter_modify_column(
                    storage,
                    txn,
                    conn_txn,
                    &table_def.def,
                    &mut columns,
                    modify_def,
                    schema,
                )?;
                // Step 2: Rename if needed.
                if rename_needed {
                    let snap2 = txn.active_snapshot(conn_txn);
                    columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.def.id)?;
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
                    let snap3 = txn.active_snapshot(conn_txn);
                    columns = CatalogReader::new(storage, snap3)?.list_columns(table_def.def.id)?;
                }
            }
        }
    }

    // Bump per-table schema_version so plan caches referencing this table
    // detect staleness on next lookup (Phase 40.2 OID-based invalidation).
    let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_def.def.id);

    Ok(alter_result)
}

fn alter_add_column(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    col_def: crate::ast::ColumnDef,
    schema: &str,
) -> Result<(), DbError> {
    // Check for duplicate column name.
    let table_name = &table_def.table_name;
    if generated_column_constraint(&col_def)?.is_some() {
        return Err(DbError::NotImplemented {
            feature: "ALTER TABLE generated columns".into(),
        });
    }
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
        default_expr: col_def.constraints.iter().find_map(|c| match c {
            crate::ast::ColumnConstraint::Default(expr) => {
                Some(crate::expr_to_sql::expr_to_sql_string(expr))
            }
            _ => None,
        }),
        on_update_expr: col_def.constraints.iter().find_map(|c| match c {
            crate::ast::ColumnConstraint::OnUpdate(expr) => {
                Some(crate::expr_to_sql::expr_to_sql_string(expr))
            }
            _ => None,
        }),
        generated_expr: None,
        generated_stored: false,
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

    // 4.22e follow-up: the heap rewrite above deleted+re-inserted every row,
    // so RIDs changed. Secondary indexes on a heap table still point at the
    // old RIDs — rebuild them from the live heap. Clustered tables don't
    // change RIDs during rewrite (`rewrite_rows_clustered` updates in place
    // with PK preserved) so no rebuild is needed there.
    if !table_def.is_clustered() {
        let snap = txn.active_snapshot(conn_txn);
        let indexes = CatalogReader::new(storage, snap.clone())?.list_indexes(table_def.id)?;
        let current_table_def = current_table_def_for_alter(table_def, txn);
        let mut built_roots_for_cleanup = Vec::new();
        let mut pages_to_defer = Vec::new();
        let rebuild_result = (|| -> Result<(), DbError> {
            for idx in &indexes {
                if idx.columns.is_empty() {
                    continue;
                }
                let build = build_index_root_from_existing_def(
                    storage,
                    &current_table_def,
                    &new_columns,
                    idx,
                    snap.clone(),
                )?;
                built_roots_for_cleanup.push(build.root_page_id);
                pages_to_defer.extend(collect_btree_pages(storage, idx.root_page_id)?);

                let mut updated = idx.clone();
                updated.root_page_id = build.root_page_id;
                CatalogWriter::new(storage, txn, conn_txn)?.replace_index_def(updated)?;
            }
            pages_to_defer.sort_unstable();
            pages_to_defer.dedup();
            txn.defer_free_pages(conn_txn, pages_to_defer);
            Ok(())
        })();
        if let Err(err) = rebuild_result {
            cleanup_rebuilt_index_roots(storage, &built_roots_for_cleanup);
            return Err(err);
        }
    }

    columns.push(new_catalog_col);
    let _ = schema; // schema already encoded in table_def
    Ok(())
}

fn alter_drop_column(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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

    let dropped_col = columns[drop_pos].clone();
    let dropped_col_idx = dropped_col.col_idx;
    let dropped_col_name = dropped_col.name.clone();
    let (indexes, constraints, child_fks, parent_fks) =
        load_alter_metadata(storage, txn, conn_txn, table_def.id)?;

    if indexes
        .iter()
        .any(|idx| idx.is_primary && idx.columns.iter().any(|c| c.col_idx == dropped_col_idx))
    {
        return Err(DbError::InvalidValue {
            reason: format!("PRIMARY KEY column '{}' cannot be dropped", name),
        });
    }
    if let Some(fk) = child_fks
        .iter()
        .find(|fk| fk.child_col_idx == dropped_col_idx)
    {
        return Err(DbError::InvalidValue {
            reason: format!(
                "Cannot drop column '{}': it is referenced by foreign key '{}'",
                name, fk.name
            ),
        });
    }
    if let Some(fk) = parent_fks
        .iter()
        .find(|fk| fk.parent_col_idx == dropped_col_idx)
    {
        return Err(DbError::InvalidValue {
            reason: format!(
                "Cannot drop column '{}': it is referenced by foreign key '{}'",
                name, fk.name
            ),
        });
    }
    for constraint in &constraints {
        if !constraint.check_expr.is_empty()
            && stored_expr_mentions_column_name(&constraint.check_expr, &dropped_col_name)?
        {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "Cannot drop column '{}': it is referenced by CHECK constraint '{}'",
                    name, constraint.name
                ),
            });
        }
    }

    let old_columns = columns.clone();

    // Build new column list (without the dropped column).
    let mut new_columns = columns.clone();
    new_columns.remove(drop_pos);
    for (new_pos, col) in new_columns.iter_mut().enumerate() {
        col.col_idx = new_pos as u16;
    }

    let mut dropped_indexes = Vec::new();
    let mut surviving_indexes = Vec::new();
    for idx in indexes {
        if index_depends_on_column(&idx, &dropped_col_name, dropped_col_idx)? {
            dropped_indexes.push(idx);
        } else {
            surviving_indexes.push((idx.clone(), remap_index_after_drop(&idx, dropped_col_idx)));
        }
    }
    let updated_child_fks: Vec<_> = child_fks
        .iter()
        .filter_map(|fk| remap_child_fk_after_drop(fk, dropped_col_idx))
        .collect();
    let updated_parent_fks: Vec<_> = parent_fks
        .iter()
        .filter_map(|fk| remap_parent_fk_after_drop(fk, dropped_col_idx))
        .collect();

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

    // 2. Replace the column catalog with the renumbered schema.
    replace_table_columns(
        storage,
        txn,
        conn_txn,
        table_def.id,
        &old_columns,
        &new_columns,
    )?;

    let mut built_roots_for_cleanup = Vec::new();
    let repair_result = (|| -> Result<(), DbError> {
        for fk in &updated_child_fks {
            CatalogWriter::new(storage, txn, conn_txn)?.replace_foreign_key(fk.clone())?;
        }
        for fk in &updated_parent_fks {
            CatalogWriter::new(storage, txn, conn_txn)?.replace_foreign_key(fk.clone())?;
        }

        let mut pages_to_defer = Vec::new();

        for idx in &dropped_indexes {
            pages_to_defer.extend(collect_btree_pages(storage, idx.root_page_id)?);
            CatalogWriter::new(storage, txn, conn_txn)?.delete_index(idx.index_id)?;
        }

        if table_def.is_clustered() {
            for (old_idx, updated_idx) in &surviving_indexes {
                if old_idx != updated_idx {
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .replace_index_def(updated_idx.clone())?;
                }
            }
        } else {
            let current_table_def = current_table_def_for_alter(table_def, txn);
            let snap = txn.active_snapshot(conn_txn);
            for (old_idx, updated_idx) in &surviving_indexes {
                let build = build_index_root_from_existing_def(
                    storage,
                    &current_table_def,
                    &new_columns,
                    updated_idx,
                    snap.clone(),
                )?;
                built_roots_for_cleanup.push(build.root_page_id);
                pages_to_defer.extend(collect_btree_pages(storage, old_idx.root_page_id)?);

                let mut final_idx = updated_idx.clone();
                final_idx.root_page_id = build.root_page_id;
                CatalogWriter::new(storage, txn, conn_txn)?.replace_index_def(final_idx)?;
            }
        }

        pages_to_defer.sort_unstable();
        pages_to_defer.dedup();
        txn.defer_free_pages(conn_txn, pages_to_defer);
        Ok(())
    })();

    if let Err(err) = repair_result {
        cleanup_rebuilt_index_roots(storage, &built_roots_for_cleanup);
        return Err(err);
    }

    *columns = new_columns;
    Ok(())
}

/// `MODIFY [COLUMN] col_name new_type [NOT NULL | NULL]`
///
/// Rewrites all rows in the table to coerce the target column to the new type,
/// then repairs secondary indexes according to the table layout.
fn alter_modify_column(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    col_def: crate::ast::ColumnDef,
    _schema: &str,
) -> Result<(), DbError> {
    use axiomdb_types::coerce::{coerce, CoercionMode};

    if generated_column_constraint(&col_def)?.is_some() {
        return Err(DbError::NotImplemented {
            feature: "ALTER TABLE generated columns".into(),
        });
    }

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
    let col_idx = old_col.col_idx;
    let (indexes, constraints, child_fks, parent_fks) =
        load_alter_metadata(storage, txn, conn_txn, table_def.id)?;
    let is_pk_col = indexes
        .iter()
        .find(|i| i.is_primary)
        .map(|pk| pk.columns.iter().any(|c| c.col_idx == col_idx))
        .unwrap_or(false);
    if is_pk_col {
        return Err(DbError::InvalidValue {
            reason: format!("PRIMARY KEY column '{}' cannot be modified", col_def.name),
        });
    }
    if let Some(fk) = child_fks.iter().find(|fk| fk.child_col_idx == col_idx) {
        return Err(DbError::InvalidValue {
            reason: format!(
                "Cannot modify column '{}': it is referenced by foreign key '{}'",
                col_def.name, fk.name
            ),
        });
    }
    if let Some(fk) = parent_fks.iter().find(|fk| fk.parent_col_idx == col_idx) {
        return Err(DbError::InvalidValue {
            reason: format!(
                "Cannot modify column '{}': it is referenced by foreign key '{}'",
                col_def.name, fk.name
            ),
        });
    }
    for constraint in &constraints {
        if !constraint.check_expr.is_empty()
            && stored_expr_mentions_column_name(&constraint.check_expr, &col_def.name)?
        {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "Cannot modify column '{}': it is referenced by CHECK constraint '{}'",
                    col_def.name, constraint.name
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
        default_expr: col_def
            .constraints
            .iter()
            .find_map(|c| match c {
                crate::ast::ColumnConstraint::Default(expr) => {
                    Some(crate::expr_to_sql::expr_to_sql_string(expr))
                }
                _ => None,
            })
            .or_else(|| old_columns[col_pos].default_expr.clone()),
        on_update_expr: col_def
            .constraints
            .iter()
            .find_map(|c| match c {
                crate::ast::ColumnConstraint::OnUpdate(expr) => {
                    Some(crate::expr_to_sql::expr_to_sql_string(expr))
                }
                _ => None,
            })
            .or_else(|| old_columns[col_pos].on_update_expr.clone()),
        generated_expr: old_columns[col_pos].generated_expr.clone(),
        generated_stored: old_columns[col_pos].generated_stored,
    };
    CatalogWriter::new(storage, txn, conn_txn)?.create_column(new_catalog_col.clone())?;

    let mut built_roots_for_cleanup = Vec::new();
    let repair_result = (|| -> Result<(), DbError> {
        let current_table_def = current_table_def_for_alter(table_def, txn);
        let snap = txn.active_snapshot(conn_txn);
        let mut pages_to_defer = Vec::new();

        for idx in &indexes {
            let must_rebuild = if table_def.is_clustered() {
                !idx.is_primary && index_depends_on_column(idx, &col_def.name, col_idx)?
            } else {
                true
            };
            if !must_rebuild {
                continue;
            }

            let build = build_index_root_from_existing_def(
                storage,
                &current_table_def,
                &new_columns,
                idx,
                snap.clone(),
            )?;
            built_roots_for_cleanup.push(build.root_page_id);
            pages_to_defer.extend(collect_btree_pages(storage, idx.root_page_id)?);

            let mut updated_idx = idx.clone();
            updated_idx.root_page_id = build.root_page_id;
            CatalogWriter::new(storage, txn, conn_txn)?.replace_index_def(updated_idx)?;
        }

        pages_to_defer.sort_unstable();
        pages_to_defer.dedup();
        txn.defer_free_pages(conn_txn, pages_to_defer);
        Ok(())
    })();

    if let Err(err) = repair_result {
        cleanup_rebuilt_index_roots(storage, &built_roots_for_cleanup);
        return Err(err);
    }

    *columns = new_columns;
    Ok(())
}

fn alter_rename_column(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    new_name: &str,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    // Check new name not already in use.
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader
        .get_table_in_database(database, schema, new_name)?
        .is_some()
    {
        return Err(DbError::TableAlreadyExists {
            schema: schema.to_string(),
            name: new_name.to_string(),
        });
    }

    CatalogWriter::new(storage, txn, conn_txn)?.rename_table(
        table_def.id,
        new_name.to_string(),
        schema,
    )?;
    Ok(())
}

/// Renames an index: update the name field in the catalog row.
fn alter_rename_index(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    old_name: &str,
    new_name: &str,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let indexes = CatalogReader::new(storage, snap)?.list_indexes(table_id)?;
    let idx = indexes
        .into_iter()
        .find(|i| i.name == old_name)
        .ok_or_else(|| DbError::NotImplemented {
            feature: format!("RENAME INDEX: index '{old_name}' not found"),
        })?;
    CatalogWriter::new(storage, txn, conn_txn)?.rename_index(idx.index_id, new_name.to_string())?;
    Ok(())
}

/// Creates an index for ALTER TABLE ADD INDEX / ADD UNIQUE INDEX.
fn alter_add_index(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
                expr: None,
            })
            .collect(),
        predicate: None,
        fillfactor: None,
        include_columns: vec![],
        index_type: IndexType::BTree,
        pages_per_range: None,
    };
    let noop_bloom = crate::bloom::BloomRegistry::new();
    execute_create_index(stmt, storage, txn, conn_txn, &noop_bloom, database).map(|_| ())
}

/// Drops an index by name for ALTER TABLE DROP INDEX.
fn alter_drop_index(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
        if name == "PRIMARY" {
            i.is_primary
        } else {
            i.name == name
        }
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
