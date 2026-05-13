// ── SortOrder ─────────────────────────────────────────────────────────────────

/// Sort direction for an index column.
///
/// Used in [`IndexColumnDef`] to indicate whether a column is indexed
/// in ascending or descending order.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc = 0,
    Desc = 1,
}

impl TryFrom<u8> for SortOrder {
    type Error = DbError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Asc),
            1 => Ok(Self::Desc),
            _ => Err(DbError::ParseError {
                message: format!("unknown SortOrder discriminant: {v}"),
                position: None,
            }),
        }
    }
}

// ── IndexColumnDef ────────────────────────────────────────────────────────────

/// One column entry within an [`IndexDef`].
///
/// Records which column position (`col_idx`) is part of the index key
/// and in which sort direction.
///
/// ## On-disk format (backward-compatible)
/// - Without expression: `[col_idx:2 LE][order:1]` (3 bytes — old format)
/// - With expression:    `[col_idx:2 LE][order:1][expr_len:2 LE][expr_sql]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnDef {
    /// Position of this column in the table (matches `ColumnDef.col_idx`).
    pub col_idx: u16,
    /// Sort direction for this column in the index key.
    pub order: SortOrder,
    /// Optional SQL expression for expression indexes.
    ///
    /// Example: `Some("LOWER(email)")` for `CREATE INDEX ON t(LOWER(email))`.
    /// `None` = ordinary column index.
    pub expr: Option<String>,
}

// ── Public type aliases ───────────────────────────────────────────────────────

/// Unique identifier for a table in the catalog. `0` is reserved (invalid).
pub type TableId = u32;

/// Default logical database used for pre-22b.3a catalogs and unbound tables.
pub const DEFAULT_DATABASE_NAME: &str = "axiomdb";

// ── DatabaseDef ──────────────────────────────────────────────────────────────

/// Metadata for a logical database — one row in `axiom_databases`.
///
/// ## On-disk format
/// v0: `[name_len:1][name UTF-8]`
/// v1: v0 + `[collation_len:1][collation UTF-8]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDef {
    pub name: String,
    pub default_collation: Option<String>,
}

impl DatabaseDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let collation = self.default_collation.as_deref().unwrap_or("").as_bytes();
        debug_assert!(name.len() <= 255, "database name too long");
        debug_assert!(collation.len() <= 255, "database collation too long");
        let mut buf = Vec::with_capacity(2 + name.len() + collation.len());
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(collation.len() as u8);
        buf.extend_from_slice(collation);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated DatabaseRow bytes".into(),
            position: None,
        };
        if bytes.is_empty() {
            return Err(err());
        }
        let len = bytes[0] as usize;
        if bytes.len() < 1 + len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[1..1 + len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in database name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = 1 + len;
        let default_collation = if bytes.len() > consumed {
            let coll_len = bytes[consumed] as usize;
            consumed += 1;
            if bytes.len() < consumed + coll_len {
                return Err(err());
            }
            let value = std::str::from_utf8(&bytes[consumed..consumed + coll_len])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in database collation".into(),
                    position: None,
                })?
                .to_string();
            consumed += coll_len;
            if value.is_empty() { None } else { Some(value) }
        } else {
            None
        };
        Ok((
            Self {
                name,
                default_collation,
            },
            consumed,
        ))
    }
}

// ── TableDatabaseDef ─────────────────────────────────────────────────────────

/// Ownership binding from a table to its logical database.
///
/// Missing rows imply legacy ownership by [`DEFAULT_DATABASE_NAME`].
///
/// ## On-disk format
/// `[table_id:4 LE][name_len:1][name UTF-8]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDatabaseDef {
    pub table_id: TableId,
    pub database_name: String,
}

impl TableDatabaseDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.database_name.as_bytes();
        debug_assert!(name.len() <= 255, "database name too long");
        let mut buf = Vec::with_capacity(4 + 1 + name.len());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated TableDatabaseRow bytes".into(),
            position: None,
        };
        if bytes.len() < 5 {
            return Err(err());
        }
        let table_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let len = bytes[4] as usize;
        if bytes.len() < 5 + len {
            return Err(err());
        }
        let database_name = std::str::from_utf8(&bytes[5..5 + len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in table database name".into(),
                position: None,
            })?
            .to_string();
        Ok((
            Self {
                table_id,
                database_name,
            },
            5 + len,
        ))
    }
}

