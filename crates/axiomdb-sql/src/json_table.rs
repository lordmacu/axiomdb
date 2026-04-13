//! Phase 11.20a — `JSON_TABLE(...)` flat row source.
//!
//! Compiles an [`ast::JsonTable`] into a reusable [`JsonTableSpec`] and
//! materializes it against a document value into a `Vec<Row>` aligned with
//! the column declaration order.
//!
//! Cross-engine reference:
//! - PostgreSQL: `src/backend/parser/parse_jsontable.c` (AST → TableFunc),
//!   `src/backend/executor/nodeTableFuncscan.c` (row emission loop).
//! - MariaDB: `sql/json_table.cc` (`Table_function_json_table`); we use
//!   MariaDB's recursive-walk model — simpler to map onto our `Vec<Row>`
//!   execution.
//!
//! Flat subset only — `NESTED PATH`, WRAPPER/QUOTES, LATERAL-like PASSING on
//! JSON_TABLE's row path are deferred to 11.20b–d.

use std::sync::Arc;

use axiomdb_catalog::schema::{ColumnDef, ColumnType, TableId};
use axiomdb_core::error::DbError;
use axiomdb_types::{
    coerce::{coerce, CoercionMode},
    jsonb::JsonbDecoder,
    DataType, Value,
};

use crate::ast::{self, JsonTable, JsonTableColumn};
use crate::eval::{eval_with, SubqueryRunner};
use crate::expr::SqlJsonOnBehavior;

// ── Lowered form ─────────────────────────────────────────────────────────────

/// Compiled JSON_TABLE: every JSONPath string is parsed once here, so each
/// materialization just walks.
#[derive(Debug, Clone)]
pub struct JsonTableSpec {
    pub alias: String,
    pub row_path: Vec<PathStepOwned>,
    pub columns: Vec<JsonTableColumnSpec>,
}

#[derive(Debug, Clone)]
pub struct JsonTableColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub kind: JsonTableColumnKind,
}

#[derive(Debug, Clone)]
pub enum JsonTableColumnKind {
    Regular {
        path: Vec<PathStepOwned>,
        on_empty: SqlJsonOnBehavior,
        on_error: SqlJsonOnBehavior,
    },
    Ordinality,
    Exists {
        path: Vec<PathStepOwned>,
        on_error: SqlJsonOnBehavior,
    },
}

/// Owned snapshot of a compiled JSONPath step (independent of the rich
/// `PathStep` enum in `eval::functions::json`, which contains `Expr` and
/// is not trivially `Clone` outside that module).
///
/// We restrict JSON_TABLE paths to a subset: `$`, `.key`, `[idx]`, `[*]`,
/// `.*`, `..key` recursive descent. No filter expressions.
#[derive(Debug, Clone)]
pub enum PathStepOwned {
    Root,
    Key(String),
    Index(usize),
    WildcardKey,
    WildcardIndex,
    Recursive,
}

// ── Compile ──────────────────────────────────────────────────────────────────

pub fn compile_json_table(jt: &JsonTable) -> Result<JsonTableSpec, DbError> {
    let row_path = parse_restricted_path(&jt.row_path)?;

    // Enforce: at most one FOR ORDINALITY per COLUMNS list.
    let ordinality_count = jt
        .columns
        .iter()
        .filter(|c| matches!(c, JsonTableColumn::Ordinality { .. }))
        .count();
    if ordinality_count > 1 {
        return Err(DbError::ParseError {
            message: "JSON_TABLE: at most one FOR ORDINALITY column allowed per COLUMNS(...) list"
                .into(),
            position: None,
        });
    }

    // Enforce: unique column names.
    let mut seen = std::collections::HashSet::new();
    for c in &jt.columns {
        let name = column_name(c);
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(DbError::ParseError {
                message: format!("JSON_TABLE: duplicate column name `{name}`"),
                position: None,
            });
        }
    }

    let mut columns = Vec::with_capacity(jt.columns.len());
    for c in &jt.columns {
        columns.push(compile_column(c)?);
    }

    let alias = jt.alias.clone().unwrap_or_else(|| "json_table".into());
    Ok(JsonTableSpec {
        alias,
        row_path,
        columns,
    })
}

