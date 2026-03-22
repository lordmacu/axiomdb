use thiserror::Error;

/// Error central de NexusDB.
/// Todos los crates retornan este tipo (o lo wrappean).
#[derive(Debug, Error)]
pub enum DbError {
    // ── Storage ──────────────────────────────────────────────────
    #[error("página {page_id} no encontrada")]
    PageNotFound { page_id: u64 },

    #[error(
        "checksum inválido en página {page_id}: esperado {expected:#010x}, obtenido {got:#010x}"
    )]
    ChecksumMismatch {
        page_id: u64,
        expected: u32,
        got: u32,
    },

    #[error("storage lleno: no hay páginas libres")]
    StorageFull,

    // ── I/O ──────────────────────────────────────────────────────
    #[error("error de I/O: {0}")]
    Io(#[from] std::io::Error),

    // ── SQL ──────────────────────────────────────────────────────
    #[error("error de sintaxis SQL: {message}")]
    ParseError { message: String },

    #[error("tabla '{name}' no encontrada")]
    TableNotFound { name: String },

    #[error("columna '{name}' no encontrada en tabla '{table}'")]
    ColumnNotFound { name: String, table: String },

    // ── Integridad ───────────────────────────────────────────────
    #[error("violación de clave única en {table}.{column}")]
    UniqueViolation { table: String, column: String },

    #[error("violación de clave foránea: {table}.{column} = {value}")]
    ForeignKeyViolation {
        table: String,
        column: String,
        value: String,
    },

    #[error("violación NOT NULL en {table}.{column}")]
    NotNullViolation { table: String, column: String },

    #[error("violación de CHECK en {table}.{constraint}")]
    CheckViolation { table: String, constraint: String },

    // ── Transacciones ────────────────────────────────────────────
    #[error("deadlock detectado entre transacciones")]
    DeadlockDetected,

    #[error("transacción {txn_id} ya no es válida")]
    TransactionExpired { txn_id: u64 },

    // ── Permisos ─────────────────────────────────────────────────
    #[error("permiso denegado: {action} en {object}")]
    PermissionDenied { action: String, object: String },

    // ── Tipos ────────────────────────────────────────────────────
    #[error("tipo incorrecto: se esperaba {expected}, se obtuvo {got}")]
    TypeMismatch { expected: String, got: String },

    // ── B+ Tree ───────────────────────────────────────────────────
    #[error("key demasiado larga: {len} bytes (máximo {max})")]
    KeyTooLong { len: usize, max: usize },

    #[error("clave duplicada en índice")]
    DuplicateKey,

    #[error("árbol B+ corrupto: {msg}")]
    BTreeCorrupted { msg: String },

    // ── General ──────────────────────────────────────────────────
    #[error("no implementado: {feature}")]
    NotImplemented { feature: String },

    #[error("{0}")]
    Other(String),
}

impl DbError {
    /// SQLSTATE code — para compatibilidad con ORMs y clientes SQL.
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
            _ => "XX000",
        }
    }
}
