//! Catalog schema types and their binary serialization.
//!
//! These types represent rows in the system tables:
//! - `axiom_tables`      → [`TableDef`]
//! - `axiom_columns`     → [`ColumnDef`]
//! - `axiom_indexes`     → [`IndexDef`]
//! - `axiom_constraints` → [`ConstraintDef`] (Phase 4.22b)
//! - `axiom_databases`   → [`DatabaseDef`] (Phase 22b.3a)
//! - `axiom_table_databases` → [`TableDatabaseDef`] (Phase 22b.3a)
//!
//! ## Binary row format
//!
//! Each type has a compact, length-prefixed binary format for storage in heap
//! slots. All multi-byte integers are little-endian. String names are stored as
//! a 1-byte length prefix followed by the UTF-8 bytes (max 255 bytes per name).
//!
//! **TableRow**: `[table_id:4][schema_len:1][schema bytes][name_len:1][name bytes]`
//!
//! **ColumnRow**: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
//! - `flags bit0` = nullable
//!
//! **IndexRow**: `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
//! - `flags bit0` = unique, `flags bit1` = primary key

use axiomdb_core::error::DbError;

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
/// ## On-disk format (3 bytes per entry)
/// `[col_idx:2 LE][order:1]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnDef {
    /// Position of this column in the table (matches `ColumnDef.col_idx`).
    pub col_idx: u16,
    /// Sort direction for this column in the index key.
    pub order: SortOrder,
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
/// `[name_len:1][name UTF-8]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDef {
    pub name: String,
}

impl DatabaseDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "database name too long");
        let mut buf = Vec::with_capacity(1 + name.len());
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
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
        Ok((Self { name }, 1 + len))
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

// ── TableDef ──────────────────────────────────────────────────────────────────

/// Metadata for a user table — one row in `axiom_tables`.
///
/// ## On-disk format (`to_bytes` / `from_bytes`)
///
/// ```text
/// legacy:
///   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8]
///
/// current:
///   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8][layout:1]
/// ```
///
/// Legacy rows without the trailing `layout` byte decode as [`TableStorageLayout::Heap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub id: TableId,
    /// Root page of the table's primary row store.
    ///
    /// For heap tables this points to the `HeapChain` root.
    /// For clustered tables this points to the clustered leaf/internal root.
    /// Must never be 0 (page 0 is the meta page).
    pub root_page_id: u64,
    pub storage_layout: TableStorageLayout,
    pub schema_name: String,
    pub table_name: String,
}

impl TableDef {
    pub fn is_heap(&self) -> bool {
        self.storage_layout == TableStorageLayout::Heap
    }

    pub fn is_clustered(&self) -> bool {
        self.storage_layout == TableStorageLayout::Clustered
    }

    pub fn ensure_heap_runtime(&self, feature: &str) -> Result<(), DbError> {
        if self.is_clustered() {
            return Err(DbError::NotImplemented {
                feature: feature.to_string(),
            });
        }
        Ok(())
    }

    /// Serializes to binary row format.
    ///
    /// Format:
    /// `[table_id:4][root_page_id:8][schema_len:1][schema bytes][name_len:1][name bytes][layout:1]`
    ///
    /// # Panics (debug only)
    /// If `schema_name` or `table_name` exceeds 255 bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.table_name.as_bytes();
        debug_assert!(schema.len() <= 255, "schema_name too long");
        debug_assert!(name.len() <= 255, "table_name too long");

