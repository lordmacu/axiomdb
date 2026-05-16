// ── INSERT ────────────────────────────────────────────────────────────────────

fn analyze_insert(
    mut s: InsertStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<InsertStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    // Validate named column list if provided.
    if let Some(ref col_names) = s.columns {
        for col_name in col_names {
            if !columns.iter().any(|c| &c.name == col_name) {
                let available = columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: format!("\"{}\" (available: {})", s.table.name, available),
                });
            }
        }
    }

    // Analyze SELECT source if present.
    if let InsertSource::Select(ref select) = s.source {
        let analyzed = analyze_select(
            *select.clone(),
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
        )?;
        s.source = InsertSource::Select(Box::new(analyzed));
    }

    let target_ctx = BindContext {
        tables: vec![BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns: columns.clone(),
            col_offset: 0,
        }],
    };

    if let Some(mut clause) = s.on_conflict.take() {
        let mut target_col_idxs = Vec::with_capacity(clause.target_columns.len());
        for col_name in &clause.target_columns {
            let Some(col) = columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(col_name))
            else {
                return Err(DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: s.table.name.clone(),
                });
            };
            target_col_idxs.push(col.col_idx);
        }

        if !target_col_idxs.is_empty() {
            let indexes = reader.list_indexes(table_def.id)?;
            let has_match = indexes.iter().any(|idx| {
                (idx.is_primary || idx.is_unique)
                    && idx.columns.len() == target_col_idxs.len()
                    && idx.columns.iter().all(|ic| {
                        ic.expr.is_none() && target_col_idxs.contains(&ic.col_idx)
                    })
            });
            if !has_match {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "ON CONFLICT target ({}) has no matching unique constraint or index",
                        clause.target_columns.join(", ")
                    ),
                });
            }
        }

        if let OnConflictAction::DoUpdate {
            assignments,
            where_clause,
        } = clause.action
        {
            let mut resolved_assignments = Vec::with_capacity(assignments.len());
            for assignment in assignments {
                if !columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&assignment.column))
                {
                    return Err(DbError::ColumnNotFound {
                        name: assignment.column,
                        table: s.table.name.clone(),
                    });
                }
                resolved_assignments.push(Assignment {
                    column: assignment.column,
                    value: resolve_expr(assignment.value, &target_ctx)?,
                });
            }
            let where_clause = where_clause
                .map(|expr| resolve_expr(expr, &target_ctx))
                .transpose()?;
            clause.action = OnConflictAction::DoUpdate {
                assignments: resolved_assignments,
                where_clause,
            };
        }

        s.on_conflict = Some(clause);
    }

    // Phase 21.4 — resolve RETURNING projection against the target table's scope.
    if !s.returning.is_empty() {
        let mut resolved = Vec::with_capacity(s.returning.len());
        for item in std::mem::take(&mut s.returning) {
            resolved.push(match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
                SelectItem::Expr { expr, alias } => SelectItem::Expr {
                    expr: resolve_expr(expr, &target_ctx)?,
                    alias,
                },
            });
        }
        s.returning = resolved;
    }

    Ok(s)
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn analyze_update(
    mut s: UpdateStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<UpdateStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    let ctx = if s.joins.is_empty() {
        let bound = BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns: columns.clone(),
            col_offset: 0,
        };
        BindContext {
            tables: vec![bound],
        }
    } else {
        build_context(
            &Some(FromClause::Table(s.table.clone())),
            &s.joins,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            &[],
        )?
    };

    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for mut join in s.joins {
        // Phase 21.9: resolve LATERAL subquery using the combined DML context
        // as an outer scope so that inner references (e.g. t.id) become
        // OuterColumn nodes, enabling per-outer-row substitution at execute time.
        if let FromClause::Subquery { query, alias, lateral } = join.table {
            if lateral {
                let analyzed_inner = analyze_select_with_outer(
                    *query,
                    storage,
                    snapshot.clone(),
                    default_database,
                    default_schema,
                    &[&ctx],
                )?;
                join.table = FromClause::Subquery {
                    query: Box::new(analyzed_inner),
                    alias,
                    lateral,
                };
            } else {
                join.table = FromClause::Subquery { query, alias, lateral };
            }
        }
        // Phase 11.20d4: resolve JSON_TABLE doc + PASSING against the combined
        // UPDATE scope so correlated doc / PASSING bind to the target table's
        // columns.
        if let FromClause::JsonTable(jt) = &mut join.table {
            let taken_doc =
                std::mem::replace(&mut jt.doc, Expr::Literal(axiomdb_types::Value::Null));
            jt.doc = resolve_expr(taken_doc, &ctx)?;
            for (expr, _name) in &mut jt.passing {
                let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                *expr = resolve_expr(taken, &ctx)?;
            }
        }
        // Phase 11.25a: resolve JSONB SRF doc for UPDATE join sources.
        if let FromClause::JsonbSrf(srf) = &mut join.table {
            let taken = std::mem::replace(&mut srf.doc, Expr::Literal(axiomdb_types::Value::Null));
            srf.doc = resolve_expr(taken, &ctx)?;
        }
        // Phase 21.22: resolve VALUES row exprs (no correlation) for UPDATE
        // join sources.
        if let FromClause::Values(vc) = &mut join.table {
            let empty_ctx = BindContext::empty();
            for row in &mut vc.rows {
                for e in row {
                    let taken = std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                    *e = resolve_expr(taken, &empty_ctx)?;
                }
            }
        }
        join.condition = match join.condition {
            JoinCondition::On(expr) => JoinCondition::On(resolve_expr(expr, &ctx)?),
            JoinCondition::Using(cols) => JoinCondition::Using(cols),
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    // Validate and resolve SET assignments.
    let mut resolved = Vec::with_capacity(s.assignments.len());
    for Assignment { column, value } in s.assignments {
        if !columns.iter().any(|c| c.name == column) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: column.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
        let value = resolve_expr(value, &ctx)?;
        resolved.push(Assignment { column, value });
    }
    s.assignments = resolved;

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;

    // Resolve ORDER BY expressions so column references have correct col_idx.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr(item.expr, &ctx)?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr(s.limit, &ctx)?;

    // Phase 21.4 — resolve RETURNING against the target scope.
    let mut resolved_returning = Vec::with_capacity(s.returning.len());
    for item in std::mem::take(&mut s.returning) {
        resolved_returning.push(match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr(expr, &ctx)?,
                alias,
            },
        });
    }
    s.returning = resolved_returning;

    Ok(s)
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn analyze_delete(
    mut s: DeleteStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DeleteStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;
    let ctx = if s.joins.is_empty() {
        let bound = BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns,
            col_offset: 0,
        };
        BindContext {
            tables: vec![bound],
        }
    } else {
        build_context(
            &Some(FromClause::Table(s.table.clone())),
            &s.joins,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            &[],
        )?
    };

    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for mut join in s.joins {
        // Phase 21.9: resolve LATERAL subquery for DELETE joins — same as UPDATE.
        if let FromClause::Subquery { query, alias, lateral } = join.table {
            if lateral {
                let analyzed_inner = analyze_select_with_outer(
                    *query,
                    storage,
                    snapshot.clone(),
                    default_database,
                    default_schema,
                    &[&ctx],
                )?;
                join.table = FromClause::Subquery {
                    query: Box::new(analyzed_inner),
                    alias,
                    lateral,
                };
            } else {
                join.table = FromClause::Subquery { query, alias, lateral };
            }
        }
        // Phase 11.20d4: resolve JSON_TABLE doc + PASSING for DELETE joins.
        if let FromClause::JsonTable(jt) = &mut join.table {
            let taken_doc =
                std::mem::replace(&mut jt.doc, Expr::Literal(axiomdb_types::Value::Null));
            jt.doc = resolve_expr(taken_doc, &ctx)?;
            for (expr, _name) in &mut jt.passing {
                let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                *expr = resolve_expr(taken, &ctx)?;
            }
        }
        if let FromClause::JsonbSrf(srf) = &mut join.table {
            let taken = std::mem::replace(&mut srf.doc, Expr::Literal(axiomdb_types::Value::Null));
            srf.doc = resolve_expr(taken, &ctx)?;
        }
        // Phase 21.22: VALUES rows for DELETE joins.
        if let FromClause::Values(vc) = &mut join.table {
            let empty_ctx = BindContext::empty();
            for row in &mut vc.rows {
                for e in row {
                    let taken = std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                    *e = resolve_expr(taken, &empty_ctx)?;
                }
            }
        }
        join.condition = match join.condition {
            JoinCondition::On(expr) => JoinCondition::On(resolve_expr(expr, &ctx)?),
            JoinCondition::Using(cols) => JoinCondition::Using(cols),
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;

    // Resolve ORDER BY expressions so column references have correct col_idx.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr(item.expr, &ctx)?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr(s.limit, &ctx)?;

    // Phase 21.4 — resolve RETURNING projection items against the target.
    let mut resolved_returning = Vec::with_capacity(s.returning.len());
    for item in s.returning {
        resolved_returning.push(match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr(expr, &ctx)?,
                alias,
            },
        });
    }
    s.returning = resolved_returning;

    Ok(s)
}

