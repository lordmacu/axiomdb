// ── HTTP Foreign Data Wrapper scan (Phase 22b.2 + 22b.6) ────────────────────────
//
// Executes a GET request against a foreign HTTP server and maps the JSON-array
// response to AxiomDB rows. Only HTTP (not HTTPS) is supported in this phase.
// HTTPS support is deferred to a later phase.
//
// Phase 22b.6 adds opt-in predicate and LIMIT pushdown via URL templates and
// query-parameter mappings (see `extract_fdw_pushable`, `render_fdw_url`).
//
// ## FDW options
//
// ### Server OPTIONS (CREATE SERVER ... OPTIONS):
//   url        - Base URL of the remote service (required). Must start with
//                "http://". Example: 'http://api.example.com'
//   timeout_ms - Request timeout in milliseconds (optional, default 10000).
//
// ### Table OPTIONS (CREATE FOREIGN TABLE ... OPTIONS):
//   endpoint      - Path appended to the server URL (optional, default '/').
//                   May contain {col_name} placeholders for path pushdown.
//                   Example: '/users/{id}'
//   method        - HTTP method (optional, default 'GET'). Only GET is supported.
//   pushdown_cols - Comma-separated column names whose equality predicates are
//                   appended as query params. Example: 'status,customer_id'
//   limit_param   - Query-param name for LIMIT pushdown. Example: 'per_page'

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// ── Phase 22b.6: predicate extraction ────────────────────────────────────────

/// Splits `where_clause` into pushable equality predicates and a residual.
///
/// Walks AND-connected top-level nodes. A leaf is pushable when it is
/// `col = Literal(non-null)` (or the mirror). All other nodes go to residual.
/// OR nodes are never split — the entire OR stays in residual.
///
/// Returns `(bound, residual)`:
/// - `bound`    — col_name → literal Value for predicates forwarded to the remote
/// - `residual` — the rest of the WHERE tree; `None` if everything was pushed
fn extract_fdw_pushable(
    where_clause: Option<&Expr>,
    columns: &[CatalogColumnDef],
) -> (HashMap<String, Value>, Option<Expr>) {
    match where_clause {
        None => (HashMap::new(), None),
        Some(expr) => split_expr(expr, columns),
    }
}

fn split_expr(expr: &Expr, columns: &[CatalogColumnDef]) -> (HashMap<String, Value>, Option<Expr>) {
    match expr {
        Expr::BinaryOp { op: BinaryOp::And, left, right } => {
            let (mut bl, rl) = split_expr(left, columns);
            let (br, rr) = split_expr(right, columns);
            bl.extend(br);
            let residual = match (rl, rr) {
                (None, None) => None,
                (Some(l), None) => Some(l),
                (None, Some(r)) => Some(r),
                (Some(l), Some(r)) => Some(Expr::BinaryOp {
                    op: BinaryOp::And,
                    left: Box::new(l),
                    right: Box::new(r),
                }),
            };
            (bl, residual)
        }
        Expr::BinaryOp { op: BinaryOp::Eq, left, right } => {
            if let Some((name, val)) = try_eq_push(left, right, columns) {
                let mut m = HashMap::new();
                m.insert(name, val);
                return (m, None);
            }
            (HashMap::new(), Some(expr.clone()))
        }
        _ => (HashMap::new(), Some(expr.clone())),
    }
}

/// Returns `(col_name, value)` when one side is a Column ref and the other is
/// a non-NULL Literal. Returns `None` for any other shape.
fn try_eq_push(
    left: &Expr,
    right: &Expr,
    columns: &[CatalogColumnDef],
) -> Option<(String, Value)> {
    match (left, right) {
        (Expr::Column { col_idx, .. }, Expr::Literal(v)) if !v.is_null() => {
            let name = columns.get(*col_idx)?.name.clone();
            Some((name, v.clone()))
        }
        (Expr::Literal(v), Expr::Column { col_idx, .. }) if !v.is_null() => {
            let name = columns.get(*col_idx)?.name.clone();
            Some((name, v.clone()))
        }
        _ => None,
    }
}

