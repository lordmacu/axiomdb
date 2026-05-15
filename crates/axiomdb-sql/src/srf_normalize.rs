/// Phase 20.14 — Pre-analysis rewrite pass for set-returning functions (SRFs)
/// in the SELECT projection list.
///
/// `UNNEST(array_expr)` at the top level of the SELECT list is rewritten to an
/// implicit LATERAL UNNEST join before the semantic analyzer builds its bind
/// context. This lets the existing LATERAL UNNEST executor handle all execution
/// with no new code paths.
///
/// The rewrite is transparent: downstream code (analyzer, executor) sees a
/// perfectly ordinary `FromClause::Unnest` join after this pass runs.
use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::ast::{FromClause, JoinClause, JoinCondition, JoinType, SelectItem, SelectStmt,
                  UnnestClause};
use crate::expr::Expr;

/// Rewrite UNNEST calls in the SELECT list to an implicit LATERAL UNNEST join.
///
/// Called at the start of `analyze_select_with_outer`, after `expand_ctes` and
/// before `build_context`, so the injected join is visible to column resolution.
///
/// Rules:
/// - Only top-level `SelectItem::Expr { Expr::Function { name: "unnest", args: [one] } }`
///   items are rewritten. Nested UNNEST (inside arithmetic, CASE, etc.) is ignored.
/// - All UNNEST calls in the same SELECT list are combined into ONE `UnnestClause`
///   so they are zipped rather than cross-joined (PostgreSQL semantics).
/// - Zero-argument UNNEST → `DbError::InvalidValue`.
/// - Multi-argument UNNEST in SELECT list → `DbError::InvalidValue` (use FROM UNNEST
///   for multi-array zip with explicit column names).
pub fn normalize_select_srf(s: &mut SelectStmt) -> Result<(), DbError> {
    // ── Collect UNNEST calls ──────────────────────────────────────────────────
    let mut srf_positions: Vec<usize> = Vec::new();
    let mut srf_arrays: Vec<Expr> = Vec::new();
    let mut srf_user_aliases: Vec<Option<String>> = Vec::new();

    for (i, item) in s.columns.iter().enumerate() {
        if let SelectItem::Expr {
            expr: Expr::Function { name, args },
            alias,
        } = item
        {
            if name.eq_ignore_ascii_case("unnest") {
                match args.len() {
                    0 => {
                        return Err(DbError::InvalidValue {
                            reason: "UNNEST requires exactly one argument".into(),
                        });
                    }
                    1 => {
                        srf_positions.push(i);
                        srf_arrays.push(args[0].clone());
                        srf_user_aliases.push(alias.clone());
                    }
                    _ => {
                        return Err(DbError::InvalidValue {
                            reason: "UNNEST in SELECT list takes exactly one argument; \
                                     use UNNEST(a, b) in FROM for multi-array zip"
                                .into(),
                        });
                    }
                }
            }
        }
    }

    if srf_positions.is_empty() {
        return Ok(());
    }

    // ── Determine output names (= UnnestClause column names = SelectItem alias) ──
    // Using the user alias (or the PostgreSQL default "unnest"/"unnest_N") as the
    // internal column name means ORDER BY can reference these names directly, since
    // the BindContext exposes them under the injected `__srf__` table.
    let output_names: Vec<String> = srf_user_aliases
        .iter()
        .enumerate()
        .map(|(seq, alias)| {
            alias.clone().unwrap_or_else(|| {
                if seq == 0 {
                    "unnest".into()
                } else {
                    format!("unnest_{seq}")
                }
            })
        })
        .collect();

    // ── Build one UnnestClause for all collected arrays (zip semantics) ───────
    let unnest_clause = UnnestClause {
        exprs: srf_arrays,
        alias: Some("__srf__".into()),
        column_names: output_names.clone(), // output names == internal column names
        lateral: true, // may reference outer table columns
    };

    // ── Replace UNNEST exprs with Column refs; pin output aliases ────────────
    for (seq, &pos) in srf_positions.iter().enumerate() {
        if let SelectItem::Expr { expr, alias } = &mut s.columns[pos] {
            // Column name matches what the BindContext exposes, so ORDER BY
            // on the alias resolves without a special alias-lookup pass.
            *expr = Expr::Column {
                col_idx: 0, // resolved to the correct offset by the analyzer
                name: output_names[seq].clone(),
            };
            // Override alias to the output name (normalizes user alias vs default).
            *alias = Some(output_names[seq].clone());
        }
    }

    // ── Inject UnnestClause into FROM / joins ─────────────────────────────────
    if s.from.is_none() {
        // UNNEST becomes the sole FROM source (e.g. `SELECT UNNEST(ARRAY[1,2])`).
        s.from = Some(FromClause::Unnest(Box::new(unnest_clause)));
    } else {
        // Implicit LATERAL cross join appended to the existing join list.
        // `lateral: true` on the UnnestClause signals the executor to check
        // `unnest_is_correlated` and re-materialize per outer row when the
        // array expression references an outer table column.
        s.joins.push(JoinClause {
            join_type: JoinType::Inner,
            table: FromClause::Unnest(Box::new(unnest_clause)),
            condition: JoinCondition::On(Expr::Literal(Value::Bool(true))),
            natural: false,
        });
    }

    Ok(())
}
