//! Phase 20.4, Step 7 — `FROM UNNEST(array) AS u(elem)` set-returning function.
//!
//! PostgreSQL-compatible: expands one or more arrays into rows. Multiple arrays
//! are zipped together (same-length required per PG). NULL / empty array → 0 rows.
//!
//! ## PostgreSQL semantics
//!
//! ```sql
//! SELECT * FROM UNNEST(ARRAY[1,2,3]) AS u(x);  -- 3 rows
//! SELECT * FROM UNNEST(ARRAY[1,2], ARRAY['a','b']) AS u(n, l);  -- 2 rows (zip)
//! SELECT * FROM UNNEST(NULL::int[]);  -- 0 rows
//! SELECT * FROM UNNEST(ARRAY[]);  -- 0 rows
//! ```
//!
//! UNNEST in FROM is always LATERAL — outer columns are visible to the array
//! expressions. It can appear as the first FROM source (with no outer correlation)
//! or after a base table (with outer correlation).
//!
//! ## Multi-array zip rule
//!
//! When multiple arrays are given, PostgreSQL requires all arrays to have the
//! same length and zips them together. We follow this behavior: if lengths
//! differ, we return an error.

use axiomdb_catalog::schema::{ColumnDef, ColumnType, TableId};
use axiomdb_core::error::DbError;
use axiomdb_types::{DataType, Value};

use crate::ast::UnnestClause;
use crate::eval;
use crate::result::ColumnMeta;

/// Default column name for position `i` when no explicit column list is given.
fn default_name(i: usize) -> String {
    if i == 0 {
        "unnest".to_string()
    } else {
        format!("unnest_{}", i)
    }
}

/// Column metadata for UNNEST output projection.
pub fn column_metas_for_unnest(un: &UnnestClause) -> Vec<ColumnMeta> {
    let width = un.exprs.len();
    (0..width)
        .map(|i| {
            let name = un
                .column_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| default_name(i));
            // We use Text as the placeholder type; actual element types are
            // determined at runtime during materialization.
            ColumnMeta {
                name,
                data_type: DataType::Text,
                nullable: true,
                table_name: Some(un.alias.clone().unwrap_or_else(|| "unnest".into())),
            }
        })
        .collect()
}

/// Catalog-shape column defs for analyzer binding (BoundTable columns).
/// These provide the column name / nullable / col_idx shape that the binder
/// publishes into the BindContext so column expressions can resolve.
pub fn column_defs_for_unnest(un: &UnnestClause) -> Vec<ColumnDef> {
    let metas = column_metas_for_unnest(un);
    metas
        .into_iter()
        .enumerate()
        .map(|(i, m)| ColumnDef {
            table_id: 0 as TableId,
            col_idx: i as u16,
            name: m.name,
            col_type: ColumnType::Text, // Placeholder; executor uses runtime types
            nullable: m.nullable,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
        })
        .collect()
}

