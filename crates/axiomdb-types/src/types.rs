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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// SQL JSON — stored as u24 LE length prefix + validated UTF-8 JSON text.
    /// Same wire format as Text; distinguished by DataType for operator dispatch.
    Json,
    /// SQL JSONB — stored as u24 LE length prefix + binary JSONB blob (Phase 11.16).
    /// O(log k) key access without re-parsing. Binary format: see jsonb.rs.
    Jsonb,
    /// SQL array type — e.g. `INT[]`, `TEXT[][]` (Phase 20.4).
    /// The inner `DataType` is the element type. Nested arrays use
    /// `Array(Box::new(Array(Box::new(T))))`.
    Array(Box<DataType>),
    /// SQL range type (Phase 20.13). Inner type is Int/BigInt/Decimal/Date/Timestamp.
    /// Maps to: Int→INT4RANGE, BigInt→INT8RANGE, Decimal→NUMRANGE,
    /// Date→DATERANGE, Timestamp→TSRANGE.
    Range(Box<DataType>),
    /// SQL MONEY — exact decimal amount + ISO 4217 3-byte currency code (Phase 20.17).
    /// Stored as 16-byte LE i128 mantissa + 1-byte scale + 3-byte ASCII currency.
    Money,
    /// SQL composite (user-defined) type (Phase 20.18).
    /// Inner vec holds `(field_name, field_type)` in declaration order.
    /// On disk: `[u32 LE data_len][encode_row(field_values, field_types)]`.
    Composite(Vec<(String, DataType)>),
    /// SQL ltree — hierarchical label path (Phase 20.19).
    /// On disk: `[u32 LE data_len][UTF-8 path bytes]`.
    Ltree,
    /// SQL XML / XMLTYPE — validated well-formed UTF-8 XML text (Phase 20.20).
    /// On disk: `[u32 LE data_len][UTF-8 XML bytes]`.
    Xml,
}

impl DataType {
    /// Human-readable name used in error messages and Display.
    ///
    /// For array types, returns `"TYPE[]"` with the inner element type name.
    /// Nested arrays produce `"TYPE[][]"` etc.
    /// For range types, returns the PostgreSQL range type name.
    pub fn name(&self) -> String {
        match self {
            Self::Bool => "BOOL".into(),
            Self::Int => "INT".into(),
            Self::BigInt => "BIGINT".into(),
            Self::Real => "REAL".into(),
            Self::Decimal => "DECIMAL".into(),
            Self::Text => "TEXT".into(),
            Self::Bytes => "BYTES".into(),
            Self::Date => "DATE".into(),
            Self::Timestamp => "TIMESTAMP".into(),
            Self::Uuid => "UUID".into(),
            Self::Json => "JSON".into(),
            Self::Jsonb => "JSONB".into(),
            Self::Array(elem) => format!("{}[]", elem.name()),
            Self::Range(inner) => match inner.as_ref() {
                DataType::Int => "INT4RANGE".into(),
                DataType::BigInt => "INT8RANGE".into(),
                DataType::Decimal => "NUMRANGE".into(),
                DataType::Date => "DATERANGE".into(),
                DataType::Timestamp => "TSRANGE".into(),
                other => format!("RANGE({})", other.name()),
            },
            Self::Money => "MONEY".into(),
            Self::Composite(_) => "COMPOSITE".into(),
            Self::Ltree => "LTREE".into(),
            Self::Xml => "XML".into(),
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

// ── Unit tests for Phase 20.4 arrays ─────────────────────────────────────────

#[cfg(test)]
mod array_type_tests {
    use super::*;

    #[test]
    fn datatype_array_variant_display() {
        let dt = DataType::Array(Box::new(DataType::Int));
        assert_eq!(format!("{}", dt), "INT[]");
        let dt2 = DataType::Array(Box::new(DataType::Array(Box::new(DataType::Text))));
        assert_eq!(format!("{}", dt2), "TEXT[][]");
    }

    #[test]
    fn datatype_array_name_1d() {
        let dt = DataType::Array(Box::new(DataType::Int));
        assert_eq!(dt.name(), "INT[]");
    }

    #[test]
    fn datatype_array_name_2d() {
        let dt = DataType::Array(Box::new(DataType::Array(Box::new(DataType::Text))));
        assert_eq!(dt.name(), "TEXT[][]");
    }

    #[test]
    fn datatype_array_name_3d() {
        let dt = DataType::Array(Box::new(DataType::Array(Box::new(DataType::Array(
            Box::new(DataType::Bool),
        )))));
        assert_eq!(dt.name(), "BOOL[][][]");
    }

    #[test]
    fn datatype_array_partial_eq() {
        let a = DataType::Array(Box::new(DataType::Int));
        let b = DataType::Array(Box::new(DataType::Int));
        let c = DataType::Array(Box::new(DataType::Text));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
