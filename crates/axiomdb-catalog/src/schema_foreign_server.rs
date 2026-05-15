/// Persistent definition for a foreign server — one row in `axiom_foreign_servers`
/// (Phase 22b.2).
///
/// ## On-disk format
/// ```text
/// [name_len:1][name UTF-8]
/// [fdw_len:1][fdw_name UTF-8]    ("http"; future: "postgres", "csv")
/// [opts_len:2 LE][options UTF-8] (JSON key-value map, e.g. {"url":"http://...","timeout_ms":"5000"})
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignServerDef {
    pub name: String,
    /// Name of the foreign-data wrapper type (e.g. `"http"`).
    pub fdw_name: String,
    /// JSON-encoded key-value options (`url`, `timeout_ms`, etc.).
    pub options: String,
}

impl ForeignServerDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let fdw = self.fdw_name.as_bytes();
        let opts = self.options.as_bytes();
        debug_assert!(name.len() <= 255, "server name too long");
        debug_assert!(fdw.len() <= 255, "fdw name too long");
        debug_assert!(opts.len() <= 65535, "server options too long");

        let mut buf = Vec::with_capacity(1 + name.len() + 1 + fdw.len() + 2 + opts.len());
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(fdw.len() as u8);
        buf.extend_from_slice(fdw);
        buf.extend_from_slice(&(opts.len() as u16).to_le_bytes());
        buf.extend_from_slice(opts);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), axiomdb_core::error::DbError> {
        let err = || axiomdb_core::error::DbError::ParseError {
            message: "truncated ForeignServerDef bytes".into(),
            position: None,
        };
        let mut pos = 0usize;

        macro_rules! read_str {
            ($len_bytes:expr) => {{
                if bytes.len() < pos + $len_bytes {
                    return Err(err());
                }
                let len = if $len_bytes == 1 {
                    bytes[pos] as usize
                } else {
                    u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize
                };
                pos += $len_bytes;
                if bytes.len() < pos + len {
                    return Err(err());
                }
                let s = std::str::from_utf8(&bytes[pos..pos + len])
                    .map_err(|_| axiomdb_core::error::DbError::ParseError {
                        message: "invalid UTF-8 in ForeignServerDef".into(),
                        position: None,
                    })?
                    .to_string();
                pos += len;
                s
            }};
        }

        let name = read_str!(1);
        let fdw_name = read_str!(1);
        let options = read_str!(2);

        Ok((
            Self {
                name,
                fdw_name,
                options,
            },
            pos,
        ))
    }
}
