//! Structured error response for SQL clients.
//!
//! [`ErrorResponse`] is the presentation type produced from a [`DbError`].
//! It carries all the information a SQL client needs to understand and
//! react to an error:
//!
//! - **`sqlstate`** — 5-character SQLSTATE code for programmatic handling
//!   (e.g. `"42P01"` = table not found, `"23505"` = unique violation).
//! - **`severity`** — `ERROR`, `WARNING`, or `NOTICE`.
//! - **`message`** — short human-readable description (same as `DbError::to_string()`).
//! - **`detail`** — optional extended context (the offending value, referenced row, etc.).
//! - **`hint`** — optional actionable suggestion for how to fix the error.
//! - **`position`** — byte offset in the SQL query (always `None` in Phase 4.25;
//!   populated in Phase 4.25b when the parser tracks token positions).
//!
//! ## Usage
//!
//! ```rust
//! use axiomdb_core::error::DbError;
//! use axiomdb_core::error_response::ErrorResponse;
//!
//! let err = DbError::TableNotFound { name: "users".into() };
//! let resp = ErrorResponse::from_error(&err);
//! println!("{resp}");
//! // ERROR 42P01: table 'users' not found
//! // HINT:     Did you spell the table name correctly? ...
//! ```
//!
//! ## Why a separate type?
//!
//! [`DbError`] is a *domain* type — it carries only the information known at
//! the point of failure. `ErrorResponse` is a *presentation* type — it builds
//! user-facing text from the error fields. Mixing them would couple error
//! generation to error display, making it harder to change either independently.

use std::fmt;

use crate::error::DbError;

// ── Severity ──────────────────────────────────────────────────────────────────

/// Severity of a SQL message.
///
/// Matches the PostgreSQL protocol severity codes used in `ErrorResponse`
/// and `NoticeResponse` packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// A fatal error that terminated the current statement.
    Error,
    /// A non-fatal warning. Statement may continue. (Phase 4.25c)
    Warning,
    /// An informational message. No action needed. (Phase 4.25c)
    Notice,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => write!(f, "ERROR"),
            Self::Warning => write!(f, "WARNING"),
            Self::Notice => write!(f, "NOTICE"),
        }
    }
}

// ── ErrorResponse ─────────────────────────────────────────────────────────────

/// Structured error response for delivery to SQL clients.
///
/// Build from a [`DbError`] using [`ErrorResponse::from_error`] or the
/// [`From`] trait. The wire protocol server (Phase 5) serializes this into a
/// MySQL `ERR_Packet` or PostgreSQL `ErrorResponse` message.
///
/// ## Display format
///
/// ```text
/// ERROR 42P01: table 'users' not found
/// HINT:     Did you spell the table name correctly? Use SHOW TABLES to list available tables.
/// ```
///
/// If `detail` and `hint` are both present:
///
/// ```text
/// ERROR 23503: foreign key violation: users.company_id = 999
/// DETAIL:   Key (company_id)=(999) is not present in table users.
/// HINT:     Insert the referenced row first, or use ON DELETE CASCADE.
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    /// SQLSTATE code — 5-character string (e.g. `"42P01"`).
    ///
    /// ORMs use this field to detect specific error conditions without parsing
    /// the message string. Always use this field for programmatic handling,
    /// never the `message` field.
    pub sqlstate: String,

    /// Severity of the error.
    pub severity: Severity,

    /// Short human-readable error message. Same as `DbError::to_string()`.
    ///
    /// Suitable for display in error dialogs and logs. Do not parse this
    /// string programmatically — use `sqlstate` instead.
    pub message: String,

    /// Optional extended detail about the error.
    ///
    /// Provides context beyond the message. Examples:
    /// - `ForeignKeyViolation` → `"Key (user_id)=(999) is not present in table users."`
    /// - `AmbiguousColumn`     → `"Column 'id' appears in: users.id, orders.id."`
    /// - `InvalidCoercion`     → `"Cannot convert 'abc' (Text) to INT: 'abc' is not a valid integer."`
    pub detail: Option<String>,

    /// Optional actionable hint for how to fix the error.
    ///
    /// Written as a direct instruction to the developer. Examples:
    /// - `TableNotFound`   → `"Did you spell the table name correctly? Use SHOW TABLES..."`
    /// - `DivisionByZero`  → `"Add a WHERE guard: WHERE divisor <> 0, or use NULLIF(divisor, 0)."`
    /// - `Overflow`        → `"Use a wider numeric type (e.g. BIGINT instead of INT)."`
    pub hint: Option<String>,

    /// Byte offset of the error in the SQL query (1-based).
    ///
    /// Always `None` in Phase 4.25. Populated in Phase 4.25b when the parser
    /// tracks source positions for tokens.
    pub position: Option<usize>,
}

