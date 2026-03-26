#[allow(clippy::too_many_arguments)] // 9 params needed: join context has inherent complexity
fn apply_join(
    left_rows: Vec<Row>,
    right_rows: &[Row],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)], // for USING: (col_name, global_col_idx) for left side
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Vec<Row>, DbError> {
    match join_type {
        JoinType::Inner | JoinType::Cross => {
            let mut result = Vec::new();
            for left in &left_rows {
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                    }
                }
            }
            Ok(result)
        }

        JoinType::Left => {
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                    }
                }
                if !matched {
                    result.push(concat_rows(left, &null_right));
                }
            }
            Ok(result)
        }

        JoinType::Right => {
            let null_left: Row = vec![Value::Null; left_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched_right[i] = true;
                    }
                }
            }
            // Emit unmatched right rows with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }
            Ok(result)
        }

        JoinType::Full => {
            // FULL OUTER JOIN = matched pairs + unmatched left rows (NULL right)
            //                 + unmatched right rows (NULL left).
            //
            // A matched-right bitmap tracks which right rows were joined so the
            // second pass can emit the unmatched ones without duplicating them.
            let null_left: Row = vec![Value::Null; left_col_count];
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                let mut matched = false;
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                        matched_right[i] = true;
                    }
                }
                if !matched {
                    // Left row had no match — emit with NULLs on the right side.
                    result.push(concat_rows(left, &null_right));
                }
            }

            // Emit right rows that were never matched with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }

            Ok(result)
        }
    }
}

/// Evaluates a join condition against a combined row.
///
/// - `On(expr)`: evaluates the expression directly (`col_idx` already resolved by analyzer).
/// - `Using(names)`: for each name, finds its index in the left schema and in the right
///   table, then checks equality. NULL = NULL is UNKNOWN (returns false per SQL semantics).
///
/// `left_schema` is a `(column_name, global_col_idx)` list for every column in the
/// accumulated left side of this join stage.
fn eval_join_cond(
    cond: &JoinCondition,
    combined: &[Value],
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<bool, DbError> {
    match cond {
        JoinCondition::On(expr) => Ok(is_truthy(&eval(expr, combined)?)),

        JoinCondition::Using(names) => {
            for col_name in names {
                // Find col_idx in the accumulated left schema.
                let left_idx = left_schema
                    .iter()
                    .find(|(name, _)| name == col_name)
                    .map(|(_, idx)| *idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "left side (USING)".into(),
                    })?;

                // Find col_idx in the right table.
                let right_pos = right_columns
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "right table (USING)".into(),
                    })?;
                let right_idx = right_col_offset + right_pos;

                let left_val = combined
                    .get(left_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: left_idx,
                        len: combined.len(),
                    })?;
                let right_val = combined
                    .get(right_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: right_idx,
                        len: combined.len(),
                    })?;

                // NULL = NULL is UNKNOWN in SQL 3-valued logic — no match.
                if matches!(left_val, Value::Null) || matches!(right_val, Value::Null) {
                    return Ok(false);
                }
                if left_val != right_val {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

/// Concatenates two row slices into a new combined row.
#[inline]
fn concat_rows(left: &[Value], right: &[Value]) -> Row {
    let mut combined = Vec::with_capacity(left.len() + right.len());
    combined.extend_from_slice(left);
    combined.extend_from_slice(right);
    combined
}

/// Builds `ColumnMeta` for the output of a JOIN query.
fn build_join_column_meta(
    items: &[SelectItem],
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> Result<Vec<ColumnMeta>, DbError> {
    // Precompute outer-join nullability once for the whole chain.
    // This correctly handles LEFT, RIGHT, FULL, and mixed chains.
    let nullable_tables = compute_outer_nullable(all_tables.len(), joins);
    let mut out = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // Expand all columns from all tables in order.
                for (t_idx, table) in all_tables.iter().enumerate() {
                    let outer_nullable = nullable_tables[t_idx];
                    for col in &table.columns {
                        out.push(ColumnMeta {
                            name: col.name.clone(),
                            data_type: column_type_to_datatype(col.col_type),
                            nullable: col.nullable || outer_nullable,
                            table_name: Some(table.def.table_name.clone()),
                        });
                    }
                }
            }

            SelectItem::QualifiedWildcard(qualifier) => {
                // Expand only the columns from the matching table.
                let t_idx = all_tables
                    .iter()
                    .position(|t| t.def.table_name == *qualifier || t.def.schema_name == *qualifier)
                    .ok_or_else(|| DbError::TableNotFound {
                        name: qualifier.clone(),
                    })?;
                let table = &all_tables[t_idx];
                let outer_nullable = nullable_tables[t_idx];
                for col in &table.columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable || outer_nullable,
                        table_name: Some(table.def.table_name.clone()),
                    });
                }
            }

            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                // Infer type: plain column reference uses catalog type; others use Text fallback.
                let (dt, nullable) = infer_expr_type_join(expr, all_tables, &nullable_tables);
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

/// Computes per-table outer-join nullability for a join chain.
///
/// Returns a `Vec<bool>` of length `table_count` where `[i]` is `true` if
/// table `i` can be null-extended by any join in the chain:
///
/// - `LEFT JOIN`: the right table becomes nullable.
/// - `RIGHT JOIN`: all accumulated left tables (0..=join_idx) become nullable.
/// - `FULL JOIN`: both sides become nullable.
/// - `INNER` / `CROSS`: no side becomes nullable.
///
/// This replaces the old `is_outer_nullable(t_idx, joins)` helper which only
/// looked at a single join and therefore produced wrong metadata for mixed
/// outer-join chains and for `FULL JOIN`.
fn compute_outer_nullable(table_count: usize, joins: &[JoinClause]) -> Vec<bool> {
    let mut nullable = vec![false; table_count];
    for (join_idx, join) in joins.iter().enumerate() {
        let right_table = join_idx + 1;
        match join.join_type {
            JoinType::Inner | JoinType::Cross => {}
            JoinType::Left => {
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
            JoinType::Right => {
                nullable[..right_table.min(table_count)].fill(true);
            }
            JoinType::Full => {
                nullable[..right_table.min(table_count)].fill(true);
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
        }
    }
    nullable
}

/// Infers (DataType, nullable) for an expression in a JOIN context.
fn infer_expr_type_join(
    expr: &Expr,
    all_tables: &[axiomdb_catalog::ResolvedTable],
    nullable_tables: &[bool],
) -> (DataType, bool) {
    if let Expr::Column { col_idx, .. } = expr {
        // Find which table owns this col_idx and what the column type is.
        let mut offset = 0;
        for (t_idx, table) in all_tables.iter().enumerate() {
            let end = offset + table.columns.len();
            if *col_idx < end {
                let local_pos = col_idx - offset;
                if let Some(col) = table.columns.get(local_pos) {
                    let outer_nullable = nullable_tables.get(t_idx).copied().unwrap_or(false);
                    let nullable = col.nullable || outer_nullable;
                    return (column_type_to_datatype(col.col_type), nullable);
                }
            }
            offset = end;
        }
    }
    (DataType::Text, true) // safe fallback for computed expressions
}