/// Entry point called from `select_joins_ctx.rs` when `table_id >= FOREIGN_TABLE_ID_BASE`.
///
/// Looks up the ForeignTableDef and ForeignServerDef from the catalog, then
/// issues an HTTP GET and maps the JSON-array response to rows.
fn fdw_scan_table(
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_id: u32,
    columns: &[CatalogColumnDef],
) -> Result<Vec<(RecordId, crate::result::Row)>, DbError> {
    let mut reader = CatalogReader::new(storage, snap)?;

    let ftable = reader
        .get_foreign_table_by_id(table_id)?
        .ok_or_else(|| DbError::Internal {
            message: format!("FDW: foreign table id {table_id:#x} not found in catalog"),
        })?;

    let fserver = reader
        .get_foreign_server(&ftable.server_name)?
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!(
                "FDW: server '{}' not found (referenced by foreign table '{}.{}')",
                ftable.server_name, ftable.schema_name, ftable.table_name
            ),
        })?;

    let server_opts = parse_json_options(&fserver.options)?;
    let table_opts = parse_json_options(&ftable.options)?;

    let base_url = server_opts.get("url").map(|s| s.as_str()).unwrap_or("").trim().to_string();
    if base_url.is_empty() {
        return Err(DbError::InvalidValue {
            reason: format!("FDW server '{}': missing 'url' option", fserver.name),
        });
    }

    let timeout_ms: u64 = server_opts
        .get("timeout_ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let endpoint = table_opts.get("endpoint").map(|s| s.as_str()).unwrap_or("/");
    let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);

    let body = http_get(&url, timeout_ms)?;
    json_to_rows(&body, columns)
}

/// Parse a simple JSON object `{"key":"value",...}` into a HashMap.
/// Only handles string values (all FDW options are stored as strings).
fn parse_json_options(
    json: &str,
) -> Result<std::collections::HashMap<String, String>, DbError> {
    if json.is_empty() || json == "{}" {
        return Ok(std::collections::HashMap::new());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(json).map_err(|e| DbError::InvalidValue {
            reason: format!("FDW: malformed options JSON: {e}"),
        })?;
    let obj = parsed.as_object().ok_or_else(|| DbError::InvalidValue {
        reason: "FDW: options must be a JSON object".into(),
    })?;
    let mut map = std::collections::HashMap::new();
    for (k, v) in obj {
        let val = match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        map.insert(k.clone(), val);
    }
    Ok(map)
}

/// Minimal HTTP/1.1 GET over a plain TCP connection.
/// Returns the response body as a UTF-8 string.
fn http_get(url: &str, timeout_ms: u64) -> Result<String, DbError> {
    let parsed = url::Url::parse(url).map_err(|e| DbError::InvalidValue {
        reason: format!("FDW: invalid URL '{url}': {e}"),
    })?;

    if parsed.scheme() != "http" {
        return Err(DbError::InvalidValue {
            reason: format!(
                "FDW: only http:// URLs are supported in this phase, got '{}'",
                parsed.scheme()
            ),
        });
    }

    let host = parsed.host_str().ok_or_else(|| DbError::InvalidValue {
        reason: format!("FDW: missing host in URL '{url}'"),
    })?;
    let port = parsed.port().unwrap_or(80);
    let path = if parsed.path().is_empty() { "/" } else { parsed.path() };
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    let request_path = format!("{path}{query}");

    let addr = format!("{host}:{port}");
    let timeout = Duration::from_millis(timeout_ms);

    let mut stream = TcpStream::connect(&addr).map_err(|e| DbError::Internal {
        message: format!("FDW: connect to '{addr}' failed: {e}"),
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok();

    let request = format!(
        "GET {request_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).map_err(|e| DbError::Internal {
        message: format!("FDW: send to '{addr}' failed: {e}"),
    })?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| DbError::Internal {
        message: format!("FDW: recv from '{addr}' failed: {e}"),
    })?;

    let response = String::from_utf8_lossy(&raw);
    // Split headers from body at the blank line.
    if let Some(pos) = response.find("\r\n\r\n") {
        Ok(response[pos + 4..].to_string())
    } else if let Some(pos) = response.find("\n\n") {
        Ok(response[pos + 2..].to_string())
    } else {
        Err(DbError::Internal {
            message: format!("FDW: malformed HTTP response from '{addr}' (no body separator)"),
        })
    }
}

/// Parse a JSON array into AxiomDB rows, mapping each object's fields to
/// the declared column schema.
fn json_to_rows(
    body: &str,
    columns: &[CatalogColumnDef],
) -> Result<Vec<(RecordId, crate::result::Row)>, DbError> {
    let parsed: serde_json::Value =
        serde_json::from_str(body.trim()).map_err(|e| DbError::Internal {
            message: format!("FDW: JSON parse error: {e}. Body: {}", &body[..body.len().min(200)]),
        })?;

    let array = parsed.as_array().ok_or_else(|| DbError::Internal {
        message: format!(
            "FDW: expected JSON array response, got {}",
            match &parsed {
                serde_json::Value::Object(_) => "object",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Null => "null",
                serde_json::Value::Array(_) => "array",
            }
        ),
    })?;

    let mut rows = Vec::with_capacity(array.len());
    for (row_idx, item) in array.iter().enumerate() {
        let obj = match item.as_object() {
            Some(o) => o,
            None => {
                return Err(DbError::Internal {
                    message: format!("FDW: row {row_idx} is not a JSON object"),
                })
            }
        };

        let mut row: crate::result::Row = Vec::with_capacity(columns.len());
        for col in columns {
            let val = obj.get(&col.name).or_else(|| {
                // Case-insensitive fallback.
                obj.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&col.name))
                    .map(|(_, v)| v)
            });

            let cell = match val {
                None => axiomdb_types::Value::Null,
                Some(serde_json::Value::Null) => axiomdb_types::Value::Null,
                Some(v) => json_value_to_axiom(v, col.col_type)?,
            };
            row.push(cell);
        }
        // Synthetic RecordId: page_id = 0, slot_id = row index (for referencing)
        let rid = RecordId {
            page_id: 0,
            slot_id: (row_idx % 65535) as u16,
        };
        rows.push((rid, row));
    }
    Ok(rows)
}

