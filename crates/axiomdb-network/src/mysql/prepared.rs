//! Prepared statement protocol support.
//!
//! Handles:
//! - Building `COM_STMT_PREPARE` responses (stmt_id + column metadata)
//! - Parsing `COM_STMT_EXECUTE` payloads (binary-encoded parameters)
//! - Decoding MySQL binary parameter values into `axiomdb_types::Value`
//! - Substituting decoded parameters into SQL `?` placeholders

use axiomdb_core::error::DbError;
use axiomdb_sql::{
    ast::{SelectItem, SelectStmt, Stmt},
    expr::Expr,
    result::ColumnMeta,
};
use axiomdb_types::Value;

use super::{
    charset::{self, CharsetDef, CollationDef},
    packets::{build_eof_packet, write_lenenc_int, write_lenenc_str},
    result::build_column_def_pub,
    session::PreparedStatement,
};

// ── PREPARE response ──────────────────────────────────────────────────────────

/// Builds the full packet sequence for a `COM_STMT_PREPARE` response.
///
/// Sequence:
/// - seq=1: Statement OK (stmt_id, num_cols, num_params)
/// - seq=2..N: parameter column defs (stubs) + EOF, if num_params > 0
/// - seq=N+1..: result column defs + EOF, if num_cols > 0
///
/// `results_collation` is used for result column definitions and parameter stub
/// column definitions, so the client receives the correct charset id.
pub fn build_prepare_response(
    stmt_id: u32,
    num_params: u16,
    result_cols: &[ColumnMeta],
    seq_start: u8,
    results_collation: &'static CollationDef,
) -> Vec<(u8, Vec<u8>)> {
    let mut packets: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut seq = seq_start;

    // Statement OK packet
    let mut ok = Vec::with_capacity(12);
    ok.push(0x00); // status = OK
    ok.extend_from_slice(&stmt_id.to_le_bytes());
    ok.extend_from_slice(&(result_cols.len() as u16).to_le_bytes()); // num_cols
    ok.extend_from_slice(&num_params.to_le_bytes());
    ok.push(0x00); // reserved
    ok.extend_from_slice(&0u16.to_le_bytes()); // warning_count
    packets.push((seq, ok));
    seq += 1;

    // Parameter column defs (stubs — type VAR_STRING)
    for _ in 0..num_params {
        packets.push((seq, build_stub_column_def("?", results_collation)));
        seq += 1;
    }
    if num_params > 0 {
        packets.push((seq, build_eof_packet()));
        seq += 1;
    }

    // Result column defs
    for col in result_cols {
        packets.push((seq, build_column_def_pub(col)));
        seq += 1;
    }
    if !result_cols.is_empty() {
        packets.push((seq, build_eof_packet()));
    }

    packets
}

fn build_stub_column_def(name: &str, results_collation: &'static CollationDef) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    write_lenenc_str(&mut buf, b"def");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, name.as_bytes());
    write_lenenc_str(&mut buf, name.as_bytes());
    write_lenenc_int(&mut buf, 0x0c);
    buf.extend_from_slice(&results_collation.id.to_le_bytes()); // charset = results_collation
    buf.extend_from_slice(&255u32.to_le_bytes()); // column_length
    buf.push(0xfd); // type = VAR_STRING
    buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    buf.push(0u8); // decimals
    buf.extend_from_slice(&0u16.to_le_bytes()); // filler
    buf
}

// ── EXECUTE payload parsing ───────────────────────────────────────────────────

/// Parsed COM_STMT_EXECUTE payload.
pub struct ExecutePacket {
    pub stmt_id: u32,
    pub params: Vec<Value>,
}

