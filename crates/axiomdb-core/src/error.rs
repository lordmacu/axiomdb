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

    // ── I/O ──────────────────────────────────────────────────────
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cannot open '{path}': another process already holds the file lock")]
    FileLocked { path: std::path::PathBuf },

    // ── SQL ──────────────────────────────────────────────────────
    #[error("SQL syntax error: {message}")]
    ParseError { message: String },

    #[error("table '{name}' not found")]
    TableNotFound { name: String },

    #[error("column '{name}' not found in table '{table}'")]
    ColumnNotFound { name: String, table: String },

    // ── Integrity ────────────────────────────────────────────────
    #[error("unique key violation on {table}.{column}")]
    UniqueViolation { table: String, column: String },

    #[error("foreign key violation: {table}.{column} = {value}")]
    ForeignKeyViolation {
        table: String,
        column: String,
        value: String,
    },

    #[error("NOT NULL violation on {table}.{column}")]
    NotNullViolation { table: String, column: String },

    #[error("CHECK constraint violation on {table}.{constraint}")]
    CheckViolation { table: String, constraint: String },

    // ── WAL ──────────────────────────────────────────────────────
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

    // ── General ──────────────────────────────────────────────────
    #[error("not implemented: {feature}")]
    NotImplemented { feature: String },

    #[error("internal error: {message}")]
    Internal { message: String },

    #[error("{0}")]
    Other(String),
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
            DbError::NotNullViolation { .. } => "23502",
            DbError::CheckViolation { .. } => "23514",
            DbError::DuplicateKey => "23505",
            // ── Transaction ───────────────────────────────────────────────
            DbError::DeadlockDetected => "40P01",
            DbError::TransactionAlreadyActive { .. } => "25001",
            DbError::NoActiveTransaction => "25P01",
            DbError::TransactionExpired { .. } => "25006",
            // ── Schema ────────────────────────────────────────────────────
            DbError::ParseError { .. } => "42601",
            DbError::TableNotFound { .. } => "42P01",
            DbError::TableAlreadyExists { .. } => "42P07",
            DbError::ColumnNotFound { .. } => "42703",
            DbError::AmbiguousColumn { .. } => "42702",
            DbError::PermissionDenied { .. } => "42501",
            // ── Data / Types ──────────────────────────────────────────────
            DbError::TypeMismatch { .. } => "42804",
            DbError::InvalidCoercion { .. } => "22018",
            DbError::DivisionByZero => "22012",
            DbError::Overflow => "22003",
            DbError::KeyTooLong { .. } => "22001",
            DbError::ValueTooLarge { .. } => "22001",
            DbError::InvalidValue { .. } => "22P02",
            // ── Subqueries ────────────────────────────────────────────────
            DbError::CardinalityViolation { .. } => "21000",
            // ── Features ─────────────────────────────────────────────────
            DbError::NotImplemented { .. } => "0A000",
            // ── System / I/O ──────────────────────────────────────────────
            DbError::Io(_) => "58030",
            DbError::FileLocked { .. } => "55006",
            DbError::StorageFull => "53100",
            DbError::SequenceOverflow => "2200H",
            // ── Internal errors (not user-facing) ─────────────────────────
            _ => "XX000",
        }
    }
}