impl ErrorResponse {
    /// Builds an [`ErrorResponse`] from a [`DbError`].
    ///
    /// `detail` and `hint` are populated for variants where the error fields
    /// carry enough information to produce useful text. `position` is always
    /// `None` in Phase 4.25 (requires parser token positions — Phase 4.25b).
    ///
    /// This function is infallible — it never panics.
    pub fn from_error(err: &DbError) -> Self {
        let (detail, hint) = derive_detail_hint(err);
        let position = match err {
            DbError::ParseError { position, .. } => *position,
            _ => None,
        };
        Self {
            sqlstate: err.sqlstate().to_string(),
            severity: Severity::Error,
            message: err.to_string(),
            detail,
            hint,
            position,
        }
    }

    /// Returns the full display string, equivalent to `format!("{self}")`.
    pub fn display_string(&self) -> String {
        self.to_string()
    }
}

// ── Display ───────────────────────────────────────────────────────────────────

impl fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.severity, self.sqlstate, self.message)?;
        if let Some(ref d) = self.detail {
            write!(f, "\nDETAIL:   {d}")?;
        }
        if let Some(ref h) = self.hint {
            write!(f, "\nHINT:     {h}")?;
        }
        if let Some(p) = self.position {
            write!(f, "\nPOSITION: {p}")?;
        }
        Ok(())
    }
}

// ── From impls ────────────────────────────────────────────────────────────────

impl From<&DbError> for ErrorResponse {
    fn from(err: &DbError) -> Self {
        Self::from_error(err)
    }
}

impl From<DbError> for ErrorResponse {
    fn from(err: DbError) -> Self {
        Self::from_error(&err)
    }
}

// ── detail / hint derivation ──────────────────────────────────────────────────

