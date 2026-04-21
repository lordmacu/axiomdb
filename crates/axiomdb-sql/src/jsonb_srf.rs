//! Phase 11.25a — PostgreSQL-compatible JSONB set-returning functions.
//!
//! `jsonb_each` / `jsonb_each_text` / `jsonb_object_keys` /
//! `jsonb_array_elements` / `jsonb_array_elements_text` produce rows
//! usable in FROM position, as join right sides (with or without
//! LATERAL correlation), and as UPDATE / DELETE source.

use std::sync::Arc;

use axiomdb_catalog::{ColumnDef, ColumnType, TableId};
use axiomdb_core::error::DbError;
use axiomdb_types::{jsonb::JsonbEncoder, DataType, JsonbRef, Value};

use crate::ast::JsonbSrfKind;
use crate::result::ColumnMeta;

/// Virtual column metadata for the SRF's output shape.
pub fn column_metas_for_srf(kind: JsonbSrfKind, alias: &str) -> Vec<ColumnMeta> {
    let alias_owned = Some(alias.to_string());
    match kind {
        JsonbSrfKind::Each => vec![
            ColumnMeta {
                name: "key".into(),
                data_type: DataType::Text,
                nullable: false,
                table_name: alias_owned.clone(),
            },
            ColumnMeta {
                name: "value".into(),
                data_type: DataType::Jsonb,
                nullable: true,
                table_name: alias_owned,
            },
        ],
        JsonbSrfKind::EachText => vec![
            ColumnMeta {
                name: "key".into(),
                data_type: DataType::Text,
                nullable: false,
                table_name: alias_owned.clone(),
            },
            ColumnMeta {
                name: "value".into(),
                data_type: DataType::Text,
                nullable: true,
                table_name: alias_owned,
            },
        ],
        JsonbSrfKind::ObjectKeys => vec![ColumnMeta {
            name: "key".into(),
            data_type: DataType::Text,
            nullable: false,
            table_name: alias_owned,
        }],
        JsonbSrfKind::ArrayElements => vec![ColumnMeta {
            name: "value".into(),
            data_type: DataType::Jsonb,
            nullable: true,
            table_name: alias_owned,
        }],
        JsonbSrfKind::ArrayElementsText => vec![ColumnMeta {
            name: "value".into(),
            data_type: DataType::Text,
            nullable: true,
            table_name: alias_owned,
        }],
    }
}

/// Catalog-shape column defs for analyzer binding (BoundTable columns).
pub fn column_defs_for_srf(kind: JsonbSrfKind) -> Vec<ColumnDef> {
    let metas = column_metas_for_srf(kind, "");
    metas
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let col_type = match m.data_type {
                DataType::Jsonb => ColumnType::Jsonb,
                _ => ColumnType::Text,
            };
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
                generated_stored: false,
            }
        })
        .collect()
}

pub fn srf_is_correlated(srf: &crate::ast::JsonbSrf) -> bool {
    crate::json_table::doc_has_column_refs(&srf.doc)
}

pub fn srf_alias(srf: &crate::ast::JsonbSrf) -> String {
    srf.alias
        .clone()
        .unwrap_or_else(|| srf.kind.fn_name().to_string())
}

/// Evaluate the SRF for a given doc value and produce rows.
pub fn materialize_jsonb_srf(
    kind: JsonbSrfKind,
    doc_val: &Value,
) -> Result<Vec<Vec<Value>>, DbError> {
    let sj = match value_to_serde(doc_val)? {
        None => return Ok(Vec::new()),
        Some(v) => v,
    };
    match kind {
        JsonbSrfKind::Each => {
            let obj = sj.as_object().ok_or_else(|| type_err(kind, "object"))?;
            let mut rows = Vec::with_capacity(obj.len());
            for (k, v) in obj {
                rows.push(vec![Value::Text(k.clone()), jsonb_from_serde(v)?]);
            }
            Ok(rows)
        }
        JsonbSrfKind::EachText => {
            let obj = sj.as_object().ok_or_else(|| type_err(kind, "object"))?;
            let mut rows = Vec::with_capacity(obj.len());
            for (k, v) in obj {
                rows.push(vec![Value::Text(k.clone()), text_projection(v)]);
            }
            Ok(rows)
        }
        JsonbSrfKind::ObjectKeys => {
            let obj = sj.as_object().ok_or_else(|| type_err(kind, "object"))?;
            Ok(obj.keys().map(|k| vec![Value::Text(k.clone())]).collect())
        }
        JsonbSrfKind::ArrayElements => {
            let arr = sj.as_array().ok_or_else(|| type_err(kind, "array"))?;
            let mut rows = Vec::with_capacity(arr.len());
            for v in arr {
                rows.push(vec![jsonb_from_serde(v)?]);
            }
            Ok(rows)
        }
        JsonbSrfKind::ArrayElementsText => {
            let arr = sj.as_array().ok_or_else(|| type_err(kind, "array"))?;
            Ok(arr.iter().map(|v| vec![text_projection(v)]).collect())
        }
    }
}

fn value_to_serde(v: &Value) -> Result<Option<serde_json::Value>, DbError> {
    match v {
        Value::Null => Ok(None),
        Value::Jsonb(bytes) => {
            let jref = JsonbRef::new(bytes.as_slice());
            Ok(Some(jref.to_serde_json()?))
        }
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(s)
                .map(Some)
                .map_err(|e| DbError::InvalidCoercion {
                    from: "text".into(),
                    to: "json".into(),
                    value: s.clone(),
                    reason: e.to_string(),
                })
        }
        other => Err(DbError::TypeMismatch {
            expected: "JSON or JSONB document".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn text_projection(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn jsonb_from_serde(v: &serde_json::Value) -> Result<Value, DbError> {
    let blob = JsonbEncoder::encode(v)?;
    Ok(Value::Jsonb(Arc::new(blob)))
}

/// Phase 11.25b — render a SQL `Value` as `serde_json::Value` for JSON
/// aggregate accumulation. Native JSON/JSONB stays as-is; primitives map
/// to their JSON equivalents; strings become JSON strings; temporal / UUID
/// / bytes fall back to string representation.
pub fn value_to_serde_for_agg(v: &Value) -> Result<serde_json::Value, DbError> {
    match v {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int(i) => Ok(serde_json::Value::Number((*i as i64).into())),
        Value::BigInt(i) => Ok(serde_json::Value::Number((*i).into())),
        Value::Real(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!("non-finite float in JSON aggregate: {f}"),
            }),
        Value::Text(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Json(s) => serde_json::from_str(s).map_err(|e| DbError::InvalidCoercion {
            from: "text".into(),
            to: "json".into(),
            value: s.clone(),
            reason: e.to_string(),
        }),
        Value::Jsonb(bytes) => {
            let jref = JsonbRef::new(bytes.as_slice());
            jref.to_serde_json()
        }
        other => Ok(serde_json::Value::String(format!("{other:?}"))),
    }
}

fn type_err(kind: JsonbSrfKind, expected: &str) -> DbError {
    DbError::TypeMismatch {
        expected: format!("JSON {} for {}", expected, kind.fn_name()),
        got: format!("non-{expected}"),
    }
}