        let mut buf = Vec::with_capacity(4 + 8 + 1 + schema.len() + 1 + name.len() + 1);
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.extend_from_slice(&self.root_page_id.to_le_bytes());
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(self.storage_layout.into());
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Returns `(TableDef, bytes_consumed)` on success.
    ///
    /// # Errors
    /// - [`DbError::ParseError`] if `bytes` is too short or contains invalid UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated TableRow bytes".into(),
            position: None,
        };

        // Minimum: 4 (id) + 8 (root_page_id) + 1 (schema_len) + 0 + 1 (name_len) + 0 = 14
        if bytes.len() < 14 {
            return Err(err());
        }

        let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let root_page_id = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        let schema_len = bytes[12] as usize;
        let pos = 13;

        if bytes.len() < pos + schema_len + 1 {
            return Err(err());
        }
        let schema_name = std::str::from_utf8(&bytes[pos..pos + schema_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in schema_name".into(),
                position: None,
            })?
            .to_string();
        let pos = pos + schema_len;

        let name_len = bytes[pos] as usize;
        let pos = pos + 1;
        if bytes.len() < pos + name_len {
            return Err(err());
        }
        let table_name = std::str::from_utf8(&bytes[pos..pos + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in table_name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = pos + name_len;
        let storage_layout = match bytes.len() {
            len if len == consumed => TableStorageLayout::Heap,
            len if len == consumed + 1 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                layout
            }
            _ => {
                return Err(DbError::ParseError {
                    message: "unexpected trailing bytes in TableRow".into(),
                    position: None,
                })
            }
        };

        Ok((
            Self {
                id,
                root_page_id,
                storage_layout,
                schema_name,
                table_name,
            },
            consumed,
        ))
    }
}

// ── ColumnDef ─────────────────────────────────────────────────────────────────

/// Metadata for a single column — one row in `axiom_columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub table_id: TableId,
    pub col_idx: u16,
    pub name: String,
    pub col_type: ColumnType,
    pub nullable: bool,
    /// `true` if this column was declared `AUTO_INCREMENT` or `SERIAL`.
    pub auto_increment: bool,
}

impl ColumnDef {
    /// Serializes to binary row format.
    ///
    /// Format: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
    /// - `flags bit0` = nullable
    /// - `flags bit1` = auto_increment
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "column name too long");

        let mut flags: u8 = 0;
        if self.nullable {
            flags |= 0x01;
        }
        if self.auto_increment {
            flags |= 0x02;
        }

        let mut buf = Vec::with_capacity(4 + 2 + 1 + 1 + 1 + name.len());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.col_idx.to_le_bytes());
        buf.push(u8::from(self.col_type));
        buf.push(flags);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Backward-compatible: bit1 of flags was always 0 in older rows,
    /// so `auto_increment` defaults to `false` for pre-4.14 catalog data.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ColumnRow bytes".into(),
            position: None,
        };

        if bytes.len() < 9 {
            return Err(err());
        }

        let table_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let col_idx = u16::from_le_bytes([bytes[4], bytes[5]]);
        let col_type = ColumnType::try_from(bytes[6])?;
        let flags = bytes[7];
        let nullable = flags & 0x01 != 0;
        let auto_increment = flags & 0x02 != 0;
        let name_len = bytes[8] as usize;

        if bytes.len() < 9 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[9..9 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in column name".into(),
                position: None,
            })?
            .to_string();
        let consumed = 9 + name_len;

        Ok((
            Self {
                table_id,
                col_idx,
                name,
                col_type,
                nullable,
                auto_increment,
            },
            consumed,
        ))
    }
}

// ── IndexDef ──────────────────────────────────────────────────────────────────

