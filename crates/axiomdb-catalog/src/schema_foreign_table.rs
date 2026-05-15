use axiomdb_core::error::DbError;
// ColumnType lives inside schema.rs (via include!("schema_database.rs")), which
// is compiled as the `schema` module. Re-exported from lib.rs as a top-level
// item, so we can reach it via `crate::schema::ColumnType`.
use crate::schema::ColumnType;

/// Table IDs at or above this value identify foreign (remote) tables.
/// The sentinel reserves bit 31 so all valid physical table IDs remain below
/// 0x8000_0000 (about 2 billion), which is plenty for any realistic schema.
pub const FOREIGN_TABLE_ID_BASE: u32 = 0x8000_0000;

/// A single column declared inside `CREATE FOREIGN TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignColumnDef {
    pub name: String,
    pub col_type: ColumnType,
    pub nullable: bool,
}

/// Persistent definition for a foreign table — one row in `axiom_foreign_tables`
/// (Phase 22b.2).
///
/// ## On-disk format
/// ```text
/// [table_id:4 LE]
/// [table_name_len:1][table_name UTF-8]
/// [schema_name_len:1][schema_name UTF-8]
/// [server_name_len:1][server_name UTF-8]
/// [col_count:2 LE]
/// for each column:
///   [name_len:1][name UTF-8][col_type:1][nullable:1]
/// [opts_len:2 LE][options UTF-8]   (JSON: {"endpoint":"/users","method":"GET"})
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignTableDef {
    /// Foreign table ID — always `>= FOREIGN_TABLE_ID_BASE`.
    pub table_id: u32,
    pub table_name: String,
    pub schema_name: String,
    pub server_name: String,
    pub columns: Vec<ForeignColumnDef>,
    /// JSON-encoded key-value options (`endpoint`, `method`, etc.).
    pub options: String,
}

impl ForeignTableDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let tname = self.table_name.as_bytes();
        let sname = self.schema_name.as_bytes();
        let srv = self.server_name.as_bytes();
        let opts = self.options.as_bytes();
        debug_assert!(tname.len() <= 255);
        debug_assert!(sname.len() <= 255);
        debug_assert!(srv.len() <= 255);
        debug_assert!(self.columns.len() <= 65535);
        debug_assert!(opts.len() <= 65535);

        let mut buf = Vec::new();
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.push(tname.len() as u8);
        buf.extend_from_slice(tname);
        buf.push(sname.len() as u8);
        buf.extend_from_slice(sname);
        buf.push(srv.len() as u8);
        buf.extend_from_slice(srv);

        buf.extend_from_slice(&(self.columns.len() as u16).to_le_bytes());
        for col in &self.columns {
            let cname = col.name.as_bytes();
            debug_assert!(cname.len() <= 255);
            buf.push(cname.len() as u8);
            buf.extend_from_slice(cname);
            buf.push(col.col_type as u8);
            buf.push(u8::from(col.nullable));
        }

        buf.extend_from_slice(&(opts.len() as u16).to_le_bytes());
        buf.extend_from_slice(opts);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ForeignTableDef bytes".into(),
            position: None,
        };
        let mut pos = 0usize;

        if bytes.len() < 4 {
            return Err(err());
        }
        let table_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        pos += 4;

        macro_rules! read_str1 {
            () => {{
                if bytes.len() < pos + 1 {
                    return Err(err());
                }
                let len = bytes[pos] as usize;
                pos += 1;
                if bytes.len() < pos + len {
                    return Err(err());
                }
                let s = std::str::from_utf8(&bytes[pos..pos + len])
                    .map_err(|_| DbError::ParseError {
                        message: "invalid UTF-8 in ForeignTableDef".into(),
                        position: None,
                    })?
                    .to_string();
                pos += len;
                s
            }};
        }

        let table_name = read_str1!();
        let schema_name = read_str1!();
        let server_name = read_str1!();

        if bytes.len() < pos + 2 {
            return Err(err());
        }
        let col_count = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;

        let mut columns = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let name = read_str1!();
            if bytes.len() < pos + 2 {
                return Err(err());
            }
            let col_type = ColumnType::try_from(bytes[pos])?;
            let nullable = bytes[pos + 1] != 0;
            pos += 2;
            columns.push(ForeignColumnDef {
                name,
                col_type,
                nullable,
            });
        }

        if bytes.len() < pos + 2 {
            return Err(err());
        }
        let opts_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if bytes.len() < pos + opts_len {
            return Err(err());
        }
        let options = std::str::from_utf8(&bytes[pos..pos + opts_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in ForeignTableDef options".into(),
                position: None,
            })?
            .to_string();
        pos += opts_len;

        Ok((
            Self {
                table_id,
                table_name,
                schema_name,
                server_name,
                columns,
                options,
            },
            pos,
        ))
    }
}