/// Parses a `COM_STMT_EXECUTE` payload (after the 0x17 command byte).
///
/// `client_charset` is used to decode string-like binary parameters.
/// Pass `conn_state.client_charset()` from the connection handler.
///
/// Updates `stmt.param_types` if the client sends a new type list
/// (`new_params_bound_flag = 1`).
pub fn parse_execute_packet(
    payload: &[u8],
    stmt: &mut PreparedStatement,
    client_charset: &'static CharsetDef,
) -> Result<ExecutePacket, DbError> {
    if payload.len() < 9 {
        return Err(DbError::ParseError {
            message: "COM_STMT_EXECUTE payload too short".into(),
            position: None,
        });
    }

    let stmt_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    // payload[4] = flags (cursor type, ignored)
    // payload[5..9] = iteration_count (always 1, ignored)
    let mut pos = 9usize;

    let n = stmt.param_count as usize;
    if n == 0 {
        return Ok(ExecutePacket {
            stmt_id,
            params: vec![],
        });
    }

    // Null bitmap: ceil(n/8) bytes
    let bitmap_len = n.div_ceil(8);
    if pos + bitmap_len > payload.len() {
        return Err(DbError::ParseError {
            message: "null bitmap truncated in COM_STMT_EXECUTE".into(),
            position: None,
        });
    }
    let null_bitmap = payload[pos..pos + bitmap_len].to_vec();
    pos += bitmap_len;

    // new_params_bound_flag
    if pos >= payload.len() {
        return Err(DbError::ParseError {
            message: "missing new_params_bound_flag".into(),
            position: None,
        });
    }
    let bound = payload[pos] == 1;
    pos += 1;

    // Read type list if provided
    if bound {
        if pos + n * 2 > payload.len() {
            return Err(DbError::ParseError {
                message: "param type list truncated".into(),
                position: None,
            });
        }
        stmt.param_types = (0..n)
            .map(|i| u16::from_le_bytes([payload[pos + i * 2], payload[pos + i * 2 + 1]]))
            .collect();
        pos += n * 2;
    }

    // Decode values
    let mut params = Vec::with_capacity(n);
    for i in 0..n {
        if is_null(&null_bitmap, i) {
            params.push(Value::Null);
            continue;
        }
        let type_code = stmt.param_types.get(i).copied().unwrap_or(0xfd);
        let (value, consumed) = decode_binary_value(&payload[pos..], type_code, client_charset)
            .map_err(|e| DbError::ParseError {
                message: format!("param {i}: {e}"),
                position: None,
            })?;
        params.push(value);
        pos += consumed;
    }

    Ok(ExecutePacket { stmt_id, params })
}

fn is_null(bitmap: &[u8], idx: usize) -> bool {
    let byte = idx / 8;
    let bit = idx % 8;
    byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1
}