/// Metadata for an index — one row in `axiom_indexes`.
///
/// ## On-disk format
///
/// ```text
/// [index_id:4 LE][table_id:4 LE][root_page_id:8 LE][flags:1][name_len:1][name bytes]
/// [ncols:1][col_idx:2 LE, order:1]×ncols
/// [pred_len:2 LE][pred_sql: pred_len UTF-8 bytes]   ← Phase 6.7; omitted if predicate = None
/// ```
///
/// The `columns` section (starting at `ncols`) is optional — old rows that
/// predate Phase 6.1 end after `name bytes`.  `from_bytes` returns
/// `columns: vec![]` for such rows (backward-compatible).
///
/// The `pred_len` section is optional — old rows without it get `predicate = None`
/// (backward-compatible with pre-6.7 databases).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// Auto-incremented unique ID, allocated by `CatalogWriter::create_index`.
    /// `0` is reserved (invalid / not yet assigned).
    pub index_id: u32,
    pub table_id: TableId,
    pub name: String,
    pub root_page_id: u64,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Ordered list of columns that form the index key.
    ///
    /// Empty for indexes created before Phase 6.1 (treated as unusable by planner).
    pub columns: Vec<IndexColumnDef>,
    /// WHERE predicate as a SQL expression string (Phase 6.7).
    ///
    /// `None` = full index (covers all rows).
    /// `Some(sql)` = partial index; only rows satisfying the predicate are indexed.
    ///
    /// Example: `Some("deleted_at IS NULL")` or `Some("status = 'active'")`.
    pub predicate: Option<String>,
    /// Target leaf-page fill factor in percent (Phase 6.8).
    ///
    /// Controls when a leaf page splits during INSERT. A leaf page with
    /// `num_keys >= ceil(ORDER_LEAF × fillfactor / 100)` triggers a split,
    /// leaving the specified percentage of capacity used and `100 - fillfactor`
    /// free for future inserts without another split.
    ///
    /// Valid range: 10–100. Default: 90 (matches PostgreSQL `BTREE_DEFAULT_FILLFACTOR`).
    /// `fillfactor = 100` → pages fill completely before splitting (current behavior).
    ///
    /// Stored as 1 byte after the predicate section. Absent in pre-6.8 rows;
    /// `from_bytes` returns 90 for backward compatibility.
    pub fillfactor: u8,
    /// `true` for FK auto-indexes that use composite keys `(fk_val | RecordId)`.
    ///
    /// Allows multiple rows with the same FK value (InnoDB approach: append 10
    /// RecordId bytes to the key to guarantee global uniqueness in the B-Tree).
    ///
    /// Stored in `flags bit 2` (`0x04`). Pre-6.9 rows have bit 2 = 0 → `false`.
    pub is_fk_index: bool,
    /// INCLUDE columns for covering index scans (Phase 6.13).
    ///
    /// Catalog-only metadata in Phase 6.13. B-Tree leaf storage of these values
    /// is planned for Phase 6.15.
    ///
    /// Stored after `is_fk_index`: `[include_len: 1 byte][col_idx: u16 LE] × len`.
    /// Absent in pre-6.13 rows → `include_columns = vec![]`.
    pub include_columns: Vec<u16>,
    /// Index type (Phase 11.1b): 0 = BTree (default), 1 = Brin.
    ///
    /// Stored as 1 byte after include_columns. Pre-11.1b rows default to 0 (BTree).
    pub index_type: u8,
    /// BRIN: heap pages per summary range (Phase 11.1b).
    ///
    /// Default 128. Only meaningful when `index_type == 1`.
    /// Stored as 4 bytes LE after index_type, only when `index_type != 0`.
    pub pages_per_range: u32,
}

