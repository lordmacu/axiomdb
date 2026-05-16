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

/// Entry point called from `select_ctx.rs` / `select_joins_ctx.rs` when
/// `table_id >= FOREIGN_TABLE_ID_BASE`.
///
/// Looks up the ForeignTableDef and ForeignServerDef from the catalog, applies
/// predicate and LIMIT pushdown via `render_fdw_url`, issues an HTTP GET, and
/// maps the JSON-array response to rows.
///
/// `pushed_predicates` — equality predicates already extracted from the WHERE
///   clause by `extract_fdw_pushable`; forwarded to the remote via URL.
/// `limit` — the LIMIT value if it was a plain integer literal; forwarded via
///   `limit_param` option if configured.
fn fdw_scan_table(
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_id: u32,
    columns: &[CatalogColumnDef],
    pushed_predicates: &HashMap<String, Value>,
    limit: Option<u64>,
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

    // Phase 22b.6: parse pushdown options.
    let pushdown_raw = table_opts.get("pushdown_cols").cloned().unwrap_or_default();
    let pushdown_cols: Vec<&str> = if pushdown_raw.is_empty() {
        vec![]
    } else {
        pushdown_raw.split(',').map(str::trim).collect()
    };
    let limit_param = table_opts.get("limit_param").map(|s| s.as_str());

    let url = render_fdw_url(
        &base_url,
        endpoint,
        pushed_predicates,
        &pushdown_cols,
        limit_param,
        limit,
    );

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
        ColumnType::TinyInt | ColumnType::SmallInt | ColumnType::Int => {
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
        | ColumnType::Array
        | ColumnType::Range
        | ColumnType::Money
        | ColumnType::Composite
        | ColumnType::Ltree
        | ColumnType::Xml => {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(Value::Text(s))
        }
    }
}

// ── Phase 22b.6: URL rendering ───────────────────────────────────────────────

/// Constructs the final HTTP URL for an FDW GET request with predicate/LIMIT pushdown.
///
/// Processing order:
///   1. Substitute `{col_name}` placeholders in `endpoint` from `bound`.
///      Unbound placeholders remain as-is; consumed cols are tracked.
///   2. For each col in `pushdown_cols` that is in `bound` AND NOT consumed by
///      a placeholder, append `?col=percent_encoded_value`.
///   3. If `limit_param` and `limit` are both set, append the limit param.
fn render_fdw_url(
    base_url: &str,
    endpoint: &str,
    bound: &HashMap<String, Value>,
    pushdown_cols: &[&str],
    limit_param: Option<&str>,
    limit: Option<u64>,
) -> String {
    let mut consumed: HashSet<String> = HashSet::new();
    let rendered_endpoint = substitute_placeholders(endpoint, bound, &mut consumed);

    let base = format!("{}{}", base_url.trim_end_matches('/'), rendered_endpoint);

    let mut params: Vec<(String, String)> = Vec::new();
    for &col in pushdown_cols {
        if !consumed.contains(col) {
            if let Some(val) = bound.get(col) {
                params.push((col.to_string(), value_to_url_string(val)));
            }
        }
    }
    if let (Some(param), Some(n)) = (limit_param, limit) {
        params.push((param.to_string(), n.to_string()));
    }

    if params.is_empty() {
        return base;
    }
    let already_has_query = base.contains('?');
    let mut out = base;
    for (i, (k, v)) in params.iter().enumerate() {
        let sep = if i == 0 && !already_has_query { '?' } else { '&' };
        out.push(sep);
        out.push_str(k);
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    out
}

/// Substitutes `{col_name}` placeholders in `endpoint`.
/// Records the name of each column that was successfully substituted in `consumed`.
fn substitute_placeholders(
    endpoint: &str,
    bound: &HashMap<String, Value>,
    consumed: &mut HashSet<String>,
) -> String {
    let mut result = String::with_capacity(endpoint.len() + 32);
    let bytes = endpoint.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find the closing '}'
            if let Some(close) = bytes[i + 1..].iter().position(|&b| b == b'}') {
                let col_name = &endpoint[i + 1..i + 1 + close];
                if let Some(val) = bound.get(col_name) {
                    let s = value_to_url_string(val);
                    result.push_str(&percent_encode(&s));
                    consumed.insert(col_name.to_string());
                } else {
                    // Unbound — pass through literally
                    result.push('{');
                    result.push_str(col_name);
                    result.push('}');
                }
                i += 1 + close + 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Percent-encodes characters that have special meaning in URLs.
/// Space→%20, &→%26, =→%3D, +→%2B, #→%23, %→%25.
/// Non-ASCII bytes are percent-encoded byte-by-byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b' ' => out.push_str("%20"),
            b'&' => out.push_str("%26"),
            b'=' => out.push_str("%3D"),
            b'+' => out.push_str("%2B"),
            b'#' => out.push_str("%23"),
            b'%' => out.push_str("%25"),
            b if b.is_ascii() => out.push(b as char),
            b => { out.push('%'); out.push_str(&format!("{b:02X}")); }
        }
    }
    out
}

