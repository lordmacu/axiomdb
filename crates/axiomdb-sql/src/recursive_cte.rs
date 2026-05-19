//! Phase 21.3 — helpers for recursive CTE (WITH RECURSIVE).
//!
//! Recursive CTE execution follows PG's two-tuplestore algorithm
//! (research/postgres/src/backend/executor/nodeRecursiveunion.c:63-173):
//! run the base term, then iterate the recursive term with the current
//! working table bound to the CTE name until the step produces no new
//! rows (with dedup for plain UNION, or keep-all for UNION ALL).

use axiomdb_catalog::{ColumnDef, ColumnType, TableId};
use axiomdb_types::{DataType, Value};

use crate::ast::{RecursiveCteClause, SelectItem};
use crate::result::ColumnMeta;

/// Maximum recursive-term iterations before bailing out.
/// PG default is unlimited; MySQL defaults to 1000. AxiomDB follows
/// MySQL for safety.
pub const MAX_RECURSION: usize = 1000;

/// Column metadata inferred from the base SELECT's select list +
/// optional column-name override.
pub fn column_metas_for_recursive(rc: &RecursiveCteClause) -> Vec<ColumnMeta> {
    let alias = rc.alias.clone();
    // Use the base SELECT's columns to infer names.
    let base_items = &rc.base.columns;
    base_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = rc
                .column_names
                .as_ref()
                .and_then(|names| names.get(i).cloned())
                .unwrap_or_else(|| item_column_name(item, i));
            ColumnMeta {
                name,
                data_type: DataType::Text,
                nullable: true,
                table_name: Some(alias.clone()),
            }
        })
        .collect()
}

/// Catalog-shape column defs for analyzer binding (BoundTable).
pub fn column_defs_for_recursive(rc: &RecursiveCteClause) -> Vec<ColumnDef> {
    column_metas_for_recursive(rc)
        .into_iter()
        .enumerate()
        .map(|(i, m)| ColumnDef {
            table_id: 0 as TableId,
            col_idx: i as u16,
            name: m.name,
            col_type: ColumnType::Text,
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
            identity_kind: axiomdb_catalog::IdentityKind::None,
        })
        .collect()
}

fn item_column_name(item: &SelectItem, idx: usize) -> String {
    match item {
        SelectItem::Expr { alias, expr } => alias.clone().unwrap_or_else(|| match expr {
            crate::expr::Expr::Column { name, .. } => {
                name.rsplit('.').next().unwrap_or(name).to_string()
            }
            _ => format!("column{}", idx + 1),
        }),
        SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
            format!("column{}", idx + 1)
        }
    }
}

/// Serialize a row into a canonical byte key for UNION dedup.
pub fn row_key(row: &[Value]) -> Vec<u8> {
    // Simple encoding: Debug-format each value, join with 0xFF sentinel.
    let mut out = Vec::new();
    for v in row {
        out.extend_from_slice(format!("{v:?}").as_bytes());
        out.push(0xFF);
    }
    out
}