impl IndexDef {
    /// Serializes to binary row format.
    ///
    /// Format:
    /// `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
    /// `[ncols:1][col_idx:2 LE, order:1]×ncols`
    /// `[pred_len:2 LE][pred_sql bytes]` — only present if predicate is Some
    ///
    /// - `flags bit0` = unique, `flags bit1` = primary key, `flags bit2` = fk_index
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "index name too long");
        debug_assert!(self.columns.len() <= 63, "too many index columns");

        let mut flags: u8 = 0;
        if self.is_unique {
            flags |= 0x01;
        }
        if self.is_primary {
            flags |= 0x02;
        }
        if self.is_fk_index {
            flags |= 0x04;
        }

        let pred_bytes = self.predicate.as_deref().map(str::as_bytes);
        let pred_len = pred_bytes.map(|b| b.len()).unwrap_or(0);

        let mut buf = Vec::with_capacity(
            4 + 4
                + 8
                + 1
                + 1
                + name.len()
                + 1
                + self.columns.len() * 3
                + if pred_len > 0 { 2 + pred_len } else { 0 },
        );
        buf.extend_from_slice(&self.index_id.to_le_bytes());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.root_page_id.to_le_bytes());
        buf.push(flags);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        // Columns section
        buf.push(self.columns.len() as u8);
        for col in &self.columns {
            buf.extend_from_slice(&col.col_idx.to_le_bytes());
            buf.push(col.order as u8);
        }
        // Predicate section (Phase 6.7) — always write pred_len u16 (0 if no predicate).
        // Pre-6.7 rows have no pred_len bytes; from_bytes skips when < 2 bytes remain.
        buf.extend_from_slice(&(pred_len as u16).to_le_bytes());
        if let Some(pred) = pred_bytes {
            buf.extend_from_slice(pred);
        }
        // Fill factor (Phase 6.8) — always written as 1 byte after predicate.
        // Pre-6.8 readers stop before this byte and use the default of 90.
        buf.push(self.fillfactor);
        // Include columns (Phase 6.13) — after fillfactor, always written.
        buf.push(self.include_columns.len() as u8);
        for &col_idx in &self.include_columns {
            buf.extend_from_slice(&col_idx.to_le_bytes());
        }
        // Index type (Phase 11.1b) — 1 byte: 0=BTree, 1=Brin.
        buf.push(self.index_type);
        // BRIN pages_per_range — 4 bytes LE, only when index_type != 0.
        if self.index_type != 0 {
            buf.extend_from_slice(&self.pages_per_range.to_le_bytes());
        }
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Backward-compatible: if the `ncols` byte is absent (old row format), returns `columns: vec![]`.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated IndexRow bytes".into(),
            position: None,
        };

        // Fixed header: 4 (index_id) + 4 (table_id) + 8 (root_page_id) + 1 (flags) + 1 (name_len) = 18
        if bytes.len() < 18 {
            return Err(err());
        }

        let index_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let table_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let root_page_id = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let flags = bytes[16];
        let is_unique = flags & 0x01 != 0;
        let is_primary = flags & 0x02 != 0;
        let is_fk_index = flags & 0x04 != 0; // Phase 6.9
        let name_len = bytes[17] as usize;

        if bytes.len() < 18 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[18..18 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in index name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = 18 + name_len;

        // Backward-compatible: columns section is optional
        let columns = if bytes.len() > consumed {
            let ncols = bytes[consumed] as usize;
            consumed += 1;
            let mut cols = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                if bytes.len() < consumed + 3 {
                    return Err(err());
                }
                let col_idx = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]);
                let order = SortOrder::try_from(bytes[consumed + 2])?;
                consumed += 3;
                cols.push(IndexColumnDef { col_idx, order });
            }
            cols
        } else {
            vec![]
        };

        // Predicate section (Phase 6.7) — backward-compatible:
        // only present when 2+ bytes remain after the columns section.
        let predicate = if bytes.len() >= consumed + 2 {
            let pred_len = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if pred_len == 0 {
                None
            } else {
                if bytes.len() < consumed + pred_len {
                    return Err(DbError::ParseError {
                        message: "IndexDef predicate section truncated".into(),
                        position: None,
                    });
                }
                let sql = std::str::from_utf8(&bytes[consumed..consumed + pred_len])
                    .map_err(|_| DbError::ParseError {
                        message: "IndexDef predicate not valid UTF-8".into(),
                        position: None,
                    })?
                    .to_string();
                consumed += pred_len;
                Some(sql)
            }
        } else {
            None // pre-6.7 row — no predicate bytes
        };

        // Fill factor (Phase 6.8) — 1 byte after predicate. Pre-6.8 rows lack
        // this byte; default to 90 (PostgreSQL BTREE_DEFAULT_FILLFACTOR).
        let fillfactor = if bytes.len() > consumed {
            let ff = bytes[consumed];
            consumed += 1;
            ff
        } else {
            90 // default for pre-6.8 rows
        };

        // Include columns (Phase 6.13) — [include_len: 1][col_idx: u16 LE] × len.
        // Absent in pre-6.13 rows → empty vec.
        let include_columns = if bytes.len() > consumed {
            let include_len = bytes[consumed] as usize;
            consumed += 1;
            let mut cols = Vec::with_capacity(include_len);
            for _ in 0..include_len {
                if bytes.len() < consumed + 2 {
                    return Err(DbError::ParseError {
                        message: "IndexDef include_columns truncated".into(),
                        position: None,
                    });
                }
                let col_idx = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]);
                consumed += 2;
                cols.push(col_idx);
            }
            cols
        } else {
            vec![] // pre-6.13 row
        };

        // Index type (Phase 11.1b) — 1 byte. Pre-11.1b rows default to 0 (BTree).
        let index_type = if bytes.len() > consumed {
            let it = bytes[consumed];
            consumed += 1;
            it
        } else {
            0 // BTree default
        };

        // BRIN pages_per_range — 4 bytes LE, only when index_type != 0.
        let pages_per_range = if index_type != 0 && bytes.len() >= consumed + 4 {
            let ppr = u32::from_le_bytes([
                bytes[consumed],
                bytes[consumed + 1],
                bytes[consumed + 2],
                bytes[consumed + 3],
            ]);
            consumed += 4;
            ppr
        } else {
            128 // default
        };

        Ok((
            Self {
                index_id,
                table_id,
                name,
                root_page_id,
                is_unique,
                is_primary,
                columns,
                predicate,
                fillfactor,
                is_fk_index,
                include_columns,
                index_type,
                pages_per_range,
            },
            consumed,
        ))
    }
}

