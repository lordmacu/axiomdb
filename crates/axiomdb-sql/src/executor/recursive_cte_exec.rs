/// Phase 21.3b — executor for `WITH RECURSIVE` CTEs appearing as the
/// first FROM source. Runs PG-style iteration:
///   1. Execute the analyzed base SELECT once.
///   2. Clone the step, substitute refs to the CTE alias with a
///      `FromClause::Values` literal holding the current working table.
///   3. Re-analyze and execute the modified step.
///   4. Dedup (for plain UNION) against the accumulated result.
///   5. Repeat until the step yields zero new rows or MAX_RECURSION.
fn execute_select_recursive_cte_ctx(
    mut stmt: SelectStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let clause = match stmt.from.take() {
        Some(FromClause::RecursiveCte(c)) => c,
        _ => unreachable!(
            "execute_select_recursive_cte_ctx called with non-RecursiveCte FROM"
        ),
    };

    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

    // 1. Execute base.
    let base_result = execute_select_ctx((*clause.base).clone(), exec_ctx, conn_txn, ctx)?;
    let (_base_cols, mut rt) = match base_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => {
            return Err(DbError::Internal {
                message: "recursive CTE base did not return rows".into(),
            })
        }
    };

    let mut wt: Vec<Row> = rt.clone();
    let mut seen: std::collections::HashSet<Vec<u8>> = if clause.union_all {
        std::collections::HashSet::new()
    } else {
        rt.iter()
            .map(|r| crate::recursive_cte::row_key(r))
            .collect()
    };

    for _depth in 0..crate::recursive_cte::MAX_RECURSION {
        if wt.is_empty() {
            break;
        }

        // 2. Build a step clone with self-refs replaced by Values(wt).
        let cte_col_names: Vec<String> = crate::recursive_cte::column_metas_for_recursive(&clause)
            .into_iter()
            .map(|m| m.name)
            .collect();
        let mut step_clone = (*clause.step).clone();
        substitute_self_ref_with_values(
            &mut step_clone,
            &clause.alias,
            &wt,
            &cte_col_names,
        )?;

        // 3. Re-analyze from scratch (base was already analyzed; step
        //    had a stub binding during expand_ctes but after the Values
        //    swap its column refs must be re-resolved).
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());
        let db = ctx
            .selected_database()
            .unwrap_or(DEFAULT_DATABASE_NAME)
            .to_string();
        let analyzed = crate::analyzer::analyze_with_defaults(
            Stmt::Select(step_clone),
            storage,
            snap,
            &db,
            "public",
        )?;
        let analyzed_step = match analyzed {
            Stmt::Select(s) => s,
            other => {
                return Err(DbError::Internal {
                    message: format!(
                        "recursive CTE step re-analysis yielded non-SELECT: {other:?}"
                    ),
                })
            }
        };

        // 4. Execute step.
        let step_result = execute_select_ctx(analyzed_step, exec_ctx, conn_txn, ctx)?;
        let new_rows = match step_result {
            QueryResult::Rows { rows, .. } => rows,
            _ => {
                return Err(DbError::Internal {
                    message: "recursive CTE step did not return rows".into(),
                })
            }
        };

        // 5. Dedup against accumulated result when !union_all.
        let filtered: Vec<Row> = if clause.union_all {
            new_rows
        } else {
            new_rows
                .into_iter()
                .filter(|r| seen.insert(crate::recursive_cte::row_key(r)))
                .collect()
        };

        if filtered.is_empty() {
            break;
        }

        rt.extend(filtered.clone());
        wt = filtered;

        if _depth == crate::recursive_cte::MAX_RECURSION - 1 {
            return Err(DbError::Other(format!(
                "recursive CTE `{}` exceeded MAX_RECURSION={}",
                clause.alias,
                crate::recursive_cte::MAX_RECURSION,
            )));
        }
    }

    // 6. Apply the outer SELECT's projection / WHERE / GROUP BY / etc.
    //    Model after execute_select_values_source: we already have the
    //    row stream; feed it through the same projection machinery via
    //    a synthetic column-metadata list derived from the CTE's alias.
    let derived_cols = crate::recursive_cte::column_metas_for_recursive(&clause);

    if !stmt.joins.is_empty() {
        let alias = clause.alias.clone();
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            rt,
            exec_ctx,
            conn_txn,
            ctx,
        );
    }

    // No joins: WHERE → ORDER/LIMIT → projection. Mirrors the tail of
    // execute_select_values_source.
    let mut combined_rows: Vec<Row> = Vec::new();
    for row in rt {
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &row)?) {
                continue;
            }
        }
        combined_rows.push(row);
    }

    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    combined_rows = apply_order_by(combined_rows, &resolved_ob)?;

    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

    if stmt.distinct {
        let mut seen_dedup: std::collections::HashSet<Vec<u8>> =
            std::collections::HashSet::new();
        rows.retain(|r| seen_dedup.insert(crate::recursive_cte::row_key(r)));
    }

    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Replace every `FromClause::Table(name==alias)` inside `stmt` (FROM +
/// each join) with a `FromClause::Values` literal containing `wt` rows.
/// Each `Value` is wrapped in `Expr::Literal`.
fn substitute_self_ref_with_values(
    stmt: &mut SelectStmt,
    alias: &str,
    wt: &[Row],
    cte_col_names: &[String],
) -> Result<(), DbError> {
    let replace = |from: FromClause| -> FromClause {
        match from {
            FromClause::Table(tref) if tref.name.eq_ignore_ascii_case(alias) => {
                let rows: Vec<Vec<Expr>> = wt
                    .iter()
                    .map(|r| r.iter().map(|v| Expr::Literal(v.clone())).collect())
                    .collect();
                FromClause::Values(Box::new(crate::ast::ValuesClause {
                    rows,
                    alias: tref.alias.unwrap_or_else(|| alias.to_string()),
                    column_names: Some(cte_col_names.to_vec()),
                }))
            }
            other => other,
        }
    };

    if let Some(from) = stmt.from.take() {
        stmt.from = Some(replace(from));
    }
    for join in &mut stmt.joins {
        let taken = std::mem::replace(
            &mut join.table,
            FromClause::Table(crate::ast::TableRef {
                database: None,
                schema: None,
                name: String::new(),
                alias: None,
            }),
        );
        join.table = replace(taken);
    }
    Ok(())
}