// ── SchemaDef ─────────────────────────────────────────────────────────────────

/// Metadata for a logical schema — one row in `axiom_schemas` (Phase 22b.4).
///
/// Schemas are scoped to a logical database.
///
/// ## On-disk format
/// `[db_len:1][db UTF-8][name_len:1][name UTF-8]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDef {
    pub database_name: String,
    pub name: String,
}

impl SchemaDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let db = self.database_name.as_bytes();
        let name = self.name.as_bytes();
        debug_assert!(db.len() <= 255, "database name too long");
        debug_assert!(name.len() <= 255, "schema name too long");
        let mut buf = Vec::with_capacity(2 + db.len() + name.len());
        buf.push(db.len() as u8);
        buf.extend_from_slice(db);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated SchemaDef bytes".into(),
            position: None,
        };
        if bytes.is_empty() {
            return Err(err());
        }
        let db_len = bytes[0] as usize;
        if bytes.len() < 1 + db_len + 1 {
            return Err(err());
        }
        let database_name = std::str::from_utf8(&bytes[1..1 + db_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in schema database name".into(),
                position: None,
            })?
            .to_string();
        let name_len = bytes[1 + db_len] as usize;
        if bytes.len() < 2 + db_len + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[2 + db_len..2 + db_len + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in schema name".into(),
                position: None,
            })?
            .to_string();
        Ok((
            Self {
                database_name,
                name,
            },
            2 + db_len + name_len,
        ))
    }
}

// ── ColumnType ────────────────────────────────────────────────────────────────

/// SQL column type stored in the catalog.
///
/// A subset of the full `DataType` enum sufficient for Phase 3-4.
/// Extended in later phases as new types are supported.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool = 1,
    Int = 2,    // i32
    BigInt = 3, // i64
    Float = 4,  // f64
    Text = 5,
    Bytes = 6,
    Timestamp = 7, // i64 microseconds since UTC epoch
    Uuid = 8,      // [u8; 16]
    Json = 9,      // validated UTF-8 JSON text (Phase 11.4)
    Jsonb = 10,    // binary JSONB blob (Phase 11.16)
    Decimal = 11,  // i128 mantissa + u8 scale
    Date = 12,     // i32 days since 1970-01-01
    Array = 13,    // PostgreSQL array (Phase 20.4)
}

impl TryFrom<u8> for ColumnType {
    type Error = DbError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Bool),
            2 => Ok(Self::Int),
            3 => Ok(Self::BigInt),
            4 => Ok(Self::Float),
            5 => Ok(Self::Text),
            6 => Ok(Self::Bytes),
            7 => Ok(Self::Timestamp),
            8 => Ok(Self::Uuid),
            9 => Ok(Self::Json),
            10 => Ok(Self::Jsonb),
            11 => Ok(Self::Decimal),
            12 => Ok(Self::Date),
            13 => Ok(Self::Array),
            _ => Err(DbError::ParseError {
                message: format!("unknown ColumnType discriminant: {v}"),
                position: None,
            }),
        }
    }
}

impl From<ColumnType> for u8 {
    fn from(c: ColumnType) -> u8 {
        c as u8
    }
}

// ── TableStorageLayout ────────────────────────────────────────────────────────

/// Physical storage layout for a table's primary row store.
///
/// `Heap` is the legacy heap-chain layout.
/// `Clustered` means the table rows live directly in the clustered primary-key tree.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableStorageLayout {
    #[default]
    Heap = 0,
    Clustered = 1,
}

impl TryFrom<u8> for TableStorageLayout {
    type Error = DbError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Heap),
            1 => Ok(Self::Clustered),
            _ => Err(DbError::ParseError {
                message: format!("unknown TableStorageLayout discriminant: {v}"),
                position: None,
            }),
        }
    }
}

impl From<TableStorageLayout> for u8 {
    fn from(layout: TableStorageLayout) -> u8 {
        layout as u8
    }
}
