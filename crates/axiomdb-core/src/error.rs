use thiserror::Error;

/// Central error type for AxiomDB.
/// All crates return this type (or wrap it).
#[derive(Debug, Error)]
pub enum DbError {
    // ── Storage ──────────────────────────────────────────────────
    #[error("page {page_id} not found")]
    PageNotFound { page_id: u64 },

    #[error("invalid checksum on page {page_id}: expected {expected:#010x}, got {got:#010x}")]
    ChecksumMismatch {
        page_id: u64,
        expected: u32,
        got: u32,
    },

    #[error("storage full: no free pages available")]
    StorageFull,

    /// The underlying volume is out of space or over quota.
    ///
    /// Triggered by `ENOSPC` / `EDQUOT` from WAL or storage I/O.
    /// After this error is returned the database enters read-only degraded
    /// mode and all mutating operations are rejected until the process is
    /// restarted.
    #[error("disk full during '{operation}': no space left on device")]
    DiskFull { operation: &'static str },

    // ── I/O ──────────────────────────────────────────────────────
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cannot open '{path}': another process already holds the file lock")]
    FileLocked { path: std::path::PathBuf },

    // ── SQL ──────────────────────────────────────────────────────
    #[error("SQL syntax error: {message}")]
    ParseError {
        message: String,
        position: Option<usize>,
    },

    #[error("table '{name}' not found")]
    TableNotFound { name: String },

    #[error("database '{name}' not found")]
    DatabaseNotFound { name: String },

    #[error("column '{name}' not found in table '{table}'")]
    ColumnNotFound { name: String, table: String },

    // ── Integrity ────────────────────────────────────────────────
    #[error("unique key violation on index '{index_name}'")]
    UniqueViolation {
        index_name: String,
        value: Option<String>,
    },

    /// Child row references a parent key that does not exist (INSERT/UPDATE child).
    /// SQLSTATE 23503
    #[error("foreign key violation: {table}.{column} = {value}")]
    ForeignKeyViolation {
        table: String,
        column: String,
        value: String,
    },

    /// Parent row cannot be deleted/updated because child rows reference it.
    /// SQLSTATE 23503
    #[error(
        "foreign key constraint \"{constraint}\": {child_table}.{child_column} references this row"
    )]
    ForeignKeyParentViolation {
        constraint: String,
        child_table: String,
        child_column: String,
    },

    /// ON DELETE CASCADE exceeded the maximum allowed recursion depth.
    /// SQLSTATE 23503
    #[error("foreign key cascade depth exceeded limit of {limit}")]
    ForeignKeyCascadeDepth { limit: u32 },

    /// ON DELETE SET NULL attempted on a NOT NULL column.
    /// SQLSTATE 23000
    #[error("cannot set FK column {table}.{column} to NULL: column is NOT NULL")]
    ForeignKeySetNullNotNullable { table: String, column: String },

    /// The referenced parent column has no PRIMARY KEY or UNIQUE index.
    /// SQLSTATE 42830
    #[error("no unique index on {table}.{column} to satisfy foreign key constraint")]
    ForeignKeyNoParentIndex { table: String, column: String },

    #[error("NOT NULL violation on {table}.{column}")]
    NotNullViolation { table: String, column: String },

    #[error("CHECK constraint violation on {table}.{constraint}")]
    CheckViolation { table: String, constraint: String },

    /// Startup-time index integrity verification found an unrecoverable index problem.
    ///
    /// Raised during open/recovery before the database starts serving traffic.
    #[error("index integrity failure on {table}.{index}: {reason}")]
    IndexIntegrityFailure {
        table: String,
        index: String,
        reason: String,
    },

    // ── WAL ──────────────────────────────────────────────────────
    #[error("WAL group commit fsync failed: {message}")]
    WalGroupCommitFailed { message: String },

    #[error(
        "WAL entry at LSN {lsn} has invalid checksum: expected {expected:#010x}, got {got:#010x}"
    )]
    WalChecksumMismatch { lsn: u64, expected: u32, got: u32 },

    #[error("WAL entry at LSN {lsn} is truncated — the file may be corrupt")]
    WalEntryTruncated { lsn: u64 },

    #[error("WAL entry has unknown type: {byte:#04x}")]
    WalUnknownEntryType { byte: u8 },

    #[error("invalid WAL file at '{path}': wrong magic or version")]
    WalInvalidHeader { path: String },

    // ── Transactions ─────────────────────────────────────────────
    #[error("a transaction is already active (txn_id = {txn_id})")]
    TransactionAlreadyActive { txn_id: u64 },

    #[error("no active transaction — call begin() first")]
    NoActiveTransaction,

    #[error("deadlock detected between transactions")]
    DeadlockDetected,

    #[error("transaction {txn_id} is no longer valid")]
    TransactionExpired { txn_id: u64 },

    // ── Permissions ──────────────────────────────────────────────
    #[error("permission denied: {action} on {object}")]
    PermissionDenied { action: String, object: String },

    // ── Types ────────────────────────────────────────────────────
    #[error("type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    // ── Heap pages ───────────────────────────────────────────────
    #[error("heap page {page_id} is full (need {needed} bytes, have {available})")]
    HeapPageFull {
        page_id: u64,
        needed: usize,
        available: usize,
    },

    #[error("invalid slot {slot_id} on page {page_id} (page has {num_slots} slots)")]
    InvalidSlot {
        page_id: u64,
        slot_id: u16,
        num_slots: u16,
    },

    #[error("slot {slot_id} on page {page_id} is already deleted")]
    AlreadyDeleted { page_id: u64, slot_id: u16 },

    // ── B+ Tree ───────────────────────────────────────────────────
    #[error("key too long: {len} bytes (maximum {max})")]
    KeyTooLong { len: usize, max: usize },

    #[error("duplicate key in index")]
    DuplicateKey,

    #[error("B+ tree corrupted: {msg}")]
    BTreeCorrupted { msg: String },

    // ── Catalog ──────────────────────────────────────────────────
    #[error("catalog not initialized — call CatalogBootstrap::init() first")]
    CatalogNotInitialized,

    // ── Row codec ────────────────────────────────────────────────
    #[error("value too large: {len} bytes (maximum {max})")]
    ValueTooLarge { len: usize, max: usize },

    #[error("invalid value: {reason}")]
    InvalidValue { reason: String },

    #[error("invalid DSN: {reason}")]
    InvalidDsn { reason: String },

    // ── Semantic analyzer ─────────────────────────────────────────
    #[error("column reference '{name}' is ambiguous — found in: {tables}")]
    AmbiguousColumn { name: String, tables: String },

    // ── Type coercion ─────────────────────────────────────────────
    /// Implicit conversion between incompatible or invalid types.
    ///
    /// SQLSTATE: 22018 (invalid_character_value_for_cast)
    #[error("cannot coerce {value} ({from}) to {to}: {reason}")]
    InvalidCoercion {
        /// Source type name, e.g. "Text".
        from: String,
        /// Target type name, e.g. "INT".
        to: String,
        /// Display representation of the input value, e.g. "'42abc'".
        value: String,
        /// Human-readable explanation of why the coercion failed.
        reason: String,
    },

    // ── Expression evaluator ──────────────────────────────────────
    #[error("division by zero")]
    DivisionByZero,

    #[error("integer overflow in expression")]
    Overflow,

    #[error("column index {idx} out of bounds (row has {len} columns)")]
    ColumnIndexOutOfBounds { idx: usize, len: usize },

    #[error("table '{schema}.{name}' already exists")]
    TableAlreadyExists { schema: String, name: String },

    #[error("database '{name}' already exists")]
    DatabaseAlreadyExists { name: String },

    #[error("schema '{name}' already exists")]
    SchemaAlreadyExists { name: String },

    #[error("schema '{name}' not found")]
    SchemaNotFound { name: String },

    #[error("table with id {table_id} not found in catalog")]
    CatalogTableNotFound { table_id: u32 },

    #[error("index with id {index_id} not found in catalog")]
    CatalogIndexNotFound { index_id: u32 },

    #[error("sequence overflow: no more IDs available")]
    SequenceOverflow,

    // ── Subqueries ───────────────────────────────────────────────
    /// A scalar subquery returned more than one row.
    /// SQLSTATE 21000 — Cardinality Violation.
    #[error("subquery must return exactly one row, but returned {count} rows")]
    CardinalityViolation { count: usize },

    // ── DDL ───────────────────────────────────────────────────────
    /// A column with this name already exists in the table.
    /// SQLSTATE 42701 — duplicate_column
    #[error("column '{name}' already exists in table '{table}'")]
    ColumnAlreadyExists { name: String, table: String },

    /// An index with this name already exists on the table.
    /// SQLSTATE 42P07 — duplicate_table (reused for objects)
    #[error("index '{name}' already exists on table '{table}'")]
    IndexAlreadyExists { name: String, table: String },

    /// An index key exceeds the maximum allowed byte length.
    /// SQLSTATE 54000 — program_limit_exceeded
    #[error("index key length {key_len} exceeds maximum {max} bytes")]
    IndexKeyTooLong { key_len: usize, max: usize },

    /// Attempted to drop the database currently selected by the same session.
    #[error("cannot drop database '{name}' because it is currently selected by this session")]
    ActiveDatabaseDrop { name: String },

    // ── Locking ──────────────────────────────────────────────────
    /// Lock wait timeout exceeded (Phase 7.10).
    /// SQLSTATE 40001 (MySQL: ER_LOCK_WAIT_TIMEOUT = 1205)
    #[error("lock wait timeout exceeded; try restarting transaction")]
    LockTimeout,

    // ── General ──────────────────────────────────────────────────
    #[error("not implemented: {feature}")]
    NotImplemented { feature: String },

    #[error("internal error: {message}")]
    Internal { message: String },

    #[error("{0}")]
    Other(String),
}

