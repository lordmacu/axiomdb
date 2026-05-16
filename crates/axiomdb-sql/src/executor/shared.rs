fn resolve_table_cached(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    tref: &crate::ast::TableRef,
) -> Result<ResolvedTable, DbError> {
    let database = effective_database_for_ref(tref, ctx);

    // If the user explicitly specified a database, verify it exists first.
    if tref.database.is_some() {
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());
        let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
        if !reader.database_exists(&database)? {
            return Err(DbError::DatabaseNotFound {
                name: database,
            });
        }
    }

    // Inside an explicit transaction, catalog metadata can change mid-transaction
    // (bulk DELETE, TRUNCATE, DDL, savepoint rollback). Skip the cache so every
    // statement always reads the current catalog state via the active snapshot.
    let in_txn = conn_txn.is_some();

    // If the user specified an explicit schema, resolve directly.
    if let Some(schema) = tref.schema.as_deref() {
        if !in_txn {
            if let Some(cached) = ctx.get_table(&database, schema, &tref.name) {
                return Ok(cached.clone());
            }
        }
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, &database)?;
        let resolved = resolver.resolve_table(Some(schema), &tref.name)?;
        if !in_txn {
            ctx.cache_table(&database, schema, &tref.name, resolved.clone());
        }
        return Ok(resolved);
    }

    // Unqualified name: scan search_path in order until a match is found.
    let search_path: Vec<String> = ctx.search_path.clone();
    for schema in &search_path {
        if !in_txn {
            if let Some(cached) = ctx.get_table(&database, schema, &tref.name) {
                return Ok(cached.clone());
            }
        }
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, &database)?;
        if let Ok(resolved) = resolver.resolve_table(Some(schema), &tref.name) {
            if !in_txn {
                ctx.cache_table(&database, schema, &tref.name, resolved.clone());
            }
            return Ok(resolved);
        }

        // Phase 22b.2: fall back to foreign table catalog when not in regular catalog.
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());
        let mut reader = CatalogReader::new(storage, snap)?;
        if let Some(ftable) = reader.get_foreign_table(schema, &tref.name)? {
            return Ok(fdw_resolved_table(ftable));
        }
    }
    // None of the search_path schemas had the table (regular or foreign).
    Err(DbError::TableNotFound {
        name: tref.name.clone(),
    })
}

/// Builds a synthetic `ResolvedTable` for a foreign table.
///
/// The `TableDef.id` is set to the foreign table ID (`>= FOREIGN_TABLE_ID_BASE`),
/// which signals to the scan layer that it should call the FDW connector rather
/// than `scan_table_any_layout`.
fn fdw_resolved_table(ftable: ForeignTableDef) -> ResolvedTable {
    use axiomdb_catalog::schema::{
        RelationKind, TablePersistence, TableStorageLayout, DEFAULT_DATABASE_NAME,
    };
    let table_id = ftable.table_id;
    let columns: Vec<CatalogColumnDef> = ftable
        .columns
        .iter()
        .enumerate()
        .map(|(i, fc)| CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: fc.name.clone(),
            col_type: fc.col_type,
            nullable: fc.nullable,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
        })
        .collect();
    let def = TableDef {
        id: table_id,
        root_page_id: 0,
        storage_layout: TableStorageLayout::Heap,
        schema_name: ftable.schema_name,
        table_name: ftable.table_name,
        schema_version: 1,
        immutable: false,
        persistence: TablePersistence::Permanent,
        relation_kind: RelationKind::Table,
        defining_query: None,
        default_collation: None,
        triggers: Vec::new(),
    };
    let _ = DEFAULT_DATABASE_NAME; // suppress unused-import warning
    ResolvedTable {
        def,
        columns,
        indexes: Vec::new(),
        constraints: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

/// Compute the effective database for a `TableRef`: if the ref has an explicit
/// `database` component, use it; otherwise fall back to the session default.
fn effective_database_for_ref(tref: &crate::ast::TableRef, ctx: &SessionContext) -> String {
    tref.database
        .as_deref()
        .unwrap_or(ctx.effective_database())
        .to_string()
}

fn owned_exclusion_constraint_name_by_index(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    table_id: u32,
    index_name: &str,
) -> Result<Option<String>, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let Some(index_id) = reader
        .list_indexes(table_id)?
        .into_iter()
        .find(|idx| idx.name == index_name)
        .map(|idx| idx.index_id)
    else {
        return Ok(None);
    };

    Ok(reader
        .list_constraints(table_id)?
        .into_iter()
        .find(|constraint| {
            constraint.kind == axiomdb_catalog::ConstraintKind::Exclusion
                && constraint.owned_index_id == index_id
        })
        .map(|constraint| constraint.name))
}

fn translate_exclusion_violation_ctx(
    err: DbError,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
    table_ref: &crate::ast::TableRef,
) -> DbError {
    let DbError::UniqueViolation { index_name, .. } = &err else {
        return err;
    };
    let Some(conn_txn) = ctx.conn_txn.take() else {
        return err;
    };
    let translated = match resolve_table_cached(
        exec_ctx.storage(),
        exec_ctx.coord(),
        ctx,
        Some(&conn_txn),
        table_ref,
    ) {
        Ok(resolved) => match owned_exclusion_constraint_name_by_index(
            exec_ctx.storage(),
            exec_ctx.coord(),
            &conn_txn,
            resolved.def.id,
            index_name,
        ) {
            Ok(Some(constraint)) => DbError::ExclusionViolation {
                table: resolved.def.table_name,
                constraint,
            },
            _ => err,
        },
        Err(_) => err,
    };
    ctx.conn_txn = Some(conn_txn);
    translated
}

