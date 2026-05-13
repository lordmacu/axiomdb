//! Phase 21.22 — inline `VALUES` row source.
//!
//! `(VALUES (expr, expr, ...), ...) AS alias(col1, col2, ...)` — SQL-standard
//! inline table constructor. Each row is evaluated against an empty scope
//! (no outer correlation in this subphase; consistent with 11.25a SRF).

use axiomdb_catalog::{ColumnDef, ColumnType, TableId};
use axiomdb_core::error::DbError;
use axiomdb_types::{DataType, Value};

use crate::ast::ValuesClause;
use crate::result::ColumnMeta;

/// Default column name for position `i` (0-indexed) when the inline VALUES
/// clause was not given an explicit `AS t(...)` column list.
fn default_name(i: usize) -> String {
    format!("column{}", i + 1)
}

/// Virtual column metadata for projection / bind context.
pub fn column_metas_for_values(vc: &ValuesClause) -> Vec<ColumnMeta> {
    let width = vc.rows.first().map(|r| r.len()).unwrap_or(0);
    (0..width)
        .map(|i| {
            let name = vc
                .column_names
                .as_ref()
                .and_then(|names| names.get(i).cloned())
                .unwrap_or_else(|| default_name(i));
            let data_type = vc
                .rows
                .first()
                .and_then(|r| r.get(i))
                .map(infer_expr_type)
                .unwrap_or(DataType::Text);
            ColumnMeta {
                name,
                data_type,
                nullable: true,
                table_name: Some(vc.alias.clone()),
            }
        })
        .collect()
}

/// Catalog-shape column defs for analyzer binding (BoundTable columns).
pub fn column_defs_for_values(vc: &ValuesClause) -> Vec<ColumnDef> {
    let metas = column_metas_for_values(vc);
    metas
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let col_type = datatype_to_column_type(m.data_type);
            ColumnDef {
                table_id: 0 as TableId,
                col_idx: i as u16,
                name: m.name,
                col_type,
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
            }
        })
        .collect()
}

fn datatype_to_column_type(dt: DataType) -> ColumnType {
    match dt {
        DataType::Int => ColumnType::Int,
        DataType::BigInt => ColumnType::BigInt,
        DataType::Bool => ColumnType::Bool,
        DataType::Text => ColumnType::Text,
        DataType::Json => ColumnType::Json,
        DataType::Jsonb => ColumnType::Jsonb,
        DataType::Array(_) => ColumnType::Array,
        _ => ColumnType::Text,
    }
}

/// Very small expression-type inference for the first row — enough to
/// drive column metadata for the common case of literal rows.
fn infer_expr_type(expr: &crate::expr::Expr) -> DataType {
    match expr {
        crate::expr::Expr::Literal(v) => value_type(v),
        _ => DataType::Text,
    }
}

fn value_type(v: &Value) -> DataType {
    match v {
        Value::Null => DataType::Text,
        Value::Bool(_) => DataType::Bool,
        Value::Int(_) => DataType::Int,
        Value::BigInt(_) => DataType::BigInt,
        Value::Real(_) => DataType::Real,
        Value::Text(_) => DataType::Text,
        Value::Json(_) => DataType::Json,
        Value::Jsonb(_) => DataType::Jsonb,
        Value::Array(_) => DataType::Array(Box::new(DataType::Text)),
        _ => DataType::Text,
    }
}

/// Evaluate every expression against an empty scope and collect into
/// `Vec<Vec<Value>>`. Row count mismatches are already rejected at parse
/// time.
pub fn materialize_values(vc: &ValuesClause) -> Result<Vec<Vec<Value>>, DbError> {
    let mut out = Vec::with_capacity(vc.rows.len());
    for row in &vc.rows {
        let mut values = Vec::with_capacity(row.len());
        for e in row {
            values.push(crate::eval::eval(e, &[])?);
        }
        out.push(values);
    }
    Ok(out)
}