/// Derives `(detail, hint)` from a `DbError`.
///
/// Returns `(None, None)` for variants where no user-actionable context is
/// available (internal errors, I/O errors, etc.).
fn derive_detail_hint(err: &DbError) -> (Option<String>, Option<String>) {
    match err {
        // ── Integrity violations ───────────────────────────────────────────
        DbError::UniqueViolation { index_name, value } => (
            Some(match value {
                Some(v) => format!("Key (value)=({v}) is already present in index {index_name}."),
                None => format!("Duplicate key in index {index_name}."),
            }),
            Some(format!(
                "A row with the same value already exists in index {index_name}. \
                 Use INSERT ... ON CONFLICT to handle duplicates."
            )),
        ),

        DbError::ForeignKeyViolation {
            table,
            column,
            value,
        } => (
            Some(format!(
                "Key ({column})=({value}) is not present in table {table}."
            )),
            Some("Insert the referenced row first, or use ON DELETE CASCADE.".into()),
        ),

        DbError::NotNullViolation { table, column } => (
            None,
            Some(format!(
                "Provide a non-NULL value for column {column} in table {table}."
            )),
        ),

        DbError::CheckViolation { table, constraint } => (
            None,
            Some(format!(
                "The row violates CHECK constraint {constraint} on table {table}. \
                 Review the constraint definition and adjust the input values."
            )),
        ),

        DbError::DuplicateKey => (
            None,
            Some(
                "A row with the same primary key already exists. \
                 Use a different key value or UPDATE the existing row."
                    .into(),
            ),
        ),

        // ── Schema errors ──────────────────────────────────────────────────
        DbError::TableNotFound { name } => (
            None,
            Some(format!(
                "Table '{name}' does not exist. \
                 Did you spell it correctly? Use SHOW TABLES to list available tables."
            )),
        ),

        DbError::TableAlreadyExists { schema, name } => (
            None,
            Some(format!(
                "Table '{schema}.{name}' already exists. \
                 Use CREATE TABLE IF NOT EXISTS to skip silently."
            )),
        ),

        DbError::ColumnNotFound { name, table } => (
            None,
            Some(format!(
                "Column '{name}' does not exist in table '{table}'. \
                 Use DESCRIBE {table} to list available columns."
            )),
        ),

        DbError::AmbiguousColumn { name, tables } => (
            Some(format!("Column '{name}' appears in: {tables}.")),
            Some(format!(
                "Qualify the column name with a table alias, e.g. t.{name}."
            )),
        ),

        // ── Type / coercion errors ─────────────────────────────────────────
        DbError::TypeMismatch { expected, got } => (
            None,
            Some(format!(
                "Expected {expected} but received {got}. \
                 Use an explicit CAST to convert between types."
            )),
        ),

        DbError::InvalidCoercion {
            from,
            to,
            value,
            reason,
        } => (
            Some(format!(
                "Cannot convert {value} ({from}) to {to}: {reason}."
            )),
            Some(format!("Use an explicit CAST: CAST({value} AS {to}).")),
        ),

        DbError::DivisionByZero => (
            None,
            Some(
                "Add a WHERE guard: WHERE divisor <> 0, or use NULLIF(divisor, 0) \
                 to return NULL instead of an error."
                    .into(),
            ),
        ),

        DbError::Overflow => (
            None,
            Some(
                "The result exceeds the range of the numeric type. \
                 Use a wider type (e.g. BIGINT instead of INT, or DECIMAL for exact arithmetic)."
                    .into(),
            ),
        ),

        DbError::KeyTooLong { len, max } => (
            None,
            Some(format!(
                "The key is {len} bytes but the maximum is {max}. \
                 Shorten the value or use a larger VARCHAR type."
            )),
        ),

        DbError::ValueTooLarge { len, max } => (
            None,
            Some(format!(
                "The value is {len} bytes but the maximum is {max}. \
                 Use BLOB/BYTEA for large binary data, or TOAST (Phase 6) for large text."
            )),
        ),

        // ── Transaction errors ─────────────────────────────────────────────
        DbError::TransactionAlreadyActive { .. } => (
            None,
            Some("COMMIT or ROLLBACK the current transaction before starting a new one.".into()),
        ),

        DbError::NoActiveTransaction => {
            (None, Some("Start a transaction with BEGIN first.".into()))
        }

        DbError::DeadlockDetected => (
            None,
            Some(
                "Retry the transaction. To reduce deadlocks, acquire locks in a \
                 consistent order across concurrent transactions."
                    .into(),
            ),
        ),

        // ── Features ──────────────────────────────────────────────────────
        DbError::NotImplemented { feature } => (
            None,
            Some(format!(
                "This feature ({feature}) is planned for a future version of AxiomDB. \
                 See the roadmap at docs/progreso.md."
            )),
        ),

        // ── System errors ─────────────────────────────────────────────────
        DbError::StorageFull => (
            None,
            Some(
                "Free up disk space or expand the storage volume. \
                 Running VACUUM (Phase 9) may reclaim space from deleted rows."
                    .into(),
            ),
        ),

        DbError::FileLocked { path } => (
            None,
            Some(format!(
                "Another AxiomDB process is already using the database at '{}'.\n\
                 Ensure only one server accesses the data directory at a time.",
                path.display()
            )),
        ),

        // ── All other variants have no actionable context ──────────────────
        _ => (None, None),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;

    // ── SQLSTATE completeness ────────────────────────────────────────────────

    #[test]
    fn test_sqlstate_integrity_variants() {
        assert_eq!(
            DbError::UniqueViolation {
                index_name: "t_c_idx".into(),
                value: None,
            }
            .sqlstate(),
            "23505"
        );
        assert_eq!(
            DbError::ForeignKeyViolation {
                table: "t".into(),
                column: "c".into(),
                value: "1".into()
            }
            .sqlstate(),
            "23503"
        );
        assert_eq!(
            DbError::NotNullViolation {
                table: "t".into(),
                column: "c".into()
            }
            .sqlstate(),
            "23502"
        );
        assert_eq!(
            DbError::CheckViolation {
                table: "t".into(),
                constraint: "ck".into()
            }
            .sqlstate(),
            "23514"
        );
        assert_eq!(DbError::DuplicateKey.sqlstate(), "23505");
    }

    #[test]
    fn test_sqlstate_transaction_variants() {
        assert_eq!(DbError::DeadlockDetected.sqlstate(), "40P01");
        assert_eq!(
            DbError::TransactionAlreadyActive { txn_id: 1 }.sqlstate(),
            "25001"
        );
        assert_eq!(DbError::NoActiveTransaction.sqlstate(), "25P01");
        assert_eq!(
            DbError::TransactionExpired { txn_id: 1 }.sqlstate(),
            "25006"
        );
    }

    #[test]
    fn test_sqlstate_schema_variants() {
        assert_eq!(
            DbError::ParseError {
                message: "x".into(),
                position: None,
            }
            .sqlstate(),
            "42601"
        );
        assert_eq!(
            DbError::TableNotFound { name: "t".into() }.sqlstate(),
            "42P01"
        );
        assert_eq!(
            DbError::TableAlreadyExists {
                schema: "public".into(),
                name: "t".into()
            }
            .sqlstate(),
            "42P07"
        );
        assert_eq!(
            DbError::ColumnNotFound {
                name: "c".into(),
                table: "t".into()
            }
            .sqlstate(),
            "42703"
        );
        assert_eq!(
            DbError::AmbiguousColumn {
                name: "id".into(),
                tables: "a, b".into()
            }
            .sqlstate(),
            "42702"
        );
        assert_eq!(
            DbError::PermissionDenied {
                action: "x".into(),
                object: "y".into()
            }
            .sqlstate(),
            "42501"
        );
    }

    #[test]
    fn test_sqlstate_data_type_variants() {
        assert_eq!(
            DbError::TypeMismatch {
                expected: "x".into(),
                got: "y".into()
            }
            .sqlstate(),
            "42804"
        );
        assert_eq!(
            DbError::InvalidCoercion {
                from: "Text".into(),
                to: "INT".into(),
                value: "'abc'".into(),
                reason: "r".into()
            }
            .sqlstate(),
            "22018"
        );
        assert_eq!(DbError::DivisionByZero.sqlstate(), "22012");
        assert_eq!(DbError::Overflow.sqlstate(), "22003");
        assert_eq!(
            DbError::KeyTooLong { len: 100, max: 64 }.sqlstate(),
            "22001"
        );
        assert_eq!(
            DbError::ValueTooLarge { len: 100, max: 64 }.sqlstate(),
            "22001"
        );
        assert_eq!(
            DbError::InvalidValue { reason: "r".into() }.sqlstate(),
            "22P02"
        );
    }

    #[test]
    fn test_sqlstate_feature_and_system_variants() {
        assert_eq!(
            DbError::NotImplemented {
                feature: "JOIN".into()
            }
            .sqlstate(),
            "0A000"
        );
        assert_eq!(DbError::StorageFull.sqlstate(), "53100");
        assert_eq!(DbError::SequenceOverflow.sqlstate(), "2200H");
    }

    #[test]
    fn test_sqlstate_internal_variants_return_xx000() {
        assert_eq!(DbError::PageNotFound { page_id: 1 }.sqlstate(), "XX000");
        assert_eq!(
            DbError::HeapPageFull {
                page_id: 1,
                needed: 10,
                available: 5
            }
            .sqlstate(),
            "XX000"
        );
        assert_eq!(
            DbError::BTreeCorrupted { msg: "x".into() }.sqlstate(),
            "XX000"
        );
        assert_eq!(DbError::CatalogNotInitialized.sqlstate(), "XX000");
        assert_eq!(
            DbError::WalChecksumMismatch {
                lsn: 1,
                expected: 0,
                got: 1
            }
            .sqlstate(),
            "XX000"
        );
    }

    // ── from_error construction ──────────────────────────────────────────────

    #[test]
    fn test_from_error_table_not_found() {
        let err = DbError::TableNotFound {
            name: "orders".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "42P01");
        assert_eq!(resp.severity, Severity::Error);
        assert!(resp.hint.is_some());
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("SHOW TABLES"),
            "hint should mention SHOW TABLES, got: {hint}"
        );
        assert_eq!(resp.detail, None);
        assert_eq!(resp.position, None);
    }

    #[test]
    fn test_from_error_column_not_found() {
        let err = DbError::ColumnNotFound {
            name: "age".into(),
            table: "users".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "42703");
        let hint = resp.hint.unwrap();
        assert!(hint.contains("age"), "hint should contain column name");
        assert!(hint.contains("users"), "hint should contain table name");
    }

    #[test]
    fn test_from_error_ambiguous_column() {
        let err = DbError::AmbiguousColumn {
            name: "id".into(),
            tables: "users.id, orders.id".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "42702");
        assert!(resp.detail.is_some());
        let detail = resp.detail.unwrap();
        assert!(detail.contains("id"), "detail should contain column name");
        assert!(
            detail.contains("users.id"),
            "detail should contain table refs"
        );
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("alias") || hint.contains("t.id"),
            "hint should suggest qualification"
        );
    }

    #[test]
    fn test_from_error_unique_violation() {
        let err = DbError::UniqueViolation {
            index_name: "users_email_idx".into(),
            value: Some("bob@example.com".into()),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "23505");
        let detail = resp.detail.unwrap();
        assert!(
            detail.contains("bob@example.com"),
            "detail should contain offending value, got: {detail}"
        );
        assert!(
            detail.contains("users_email_idx"),
            "detail should contain index name, got: {detail}"
        );
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("users_email_idx"),
            "hint should reference index name, got: {hint}"
        );
    }

    #[test]
    fn test_from_error_parse_error_position() {
        let err = DbError::ParseError {
            message: "unexpected token 'FORM'".into(),
            position: Some(9),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "42601");
        assert_eq!(resp.position, Some(9));
    }

    #[test]
    fn test_from_error_parse_error_no_position() {
        let err = DbError::ParseError {
            message: "input too long".into(),
            position: None,
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.position, None);
    }

    #[test]
    fn test_from_error_foreign_key_violation() {
        let err = DbError::ForeignKeyViolation {
            table: "users".into(),
            column: "company_id".into(),
            value: "999".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "23503");
        let detail = resp.detail.unwrap();
        assert!(
            detail.contains("999"),
            "detail should contain offending value"
        );
        assert!(
            detail.contains("company_id"),
            "detail should contain column name"
        );
        assert!(resp.hint.is_some());
    }

    #[test]
    fn test_from_error_not_null_violation() {
        let err = DbError::NotNullViolation {
            table: "t".into(),
            column: "name".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "23502");
        let hint = resp.hint.unwrap();
        assert!(hint.contains("name") || hint.contains("NULL"));
    }

    #[test]
    fn test_from_error_invalid_coercion() {
        let err = DbError::InvalidCoercion {
            from: "Text".into(),
            to: "INT".into(),
            value: "'abc'".into(),
            reason: "not a valid integer".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "22018");
        let detail = resp.detail.unwrap();
        assert!(
            detail.contains("'abc'"),
            "detail should contain the offending value"
        );
        assert!(detail.contains("INT"), "detail should mention target type");
        let hint = resp.hint.unwrap();
        assert!(hint.contains("CAST"), "hint should suggest CAST");
    }

    #[test]
    fn test_from_error_division_by_zero() {
        let err = DbError::DivisionByZero;
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "22012");
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("NULLIF") || hint.contains("WHERE"),
            "hint should suggest workaround"
        );
    }

    #[test]
    fn test_from_error_overflow() {
        let resp = ErrorResponse::from_error(&DbError::Overflow);
        assert_eq!(resp.sqlstate, "22003");
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("BIGINT") || hint.contains("wider"),
            "hint should suggest wider type"
        );
    }

    #[test]
    fn test_from_error_transaction_already_active() {
        let resp = ErrorResponse::from_error(&DbError::TransactionAlreadyActive { txn_id: 1 });
        assert_eq!(resp.sqlstate, "25001");
        let hint = resp.hint.unwrap();
        assert!(hint.contains("COMMIT") || hint.contains("ROLLBACK"));
    }

    #[test]
    fn test_from_error_no_active_transaction() {
        let resp = ErrorResponse::from_error(&DbError::NoActiveTransaction);
        assert_eq!(resp.sqlstate, "25P01");
        let hint = resp.hint.unwrap();
        assert!(hint.contains("BEGIN"));
    }

    #[test]
    fn test_from_error_not_implemented() {
        let err = DbError::NotImplemented {
            feature: "JOIN".into(),
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "0A000");
        let hint = resp.hint.unwrap();
        assert!(
            hint.contains("JOIN"),
            "hint should mention the feature name"
        );
    }

    #[test]
    fn test_from_error_parse_error() {
        let err = DbError::ParseError {
            message: "unexpected token".into(),
            position: None,
        };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "42601");
        // Parse errors are self-explanatory; no generic hint is added.
        assert_eq!(resp.detail, None);
    }

    #[test]
    fn test_from_error_internal_error_no_hint() {
        let err = DbError::PageNotFound { page_id: 42 };
        let resp = ErrorResponse::from_error(&err);
        assert_eq!(resp.sqlstate, "XX000");
        assert_eq!(resp.detail, None);
        assert_eq!(resp.hint, None);
    }

    // ── Display format ───────────────────────────────────────────────────────

    #[test]
    fn test_display_error_only() {
        let resp = ErrorResponse {
            sqlstate: "42P01".into(),
            severity: Severity::Error,
            message: "table 'users' not found".into(),
            detail: None,
            hint: None,
            position: None,
        };
        let s = resp.to_string();
        assert_eq!(s, "ERROR 42P01: table 'users' not found");
    }

    #[test]
    fn test_display_with_hint() {
        let resp = ErrorResponse {
            sqlstate: "42P01".into(),
            severity: Severity::Error,
            message: "table 'users' not found".into(),
            detail: None,
            hint: Some("Use SHOW TABLES.".into()),
            position: None,
        };
        let s = resp.to_string();
        assert!(s.contains("ERROR 42P01: table 'users' not found"));
        assert!(s.contains("HINT:     Use SHOW TABLES."));
    }

    #[test]
    fn test_display_with_detail_and_hint() {
        let resp = ErrorResponse {
            sqlstate: "23503".into(),
            severity: Severity::Error,
            message: "foreign key violation: users.company_id = 999".into(),
            detail: Some("Key (company_id)=(999) is not present in table companies.".into()),
            hint: Some("Insert the referenced row first.".into()),
            position: None,
        };
        let s = resp.to_string();
        assert!(s.contains("ERROR 23503:"));
        assert!(s.contains("DETAIL:   Key (company_id)=(999)"));
        assert!(s.contains("HINT:     Insert the referenced row first."));
    }

    #[test]
    fn test_display_severity_shown() {
        let resp = ErrorResponse {
            sqlstate: "42P01".into(),
            severity: Severity::Error,
            message: "test".into(),
            detail: None,
            hint: None,
            position: None,
        };
        assert!(resp.to_string().starts_with("ERROR "));
    }

    // ── From trait ───────────────────────────────────────────────────────────

    #[test]
    fn test_from_ref_dbeerror() {
        let err = DbError::TableNotFound { name: "t".into() };
        let via_from: ErrorResponse = ErrorResponse::from(&err);
        let via_method = ErrorResponse::from_error(&err);
        assert_eq!(via_from, via_method);
    }

    #[test]
    fn test_from_owned_dbeerror() {
        // Use two separately-constructed identical errors to test both paths.
        let via_from: ErrorResponse = ErrorResponse::from(DbError::DivisionByZero);
        let via_method = ErrorResponse::from_error(&DbError::DivisionByZero);
        assert_eq!(via_from, via_method);
    }
}
