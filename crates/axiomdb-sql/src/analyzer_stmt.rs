// ── Statement analysis ────────────────────────────────────────────────────────

fn analyze_stmt(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<Stmt, DbError> {
    match stmt {
        Stmt::Select(s) => {
            analyze_select(s, storage, snapshot, default_database, default_schema).map(Stmt::Select)
        }
        Stmt::Insert(s) => {
            analyze_insert(s, storage, snapshot, default_database, default_schema).map(Stmt::Insert)
        }
        Stmt::Update(s) => {
            analyze_update(s, storage, snapshot, default_database, default_schema).map(Stmt::Update)
        }
        Stmt::Delete(s) => {
            analyze_delete(s, storage, snapshot, default_database, default_schema).map(Stmt::Delete)
        }
        Stmt::Merge(s) => {
            analyze_merge(s, storage, snapshot, default_database, default_schema).map(Stmt::Merge)
        }
        Stmt::CreateTable(s) => {
            analyze_create_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::CreateTable)
        }
        // CREATE TABLE LIKE — no column resolution needed; source table is resolved at execution
        Stmt::CreateTableLike(_) => Ok(stmt),
        // CREATE TABLE AS SELECT — analyze the inner SELECT
        Stmt::CreateTableAsSelect(mut s) => {
            s.select = analyze_select(
                s.select,
                storage,
                snapshot,
                default_database,
                default_schema,
            )?;
            Ok(Stmt::CreateTableAsSelect(s))
        }
        Stmt::CreateMaterializedView(mut s) => {
            s.select = analyze_select(
                s.select,
                storage,
                snapshot,
                default_database,
                default_schema,
            )?;
            Ok(Stmt::CreateMaterializedView(s))
        }
        Stmt::CreateView(mut s) => {
            s.select = analyze_select(
                s.select,
                storage,
                snapshot,
                default_database,
                default_schema,
            )?;
            Ok(Stmt::CreateView(s))
        }
        Stmt::DropView(_) | Stmt::ShowCreateView(_) => Ok(stmt),
        Stmt::CreateTrigger(_)
        | Stmt::DropTrigger(_)
        | Stmt::ShowCreateTrigger(_)
        | Stmt::CreateAggregate(_)
        | Stmt::CreateSequence(_)
        | Stmt::CreateEnumType(_)
        | Stmt::DropSequence(_)
        | Stmt::DropAggregate(_) => Ok(stmt),
        Stmt::DropTable(s) => {
            analyze_drop_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::DropTable)
        }
        Stmt::DropMaterializedView(s) => {
            let drop_stmt = crate::ast::DropTableStmt {
                if_exists: s.if_exists,
                tables: s.views,
                cascade: s.cascade,
            };
            analyze_drop_table(drop_stmt, storage, snapshot, default_database, default_schema)
                .map(|drop_stmt| {
                    Stmt::DropMaterializedView(crate::ast::DropMaterializedViewStmt {
                        if_exists: drop_stmt.if_exists,
                        views: drop_stmt.tables,
                        cascade: drop_stmt.cascade,
                    })
                })
        }
        Stmt::CreateIndex(s) => {
            analyze_create_index(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::CreateIndex)
        }
        Stmt::AlterTable(s) => {
            analyze_alter_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::AlterTable)
        }
        // UNION / INTERSECT / EXCEPT — analyze first + each tail SELECT.
        Stmt::SetOp { first, rest } => {
            let first = analyze_select(
                first,
                storage,
                snapshot.clone(),
                default_database,
                default_schema,
            )?;
            let rest: Result<Vec<_>, _> = rest
                .into_iter()
                .map(|t| {
                    analyze_select(
                        t.select,
                        storage,
                        snapshot.clone(),
                        default_database,
                        default_schema,
                    )
                    .map(|select| crate::ast::SetOpTail {
                        kind: t.kind,
                        all: t.all,
                        select,
                    })
                })
                .collect();
            Ok(Stmt::SetOp { first, rest: rest? })
        }
        Stmt::DeclareCursor(mut s) => {
            s.query = Box::new(analyze_stmt(
                *s.query,
                storage,
                snapshot,
                default_database,
                default_schema,
            )?);
            Ok(Stmt::DeclareCursor(s))
        }
        // Statements that need no semantic analysis for Phase 4.18:
        other => Ok(other),
    }
}

// ── Cached analysis dispatcher ────────────────────────────────────────────────

fn analyze_stmt_cached(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    match stmt {
        // INSERT is the hot path — use the cached variant
        Stmt::Insert(s) => analyze_insert_cached(
            s,
            storage,
            snapshot,
            default_database,
            default_schema,
            cache,
        )
        .map(Stmt::Insert),
        // DDL invalidates the cache
        Stmt::CreateTable(_)
        | Stmt::CreateTableLike(_)
        | Stmt::CreateTableAsSelect(_)
        | Stmt::CreateMaterializedView(_)
        | Stmt::CreateView(_)
        | Stmt::CreateTrigger(_)
        | Stmt::CreateAggregate(_)
        | Stmt::CreateSequence(_)
        | Stmt::CreateEnumType(_)
        | Stmt::DropTable(_)
        | Stmt::DropMaterializedView(_)
        | Stmt::DropView(_)
        | Stmt::DropTrigger(_)
        | Stmt::DropAggregate(_)
        | Stmt::DropSequence(_)
        | Stmt::AlterTable(_) => {
            cache.invalidate();
            analyze_stmt(stmt, storage, snapshot, default_database, default_schema)
        }
        // Everything else: fall back to uncached
        other => analyze_stmt(other, storage, snapshot, default_database, default_schema),
    }
}