// ── MERGE ────────────────────────────────────────────────────────────────────

fn analyze_merge(
    mut s: MergeStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<MergeStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.target, default_database, default_schema)?;
    let target_columns = reader.list_columns(table_def.id)?;

    s.source = analyze_merge_source(
        s.source,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
    )?;

    let mut col_offset = target_columns.len();
    let mut tables = vec![BoundTable {
        alias: s.target.alias.clone(),
        name: s.target.name.clone(),
        columns: target_columns.clone(),
        col_offset: 0,
    }];
    tables.extend(bound_from_clause(
        &s.source,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
        &mut col_offset,
        &[],
    )?);
    let ctx = BindContext { tables };

    let state = AnalyzeState {
        storage,
        snapshot,
        default_database,
        default_schema,
    };

    s.on = resolve_expr_full(s.on, &ctx, &[], Some(&state))?;

    let mut resolved_actions = Vec::with_capacity(s.actions.len());
    for mut action in s.actions {
        action.guard = resolve_opt_expr_full(action.guard, &ctx, &[], Some(&state))?;
        action.kind = match action.kind {
            MergeActionKind::Update(assignments) => {
                let mut resolved = Vec::with_capacity(assignments.len());
                for Assignment { column, value } in assignments {
                    if !target_columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&column))
                    {
                        let available = target_columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(DbError::ColumnNotFound {
                            name: column.clone(),
                            table: format!("\"{}\" (available: {})", s.target.name, available),
                        });
                    }
                    resolved.push(Assignment {
                        column,
                        value: resolve_expr_full(value, &ctx, &[], Some(&state))?,
                    });
                }
                MergeActionKind::Update(resolved)
            }
            MergeActionKind::Insert { columns, values } => {
                let expected_len = if let Some(ref cols) = columns {
                    for col_name in cols {
                        if !target_columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(col_name))
                        {
                            let available = target_columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(DbError::ColumnNotFound {
                                name: col_name.clone(),
                                table: format!(
                                    "\"{}\" (available: {})",
                                    s.target.name, available
                                ),
                            });
                        }
                    }
                    cols.len()
                } else {
                    target_columns.len()
                };
                if values.len() != expected_len {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "MERGE INSERT has {} value(s) for {} target column(s)",
                            values.len(),
                            expected_len
                        ),
                    });
                }
                let values = values
                    .into_iter()
                    .map(|value| resolve_expr_full(value, &ctx, &[], Some(&state)))
                    .collect::<Result<Vec<_>, _>>()?;
                MergeActionKind::Insert { columns, values }
            }
            MergeActionKind::Delete => MergeActionKind::Delete,
            MergeActionKind::DoNothing => MergeActionKind::DoNothing,
        };
        resolved_actions.push(action);
    }
    s.actions = resolved_actions;

    Ok(s)
}

