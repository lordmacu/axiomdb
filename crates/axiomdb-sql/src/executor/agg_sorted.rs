
// ── Sorted streaming aggregation (4.9b) ──────────────────────────────────────

/// Sorted streaming GROUP BY.
///
/// When `presorted = true`, input rows are already in group-key order
/// (guaranteed by the B-Tree access method). Groups are formed by streaming
/// adjacent equal-key rows without building any hash table.
///
/// When `presorted = false`, the input is sorted by group keys first, then
/// streamed. This path is not auto-selected in 4.9b but is available for
/// testing and future use.
fn execute_select_grouped_sorted(
    stmt: SelectStmt,
    mut combined_rows: Vec<Row>,
    presorted: bool,
) -> Result<QueryResult, DbError> {
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Evaluate GROUP BY expressions for every row up front.
    // This avoids re-evaluating the same expressions during boundary detection.
    struct KeyedRow {
        row: Row,
        key_values: Vec<Value>,
    }
    let mut keyed: Vec<KeyedRow> = combined_rows
        .drain(..)
        .map(|row| {
            let key_values: Vec<Value> = stmt
                .group_by
                .exprs()
                .iter()
                .map(|e| eval(e, &row))
                .collect::<Result<_, _>>()?;
            Ok(KeyedRow { row, key_values })
        })
        .collect::<Result<Vec<_>, DbError>>()?;

    if !presorted {
        // Stable sort by group keys — NULL last, same as hash path output order.
        keyed.sort_by(|a, b| compare_group_key_lists(&a.key_values, &b.key_values));
    }

    // Stream adjacent equal groups.
    let mut output_rows: Vec<Row> = Vec::new();

    if keyed.is_empty() {
        // Ungrouped aggregate on empty input: emit one row (e.g., COUNT(*) → 0).
        if stmt.group_by.is_empty() {
            let accumulators: Vec<AggAccumulator> =
                agg_exprs.iter().map(AggAccumulator::new).collect();
            let agg_values: Vec<Value> = accumulators
                .into_iter()
                .map(|acc| acc.finalize())
                .collect::<Result<_, _>>()?;
            let out_row = project_grouped_row(&stmt.columns, &[], &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    } else {
        // Initialize first group.
        let first = &keyed[0];
        let mut current_key = first.key_values.clone();
        let mut representative_row = first.row.clone();
        let mut accumulators: Vec<AggAccumulator> =
            agg_exprs.iter().map(AggAccumulator::new).collect();
        for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(&first.row, agg)?;
        }

        for kr in &keyed[1..] {
            if group_keys_equal(&current_key, &kr.key_values) {
                // Same group — accumulate.
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            } else {
                // Group boundary — drain current accumulators by value, finalize, emit.
                let finished: Vec<AggAccumulator> = std::mem::replace(
                    &mut accumulators,
                    agg_exprs.iter().map(AggAccumulator::new).collect(),
                );
                let agg_values: Vec<Value> = finished
                    .into_iter()
                    .map(|acc| acc.finalize())
                    .collect::<Result<_, _>>()?;
                if let Some(ref having) = stmt.having {
                    let resolved_having = resolve_having_aliases(having.clone(), &stmt.columns);
                    let v = eval_with_aggs(&resolved_having, &representative_row, &agg_values, &agg_exprs)?;
                    if is_truthy(&v) {
                        let out_row = project_grouped_row(
                            &stmt.columns,
                            &representative_row,
                            &agg_values,
                            &agg_exprs,
                        )?;
                        output_rows.push(out_row);
                    }
                } else {
                    let out_row = project_grouped_row(
                        &stmt.columns,
                        &representative_row,
                        &agg_values,
                        &agg_exprs,
                    )?;
                    output_rows.push(out_row);
                }

                // Start next group (accumulators already reset by mem::replace above).
                current_key = kr.key_values.clone();
                representative_row = kr.row.clone();
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            }
        }

        // Finalize the last group.
        let agg_values: Vec<Value> = accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;
        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &representative_row, &agg_values, &agg_exprs)?;
            if is_truthy(&v) {
                let out_row = project_grouped_row(
                    &stmt.columns,
                    &representative_row,
                    &agg_values,
                    &agg_exprs,
                )?;
                output_rows.push(out_row);
            }
        } else {
            let out_row =
                project_grouped_row(&stmt.columns, &representative_row, &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    }

    let mut rows = output_rows;
    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    rows = apply_order_by(rows, &remapped_ob)?;
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Projects one output row for the grouped path.
///
/// For each `SelectItem::Expr`:
/// - If the expression contains an aggregate → `eval_with_aggs`
/// - Otherwise → standard `eval` against `representative_row`
fn project_grouped_row(
    items: &[SelectItem],
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, .. } => {
                let v = if contains_aggregate(expr) {
                    eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?
                } else {
                    eval(expr, representative_row)?
                };
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// Builds `ColumnMeta` for the output of a grouped SELECT.
fn build_grouped_column_meta(
    items: &[SelectItem],
    agg_exprs: &[AggExpr],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| grouped_expr_name(expr, agg_exprs));
                let (dt, nullable) = grouped_expr_type(expr, agg_exprs);
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

/// Returns a display name for a grouped SELECT expression.
fn grouped_expr_name(expr: &Expr, _agg_exprs: &[AggExpr]) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::GroupConcat { .. } => "GROUP_CONCAT(...)".into(),
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            if args.is_empty() {
                format!("{name}(*)")
            } else {
                format!("{name}(...)")
            }
        }
        _ => "?column?".into(),
    }
}

/// Infers `(DataType, nullable)` for a grouped SELECT expression.
/// Aggregate results: COUNT → BigInt non-null; SUM/MIN/MAX/AVG → nullable.
fn grouped_expr_type(expr: &Expr, _agg_exprs: &[AggExpr]) -> (DataType, bool) {
    match expr {
        // GROUP_CONCAT always produces TEXT; nullable (empty group → NULL).
        Expr::GroupConcat { .. } => (DataType::Text, true),
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => match name.as_str() {
            "count" => (DataType::BigInt, false),
            "avg" => (DataType::Real, true),
            _ => (DataType::Text, true), // SUM/MIN/MAX: type depends on column — Text fallback
        },
        Expr::Column { .. } => (DataType::Text, true), // Column refs: safe fallback
        _ => (DataType::Text, true),
    }
}