fn analyze_insert_cached(
    mut s: InsertStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<InsertStmt, DbError> {
    if s.on_conflict.is_some() || !s.returning.is_empty() {
        return analyze_insert(s, storage, snapshot, default_database, default_schema);
    }

    let database = s.table.database.as_deref().unwrap_or(default_database);
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);

    // Try cache first — avoids HeapChain::scan_visible × 2 on repeated inserts
    let (table_def, columns): (axiomdb_catalog::TableDef, Vec<axiomdb_catalog::ColumnDef>) =
        if let Some(td) = cache.get_table(database, schema, &s.table.name) {
            let cols = cache.get_columns(td.id).cloned().unwrap_or_default();
            (td.clone(), cols)
        } else {
            // Cache miss: resolve with search_path fallback
            let mut reader = CatalogReader::new(storage, snapshot.clone())?;
            let td = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
            let cols = reader.list_columns(td.id)?;
            cache.insert(database, schema, &s.table.name, td.clone(), cols.clone());
            (td, cols)
        };
    let _ = table_def; // used only to populate cache; executor reads from catalog directly

    // Validate named column list if provided
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

    // Analyze SELECT source if present
    if let InsertSource::Select(ref select) = s.source {
        let analyzed = analyze_select(
            *select.clone(),
            storage,
            snapshot,
            default_database,
            default_schema,
        )?;
        s.source = InsertSource::Select(Box::new(analyzed));
    }

    Ok(s)
}

// ── SELECT ────────────────────────────────────────────────────────────────────