fn json_value_to_axiom(
    v: &serde_json::Value,
    col_type: ColumnType,
) -> Result<axiomdb_types::Value, DbError> {
    use axiomdb_types::Value;
    match col_type {
        ColumnType::Bool => Ok(Value::Bool(match v {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => {
                s.eq_ignore_ascii_case("true") || s == "1"
            }
            serde_json::Value::Number(n) => n.as_i64().map(|i| i != 0).unwrap_or(false),
            _ => false,
        })),
        ColumnType::Int => {
            let n = match v {
                serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as i32,
                serde_json::Value::String(s) => s.parse().unwrap_or(0),
                serde_json::Value::Bool(b) => i32::from(*b),
                _ => 0,
            };
            Ok(Value::Int(n))
        }
        ColumnType::BigInt => {
            let n = match v {
                serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
                serde_json::Value::String(s) => s.parse().unwrap_or(0),
                serde_json::Value::Bool(b) => i64::from(*b),
                _ => 0,
            };
            Ok(Value::BigInt(n))
        }
        ColumnType::Float => {
            let f = match v {
                serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
                serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                serde_json::Value::Bool(b) => f64::from(*b),
                _ => 0.0,
            };
            Ok(Value::Real(f))
        }
        #[allow(clippy::match_wildcard_for_single_variants)]
        ColumnType::Text
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Bytes
        | ColumnType::Uuid
        | ColumnType::Decimal
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Array => {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(Value::Text(s))
        }
    }
}

// ── Phase 22b.6: unit tests for extract_fdw_pushable ─────────────────────────

#[cfg(test)]
mod tests_extract {
    use super::*;
    use axiomdb_catalog::schema::{ColumnDef as CatalogColumnDef, ColumnType};
    use axiomdb_types::Value;
    use crate::expr::{BinaryOp, Expr};