/// Converts an AxiomDB Value to its string representation for embedding in a URL.
fn value_to_url_string(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => {
            if f.is_nan() { return "NaN".into(); }
            if f.is_infinite() {
                return if *f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
            }
            format!("{f}")
        }
        Value::Decimal(m, s) => format!("{m}e-{s}"),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Bytes(b) => format!("{b:?}"),
        Value::Date(d) => d.to_string(),
        Value::Timestamp(ts) => ts.to_string(),
        Value::Uuid(u) => {
            let h = u.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("");
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
        }
        Value::Jsonb(b) => format!("{b:?}"),
        Value::Array(items) => {
            let inner = items.iter().map(value_to_url_string).collect::<Vec<_>>().join(",");
            format!("[{inner}]")
        }
        Value::Range(rv) => rv.to_display_string(),
        Value::Money(m, s, c) => Value::Money(*m, *s, *c).to_string(),
        Value::Composite(fields) => Value::Composite(fields.clone()).to_string(),
        Value::Ltree(s) | Value::Xml(s) => s.clone(),
        Value::Null => String::new(),
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

// ── Phase 22b.6: unit tests for render_fdw_url ───────────────────────────────

#[cfg(test)]
mod tests_render {
    use super::*;
    use axiomdb_types::Value;

    fn bound(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn render_no_pushdown() {
        // No placeholders, no pushdown_cols, no limit → URL unchanged
        let url = render_fdw_url("http://api.example.com", "/users", &bound(&[]), &[], None, None);
        assert_eq!(url, "http://api.example.com/users");
    }

    #[test]
    fn render_path_placeholder_int() {
        // {id} in endpoint, bound id→5 → /users/5
        let b = bound(&[("id", Value::Int(5))]);
        let url = render_fdw_url("http://api.example.com", "/users/{id}", &b, &[], None, None);
        assert_eq!(url, "http://api.example.com/users/5");
    }

    #[test]
    fn render_path_placeholder_text() {
        // {cat} in endpoint, bound cat→'shoes' → /products/shoes
        let b = bound(&[("cat", Value::Text("shoes".into()))]);
        let url = render_fdw_url("http://api.example.com", "/products/{cat}", &b, &[], None, None);
        assert_eq!(url, "http://api.example.com/products/shoes");
    }

    #[test]
    fn render_unbound_placeholder_left_as_literal() {
        // {id} in endpoint, no id in bound → /users/{id} (literal)
        let url = render_fdw_url("http://api.example.com", "/users/{id}", &bound(&[]), &[], None, None);
        assert_eq!(url, "http://api.example.com/users/{id}");
    }

    #[test]
    fn render_query_param_pushdown() {
        // pushdown_cols ['status'], bound status→'active' → ?status=active
        let b = bound(&[("status", Value::Text("active".into()))]);
        let url = render_fdw_url("http://api.example.com", "/orders", &b, &["status"], None, None);
        assert_eq!(url, "http://api.example.com/orders?status=active");
    }

    #[test]
    fn render_limit_param() {
        // limit_param 'per_page', limit=10 → ?per_page=10
        let url = render_fdw_url(
            "http://api.example.com", "/items", &bound(&[]),
            &[], Some("per_page"), Some(10),
        );
        assert_eq!(url, "http://api.example.com/items?per_page=10");
    }

    #[test]
    fn render_mixed_path_and_query() {
        // {cat} in path, pushdown_cols ['brand'], limit_param 'limit', limit=5
        let b = bound(&[
            ("cat", Value::Text("shoes".into())),
            ("brand", Value::Text("nike".into())),
        ]);
        let url = render_fdw_url(
            "http://api.example.com", "/products/{cat}",
            &b, &["brand"], Some("limit"), Some(5),
        );
        assert_eq!(url, "http://api.example.com/products/shoes?brand=nike&limit=5");
    }

    #[test]
    fn render_placeholder_not_duplicated_in_query() {
        // {id} in path AND in pushdown_cols → only substituted in path, NOT appended as param
        let b = bound(&[("id", Value::Int(7))]);
        let url = render_fdw_url(
            "http://api.example.com", "/users/{id}",
            &b, &["id"], None, None,
        );
        assert_eq!(url, "http://api.example.com/users/7");
        assert!(!url.contains("id="), "id should not appear as query param");
    }

    #[test]
    fn render_percent_encode_spaces() {
        // Text with space → %20
        let b = bound(&[("q", Value::Text("hello world".into()))]);
        let url = render_fdw_url("http://api.example.com", "/search", &b, &["q"], None, None);
        assert_eq!(url, "http://api.example.com/search?q=hello%20world");
    }

    #[test]
    fn render_percent_encode_ampersand() {
        // Text with '&' → %26
        let b = bound(&[("q", Value::Text("a&b".into()))]);
        let url = render_fdw_url("http://api.example.com", "/search", &b, &["q"], None, None);
        assert_eq!(url, "http://api.example.com/search?q=a%26b");
    }

    #[test]
    fn render_existing_query_string_appends_with_ampersand() {
        // endpoint already has ?v=1 → new params appended with &
        let b = bound(&[("status", Value::Text("ok".into()))]);
        let url = render_fdw_url(
            "http://api.example.com", "/items?v=1",
            &b, &["status"], None, None,
        );
        assert_eq!(url, "http://api.example.com/items?v=1&status=ok");
    }

    #[test]
    fn render_real_nan_infinity() {
        let b_nan = bound(&[("x", Value::Real(f64::NAN))]);
        let url_nan = render_fdw_url("http://api.example.com", "/f", &b_nan, &["x"], None, None);
        assert!(url_nan.contains("NaN"));

        let b_inf = bound(&[("x", Value::Real(f64::INFINITY))]);
        let url_inf = render_fdw_url("http://api.example.com", "/f", &b_inf, &["x"], None, None);
        assert!(url_inf.contains("Infinity"));
    }

    #[test]
    fn render_bool_values() {
        let b = bound(&[("active", Value::Bool(true))]);
        let url = render_fdw_url("http://api.example.com", "/u", &b, &["active"], None, None);
        assert_eq!(url, "http://api.example.com/u?active=true");
    }

    #[test]
    fn render_limit_zero() {
        let url = render_fdw_url(
            "http://api.example.com", "/items", &bound(&[]),
            &[], Some("limit"), Some(0),
        );
        assert_eq!(url, "http://api.example.com/items?limit=0");
    }
}
