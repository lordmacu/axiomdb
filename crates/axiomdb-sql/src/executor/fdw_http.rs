// ── HTTP Foreign Data Wrapper scan (Phase 22b.2) ───────────────────────────────
//
// Executes a GET request against a foreign HTTP server and maps the JSON-array
// response to AxiomDB rows. Only HTTP (not HTTPS) is supported in this phase.
// HTTPS support is deferred to a later phase.
//
// ## FDW options
//
// ### Server OPTIONS (CREATE SERVER ... OPTIONS):
//   url        - Base URL of the remote service (required). Must start with
//                "http://". Example: 'http://api.example.com'
//   timeout_ms - Request timeout in milliseconds (optional, default 10000).
//
// ### Table OPTIONS (CREATE FOREIGN TABLE ... OPTIONS):
//   endpoint   - Path appended to the server URL (optional, default '/').
//                Example: '/users'
//   method     - HTTP method (optional, default 'GET'). Only GET is supported.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

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