// ── ConstraintDef ─────────────────────────────────────────────────────────────

/// A row in `axiom_constraints` — a named constraint persisted in the catalog.
///
/// Currently used for CHECK constraints added via `ALTER TABLE ADD CONSTRAINT`.
/// UNIQUE constraints are stored as indexes in `axiom_indexes` instead.
///
/// ## Binary row format
///
/// ```text
/// [constraint_id: u32 LE][table_id: u32 LE]
/// [name_len: u32 LE][name: utf-8 bytes]
/// [expr_len: u32 LE][check_expr: utf-8 bytes]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintDef {
    /// Catalog-allocated monotonic ID.
    pub constraint_id: u32,
    /// Table this constraint belongs to.
    pub table_id: u32,
    /// Constraint name (required — anonymous constraints not supported in ALTER TABLE).
    pub name: String,
    /// SQL expression string for CHECK constraints. Empty for future types.
    pub check_expr: String,
}

impl ConstraintDef {
    /// Serializes this definition to bytes for heap storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let expr_bytes = self.check_expr.as_bytes();
        let mut buf = Vec::with_capacity(4 + 4 + 4 + name_bytes.len() + 4 + expr_bytes.len());
        buf.extend_from_slice(&self.constraint_id.to_le_bytes());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&(expr_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(expr_bytes);
        buf
    }

    /// Deserializes a `ConstraintDef` from a byte slice.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        if data.len() < 8 {
            return Err(DbError::ParseError {
                message: "ConstraintDef row too short".into(),
                position: None,
            });
        }
        let constraint_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let table_id = u32::from_le_bytes(data[4..8].try_into().unwrap());

        let mut pos = 8usize;

        let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let name = String::from_utf8(data[pos..pos + name_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef name not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += name_len;

        let expr_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let check_expr = String::from_utf8(data[pos..pos + expr_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef check_expr not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += expr_len;

        Ok((
            Self {
                constraint_id,
                table_id,
                name,
                check_expr,
            },
            pos,
        ))
    }
}

// ── FkAction ──────────────────────────────────────────────────────────────────

/// The referential action taken when the parent row is deleted or updated.
///
/// Stored as a single byte in the `axiom_foreign_keys` heap.
/// `NoAction` and `Restrict` behave identically in AxiomDB (both enforce
/// immediately — deferred enforcement requires Phase 7 DEFERRABLE support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FkAction {
    /// Default SQL action — error if children exist (same as Restrict here).
    NoAction = 0,
    /// Error immediately if children exist.
    Restrict = 1,
    /// Delete / update child rows automatically.
    Cascade = 2,
    /// Set child FK column to NULL.
    SetNull = 3,
    /// Set child FK column to its DEFAULT value (deferred to Phase 6.9).
    SetDefault = 4,
}

impl TryFrom<u8> for FkAction {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NoAction),
            1 => Ok(Self::Restrict),
            2 => Ok(Self::Cascade),
            3 => Ok(Self::SetNull),
            4 => Ok(Self::SetDefault),
            _ => Err(DbError::ParseError {
                message: format!("unknown FkAction byte: {value}"),
                position: None,
            }),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for FkAction {
    fn default() -> Self {
        Self::NoAction
    }
}

// ── FkDef ─────────────────────────────────────────────────────────────────────

/// A row in `axiom_foreign_keys` — one entry per FK constraint (Phase 6.5).
///
/// Scoped to **single-column** FKs. Composite FK support is deferred to
/// Phase 6.9.
///
/// ## Binary row format
///
/// ```text
/// [fk_id:          4 bytes LE u32]
/// [child_table_id: 4 bytes LE u32]
/// [child_col_idx:  2 bytes LE u16]
/// [parent_table_id:4 bytes LE u32]
/// [parent_col_idx: 2 bytes LE u16]
/// [on_delete:      1 byte  u8   ]
/// [on_update:      1 byte  u8   ]
/// [fk_index_id:    4 bytes LE u32]  — auto-created index on child FK col;
///                                      0 = user-provided, not auto-created
/// [name_len:       4 bytes LE u32]
/// [name:           name_len bytes UTF-8]
/// ```
///
/// Fixed header: 26 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkDef {
    /// Catalog-allocated monotonic ID.
    pub fk_id: u32,
    /// Table that owns the FK column (the "child" / referencing table).
    pub child_table_id: u32,
    /// Column index in the child table that holds the FK value.
    pub child_col_idx: u16,
    /// Table being referenced (the "parent" table).
    pub parent_table_id: u32,
    /// Column index in the parent table that is referenced (must be PK/UNIQUE).
    pub parent_col_idx: u16,
    /// Action when the parent row is deleted.
    pub on_delete: FkAction,
    /// Action when the parent key is updated.
    pub on_update: FkAction,
    /// `index_id` of the B-Tree index auto-created on `child_col_idx`.
    /// `0` means the user already had a suitable index — we did not create one,
    /// and therefore we must NOT drop it when the FK is dropped.
    pub fk_index_id: u32,
    /// Constraint name. Auto-generated as `fk_{child_table}_{col}_{parent_table}`
    /// when not explicitly specified.
    pub name: String,
}

