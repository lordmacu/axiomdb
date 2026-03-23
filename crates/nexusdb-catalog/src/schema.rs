//! Catalog schema types and their binary serialization.
//!
//! These types represent rows in the three system tables:
//! - `nexus_tables`  → [`TableDef`]
//! - `nexus_columns` → [`ColumnDef`]
//! - `nexus_indexes` → [`IndexDef`]
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

use nexusdb_core::error::DbError;

// ── Public type aliases ───────────────────────────────────────────────────────

/// Unique identifier for a table in the catalog. `0` is reserved (invalid).
pub type TableId = u32;

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
            }),
        }
    }
}

impl From<ColumnType> for u8 {
    fn from(c: ColumnType) -> u8 {
        c as u8
    }
}

// ── TableDef ──────────────────────────────────────────────────────────────────

/// Metadata for a user table — one row in `nexus_tables`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub id: TableId,
    pub schema_name: String,
    pub table_name: String,
}

impl TableDef {
    /// Serializes to binary row format.
    ///
    /// Format: `[table_id:4][schema_len:1][schema bytes][name_len:1][name bytes]`
    ///
    /// # Panics (debug only)
    /// If `schema_name` or `table_name` exceeds 255 bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.table_name.as_bytes();
        debug_assert!(schema.len() <= 255, "schema_name too long");
        debug_assert!(name.len() <= 255, "table_name too long");

        let mut buf = Vec::with_capacity(4 + 1 + schema.len() + 1 + name.len());
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Returns `(TableDef, bytes_consumed)` on success.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated TableRow bytes".into(),
        };

        if bytes.len() < 6 {
            return Err(err());
        }

        let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let schema_len = bytes[4] as usize;
        let pos = 5;

        if bytes.len() < pos + schema_len + 1 {
            return Err(err());
        }
        let schema_name = std::str::from_utf8(&bytes[pos..pos + schema_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in schema_name".into(),
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
            })?
            .to_string();
        let consumed = pos + name_len;

        Ok((
            Self {
                id,
                schema_name,
                table_name,
            },
            consumed,
        ))
    }
}

// ── ColumnDef ─────────────────────────────────────────────────────────────────

/// Metadata for a single column — one row in `nexus_columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub table_id: TableId,
    pub col_idx: u16,
    pub name: String,
    pub col_type: ColumnType,
    pub nullable: bool,
}

impl ColumnDef {
    /// Serializes to binary row format.
    ///
    /// Format: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
    /// - `flags bit0` = nullable
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "column name too long");

        let flags: u8 = if self.nullable { 0x01 } else { 0x00 };
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
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ColumnRow bytes".into(),
        };

        if bytes.len() < 9 {
            return Err(err());
        }

        let table_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let col_idx = u16::from_le_bytes([bytes[4], bytes[5]]);
        let col_type = ColumnType::try_from(bytes[6])?;
        let flags = bytes[7];
        let nullable = flags & 0x01 != 0;
        let name_len = bytes[8] as usize;

        if bytes.len() < 9 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[9..9 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in column name".into(),
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
            },
            consumed,
        ))
    }
}

// ── IndexDef ──────────────────────────────────────────────────────────────────

/// Metadata for an index — one row in `nexus_indexes`.
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
}

impl IndexDef {
    /// Serializes to binary row format.
    ///
    /// Format: `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
    /// - `flags bit0` = unique, `flags bit1` = primary key
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "index name too long");

        let mut flags: u8 = 0;
        if self.is_unique {
            flags |= 0x01;
        }
        if self.is_primary {
            flags |= 0x02;
        }

        let mut buf = Vec::with_capacity(4 + 4 + 8 + 1 + 1 + name.len());
        buf.extend_from_slice(&self.index_id.to_le_bytes());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.root_page_id.to_le_bytes());
        buf.push(flags);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Format: `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated IndexRow bytes".into(),
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
        let name_len = bytes[17] as usize;

        if bytes.len() < 18 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[18..18 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in index name".into(),
            })?
            .to_string();
        let consumed = 18 + name_len;

        Ok((
            Self {
                index_id,
                table_id,
                name,
                root_page_id,
                is_unique,
                is_primary,
            },
            consumed,
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

    // ── TableDef ──────────────────────────────────────────────────────────────

    #[test]
    fn test_table_def_roundtrip() {
        let def = TableDef {
            id: 42,
            schema_name: "public".to_string(),
            table_name: "users".to_string(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_empty_strings() {
        let def = TableDef {
            id: 1,
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
            schema_name: "s".into(),
            table_name: "t".into(),
        };
        let bytes = def.to_bytes();
        // Truncate to 3 bytes — not enough for the fixed header.
        assert!(TableDef::from_bytes(&bytes[..3]).is_err());
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
        };
        let bytes = def.to_bytes();
        let (back, _) = ColumnDef::from_bytes(&bytes).unwrap();
        assert_eq!(back.nullable, false);
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
        };
        let bytes = def.to_bytes();
        let (back, _) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back.index_id, 5);
        assert_eq!(back.is_unique, false);
        assert_eq!(back.is_primary, false);
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
        };
        let bytes = def.to_bytes();
        assert!(IndexDef::from_bytes(&bytes[..10]).is_err());
    }
}
