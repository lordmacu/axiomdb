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
    /// Total number of slots in each emitted row (sum of leaf columns across
    /// every level of NESTED PATH). Leaves count 1 each; `Nested` columns
    /// expand into their children's slot range.
    pub total_slots: usize,
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
        /// Slot index in the emitted row.
        slot: usize,
        path: Vec<PathStepOwned>,
        on_empty: SqlJsonOnBehavior,
        on_error: SqlJsonOnBehavior,
    },
    Ordinality {
        slot: usize,
    },
    Exists {
        slot: usize,
        path: Vec<PathStepOwned>,
        on_error: SqlJsonOnBehavior,
    },
    /// Phase 11.20b — a single-level `NESTED PATH ... COLUMNS(...)` subtree.
    /// 11.20b rejects multi-sibling + multi-level NESTED at compile time;
    /// `children` therefore contains only `Regular`, `Ordinality`, and
    /// `Exists` variants. Slot range `[start, end)` is a contiguous span
    /// inside the emitted row's flat slot vector.
    Nested {
        path: Vec<PathStepOwned>,
        children: Vec<JsonTableColumnSpec>,
        slot_range: (usize, usize),
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

    // Enforce: unique column names across every level.
    let mut seen = std::collections::HashSet::new();
    collect_names_recursive(&jt.columns, &mut seen)?;

    // Depth-first slot assignment; enforces depth ≤ 1 and single NESTED
    // sibling per list (11.20b constraints; 11.20c lifts both).
    let mut next_slot = 0usize;
    let columns = compile_columns_recursive(&jt.columns, &mut next_slot, 0)?;

    let alias = jt.alias.clone().unwrap_or_else(|| "json_table".into());
    Ok(JsonTableSpec {
        alias,
        row_path,
        columns,
        total_slots: next_slot,
    })
}

fn collect_names_recursive(
    cols: &[JsonTableColumn],
    seen: &mut std::collections::HashSet<String>,
) -> Result<(), DbError> {
    for c in cols {
        match c {
            JsonTableColumn::Regular { name, .. }
            | JsonTableColumn::Ordinality { name }
            | JsonTableColumn::Exists { name, .. } => {
                if !seen.insert(name.to_ascii_lowercase()) {
                    return Err(DbError::ParseError {
                        message: format!("JSON_TABLE: duplicate column name `{name}`"),
                        position: None,
                    });
                }
            }
            JsonTableColumn::Nested { columns, .. } => {
                collect_names_recursive(columns, seen)?;
            }
        }
    }
    Ok(())
}