impl FkDef {
    /// Serializes this definition to bytes for heap storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let mut buf = Vec::with_capacity(26 + name_bytes.len());
        buf.extend_from_slice(&self.fk_id.to_le_bytes());
        buf.extend_from_slice(&self.child_table_id.to_le_bytes());
        buf.extend_from_slice(&self.child_col_idx.to_le_bytes());
        buf.extend_from_slice(&self.parent_table_id.to_le_bytes());
        buf.extend_from_slice(&self.parent_col_idx.to_le_bytes());
        buf.push(self.on_delete as u8);
        buf.push(self.on_update as u8);
        buf.extend_from_slice(&self.fk_index_id.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf
    }

    /// Deserializes a `FkDef` from a byte slice.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        const FIXED: usize = 26;
        if data.len() < FIXED {
            return Err(DbError::ParseError {
                message: format!(
                    "FkDef row too short: need {FIXED} bytes, got {}",
                    data.len()
                ),
                position: None,
            });
        }

        let fk_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let child_table_id = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let child_col_idx = u16::from_le_bytes(data[8..10].try_into().unwrap());
        let parent_table_id = u32::from_le_bytes(data[10..14].try_into().unwrap());
        let parent_col_idx = u16::from_le_bytes(data[14..16].try_into().unwrap());
        let on_delete = FkAction::try_from(data[16])?;
        let on_update = FkAction::try_from(data[17])?;
        let fk_index_id = u32::from_le_bytes(data[18..22].try_into().unwrap());
        let name_len = u32::from_le_bytes(data[22..26].try_into().unwrap()) as usize;

        let end = FIXED + name_len;
        if data.len() < end {
            return Err(DbError::ParseError {
                message: format!(
                    "FkDef row truncated: name claims {name_len} bytes but only {} remain",
                    data.len() - FIXED
                ),
                position: None,
            });
        }
        let name =
            String::from_utf8(data[FIXED..end].to_vec()).map_err(|e| DbError::ParseError {
                message: format!("FkDef name not valid UTF-8: {e}"),
                position: None,
            })?;

        Ok((
            Self {
                fk_id,
                child_table_id,
                child_col_idx,
                parent_table_id,
                parent_col_idx,
                on_delete,
                on_update,
                fk_index_id,
                name,
            },
            end,
        ))
    }
}

// ── StatsDef ──────────────────────────────────────────────────────────────────

/// Per-column statistics stored in `axiom_stats` (Phase 6.10).
///
/// Used by the query planner to choose between index scan and full table scan
/// via selectivity estimation: `selectivity = 1.0 / ndv`.
///
/// ## Binary format (22 bytes fixed)
///
/// ```text
/// [table_id:  4 bytes LE u32]
/// [col_idx:   2 bytes LE u16]
/// [row_count: 8 bytes LE u64]  — visible rows at last ANALYZE / CREATE INDEX
/// [ndv:       8 bytes LE i64]  — distinct non-NULL values (PostgreSQL dual-encoding)
/// ```
///
/// `ndv` encoding (same as PostgreSQL `stadistinct`):
/// - `> 0` → absolute distinct count (e.g. 9999 unique emails)
/// - `< 0` → proportion multiplier (reserved; Phase 6.10 always writes > 0)
/// - `= 0` → unknown → planner uses `DEFAULT_NUM_DISTINCT = 200`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsDef {
    pub table_id: u32,
    pub col_idx: u16,
    pub row_count: u64,
    pub ndv: i64,
}

