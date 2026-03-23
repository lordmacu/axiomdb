//! SQL data types as seen by the executor and the row codec.
//!
//! [`DataType`] is the in-memory type descriptor used by `axiomdb-types` and
//! `axiomdb-sql`. It is intentionally separate from [`ColumnType`] in
//! `axiomdb-catalog`, which is a compact `repr(u8)` enum for disk storage.
//! The executor converts between the two when reading column definitions from
//! the catalog.
//!
//! [`ColumnType`]: axiomdb_catalog::schema::ColumnType

/// SQL column type descriptor used by the executor and the row codec.
///
/// Does not carry type parameters (precision, scale, max-length) yet —
/// those are added in Phase 4.3 when the DDL parser gains `DECIMAL(p,s)`
/// and `VARCHAR(n)` syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// SQL BOOLEAN — stored as 1 byte (0x00 / 0x01).
    Bool,
    /// SQL INT / INTEGER — stored as 4-byte little-endian i32.
    Int,
    /// SQL BIGINT — stored as 8-byte little-endian i64.
    BigInt,
    /// SQL REAL / DOUBLE PRECISION / FLOAT — stored as 8-byte LE f64 (IEEE 754).
    Real,
    /// SQL DECIMAL / NUMERIC — stored as 16-byte LE i128 mantissa + 1-byte scale.
    /// Represents `mantissa × 10^(-scale)`.
    Decimal,
    /// SQL TEXT / VARCHAR — stored as u24 LE length prefix + UTF-8 bytes.
    Text,
    /// SQL BLOB / BYTEA — stored as u24 LE length prefix + raw bytes.
    Bytes,
    /// SQL DATE — stored as 4-byte LE i32 (days since 1970-01-01).
    Date,
    /// SQL TIMESTAMP — stored as 8-byte LE i64 (microseconds since 1970-01-01 UTC).
    Timestamp,
    /// SQL UUID — stored as 16 raw bytes (big-endian UUID byte order).
    Uuid,
}

impl DataType {
    /// Human-readable name used in error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::Int => "INT",
            Self::BigInt => "BIGINT",
            Self::Real => "REAL",
            Self::Decimal => "DECIMAL",
            Self::Text => "TEXT",
            Self::Bytes => "BYTES",
            Self::Date => "DATE",
            Self::Timestamp => "TIMESTAMP",
            Self::Uuid => "UUID",
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
