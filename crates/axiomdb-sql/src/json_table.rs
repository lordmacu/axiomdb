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
use crate::eval::functions::{execute_jsonpath_owned_env, parse_jsonpath, PassingEnv, PathStep};
use crate::eval::{eval_with, SubqueryRunner};
use crate::expr::{Expr, SqlJsonOnBehavior, SqlJsonQuotes, SqlJsonWrapper};

// ── Lowered form ─────────────────────────────────────────────────────────────

/// Compiled JSON_TABLE: every JSONPath string is parsed once here, so each
/// materialization just walks.
///
/// Phase 11.20d1 migrated path storage from the legacy restricted walker
/// to `eval::functions::json::PathStep` so that filters, `.size()` /
/// `.type()` accessors, and PASSING `$var` references all work uniformly.
#[derive(Debug, Clone)]
pub struct JsonTableSpec {
    pub alias: String,
    pub(crate) row_path: Vec<PathStep>,
    pub columns: Vec<JsonTableColumnSpec>,
    /// Total number of slots in each emitted row (sum of leaf columns across
    /// every level of NESTED PATH). Leaves count 1 each; `Nested` columns
    /// expand into their children's slot range.
    pub total_slots: usize,
    /// PASSING bindings (Phase 11.20d1). Expressions are evaluated once
    /// per JSON_TABLE invocation before any path walking. The resolved
    /// values feed a `PassingEnv` shared by the row path, every column
    /// path, and every NESTED path at any depth.
    pub passing: Vec<(Expr, String)>,
}

#[derive(Debug, Clone)]
pub struct JsonTableColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub(crate) kind: JsonTableColumnKind,
}

#[derive(Debug, Clone)]
pub(crate) enum JsonTableColumnKind {
    Regular {
        /// Slot index in the emitted row.
        slot: usize,
        path: Vec<PathStep>,
        /// Phase 11.20d1 — default `Without`.
        wrapper: SqlJsonWrapper,
        /// Phase 11.20d1 — default `Keep`.
        quotes: SqlJsonQuotes,
        on_empty: SqlJsonOnBehavior,
        on_error: SqlJsonOnBehavior,
    },
    Ordinality {
        slot: usize,
    },
    Exists {
        slot: usize,
        path: Vec<PathStep>,
        on_error: SqlJsonOnBehavior,
    },
    /// Phase 11.20b — a single-level `NESTED PATH ... COLUMNS(...)` subtree.
    /// 11.20c lifted the depth and multi-sibling restrictions; `children`
    /// now contains any `JsonTableColumnSpec`, including further `Nested`.
    /// Slot range `[start, end)` is a contiguous span inside the emitted
    /// row's flat slot vector.
    Nested {
        path: Vec<PathStep>,
        children: Vec<JsonTableColumnSpec>,
        /// `[start, end)` slot range occupied by this subtree; retained for
        /// debug/EXPLAIN output and future LATERAL-pruning work (11.20d3).
        #[allow(dead_code)]
        slot_range: (usize, usize),
    },
}

// ── Compile ──────────────────────────────────────────────────────────────────