impl StatsDef {
    /// Serializes to the 22-byte binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(22);
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.col_idx.to_le_bytes());
        buf.extend_from_slice(&self.row_count.to_le_bytes());
        buf.extend_from_slice(&self.ndv.to_le_bytes());
        buf
    }

    /// Deserializes from the 22-byte binary format.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        if data.len() < 22 {
            return Err(DbError::ParseError {
                message: format!("StatsDef row too short: need 22 bytes, got {}", data.len()),
                position: None,
            });
        }
        let table_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let col_idx = u16::from_le_bytes(data[4..6].try_into().unwrap());
        let row_count = u64::from_le_bytes(data[6..14].try_into().unwrap());
        let ndv = i64::from_le_bytes(data[14..22].try_into().unwrap());
        Ok((
            Self {
                table_id,
                col_idx,
                row_count,
                ndv,
            },
            22,
        ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ColumnType ────────────────────────────────────────────────────────────

    #[test]
    fn test_column_type_roundtrip_all_variants() {
        let variants = [
            ColumnType::Bool,
            ColumnType::Int,
            ColumnType::BigInt,
            ColumnType::Float,
            ColumnType::Text,
            ColumnType::Bytes,
            ColumnType::Timestamp,
            ColumnType::Uuid,
        ];
        for v in variants {
            let byte: u8 = v.into();
            let back = ColumnType::try_from(byte).expect("roundtrip failed");
            assert_eq!(back, v, "roundtrip failed for {v:?}");
        }
    }

    #[test]
    fn test_column_type_invalid_discriminant() {
        assert!(ColumnType::try_from(0).is_err());
        assert!(ColumnType::try_from(9).is_err());
        assert!(ColumnType::try_from(255).is_err());
    }

    // ── DatabaseDef ───────────────────────────────────────────────────────────

    #[test]
    fn test_database_def_roundtrip() {
        let def = DatabaseDef {
            name: "ventas".into(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = DatabaseDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_database_def_roundtrip() {
        let def = TableDatabaseDef {
            table_id: 42,
            database_name: "analytics".into(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDatabaseDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    // ── TableDef ──────────────────────────────────────────────────────────────

    #[test]
    fn test_table_def_roundtrip() {
        let def = TableDef {
            id: 42,
            root_page_id: 7,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".to_string(),
            table_name: "users".to_string(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_roundtrip_with_root_page() {
        // Verify that root_page_id round-trips correctly for various values.
        for &root in &[1u64, 100, u64::MAX / 2, u64::MAX - 1] {
            let def = TableDef {
                id: 1,
                root_page_id: root,
                storage_layout: TableStorageLayout::Heap,
                schema_name: "public".into(),
                table_name: "t".into(),
            };
            let (back, _) = TableDef::from_bytes(&def.to_bytes()).unwrap();
            assert_eq!(back.root_page_id, root);
        }
    }

    #[test]
    fn test_table_def_empty_strings() {
        let def = TableDef {
            id: 1,
            root_page_id: 5,
            storage_layout: TableStorageLayout::Heap,
            schema_name: String::new(),
            table_name: String::new(),
        };
        let bytes = def.to_bytes();
        let (back, _) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn test_table_def_truncated_input_error() {
        let def = TableDef {
            id: 1,
            root_page_id: 3,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "s".into(),
            table_name: "t".into(),
        };
        let bytes = def.to_bytes();
        // Minimum is 14 bytes; truncate to 10 (has id+root but no schema_len).
        assert!(TableDef::from_bytes(&bytes[..10]).is_err());
        // Old 3-byte truncation still fails.
        assert!(TableDef::from_bytes(&bytes[..3]).is_err());
    }

    #[test]
    fn test_table_def_roundtrip_clustered_layout() {
        let def = TableDef {
            id: 9,
            root_page_id: 77,
            storage_layout: TableStorageLayout::Clustered,
            schema_name: "public".into(),
            table_name: "orders".into(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_legacy_bytes_decode_as_heap() {
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&42u32.to_le_bytes());
        legacy.extend_from_slice(&99u64.to_le_bytes());
        legacy.push(6);
        legacy.extend_from_slice(b"public");
        legacy.push(5);
        legacy.extend_from_slice(b"users");

        let (back, consumed) = TableDef::from_bytes(&legacy).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.root_page_id, 99);
        assert_eq!(back.storage_layout, TableStorageLayout::Heap);
        assert_eq!(back.schema_name, "public");
        assert_eq!(back.table_name, "users");
        assert_eq!(consumed, legacy.len());
    }

    // ── ColumnDef ─────────────────────────────────────────────────────────────

    #[test]
    fn test_column_def_roundtrip_nullable() {
        let def = ColumnDef {
            table_id: 5,
            col_idx: 2,
            name: "email".to_string(),
            col_type: ColumnType::Text,
            nullable: true,
            auto_increment: false,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = ColumnDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_column_def_roundtrip_not_nullable() {
        let def = ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: "id".to_string(),
            col_type: ColumnType::BigInt,
            nullable: false,
            auto_increment: false,
        };
        let bytes = def.to_bytes();
        let (back, _) = ColumnDef::from_bytes(&bytes).unwrap();
        assert!(!back.nullable);
        assert_eq!(back.col_type, ColumnType::BigInt);
    }

    #[test]
    fn test_column_def_truncated_input_error() {
        let def = ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: "x".into(),
            col_type: ColumnType::Int,
            nullable: false,
            auto_increment: false,
        };
        let bytes = def.to_bytes();
        assert!(ColumnDef::from_bytes(&bytes[..5]).is_err());
    }

    // ── IndexDef ──────────────────────────────────────────────────────────────

    #[test]
    fn test_index_def_roundtrip_primary_unique() {
        let def = IndexDef {
            index_id: 1,
            table_id: 3,
            name: "users_pkey".to_string(),
            root_page_id: 77,
            is_unique: true,
            is_primary: true,
            columns: vec![IndexColumnDef {
                col_idx: 0,
                order: SortOrder::Asc,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_index_def_roundtrip_non_unique() {
        let def = IndexDef {
            index_id: 5,
            table_id: 2,
            name: "orders_user_id_idx".to_string(),
            root_page_id: 100,
            is_unique: false,
            is_primary: false,
            columns: vec![IndexColumnDef {
                col_idx: 2,
                order: SortOrder::Asc,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, _) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back.index_id, 5);
        assert!(!back.is_unique);
        assert!(!back.is_primary);
        assert_eq!(back.columns.len(), 1);
        assert_eq!(back.columns[0].col_idx, 2);
    }

    #[test]
    fn test_index_def_truncated_input_error() {
        let def = IndexDef {
            index_id: 1,
            table_id: 1,
            name: "x".into(),
            root_page_id: 0,
            is_unique: false,
            is_primary: false,
            columns: vec![],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        assert!(IndexDef::from_bytes(&bytes[..10]).is_err());
    }

    #[test]
    fn test_index_def_roundtrip_multi_column() {
        let def = IndexDef {
            index_id: 7,
            table_id: 4,
            name: "composite_idx".to_string(),
            root_page_id: 200,
            is_unique: false,
            is_primary: false,
            columns: vec![
                IndexColumnDef {
                    col_idx: 1,
                    order: SortOrder::Asc,
                },
                IndexColumnDef {
                    col_idx: 3,
                    order: SortOrder::Desc,
                },
            ],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_index_def_old_format_backward_compat() {
        // Simulate an old-format row that ends after the name (no columns section).
        let def = IndexDef {
            index_id: 2,
            table_id: 1,
            name: "old_idx".to_string(),
            root_page_id: 50,
            is_unique: false,
            is_primary: false,
            columns: vec![],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let full_bytes = def.to_bytes();
        // Truncate the columns section (last byte is ncols=0, remove it).
        let old_bytes = &full_bytes[..full_bytes.len() - 1];
        let (back, consumed) = IndexDef::from_bytes(old_bytes).unwrap();
        assert_eq!(back.columns, vec![]);
        assert_eq!(consumed, old_bytes.len());
    }
}