fn translate_exclusion_violation_legacy(
    err: DbError,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    table_ref: &crate::ast::TableRef,
    default_database: &str,
) -> DbError {
    let DbError::UniqueViolation { index_name, .. } = &err else {
        return err;
    };
    let schema = table_ref.schema.as_deref().unwrap_or("public");
    let mut resolver = match make_resolver_with_database(storage, txn, Some(conn_txn), default_database)
    {
        Ok(resolver) => resolver,
        Err(_) => return err,
    };
    let resolved = match resolver.resolve_table(Some(schema), &table_ref.name) {
        Ok(resolved) => resolved,
        Err(_) => return err,
    };
    match owned_exclusion_constraint_name_by_index(
        storage,
        txn,
        conn_txn,
        resolved.def.id,
        index_name,
    ) {
        Ok(Some(constraint)) => DbError::ExclusionViolation {
            table: resolved.def.table_name,
            constraint,
        },
        _ => err,
    }
}

// ── ctx-aware DML handlers ────────────────────────────────────────────────────


#[allow(dead_code)]
fn build_column_mask(n_cols: usize, exprs: &[&Expr]) -> Vec<bool> {
    let mut mask = vec![false; n_cols];
    for expr in exprs {
        collect_column_refs(expr, &mut mask);
    }
    mask
}

/// Walks `expr` and marks every referenced local column index in `mask`.
///
/// Does **not** recurse into subquery bodies (`Subquery`, `InSubquery`,
/// `Exists`) — those reference an inner scope with a different row layout.
/// [`OuterColumn`] references point to an enclosing scope, not this row.
#[allow(dead_code)]
fn collect_column_refs(expr: &Expr, mask: &mut Vec<bool>) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
        Expr::Literal(_)
        | Expr::Default
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::SqlJsonQuery { .. }
        | Expr::Param { .. } => {}
        Expr::UnaryOp { operand, .. } => collect_column_refs(operand, mask),
        Expr::Collate { expr, .. } => collect_column_refs(expr, mask),
        Expr::BinaryOp { left, right, .. } => {
            collect_column_refs(left, mask);
            collect_column_refs(right, mask);
        }
        Expr::IsNull { expr, .. } => collect_column_refs(expr, mask),
        Expr::IsBoolean { expr, .. } => collect_column_refs(expr, mask),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_refs(expr, mask);
            collect_column_refs(low, mask);
            collect_column_refs(high, mask);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, mask);
            collect_column_refs(pattern, mask);
        }
        Expr::In { expr, list, .. } => {
            collect_column_refs(expr, mask);
            for e in list {
                collect_column_refs(e, mask);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_column_refs(a, mask);
            }
        }
        Expr::Window { spec, .. } => {
            for e in &spec.partition_by {
                collect_column_refs(e, mask);
            }
            for item in &spec.order_by {
                collect_column_refs(&item.expr, mask);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                collect_column_refs(op, mask);
            }
            for (w, t) in when_thens {
                collect_column_refs(w, mask);
                collect_column_refs(t, mask);
            }
            if let Some(e) = else_result {
                collect_column_refs(e, mask);
            }
        }
        Expr::Cast { expr, .. } => collect_column_refs(expr, mask),
        // InSubquery: recurse only on the outer expression, not the inner query.
        Expr::InSubquery { expr, .. } => collect_column_refs(expr, mask),
        // Subquery and Exists reference inner scopes — do not recurse.
        Expr::Subquery(_) | Expr::Exists { .. } => {}
        // GroupConcat: recurse into the concatenated expr and ORDER BY exprs.
        Expr::GroupConcat { expr, order_by, .. } => {
            collect_column_refs(expr, mask);
            for (e, _) in order_by {
                collect_column_refs(e, mask);
            }
        }
        // ArrayAgg: recurse into the aggregated expr and ORDER BY exprs.
        Expr::ArrayAgg { expr, order_by, .. } => {
            collect_column_refs(expr, mask);
            for (e, _) in order_by {
                collect_column_refs(e, mask);
            }
        }
        // GROUPING() args may reference columns — recurse.
        Expr::Grouping { args, .. } => {
            for a in args {
                collect_column_refs(a, mask);
            }
        }
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => {
            for e in elements {
                collect_column_refs(e, mask);
            }
        }
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript { array, index, slice } => {
            collect_column_refs(array, mask);
            collect_column_refs(index, mask);
            if let Some(s) = slice {
                collect_column_refs(s, mask);
            }
        }
        // Phase 20.4 — ANY/ALL: recurse into expr (comparison target) and array.
        Expr::AnyOf { expr, array, .. } | Expr::AllOf { expr, array, .. } => {
            collect_column_refs(expr, mask);
            collect_column_refs(array, mask);
        }
        Expr::Row(elems) => {
            for e in elems {
                collect_column_refs(e, mask);
            }
        }
        Expr::FieldAccess { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

fn make_resolver_with_database<'a>(
    storage: &'a dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    database: &'a str,
) -> Result<SchemaResolver<'a>, DbError> {
    let snap: TransactionSnapshot = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());
    SchemaResolver::new(storage, snap, database, "public")
}

/// Converts a SQL [`DataType`] (from the AST) to the compact [`ColumnType`] stored
/// in the catalog. Returns [`DbError::NotImplemented`] for types not yet in the catalog.
fn datatype_to_column_type(dt: &DataType) -> Result<ColumnType, DbError> {
    match dt {
        DataType::Bool => Ok(ColumnType::Bool),
        DataType::Int => Ok(ColumnType::Int),
        DataType::BigInt => Ok(ColumnType::BigInt),
        DataType::Real => Ok(ColumnType::Float),
        DataType::Text => Ok(ColumnType::Text),
        DataType::Json => Ok(ColumnType::Json),
        DataType::Jsonb => Ok(ColumnType::Jsonb),
        DataType::Bytes => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid => Ok(ColumnType::Uuid),
        DataType::Decimal => Ok(ColumnType::Decimal),
        DataType::Date => Ok(ColumnType::Date),
        DataType::Array(_elem) => Ok(ColumnType::Array),
        DataType::Range(_) => Ok(ColumnType::Range),
        DataType::Money => Ok(ColumnType::Money),
        DataType::Composite(_) => Ok(ColumnType::Composite),
    }
}