pub fn compile_json_table(jt: &JsonTable) -> Result<JsonTableSpec, DbError> {
    let row_path = parse_jsonpath(&jt.row_path)?;

    // Enforce: unique column names across every level.
    let mut seen = std::collections::HashSet::new();
    collect_names_recursive(&jt.columns, &mut seen)?;

    // Enforce: unique PASSING variable names (case-insensitive). The
    // parser also checks this, but analyzer passes could theoretically
    // synthesize duplicates — defense in depth.
    {
        let mut seen_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, name) in &jt.passing {
            if !seen_vars.insert(name.to_ascii_lowercase()) {
                return Err(DbError::ParseError {
                    message: format!("JSON_TABLE: duplicate PASSING variable `{name}`"),
                    position: None,
                });
            }
        }
    }

    // Depth-first slot assignment. Phase 11.20c supports arbitrary NESTED
    // depth and multi-sibling; a defensive depth ≤ 32 guard remains.
    let mut next_slot = 0usize;
    let columns = compile_columns_recursive(&jt.columns, &mut next_slot, 0)?;

    let alias = jt.alias.clone().unwrap_or_else(|| "json_table".into());
    Ok(JsonTableSpec {
        alias,
        row_path,
        columns,
        total_slots: next_slot,
        passing: jt.passing.clone(),
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

    // Phase 11.20c — defensive depth limit. Multi-sibling and multi-level
    // NESTED are now supported; this guard exists solely to stop
    // pathological AST recursion (the SQL compiler already rejects inputs
    // deeper than a few hundred tokens, so 32 is well beyond any real
    // workload).
    if depth > 32 {
        return Err(DbError::ParseError {
            message: "JSON_TABLE: NESTED PATH recursion depth exceeds 32".into(),
            position: None,
        });
    }

    let mut out = Vec::with_capacity(cols.len());
    for c in cols {
        match c {
            JsonTableColumn::Regular {
                name,
                ty,
                path,
                wrapper,
                quotes,
                on_empty,
                on_error,
            } => {
                let slot = *next_slot;
                *next_slot += 1;
                out.push(JsonTableColumnSpec {
                    name: name.clone(),
                    ty: ty.clone(),
                    kind: JsonTableColumnKind::Regular {
                        slot,
                        path: parse_jsonpath(path)?,
                        wrapper: *wrapper,
                        quotes: *quotes,
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
                    ty: ty.clone(),
                    kind: JsonTableColumnKind::Exists {
                        slot,
                        path: parse_jsonpath(path)?,
                        on_error: on_error.clone(),
                    },
                });
            }
            JsonTableColumn::Nested { path, columns } => {
                let start = *next_slot;
                let children = compile_columns_recursive(columns, next_slot, depth + 1)?;
                let end = *next_slot;
                // Nested contributes no own slot; it expands into child slots.
                // The spec's `name`/`ty` are placeholders for uniformity.
                out.push(JsonTableColumnSpec {
                    name: String::new(),
                    ty: DataType::BigInt,
                    kind: JsonTableColumnKind::Nested {
                        path: parse_jsonpath(path)?,
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
                    generated_expr: None,
                    collation: None,
                    generated_stored: false,
                    enum_type_name: None,
                    array_element_type: None,
                    array_ndims: None,
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
                    generated_expr: None,
                    collation: None,
                    generated_stored: false,
                    enum_type_name: None,
                    array_element_type: None,
                    array_ndims: None,
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
                    generated_expr: None,
                    collation: None,
                    generated_stored: false,
                    enum_type_name: None,
                    array_element_type: None,
                    array_ndims: None,
                });
            }
            JsonTableColumn::Nested { columns, .. } => {
                flatten_defs_recursive(columns, out, /*inside_nested=*/ true)?;
            }
        }
    }
    Ok(())
}

pub fn datatype_to_column_type_pub(dt: &DataType) -> Result<ColumnType, DbError> {
    datatype_to_column_type(dt)
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
        DataType::Array(_) => {
            return Err(DbError::NotImplemented {
                feature: "JSON_TABLE with array return type".into(),
            });
        }
        DataType::Range(_) => {
            return Err(DbError::NotImplemented {
                feature: "JSON_TABLE with range return type".into(),
            });
        }
        DataType::Money => {
            return Err(DbError::NotImplemented {
                feature: "JSON_TABLE with money return type".into(),
            });
        }
        DataType::Composite(_) => {
            return Err(DbError::NotImplemented {
                feature: "JSON_TABLE with composite return type".into(),
            });
        }
        DataType::Ltree => ColumnType::Ltree,
        DataType::Xml => ColumnType::Xml,
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
    // Phase 11.20d1 — evaluate PASSING bindings once per invocation.
    // Correlation (doc or PASSING referencing outer columns) is deferred to
    // Phase 11.20d3; this subphase evaluates bindings against the current
    // `outer_row` (which is `&[]` when JSON_TABLE is the first FROM entry).
    let mut env = PassingEnv::new();
    for (expr, name) in &spec.passing {
        let v = eval_with(expr, outer_row, sq)?;
        env.insert(name.clone(), value_to_serde_for_env(&v)?);
    }

    let parent_matches = execute_jsonpath_owned_env(doc, &spec.row_path, &env);
    let mut rows: Vec<Vec<Value>> = Vec::new();

    for (i, parent) in parent_matches.iter().enumerate() {
        let ord = (i as i64) + 1;
        // Row template initialised to NULLs; the recursive emitter fills
        // leaves at every level and emits UNION rows across sibling
        // NESTED entries (Phase 11.20c — arbitrary depth, arbitrary
        // number of siblings).
        let template: Vec<Value> = vec![Value::Null; spec.total_slots];
        emit_rows_rec(
            &spec.columns,
            parent,
            template,
            ord,
            outer_row,
            sq,
            &env,
            &mut rows,
        )?;
    }
    Ok(rows)
}

/// Render a SQL `Value` as a `serde_json::Value` for use as a PASSING
/// variable binding. Keeps scalar JSON types native, encodes JSONB by
/// decoding its binary form, and falls back to string representations for
/// temporal / UUID / bytes. Used only by the JSON_TABLE env builder.
fn value_to_serde_for_env(v: &Value) -> Result<serde_json::Value, DbError> {
    match v {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int(i) => Ok(serde_json::json!(*i)),
        Value::BigInt(i) => Ok(serde_json::json!(*i)),
        Value::Real(f) => Ok(serde_json::json!(*f)),
        Value::Text(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Json(s) => serde_json::from_str(s).map_err(|e| DbError::InvalidCoercion {
            from: "JSON".into(),
            to: "PASSING var".into(),
            value: truncate_for_error(s),
            reason: format!("invalid JSON in PASSING binding: {e}"),
        }),
        Value::Jsonb(b) => Ok(JsonbDecoder::decode(b.as_ref())?),
        other => {
            // Timestamp / Date / Uuid / Bytes / Decimal → render as string,
            // the same shape a user would see via the MySQL wire.
            Ok(serde_json::Value::String(format!("{other:?}")))
        }
    }
}

/// Phase 11.20c — recursive row emitter supporting arbitrary NESTED depth
/// and arbitrary number of sibling NESTED entries per `COLUMNS(...)`
/// list.
///
/// Semantics:
/// 1. Fill this level's leaves (Regular / Ordinality / Exists) into the
///    caller-supplied template in place.
/// 2. If there are zero NESTED siblings at this level, push the template.
/// 3. Otherwise emit **UNION** across each NESTED sibling: each sibling
///    walks its child path and produces `max(1, |child_matches|)` rows,
///    cloning the template per row so the other siblings' ranges stay
///    `NULL` (LEFT-OUTER pad).
#[allow(clippy::too_many_arguments)]
fn emit_rows_rec<R: SubqueryRunner>(
    cols: &[JsonTableColumnSpec],
    node: &serde_json::Value,
    mut template: Vec<Value>,
    level_ord: i64,
    outer_row: &[Value],
    sq: &mut R,
    env: &PassingEnv,
    rows: &mut Vec<Vec<Value>>,
) -> Result<(), DbError> {
    // Pass 1 — fill this level's leaf columns in place.
    for col in cols {
        match &col.kind {
            JsonTableColumnKind::Ordinality { slot } => {
                template[*slot] = Value::BigInt(level_ord);
            }
            JsonTableColumnKind::Regular {
                slot,
                path,
                wrapper,
                quotes,
                on_empty,
                on_error,
            } => {
                template[*slot] = materialize_regular(
                    node,
                    path,
                    col.ty.clone(),
                    *wrapper,
                    *quotes,
                    on_empty,
                    on_error,
                    outer_row,
                    sq,
                    env,
                )?;
            }
            JsonTableColumnKind::Exists {
                slot,
                path,
                on_error,
            } => {
                template[*slot] =
                    materialize_exists(node, path, col.ty.clone(), on_error, outer_row, sq, env)?;
            }
            JsonTableColumnKind::Nested { .. } => {
                // Handled in pass 2.
            }
        }
    }

    // Pass 2 — collect NESTED siblings at this level.
    let nested_siblings: Vec<&JsonTableColumnSpec> = cols
        .iter()
        .filter(|c| matches!(c.kind, JsonTableColumnKind::Nested { .. }))
        .collect();

    if nested_siblings.is_empty() {
        rows.push(template);
        return Ok(());
    }

    // UNION across siblings: each sibling contributes its own row set
    // with the other siblings' slot ranges left NULL (template init).
    for nested in nested_siblings {
        let (path, children) = match &nested.kind {
            JsonTableColumnKind::Nested { path, children, .. } => (path, children),
            _ => unreachable!("filtered to Nested above"),
        };
        let child_matches = execute_jsonpath_owned_env(node, path, env);
        if child_matches.is_empty() {
            // LEFT-OUTER pad for this sibling. Clone so other siblings see
            // the same parent template on the next iteration.
            rows.push(template.clone());
        } else {
            for (j, child) in child_matches.iter().enumerate() {
                let child_ord = (j as i64) + 1;
                emit_rows_rec(
                    children,
                    child,
                    template.clone(),
                    child_ord,
                    outer_row,
                    sq,
                    env,
                    rows,
                )?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_regular<R: SubqueryRunner>(
    parent: &serde_json::Value,
    path: &[PathStep],
    ty: DataType,
    wrapper: SqlJsonWrapper,
    quotes: SqlJsonQuotes,
    on_empty: &SqlJsonOnBehavior,
    on_error: &SqlJsonOnBehavior,
    outer_row: &[Value],
    sq: &mut R,
    env: &PassingEnv,
) -> Result<Value, DbError> {
    let hits = execute_jsonpath_owned_env(parent, path, env);
    if hits.is_empty() {
        return apply_on_behavior(on_empty, ty, outer_row, sq);
    }
    // Phase 11.20d1 — apply WRAPPER before coercion:
    //   WITHOUT           → single hit: pass-through; multi-hit: ON ERROR.
    //   UNCONDITIONAL     → always wrap as JSON array.
    //   CONDITIONAL       → single array hit: unwrap; otherwise wrap.
    let wrapped: serde_json::Value = match wrapper {
        SqlJsonWrapper::Without => {
            if hits.len() == 1 {
                hits.into_iter().next().unwrap()
            } else {
                return apply_on_behavior(on_error, ty, outer_row, sq);
            }
        }
        SqlJsonWrapper::Unconditional => serde_json::Value::Array(hits),
        SqlJsonWrapper::Conditional => {
            if hits.len() == 1 && hits[0].is_array() {
                hits.into_iter().next().unwrap()
            } else {
                serde_json::Value::Array(hits)
            }
        }
    };
    // Phase 11.20d1 — OMIT QUOTES on a TEXT-returning column strips the
    // JSON double-quote pair around a string scalar. Parser already
    // enforces OMIT is only allowed on TEXT.
    if matches!(quotes, SqlJsonQuotes::Omit) && matches!(ty, DataType::Text) {
        if let serde_json::Value::String(s) = &wrapped {
            return Ok(Value::Text(s.clone()));
        }
    }
    match serde_to_value_typed(&wrapped, ty.clone()) {
        Ok(v) => Ok(v),
        Err(_) => apply_on_behavior(on_error, ty, outer_row, sq),
    }
}

fn materialize_exists<R: SubqueryRunner>(
    parent: &serde_json::Value,
    path: &[PathStep],
    ty: DataType,
    on_error: &SqlJsonOnBehavior,
    outer_row: &[Value],
    sq: &mut R,
    env: &PassingEnv,
) -> Result<Value, DbError> {
    let _ = on_error; // the full engine does not surface strict-mode errors here
    let hits = execute_jsonpath_owned_env(parent, path, env);
    let b = !hits.is_empty();
    let base = Value::Bool(b);
    if ty == DataType::Bool {
        Ok(base)
    } else {
        // EXISTS column declared as INT → coerce TRUE/FALSE to 1/0.
        coerce(base, ty.clone(), CoercionMode::Strict)
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
    coerce(base, ty.clone(), CoercionMode::Strict)
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
            data_type: c.ty.clone(),
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
/// Phase 11.20d3 — true when JSON_TABLE call requires per-outer-row
/// re-materialization because its `doc` or any PASSING expression
/// references outer columns. Non-correlated calls (literal / param
/// doc, literal PASSING) return false and stay on the single-
/// materialization fast path.
pub fn jsontable_is_correlated(jt: &crate::ast::JsonTable) -> bool {
    doc_has_column_refs(&jt.doc) || jt.passing.iter().any(|(expr, _)| doc_has_column_refs(expr))
}

/// Phase 21.9 — Returns `true` if the expression tree contains an
/// `OuterColumn` node at depth 0 (immediate outer scope). Used to detect
/// correlated LATERAL subqueries that must be re-evaluated per outer row.
pub fn expr_has_outer_column_refs(expr: &crate::expr::Expr) -> bool {
    use crate::expr::Expr;
    match expr {
        Expr::OuterColumn { depth: 0, .. } => true,
        // Depth > 0: deeper nesting — not relevant for LATERAL correlation detection
        Expr::OuterColumn { .. } => false,
        Expr::Column { .. }
        | Expr::Literal(_)
        | Expr::Default
        | Expr::Param { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. } => false,
        Expr::UnaryOp { operand, .. } | Expr::Collate { expr: operand, .. } => {
            expr_has_outer_column_refs(operand)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_outer_column_refs(left) || expr_has_outer_column_refs(right)
        }
        Expr::Function { args, .. } => args.iter().any(expr_has_outer_column_refs),
        Expr::Window { spec, .. } => {
            spec.partition_by.iter().any(expr_has_outer_column_refs)
                || spec
                    .order_by
                    .iter()
                    .any(|item| expr_has_outer_column_refs(&item.expr))
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand
                .as_ref()
                .is_some_and(|o| expr_has_outer_column_refs(o))
                || when_thens
                    .iter()
                    .any(|(a, b)| expr_has_outer_column_refs(a) || expr_has_outer_column_refs(b))
                || else_result
                    .as_ref()
                    .is_some_and(|e| expr_has_outer_column_refs(e))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_outer_column_refs(expr)
                || expr_has_outer_column_refs(low)
                || expr_has_outer_column_refs(high)
        }
        Expr::In { expr, list, .. } => {
            expr_has_outer_column_refs(expr) || list.iter().any(expr_has_outer_column_refs)
        }
        Expr::IsBoolean { expr, .. } => expr_has_outer_column_refs(expr),
        Expr::Cast { expr, .. } => expr_has_outer_column_refs(expr),
        Expr::IsNull { expr, .. } => expr_has_outer_column_refs(expr),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_outer_column_refs(expr)
                || expr_has_outer_column_refs(pattern)
                || escape
                    .as_ref()
                    .is_some_and(|e| expr_has_outer_column_refs(e))
        }
        Expr::SqlJsonQuery {
            doc,
            passing,
            on_empty,
            on_error,
            ..
        } => {
            expr_has_outer_column_refs(doc)
                || passing.iter().any(|(e, _)| expr_has_outer_column_refs(e))
                || on_behavior_has_outer_col_refs(on_empty)
                || on_behavior_has_outer_col_refs(on_error)
        }
        Expr::GroupConcat { expr, order_by, .. } => {
            expr_has_outer_column_refs(expr)
                || order_by.iter().any(|(e, _)| expr_has_outer_column_refs(e))
        }
        Expr::ArrayAgg { expr, order_by, .. } => {
            expr_has_outer_column_refs(expr)
                || order_by.iter().any(|(e, _)| expr_has_outer_column_refs(e))
        }
        Expr::Grouping { args, .. } => args.iter().any(expr_has_outer_column_refs),
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => elements.iter().any(expr_has_outer_column_refs),
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript {
            array,
            index,
            slice,
        } => {
            expr_has_outer_column_refs(array)
                || expr_has_outer_column_refs(index)
                || slice
                    .as_ref()
                    .is_some_and(|s| expr_has_outer_column_refs(s))
        }
        // Phase 20.4 — ANY/ALL: recurse into expr (comparison target) and array.
        Expr::AnyOf { expr, array, .. } | Expr::AllOf { expr, array, .. } => {
            expr_has_outer_column_refs(expr) || expr_has_outer_column_refs(array)
        }
        Expr::Row(elems) => elems.iter().any(expr_has_outer_column_refs),
        Expr::FieldAccess { .. } => false,
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => false,
        // Phase 20.20 — XML constructor forms: recurse into sub-expressions.
        Expr::XmlElement { attrs, content, .. } => {
            attrs.iter().any(|(e, _)| expr_has_outer_column_refs(e))
                || content.iter().any(expr_has_outer_column_refs)
        }
        Expr::XmlForest { items } => items.iter().any(|(e, _)| expr_has_outer_column_refs(e)),
        Expr::XmlRoot { doc, .. } => expr_has_outer_column_refs(doc),
        Expr::XmlConcat { args } => args.iter().any(expr_has_outer_column_refs),
        Expr::XmlQuery { doc, .. } => expr_has_outer_column_refs(doc),
    }
}

fn on_behavior_has_outer_col_refs(b: &SqlJsonOnBehavior) -> bool {
    match b {
        SqlJsonOnBehavior::Default(e) => expr_has_outer_column_refs(e),
        _ => false,
    }
}

/// Phase 21.9 — Returns `col_idx` of the first depth-0 `OuterColumn` found
/// in `expr`, or `None` if the expression contains no outer column references.
pub fn outer_column_idx(expr: &crate::expr::Expr) -> Option<usize> {
    use crate::expr::Expr;
    match expr {
        Expr::OuterColumn {
            col_idx, depth: 0, ..
        } => Some(*col_idx),
        Expr::UnaryOp { operand, .. } => outer_column_idx(operand),
        Expr::BinaryOp { left, right, .. } => {
            outer_column_idx(left).or_else(|| outer_column_idx(right))
        }
        Expr::Function { args, .. } => args.iter().find_map(outer_column_idx),
        Expr::Window { spec, .. } => {
            spec.partition_by
                .iter()
                .find_map(outer_column_idx)
                .or_else(|| {
                    spec.order_by
                        .iter()
                        .find_map(|item| outer_column_idx(&item.expr))
                })
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => operand
            .as_ref()
            .and_then(|o| outer_column_idx(o))
            .or_else(|| {
                when_thens
                    .iter()
                    .find_map(|(a, b)| outer_column_idx(a).or_else(|| outer_column_idx(b)))
            })
            .or_else(|| else_result.as_ref().and_then(|e| outer_column_idx(e))),
        // Depth > 0: deeper nesting — not relevant for LATERAL correlation
        Expr::OuterColumn { .. } => None,
        _ => None,
    }
}

/// Phase 21.9 — Returns `true` if `subquery`'s SELECT list, WHERE, GROUP BY,
/// HAVING, ORDER BY, or JOIN ON expressions contain any `OuterColumn` node at
/// depth 0 whose `col_idx` refers to a column that precedes the subquery's
/// own columns (i.e., `col_idx < left_col_count`). Such a subquery is
/// correlated to the outer query and must be re-evaluated per outer row.
pub fn subquery_is_correlated(subquery: &crate::ast::SelectStmt, left_col_count: usize) -> bool {
    // Check SELECT items
    for item in &subquery.columns {
        if let crate::ast::SelectItem::Expr { expr, .. } = item {
            if expr_has_outer_column_refs(expr) {
                if let Some(idx) = outer_column_idx(expr) {
                    if idx < left_col_count {
                        return true;
                    }
                }
            }
        }
    }
    // Check WHERE
    if let Some(ref where_clause) = subquery.where_clause {
        if expr_has_outer_column_refs(where_clause) {
            if let Some(idx) = outer_column_idx(where_clause) {
                if idx < left_col_count {
                    return true;
                }
            }
        }
    }
    // Check GROUP BY
    for expr in subquery.group_by.exprs() {
        if expr_has_outer_column_refs(expr) {
            if let Some(idx) = outer_column_idx(expr) {
                if idx < left_col_count {
                    return true;
                }
            }
        }
    }
    // Check HAVING
    if let Some(ref having) = subquery.having {
        if expr_has_outer_column_refs(having) {
            if let Some(idx) = outer_column_idx(having) {
                if idx < left_col_count {
                    return true;
                }
            }
        }
    }
    // Check ORDER BY
    for ob in &subquery.order_by {
        if expr_has_outer_column_refs(&ob.expr) {
            if let Some(idx) = outer_column_idx(&ob.expr) {
                if idx < left_col_count {
                    return true;
                }
            }
        }
    }
    // Check DISTINCT ON
    for e in &subquery.distinct_on {
        if expr_has_outer_column_refs(e) {
            if let Some(idx) = outer_column_idx(e) {
                if idx < left_col_count {
                    return true;
                }
            }
        }
    }
    // Check JOIN conditions in the subquery itself
    for join in &subquery.joins {
        if let crate::ast::JoinCondition::On(ref expr) = join.condition {
            if expr_has_outer_column_refs(expr) {
                if let Some(idx) = outer_column_idx(expr) {
                    if idx < left_col_count {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub fn doc_has_column_refs(expr: &crate::expr::Expr) -> bool {
    use crate::expr::Expr;
    match expr {
        Expr::Column { .. }
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. } => true,
        Expr::Literal(_) | Expr::Default | Expr::Param { .. } => false,
        Expr::UnaryOp { operand, .. } | Expr::Collate { expr: operand, .. } => {
            doc_has_column_refs(operand)
        }
        Expr::BinaryOp { left, right, .. } => {
            doc_has_column_refs(left) || doc_has_column_refs(right)
        }
        Expr::Function { args, .. } => args.iter().any(doc_has_column_refs),
        Expr::Window { spec, .. } => {
            spec.partition_by.iter().any(doc_has_column_refs)
                || spec
                    .order_by
                    .iter()
                    .any(|item| doc_has_column_refs(&item.expr))
        }
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
        Expr::ArrayAgg { expr, order_by, .. } => {
            doc_has_column_refs(expr) || order_by.iter().any(|(e, _)| doc_has_column_refs(e))
        }
        Expr::Grouping { args, .. } => args.iter().any(doc_has_column_refs),
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => elements.iter().any(doc_has_column_refs),
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript {
            array,
            index,
            slice,
        } => {
            doc_has_column_refs(array)
                || doc_has_column_refs(index)
                || slice.as_ref().is_some_and(|s| doc_has_column_refs(s))
        }
        // Phase 20.4 — ANY/ALL: recurse into expr (comparison target) and array.
        Expr::AnyOf { expr, array, .. } | Expr::AllOf { expr, array, .. } => {
            doc_has_column_refs(expr) || doc_has_column_refs(array)
        }
        Expr::Row(elems) => elems.iter().any(doc_has_column_refs),
        Expr::FieldAccess { .. } => true,
        // Subqueries are not correlation we can detect at this layer — treat
        // as "yes" to force the NotImplemented branch (users can wrap the
        // doc in a CTE / derived table if they need constant materialization).
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
        // Phase 20.20 — XML constructor forms: recurse into sub-expressions.
        Expr::XmlElement { attrs, content, .. } => {
            attrs.iter().any(|(e, _)| doc_has_column_refs(e))
                || content.iter().any(doc_has_column_refs)
        }
        Expr::XmlForest { items } => items.iter().any(|(e, _)| doc_has_column_refs(e)),
        Expr::XmlRoot { doc, .. } => doc_has_column_refs(doc),
        Expr::XmlConcat { args } => args.iter().any(doc_has_column_refs),
        Expr::XmlQuery { doc, .. } => doc_has_column_refs(doc),
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
            passing: Vec::new(),
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
            passing: Vec::new(),
            columns: vec![
                JsonTableColumn::Ordinality { name: "ord".into() },
                JsonTableColumn::Regular {
                    name: "ORD".into(), // case-insensitive collision
                    ty: DataType::Int,
                    path: "$.a".into(),
                    wrapper: SqlJsonWrapper::Without,
                    quotes: SqlJsonQuotes::Keep,
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
    fn compile_rejects_duplicate_passing_var() {
        let jt = JsonTable {
            doc: crate::expr::Expr::Literal(Value::Null),
            row_path: "$".into(),
            passing: vec![
                (crate::expr::Expr::Literal(Value::BigInt(1)), "v".into()),
                (crate::expr::Expr::Literal(Value::BigInt(2)), "V".into()),
            ],
            columns: vec![JsonTableColumn::Ordinality { name: "a".into() }],
            alias: None,
        };
        let err = compile_json_table(&jt).unwrap_err();
        assert!(format!("{err:?}").contains("duplicate PASSING"));
    }

    #[test]
    fn compile_rejects_invalid_path() {
        let jt = JsonTable {
            doc: crate::expr::Expr::Literal(Value::Null),
            row_path: "not-a-path".into(),
            passing: Vec::new(),
            columns: vec![JsonTableColumn::Ordinality { name: "a".into() }],
            alias: None,
        };
        assert!(compile_json_table(&jt).is_err());
    }
}