fn compile_column(c: &JsonTableColumn) -> Result<JsonTableColumnSpec, DbError> {
    match c {
        JsonTableColumn::Regular {
            name,
            ty,
            path,
            on_empty,
            on_error,
        } => Ok(JsonTableColumnSpec {
            name: name.clone(),
            ty: *ty,
            kind: JsonTableColumnKind::Regular {
                path: parse_restricted_path(path)?,
                on_empty: on_empty.clone(),
                on_error: on_error.clone(),
            },
        }),
        JsonTableColumn::Ordinality { name } => Ok(JsonTableColumnSpec {
            name: name.clone(),
            ty: DataType::BigInt,
            kind: JsonTableColumnKind::Ordinality,
        }),
        JsonTableColumn::Exists {
            name,
            ty,
            path,
            on_error,
        } => Ok(JsonTableColumnSpec {
            name: name.clone(),
            ty: *ty,
            kind: JsonTableColumnKind::Exists {
                path: parse_restricted_path(path)?,
                on_error: on_error.clone(),
            },
        }),
    }
}

fn column_name(c: &JsonTableColumn) -> &str {
    match c {
        JsonTableColumn::Regular { name, .. }
        | JsonTableColumn::Ordinality { name }
        | JsonTableColumn::Exists { name, .. } => name.as_str(),
    }
}

// ── ColumnDef projection (for analyzer_bind) ────────────────────────────────

pub fn column_defs_for_ast(jt: &JsonTable) -> Result<Vec<ColumnDef>, DbError> {
    let mut out = Vec::with_capacity(jt.columns.len());
    for (idx, c) in jt.columns.iter().enumerate() {
        let (name, dt, nullable) = match c {
            JsonTableColumn::Regular {
                name,
                ty,
                on_empty,
                on_error,
                ..
            } => {
                let nullable = matches!(on_empty, SqlJsonOnBehavior::Null)
                    || matches!(on_error, SqlJsonOnBehavior::Null);
                (name.as_str(), *ty, nullable)
            }
            JsonTableColumn::Ordinality { name } => (name.as_str(), DataType::BigInt, false),
            JsonTableColumn::Exists { name, ty, .. } => (name.as_str(), *ty, false),
        };
        out.push(ColumnDef {
            table_id: 0 as TableId,
            col_idx: idx as u16,
            name: name.to_string(),
            col_type: datatype_to_column_type(&dt)?,
            nullable,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
        });
    }
    Ok(out)
}

fn datatype_to_column_type(dt: &DataType) -> Result<ColumnType, DbError> {
    Ok(match dt {
        DataType::Bool => ColumnType::Bool,
        DataType::Int => ColumnType::Int,
        DataType::BigInt => ColumnType::BigInt,
        DataType::Real => ColumnType::Float,
        DataType::Text => ColumnType::Text,
        DataType::Json => ColumnType::Json,
        DataType::Jsonb => ColumnType::Jsonb,
        DataType::Bytes => ColumnType::Bytes,
        DataType::Timestamp => ColumnType::Timestamp,
        DataType::Uuid => ColumnType::Uuid,
        DataType::Decimal => ColumnType::Decimal,
        DataType::Date => ColumnType::Date,
    })
}

// ── Materialize ─────────────────────────────────────────────────────────────

/// Convert a SQL `Value` to a `serde_json::Value` suitable for path walking.
///
/// - `Value::Null`     → signals "no rows" via returning `Ok(None)` (caller
///   treats as zero rows; no error).
/// - `Value::Jsonb(_)` → decoded via `JsonbDecoder`.
/// - `Value::Json(_)` / `Value::Text(_)` → parsed via `serde_json::from_str`
///   (invalid JSON surfaces as `DbError::InvalidCoercion`).
/// - Other variants → `DbError::TypeMismatch`.
pub fn doc_to_serde(doc: &Value) -> Result<Option<serde_json::Value>, DbError> {
    match doc {
        Value::Null => Ok(None),
        Value::Jsonb(b) => Ok(Some(JsonbDecoder::decode(b.as_ref())?)),
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(s)
                .map(Some)
                .map_err(|e| DbError::InvalidCoercion {
                    from: "TEXT".into(),
                    to: "JSON".into(),
                    value: truncate_for_error(s),
                    reason: format!("invalid JSON document: {e}"),
                })
        }
        other => Err(DbError::TypeMismatch {
            expected: "JSON or JSONB document".into(),
            got: format!("{other:?}"),
        }),
    }
}

