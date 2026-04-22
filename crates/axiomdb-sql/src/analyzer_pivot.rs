fn is_supported_pivot_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "min"
            | "max"
            | "avg"
            | "count_distinct"
            | "sum_distinct"
            | "avg_distinct"
            | "jsonb_agg"
            | "json_agg"
            | "json_arrayagg"
    )
}

fn pivot_result_alias(pivot: &crate::ast::PivotClause) -> String {
    if let Some(alias) = &pivot.alias {
        return alias.clone();
    }
    match pivot.source.as_ref() {
        FromClause::Table(table_ref) => table_ref
            .alias
            .clone()
            .unwrap_or_else(|| table_ref.name.clone()),
        FromClause::Subquery { alias, .. } => alias.clone(),
        FromClause::JsonTable(jt) => jt.alias.clone().unwrap_or_else(|| "json_table".into()),
        FromClause::JsonbSrf(srf) => crate::jsonb_srf::srf_alias(srf),
        FromClause::Values(vc) => vc.alias.clone(),
        FromClause::RecursiveCte(rc) => rc.alias.clone(),
        FromClause::Pivot(_) => "__pivot".into(),
    }
}

fn pivot_source_lateral(source: &FromClause) -> bool {
    matches!(source, FromClause::Subquery { lateral: true, .. })
}

fn pivot_output_name(value_expr: &Expr) -> Result<String, DbError> {
    match value_expr {
        Expr::Literal(value) => Ok(value.to_string()),
        _ => Err(DbError::ParseError {
            message: "PIVOT IN values must be literals".into(),
            position: None,
        }),
    }
}

fn build_pivot_virtual_columns(
    pivot: &crate::ast::PivotClause,
    source_cols: &[ColumnDef],
    pivot_expr: &Expr,
    aggregate_arg: &Expr,
) -> Result<Vec<ColumnDef>, DbError> {
    let mut referenced: std::collections::HashSet<usize> = std::collections::HashSet::new();
    referenced.extend(crate::partial_index::collect_column_indices(pivot_expr));
    referenced.extend(crate::partial_index::collect_column_indices(aggregate_arg));

    let mut out = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, col) in source_cols.iter().enumerate() {
        if referenced.contains(&idx) {
            continue;
        }
        let mut cloned = col.clone();
        cloned.col_idx = out.len() as u16;
        seen_names.insert(col.name.to_ascii_lowercase());
        out.push(cloned);
    }

    for value in &pivot.values {
        let name = pivot_output_name(value)?;
        let key = name.to_ascii_lowercase();
        if !seen_names.insert(key) {
            return Err(DbError::Other(format!(
                "duplicate PIVOT output column `{name}`"
            )));
        }
        out.push(ColumnDef {
            table_id: 0,
            col_idx: (out.len()) as u16,
            name,
            col_type: axiomdb_catalog::schema::ColumnType::Text,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            generated_stored: false,
        });
    }

    Ok(out)
}

fn lower_pivot_clause_to_subquery(
    pivot: crate::ast::PivotClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
) -> Result<FromClause, DbError> {
    if !is_supported_pivot_aggregate(&pivot.aggregate_name) {
        return Err(DbError::NotImplemented {
            feature: format!("PIVOT aggregate `{}`", pivot.aggregate_name),
        });
    }

    let mut source_col_offset = 0usize;
    let source_bounds = bound_from_clause(
        &pivot.source,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
        &mut source_col_offset,
        outer_scopes,
    )?;
    if source_bounds.len() != 1 {
        return Err(DbError::Internal {
            message: "PIVOT source must bind to exactly one table shape".into(),
        });
    }
    let source_ctx = BindContext {
        tables: source_bounds,
    };
    let source_cols = source_ctx.tables[0].columns.clone();

    let state = AnalyzeState {
        storage,
        snapshot: snapshot.clone(),
        default_database,
        default_schema,
    };
    let resolved_aggregate_arg = resolve_expr_full(
        pivot.aggregate_arg.clone(),
        &source_ctx,
        outer_scopes,
        Some(&state),
    )?;
    let resolved_pivot_expr =
        resolve_expr_full(pivot.pivot_expr.clone(), &source_ctx, outer_scopes, Some(&state))?;
    let pivot_virtual_cols = build_pivot_virtual_columns(
        &pivot,
        &source_cols,
        &resolved_pivot_expr,
        &resolved_aggregate_arg,
    )?;

    let referenced: std::collections::HashSet<usize> = crate::partial_index::collect_column_indices(
        &resolved_pivot_expr,
    )
    .into_iter()
    .chain(crate::partial_index::collect_column_indices(&resolved_aggregate_arg))
    .collect();

    let passthrough_exprs: Vec<Expr> = source_cols
        .iter()
        .enumerate()
        .filter(|(idx, _)| !referenced.contains(idx))
        .map(|(_, col)| Expr::Column {
            col_idx: 0,
            name: col.name.clone(),
        })
        .collect();

    let mut columns = Vec::with_capacity(pivot_virtual_cols.len());
    for expr in &passthrough_exprs {
        let name = match expr {
            Expr::Column { name, .. } => split_name(name).1.to_string(),
            _ => unreachable!(),
        };
        columns.push(SelectItem::Expr {
            expr: expr.clone(),
            alias: Some(name),
        });
    }

    for value in &pivot.values {
        let Expr::Literal(literal_value) = value.clone() else {
            return Err(DbError::ParseError {
                message: "PIVOT IN values must be literals".into(),
                position: None,
            });
        };
        let pivot_col_name = pivot_output_name(value)?;
        let case_expr = Expr::Case {
            operand: None,
            when_thens: vec![(
                Expr::BinaryOp {
                    op: crate::expr::BinaryOp::Eq,
                    left: Box::new(resolved_pivot_expr.clone()),
                    right: Box::new(Expr::Literal(literal_value)),
                },
                resolved_aggregate_arg.clone(),
            )],
            else_result: Some(Box::new(Expr::Literal(axiomdb_types::Value::Null))),
        };
        columns.push(SelectItem::Expr {
            expr: Expr::Function {
                name: pivot.aggregate_name.clone(),
                args: vec![case_expr],
            },
            alias: Some(pivot_col_name),
        });
    }

    let rewritten = SelectStmt {
        with_ctes: vec![],
        distinct: false,
        distinct_on: vec![],
        hints: vec![],
        calc_found_rows: false,
        columns,
        from: Some(*pivot.source.clone()),
        joins: vec![],
        where_clause: None,
        group_by: if passthrough_exprs.is_empty() {
            GroupByClause::None
        } else {
            GroupByClause::Simple(passthrough_exprs)
        },
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        lock_mode: None,
        set_op_rest: vec![],
    };

    let analyzed = analyze_select_with_outer(
        rewritten,
        storage,
        snapshot,
        default_database,
        default_schema,
        outer_scopes,
    )?;

    Ok(FromClause::Subquery {
        query: Box::new(analyzed),
        alias: pivot_result_alias(&pivot),
        lateral: pivot_source_lateral(&pivot.source),
    })
}