/// Public entry for analyzing a SELECT with no outer scopes.
///
/// Delegates to [`analyze_select_with_outer`] with an empty outer-scope slice.
fn analyze_select(
    s: SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<SelectStmt, DbError> {
    analyze_select_with_outer(s, storage, snapshot, default_database, default_schema, &[])
}

/// Analyze a SELECT statement, threading `outer_scopes` through every expression
/// so that correlated column references produce `Expr::OuterColumn` nodes.
///
/// Called recursively for subqueries: when a subquery is encountered inside
/// `resolve_expr_full`, the current `BindContext` is appended to `outer_scopes`
/// and this function is invoked on the inner `SelectStmt`.
fn analyze_select_with_outer(
    mut s: SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
) -> Result<SelectStmt, DbError> {
    // Phase 20.1 — expand regular view references before CTE expansion.
    // CTEs shadow views of the same name (CTE wins).
    expand_views(
        &mut s,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
        outer_scopes,
        &mut std::collections::HashSet::new(),
    )?;

    // Phase 21.2 — expand CTE bindings before resolving FROM.
    // Each CTE body is analyzed in order so later CTEs can reference
    // earlier ones; references in the outer query's FROM / JOINs are
    // rewritten to FromClause::Subquery.
    if !s.with_ctes.is_empty() {
        expand_ctes(
            &mut s,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
        )?;
    }

    // Build resolution context from FROM and JOINs.
    // Phase 21.9: Pass outer_scopes so LATERAL subqueries in JOINs can reference
    // outer tables (they become OuterColumn during analysis).
    let ctx = build_context(
        &s.from,
        &s.joins,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
        outer_scopes,
    )?;

    // If FROM is a derived table (subquery in FROM), `build_context` analyzed
    // the inner query to extract virtual column names, but did NOT store the
    // analyzed version back into `s.from`. Fix that here so the executor
    // receives the analyzed inner query with correct `col_idx` values.
    if let Some(FromClause::Subquery {
        query,
        alias,
        lateral,
    }) = s.from
    {
        // For LATERAL subqueries in first-FROM position, there are no left-side
        // tables yet, so effective_scopes = outer_scopes (same as non-LATERAL).
        let analyzed_inner = analyze_select_with_outer(
            *query,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
        )?;
        s.from = Some(FromClause::Subquery {
            query: Box::new(analyzed_inner),
            alias,
            lateral,
        });
    }

    if let Some(FromClause::Pivot(pivot)) = s.from {
        s.from = Some(lower_pivot_clause_to_subquery(
            *pivot,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
        )?);
    }

    // AnalyzeState is needed so that subquery arms inside expressions can
    // recurse back into analyze_select_with_outer.
    let state = AnalyzeState {
        storage,
        snapshot,
        default_database,
        default_schema,
    };

    // Phase 11.20a — resolve JSON_TABLE's `doc` expression (and any
    // `DEFAULT` expressions inside ON EMPTY / ON ERROR) against the full
    // bind context. This mirrors how subqueries are re-analyzed above.
    if let Some(FromClause::JsonTable(jt)) = s.from {
        let resolved = resolve_json_table(*jt, &ctx, outer_scopes, &state)?;
        s.from = Some(FromClause::JsonTable(Box::new(resolved)));
    }

    // Phase 11.25a — resolve JSONB SRF doc expression for first-FROM position.
    if let Some(FromClause::JsonbSrf(mut srf)) = s.from {
        let taken = std::mem::replace(&mut srf.doc, Expr::Literal(axiomdb_types::Value::Null));
        srf.doc = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
        s.from = Some(FromClause::JsonbSrf(srf));
    }

    // Phase 21.22 — resolve VALUES row exprs for first-FROM position.
    if let Some(FromClause::Values(mut vc)) = s.from {
        let empty_ctx = BindContext::empty();
        for row in &mut vc.rows {
            for e in row {
                let taken = std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                *e = resolve_expr_full(taken, &empty_ctx, &[], Some(&state))?;
            }
        }
        s.from = Some(FromClause::Values(vc));
    }

    // Persist analyzed join-side derived tables back into the AST before
    // resolving JOIN conditions so the executor receives analyzed inner SELECTs.
    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for (join_idx, mut join) in s.joins.into_iter().enumerate() {
        match join.table {
            FromClause::Subquery {
                query,
                alias,
                lateral,
            } => {
                // For LATERAL subqueries, pass the accumulated context (ctx) as an
                // additional outer scope so that references to left-side tables (like t.id)
                // are resolved as OuterColumn. For non-LATERAL, use the original outer_scopes.
                let effective_scopes = if lateral {
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(&ctx);
                    scopes
                } else {
                    outer_scopes.to_vec()
                };
                let analyzed_inner = analyze_select_with_outer(
                    *query,
                    storage,
                    state.snapshot.clone(),
                    state.default_database,
                    state.default_schema,
                    &effective_scopes,
                )?;
                join.table = FromClause::Subquery {
                    query: Box::new(analyzed_inner),
                    alias,
                    lateral,
                };
            }
            // Phase 11.20d3 — resolve JSON_TABLE doc + PASSING on the JOIN
            // right side against the combined scope so correlated expressions
            // pick up outer-column bindings.
            FromClause::JsonTable(jt) => {
                let resolved = resolve_json_table(*jt, &ctx, outer_scopes, &state)?;
                join.table = FromClause::JsonTable(Box::new(resolved));
            }
            // Phase 11.25a — resolve SRF doc against combined + outer scope.
            FromClause::JsonbSrf(mut srf) => {
                let taken =
                    std::mem::replace(&mut srf.doc, Expr::Literal(axiomdb_types::Value::Null));
                srf.doc = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
                join.table = FromClause::JsonbSrf(srf);
            }
            // Phase 21.22 — VALUES rows resolve against an empty scope
            // (no correlation in this subphase).
            FromClause::Values(mut vc) => {
                let empty_ctx = BindContext::empty();
                for row in &mut vc.rows {
                    for e in row {
                        let taken = std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                        *e = resolve_expr_full(taken, &empty_ctx, &[], Some(&state))?;
                    }
                }
                join.table = FromClause::Values(vc);
            }
            FromClause::Pivot(pivot) => {
                let effective_scopes = if pivot_source_lateral(&pivot.source) {
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(&ctx);
                    scopes
                } else {
                    outer_scopes.to_vec()
                };
                join.table = lower_pivot_clause_to_subquery(
                    *pivot,
                    storage,
                    state.snapshot.clone(),
                    state.default_database,
                    state.default_schema,
                    &effective_scopes,
                )?;
            }
            FromClause::Table(_) => {}
            // Phase 21.3 — recursive CTE already pre-analyzed by expand_ctes.
            FromClause::RecursiveCte(_) => {}
            // Phase GAP-20.4b — UNNEST: resolve array expressions against
            // the accumulated scope (LATERAL correlation). For LATERAL UNNEST,
            // use empty ctx so that column references to left-side tables
            // (e.g., t.arr in LATERAL unnest(t.arr)) are resolved as
            // OuterColumn nodes. For non-LATERAL, use ctx for local resolution.
            FromClause::Unnest(mut un) => {
                if un.lateral {
                    // Use empty ctx so resolution falls through to outer_scopes,
                    // creating OuterColumn references for left-side column refs.
                    let empty_ctx = BindContext::empty();
                    let mut scopes = outer_scopes.to_vec();
                    scopes.push(&ctx);
                    for expr in &mut un.exprs {
                        let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                        *expr = resolve_expr_full(taken, &empty_ctx, &scopes, Some(&state))?;
                    }
                } else {
                    for expr in &mut un.exprs {
                        let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                        *expr = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
                    }
                }
                join.table = FromClause::Unnest(un);
            }
            // Phase 20.10 — GENERATE_SERIES: resolve start/stop/step expressions.
            FromClause::GenerateSeries(mut gs) => {
                let taken = std::mem::replace(&mut gs.start, Expr::Literal(axiomdb_types::Value::Null));
                gs.start = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
                let taken = std::mem::replace(&mut gs.stop, Expr::Literal(axiomdb_types::Value::Null));
                gs.stop = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
                if let Some(step) = gs.step.take() {
                    let resolved = resolve_expr_full(step, &ctx, outer_scopes, Some(&state))?;
                    gs.step = Some(resolved);
                }
                join.table = FromClause::GenerateSeries(gs);
            }
        }
        // Phase 21.18 — NATURAL JOIN: compute the shared-column list between
        // the accumulated left-side scope and this join's right-side BoundTable,
        // then rewrite condition to the equivalent `USING (shared...)`.
        if join.natural {
            // ctx.tables = [first_FROM, join[0].right, join[1].right, ...]
            // For joins[join_idx], left = ctx.tables[..=join_idx],
            // right = ctx.tables[join_idx + 1].
            if join_idx + 1 >= ctx.tables.len() {
                return Err(DbError::Internal {
                    message: "NATURAL JOIN: analyzer scope out of sync with join list".into(),
                });
            }
            let right_cols: Vec<String> = ctx.tables[join_idx + 1]
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let mut shared: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for bt in &ctx.tables[..=join_idx] {
                for col in &bt.columns {
                    let cname_ci = col.name.to_ascii_lowercase();
                    if !seen.insert(cname_ci.clone()) {
                        continue;
                    }
                    if right_cols.iter().any(|r| r.eq_ignore_ascii_case(&col.name)) {
                        shared.push(col.name.clone());
                    }
                }
            }
            if shared.is_empty() {
                let left_name = ctx.tables[join_idx].name.clone();
                let right_name = ctx.tables[join_idx + 1].name.clone();
                return Err(DbError::ParseError {
                    message: format!(
                        "NATURAL JOIN: no shared columns between `{left_name}` and \
                         `{right_name}`"
                    ),
                    position: None,
                });
            }
            join.condition = JoinCondition::Using(shared);
            join.natural = false;
        }
        join.condition = match join.condition {
            JoinCondition::On(expr) => {
                JoinCondition::On(resolve_expr_full(expr, &ctx, outer_scopes, Some(&state))?)
            }
            JoinCondition::Using(cols) => {
                // Detailed column-by-column validation deferred (Phase 4.22).
                JoinCondition::Using(cols)
            }
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    // Resolve WHERE, GROUP BY, HAVING, ORDER BY, LIMIT, OFFSET.
    s.where_clause = resolve_opt_expr_full(s.where_clause, &ctx, outer_scopes, Some(&state))?;
    {
        let resolved_exprs = s.group_by.exprs().to_owned()
            .into_iter()
            .map(|e| resolve_expr_full(e, &ctx, outer_scopes, Some(&state)))
            .collect::<Result<Vec<_>, _>>()?;
        s.group_by = match s.group_by {
            GroupByClause::Simple(_) => GroupByClause::Simple(resolved_exprs),
            GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved_exprs),
            GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved_exprs, sets },
            GroupByClause::None => GroupByClause::None,
        };
    }
    s.having = resolve_opt_expr_full(s.having, &ctx, outer_scopes, Some(&state))?;

    // Resolve ORDER BY.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr_full(item.expr, &ctx, outer_scopes, Some(&state))?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr_full(s.limit, &ctx, outer_scopes, Some(&state))?;
    s.offset = resolve_opt_expr_full(s.offset, &ctx, outer_scopes, Some(&state))?;

    // Phase 21.12 — Resolve DISTINCT ON expressions (evaluated against pre-projection rows,
    // same scope as ORDER BY, so source columns not in the SELECT list are accessible).
    if !s.distinct_on.is_empty() {
        if !s.group_by.is_empty() {
            return Err(DbError::NotImplemented {
                feature: "DISTINCT ON combined with GROUP BY".into(),
            });
        }
        let mut resolved_distinct_on = Vec::with_capacity(s.distinct_on.len());
        for e in s.distinct_on {
            resolved_distinct_on.push(resolve_expr_full(e, &ctx, outer_scopes, Some(&state))?);
        }
        s.distinct_on = resolved_distinct_on;
    }

    // Resolve SELECT list.
    let mut resolved_cols = Vec::with_capacity(s.columns.len());
    for item in s.columns {
        let resolved = match item {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::QualifiedWildcard(ref table_name) => {
                // Validate the table/alias is in scope.
                if !ctx.tables.is_empty() && ctx.find_table(table_name).is_none() {
                    return Err(DbError::TableNotFound {
                        name: format!("{table_name}.*"),
                    });
                }
                item
            }
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr_full(expr, &ctx, outer_scopes, Some(&state))?,
                alias,
            },
        };
        resolved_cols.push(resolved);
    }
    s.columns = resolved_cols;

    rewrite_custom_aggregates_in_select(&mut s, storage, state.snapshot.clone(), default_schema)?;

    validate_window_usage(&s)?;

    // Post-pass: populate universe_indices on every GROUPING() call in HAVING,
    // SELECT list, and ORDER BY — all resolved now.
    if let GroupByClause::Sets { ref universe, .. } = s.group_by {
        let universe_snapshot: Vec<Expr> = universe.clone();
        if let Some(ref mut having_expr) = s.having {
            populate_grouping_indices(having_expr, &universe_snapshot);
        }
        for item in s.columns.iter_mut() {
            if let SelectItem::Expr { ref mut expr, .. } = item {
                populate_grouping_indices(expr, &universe_snapshot);
            }
        }
        for ob in s.order_by.iter_mut() {
            populate_grouping_indices(&mut ob.expr, &universe_snapshot);
        }
    }

    // 4.12c: DISTINCT + non-selected ORDER BY → MySQL error 3065.
    // `SELECT DISTINCT a FROM t ORDER BY b` is invalid when b is not derived
    // from any expression in the SELECT list, because DISTINCT deduplications
    // happen on the projection output before sorting; the sort key must therefore
    // be computable from the projected columns.
    if s.distinct && !s.order_by.is_empty() {
        // Build the set of col_idx values that appear in the SELECT list.
        let selected_col_idxs: std::collections::HashSet<usize> = s
            .columns
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expr, .. } => expr_column_idx(expr),
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => None,
            })
            .collect();

        // Wildcards cover all columns — no restriction possible.
        let has_wildcard = s.columns.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)
            )
        });

        if !has_wildcard {
            for item in &s.order_by {
                // If the ORDER BY expression is a column reference, it must
                // appear in the SELECT list. Non-column expressions (literals,
                // functions, CASE) are rejected unless they match a SELECT expr.
                if let Some(idx) = expr_column_idx(&item.expr) {
                    if !selected_col_idxs.contains(&idx) {
                        return Err(DbError::ParseError {
                            message: "ORDER BY expression not in DISTINCT SELECT list \
                                      (expression must appear in the SELECT list)"
                                .into(),
                            position: None,
                        });
                    }
                }
            }
        }
    }

    Ok(s)
}

