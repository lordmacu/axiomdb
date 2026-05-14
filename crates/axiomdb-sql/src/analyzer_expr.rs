// ── Expression resolution ─────────────────────────────────────────────────────

/// Storage + snapshot context threaded through `resolve_expr_full` so that
/// subquery arms can call `analyze_select_with_outer`.
///
/// `None` is used on pure paths (UPDATE/DELETE/INSERT) where subqueries are not
/// expected; subquery arms return `DbError::NotImplemented` when `state` is `None`.
struct AnalyzeState<'a> {
    storage: &'a dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &'a str,
    default_schema: &'a str,
}

/// Thin wrapper — resolves `expr` in `ctx` with no outer scopes and no state.
///
/// Sufficient for all non-subquery expressions (UPDATE SET, DELETE WHERE, etc.).
fn resolve_expr(expr: Expr, ctx: &BindContext) -> Result<Expr, DbError> {
    resolve_expr_full(expr, ctx, &[], None)
}

/// Resolves column references in `expr` against `ctx` (inner scope) and
/// `outer_scopes` (enclosing query scopes, outermost last).
///
/// `state` must be `Some(...)` for any query that may contain subquery expressions
/// (`Expr::Subquery`, `Expr::InSubquery`, `Expr::Exists`). When `state` is `None`
/// those arms return `DbError::NotImplemented`.
///
/// When a column is found in an outer scope but not `ctx`, it is emitted as
/// `Expr::OuterColumn` — a correlated reference the executor substitutes with the
/// outer row before executing the inner query.
///
/// Only depth-1 correlation is supported (Phase 4.11). Deeper nesting returns
/// `DbError::NotImplemented`.
fn resolve_expr_full(
    expr: Expr,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: Option<&AnalyzeState<'_>>,
) -> Result<Expr, DbError> {
    match expr {
        Expr::Literal(v) => Ok(Expr::Literal(v)),
        Expr::Default => Ok(Expr::Default),

        // Phase 11.19a: SQL/JSON query special form. Walk the doc
        // sub-expression and any DEFAULT handlers in on_empty / on_error.
        Expr::SqlJsonQuery {
            kind,
            doc,
            path,
            path_mode,
            passing,
            returning,
            wrapper,
            quotes,
            on_empty,
            on_error,
        } => {
            let doc = resolve_expr_full(*doc, ctx, outer_scopes, state)?;
            let mut resolved_passing = Vec::with_capacity(passing.len());
            for (e, name) in passing {
                resolved_passing.push((
                    resolve_expr_full(e, ctx, outer_scopes, state)?,
                    name,
                ));
            }
            let on_empty = resolve_sql_json_behavior(on_empty, ctx, outer_scopes, state)?;
            let on_error = resolve_sql_json_behavior(on_error, ctx, outer_scopes, state)?;
            Ok(Expr::SqlJsonQuery {
                kind,
                doc: Box::new(doc),
                path,
                path_mode,
                passing: resolved_passing,
                returning,
                wrapper,
                quotes,
                on_empty,
                on_error,
            })
        }

        Expr::InsertValue { col_idx: _, name } => {
            // Resolve the proposed-row column against the current target
            // table's schema. Out-of-scope contexts surface as ColumnNotFound.
            if !ctx.tables.is_empty() {
                if let Ok(idx) = ctx.resolve_column(&name) {
                    return Ok(Expr::InsertValue { col_idx: idx, name });
                }
            }
            Err(DbError::ColumnNotFound {
                name: name.clone(),
                table: "VALUES()".into(),
            })
        }
        Expr::ExcludedValue { col_idx: _, name } => {
            if !ctx.tables.is_empty() {
                if let Ok(idx) = ctx.resolve_column(&name) {
                    return Ok(Expr::ExcludedValue { col_idx: idx, name });
                }
            }
            Err(DbError::ColumnNotFound {
                name: name.clone(),
                table: "EXCLUDED".into(),
            })
        }
        Expr::Column { col_idx: _, name } => {
            // 1. Try the inner (current) scope first.
            if !ctx.tables.is_empty() {
                if let Ok((idx, col)) = ctx.resolve_column_with_def(&name) {
                    let resolved = Expr::Column { col_idx: idx, name };
                    return Ok(match col.collation.as_deref() {
                        Some(collation) => Expr::Collate {
                            expr: Box::new(resolved),
                            collation: collation.to_string(),
                        },
                        None => resolved,
                    });
                }
            }
            // 2. Try outer scopes — emit OuterColumn if found.
            // `outer_scopes` is ordered outermost→innermost (see subquery arms).
            // `depth` is measured from the immediate enclosing query:
            // innermost outer = 0, next = 1, etc.
            let n_outer = outer_scopes.len();
            for (i, outer_ctx) in outer_scopes.iter().enumerate() {
                if let Ok(idx) = outer_ctx.resolve_column(&name) {
                    let depth = (n_outer - 1 - i) as u16;
                    return Ok(Expr::OuterColumn {
                        col_idx: idx,
                        name,
                        depth,
                    });
                }
            }
            // 3. Not found anywhere.
            if ctx.tables.is_empty() && outer_scopes.is_empty() {
                return Err(DbError::ColumnNotFound {
                    name: name.clone(),
                    table: "no tables in scope (missing FROM clause)".into(),
                });
            }
            // Delegate to ctx.resolve_column for the best error message.
            ctx.resolve_column(&name).map(|_| unreachable!())?;
            unreachable!()
        }

        Expr::OuterColumn { .. } => Ok(expr), // already resolved — pass through

        // Prepared statement parameter: type is determined at execute time.
        // No column resolution needed — pass through unchanged.
        Expr::Param { .. } => Ok(expr),

        Expr::UnaryOp { op, operand } => Ok(Expr::UnaryOp {
            op,
            operand: Box::new(resolve_expr_full(*operand, ctx, outer_scopes, state)?),
        }),

        Expr::Collate { expr, collation } => Ok(Expr::Collate {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            collation,
        }),

        Expr::BinaryOp { op, left, right } => Ok(Expr::BinaryOp {
            op,
            left: Box::new(resolve_expr_full(*left, ctx, outer_scopes, state)?),
            right: Box::new(resolve_expr_full(*right, ctx, outer_scopes, state)?),
        }),

        Expr::IsNull { expr, negated } => Ok(Expr::IsNull {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            negated,
        }),

        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => Ok(Expr::IsBoolean {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            value,
            negated,
        }),

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            low: Box::new(resolve_expr_full(*low, ctx, outer_scopes, state)?),
            high: Box::new(resolve_expr_full(*high, ctx, outer_scopes, state)?),
            negated,
        }),

        Expr::Like {
            expr,
            pattern,
            negated,
            escape,
        } => Ok(Expr::Like {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            pattern: Box::new(resolve_expr_full(*pattern, ctx, outer_scopes, state)?),
            negated,
            escape: escape
                .map(|e| resolve_expr_full(*e, ctx, outer_scopes, state).map(Box::new))
                .transpose()?,
        }),

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let expr = Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?);
            let list = list
                .into_iter()
                .map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::In {
                expr,
                list,
                negated,
            })
        }

        Expr::Function { name, args } => {
            let args = args
                .into_iter()
                .map(|a| resolve_expr_full(a, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Function { name, args })
        }

        Expr::Window { func, spec } => {
            let partition_by = spec
                .partition_by
                .into_iter()
                .map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            let mut order_by = Vec::with_capacity(spec.order_by.len());
            for mut item in spec.order_by {
                item.expr = resolve_expr_full(item.expr, ctx, outer_scopes, state)?;
                order_by.push(item);
            }
            Ok(Expr::Window {
                func,
                spec: crate::ast::WindowSpec {
                    partition_by,
                    order_by,
                },
            })
        }

        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let operand = operand
                .map(|e| resolve_expr_full(*e, ctx, outer_scopes, state).map(Box::new))
                .transpose()?;
            let when_thens = when_thens
                .into_iter()
                .map(|(w, t)| {
                    Ok((
                        resolve_expr_full(w, ctx, outer_scopes, state)?,
                        resolve_expr_full(t, ctx, outer_scopes, state)?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let else_result = else_result
                .map(|e| resolve_expr_full(*e, ctx, outer_scopes, state).map(Box::new))
                .transpose()?;
            Ok(Expr::Case {
                operand,
                when_thens,
                else_result,
            })
        }

        Expr::Cast { expr, target } => Ok(Expr::Cast {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            target,
        }),

        Expr::GroupConcat {
            expr,
            distinct,
            order_by,
            separator,
        } => {
            let expr = resolve_expr_full(*expr, ctx, outer_scopes, state)?;
            let order_by = order_by
                .into_iter()
                .map(|(e, dir)| Ok((resolve_expr_full(e, ctx, outer_scopes, state)?, dir)))
                .collect::<Result<Vec<_>, DbError>>()?;
            Ok(Expr::GroupConcat {
                expr: Box::new(expr),
                distinct,
                order_by,
                separator,
            })
        }

        // ── Subquery variants ────────────────────────────────────────────────
        //
        // The inner SELECT is analyzed with `ctx` pushed as an outer scope so
        // that column references to the enclosing query become `OuterColumn`.
        // Depth-1 correlated subqueries are fully supported.
        // Depth > 1 correlated subqueries fail at executor time with a clear
        // "unsubstituted OuterColumn" error (Phase 6 will fix this).
        // Uncorrelated subqueries at any depth work correctly.
        Expr::Subquery(inner) => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *inner,
                st.storage,
                st.snapshot.clone(),
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::Subquery(Box::new(analyzed)))
        }

        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let expr = Box::new(resolve_expr_full(*expr, ctx, outer_scopes, Some(st))?);
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *query,
                st.storage,
                st.snapshot.clone(),
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::InSubquery {
                expr,
                query: Box::new(analyzed),
                negated,
            })
        }

        Expr::Exists { query, negated } => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *query,
                st.storage,
                st.snapshot.clone(),
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::Exists {
                query: Box::new(analyzed),
                negated,
            })
        }

        // GROUPING(expr, ...) — resolve each arg; universe_indices are populated
        // later by a post-pass in analyzer_stmt once the GROUP BY universe is known.
        Expr::Grouping { args, .. } => {
            let resolved_args = args
                .into_iter()
                .map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Grouping {
                args: resolved_args,
                universe_indices: None,
            })
        }

        // Phase 20.4 — ARRAY[expr, ...] constructor: resolve each element.
        Expr::ArrayConstructor { elements } => {
            let resolved_elements = elements
                .into_iter()
                .map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::ArrayConstructor { elements: resolved_elements })
        }

        // Phase 20.4, Step 5 — array subscript/slice: resolve array and index expressions.
        Expr::Subscript { array, index, slice } => {
            let resolved_array = resolve_expr_full(*array, ctx, outer_scopes, state)?;
            let resolved_index = resolve_expr_full(*index, ctx, outer_scopes, state)?;
            let resolved_slice = slice
                .map(|s| resolve_expr_full(*s, ctx, outer_scopes, state))
                .map(|r| r.map(Box::new))
                .transpose()?;
            Ok(Expr::Subscript {
                array: Box::new(resolved_array),
                index: Box::new(resolved_index),
                slice: resolved_slice,
            })
        }

        // Phase 20.4, Step 7 — ANY/ALL: resolve both the expr and array sub-expressions.
        Expr::AnyOf { expr, array } => {
            let resolved_expr = resolve_expr_full(*expr, ctx, outer_scopes, state)?;
            let resolved_array = resolve_expr_full(*array, ctx, outer_scopes, state)?;
            Ok(Expr::AnyOf {
                expr: Box::new(resolved_expr),
                array: Box::new(resolved_array),
            })
        }

        Expr::AllOf { expr, array } => {
            let resolved_expr = resolve_expr_full(*expr, ctx, outer_scopes, state)?;
            let resolved_array = resolve_expr_full(*array, ctx, outer_scopes, state)?;
            Ok(Expr::AllOf {
                expr: Box::new(resolved_expr),
                array: Box::new(resolved_array),
            })
        }
    }
}