/// Decodes one binary-encoded parameter value.
/// Returns `(value, bytes_consumed)`.
fn decode_binary_value(
    buf: &[u8],
    type_code: u16,
    client_charset: &'static CharsetDef,
) -> Result<(Value, usize), DbError> {
    let type_base = (type_code & 0x00FF) as u8;
    let unsigned = (type_code >> 8) & 0x80 != 0; // unsigned flag in high byte

    let trunc = |what: &str| DbError::ParseError {
        message: format!("{what} truncated in COM_STMT_EXECUTE param"),
        position: None,
    };

    match type_base {
        0x01 => {
            // TINY (u8 or i8) — Python bool True/False comes as TINY(1)/TINY(0)
            if buf.is_empty() {
                return Err(trunc("TINY"));
            }
            let n = buf[0];
            let v = if unsigned {
                Value::Int(n as i32)
            } else {
                Value::Int(n as i8 as i32)
            };
            Ok((v, 1))
        }
        0x02 => {
            // SHORT
            if buf.len() < 2 {
                return Err(trunc("SHORT"));
            }
            let raw = i16::from_le_bytes([buf[0], buf[1]]);
            Ok((Value::Int(raw as i32), 2))
        }
        0x03 | 0x09 => {
            // LONG / INT24
            if buf.len() < 4 {
                return Err(trunc("LONG"));
            }
            Ok((
                Value::Int(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
                4,
            ))
        }
        0x08 => {
            // LONGLONG
            if buf.len() < 8 {
                return Err(trunc("LONGLONG"));
            }
            Ok((
                Value::BigInt(i64::from_le_bytes(buf[..8].try_into().unwrap())),
                8,
            ))
        }
        0x04 => {
            // FLOAT
            if buf.len() < 4 {
                return Err(trunc("FLOAT"));
            }
            let f = f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            Ok((Value::Real(f as f64), 4))
        }
        0x05 => {
            // DOUBLE
            if buf.len() < 8 {
                return Err(trunc("DOUBLE"));
            }
            Ok((
                Value::Real(f64::from_le_bytes(buf[..8].try_into().unwrap())),
                8,
            ))
        }
        0x06 => {
            // NULL (should be in null bitmap, but handle defensively)
            Ok((Value::Null, 0))
        }
        0x0a => {
            // DATE: [len][year u16][month u8][day u8]
            if buf.is_empty() {
                return Err(trunc("DATE"));
            }
            let len = buf[0] as usize;
            if buf.len() < 1 + len {
                return Err(trunc("DATE data"));
            }
            let data = &buf[1..1 + len];
            let val = if len >= 4 {
                let year = u16::from_le_bytes([data[0], data[1]]) as i32;
                let month = data[2] as i32;
                let day = data[3] as i32;
                Value::Date(ymd_to_days(year, month, day))
            } else {
                Value::Null
            };
            Ok((val, 1 + len))
        }
        0x07 | 0x0c => {
            // TIMESTAMP / DATETIME: [len][year u16][month][day][hour][min][sec][microsec u32]
            if buf.is_empty() {
                return Err(trunc("DATETIME"));
            }
            let len = buf[0] as usize;
            if buf.len() < 1 + len {
                return Err(trunc("DATETIME data"));
            }
            let data = &buf[1..1 + len];
            let val = if len >= 4 {
                let year = u16::from_le_bytes([data[0], data[1]]) as i64;
                let month = data[2] as i64;
                let day = data[3] as i64;
                let hour = if len > 4 { data[4] as i64 } else { 0 };
                let minute = if len > 5 { data[5] as i64 } else { 0 };
                let second = if len > 6 { data[6] as i64 } else { 0 };
                let days = ymd_to_days(year as i32, month as i32, day as i32) as i64;
                let secs = days * 86400 + hour * 3600 + minute * 60 + second;
                Value::Timestamp(secs * 1_000_000)
            } else {
                Value::Null
            };
            Ok((val, 1 + len))
        }
        0x00 | 0xf6 => {
            // DECIMAL / NEWDECIMAL — lenenc string (always ASCII digits, charset irrelevant)
            let (s, consumed) = read_lenenc_str(buf, client_charset)?;
            Ok((Value::Text(s), consumed))
        }
        // All string/blob types: lenenc-prefixed bytes decoded with client charset
        0x0f | 0xfc | 0xfd | 0xfe | 0xf5 | 0x10 | 0xf3 | 0xf4 => {
            let (s, consumed) = read_lenenc_str(buf, client_charset)?;
            Ok((Value::Text(s), consumed))
        }
        _ => {
            // Unknown type — read as lenenc string with client charset
            let (s, consumed) = read_lenenc_str(buf, client_charset)?;
            Ok((Value::Text(s), consumed))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_lenenc_int(buf: &[u8]) -> Result<(u64, usize), &'static str> {
    if buf.is_empty() {
        return Err("lenenc int truncated");
    }
    match buf[0] {
        0..=250 => Ok((buf[0] as u64, 1)),
        0xfc => {
            if buf.len() < 3 {
                return Err("lenenc 2b truncated");
            }
            Ok((u16::from_le_bytes([buf[1], buf[2]]) as u64, 3))
        }
        0xfd => {
            if buf.len() < 4 {
                return Err("lenenc 3b truncated");
            }
            Ok((u32::from_le_bytes([buf[1], buf[2], buf[3], 0]) as u64, 4))
        }
        0xfe => {
            if buf.len() < 9 {
                return Err("lenenc 8b truncated");
            }
            Ok((u64::from_le_bytes(buf[1..9].try_into().unwrap()), 9))
        }
        _ => Err("invalid lenenc byte (0xfb = NULL marker, not valid here)"),
    }
}

fn read_lenenc_str(
    buf: &[u8],
    client_charset: &'static CharsetDef,
) -> Result<(String, usize), DbError> {
    let (len, llen) = read_lenenc_int(buf).map_err(|msg| DbError::ParseError {
        message: format!("lenenc string: {msg}"),
        position: None,
    })?;
    let len = len as usize;
    if buf.len() < llen + len {
        return Err(DbError::ParseError {
            message: "lenenc string data truncated in COM_STMT_EXECUTE".into(),
            position: None,
        });
    }
    let s =
        charset::decode_text(client_charset, &buf[llen..llen + len]).map(|cow| cow.into_owned())?;
    Ok((s, llen + len))
}

/// Converts year/month/day to days since Unix epoch (1970-01-01 = 0).
///
/// Inverse of `days_to_ymd` in `result.rs`.
/// Uses Howard Hinnant's civil calendar algorithm.
fn ymd_to_days(year: i32, month: i32, day: i32) -> i32 {
    // Shift Jan/Feb to end of previous year
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    // Era (400-year period)
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32; // year-of-era [0, 399]
    let doy = (153 * m as u32 + 2) / 5 + day as u32 - 1; // day-of-year [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day-of-era [0, 146096]
                                                     // Adjust: era*146097 + doe gives Julian Day; subtract 719468 for Unix epoch
    era * 146097 + doe as i32 - 719468
}

// ── SQL Substitution ──────────────────────────────────────────────────────────

/// Replaces `?` placeholders with SQL literals.
///
/// Safe from SQL injection: strings are single-quote escaped using `value_to_sql_literal`.
/// `?` inside single-quoted string literals in the template are not replaced.
pub fn substitute_params(template: &str, params: &[Value]) -> Result<String, DbError> {
    let mut result = String::with_capacity(template.len() + params.len() * 8);
    let mut param_idx = 0usize;
    let mut in_string = false;
    let bytes = template.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            b'\'' => {
                in_string = !in_string;
                result.push('\'');
            }
            b'?' if !in_string => {
                if param_idx >= params.len() {
                    return Err(DbError::ParseError {
                        message: format!(
                            "prepared statement has {} placeholders but only {} params provided",
                            count_question_marks(template),
                            params.len()
                        ),
                        position: None,
                    });
                }
                result.push_str(&value_to_sql_literal(&params[param_idx]));
                param_idx += 1;
            }
            _ => result.push(ch as char),
        }
        i += 1;
    }
    Ok(result)
}

fn count_question_marks(sql: &str) -> usize {
    let mut count = 0;
    let mut in_string = false;
    for ch in sql.chars() {
        match ch {
            '\'' => in_string = !in_string,
            '?' if !in_string => count += 1,
            _ => {}
        }
    }
    count
}

/// Converts a `Value` to a SQL literal string safe for embedding in SQL.
pub fn value_to_sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            // Use TRUE/FALSE so the parser produces Value::Bool, avoiding
            // Int→Bool strict coercion failure on BOOL columns.
            if *b {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("x'{hex}'")
        }
        Value::Decimal(m, s) => super::result::format_decimal_pub(*m, *s),
        Value::Date(d) => format!("'{}'", super::result::format_date_pub(*d)),
        Value::Timestamp(t) => format!("'{}'", super::result::format_timestamp_pub(*t)),
        Value::Uuid(u) => {
            format!(
                "'{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}'",
                u[0],u[1],u[2],u[3],u[4],u[5],u[6],u[7],
                u[8],u[9],u[10],u[11],u[12],u[13],u[14],u[15]
            )
        }
    }
}