/// Returns the `col_idx` if `expr` is a simple `Expr::Column`; `None` otherwise.
fn expr_column_idx(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Column { col_idx, .. } => Some(*col_idx),
        _ => None,
    }
}

fn validate_window_usage(stmt: &SelectStmt) -> Result<(), DbError> {
    let any_windows = stmt.columns.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => expr_contains_window(expr),
        _ => false,
    }) || stmt
        .where_clause
        .as_ref()
        .map(expr_contains_window)
        .unwrap_or(false)
        || stmt.group_by.exprs().iter().any(expr_contains_window)
        || stmt
            .having
            .as_ref()
            .map(expr_contains_window)
            .unwrap_or(false)
        || stmt.order_by.iter().any(|item| expr_contains_window(&item.expr))
        || stmt.distinct_on.iter().any(expr_contains_window)
        || stmt.joins.iter().any(|join| match &join.condition {
            JoinCondition::On(expr) => expr_contains_window(expr),
            JoinCondition::Using(_) => false,
        });

    if !any_windows {
        return Ok(());
    }

    if stmt.distinct || !stmt.distinct_on.is_empty() {
        return Err(DbError::NotImplemented {
            feature: "window functions with DISTINCT/DISTINCT ON".into(),
        });
    }

    if !stmt.joins.is_empty() {
        return Err(DbError::NotImplemented {
            feature: "window functions on joined SELECTs".into(),
        });
    }

    if !stmt.group_by.is_empty() || stmt.having.is_some() {
        return Err(DbError::NotImplemented {
            feature: "window functions with GROUP BY/HAVING".into(),
        });
    }

    if let Some(expr) = &stmt.where_clause {
        if expr_contains_window(expr) {
            return Err(DbError::NotImplemented {
                feature: "window functions in WHERE".into(),
            });
        }
    }
    for expr in stmt.group_by.exprs() {
        if expr_contains_window(expr) {
            return Err(DbError::NotImplemented {
                feature: "window functions in GROUP BY".into(),
            });
        }
    }
    if let Some(expr) = &stmt.having {
        if expr_contains_window(expr) {
            return Err(DbError::NotImplemented {
                feature: "window functions in HAVING".into(),
            });
        }
    }
    for item in &stmt.order_by {
        if expr_contains_window(&item.expr) {
            return Err(DbError::NotImplemented {
                feature: "window functions in ORDER BY".into(),
            });
        }
    }
    for expr in &stmt.distinct_on {
        if expr_contains_window(expr) {
            return Err(DbError::NotImplemented {
                feature: "window functions in DISTINCT ON".into(),
            });
        }
    }
    for join in &stmt.joins {
        if let JoinCondition::On(expr) = &join.condition {
            if expr_contains_window(expr) {
                return Err(DbError::NotImplemented {
                    feature: "window functions in JOIN conditions".into(),
                });
            }
        }
    }

    for item in &stmt.columns {
        let SelectItem::Expr { expr, .. } = item else {
            continue;
        };
        match expr {
            Expr::Window { spec, .. } => validate_window_spec(spec)?,
            other if expr_contains_window(other) => {
                return Err(DbError::NotImplemented {
                    feature: "window functions nested inside other SELECT expressions".into(),
                });
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_window_spec(spec: &crate::ast::WindowSpec) -> Result<(), DbError> {
    for expr in &spec.partition_by {
        if expr_contains_window(expr) {
            return Err(DbError::NotImplemented {
                feature: "nested window functions in PARTITION BY".into(),
            });
        }
        if expr_contains_aggregate(expr) {
            return Err(DbError::NotImplemented {
                feature: "aggregate expressions in PARTITION BY".into(),
            });
        }
    }
    for item in &spec.order_by {
        if expr_contains_window(&item.expr) {
            return Err(DbError::NotImplemented {
                feature: "nested window functions in window ORDER BY".into(),
            });
        }
        if expr_contains_aggregate(&item.expr) {
            return Err(DbError::NotImplemented {
                feature: "aggregate expressions in window ORDER BY".into(),
            });
        }
    }
    Ok(())
}

fn expr_contains_window(expr: &Expr) -> bool {
    match expr {
        Expr::Window { .. } => true,
        Expr::UnaryOp { operand, .. }
        | Expr::Collate { expr: operand, .. }
        | Expr::IsNull { expr: operand, .. }
        | Expr::IsBoolean { expr: operand, .. }
        | Expr::Cast { expr: operand, .. } => expr_contains_window(operand),
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_window(left) || expr_contains_window(right)
        }
        Expr::Between { expr, low, high, .. } => {
            expr_contains_window(expr)
                || expr_contains_window(low)
                || expr_contains_window(high)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_contains_window(expr)
                || expr_contains_window(pattern)
                || escape
                    .as_deref()
                    .map(expr_contains_window)
                    .unwrap_or(false)
        }
        Expr::In { expr, list, .. } => {
            expr_contains_window(expr) || list.iter().any(expr_contains_window)
        }
        Expr::Function { args, .. } => args.iter().any(expr_contains_window),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand
                .as_deref()
                .map(expr_contains_window)
                .unwrap_or(false)
                || when_thens
                    .iter()
                    .any(|(when, then)| expr_contains_window(when) || expr_contains_window(then))
                || else_result
                    .as_deref()
                    .map(expr_contains_window)
                    .unwrap_or(false)
        }
        Expr::GroupConcat { expr, order_by, .. } => {
            expr_contains_window(expr)
                || order_by.iter().any(|(expr, _)| expr_contains_window(expr))
        }
        Expr::ArrayAgg { expr, order_by, .. } => {
            expr_contains_window(expr)
                || order_by.iter().any(|(expr, _)| expr_contains_window(expr))
        }
        Expr::Grouping { args, .. } => args.iter().any(expr_contains_window),
        Expr::SqlJsonQuery {
            doc,
            passing,
            on_empty,
            on_error,
            ..
        } => {
            expr_contains_window(doc)
                || passing.iter().any(|(expr, _)| expr_contains_window(expr))
                || on_behavior_contains_window(on_empty)
                || on_behavior_contains_window(on_error)
        }
        Expr::InSubquery { expr, .. } => expr_contains_window(expr),
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Default
        | Expr::Param { .. }
        | Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::ArrayConstructor { .. }
        | Expr::Subscript { .. }
        | Expr::AnyOf { .. }
        | Expr::AllOf { .. } => false,
    }
}

fn on_behavior_contains_window(behavior: &crate::expr::SqlJsonOnBehavior) -> bool {
    match behavior {
        crate::expr::SqlJsonOnBehavior::Default(expr) => expr_contains_window(expr),
        _ => false,
    }
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::GroupConcat { .. } | Expr::ArrayAgg { .. } => true,
        Expr::Function { name, .. } if is_aggregate_name(name) => true,
        Expr::UnaryOp { operand, .. }
        | Expr::Collate { expr: operand, .. }
        | Expr::IsNull { expr: operand, .. }
        | Expr::IsBoolean { expr: operand, .. }
        | Expr::Cast { expr: operand, .. } => expr_contains_aggregate(operand),
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::Between { expr, low, high, .. } => {
            expr_contains_aggregate(expr)
                || expr_contains_aggregate(low)
                || expr_contains_aggregate(high)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_contains_aggregate(expr)
                || expr_contains_aggregate(pattern)
                || escape
                    .as_deref()
                    .map(expr_contains_aggregate)
                    .unwrap_or(false)
        }
        Expr::In { expr, list, .. } => {
            expr_contains_aggregate(expr) || list.iter().any(expr_contains_aggregate)
        }
        Expr::Function { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand
                .as_deref()
                .map(expr_contains_aggregate)
                .unwrap_or(false)
                || when_thens.iter().any(|(when, then)| {
                    expr_contains_aggregate(when) || expr_contains_aggregate(then)
                })
                || else_result
                    .as_deref()
                    .map(expr_contains_aggregate)
                    .unwrap_or(false)
        }
        Expr::Window { spec, .. } => {
            spec.partition_by.iter().any(expr_contains_aggregate)
                || spec.order_by.iter().any(|item| expr_contains_aggregate(&item.expr))
        }
        Expr::Grouping { .. } => false,
        Expr::SqlJsonQuery {
            doc,
            passing,
            on_empty,
            on_error,
            ..
        } => {
            expr_contains_aggregate(doc)
                || passing.iter().any(|(expr, _)| expr_contains_aggregate(expr))
                || on_behavior_contains_aggregate(on_empty)
                || on_behavior_contains_aggregate(on_error)
        }
        Expr::InSubquery { expr, .. } => expr_contains_aggregate(expr),
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Default
        | Expr::Param { .. }
        | Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::ArrayConstructor { .. }
        | Expr::Subscript { .. }
        | Expr::AnyOf { .. }
        | Expr::AllOf { .. } => false,
    }
}