fn compile_columns_recursive(
    cols: &[JsonTableColumn],
    next_slot: &mut usize,
    depth: usize,
) -> Result<Vec<JsonTableColumnSpec>, DbError> {
    // At-most-one FOR ORDINALITY per list (each level has its own counter).
    let ordinality_count = cols
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

    // Phase 11.20b restriction: at most one NESTED sibling per list.
    let nested_count = cols
        .iter()
        .filter(|c| matches!(c, JsonTableColumn::Nested { .. }))
        .count();
    if nested_count > 1 {
        return Err(DbError::NotImplemented {
            feature: "JSON_TABLE: multiple NESTED PATH siblings in the same COLUMNS(...) \
                      list — deferred to 11.20c"
                .into(),
        });
    }

    let mut out = Vec::with_capacity(cols.len());
    for c in cols {
        match c {
            JsonTableColumn::Regular {
                name,
                ty,
                path,
                on_empty,
                on_error,
            } => {
                let slot = *next_slot;
                *next_slot += 1;
                out.push(JsonTableColumnSpec {
                    name: name.clone(),
                    ty: *ty,
                    kind: JsonTableColumnKind::Regular {
                        slot,
                        path: parse_restricted_path(path)?,
                        on_empty: on_empty.clone(),
                        on_error: on_error.clone(),
                    },
                });
            }
            JsonTableColumn::Ordinality { name } => {
                let slot = *next_slot;
                *next_slot += 1;
                out.push(JsonTableColumnSpec {
                    name: name.clone(),
                    ty: DataType::BigInt,
                    kind: JsonTableColumnKind::Ordinality { slot },
                });
            }
            JsonTableColumn::Exists {
                name,
                ty,
                path,
                on_error,
            } => {
                let slot = *next_slot;
                *next_slot += 1;
                out.push(JsonTableColumnSpec {
                    name: name.clone(),
                    ty: *ty,
                    kind: JsonTableColumnKind::Exists {
                        slot,
                        path: parse_restricted_path(path)?,
                        on_error: on_error.clone(),
                    },
                });
            }
            JsonTableColumn::Nested { path, columns } => {
                if depth >= 1 {
                    return Err(DbError::NotImplemented {
                        feature: "JSON_TABLE: NESTED PATH inside NESTED PATH (depth ≥ 2) — \
                                  deferred to 11.20c"
                            .into(),
                    });
                }
                let start = *next_slot;
                let children = compile_columns_recursive(columns, next_slot, depth + 1)?;
                let end = *next_slot;
                // Nested contributes no own slot; it expands into child slots.
                // The spec's `name`/`ty` are placeholders for uniformity.
                out.push(JsonTableColumnSpec {
                    name: String::new(),
                    ty: DataType::BigInt,
                    kind: JsonTableColumnKind::Nested {
                        path: parse_restricted_path(path)?,
                        children,
                        slot_range: (start, end),
                    },
                });
            }
        }
    }
    Ok(out)
}

// ── ColumnDef projection (for analyzer_bind) ────────────────────────────────

pub fn column_defs_for_ast(jt: &JsonTable) -> Result<Vec<ColumnDef>, DbError> {
    let mut out = Vec::new();
    flatten_defs_recursive(&jt.columns, &mut out, /*inside_nested=*/ false)?;
    Ok(out)
}

