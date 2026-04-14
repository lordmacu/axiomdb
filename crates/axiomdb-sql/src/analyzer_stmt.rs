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
        Stmt::CreateTable(s) => {
            analyze_create_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::CreateTable)
        }
        // CREATE TABLE LIKE — no column resolution needed; source table is resolved at execution
        Stmt::CreateTableLike(_) => Ok(stmt),
        // CREATE TABLE AS SELECT — analyze the inner SELECT
        Stmt::CreateTableAsSelect(mut s) => {
            s.select =
                analyze_select(s.select, storage, snapshot, default_database, default_schema)?;
            Ok(Stmt::CreateTableAsSelect(s))
        }
        Stmt::DropTable(s) => {
            analyze_drop_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::DropTable)
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
            let first = analyze_select(first, storage, snapshot.clone(), default_database, default_schema)?;
            let rest: Result<Vec<_>, _> = rest
                .into_iter()
                .map(|t| {
                    analyze_select(t.select, storage, snapshot.clone(), default_database, default_schema)
                        .map(|select| crate::ast::SetOpTail { kind: t.kind, all: t.all, select })
                })
                .collect();
            Ok(Stmt::SetOp { first, rest: rest? })
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
        | Stmt::DropTable(_)
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
    let ctx = build_context(
        &s.from,
        &s.joins,
        storage,
        snapshot.clone(),
        default_database,
        default_schema,
    )?;

    // If FROM is a derived table (subquery in FROM), `build_context` analyzed
    // the inner query to extract virtual column names, but did NOT store the
    // analyzed version back into `s.from`. Fix that here so the executor
    // receives the analyzed inner query with correct `col_idx` values.
    if let Some(FromClause::Subquery { query, alias }) = s.from {
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
        });
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
            FromClause::Subquery { query, alias } => {
                let analyzed_inner = analyze_select_with_outer(
                    *query,
                    storage,
                    state.snapshot.clone(),
                    state.default_database,
                    state.default_schema,
                    outer_scopes,
                )?;
                join.table = FromClause::Subquery {
                    query: Box::new(analyzed_inner),
                    alias,
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
                let taken = std::mem::replace(
                    &mut srf.doc,
                    Expr::Literal(axiomdb_types::Value::Null),
                );
                srf.doc = resolve_expr_full(taken, &ctx, outer_scopes, Some(&state))?;
                join.table = FromClause::JsonbSrf(srf);
            }
            // Phase 21.22 — VALUES rows resolve against an empty scope
            // (no correlation in this subphase).
            FromClause::Values(mut vc) => {
                let empty_ctx = BindContext::empty();
                for row in &mut vc.rows {
                    for e in row {
                        let taken = std::mem::replace(
                            e,
                            Expr::Literal(axiomdb_types::Value::Null),
                        );
                        *e = resolve_expr_full(taken, &empty_ctx, &[], Some(&state))?;
                    }
                }
                join.table = FromClause::Values(vc);
            }
            FromClause::Table(_) => {}
            // Phase 21.3 — recursive CTE already pre-analyzed by expand_ctes.
            FromClause::RecursiveCte(_) => {}
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
                    message: "NATURAL JOIN: analyzer scope out of sync with join list"
                        .into(),
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
                    if right_cols
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(&col.name))
                    {
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
    s.group_by = s
        .group_by
        .into_iter()
        .map(|e| resolve_expr_full(e, &ctx, outer_scopes, Some(&state)))
        .collect::<Result<_, _>>()?;
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
        let inner = std::mem::replace(
            boxed.as_mut(),
            Expr::Literal(axiomdb_types::Value::Null),
        );
        let resolved = resolve_expr_full(inner, ctx, outer_scopes, Some(state))?;
        *boxed.as_mut() = resolved;
    }
    Ok(())
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

    for cte in bindings {
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
        s.from = Some(substitute_cte_ref(from, &dict));
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
        join.table = substitute_cte_ref(taken, &dict);
    }

    Ok(())
}

fn substitute_cte_ref(
    from: FromClause,
    dict: &std::collections::HashMap<String, Box<SelectStmt>>,
) -> FromClause {
    match from {
        FromClause::Table(tref)
            if tref.database.is_none() && tref.schema.is_none() =>
        {
            if let Some(body) = dict.get(&tref.name.to_ascii_lowercase()) {
                let alias = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                return FromClause::Subquery {
                    query: body.clone(),
                    alias,
                };
            }
            FromClause::Table(tref)
        }
        other => other,
    }
}

fn apply_cte_column_rename(
    mut s: SelectStmt,
    names: &[String],
) -> Result<SelectStmt, DbError> {
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