fn on_behavior_contains_aggregate(behavior: &crate::expr::SqlJsonOnBehavior) -> bool {
    match behavior {
        crate::expr::SqlJsonOnBehavior::Default(expr) => expr_contains_aggregate(expr),
        _ => false,
    }
}

fn rewrite_custom_aggregates_in_select(
    stmt: &mut SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_schema: &str,
) -> Result<(), DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;

    if let Some(expr) = stmt.where_clause.as_mut() {
        rewrite_custom_aggregates_in_expr(expr, &mut reader, default_schema)?;
    }
    if !stmt.group_by.is_empty() {
        for expr in stmt.group_by.exprs_mut() {
            rewrite_custom_aggregates_in_expr(expr, &mut reader, default_schema)?;
        }
    }
    if let Some(expr) = stmt.having.as_mut() {
        rewrite_custom_aggregates_in_expr(expr, &mut reader, default_schema)?;
    }
    for item in &mut stmt.order_by {
        rewrite_custom_aggregates_in_expr(&mut item.expr, &mut reader, default_schema)?;
    }
    for item in &mut stmt.columns {
        if let SelectItem::Expr { expr, .. } = item {
            rewrite_custom_aggregates_in_expr(expr, &mut reader, default_schema)?;
        }
    }
    for expr in &mut stmt.distinct_on {
        rewrite_custom_aggregates_in_expr(expr, &mut reader, default_schema)?;
    }
    Ok(())
}