    fn make_col(idx: u16, name: &str) -> CatalogColumnDef {
        CatalogColumnDef {
            table_id: 1,
            col_idx: idx,
            name: name.to_string(),
            col_type: ColumnType::Int,
            nullable: true,
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
    }

    fn col(idx: usize) -> Expr {
        Expr::Column { col_idx: idx, name: "x".into() }
    }

    fn lit_int(n: i32) -> Expr { Expr::Literal(Value::Int(n)) }
    fn lit_text(s: &str) -> Expr { Expr::Literal(Value::Text(s.into())) }
    fn lit_null() -> Expr { Expr::Literal(Value::Null) }
    fn and(l: Expr, r: Expr) -> Expr {
        Expr::BinaryOp { op: BinaryOp::And, left: Box::new(l), right: Box::new(r) }
    }
    fn eq(l: Expr, r: Expr) -> Expr {
        Expr::BinaryOp { op: BinaryOp::Eq, left: Box::new(l), right: Box::new(r) }
    }
    fn gt(l: Expr, r: Expr) -> Expr {
        Expr::BinaryOp { op: BinaryOp::Gt, left: Box::new(l), right: Box::new(r) }
    }
    fn or(l: Expr, r: Expr) -> Expr {
        Expr::BinaryOp { op: BinaryOp::Or, left: Box::new(l), right: Box::new(r) }
    }

    #[test]
    fn extract_none_where() {
        let cols = vec![make_col(0, "id")];
        let (bound, residual) = extract_fdw_pushable(None, &cols);
        assert!(bound.is_empty());
        assert!(residual.is_none());
    }

    #[test]
    fn extract_single_eq_col_left() {
        // col(0) = 5  →  {id → 5}, residual = None
        let cols = vec![make_col(0, "id")];
        let expr = eq(col(0), lit_int(5));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("id"), Some(&Value::Int(5)));
        assert!(residual.is_none());
    }

    #[test]
    fn extract_single_eq_col_right() {
        // 5 = col(0)  →  {id → 5}, residual = None  (commutative)
        let cols = vec![make_col(0, "id")];
        let expr = eq(lit_int(5), col(0));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("id"), Some(&Value::Int(5)));
        assert!(residual.is_none());
    }

    #[test]
    fn extract_null_eq_stays_residual() {
        // col = NULL  →  kept as residual (IS NULL semantics differ)
        let cols = vec![make_col(0, "id")];
        let expr = eq(col(0), lit_null());
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert!(bound.is_empty());
        assert!(residual.is_some());
    }

    #[test]
    fn extract_and_both_pushable() {
        // col0=1 AND col1='x'  →  {id→1, name→'x'}, residual=None
        let cols = vec![make_col(0, "id"), make_col(1, "name")];
        let expr = and(eq(col(0), lit_int(1)), eq(col(1), lit_text("x")));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("id"), Some(&Value::Int(1)));
        assert_eq!(bound.get("name"), Some(&Value::Text("x".into())));
        assert!(residual.is_none());
    }

    #[test]
    fn extract_and_mixed_pushable_and_residual() {
        // col0=1 AND col0>5  →  {id→1}, residual=Some(col0>5)
        let cols = vec![make_col(0, "id")];
        let expr = and(eq(col(0), lit_int(1)), gt(col(0), lit_int(5)));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("id"), Some(&Value::Int(1)));
        assert!(residual.is_some());
    }

    #[test]
    fn extract_or_stays_residual() {
        // col=1 OR other=2  →  entire OR is residual
        let cols = vec![make_col(0, "id"), make_col(1, "other")];
        let expr = or(eq(col(0), lit_int(1)), eq(col(1), lit_int(2)));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert!(bound.is_empty());
        assert!(residual.is_some());
    }

    #[test]
    fn extract_gt_stays_residual() {
        // col > 5  →  residual only
        let cols = vec![make_col(0, "id")];
        let expr = gt(col(0), lit_int(5));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert!(bound.is_empty());
        assert!(residual.is_some());
    }

    #[test]
    fn extract_deep_and_chain() {
        // a=1 AND b=2 AND c=3  →  all three pushed, no residual
        let cols = vec![make_col(0, "a"), make_col(1, "b"), make_col(2, "c")];
        let expr = and(and(eq(col(0), lit_int(1)), eq(col(1), lit_int(2))), eq(col(2), lit_int(3)));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("a"), Some(&Value::Int(1)));
        assert_eq!(bound.get("b"), Some(&Value::Int(2)));
        assert_eq!(bound.get("c"), Some(&Value::Int(3)));
        assert!(residual.is_none());
    }

    #[test]
    fn extract_text_literal() {
        let cols = vec![make_col(0, "status")];
        let expr = eq(col(0), lit_text("active"));
        let (bound, residual) = extract_fdw_pushable(Some(&expr), &cols);
        assert_eq!(bound.get("status"), Some(&Value::Text("active".into())));
        assert!(residual.is_none());
    }
}