// ── AST parameter substitution ────────────────────────────────────────────────

/// Replaces every `Expr::Param { idx }` in `stmt` with `Expr::Literal(params[idx])`.
///
/// Called on each `COM_STMT_EXECUTE` using a **cached** analyzed statement.
/// This is a simple tree-walk (~1µs) — orders of magnitude faster than
/// re-running `parse()` + `analyze()` (~5ms combined).
pub fn substitute_params_in_ast(stmt: Stmt, params: &[Value]) -> Result<Stmt, DbError> {
    match stmt {
        Stmt::Select(s) => Ok(Stmt::Select(subst_select(s, params)?)),
        Stmt::Insert(mut s) => {
            use axiomdb_sql::ast::InsertSource;
            s.source = match s.source {
                InsertSource::Values(rows) => InsertSource::Values(
                    rows.into_iter()
                        .map(|row| {
                            row.into_iter()
                                .map(|e| subst_expr_param(e, params))
                                .collect()
                        })
                        .collect(),
                ),
                InsertSource::Select(sel) => {
                    InsertSource::Select(Box::new(subst_select(*sel, params)?))
                }
                other => other,
            };
            Ok(Stmt::Insert(s))
        }
        Stmt::Update(mut s) => {
            s.where_clause = s.where_clause.map(|e| subst_expr_param(e, params));
            s.assignments = s
                .assignments
                .into_iter()
                .map(|mut a| {
                    a.value = subst_expr_param(a.value, params);
                    a
                })
                .collect();
            Ok(Stmt::Update(s))
        }
        Stmt::Delete(mut s) => {
            s.where_clause = s.where_clause.map(|e| subst_expr_param(e, params));
            Ok(Stmt::Delete(s))
        }
        other => Ok(other), // DDL and control statements have no params
    }
}