fn rewrite_custom_aggregates_in_expr(
    expr: &mut Expr,
    reader: &mut CatalogReader,
    default_schema: &str,
) -> Result<(), DbError> {
    match expr {
        Expr::Function { name, args } => {
            for arg in args.iter_mut() {
                rewrite_custom_aggregates_in_expr(arg, reader, default_schema)?;
            }
            if let Some(def) = reader.get_aggregate(default_schema, name, args.len())? {
                *name = crate::custom_aggregate::internal_aggregate_name(def.helper_kind).into();
            }
        }
        Expr::GroupConcat {
            expr,
            order_by,
            ..
        } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            for (item_expr, _) in order_by {
                rewrite_custom_aggregates_in_expr(item_expr, reader, default_schema)?;
            }
        }
        Expr::ArrayAgg {
            expr,
            order_by,
            ..
        } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            for (item_expr, _) in order_by {
                rewrite_custom_aggregates_in_expr(item_expr, reader, default_schema)?;
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            rewrite_custom_aggregates_in_expr(left, reader, default_schema)?;
            rewrite_custom_aggregates_in_expr(right, reader, default_schema)?;
        }
        Expr::UnaryOp { operand, .. }
        | Expr::Collate { expr: operand, .. }
        | Expr::IsNull { expr: operand, .. }
        | Expr::IsBoolean { expr: operand, .. }
        | Expr::Cast { expr: operand, .. } => {
            rewrite_custom_aggregates_in_expr(operand, reader, default_schema)?;
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            rewrite_custom_aggregates_in_expr(low, reader, default_schema)?;
            rewrite_custom_aggregates_in_expr(high, reader, default_schema)?;
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            rewrite_custom_aggregates_in_expr(pattern, reader, default_schema)?;
            if let Some(expr) = escape {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
        }
        Expr::In { expr, list, .. } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            for item in list {
                rewrite_custom_aggregates_in_expr(item, reader, default_schema)?;
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(expr) = operand {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
            for (when, then) in when_thens {
                rewrite_custom_aggregates_in_expr(when, reader, default_schema)?;
                rewrite_custom_aggregates_in_expr(then, reader, default_schema)?;
            }
            if let Some(expr) = else_result {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
        }
        Expr::Window { spec, .. } => {
            for expr in &mut spec.partition_by {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
            for item in &mut spec.order_by {
                rewrite_custom_aggregates_in_expr(&mut item.expr, reader, default_schema)?;
            }
        }
        Expr::Grouping { args, .. } => {
            for expr in args {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
        }
        Expr::SqlJsonQuery {
            doc,
            passing,
            on_empty,
            on_error,
            ..
        } => {
            rewrite_custom_aggregates_in_expr(doc, reader, default_schema)?;
            for (expr, _) in passing {
                rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            }
            rewrite_on_behavior_custom_aggregates(on_empty, reader, default_schema)?;
            rewrite_on_behavior_custom_aggregates(on_error, reader, default_schema)?;
        }
        Expr::InSubquery { expr, .. } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
        }
        Expr::AnyOf { expr, array } | Expr::AllOf { expr, array } => {
            rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
            rewrite_custom_aggregates_in_expr(array, reader, default_schema)?;
        }
        Expr::Literal(_)
        | Expr::Default
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Param { .. }
        | Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::ArrayConstructor { .. }
        | Expr::Subscript { .. } => {}
    }
    Ok(())
}

fn rewrite_on_behavior_custom_aggregates(
    behavior: &mut crate::expr::SqlJsonOnBehavior,
    reader: &mut CatalogReader,
    default_schema: &str,
) -> Result<(), DbError> {
    if let crate::expr::SqlJsonOnBehavior::Default(expr) = behavior {
        rewrite_custom_aggregates_in_expr(expr, reader, default_schema)?;
    }
    Ok(())
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
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
            | "jsonb_object_agg"
            | "json_object_agg"
            | "json_objectagg"
            | "__axiom_internal_median"
    )
}

/// Recursively walks `expr` and fills in `universe_indices` on every
/// `Expr::Grouping` node by matching each argument against the `universe`
/// list from `GroupByClause::Sets`.
///
/// An argument that is not found in the universe gets index `usize::MAX`
/// (will be treated as never-rolled-up = returns 0 for that bit).
fn populate_grouping_indices(expr: &mut Expr, universe: &[Expr]) {
    match expr {
        Expr::Grouping { args, universe_indices } => {
            let indices: Vec<usize> = args
                .iter()
                .map(|arg| universe.iter().position(|u| u == arg).unwrap_or(usize::MAX))
                .collect();
            *universe_indices = Some(indices);
            // Recurse into args (GROUPING inside GROUPING is unusual but valid).
            for a in args.iter_mut() {
                populate_grouping_indices(a, universe);
            }
        }
        // Recurse into all compound variants.
        Expr::BinaryOp { left, right, .. } => {
            populate_grouping_indices(left, universe);
            populate_grouping_indices(right, universe);
        }
        Expr::UnaryOp { operand, .. } => populate_grouping_indices(operand, universe),
        Expr::Collate { expr, .. } => populate_grouping_indices(expr, universe),
        Expr::IsNull { expr, .. } => populate_grouping_indices(expr, universe),
        Expr::IsBoolean { expr, .. } => populate_grouping_indices(expr, universe),
        Expr::Between { expr, low, high, .. } => {
            populate_grouping_indices(expr, universe);
            populate_grouping_indices(low, universe);
            populate_grouping_indices(high, universe);
        }
        Expr::Like { expr, pattern, escape, .. } => {
            populate_grouping_indices(expr, universe);
            populate_grouping_indices(pattern, universe);
            if let Some(e) = escape { populate_grouping_indices(e, universe); }
        }
        Expr::In { expr, list, .. } => {
            populate_grouping_indices(expr, universe);
            for e in list { populate_grouping_indices(e, universe); }
        }
        Expr::Function { args, .. } => {
            for a in args { populate_grouping_indices(a, universe); }
        }
        Expr::Window { spec, .. } => {
            for e in &mut spec.partition_by {
                populate_grouping_indices(e, universe);
            }
            for item in &mut spec.order_by {
                populate_grouping_indices(&mut item.expr, universe);
            }
        }
        Expr::Case { operand, when_thens, else_result } => {
            if let Some(op) = operand { populate_grouping_indices(op, universe); }
            for (w, t) in when_thens {
                populate_grouping_indices(w, universe);
                populate_grouping_indices(t, universe);
            }
            if let Some(e) = else_result { populate_grouping_indices(e, universe); }
        }
        Expr::Cast { expr, .. } => populate_grouping_indices(expr, universe),
        Expr::GroupConcat { expr, order_by, .. } => {
            populate_grouping_indices(expr, universe);
            for (e, _) in order_by { populate_grouping_indices(e, universe); }
        }
        Expr::ArrayAgg { expr, order_by, .. } => {
            populate_grouping_indices(expr, universe);
            for (e, _) in order_by { populate_grouping_indices(e, universe); }
        }
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => {
            for e in elements {
                populate_grouping_indices(e, universe);
            }
        }
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript { array, index, slice } => {
            populate_grouping_indices(array, universe);
            populate_grouping_indices(index, universe);
            if let Some(s) = slice {
                populate_grouping_indices(s, universe);
            }
        }
        // Leaf nodes — nothing to recurse into.
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Default
        | Expr::Param { .. }
        | Expr::SqlJsonQuery { .. }
        | Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. } => {}
        // Phase 20.4, Step 7 — ANY/ALL: recurse into expr and array.
        Expr::AnyOf { expr, array } | Expr::AllOf { expr, array } => {
            populate_grouping_indices(expr, universe);
            populate_grouping_indices(array, universe);
        }
    }
}

/// Phase 11.20a — resolve references inside a `JSON_TABLE(...)` node.
///
/// Walks the `doc` expression and every `DEFAULT ...` expression inside the
/// column list's ON EMPTY / ON ERROR clauses. Uses the same resolver as
/// WHERE/ORDER BY so correlation through `OuterColumn` works consistently.
fn resolve_json_table(
    mut jt: crate::ast::JsonTable,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: &AnalyzeState<'_>,
) -> Result<crate::ast::JsonTable, DbError> {
    jt.doc = resolve_expr_full(jt.doc, ctx, outer_scopes, Some(state))?;
    // Phase 11.20d3: resolve PASSING expressions against outer scope so
    // correlated PASSING refs (`PASSING outer.col AS var`) bind properly.
    // Non-correlated PASSING (literals, params) resolves to itself.
    for (expr, _name) in &mut jt.passing {
        let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
        *expr = resolve_expr_full(taken, ctx, outer_scopes, Some(state))?;
    }
    for c in &mut jt.columns {
        if let crate::ast::JsonTableColumn::Regular {
            on_empty, on_error, ..
        } = c
        {
            resolve_on_behavior(on_empty, ctx, outer_scopes, state)?;
            resolve_on_behavior(on_error, ctx, outer_scopes, state)?;
        }
    }
    Ok(jt)
}

fn resolve_on_behavior(
    b: &mut crate::expr::SqlJsonOnBehavior,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: &AnalyzeState<'_>,
) -> Result<(), DbError> {
    if let crate::expr::SqlJsonOnBehavior::Default(boxed) = b {
        let inner = std::mem::replace(boxed.as_mut(), Expr::Literal(axiomdb_types::Value::Null));
        let resolved = resolve_expr_full(inner, ctx, outer_scopes, Some(state))?;
        *boxed.as_mut() = resolved;
    }
    Ok(())
}

/// Phase 20.1 — rewrite every `FromClause::Table` that resolves to a
/// `RelationKind::View` into a `FromClause::Subquery` containing the
/// parsed and analyzed view body. CTEs shadow views of the same name;
/// call this before `expand_ctes`.
///
/// `expanding` tracks the set of view names currently being expanded to
/// detect circular references.
#[allow(clippy::too_many_arguments)]
fn expand_views(
    s: &mut SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
    expanding: &mut std::collections::HashSet<String>,
) -> Result<(), DbError> {
    if let Some(from) = s.from.take() {
        s.from = Some(substitute_view_ref(
            from,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
            expanding,
        )?);
    }
    for join in &mut s.joins {
        let taken = std::mem::replace(
            &mut join.table,
            FromClause::Table(crate::ast::TableRef {
                database: None,
                schema: None,
                name: String::new(),
                alias: None,
            }),
        );
        join.table = substitute_view_ref(
            taken,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
            expanding,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn substitute_view_ref(
    from: FromClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
    expanding: &mut std::collections::HashSet<String>,
) -> Result<FromClause, DbError> {
    match from {
        FromClause::Table(ref tref) => {
            // Only expand unqualified or public-schema references.
            let schema = tref
                .schema
                .as_deref()
                .unwrap_or(default_schema);
            let database = tref
                .database
                .as_deref()
                .unwrap_or(default_database);
            let mut reader = CatalogReader::new(storage, snapshot.clone())?;
            let def = reader.get_table_in_database(database, schema, &tref.name)?;
            match def {
                Some(d) if d.is_view() => {
                    let view_name = format!("{}.{}", schema, tref.name);
                    if expanding.contains(&view_name) {
                        return Err(DbError::InvalidValue {
                            reason: format!("circular view reference: {}", tref.name),
                        });
                    }
                    expanding.insert(view_name.clone());

                    let query_sql = d.defining_query.clone().ok_or_else(|| DbError::Internal {
                        message: format!("view '{}' has no defining query", tref.name),
                    })?;
                    let parsed = crate::parse(&query_sql, None)?;
                    let view_select = match parsed {
                        Stmt::Select(sel) => sel,
                        other => {
                            return Err(DbError::Internal {
                                message: format!(
                                    "view '{}' defining query is not a SELECT: {other:?}",
                                    tref.name
                                ),
                            })
                        }
                    };
                    // Recursively expand views inside this view body.
                    let mut body = view_select;
                    expand_views(
                        &mut body,
                        storage,
                        snapshot.clone(),
                        default_database,
                        default_schema,
                        outer_scopes,
                        expanding,
                    )?;
                    expanding.remove(&view_name);

                    let alias = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                    Ok(FromClause::Subquery {
                        query: Box::new(body),
                        alias,
                        lateral: false,
                    })
                }
                _ => Ok(from),
            }
        }
        // Recurse into subqueries to expand views inside them.
        FromClause::Subquery { query, alias, lateral } => {
            let mut body = *query;
            expand_views(
                &mut body,
                storage,
                snapshot,
                default_database,
                default_schema,
                outer_scopes,
                expanding,
            )?;
            Ok(FromClause::Subquery {
                query: Box::new(body),
                alias,
                lateral,
            })
        }
        other => Ok(other),
    }
}

/// Phase 21.2 — analyze each CTE body in order, then rewrite every
/// reference in `s.from` / `s.joins` that resolves to a CTE name into
/// `FromClause::Subquery`. Clears `s.with_ctes` after substitution so
/// downstream code never sees non-empty entries.
fn expand_ctes(
    s: &mut SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
) -> Result<(), DbError> {
    use std::collections::HashMap;

    let bindings = std::mem::take(&mut s.with_ctes);
    let mut dict: HashMap<String, Box<SelectStmt>> = HashMap::new();
    // Phase 21.3b — recursive CTEs register here; substitute_cte_ref
    // rewrites matching Table refs into FromClause::RecursiveCte.
    let mut recursive_dict: HashMap<String, Box<crate::ast::RecursiveCteClause>> = HashMap::new();

    for cte in bindings {
        if cte.recursive {
            let step = cte
                .recursive_step
                .clone()
                .ok_or_else(|| DbError::ParseError {
                    message: format!(
                        "WITH RECURSIVE `{}`: body must be `SELECT base UNION [ALL] SELECT step`",
                        cte.name
                    ),
                    position: None,
                })?;
            let mut base_body = *cte.query.clone();
            base_body.with_ctes = dict
                .iter()
                .map(|(name, q)| crate::ast::CteBinding {
                    name: name.clone(),
                    column_names: None,
                    query: q.clone(),
                    recursive: false,
                    recursive_step: None,
                    recursive_union_all: false,
                })
                .collect();
            let analyzed_base = analyze_select_with_outer(
                base_body,
                storage,
                snapshot.clone(),
                default_database,
                default_schema,
                outer_scopes,
            )?;
            let analyzed_base = if let Some(ref names) = cte.column_names {
                apply_cte_column_rename(analyzed_base, names)?
            } else {
                analyzed_base
            };
            let clause = crate::ast::RecursiveCteClause {
                alias: cte.name.clone(),
                column_names: cte.column_names.clone(),
                base: Box::new(analyzed_base),
                step,
                union_all: cte.recursive_union_all,
            };
            recursive_dict.insert(cte.name.to_ascii_lowercase(), Box::new(clause));
            continue;
        }

        // Allow the current CTE body to reference previously-analyzed
        // CTEs by re-attaching them as with_ctes in the body (analyzer
        // recursion will substitute them there too).
        let mut body = *cte.query;
        body.with_ctes = dict
            .iter()
            .map(|(name, q)| crate::ast::CteBinding {
                name: name.clone(),
                column_names: None,
                query: q.clone(),
                recursive: false,
                recursive_step: None,
                recursive_union_all: false,
            })
            .collect();

        let analyzed = analyze_select_with_outer(
            body,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            outer_scopes,
        )?;

        let analyzed = if let Some(col_override) = cte.column_names {
            apply_cte_column_rename(analyzed, &col_override)?
        } else {
            analyzed
        };

        dict.insert(cte.name.to_ascii_lowercase(), Box::new(analyzed));
    }

    // Substitute references in FROM and each join's FromClause.
    if let Some(from) = s.from.take() {
        s.from = Some(substitute_cte_ref(from, &dict, &recursive_dict));
    }
    for join in &mut s.joins {
        let taken = std::mem::replace(
            &mut join.table,
            FromClause::Table(crate::ast::TableRef {
                database: None,
                schema: None,
                name: String::new(),
                alias: None,
            }),
        );
        join.table = substitute_cte_ref(taken, &dict, &recursive_dict);
    }

    Ok(())
}

fn substitute_cte_ref(
    from: FromClause,
    dict: &std::collections::HashMap<String, Box<SelectStmt>>,
    recursive_dict: &std::collections::HashMap<String, Box<crate::ast::RecursiveCteClause>>,
) -> FromClause {
    match from {
        FromClause::Table(tref) if tref.database.is_none() && tref.schema.is_none() => {
            let key = tref.name.to_ascii_lowercase();
            if let Some(clause) = recursive_dict.get(&key) {
                let mut c = (**clause).clone();
                if let Some(alias) = tref.alias.clone() {
                    c.alias = alias;
                }
                return FromClause::RecursiveCte(Box::new(c));
            }
            if let Some(body) = dict.get(&key) {
                let alias = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                return FromClause::Subquery {
                    query: body.clone(),
                    alias,
                    lateral: false,
                };
            }
            FromClause::Table(tref)
        }
        other => other,
    }
}

fn apply_cte_column_rename(mut s: SelectStmt, names: &[String]) -> Result<SelectStmt, DbError> {
    // Count non-wildcard select items to compare width.
    let item_count: usize = s.columns.iter().map(|_| 1).sum();
    if item_count != names.len() {
        return Err(DbError::ParseError {
            message: format!(
                "CTE column-name list has {} name(s) but select list produces {} column(s)",
                names.len(),
                item_count,
            ),
            position: None,
        });
    }
    // Positionally reassign alias on each SelectItem::Expr. Wildcards are
    // not renamable at this stage (analyzer has already expanded them).
    for (item, new_name) in s.columns.iter_mut().zip(names.iter()) {
        if let crate::ast::SelectItem::Expr { alias, .. } = item {
            *alias = Some(new_name.clone());
        }
    }
    Ok(s)
}