/// Converts a compact catalog [`ColumnType`] back to the full [`DataType`].
fn column_type_to_datatype(ct: ColumnType) -> DataType {
    match ct {
        ColumnType::Bool => DataType::Bool,
        ColumnType::Int => DataType::Int,
        ColumnType::BigInt => DataType::BigInt,
        ColumnType::Float => DataType::Real,
        ColumnType::Text => DataType::Text,
        ColumnType::Json => DataType::Json,
        ColumnType::Jsonb => DataType::Jsonb,
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
        ColumnType::Decimal => DataType::Decimal,
        ColumnType::Date => DataType::Date,
        ColumnType::Array => DataType::Array(Box::new(DataType::Text)),
        ColumnType::Range => DataType::Range(Box::new(DataType::Int)),
        ColumnType::Money => DataType::Money,
        ColumnType::Composite => DataType::Composite(vec![]),
    }
}

fn catalog_col_to_datatype(col: &CatalogColumnDef) -> DataType {
    if col.col_type == ColumnType::Array {
        let elem_ct = col.array_element_type.unwrap_or(ColumnType::Text);
        DataType::Array(Box::new(column_type_to_datatype(elem_ct)))
    } else {
        column_type_to_datatype(col.col_type)
    }
}

/// Returns the [`DataType`] that best describes a runtime [`Value`].
/// Used for computing `ColumnMeta.data_type` for computed SELECT expressions.
fn datatype_of_value(v: &Value) -> DataType {
    match v {
        Value::Null => DataType::Text, // unknown type — use Text as fallback
        Value::Bool(_) => DataType::Bool,
        Value::Int(_) => DataType::Int,
        Value::BigInt(_) => DataType::BigInt,
        Value::Real(_) => DataType::Real,
        Value::Decimal(..) => DataType::Decimal,
        Value::Text(_) => DataType::Text,
        Value::Json(_) => DataType::Json,
        Value::Jsonb(_) => DataType::Jsonb,
        Value::Bytes(_) => DataType::Bytes,
        Value::Date(_) => DataType::Date,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Uuid(_) => DataType::Uuid,
        Value::Array(elems) => {
            // Infer element type from the first non-null element.
            let elem_dt = elems.iter().find(|e| !matches!(e, Value::Null)).map(datatype_of_value).unwrap_or(DataType::Int);
            DataType::Array(Box::new(elem_dt))
        }
        Value::Range(_) => DataType::Range(Box::new(DataType::Int)),
        Value::Money(..) => DataType::Money,
        Value::Composite(_) => DataType::Composite(vec![]),
    }
}

/// Infers the `(DataType, nullable)` pair for a SELECT expression.
///
/// For plain column references, uses the catalog type. For all other expressions,
/// returns `(DataType::Text, true)` as a safe fallback (proper type inference is Phase 6).
fn infer_expr_type(expr: &Expr, columns: &[CatalogColumnDef]) -> (DataType, bool) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if let Some(col) = columns.get(*col_idx) {
                (catalog_col_to_datatype(col), col.nullable)
            } else {
                (DataType::Text, true)
            }
        }
        Expr::Window { .. } => (DataType::BigInt, false),
        _ => (DataType::Text, true),
    }
}

/// Returns the output name for a SELECT expression item.
fn expr_column_name(expr: &Expr, alias: Option<&str>) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Window { func, .. } => match func {
            crate::expr::WindowFunc::RowNumber => "row_number".into(),
            crate::expr::WindowFunc::Rank => "rank".into(),
            crate::expr::WindowFunc::DenseRank => "dense_rank".into(),
        },
        _ => "?column?".to_string(),
    }
}

/// Expands rows that contain `unnest(array_expr)` SELECT items.
///
/// For each source row, if any SELECT item is an `unnest()` call whose
/// projected value is `Value::Array(elems)`, the row is replaced by
/// `elems.len()` rows — one per element. Multiple unnest columns zip by
/// position (PostgreSQL semantics). `unnest(NULL)` produces zero rows.
pub(super) fn expand_unnest_rows(items: &[SelectItem], rows: Vec<Row>) -> Vec<Row> {
    let unnest_idxs: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            SelectItem::Expr {
                expr: Expr::Function { name, .. },
                ..
            } if name.eq_ignore_ascii_case("unnest") => Some(i),
            _ => None,
        })
        .collect();

    if unnest_idxs.is_empty() {
        return rows;
    }

    let mut result = Vec::new();
    for row in rows {
        let col_idx = unnest_idxs[0];
        if col_idx >= row.len() {
            result.push(row);
            continue;
        }
        match &row[col_idx] {
            Value::Array(elems) => {
                for ei in 0..elems.len() {
                    let mut new_row = row.clone();
                    for &uidx in &unnest_idxs {
                        if uidx >= new_row.len() {
                            break;
                        }
                        new_row[uidx] = match &row[uidx] {
                            Value::Array(a) => a.get(ei).cloned().unwrap_or(Value::Null),
                            other => other.clone(),
                        };
                    }
                    result.push(new_row);
                }
            }
            Value::Null => {} // unnest(NULL) → no rows
            _ => result.push(row),
        }
    }
    result
}

fn select_items_have_window_functions(items: &[SelectItem]) -> bool {
    items.iter().any(|item| matches!(
        item,
        SelectItem::Expr {
            expr: Expr::Window { .. },
            ..
        }
    ))
}