pub fn materialize_json_table<R: SubqueryRunner>(
    spec: &JsonTableSpec,
    doc: &serde_json::Value,
    outer_row: &[Value],
    sq: &mut R,
) -> Result<Vec<Vec<Value>>, DbError> {
    let parent_matches = walk_path_owned(doc, &spec.row_path);
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(parent_matches.len());

    for (i, parent) in parent_matches.iter().enumerate() {
        let ord = (i as i64) + 1;
        let mut row: Vec<Value> = Vec::with_capacity(spec.columns.len());

        for col in &spec.columns {
            let v = match &col.kind {
                JsonTableColumnKind::Ordinality => Value::BigInt(ord),
                JsonTableColumnKind::Regular {
                    path,
                    on_empty,
                    on_error,
                } => materialize_regular(parent, path, col.ty, on_empty, on_error, outer_row, sq)?,
                JsonTableColumnKind::Exists { path, on_error } => {
                    materialize_exists(parent, path, col.ty, on_error, outer_row, sq)?
                }
            };
            row.push(v);
        }

        rows.push(row);
    }
    Ok(rows)
}

fn materialize_regular<R: SubqueryRunner>(
    parent: &serde_json::Value,
    path: &[PathStepOwned],
    ty: DataType,
    on_empty: &SqlJsonOnBehavior,
    on_error: &SqlJsonOnBehavior,
    outer_row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let hits = walk_path_owned(parent, path);
    match hits.len() {
        0 => apply_on_behavior(on_empty, ty, outer_row, sq),
        1 => {
            let sj = &hits[0];
            match serde_to_value_typed(sj, ty) {
                Ok(v) => Ok(v),
                Err(_) => apply_on_behavior(on_error, ty, outer_row, sq),
            }
        }
        _ => apply_on_behavior(on_error, ty, outer_row, sq),
    }
}

fn materialize_exists<R: SubqueryRunner>(
    parent: &serde_json::Value,
    path: &[PathStepOwned],
    ty: DataType,
    on_error: &SqlJsonOnBehavior,
    outer_row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    // walk_path_owned never errors in this subset; we keep the on_error plumbing
    // to stay grammar-compatible with PG and to accommodate future richer paths.
    let _ = on_error; // currently unused — kept for future error-producing paths
    let hits = walk_path_owned(parent, path);
    let b = !hits.is_empty();
    let base = Value::Bool(b);
    if ty == DataType::Bool {
        Ok(base)
    } else {
        // EXISTS column declared as INT → coerce TRUE/FALSE to 1/0.
        coerce(base, ty, CoercionMode::Strict)
            .or_else(|_| apply_on_behavior(on_error, ty, outer_row, sq))
    }
}

fn apply_on_behavior<R: SubqueryRunner>(
    behavior: &SqlJsonOnBehavior,
    ty: DataType,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    match behavior {
        SqlJsonOnBehavior::Null => Ok(Value::Null),
        SqlJsonOnBehavior::Error => Err(DbError::InvalidValue {
            reason: "JSON_TABLE column: ON EMPTY / ON ERROR requested ERROR".into(),
        }),
        SqlJsonOnBehavior::Default(expr) => {
            let v = eval_with(expr, row, sq)?;
            coerce(v, ty, CoercionMode::Strict)
        }
        SqlJsonOnBehavior::TrueLit => coerce(Value::Bool(true), ty, CoercionMode::Strict),
        SqlJsonOnBehavior::FalseLit => coerce(Value::Bool(false), ty, CoercionMode::Strict),
        SqlJsonOnBehavior::Unknown => Ok(Value::Null),
    }
}

fn serde_to_value_typed(sj: &serde_json::Value, ty: DataType) -> Result<Value, DbError> {
    let base = match sj {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(i)
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                Value::Text(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            // For JSONB / JSON / TEXT columns, route the subtree as-is;
            // for every other declared type, fall through to on_error.
            match ty {
                DataType::Jsonb => {
                    let blob = axiomdb_types::JsonbEncoder::encode(sj)?;
                    return Ok(Value::Jsonb(Arc::new(blob)));
                }
                DataType::Json => return Ok(Value::Json(sj.to_string())),
                DataType::Text => return Ok(Value::Text(sj.to_string())),
                _ => {
                    return Err(DbError::InvalidCoercion {
                        from: "JSON composite".into(),
                        to: format!("{ty:?}"),
                        value: truncate_for_error(&sj.to_string()),
                        reason: "JSON_TABLE regular column expected a scalar match".into(),
                    })
                }
            }
        }
    };
    coerce(base, ty, CoercionMode::Strict)
}

