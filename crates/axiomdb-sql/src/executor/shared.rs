fn resolve_table_cached(
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
    tref: &crate::ast::TableRef,
) -> Result<ResolvedTable, DbError> {
    let database = effective_database_for_ref(tref, ctx);
    let schema_str = tref.schema.as_deref().unwrap_or("public");
    let table_name = &tref.name;
    if let Some(cached) = ctx.get_table(&database, schema_str, table_name) {
        return Ok(cached.clone());
    }
    // If the user explicitly specified a database, verify it exists before
    // attempting table resolution — otherwise we'd return TableNotFound for a
    // database that doesn't exist at all.
    if tref.database.is_some() {
        let snap = txn.active_snapshot()?;
        let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
        if !reader.database_exists(&database)? {
            return Err(DbError::DatabaseNotFound {
                name: database,
            });
        }
    }
    let mut resolver = make_resolver_with_database(storage, txn, &database)?;
    let resolved = resolver.resolve_table(tref.schema.as_deref(), table_name)?;
    ctx.cache_table(&database, schema_str, table_name, resolved.clone());
    Ok(resolved)
}

/// Compute the effective database for a `TableRef`: if the ref has an explicit
/// `database` component, use it; otherwise fall back to the session default.
fn effective_database_for_ref(tref: &crate::ast::TableRef, ctx: &SessionContext) -> String {
    tref.database
        .as_deref()
        .unwrap_or(ctx.effective_database())
        .to_string()
}

// ── ctx-aware DML handlers ────────────────────────────────────────────────────


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
fn collect_column_refs(expr: &Expr, mask: &mut Vec<bool>) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
        Expr::Literal(_) | Expr::OuterColumn { .. } | Expr::Param { .. } => {}
        Expr::UnaryOp { operand, .. } => collect_column_refs(operand, mask),
        Expr::BinaryOp { left, right, .. } => {
            collect_column_refs(left, mask);
            collect_column_refs(right, mask);
        }
        Expr::IsNull { expr, .. } => collect_column_refs(expr, mask),
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
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Routes a statement to its handler. Called both inside `autocommit` and
/// directly when an explicit transaction is already active.
fn make_resolver<'a>(
    storage: &'a mut dyn StorageEngine,
    txn: &TxnManager,
) -> Result<SchemaResolver<'a>, DbError> {
    make_resolver_with_database(storage, txn, DEFAULT_DATABASE_NAME)
}

fn make_resolver_with_database<'a>(
    storage: &'a mut dyn StorageEngine,
    txn: &TxnManager,
    database: &'a str,
) -> Result<SchemaResolver<'a>, DbError> {
    let snap: TransactionSnapshot = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
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
        DataType::Bytes => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid => Ok(ColumnType::Uuid),
        DataType::Decimal => Err(DbError::NotImplemented {
            feature: "DECIMAL column type — Phase 4.3".into(),
        }),
        DataType::Date => Err(DbError::NotImplemented {
            feature: "DATE column type — Phase 4.19".into(),
        }),
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
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
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
        Value::Bytes(_) => DataType::Bytes,
        Value::Date(_) => DataType::Date,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Uuid(_) => DataType::Uuid,
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
                (column_type_to_datatype(col.col_type), col.nullable)
            } else {
                (DataType::Text, true)
            }
        }
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
        _ => "?column?".to_string(),
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
                        data_type: column_type_to_datatype(col.col_type),
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
fn project_row(items: &[SelectItem], values: &[Value]) -> Result<Row, DbError> {
    project_row_with(items, values, &mut crate::eval::NoSubquery)
}

/// Subquery-aware version of [`project_row`].
///
/// Uses `eval_with` so that scalar subqueries in the SELECT list
/// (e.g., `(SELECT COUNT(*) FROM orders WHERE user_id = u.id)`) are executed
/// via `sq`. Performance identical to `project_row` when using [`NoSubquery`]
/// due to monomorphization.
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

/// Compares two non-NULL values using the expression evaluator.
///
/// Delegates to `eval()` via synthetic `Expr::BinaryOp { Lt }` and `Eq`
/// expressions to reuse all existing type coercion and comparison logic.
/// Returns `Equal` if the comparison fails (type mismatch in ORDER BY).
fn compare_non_null_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;

    let lt = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    let eq = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    match (lt, eq) {
        (Ok(lt_v), Ok(eq_v)) => {
            if is_truthy(&lt_v) {
                Less
            } else if is_truthy(&eq_v) {
                Equal
            } else {
                Greater
            }
        }
        // Type mismatch or error: treat as equal (stable, no crash).
        _ => Equal,
    }
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

/// Sorts `rows` in place according to `order_items`.
///
/// Uses `sort_by` (stable) to preserve insertion order for equal keys.
/// Errors from expression evaluation are captured via `sort_err` and
/// returned after the sort completes — `sort_by` cannot return `Result`.
fn apply_order_by(mut rows: Vec<Row>, order_items: &[OrderByItem]) -> Result<Vec<Row>, DbError> {
    if order_items.is_empty() {
        return Ok(rows);
    }
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

// ── Non-unique index key helpers ──────────────────────────────────────────────

/// Returns the lower bound for a non-unique index range scan on `prefix`.
///
/// Non-unique secondary indexes store `encode_index_key(vals) || encode_rid(rid)`
/// so that multiple rows with the same indexed value each get a unique B-Tree key.
/// To find all entries with a given prefix, use `[prefix||0x00..00, prefix||0xFF..FF]`.
fn rid_lo(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(&[0u8; 10]);
    v
}

/// Returns the upper bound for a non-unique index range scan on `prefix`.
fn rid_hi(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(&[0xFFu8; 10]);
    v
}