fn project_rows_with_window_support<E>(
    items: &[SelectItem],
    combined_rows: &[Row],
    mut eval_expr: E,
) -> Result<Vec<Row>, DbError>
where
    E: FnMut(&Expr, &[Value]) -> Result<Value, DbError>,
{
    let mut window_values: Vec<Option<Vec<Value>>> = vec![None; items.len()];
    for (idx, item) in items.iter().enumerate() {
        let SelectItem::Expr {
            expr: Expr::Window { func, spec },
            ..
        } = item
        else {
            continue;
        };
        window_values[idx] = Some(compute_window_values(*func, spec, combined_rows, &mut eval_expr)?);
    }

    let mut rows = Vec::with_capacity(combined_rows.len());
    for (row_idx, values) in combined_rows.iter().enumerate() {
        let mut out = Vec::new();
        for (item_idx, item) in items.iter().enumerate() {
            match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => out.extend_from_slice(values),
                SelectItem::Expr { expr, .. } => {
                    if let Some(cache) = &window_values[item_idx] {
                        out.push(cache[row_idx].clone());
                    } else {
                        out.push(eval_expr(expr, values)?);
                    }
                }
            }
        }
        rows.push(out);
    }

    Ok(rows)
}

#[derive(Clone)]
struct WindowEntry {
    row_idx: usize,
    partition_keys: Vec<Value>,
    order_keys: Vec<Value>,
}

fn compute_window_values<E>(
    func: crate::expr::WindowFunc,
    spec: &crate::ast::WindowSpec,
    combined_rows: &[Row],
    eval_expr: &mut E,
) -> Result<Vec<Value>, DbError>
where
    E: FnMut(&Expr, &[Value]) -> Result<Value, DbError>,
{
    let mut entries = Vec::with_capacity(combined_rows.len());
    for (row_idx, row) in combined_rows.iter().enumerate() {
        let mut partition_keys = Vec::with_capacity(spec.partition_by.len());
        for expr in &spec.partition_by {
            partition_keys.push(eval_expr(expr, row)?);
        }
        let mut order_keys = Vec::with_capacity(spec.order_by.len());
        for item in &spec.order_by {
            order_keys.push(eval_expr(&item.expr, row)?);
        }
        entries.push(WindowEntry {
            row_idx,
            partition_keys,
            order_keys,
        });
    }

    entries.sort_by(|a, b| compare_window_entries(a, b, spec));

    let mut out = vec![Value::Null; combined_rows.len()];
    let mut partition_start = 0usize;
    while partition_start < entries.len() {
        let mut partition_end = partition_start + 1;
        while partition_end < entries.len()
            && partition_keys_equal(
                &entries[partition_start].partition_keys,
                &entries[partition_end].partition_keys,
            )
        {
            partition_end += 1;
        }

        let mut dense_rank = 1i64;
        let mut previous_order_keys: Option<&[Value]> = None;
        let mut current_rank = 1i64;
        for (offset, entry) in entries[partition_start..partition_end].iter().enumerate() {
            let row_number = (offset + 1) as i64;
            let order_changed = previous_order_keys
                .map(|prev| !partition_keys_equal(prev, &entry.order_keys))
                .unwrap_or(true);
            if order_changed {
                current_rank = row_number;
                if previous_order_keys.is_some() {
                    dense_rank += 1;
                }
                previous_order_keys = Some(&entry.order_keys);
            }

            let value = match func {
                crate::expr::WindowFunc::RowNumber => row_number,
                crate::expr::WindowFunc::Rank => current_rank,
                crate::expr::WindowFunc::DenseRank => dense_rank,
            };
            out[entry.row_idx] = Value::BigInt(value);
        }

        partition_start = partition_end;
    }

    Ok(out)
}

fn compare_window_entries(
    left: &WindowEntry,
    right: &WindowEntry,
    spec: &crate::ast::WindowSpec,
) -> std::cmp::Ordering {
    for (a, b) in left.partition_keys.iter().zip(&right.partition_keys) {
        let ord = compare_partition_values(a, b);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    for ((a, b), item) in left
        .order_keys
        .iter()
        .zip(&right.order_keys)
        .zip(&spec.order_by)
    {
        let ord = compare_sort_values(a, b, item.order, item.nulls);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    left.row_idx.cmp(&right.row_idx)
}

fn partition_keys_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(a, b)| compare_partition_values(a, b) == std::cmp::Ordering::Equal)
}

fn compare_partition_values(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        _ => compare_non_null_for_sort(left, right),
    }
}

/// Builds the [`ColumnMeta`] vector for the output of a SELECT statement.
fn build_select_column_meta(
    items: &[SelectItem],
    columns: &[CatalogColumnDef],
    table_def: &TableDef,
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for col in columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: catalog_col_to_datatype(col),
                        nullable: col.nullable,
                        table_name: Some(table_def.table_name.clone()),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                let (dt, nullable) = infer_expr_type(expr, columns);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Projects a row through a SELECT item list (no subquery support).
#[allow(dead_code)]
fn project_row(items: &[SelectItem], values: &[Value]) -> Result<Row, DbError> {
    project_row_with(items, values, &mut crate::eval::NoSubquery)
}

/// Subquery-aware version of [`project_row`].
///
/// Uses `eval_with` so that scalar subqueries in the SELECT list
/// (e.g., `(SELECT COUNT(*) FROM orders WHERE user_id = u.id)`) are executed
/// via `sq`. Performance identical to `project_row` when using [`NoSubquery`]
/// due to monomorphization.
#[allow(dead_code)]
fn project_row_with<R: SubqueryRunner>(
    items: &[SelectItem],
    values: &[Value],
    sq: &mut R,
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                out.extend_from_slice(values);
            }
            SelectItem::Expr { expr, .. } => {
                out.push(eval_with(expr, values, sq)?);
            }
        }
    }
    Ok(out)
}