fn flatten_defs_recursive(
    cols: &[JsonTableColumn],
    out: &mut Vec<ColumnDef>,
    inside_nested: bool,
) -> Result<(), DbError> {
    for c in cols {
        match c {
            JsonTableColumn::Regular {
                name,
                ty,
                on_empty,
                on_error,
                ..
            } => {
                let nullable = inside_nested
                    || matches!(on_empty, SqlJsonOnBehavior::Null)
                    || matches!(on_error, SqlJsonOnBehavior::Null);
                out.push(ColumnDef {
                    table_id: 0 as TableId,
                    col_idx: out.len() as u16,
                    name: name.clone(),
                    col_type: datatype_to_column_type(ty)?,
                    nullable,
                    auto_increment: false,
                    type_len: 0,
                    is_fixed_len: false,
                    default_expr: None,
                    on_update_expr: None,
                });
            }
            JsonTableColumn::Ordinality { name } => {
                out.push(ColumnDef {
                    table_id: 0 as TableId,
                    col_idx: out.len() as u16,
                    name: name.clone(),
                    col_type: ColumnType::BigInt,
                    // Inner ordinality column IS nullable on LEFT-OUTER pad
                    // (no child matches → NULL-pad row for that level).
                    nullable: inside_nested,
                    auto_increment: false,
                    type_len: 0,
                    is_fixed_len: false,
                    default_expr: None,
                    on_update_expr: None,
                });
            }
            JsonTableColumn::Exists { name, ty, .. } => {
                out.push(ColumnDef {
                    table_id: 0 as TableId,
                    col_idx: out.len() as u16,
                    name: name.clone(),
                    col_type: datatype_to_column_type(ty)?,
                    nullable: inside_nested,
                    auto_increment: false,
                    type_len: 0,
                    is_fixed_len: false,
                    default_expr: None,
                    on_update_expr: None,
                });
            }
            JsonTableColumn::Nested { columns, .. } => {
                flatten_defs_recursive(columns, out, /*inside_nested=*/ true)?;
            }
        }
    }
    Ok(())
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
    let mut rows: Vec<Vec<Value>> = Vec::new();

    for (i, parent) in parent_matches.iter().enumerate() {
        let ord = (i as i64) + 1;
        // Row template initialised to NULLs; fill parent-level leaves then
        // expand via NESTED PATH children (Phase 11.20b: single NESTED).
        let mut template: Vec<Value> = vec![Value::Null; spec.total_slots];

        // Fill leaf columns and locate the (at most one) nested child.
        let mut nested_child: Option<&JsonTableColumnSpec> = None;
        for col in &spec.columns {
            match &col.kind {
                JsonTableColumnKind::Ordinality { slot } => {
                    template[*slot] = Value::BigInt(ord);
                }
                JsonTableColumnKind::Regular {
                    slot,
                    path,
                    on_empty,
                    on_error,
                } => {
                    template[*slot] = materialize_regular(
                        parent, path, col.ty, on_empty, on_error, outer_row, sq,
                    )?;
                }
                JsonTableColumnKind::Exists {
                    slot,
                    path,
                    on_error,
                } => {
                    template[*slot] =
                        materialize_exists(parent, path, col.ty, on_error, outer_row, sq)?;
                }
                JsonTableColumnKind::Nested { .. } => {
                    nested_child = Some(col);
                }
            }
        }

        // Expand NESTED (Phase 11.20b: depth 1, at most one per list).
        match nested_child {
            None => rows.push(template),
            Some(nested) => {
                let (path, children, (start, end)) = match &nested.kind {
                    JsonTableColumnKind::Nested {
                        path,
                        children,
                        slot_range,
                    } => (path, children, *slot_range),
                    _ => unreachable!("nested_child is Nested"),
                };
                let child_matches = walk_path_owned(parent, path);
                if child_matches.is_empty() {
                    // LEFT-OUTER NULL pad: emit one row; child slots stay
                    // NULL from the template initialisation.
                    rows.push(template);
                } else {
                    for (j, child) in child_matches.iter().enumerate() {
                        let child_ord = (j as i64) + 1;
                        let mut row = template.clone();
                        fill_leaf_children(children, child, child_ord, outer_row, sq, &mut row)?;
                        // `fill_leaf_children` writes into the (start..end)
                        // slot range; slots outside that range retain their
                        // parent values from the template.
                        let _ = (start, end); // no-op; present for clarity
                        rows.push(row);
                    }
                }
            }
        }
    }
    Ok(rows)
}

/// Phase 11.20b: fill child-level leaf slots for one child match.
/// Depth-1 constraint is enforced at compile time, so `children` contains
/// only `Regular`, `Ordinality`, and `Exists` kinds.
fn fill_leaf_children<R: SubqueryRunner>(
    children: &[JsonTableColumnSpec],
    child: &serde_json::Value,
    child_ord: i64,
    outer_row: &[Value],
    sq: &mut R,
    row: &mut [Value],
) -> Result<(), DbError> {
    for c in children {
        match &c.kind {
            JsonTableColumnKind::Ordinality { slot } => {
                row[*slot] = Value::BigInt(child_ord);
            }
            JsonTableColumnKind::Regular {
                slot,
                path,
                on_empty,
                on_error,
            } => {
                row[*slot] =
                    materialize_regular(child, path, c.ty, on_empty, on_error, outer_row, sq)?;
            }
            JsonTableColumnKind::Exists {
                slot,
                path,
                on_error,
            } => {
                row[*slot] = materialize_exists(child, path, c.ty, on_error, outer_row, sq)?;
            }
            JsonTableColumnKind::Nested { .. } => {
                // Compile-time depth check in `compile_columns_recursive`
                // prevents reaching this arm in 11.20b.
                return Err(DbError::NotImplemented {
                    feature: "JSON_TABLE: multi-level NESTED — deferred to 11.20c".into(),
                });
            }
        }
    }
    Ok(())
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
