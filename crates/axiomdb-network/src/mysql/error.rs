//! DbError → MySQL error code + SQLSTATE mapping.

use axiomdb_core::error::DbError;

pub struct MysqlError {
    pub code: u16,
    pub sql_state: [u8; 5],
    pub message: String,
}

// ── Visual snippet ─────────────────────────────────────────────────────────────

/// Builds a 2-line visual snippet showing where in `sql` the error occurred.
///
/// `pos` is the 0-based byte offset of the unexpected token (as stored in
/// `DbError::ParseError::position`). Returns an empty string when `pos` is
/// out of bounds or `sql` is empty, so the caller can safely append it.
///
/// Example output (prepended with `\n`):
/// ```text
///
///   SELECT * FORM t
///            ^
/// ```
fn build_error_snippet(sql: &str, pos: usize) -> String {
    if sql.is_empty() || pos >= sql.len() {
        return String::new();
    }
    // Byte-safe: parser positions are always at token start boundaries.
    let line_start = sql[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = sql[pos..].find('\n').map(|i| pos + i).unwrap_or(sql.len());
    let line = &sql[line_start..line_end];
    let col = pos - line_start;

    const MAX_LINE: usize = 120;
    let (display_line, display_col) = if line.len() > MAX_LINE {
        (line[..MAX_LINE].to_string(), col.min(MAX_LINE - 1))
    } else {
        (line.to_string(), col)
    };

    format!("\n  {display_line}\n  {}^", " ".repeat(display_col))
}

// ── dberror_to_mysql ───────────────────────────────────────────────────────────

/// Converts a `DbError` to the closest MySQL error code and SQLSTATE.
///
/// `sql` is the original SQL string that caused the error, used to build
/// a visual snippet for `ParseError`. Pass `None` when the SQL is unavailable
/// (e.g. auth errors, commit callbacks).
pub fn dberror_to_mysql(e: &DbError, sql: Option<&str>) -> MysqlError {
    let (code, state, msg): (u16, &[u8; 5], String) = match e {
        DbError::ParseError { message, position } => {
            let snippet = position
                .and_then(|pos| sql.map(|s| build_error_snippet(s, pos)))
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            (
                1064,
                b"42000",
                format!("You have an error in your SQL syntax: {message}{snippet}"),
            )
        }
        DbError::TableNotFound { name } => {
            (1146, b"42S02", format!("Table '{name}' doesn't exist"))
        }
        DbError::DatabaseNotFound { name } => {
            (1049, b"42000", format!("Unknown database '{name}'"))
        }
        DbError::ColumnNotFound { name, table } => (
            1054,
            b"42S22",
            format!("Unknown column '{name}' in '{table}'"),
        ),
        DbError::ColumnAlreadyExists { name, table } => (
            1060,
            b"42701",
            format!("Duplicate column name '{name}' in '{table}'"),
        ),
        DbError::TableAlreadyExists { name, schema } => (
            1050,
            b"42S01",
            format!("Table '{schema}.{name}' already exists"),
        ),
        DbError::DatabaseAlreadyExists { name } => (
            1007,
            b"HY000",
            format!("Can't create database '{name}'; database exists"),
        ),
        DbError::UniqueViolation { index_name, value } => (
            1062,
            b"23000",
            match value {
                Some(v) => format!("Duplicate entry '{v}' for key '{index_name}'"),
                None => format!("Duplicate entry for key '{index_name}'"),
            },
        ),
        DbError::NotNullViolation { table, column } => (
            1048,
            b"23000",
            format!("Column '{table}.{column}' cannot be null"),
        ),
        DbError::ForeignKeyViolation {
            table,
            column,
            value,
        } => (
            1452,
            b"23000",
            format!("Foreign key constraint fails: '{table}.{column}' = '{value}'"),
        ),
        DbError::CheckViolation { table, constraint } => (
            3819,
            b"HY000",
            format!("Check constraint '{constraint}' is violated for table '{table}'"),
        ),
        DbError::CardinalityViolation { count } => (
            1242,
            b"21000",
            format!("Subquery returns more than 1 row ({count} rows)"),
        ),
        DbError::DivisionByZero => (1365, b"22012", "Division by 0".into()),
        DbError::Overflow => (1690, b"22003", "Value is out of range".into()),
        DbError::TypeMismatch { expected, got } => (
            1292,
            b"22007",
            format!("Type mismatch: expected {expected}, got {got}"),
        ),
        DbError::InvalidCoercion {
            from,
            to,
            value,
            reason,
        } => (
            1292,
            b"22007",
            format!("Cannot cast {value} ({from}) to {to}: {reason}"),
        ),
        DbError::NotImplemented { feature } => {
            (1235, b"0A000", format!("Not supported yet: {feature}"))
        }
        DbError::NoActiveTransaction => (1305, b"42000", "No active transaction".into()),
        DbError::TransactionAlreadyActive { .. } => {
            (1213, b"40001", "Transaction already active".into())
        }
        DbError::SequenceOverflow => (
            1467,
            b"HY000",
            "Sequence overflow: no more IDs available".into(),
        ),
        DbError::ActiveDatabaseDrop { name } => (
            1105,
            b"HY000",
            format!("Can't drop database '{name}'; database is currently selected"),
        ),
        DbError::ColumnIndexOutOfBounds { idx, len } => (
            1105,
            b"HY000",
            format!("Internal: column index {idx} out of bounds (row has {len} columns)"),
        ),
        DbError::Internal { message } => (1105, b"HY000", format!("Internal error: {message}")),
        // Fallback for storage, WAL, and other internal errors
        other => (1105, b"HY000", other.to_string()),
    };

    MysqlError {
        code,
        sql_state: *state,
        message: msg,
    }
}

/// Converts a `DbError` into a MySQL warning code and message string for use
/// with `on_error = 'ignore'`.
///
/// Reuses [`dberror_to_mysql`] so the warning carries the same code and message
/// the client would have seen in an ERR packet.
pub fn dberror_to_mysql_warning(e: &DbError, sql: Option<&str>) -> (u16, String) {
    let me = dberror_to_mysql(e, sql);
    (me.code, me.message)
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_core::error::DbError;

    #[test]
    fn parse_error_with_snippet() {
        let sql = "SELECT * FORM t";
        let err = DbError::ParseError {
            message: "unexpected identifier 'FORM'".into(),
            position: Some(9),
        };
        let me = dberror_to_mysql(&err, Some(sql));
        assert_eq!(me.code, 1064);
        assert!(
            me.message.contains('^'),
            "should contain ^ marker: {}",
            me.message
        );
        assert!(
            me.message.contains("FORM"),
            "should contain the offending token: {}",
            me.message
        );
    }

    #[test]
    fn parse_error_no_sql_no_snippet() {
        let err = DbError::ParseError {
            message: "unexpected token".into(),
            position: Some(5),
        };
        let me = dberror_to_mysql(&err, None);
        assert_eq!(me.code, 1064);
        assert!(!me.message.contains('^'), "no snippet when sql is None");
    }

    #[test]
    fn parse_error_no_position_no_snippet() {
        let err = DbError::ParseError {
            message: "input too long".into(),
            position: None,
        };
        let me = dberror_to_mysql(&err, Some("anything"));
        assert!(
            !me.message.contains('^'),
            "no snippet when position is None"
        );
    }

    #[test]
    fn unique_violation_with_value() {
        let err = DbError::UniqueViolation {
            index_name: "users_email_idx".into(),
            value: Some("bob@example.com".into()),
        };
        let me = dberror_to_mysql(&err, None);
        assert_eq!(me.code, 1062);
        assert_eq!(
            me.message,
            "Duplicate entry 'bob@example.com' for key 'users_email_idx'"
        );
    }

    #[test]
    fn unique_violation_without_value() {
        let err = DbError::UniqueViolation {
            index_name: "pk_idx".into(),
            value: None,
        };
        let me = dberror_to_mysql(&err, None);
        assert_eq!(me.message, "Duplicate entry for key 'pk_idx'");
    }

    #[test]
    fn build_error_snippet_basic() {
        let sql = "SELECT * FORM t";
        let snippet = build_error_snippet(sql, 9);
        assert!(snippet.contains("SELECT * FORM t"));
        assert!(snippet.contains('^'));
        // 9 spaces before ^
        let lines: Vec<&str> = snippet.trim_start_matches('\n').lines().collect();
        assert_eq!(lines.len(), 2);
        let caret_line = lines[1];
        assert!(caret_line.trim_start().starts_with('^'));
    }

    #[test]
    fn build_error_snippet_out_of_bounds() {
        assert_eq!(build_error_snippet("abc", 99), "");
        assert_eq!(build_error_snippet("", 0), "");
    }
}
