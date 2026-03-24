//! DbError → MySQL error code + SQLSTATE mapping.

use axiomdb_core::error::DbError;

pub struct MysqlError {
    pub code: u16,
    pub sql_state: [u8; 5],
    pub message: String,
}

/// Converts a `DbError` to the closest MySQL error code and SQLSTATE.
pub fn dberror_to_mysql(e: &DbError) -> MysqlError {
    let (code, state, msg): (u16, &[u8; 5], String) = match e {
        DbError::ParseError { message } => (
            1064,
            b"42000",
            format!("You have an error in your SQL syntax: {message}"),
        ),
        DbError::TableNotFound { name } => {
            (1146, b"42S02", format!("Table '{name}' doesn't exist"))
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
        DbError::UniqueViolation { table, column } => (
            1062,
            b"23000",
            format!("Duplicate entry for key '{table}.{column}'"),
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
