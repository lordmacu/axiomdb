use thiserror::Error;

/// Central error type for NexusDB.
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

    // ── General ──────────────────────────────────────────────────
    #[error("not implemented: {feature}")]
    NotImplemented { feature: String },

    #[error("{0}")]
    Other(String),
}

impl DbError {
    /// SQLSTATE code — for compatibility with ORMs and SQL clients.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            DbError::UniqueViolation { .. } => "23505",
            DbError::ForeignKeyViolation { .. } => "23503",
            DbError::NotNullViolation { .. } => "23502",
            DbError::CheckViolation { .. } => "23514",
            DbError::DeadlockDetected => "40P01",
            DbError::ParseError { .. } => "42601",
            DbError::TableNotFound { .. } => "42P01",
            DbError::ColumnNotFound { .. } => "42703",
            DbError::PermissionDenied { .. } => "42501",
            DbError::TypeMismatch { .. } => "42804",
            DbError::Io(_) => "58030",
            DbError::FileLocked { .. } => "55006", // object_in_use
            _ => "XX000",
        }
    }
}