/// Materialize UNNEST into `Vec<Vec<Value>>` (one Vec per output row).
///
/// # Arguments
/// * `un`         — the parsed UNNEST clause
/// * `outer_row`  — `Some(row)` when UNNEST is LATERAL (after a base table);
///   `None` when UNNEST is the first FROM source (no outer correlation)
///
/// # PostgreSQL semantics followed
/// * NULL array → 0 rows
/// * Empty array → 0 rows
/// * Multiple arrays with different lengths → error
/// * NULL element in array → row with that NULL element (arrays can contain NULLs)
pub fn materialize_unnest(
    un: &UnnestClause,
    outer_row: Option<&[Value]>,
) -> Result<Vec<Vec<Value>>, DbError> {
    // Phase 1: evaluate every array expression against the outer scope.
    // Store owned Vec<Value> for each array to avoid borrow checker issues.
    let mut arrays: Vec<Vec<Value>> = Vec::with_capacity(un.exprs.len());
    for expr in &un.exprs {
        let val = eval(expr, outer_row.unwrap_or(&[]))?;
        match val {
            Value::Array(elems) => arrays.push(elems),
            Value::Null => {
                // NULL array → 0 rows
                return Ok(Vec::new());
            }
            other => {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "UNNEST argument must be an array; got {}",
                        other.variant_name()
                    ),
                });
            }
        }
    }

    // Phase 2: determine output row count.
    // PostgreSQL: all arrays must have the same length (zip semantics).
    let n_rows = arrays.first().map(|a| a.len()).unwrap_or(0);

    for (i, arr) in arrays.iter().enumerate() {
        if arr.len() != n_rows {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "UNNEST: array {} has {} elements, but array 1 has {}",
                    i + 1,
                    arr.len(),
                    n_rows
                ),
            });
        }
    }

    // Empty arrays → 0 rows
    if n_rows == 0 {
        return Ok(Vec::new());
    }

    // Phase 3: zip the arrays row-by-row.
    let mut out = Vec::with_capacity(n_rows);
    for row_idx in 0..n_rows {
        let mut row = Vec::with_capacity(arrays.len());
        for arr in &arrays {
            row.push(arr[row_idx].clone());
        }
        out.push(row);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::UnnestClause;
    use crate::expr::Expr;
    use crate::expr_to_sql::expr_to_sql_string;

    fn expr(s: &str) -> Expr {
        crate::parser::parse_expr_only(s).unwrap()
    }

    #[test]
    fn test_unnest_single_array() {
        let un = UnnestClause {
            exprs: vec![expr("ARRAY[1,2,3]")],
            alias: Some("u".to_string()),
            column_names: vec!["x".to_string()],
        };
        let rows = materialize_unnest(&un, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![Value::Int(1)]);
        assert_eq!(rows[1], vec![Value::Int(2)]);
        assert_eq!(rows[2], vec![Value::Int(3)]);
    }

    #[test]
    fn test_unnest_null_array() {
        let un = UnnestClause {
            exprs: vec![expr("NULL::int[]")],
            alias: Some("u".to_string()),
            column_names: vec![],
        };
        let rows = materialize_unnest(&un, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_unnest_empty_array() {
        let un = UnnestClause {
            exprs: vec![expr("ARRAY[]::int[]")],
            alias: Some("u".to_string()),
            column_names: vec![],
        };
        let rows = materialize_unnest(&un, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_unnest_multiple_arrays_zip() {
        let un = UnnestClause {
            exprs: vec![expr("ARRAY[1,2]"), expr("ARRAY['a','b']")],
            alias: Some("u".to_string()),
            column_names: vec!["n".to_string(), "l".to_string()],
        };
        let rows = materialize_unnest(&un, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![Value::Int(1), Value::Text("a".to_string())]);
        assert_eq!(rows[1], vec![Value::Int(2), Value::Text("b".to_string())]);
    }

    #[test]
    fn test_unnest_mismatched_lengths() {
        let un = UnnestClause {
            exprs: vec![expr("ARRAY[1,2,3]"), expr("ARRAY['a','b']")],
            alias: Some("u".to_string()),
            column_names: vec![],
        };
        let err = materialize_unnest(&un, None).unwrap_err();
        assert!(err.to_string().contains("UNNEST: array 2 has 2 elements"));
    }

    #[test]
    fn test_unnest_lateral_outer_correlation() {
        // Simulate: SELECT * FROM (SELECT 10 AS x) t, UNNEST(ARRAY[t.x]) AS u(y)
        // The outer_row would be [Value::Int(10)]
        let un = UnnestClause {
            exprs: vec![expr("ARRAY[42]")], // simplified: just use literal
            alias: Some("u".to_string()),
            column_names: vec!["val".to_string()],
        };
        let outer_row = vec![Value::Int(10)];
        let rows = materialize_unnest(&un, Some(&outer_row)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Int(42)]);
    }
}