// ── Path parsing & walking (flat subset) ────────────────────────────────────

/// Parse `$`, `$.key`, `$.key[0]`, `$[*]`, `$.*`, `$..key` into a step list.
/// Deliberately strict — no filter expressions or `.size()`/`.type()` etc.
fn parse_restricted_path(s: &str) -> Result<Vec<PathStepOwned>, DbError> {
    let trimmed = s.trim();
    if !trimmed.starts_with('$') {
        return Err(DbError::ParseError {
            message: format!("JSON_TABLE path must start with `$`: {trimmed:?}"),
            position: None,
        });
    }
    let mut steps = vec![PathStepOwned::Root];
    let bytes = trimmed.as_bytes();
    let mut i = 1usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'.' {
            i += 1;
            // `..key` recursive descent
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                let (key, j) = parse_ident(bytes, i)?;
                steps.push(PathStepOwned::Recursive);
                steps.push(PathStepOwned::Key(key));
                i = j;
                continue;
            }
            // `.*` wildcard key
            if i < bytes.len() && bytes[i] == b'*' {
                steps.push(PathStepOwned::WildcardKey);
                i += 1;
                continue;
            }
            let (key, j) = parse_ident(bytes, i)?;
            steps.push(PathStepOwned::Key(key));
            i = j;
        } else if c == b'[' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'*' {
                if i + 1 >= bytes.len() || bytes[i + 1] != b']' {
                    return Err(DbError::ParseError {
                        message: format!("JSON_TABLE path: unterminated [*] in {trimmed:?}"),
                        position: None,
                    });
                }
                steps.push(PathStepOwned::WildcardIndex);
                i += 2;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i] != b']' {
                i += 1;
            }
            if i >= bytes.len() {
                return Err(DbError::ParseError {
                    message: format!("JSON_TABLE path: unterminated `[` in {trimmed:?}"),
                    position: None,
                });
            }
            let idx_str =
                std::str::from_utf8(&bytes[start..i]).map_err(|_| DbError::ParseError {
                    message: format!("JSON_TABLE path: non-utf8 index in {trimmed:?}"),
                    position: None,
                })?;
            let idx: usize = idx_str.trim().parse().map_err(|_| DbError::ParseError {
                message: format!("JSON_TABLE path: invalid array index `{idx_str}`"),
                position: None,
            })?;
            steps.push(PathStepOwned::Index(idx));
            i += 1; // consume ']'
        } else {
            return Err(DbError::ParseError {
                message: format!(
                    "JSON_TABLE path: unexpected character `{}` at position {i} of {trimmed:?}",
                    c as char
                ),
                position: None,
            });
        }
    }
    Ok(steps)
}

fn parse_ident(bytes: &[u8], start: usize) -> Result<(String, usize), DbError> {
    let mut j = start;
    // Support `"quoted key"`.
    if j < bytes.len() && bytes[j] == b'"' {
        j += 1;
        let from = j;
        while j < bytes.len() && bytes[j] != b'"' {
            j += 1;
        }
        if j >= bytes.len() {
            return Err(DbError::ParseError {
                message: "JSON_TABLE path: unterminated quoted key".into(),
                position: None,
            });
        }
        let s = String::from_utf8_lossy(&bytes[from..j]).into_owned();
        Ok((s, j + 1))
    } else {
        let from = j;
        while j < bytes.len() {
            let c = bytes[j];
            if c == b'.' || c == b'[' || c == b' ' {
                break;
            }
            j += 1;
        }
        if from == j {
            return Err(DbError::ParseError {
                message: "JSON_TABLE path: expected key after `.`".into(),
                position: None,
            });
        }
        let s = String::from_utf8_lossy(&bytes[from..j]).into_owned();
        Ok((s, j))
    }
}