/// Builds output column metadata for a SELECT over a derived table
/// (`FROM (SELECT ...) AS alias`).
///
/// `SELECT *` expands to the derived table's own column metadata.
/// `SELECT expr [AS alias]` uses the alias or the expression name.
fn build_derived_output_columns(
    items: &[SelectItem],
    derived_cols: &[ColumnMeta],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                out.extend_from_slice(derived_cols);
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| expr_column_name(expr, None));
                out.push(ColumnMeta::computed(name, axiomdb_types::DataType::Text));
            }
        }
    }
    Ok(out)
}

// ── ORDER BY / LIMIT helpers ──────────────────────────────────────────────────

/// Compares two values for ORDER BY sorting, correctly handling NULLs.
///
/// ## NULL ordering defaults (PostgreSQL-compatible)
/// - `ASC` with no explicit NULLS → NULLs sort **last** (after non-NULLs)
/// - `DESC` with no explicit NULLS → NULLs sort **first** (before non-NULLs)
///
/// Explicit `NULLS FIRST` or `NULLS LAST` overrides the default.
fn compare_sort_values(
    a: &Value,
    b: &Value,
    direction: SortOrder,
    nulls: Option<NullsOrder>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;

    let nulls_first = match (direction, nulls) {
        (_, Some(NullsOrder::First)) => true,
        (_, Some(NullsOrder::Last)) => false,
        (SortOrder::Asc, None) => false, // default: NULLS LAST for ASC
        (SortOrder::Desc, None) => true, // default: NULLS FIRST for DESC
    };

    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => {
            if nulls_first {
                Less
            } else {
                Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Greater
            } else {
                Less
            }
        }
        (a, b) => {
            let ord = compare_non_null_for_sort(a, b);
            if direction == SortOrder::Desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Compares two non-NULL values directly without heap allocations.
///
/// Delegates to `crate::eval::ops::compare_values` which uses direct
/// pattern-matching on Value variants with proper type coercion.
///
/// Previously this function constructed `Expr::BinaryOp { Lt }` and `Eq`
/// AST nodes per comparison (4 heap allocations × ~130K comparisons for
/// 10K rows). Inspired by PostgreSQL's SortSupport and DuckDB's sort-key
/// approach, both of which avoid per-comparison allocations.
///
/// Returns `Equal` if the comparison fails (type mismatch in ORDER BY).
fn compare_non_null_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    crate::eval::compare_values(a, b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Compares two rows using all ORDER BY items (multi-column composite key).
///
/// Items are applied left-to-right; the first non-Equal result determines
/// the order. Returns `Equal` only when all items produce equal keys.
fn compare_rows_for_sort(
    a: &[Value],
    b: &[Value],
    order_items: &[OrderByItem],
) -> Result<std::cmp::Ordering, DbError> {
    for item in order_items {
        let key_a = eval(&item.expr, a)?;
        let key_b = eval(&item.expr, b)?;
        let ord = compare_sort_values(&key_a, &key_b, item.order, item.nulls);
        if ord != std::cmp::Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

/// Checks whether all ORDER BY expressions are simple column references.
///
/// When all sort keys are plain `Expr::Column { col_idx }`, we can skip the
/// expression evaluator entirely and compare values directly from the row
/// by index. This is the common case for queries like `ORDER BY score DESC`.
///
/// Returns `Some(indices_and_dirs)` if all expressions are simple columns,
/// or `None` if any expression requires evaluation (e.g., `ORDER BY a + b`).
fn try_extract_sort_columns(
    order_items: &[OrderByItem],
) -> Option<Vec<(usize, SortOrder, Option<NullsOrder>)>> {
    let mut cols = Vec::with_capacity(order_items.len());
    for item in order_items {
        match &item.expr {
            Expr::Column { col_idx, .. } => {
                cols.push((*col_idx, item.order, item.nulls));
            }
            _ => return None,
        }
    }
    Some(cols)
}

/// Fast-path row comparator for simple column ORDER BY.
///
/// Avoids the expression evaluator entirely — reads sort keys directly from
/// the row by column index. Inspired by PostgreSQL's SortSupport which
/// pre-computes sort keys to eliminate per-comparison overhead.
///
/// For `ORDER BY score DESC LIMIT 100` on 10K rows, this eliminates ~20K
/// `eval()` dispatch calls, each of which traverses a 30-variant match.
fn compare_rows_by_columns(
    a: &[Value],
    b: &[Value],
    cols: &[(usize, SortOrder, Option<NullsOrder>)],
) -> std::cmp::Ordering {
    for &(idx, direction, nulls) in cols {
        let va = a.get(idx).unwrap_or(&Value::Null);
        let vb = b.get(idx).unwrap_or(&Value::Null);
        let ord = compare_sort_values(va, vb, direction, nulls);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Sorts `rows` in place according to `order_items`.
///
/// Uses `sort_by` (stable) to preserve insertion order for equal keys.
/// Errors from expression evaluation are captured via `sort_err` and
/// returned after the sort completes — `sort_by` cannot return `Result`.
/// Returns true if `expr` is a zero-argument call to RAND or RANDOM.
fn is_rand_call(expr: &Expr) -> bool {
    matches!(expr, Expr::Function { name, args }
        if args.is_empty()
            && matches!(name.to_ascii_lowercase().as_str(), "rand" | "random"))
}

/// Returns true if any ORDER BY item expression is a RAND/RANDOM() call.
fn order_by_has_random(order_items: &[OrderByItem]) -> bool {
    order_items.iter().any(|item| is_rand_call(&item.expr))
}

fn apply_order_by(mut rows: Vec<Row>, order_items: &[OrderByItem]) -> Result<Vec<Row>, DbError> {
    if order_items.is_empty() {
        return Ok(rows);
    }

    // Pure RANDOM() — single ORDER BY item that is a zero-arg RAND()/RANDOM() call.
    // Fisher-Yates shuffle via rand crate: O(n), one key per row (not per comparison).
    if order_items.len() == 1 && is_rand_call(&order_items[0].expr) {
        use rand::seq::SliceRandom;
        rows.shuffle(&mut rand::thread_rng());
        return Ok(rows);
    }

    // Mixed ORDER BY: RANDOM() combined with other sort expressions.
    // Pre-materialize one f64 per RANDOM() position per row before sorting so the
    // comparator sees stable keys (calling RANDOM() inside sort_by would violate
    // comparison transitivity and produce corrupt output).
    if order_by_has_random(order_items) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let random_positions: Vec<usize> = order_items
            .iter()
            .enumerate()
            .filter(|(_, item)| is_rand_call(&item.expr))
            .map(|(i, _)| i)
            .collect();
        let mut rows_with_keys: Vec<(Row, Vec<f64>)> = rows
            .into_iter()
            .map(|row| {
                let keys: Vec<f64> =
                    random_positions.iter().map(|_| rng.gen::<f64>()).collect();
                (row, keys)
            })
            .collect();
        let mut sort_err: Option<DbError> = None;
        rows_with_keys.sort_by(|(row_a, keys_a), (row_b, keys_b)| {
            if sort_err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            let mut rand_key_a = keys_a.iter();
            let mut rand_key_b = keys_b.iter();
            for item in order_items {
                let (key_a, key_b) = if is_rand_call(&item.expr) {
                    (
                        Value::Real(*rand_key_a.next().unwrap()),
                        Value::Real(*rand_key_b.next().unwrap()),
                    )
                } else {
                    match (eval(&item.expr, row_a), eval(&item.expr, row_b)) {
                        (Ok(a), Ok(b)) => (a, b),
                        (Err(e), _) | (_, Err(e)) => {
                            sort_err = Some(e);
                            return std::cmp::Ordering::Equal;
                        }
                    }
                };
                let ord = compare_sort_values(&key_a, &key_b, item.order, item.nulls);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        if let Some(e) = sort_err {
            return Err(e);
        }
        return Ok(rows_with_keys.into_iter().map(|(row, _)| row).collect());
    }

    // Fast path: when all ORDER BY expressions are simple column references,
    // skip the expression evaluator and compare values directly by index.
    if let Some(cols) = try_extract_sort_columns(order_items) {
        rows.sort_by(|a, b| compare_rows_by_columns(a, b, &cols));
        return Ok(rows);
    }
    // Slow path: expressions require eval() per comparison.
    let mut sort_err: Option<DbError> = None;
    rows.sort_by(|a, b| {
        if sort_err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_rows_for_sort(a, b, order_items) {
            Ok(ord) => ord,
            Err(e) => {
                sort_err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }
    Ok(rows)
}

/// Top-N heap sort: keeps only the top `limit + offset` rows in a BinaryHeap,
/// avoiding a full O(n log n) sort when only a small subset is needed.
///
/// Inspired by PostgreSQL's `tuplesort_heap_insert` (tuplesort.c) and DuckDB's
/// `TopN` physical operator which both use a bounded heap to avoid sorting the
/// entire result set.
///
/// Complexity: O(n log k) where k = limit + offset, vs O(n log n) for full sort.
/// For `ORDER BY score DESC LIMIT 100` on 10K rows: ~10K × log(100) ≈ 66K
/// comparisons vs ~10K × log(10K) ≈ 133K, plus dramatically less memory pressure.
fn apply_order_by_top_n(
    rows: Vec<Row>,
    order_items: &[OrderByItem],
    top_n: usize,
) -> Result<Vec<Row>, DbError> {
    use std::cmp::Ordering;

    if order_items.is_empty() || rows.is_empty() || top_n == 0 {
        return Ok(Vec::new());
    }

    // RANDOM() in ORDER BY requires pre-materialized keys — the heap comparator cannot
    // call RANDOM() per comparison (non-transitive). Fall back to full shuffle + truncate.
    if order_by_has_random(order_items) {
        let mut sorted = apply_order_by(rows, order_items)?;
        sorted.truncate(top_n);
        return Ok(sorted);
    }

    let k = top_n.min(rows.len());
    if k >= rows.len() {
        // Need all rows anyway — fall back to full sort.
        return apply_order_by(rows, order_items);
    }

    // Use indices + select_nth_unstable_by (introselect) for O(n) partitioning,
    // then sort only the top-k portion in O(k log k).
    // This is the same strategy PostgreSQL uses for bounded sorts in
    // tuplesort.c when the working set fits in memory.
    let mut indices: Vec<usize> = (0..rows.len()).collect();

    // Fast path: simple column references → zero-eval comparator.
    if let Some(cols) = try_extract_sort_columns(order_items) {
        indices.select_nth_unstable_by(k - 1, |&a, &b| {
            compare_rows_by_columns(&rows[a], &rows[b], &cols)
        });
        let mut top_indices = indices[..k].to_vec();
        top_indices.sort_by(|&a, &b| {
            compare_rows_by_columns(&rows[a], &rows[b], &cols)
        });
        let mut row_slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
        return Ok(top_indices.into_iter().map(|i| row_slots[i].take().unwrap()).collect());
    }

    // Slow path: expressions require eval() per comparison.
    let mut sort_err: Option<DbError> = None;
    indices.select_nth_unstable_by(k - 1, |&a, &b| {
        match compare_rows_for_sort(&rows[a], &rows[b], order_items) {
            Ok(ord) => ord,
            Err(e) => {
                if sort_err.is_none() {
                    sort_err = Some(e);
                }
                Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }

    let mut top_indices = indices[..k].to_vec();
    top_indices.sort_by(|&a, &b| {
        match compare_rows_for_sort(&rows[a], &rows[b], order_items) {
            Ok(ord) => ord,
            Err(e) => {
                if sort_err.is_none() {
                    sort_err = Some(e);
                }
                Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }

    // Collect the sorted top-k rows by index.
    // We need to move rows out without double-moving. Convert to Option first.
    let mut row_slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    let result: Vec<Row> = top_indices
        .into_iter()
        .map(|i| row_slots[i].take().unwrap())
        .collect();

    Ok(result)
}

/// Evaluates `limit` and `offset` expressions to a usize, if present.
fn eval_limit_offset_usize(
    limit: &Option<Expr>,
    offset: &Option<Expr>,
) -> Result<(Option<usize>, usize), DbError> {
    let offset_n = offset
        .as_ref()
        .map(eval_row_count_as_usize)
        .transpose()?
        .unwrap_or(0);
    let limit_n = limit.as_ref().map(eval_row_count_as_usize).transpose()?;
    Ok((limit_n, offset_n))
}

/// Remaps ORDER BY expressions so they can be evaluated against grouped output rows.
///
/// Grouped output rows are indexed by SELECT output position (0 = first SELECT item,
/// 1 = second, ...).  ORDER BY expressions, however, reference the *source* schema:
/// `Expr::Column { col_idx }` means "column col_idx in the original table row".
///
/// This function rewrites every sub-expression that structurally matches a SELECT
/// item with `Expr::Column { col_idx: output_pos }`, so that `apply_order_by` can
/// index into the projected output row correctly.  Handles both plain column
/// references and aggregate expressions (COUNT(*), SUM(col), GROUP_CONCAT, …).
///
/// Expressions that match no SELECT item are left unchanged (they will produce
/// errors or Null at evaluation time, which is the correct behavior for
/// semantically invalid ORDER BY in GROUP BY context).
fn remap_order_by_for_grouped(
    order_by: &[crate::ast::OrderByItem],
    select_items: &[SelectItem],
) -> Vec<crate::ast::OrderByItem> {
    order_by
        .iter()
        .map(|item| crate::ast::OrderByItem {
            expr: remap_expr_for_grouped(&item.expr, select_items),
            order: item.order,
            nulls: item.nulls,
        })
        .collect()
}

/// Recursively rewrites `expr` for grouped output row evaluation.
///
/// - If `expr` structurally matches a SELECT item at output position `pos`,
///   returns `Expr::Column { col_idx: pos, … }`.
/// - Otherwise recurses into compound expressions (BinaryOp, UnaryOp, etc.)
///   so that `ORDER BY col + 1` is also handled when `col` is in the SELECT.
fn remap_expr_for_grouped(expr: &Expr, select_items: &[SelectItem]) -> Expr {
    // Direct match against a SELECT item.
    for (pos, item) in select_items.iter().enumerate() {
        if let SelectItem::Expr { expr: sel_expr, .. } = item {
            if expr == sel_expr {
                return Expr::Column {
                    col_idx: pos,
                    name: format!("_out{pos}"),
                };
            }
        }
    }
    // Recurse into compound expressions.
    match expr.clone() {
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(remap_expr_for_grouped(&left, select_items)),
            right: Box::new(remap_expr_for_grouped(&right, select_items)),
        },
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(remap_expr_for_grouped(&operand, select_items)),
        },
        Expr::IsNull {
            expr: inner,
            negated,
        } => Expr::IsNull {
            expr: Box::new(remap_expr_for_grouped(&inner, select_items)),
            negated,
        },
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(remap_expr_for_grouped(&inner, select_items)),
            low: Box::new(remap_expr_for_grouped(&low, select_items)),
            high: Box::new(remap_expr_for_grouped(&high, select_items)),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args
                .iter()
                .map(|a| remap_expr_for_grouped(a, select_items))
                .collect(),
        },
        other => other,
    }
}

/// Resolves positional `ORDER BY` references (integer literals) to the
/// corresponding SELECT item expressions (4.10e / G8).
///
/// `ORDER BY 1` → expression of the 1st SELECT item (1-indexed).
/// Non-integer or out-of-range positions are left unchanged.
fn resolve_positional_order_by(
    order_by: &[crate::ast::OrderByItem],
    select_items: &[SelectItem],
) -> Vec<crate::ast::OrderByItem> {
    order_by
        .iter()
        .map(|item| {
            let resolved_expr = match &item.expr {
                Expr::Literal(Value::Int(n)) if *n >= 1 => {
                    let idx = (*n as usize) - 1;
                    if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                        expr.clone()
                    } else {
                        item.expr.clone()
                    }
                }
                Expr::Literal(Value::BigInt(n)) if *n >= 1 => {
                    let idx = (*n as usize) - 1;
                    if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                        expr.clone()
                    } else {
                        item.expr.clone()
                    }
                }
                _ => item.expr.clone(),
            };
            crate::ast::OrderByItem {
                expr: resolved_expr,
                order: item.order,
                nulls: item.nulls,
            }
        })
        .collect()
}

/// Resolves positional `GROUP BY` references (integer literals) to the
/// corresponding SELECT item expressions (4.10e / G8).
///
/// `GROUP BY 1` → expression of the 1st SELECT item (1-indexed).
/// Non-integer or out-of-range positions are left unchanged.
fn resolve_positional_group_by(exprs: &[Expr], select_items: &[SelectItem]) -> Vec<Expr> {
    exprs
        .iter()
        .map(|e| match e {
            Expr::Literal(Value::Int(n)) if *n >= 1 => {
                let idx = (*n as usize) - 1;
                if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                    expr.clone()
                } else {
                    e.clone()
                }
            }
            Expr::Literal(Value::BigInt(n)) if *n >= 1 => {
                let idx = (*n as usize) - 1;
                if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                    expr.clone()
                } else {
                    e.clone()
                }
            }
            _ => e.clone(),
        })
        .collect()
}

/// Evaluates a LIMIT or OFFSET expression as a non-negative `usize`.
///
/// Accepted value types and their contracts:
/// - `Int(n)`    where `n >= 0`  → `n as usize`
/// - `BigInt(n)` where `n >= 0`  → `usize::try_from(n)` (errors on overflow)
/// - `Text(s)`   where `s.trim()` is an exact base-10 integer `>= 0`  → parsed
///
/// Everything else — negatives, non-integral text, NULL, REAL, BOOL, etc. —
/// returns `DbError::TypeMismatch`.
///
/// This function is the single enforcement point for LIMIT/OFFSET row-count
/// coercion for both the cached-AST prepared-statement path and the
/// SQL-string substitution fallback path.
fn eval_row_count_as_usize(expr: &Expr) -> Result<usize, DbError> {
    fn mismatch(expected: &str, got: &str) -> DbError {
        DbError::TypeMismatch {
            expected: expected.into(),
            got: got.into(),
        }
    }

    match eval(expr, &[])? {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        Value::Int(_) => Err(mismatch(
            "non-negative integer for LIMIT/OFFSET",
            "negative integer",
        )),
        Value::BigInt(n) if n >= 0 => usize::try_from(n).map_err(|_| {
            mismatch(
                "non-negative integer for LIMIT/OFFSET",
                "integer too large for this platform",
            )
        }),
        Value::BigInt(_) => Err(mismatch(
            "non-negative integer for LIMIT/OFFSET",
            "negative integer",
        )),
        Value::Text(s) => {
            let trimmed = s.trim();
            let parsed = trimmed.parse::<i64>().map_err(|_| {
                mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    &format!("non-integral text: {trimmed:?}"),
                )
            })?;
            if parsed < 0 {
                return Err(mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    "negative integer",
                ));
            }
            usize::try_from(parsed).map_err(|_| {
                mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    "integer too large for this platform",
                )
            })
        }
        other => Err(mismatch("integer for LIMIT/OFFSET", other.variant_name())),
    }
}

/// Applies LIMIT and OFFSET to a row vector.
///
/// `skip(offset).take(limit)` — LIMIT is applied after ORDER BY and after
/// OFFSET. Passing `limit = None` returns all remaining rows.
fn apply_limit_offset(
    rows: Vec<Row>,
    limit: &Option<Expr>,
    offset: &Option<Expr>,
) -> Result<Vec<Row>, DbError> {
    let offset_n = offset
        .as_ref()
        .map(eval_row_count_as_usize)
        .transpose()?
        .unwrap_or(0);
    let limit_n = limit.as_ref().map(eval_row_count_as_usize).transpose()?;
    Ok(rows
        .into_iter()
        .skip(offset_n)
        .take(limit_n.unwrap_or(usize::MAX))
        .collect())
}

// ── Secondary-index logical-prefix helpers ───────────────────────────────────

/// Returns the lower bound for a secondary-index scan on a logical-key prefix.
///
/// All heap secondary entries begin with the logical encoded key, then may add
/// INCLUDE payload bytes and/or a RID suffix depending on the index format.
fn rid_lo(prefix: &[u8]) -> Vec<u8> {
    prefix.to_vec()
}

/// Returns the upper bound for a secondary-index scan on a logical-key prefix.
fn rid_hi(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    if v.len() < crate::key_encoding::MAX_INDEX_KEY {
        v.resize(crate::key_encoding::MAX_INDEX_KEY, 0xFF);
    } else {
        v.push(0xFF);
    }
    v
}

fn validate_enum_row_values(
    values: &[Value],
    columns: &[CatalogColumnDef],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
) -> Result<(), DbError> {
    for (value, column) in values.iter().zip(columns.iter()) {
        validate_enum_column_value(value, column, storage, txn, conn_txn)?;
    }
    Ok(())
}

fn validate_enum_sparse_values(
    values: &[(usize, Value)],
    columns: &[CatalogColumnDef],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
) -> Result<(), DbError> {
    for (col_pos, value) in values {
        let Some(column) = columns.get(*col_pos) else {
            continue;
        };
        validate_enum_column_value(value, column, storage, txn, conn_txn)?;
    }
    Ok(())
}

fn validate_enum_column_value(
    value: &Value,
    column: &CatalogColumnDef,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
) -> Result<(), DbError> {
    // Composite columns reuse enum_type_name to store the qualified type name
    // but are not enum columns — skip enum label validation for them.
    if column.col_type == axiomdb_catalog::ColumnType::Composite {
        return Ok(());
    }
    let Some(type_name) = column.enum_type_name.as_deref() else {
        return Ok(());
    };
    if matches!(value, Value::Null) {
        return Ok(());
    }
    let Value::Text(label) = value else {
        return Err(DbError::InvalidValue {
            reason: format!(
                "invalid value for enum column '{}': expected text label",
                column.name
            ),
        });
    };
    let (schema, name) = type_name
        .split_once('.')
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("invalid enum type metadata '{type_name}'"),
        })?;
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let enum_type = reader
        .get_enum_type(schema, name)?
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("enum type '{type_name}' does not exist"),
        })?;
    if enum_type.labels.iter().any(|allowed| allowed == label) {
        return Ok(());
    }
    Err(DbError::InvalidValue {
        reason: format!(
            "invalid input value '{}' for enum type '{}'",
            label, type_name
        ),
    })
}