fn analyze_merge_source(
    source: FromClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<FromClause, DbError> {
    let state = AnalyzeState {
        storage,
        snapshot: snapshot.clone(),
        default_database,
        default_schema,
    };
    let empty_ctx = BindContext::empty();

    match source {
        FromClause::Subquery {
            query,
            alias,
            lateral,
        } => {
            let analyzed =
                analyze_select(*query, storage, snapshot, default_database, default_schema)?;
            Ok(FromClause::Subquery {
                query: Box::new(analyzed),
                alias,
                lateral,
            })
        }
        FromClause::JsonTable(jt) => {
            let resolved = resolve_json_table(*jt, &empty_ctx, &[], &state)?;
            Ok(FromClause::JsonTable(Box::new(resolved)))
        }
        FromClause::JsonbSrf(mut srf) => {
            let taken = std::mem::replace(&mut srf.doc, Expr::Literal(axiomdb_types::Value::Null));
            srf.doc = resolve_expr_full(taken, &empty_ctx, &[], Some(&state))?;
            Ok(FromClause::JsonbSrf(srf))
        }
        FromClause::Values(mut vc) => {
            for row in &mut vc.rows {
                for expr in row {
                    let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                    *expr = resolve_expr_full(taken, &empty_ctx, &[], Some(&state))?;
                }
            }
            Ok(FromClause::Values(vc))
        }
        FromClause::Pivot(pivot) => lower_pivot_clause_to_subquery(
            *pivot,
            storage,
            snapshot,
            default_database,
            default_schema,
            &[],
        ),
        other @ FromClause::Table(_) | other @ FromClause::RecursiveCte(_) => Ok(other),
        // Phase 20.4, Step 7 — UNNEST not allowed in DDL context (no LATERAL FROM in subquery context).
        FromClause::Unnest(_) => Err(DbError::NotImplemented {
            feature: "UNNEST in subquery/DDL context".into(),
        }),
        // Phase 20.10 — GENERATE_SERIES not allowed in DDL context.
        FromClause::GenerateSeries(_) => Err(DbError::NotImplemented {
            feature: "GENERATE_SERIES in subquery/DDL context".into(),
        }),
        // Phase 20.6 — READ_PARQUET not allowed in DDL context.
        FromClause::ReadParquet(_) => Err(DbError::NotImplemented {
            feature: "READ_PARQUET in subquery/DDL context".into(),
        }),
        // Phase 20.20 — XMLTABLE not allowed in DDL context.
        FromClause::XmlTable(_) => Err(DbError::NotImplemented {
            feature: "XMLTABLE in subquery/DDL context".into(),
        }),
    }
}

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

fn analyze_create_table(
    s: CreateTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateTableStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate FK REFERENCES targets.
    for col_def in &s.columns {
        for constraint in &col_def.constraints {
            if let ColumnConstraint::References {
                table: ref_table,
                column: ref_col,
                ..
            } = constraint
            {
                let schema = default_schema;
                let exists = reader
                    .get_table_in_database(default_database, schema, ref_table)?
                    .is_some()
                    || reader
                        .get_table_in_database(default_database, "public", ref_table)?
                        .is_some();
                if !exists {
                    return Err(DbError::TableNotFound {
                        name: ref_table.clone(),
                    });
                }
                // If a specific column is referenced, validate it exists.
                if let Some(col_name) = ref_col {
                    let ref_table_def = reader
                        .get_table_in_database(default_database, default_schema, ref_table)?
                        .or_else(|| {
                            reader
                                .get_table_in_database(default_database, "public", ref_table)
                                .ok()
                                .flatten()
                        });
                    if let Some(ref_def) = ref_table_def {
                        let ref_cols = reader.list_columns(ref_def.id)?;
                        if !ref_cols.iter().any(|c| &c.name == col_name) {
                            return Err(DbError::ColumnNotFound {
                                name: col_name.clone(),
                                table: ref_table.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(s)
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn analyze_drop_table(
    s: DropTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DropTableStmt, DbError> {
    if s.if_exists {
        return Ok(s); // IF EXISTS: no validation needed
    }

    let mut reader = CatalogReader::new(storage, snapshot)?;
    for table_ref in &s.tables {
        let database = table_ref.database.as_deref().unwrap_or(default_database);
        let schema = table_ref.schema.as_deref().unwrap_or(default_schema);
        if table_ref.database.is_some() && !reader.database_exists(database)? {
            return Err(DbError::DatabaseNotFound {
                name: database.to_string(),
            });
        }
        let exists = reader
            .get_table_in_database(database, schema, &table_ref.name)?
            .is_some();
        if !exists {
            return Err(DbError::TableNotFound {
                name: table_ref.name.clone(),
            });
        }
    }

    Ok(s)
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn analyze_create_index(
    s: CreateIndexStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateIndexStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    let table_def = reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    let columns = reader.list_columns(table_def.id)?;

    for idx_col in &s.columns {
        // Phase 21.8: for expression indexes we validate the expression tree
        // (rejecting subqueries, aggregates, window functions, parameters) and
        // ensure every referenced column exists. For plain column indexes we
        // just check the column name against the catalog.
        if let Some(expr) = &idx_col.expr {
            reject_disallowed_in_index_expr(expr)?;
            ensure_expr_columns_exist(expr, &s.table.name, &columns)?;
        } else if !columns.iter().any(|c| c.name == idx_col.name) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: idx_col.name.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
    }

    Ok(s)
}

/// Rejects expression-index expressions that reference constructs which cannot
/// be persisted or re-evaluated deterministically: subqueries, aggregate
/// function calls, window functions, `EXISTS`, `OuterColumn`, prepared
/// parameters, or `InsertValue`.
fn reject_disallowed_in_index_expr(expr: &crate::expr::Expr) -> Result<(), DbError> {
    use crate::expr::Expr;
    match expr {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
            Err(DbError::NotImplemented {
                feature: "subquery in index expression is not allowed".into(),
            })
        }
        Expr::Function { name, args } => {
            if is_aggregate_function(name) {
                return Err(DbError::NotImplemented {
                    feature: format!(
                        "aggregate function '{name}' is not allowed in index expression"
                    ),
                });
            }
            for a in args {
                reject_disallowed_in_index_expr(a)?;
            }
            Ok(())
        }
        Expr::GroupConcat { .. } => Err(DbError::NotImplemented {
            feature: "GROUP_CONCAT is not allowed in index expression".into(),
        }),
        Expr::ArrayAgg { .. } => Err(DbError::NotImplemented {
            feature: "array_agg is not allowed in index expression".into(),
        }),
        Expr::Grouping { .. } => Err(DbError::NotImplemented {
            feature: "GROUPING() is not allowed in index expression".into(),
        }),
        Expr::Window { .. } => Err(DbError::NotImplemented {
            feature: "window function is not allowed in index expression".into(),
        }),
        Expr::Param { .. } => Err(DbError::NotImplemented {
            feature: "prepared parameter is not allowed in index expression".into(),
        }),
        Expr::OuterColumn { .. } | Expr::InsertValue { .. } | Expr::ExcludedValue { .. } => Err(DbError::NotImplemented {
            feature: "outer/insert reference is not allowed in index expression".into(),
        }),
        Expr::BinaryOp { left, right, .. } => {
            reject_disallowed_in_index_expr(left)?;
            reject_disallowed_in_index_expr(right)
        }
        Expr::UnaryOp { operand, .. } => reject_disallowed_in_index_expr(operand),
        Expr::Collate { expr, .. } => reject_disallowed_in_index_expr(expr),
        Expr::IsNull { expr, .. } => reject_disallowed_in_index_expr(expr),
        Expr::IsBoolean { expr, .. } => reject_disallowed_in_index_expr(expr),
        Expr::Between {
            expr, low, high, ..
        } => {
            reject_disallowed_in_index_expr(expr)?;
            reject_disallowed_in_index_expr(low)?;
            reject_disallowed_in_index_expr(high)
        }
        Expr::Like { expr, pattern, .. } => {
            reject_disallowed_in_index_expr(expr)?;
            reject_disallowed_in_index_expr(pattern)
        }
        Expr::In { expr, list, .. } => {
            reject_disallowed_in_index_expr(expr)?;
            for item in list {
                reject_disallowed_in_index_expr(item)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                reject_disallowed_in_index_expr(op)?;
            }
            for (c, r) in when_thens {
                reject_disallowed_in_index_expr(c)?;
                reject_disallowed_in_index_expr(r)?;
            }
            if let Some(e) = else_result {
                reject_disallowed_in_index_expr(e)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => reject_disallowed_in_index_expr(expr),
        Expr::SqlJsonQuery { doc, passing, .. } => {
            reject_disallowed_in_index_expr(doc)?;
            for (v, _) in passing {
                reject_disallowed_in_index_expr(v)?;
            }
            Ok(())
        }
        // Phase 20.4 — ARRAY constructor not allowed in index expressions.
        Expr::ArrayConstructor { .. } => Err(DbError::NotImplemented {
            feature: "ARRAY constructor is not allowed in index expression".into(),
        }),
        // Phase 20.4, Step 5 — array subscript is allowed (element access).
        Expr::Subscript { array, index, slice } => {
            reject_disallowed_in_index_expr(array)?;
            reject_disallowed_in_index_expr(index)?;
            if let Some(s) = slice {
                reject_disallowed_in_index_expr(s)?;
            }
            Ok(())
        }
        // Phase 20.4, Step 7 — ANY/ALL: recurse into expr and array.
        Expr::AnyOf { expr, array } | Expr::AllOf { expr, array } => {
            reject_disallowed_in_index_expr(expr)?;
            reject_disallowed_in_index_expr(array)
        }
        Expr::Row(elems) => {
            for e in elems {
                reject_disallowed_in_index_expr(e)?;
            }
            Ok(())
        }
        Expr::FieldAccess { .. } => Ok(()),
        Expr::Literal(_) | Expr::Column { .. } | Expr::Default => Ok(()),
        // Phase 20.20 — XML constructor forms not allowed in index expressions.
        Expr::XmlElement { .. }
        | Expr::XmlForest { .. }
        | Expr::XmlRoot { .. }
        | Expr::XmlConcat { .. }
        | Expr::XmlQuery { .. } => Err(DbError::NotImplemented {
            feature: "XML constructor is not allowed in index expression".into(),
        }),
    }
}

fn is_aggregate_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "group_concat"
            | "string_agg"
            | "array_agg"
            | "bit_and"
            | "bit_or"
            | "bit_xor"
            | "stddev"
            | "stddev_pop"
            | "stddev_samp"
            | "var_pop"
            | "var_samp"
            | "variance"
    )
}

/// Walks the expression and errors with `ColumnNotFound` for any column
/// reference whose name is not in `columns`.
fn ensure_expr_columns_exist(
    expr: &crate::expr::Expr,
    table_name: &str,
    columns: &[axiomdb_catalog::ColumnDef],
) -> Result<(), DbError> {
    use crate::expr::Expr;
    let mut missing: Option<String> = None;
    walk_columns(expr, &mut |name| {
        if missing.is_none() && !columns.iter().any(|c| c.name == name) {
            missing = Some(name.to_string());
        }
    });
    if let Some(name) = missing {
        let available = columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DbError::ColumnNotFound {
            name,
            table: format!("\"{table_name}\" (available: {available})"),
        });
    }
    return Ok(());

    fn walk_columns(e: &Expr, f: &mut impl FnMut(&str)) {
        match e {
            Expr::Column { name, .. } => f(name),
            Expr::BinaryOp { left, right, .. } => {
                walk_columns(left, f);
                walk_columns(right, f);
            }
            Expr::UnaryOp { operand, .. } => walk_columns(operand, f),
            Expr::IsNull { expr, .. } => walk_columns(expr, f),
            Expr::IsBoolean { expr, .. } => walk_columns(expr, f),
            Expr::Between {
                expr, low, high, ..
            } => {
                walk_columns(expr, f);
                walk_columns(low, f);
                walk_columns(high, f);
            }
            Expr::Like { expr, pattern, .. } => {
                walk_columns(expr, f);
                walk_columns(pattern, f);
            }
            Expr::In { expr, list, .. } => {
                walk_columns(expr, f);
                for it in list {
                    walk_columns(it, f);
                }
            }
            Expr::Function { args, .. } => {
                for a in args {
                    walk_columns(a, f);
                }
            }
            Expr::Case {
                operand,
                when_thens,
                else_result,
            } => {
                if let Some(op) = operand {
                    walk_columns(op, f);
                }
                for (c, r) in when_thens {
                    walk_columns(c, f);
                    walk_columns(r, f);
                }
                if let Some(er) = else_result {
                    walk_columns(er, f);
                }
            }
            Expr::Cast { expr, .. } => walk_columns(expr, f),
            _ => {}
        }
    }
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

fn analyze_alter_table(
    s: AlterTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<AlterTableStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate the target table exists.
    reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    // Individual operations validated at execution time (Phase 4.22).
    // For now just validate the table exists.
    let _ = s.operations; // suppress unused warning
    Ok(s)
}