fn walk_path_owned(root: &serde_json::Value, steps: &[PathStepOwned]) -> Vec<serde_json::Value> {
    let mut current: Vec<serde_json::Value> = vec![root.clone()];
    for step in steps {
        let next = match step {
            PathStepOwned::Root => continue,
            PathStepOwned::Key(k) => step_key(&current, k),
            PathStepOwned::Index(i) => step_index(&current, *i),
            PathStepOwned::WildcardIndex => step_wildcard_idx(&current),
            PathStepOwned::WildcardKey => step_wildcard_key(&current),
            PathStepOwned::Recursive => step_recursive(&current),
        };
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

fn step_key(nodes: &[serde_json::Value], key: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for n in nodes {
        if let serde_json::Value::Object(m) = n {
            if let Some(v) = m.get(key) {
                out.push(v.clone());
            }
        }
    }
    out
}

fn step_index(nodes: &[serde_json::Value], idx: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for n in nodes {
        if let serde_json::Value::Array(a) = n {
            if idx < a.len() {
                out.push(a[idx].clone());
            }
        }
    }
    out
}

fn step_wildcard_idx(nodes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for n in nodes {
        if let serde_json::Value::Array(a) = n {
            out.extend(a.iter().cloned());
        }
    }
    out
}

fn step_wildcard_key(nodes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for n in nodes {
        if let serde_json::Value::Object(m) = n {
            out.extend(m.values().cloned());
        }
    }
    out
}

fn step_recursive(nodes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    // `..` prefixes the *next* key step; for `..key` we emit every descendant
    // container, and the following Key step filters. For standalone `..`
    // (rare) we also yield every descendant.
    let mut out = Vec::new();
    for n in nodes {
        collect_descendants(n, &mut out);
    }
    out
}

fn collect_descendants(n: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    out.push(n.clone());
    match n {
        serde_json::Value::Array(a) => {
            for v in a {
                collect_descendants(v, out);
            }
        }
        serde_json::Value::Object(m) => {
            for v in m.values() {
                collect_descendants(v, out);
            }
        }
        _ => {}
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build `ColumnMeta` list from a compiled `JsonTableSpec` — convenience for
/// executor sites that need `result::ColumnMeta` rather than catalog
/// `ColumnDef`.
pub fn column_metas_for_spec(spec: &JsonTableSpec) -> Vec<crate::result::ColumnMeta> {
    spec.columns
        .iter()
        .map(|c| crate::result::ColumnMeta {
            name: c.name.clone(),
            data_type: c.ty,
            nullable: matches!(
                c.kind,
                JsonTableColumnKind::Regular {
                    on_empty: SqlJsonOnBehavior::Null,
                    ..
                }
            ),
            table_name: Some(spec.alias.clone()),
        })
        .collect()
}

/// Returns `true` if the expression tree contains any column reference —
/// used to detect LATERAL-style correlated `doc` expressions that 11.20a
/// does not yet support.
pub fn doc_has_column_refs(expr: &crate::expr::Expr) -> bool {
    use crate::expr::Expr;
    match expr {
        Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::InsertValue { .. } => true,
        Expr::Literal(_) | Expr::Default | Expr::Param { .. } => false,
        Expr::UnaryOp { operand, .. } => doc_has_column_refs(operand),
        Expr::BinaryOp { left, right, .. } => {
            doc_has_column_refs(left) || doc_has_column_refs(right)
        }
        Expr::Function { args, .. } => args.iter().any(doc_has_column_refs),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand.as_ref().is_some_and(|o| doc_has_column_refs(o))
                || when_thens
                    .iter()
                    .any(|(a, b)| doc_has_column_refs(a) || doc_has_column_refs(b))
                || else_result.as_ref().is_some_and(|e| doc_has_column_refs(e))
        }
        Expr::Between {
            expr, low, high, ..
        } => doc_has_column_refs(expr) || doc_has_column_refs(low) || doc_has_column_refs(high),
        Expr::In { expr, list, .. } => {
            doc_has_column_refs(expr) || list.iter().any(doc_has_column_refs)
        }
        Expr::IsBoolean { expr, .. } => doc_has_column_refs(expr),
        Expr::Cast { expr, .. } => doc_has_column_refs(expr),
        Expr::IsNull { expr, .. } => doc_has_column_refs(expr),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            doc_has_column_refs(expr)
                || doc_has_column_refs(pattern)
                || escape.as_ref().is_some_and(|e| doc_has_column_refs(e))
        }
        Expr::SqlJsonQuery {
            doc,
            passing,
            on_empty,
            on_error,
            ..
        } => {
            doc_has_column_refs(doc)
                || passing.iter().any(|(e, _)| doc_has_column_refs(e))
                || on_behavior_has_col_refs(on_empty)
                || on_behavior_has_col_refs(on_error)
        }
        Expr::GroupConcat { expr, order_by, .. } => {
            doc_has_column_refs(expr) || order_by.iter().any(|(e, _)| doc_has_column_refs(e))
        }
        // Subqueries are not correlation we can detect at this layer — treat
        // as "yes" to force the NotImplemented branch (users can wrap the
        // doc in a CTE / derived table if they need constant materialization).
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
    }
}

fn on_behavior_has_col_refs(b: &SqlJsonOnBehavior) -> bool {
    match b {
        SqlJsonOnBehavior::Default(e) => doc_has_column_refs(e),
        _ => false,
    }
}

fn truncate_for_error(s: &str) -> String {
    if s.len() <= 64 {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(64).collect();
        out.push('…');
        out
    }
}

// ── AST reference re-export for callers ─────────────────────────────────────

pub use ast::JsonTable as JsonTableAst;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_rejects_duplicate_ordinality() {
        let jt = JsonTable {
            doc: crate::expr::Expr::Literal(Value::Null),
            row_path: "$".into(),
            columns: vec![
                JsonTableColumn::Ordinality { name: "a".into() },
                JsonTableColumn::Ordinality { name: "b".into() },
            ],
            alias: None,
        };
        let err = compile_json_table(&jt).unwrap_err();
        assert!(format!("{err:?}").contains("FOR ORDINALITY"));
    }

    #[test]
    fn compile_rejects_duplicate_column_name() {
        let jt = JsonTable {
            doc: crate::expr::Expr::Literal(Value::Null),
            row_path: "$".into(),
            columns: vec![
                JsonTableColumn::Ordinality { name: "ord".into() },
                JsonTableColumn::Regular {
                    name: "ORD".into(), // case-insensitive collision
                    ty: DataType::Int,
                    path: "$.a".into(),
                    on_empty: SqlJsonOnBehavior::Null,
                    on_error: SqlJsonOnBehavior::Null,
                },
            ],
            alias: None,
        };
        let err = compile_json_table(&jt).unwrap_err();
        assert!(format!("{err:?}").contains("duplicate"));
    }

    #[test]
    fn parse_restricted_path_basic_shapes() {
        assert_eq!(parse_restricted_path("$").unwrap().len(), 1);
        assert_eq!(parse_restricted_path("$.a.b").unwrap().len(), 3);
        assert_eq!(parse_restricted_path("$[0]").unwrap().len(), 2);
        assert_eq!(parse_restricted_path("$[*]").unwrap().len(), 2);
        assert!(parse_restricted_path("foo").is_err());
        assert!(parse_restricted_path("$[").is_err());
    }

    #[test]
    fn walk_simple_object_key() {
        let doc = serde_json::json!({"a": {"b": 42}});
        let steps = parse_restricted_path("$.a.b").unwrap();
        let hits = walk_path_owned(&doc, &steps);
        assert_eq!(hits, vec![serde_json::json!(42)]);
    }

    #[test]
    fn walk_array_wildcard() {
        let doc = serde_json::json!([1, 2, 3]);
        let steps = parse_restricted_path("$[*]").unwrap();
        let hits = walk_path_owned(&doc, &steps);
        assert_eq!(
            hits,
            vec![
                serde_json::json!(1),
                serde_json::json!(2),
                serde_json::json!(3)
            ]
        );
    }

    #[test]
    fn walk_recursive_descent() {
        let doc = serde_json::json!({"a": [{"v": 1}, {"v": 2}], "b": {"c": {"v": 3}}});
        let steps = parse_restricted_path("$..v").unwrap();
        let mut hits: Vec<_> = walk_path_owned(&doc, &steps)
            .into_iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        hits.sort();
        assert_eq!(hits, vec![1, 2, 3]);
    }
}
