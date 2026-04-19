fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "min" | "max" | "avg"
            | "count_distinct" | "sum_distinct" | "avg_distinct"
            // Phase 11.25b — PG + MySQL JSON aggregates.
            | "jsonb_agg" | "json_agg" | "json_arrayagg"
            | "jsonb_object_agg" | "json_object_agg" | "json_objectagg"
    )
}

/// Phase 11.25b — true when the aggregate is a 2-arg object aggregate.
fn is_object_agg(name: &str) -> bool {
    matches!(name, "jsonb_object_agg" | "json_object_agg" | "json_objectagg")
}

/// Phase 11.25b — true when the aggregate is a 1-arg array aggregate.
#[allow(dead_code)]
fn is_array_agg(name: &str) -> bool {
    matches!(name, "jsonb_agg" | "json_agg" | "json_arrayagg")
}

/// Returns `true` if `expr` or any sub-expression is an aggregate call.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        // GROUP_CONCAT is always an aggregate — detected via the dedicated AST variant.
        Expr::GroupConcat { .. } => true,
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { operand, .. } => contains_aggregate(operand),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::IsBoolean { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::In { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Function { args, .. } => args.iter().any(contains_aggregate),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || when_thens
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Literal(_)
        | Expr::Default
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Param { .. } => false,
        // Subquery internals are analyzed independently; aggregates inside them
        // do not count as aggregates of the outer query.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => false,
        // Phase 11.19a: SQL/JSON special form. Aggregates are not allowed
        // inside its clauses in this MVP (PG allows in DEFAULT but it is
        // pathological); treat as a leaf for aggregation analysis.
        Expr::SqlJsonQuery { .. } => false,
        // GROUPING() is not an aggregate function — it reads a hidden mask column.
        Expr::Grouping { .. } => false,
    }
}

/// Returns `true` if the SELECT list or HAVING clause contain any aggregate call.
fn has_aggregates(items: &[SelectItem], having: &Option<Expr>) -> bool {
    let in_select = items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        _ => false,
    });
    let in_having = having.as_ref().is_some_and(contains_aggregate);
    in_select || in_having
}

// ── Aggregate descriptor ──────────────────────────────────────────────────────