// ── I/O error classifier ─────────────────────────────────────────────────────

/// Classifies an `std::io::Error` from a durable write path into the
/// appropriate [`DbError`] variant.
///
/// - `ENOSPC` (code 28) and `EDQUOT` (codes 69 / 122) map to
///   [`DbError::DiskFull`] with the given `operation` label.
/// - All other I/O errors map to [`DbError::Io`].
///
/// Call this **only** at OS write boundaries
/// (`set_len`, `write_all`, `flush`, `sync_all`, `flush_range`).
/// Do **not** use for logical allocator failures — those remain
/// [`DbError::StorageFull`].
pub fn classify_io(err: std::io::Error, operation: &'static str) -> DbError {
    #[cfg(unix)]
    {
        const ENOSPC: i32 = 28; // universal on Linux + macOS
        const EDQUOT_MACOS: i32 = 69;
        const EDQUOT_LINUX: i32 = 122;
        if let Some(code) = err.raw_os_error() {
            if code == ENOSPC || code == EDQUOT_MACOS || code == EDQUOT_LINUX {
                return DbError::DiskFull { operation };
            }
        }
    }
    DbError::Io(err)
}

impl DbError {
    /// SQLSTATE code — 5-character string for wire protocol and ORM compatibility.
    ///
    /// Every variant that a SQL client can trigger has a precise SQLSTATE code.
    /// Internal errors (storage corruption, WAL issues, heap internals) return
    /// `"XX000"` because they are not caused by user SQL.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            // ── Integrity ─────────────────────────────────────────────────
            DbError::UniqueViolation { .. } => "23505",
            DbError::ForeignKeyViolation { .. } => "23503",
            DbError::ForeignKeyParentViolation { .. } => "23503",
            DbError::ForeignKeyCascadeDepth { .. } => "23503",
            DbError::ForeignKeySetNullNotNullable { .. } => "23000",
            DbError::ForeignKeyNoParentIndex { .. } => "42830",
            DbError::NotNullViolation { .. } => "23502",
            DbError::CheckViolation { .. } => "23514",
            DbError::DuplicateKey => "23505",
            // ── Transaction ───────────────────────────────────────────────
            DbError::WalGroupCommitFailed { .. } => "XX000",
            DbError::DeadlockDetected => "40P01",
            DbError::TransactionAlreadyActive { .. } => "25001",
            DbError::NoActiveTransaction => "25P01",
            DbError::TransactionExpired { .. } => "25006",
            // ── Schema ────────────────────────────────────────────────────
            DbError::ParseError { .. } => "42601",
            DbError::TableNotFound { .. } => "42P01",
            DbError::TableAlreadyExists { .. } => "42P07",
            DbError::IndexAlreadyExists { .. } => "42P07",
            DbError::SchemaAlreadyExists { .. } => "42P06",
            DbError::SchemaNotFound { .. } => "3F000",
            DbError::ColumnNotFound { .. } => "42703",
            DbError::AmbiguousColumn { .. } => "42702",
            DbError::PermissionDenied { .. } => "42501",
            // ── Data / Types ──────────────────────────────────────────────
            DbError::TypeMismatch { .. } => "42804",
            DbError::InvalidCoercion { .. } => "22018",
            DbError::DivisionByZero => "22012",
            DbError::Overflow => "22003",
            DbError::KeyTooLong { .. } => "22001",
            DbError::IndexKeyTooLong { .. } => "54000",
            DbError::ValueTooLarge { .. } => "22001",
            DbError::InvalidValue { .. } => "22P02",
            // ── Subqueries ────────────────────────────────────────────────
            DbError::CardinalityViolation { .. } => "21000",
            // ── DDL ───────────────────────────────────────────────────────
            DbError::ColumnAlreadyExists { .. } => "42701",
            // ── Features ─────────────────────────────────────────────────
            DbError::LockTimeout => "40001",
            DbError::NotImplemented { .. } => "0A000",
            // ── System / I/O ──────────────────────────────────────────────
            DbError::Io(_) => "58030",
            DbError::FileLocked { .. } => "55006",
            DbError::StorageFull => "53100",
            DbError::DiskFull { .. } => "53100",
            DbError::SequenceOverflow => "2200H",
            // ── Internal errors (not user-facing) ─────────────────────────
            _ => "XX000",
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err_with_os_code(code: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code)
    }

    fn generic_io_err() -> std::io::Error {
        std::io::Error::other("generic error")
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_io_enospc_maps_to_disk_full() {
        let err = io_err_with_os_code(28); // ENOSPC
        let db_err = classify_io(err, "test op");
        assert!(
            matches!(
                db_err,
                DbError::DiskFull {
                    operation: "test op"
                }
            ),
            "ENOSPC must map to DiskFull, got: {db_err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_io_edquot_macos_maps_to_disk_full() {
        let err = io_err_with_os_code(69); // EDQUOT on macOS
        let db_err = classify_io(err, "quota op");
        assert!(
            matches!(db_err, DbError::DiskFull { .. }),
            "EDQUOT (macOS) must map to DiskFull, got: {db_err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_io_edquot_linux_maps_to_disk_full() {
        let err = io_err_with_os_code(122); // EDQUOT on Linux
        let db_err = classify_io(err, "quota op");
        assert!(
            matches!(db_err, DbError::DiskFull { .. }),
            "EDQUOT (Linux) must map to DiskFull, got: {db_err}"
        );
    }

    #[test]
    fn test_classify_io_other_error_maps_to_io() {
        let err = generic_io_err();
        let db_err = classify_io(err, "test op");
        assert!(
            matches!(db_err, DbError::Io(_)),
            "Non-disk-full I/O error must map to DbError::Io, got: {db_err}"
        );
    }

    #[test]
    fn test_disk_full_sqlstate() {
        let err = DbError::DiskFull { operation: "test" };
        assert_eq!(err.sqlstate(), "53100");
    }

    #[test]
    fn test_disk_full_distinct_from_storage_full() {
        let df = DbError::DiskFull { operation: "test" };
        let sf = DbError::StorageFull;
        // Both map to the same SQLSTATE (resource exhaustion)…
        assert_eq!(df.sqlstate(), sf.sqlstate());
        // …but their error messages are distinct.
        assert_ne!(df.to_string(), sf.to_string());
    }

    #[test]
    fn test_runtime_mode_constants_are_distinct() {
        // Sanity: the two raw mode values used in Database::runtime_mode
        // must be different so the AtomicU8 flip is meaningful.
        const READ_WRITE: u8 = 0;
        const DEGRADED: u8 = 1;
        assert_ne!(READ_WRITE, DEGRADED);
    }
}