/// Convenience wrapper for `Option<Expr>` with no outer scopes and no state.
fn resolve_opt_expr(expr: Option<Expr>, ctx: &BindContext) -> Result<Option<Expr>, DbError> {
    expr.map(|e| resolve_expr(e, ctx)).transpose()
}

/// Convenience wrapper for `Option<Expr>` with full state threading.
fn resolve_opt_expr_full(
    expr: Option<Expr>,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: Option<&AnalyzeState<'_>>,
) -> Result<Option<Expr>, DbError> {
    expr.map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
        .transpose()
}

/// Resolves any `Default(expr)` inside a [`crate::expr::SqlJsonOnBehavior`]
/// so the DEFAULT expression sees the surrounding scope. All other variants
/// pass through unchanged.
fn resolve_sql_json_behavior(
    behavior: crate::expr::SqlJsonOnBehavior,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: Option<&AnalyzeState<'_>>,
) -> Result<crate::expr::SqlJsonOnBehavior, DbError> {
    match behavior {
        crate::expr::SqlJsonOnBehavior::Default(e) => {
            let resolved = resolve_expr_full(*e, ctx, outer_scopes, state)?;
            Ok(crate::expr::SqlJsonOnBehavior::Default(Box::new(resolved)))
        }
        other => Ok(other),
    }
}