/// Descriptor for one aggregate expression in the query.
///
/// Collected from the SELECT list and HAVING clause before the scan loop.
/// Deduplicated: if `COUNT(*)` appears in both SELECT and HAVING, only one
/// `AggExpr` is created and both share the same accumulator index.
#[derive(Debug, Clone)]
enum AggExpr {
    /// Standard aggregate: COUNT, SUM, MIN, MAX, AVG.
    Simple {
        /// Lowercase function name: "count", "sum", "min", "max", "avg".
        name: String,
        /// The argument expression. `None` for `COUNT(*)`.
        arg: Option<Expr>,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
    /// GROUP_CONCAT / string_agg aggregate.
    GroupConcat {
        /// Expression to evaluate and concatenate per row.
        expr: Box<Expr>,
        /// If true, deduplicate values before concatenating.
        distinct: bool,
        /// Per-aggregate ORDER BY: (sort_expr, direction) pairs.
        order_by: Vec<(Expr, crate::ast::SortOrder)>,
        /// Separator string (default `","`).
        separator: String,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
    /// Phase 11.25b — 2-arg object aggregate: `jsonb_object_agg(key, value)`,
    /// `json_object_agg(key, value)`, MySQL `JSON_OBJECTAGG(key, value)`.
    JsonbObjectAgg {
        key_expr: Box<Expr>,
        value_expr: Box<Expr>,
        /// Returns JSON text when `false` (MySQL `JSON_OBJECTAGG`, PG
        /// `json_object_agg`), JSONB binary when `true` (PG `jsonb_object_agg`).
        returns_jsonb: bool,
        #[allow(dead_code)]
        agg_idx: usize,
    },
}

impl AggExpr {
    /// Returns the accumulator index for this aggregate.
    #[allow(dead_code)]
    fn agg_idx(&self) -> usize {
        match self {
            Self::Simple { agg_idx, .. }
            | Self::GroupConcat { agg_idx, .. }
            | Self::JsonbObjectAgg { agg_idx, .. } => *agg_idx,
        }
    }

    /// Returns `true` if this descriptor matches the given simple function call.
    fn matches_simple(&self, name: &str, args: &[Expr]) -> bool {
        match self {
            Self::Simple { name: n, arg, .. } => {
                if n != name {
                    return false;
                }
                match (arg, args.first()) {
                    // Both COUNT(*): arg = None, args is empty
                    (None, None) => args.is_empty(),
                    // Both have an argument — compare by col_idx if both are Column refs
                    (
                        Some(Expr::Column { col_idx: a, .. }),
                        Some(Expr::Column { col_idx: b, .. }),
                    ) => a == b,
                    _ => false,
                }
            }
            Self::GroupConcat { .. } => false,
            Self::JsonbObjectAgg { .. } => false,
        }
    }

    /// Phase 11.25b — match for 2-arg object aggregates.
    fn matches_object_agg(
        &self,
        name: &str,
        args: &[Expr],
    ) -> bool {
        match self {
            Self::JsonbObjectAgg {
                key_expr,
                value_expr,
                returns_jsonb,
                ..
            } => {
                if args.len() != 2 {
                    return false;
                }
                let expected_jsonb = matches!(name, "jsonb_object_agg");
                if expected_jsonb != *returns_jsonb {
                    return false;
                }
                key_expr.as_ref() == &args[0] && value_expr.as_ref() == &args[1]
            }
            _ => false,
        }
    }

    /// Returns `true` if this descriptor matches the given GROUP_CONCAT call.
    fn matches_group_concat(
        &self,
        gc_expr: &Expr,
        distinct: bool,
        order_by: &[(Expr, crate::ast::SortOrder)],
        separator: &str,
    ) -> bool {
        match self {
            Self::GroupConcat {
                expr,
                distinct: d,
                order_by: ob,
                separator: sep,
                ..
            } => {
                expr.as_ref() == gc_expr
                    && *d == distinct
                    && ob == order_by
                    && sep.as_str() == separator
            }
            Self::Simple { .. } | Self::JsonbObjectAgg { .. } => false,
        }
    }
}

/// Walks `expr` and registers any aggregate function calls into `result`.
fn collect_agg_exprs_from(expr: &Expr, result: &mut Vec<AggExpr>) {
    match expr {
        // GROUP_CONCAT: register as GroupConcat AggExpr and deduplicate.
        // Do NOT recurse into `gc_expr` itself (it IS the aggregate root).
        // Only recurse into ORDER BY sub-exprs (they could contain subqueries, etc.).
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let already = result
                .iter()
                .any(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator));
            if !already {
                let idx = result.len();
                result.push(AggExpr::GroupConcat {
                    expr: gc_expr.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    separator: separator.clone(),
                    agg_idx: idx,
                });
            }
            for (e, _) in order_by {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let lower = name.to_ascii_lowercase();
            // Phase 11.25b — 2-arg object aggregates.
            if is_object_agg(&lower) {
                if args.len() != 2 {
                    // Parse error / analyzer will catch; emit no descriptor.
                    return;
                }
                let already = result
                    .iter()
                    .any(|ae| ae.matches_object_agg(&lower, args));
                if !already {
                    let idx = result.len();
                    let returns_jsonb = matches!(lower.as_str(), "jsonb_object_agg");
                    result.push(AggExpr::JsonbObjectAgg {
                        key_expr: Box::new(args[0].clone()),
                        value_expr: Box::new(args[1].clone()),
                        returns_jsonb,
                        agg_idx: idx,
                    });
                }
                return;
            }
            // Phase 11.25b — 1-arg array aggregates piggyback on Simple via
            // a normalized name so dedup by arg works. Keep the original
            // name in the AggExpr so the accumulator can distinguish
            // JSON vs JSONB output.
            let arg = args.first().cloned();
            // Deduplicate: only add if not already registered.
            let already = result
                .iter()
                .any(|ae| ae.matches_simple(&lower, args));
            if !already {
                let idx = result.len();
                result.push(AggExpr::Simple {
                    name: lower,
                    arg,
                    agg_idx: idx,
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, result);
            collect_agg_exprs_from(right, result);
        }
        Expr::UnaryOp { operand, .. } => collect_agg_exprs_from(operand, result),
        Expr::IsNull { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::IsBoolean { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(low, result);
            collect_agg_exprs_from(high, result);
        }
        Expr::In { expr, list, .. } => {
            collect_agg_exprs_from(expr, result);
            for e in list {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_agg_exprs_from(a, result);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_agg_exprs_from(op, result);
            }
            for (w, t) in when_thens {
                collect_agg_exprs_from(w, result);
                collect_agg_exprs_from(t, result);
            }
            if let Some(e) = else_result {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Cast { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Like { expr, pattern, .. } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(pattern, result);
        }
        Expr::Literal(_)
        | Expr::Default
        | Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::SqlJsonQuery { .. }
        | Expr::Param { .. } => {}
        // Aggregates inside a subquery belong to the inner query, not the outer.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {}
        // GROUPING() is not an aggregate; recurse into its args.
        Expr::Grouping { args, .. } => {
            for a in args { collect_agg_exprs_from(a, result); }
        }
    }
}

/// Builds the deduplicated list of aggregate expressions from SELECT + HAVING.
fn collect_agg_exprs(items: &[SelectItem], having: &Option<Expr>) -> Vec<AggExpr> {
    let mut result = Vec::new();
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_agg_exprs_from(expr, &mut result);
        }
    }
    if let Some(h) = having {
        collect_agg_exprs_from(h, &mut result);
    }
    result
}