fn subst_select(mut s: SelectStmt, params: &[Value]) -> Result<SelectStmt, DbError> {
    s.where_clause = s.where_clause.map(|e| subst_expr_param(e, params));
    s.having = s.having.map(|e| subst_expr_param(e, params));
    s.columns = s
        .columns
        .into_iter()
        .map(|item| match item {
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: subst_expr_param(expr, params),
                alias,
            },
            other => other,
        })
        .collect();
    s.group_by = s
        .group_by
        .into_iter()
        .map(|e| subst_expr_param(e, params))
        .collect();
    s.order_by = s
        .order_by
        .into_iter()
        .map(|mut item| {
            item.expr = subst_expr_param(item.expr, params);
            item
        })
        .collect();
    s.limit = s.limit.map(|e| subst_expr_param(e, params));
    s.offset = s.offset.map(|e| subst_expr_param(e, params));
    Ok(s)
}

/// Recursively replaces `Expr::Param { idx }` with `Expr::Literal(params[idx])`.
fn subst_expr_param(expr: Expr, params: &[Value]) -> Expr {
    match expr {
        Expr::Param { idx } => Expr::Literal(params.get(idx).cloned().unwrap_or(Value::Null)),
        // Compound nodes — recurse.
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(subst_expr_param(*operand, params)),
        },
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(subst_expr_param(*left, params)),
            right: Box::new(subst_expr_param(*right, params)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_expr_param(*expr, params)),
            negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_expr_param(*expr, params)),
            low: Box::new(subst_expr_param(*low, params)),
            high: Box::new(subst_expr_param(*high, params)),
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(subst_expr_param(*expr, params)),
            pattern: Box::new(subst_expr_param(*pattern, params)),
            negated,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(subst_expr_param(*expr, params)),
            list: list
                .into_iter()
                .map(|e| subst_expr_param(e, params))
                .collect(),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args
                .into_iter()
                .map(|e| subst_expr_param(e, params))
                .collect(),
        },
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => Expr::Case {
            operand: operand.map(|e| Box::new(subst_expr_param(*e, params))),
            when_thens: when_thens
                .into_iter()
                .map(|(w, t)| (subst_expr_param(w, params), subst_expr_param(t, params)))
                .collect(),
            else_result: else_result.map(|e| Box::new(subst_expr_param(*e, params))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(subst_expr_param(*expr, params)),
            target,
        },
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(subst_expr_param(*expr, params)),
            query,
            negated,
        },
        // Leaf nodes — pass through unchanged.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::charset::{DEFAULT_SERVER_COLLATION, UTF8MB4_CHARSET};
    use super::*;
    use axiomdb_types::DataType;

    #[test]
    fn test_substitute_int() {
        let result = substitute_params("SELECT * FROM t WHERE id = ?", &[Value::Int(42)]).unwrap();
        assert_eq!(result, "SELECT * FROM t WHERE id = 42");
    }

    #[test]
    fn test_substitute_text_with_quote_escape() {
        let result = substitute_params(
            "SELECT * FROM t WHERE name = ?",
            &[Value::Text("O'Brien".into())],
        )
        .unwrap();
        assert_eq!(result, "SELECT * FROM t WHERE name = 'O''Brien'");
    }

    #[test]
    fn test_substitute_null() {
        let result = substitute_params("INSERT INTO t VALUES (?)", &[Value::Null]).unwrap();
        assert_eq!(result, "INSERT INTO t VALUES (NULL)");
    }

    #[test]
    fn test_substitute_multiple_params() {
        let result = substitute_params(
            "INSERT INTO t VALUES (?, ?)",
            &[Value::Int(1), Value::Text("hello".into())],
        )
        .unwrap();
        assert_eq!(result, "INSERT INTO t VALUES (1, 'hello')");
    }

    #[test]
    fn test_question_mark_in_string_not_substituted() {
        let result = substitute_params("SELECT '?' FROM t WHERE id = ?", &[Value::Int(5)]).unwrap();
        assert_eq!(result, "SELECT '?' FROM t WHERE id = 5");
    }

    #[test]
    fn test_too_few_params_error() {
        let result = substitute_params("SELECT * FROM t WHERE id = ?", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_is_null_bitmap() {
        let bitmap = [0b00000101u8]; // params 0 and 2 are NULL
        assert!(is_null(&bitmap, 0));
        assert!(!is_null(&bitmap, 1));
        assert!(is_null(&bitmap, 2));
        assert!(!is_null(&bitmap, 3));
    }

    #[test]
    fn test_decode_tiny() {
        let buf = [42u8];
        let (val, consumed) = decode_binary_value(&buf, 0x01, &UTF8MB4_CHARSET).unwrap();
        assert_eq!(val, Value::Int(42));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_decode_longlong() {
        let n: i64 = 1_000_000_000_000;
        let buf = n.to_le_bytes();
        let (val, consumed) = decode_binary_value(&buf, 0x08, &UTF8MB4_CHARSET).unwrap();
        assert_eq!(val, Value::BigInt(n));
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_decode_string() {
        let s = b"hello";
        let mut buf = vec![s.len() as u8];
        buf.extend_from_slice(s);
        let (val, consumed) = decode_binary_value(&buf, 0xfd, &UTF8MB4_CHARSET).unwrap();
        assert_eq!(val, Value::Text("hello".into()));
        assert_eq!(consumed, 6);
    }

    #[test]
    fn test_ymd_to_days_epoch() {
        assert_eq!(ymd_to_days(1970, 1, 1), 0);
        assert_eq!(ymd_to_days(1970, 1, 2), 1);
        assert_eq!(ymd_to_days(1971, 1, 1), 365);
    }

    #[test]
    fn test_prepare_response_structure() {
        let cols = vec![ColumnMeta::computed("id".to_string(), DataType::Int)];
        let packets = build_prepare_response(1, 2, &cols, 1, DEFAULT_SERVER_COLLATION);
        // seq=1: OK, seq=2: param stub, seq=3: param stub, seq=4: EOF, seq=5: col def, seq=6: EOF
        assert_eq!(packets.len(), 6);
        assert_eq!(packets[0].0, 1); // OK at seq=1
        assert_eq!(packets[0].1[0], 0x00); // status = OK
    }

    // ── 4.10d — Parameterized LIMIT/OFFSET substitution ──────────────────────

    #[test]
    fn test_substitute_params_limit_offset_string_path() {
        // SQL-string substitution path: Text params become single-quoted literals.
        // The executor's eval_row_count_as_usize then accepts them as valid row counts.
        let sql = substitute_params(
            "SELECT * FROM t ORDER BY a LIMIT ? OFFSET ?",
            &[Value::Text("2".into()), Value::Text("1".into())],
        )
        .unwrap();
        assert_eq!(sql, "SELECT * FROM t ORDER BY a LIMIT '2' OFFSET '1'");
    }

    #[test]
    fn test_substitute_params_limit_offset_int_path() {
        // Int params remain unquoted — still valid row counts.
        let sql = substitute_params(
            "SELECT * FROM t ORDER BY a LIMIT ? OFFSET ?",
            &[Value::Int(5), Value::Int(10)],
        )
        .unwrap();
        assert_eq!(sql, "SELECT * FROM t ORDER BY a LIMIT 5 OFFSET 10");
    }

    #[test]
    fn test_subst_select_limit_offset_ast_path() {
        // Verify the cached-AST substitution path replaces Param nodes in
        // limit and offset without requiring a full DB setup.
        // parse() builds an AST with Expr::Param nodes in limit/offset;
        // substitute_params_in_ast() replaces them with Literal values.
        let template =
            axiomdb_sql::parse("SELECT a FROM t ORDER BY a LIMIT ? OFFSET ?", None).unwrap();

        let substituted =
            substitute_params_in_ast(template, &[Value::Int(2), Value::Int(1)]).unwrap();

        if let axiomdb_sql::Stmt::Select(s) = substituted {
            assert!(
                matches!(s.limit, Some(axiomdb_sql::Expr::Literal(Value::Int(2)))),
                "limit should be Literal(Int(2)), got {:?}",
                s.limit
            );
            assert!(
                matches!(s.offset, Some(axiomdb_sql::Expr::Literal(Value::Int(1)))),
                "offset should be Literal(Int(1)), got {:?}",
                s.offset
            );
        } else {
            panic!("expected Select stmt");
        }
    }

    #[test]
    fn test_decode_string_type_for_limit_param() {
        // MYSQL_TYPE_STRING (0xfd) carrying the text "1" decodes to Value::Text("1").
        // This is the compatibility case from MariaDB clients that bind LIMIT ?
        // as a string type.
        let s = b"1";
        let mut buf = vec![s.len() as u8]; // lenenc: 1 byte length prefix
        buf.extend_from_slice(s);
        let (val, consumed) = decode_binary_value(&buf, 0xfd, &UTF8MB4_CHARSET).unwrap();
        assert_eq!(val, Value::Text("1".into()));
        assert_eq!(consumed, 2); // 1 byte lenenc + 1 byte payload
    }

    #[test]
    fn test_parse_execute_packet_string_limit_param() {
        // Full parse_execute_packet path: one MYSQL_TYPE_STRING param carrying "2".
        let mut stmt = PreparedStatement {
            stmt_id: 1,
            sql_template: "SELECT a FROM t LIMIT ?".into(),
            param_count: 1,
            param_types: vec![0xfd], // MYSQL_TYPE_STRING
            analyzed_stmt: None,
            compiled_at_version: 0,
            last_used_seq: 0,
        };

        // Build a minimal COM_STMT_EXECUTE payload:
        //   [0..4]  stmt_id = 1 (LE u32)
        //   [4]     flags = 0
        //   [5..9]  iteration_count = 1 (LE u32)
        //   [9]     null_bitmap: 1 param, not null → 0x00
        //   [10]    new_params_bound_flag = 1
        //   [11..13] type list: 0xfd 0x00 (MYSQL_TYPE_STRING, unsigned=0)
        //   [13]    lenenc length = 1
        //   [14]    payload = b'2'
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes()); // stmt_id
        payload.push(0); // flags
        payload.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
        payload.push(0x00); // null_bitmap (param 0 is not null)
        payload.push(1); // new_params_bound_flag
        payload.push(0xfd); // MYSQL_TYPE_STRING
        payload.push(0x00); // unsigned flag
        payload.push(1); // lenenc: length = 1
        payload.push(b'2'); // the string "2"

        let exec = parse_execute_packet(&payload, &mut stmt, &UTF8MB4_CHARSET).unwrap();
        assert_eq!(exec.params.len(), 1);
        assert_eq!(exec.params[0], Value::Text("2".into()));
    }
}
