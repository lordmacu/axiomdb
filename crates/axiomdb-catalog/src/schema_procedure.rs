use crate::schema::ColumnType;
use axiomdb_core::error::DbError;

/// Parameter passing mode for a stored-procedure parameter (Phase 16.7).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcParamMode {
    /// Bound by value from the CALL argument; read-only inside the body.
    In = 0,
    /// Not bound from the caller; assigned by the body; returned to the caller.
    Out = 1,
    /// Bound from the caller AND returned.
    InOut = 2,
}

impl From<ProcParamMode> for u8 {
    fn from(m: ProcParamMode) -> u8 {
        m as u8
    }
}

impl TryFrom<u8> for ProcParamMode {
    type Error = DbError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::In),
            1 => Ok(Self::Out),
            2 => Ok(Self::InOut),
            _ => Err(DbError::ParseError {
                message: format!("unknown ProcParamMode discriminant: {v}"),
                position: None,
            }),
        }
    }
}

/// Source language / dialect of a stored procedure body (Phase 16.7).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcLanguage {
    /// PostgreSQL PL/pgSQL (`LANGUAGE plpgsql AS $$ … $$`).
    PlPgSql = 0,
    /// MySQL compound statement (`BEGIN … END`).
    MySql = 1,
}

impl From<ProcLanguage> for u8 {
    fn from(l: ProcLanguage) -> u8 {
        l as u8
    }
}

impl TryFrom<u8> for ProcLanguage {
    type Error = DbError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::PlPgSql),
            1 => Ok(Self::MySql),
            _ => Err(DbError::ParseError {
                message: format!("unknown ProcLanguage discriminant: {v}"),
                position: None,
            }),
        }
    }
}

/// One formal parameter of a stored procedure.
///
/// The type is stored as a [`ColumnType`] (one byte) for catalog consistency with
/// `ColumnDef`; the SQL layer's `DataType` is converted to/from `ColumnType` at the
/// AST↔catalog boundary. Type modifiers (e.g. `DECIMAL(p,s)` precision, `VARCHAR(n)`
/// length) are not preserved in v1 — only the base type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParam {
    pub mode: ProcParamMode,
    pub name: String,
    pub data_type: ColumnType,
}

/// Persistent definition of a stored procedure (Phase 16.7).
///
/// A non-table catalog object (like [`HolidayCalendarDef`](crate::HolidayCalendarDef)
/// / [`ExchangeRateDef`](crate::ExchangeRateDef)), keyed by `(schema_name, name)`.
/// The body is stored as raw source text and re-parsed on `CALL` (mirrors triggers /
/// views); no compiled-AST cache in v1.
///
/// Binary layout (little-endian):
/// ```text
/// [schema_len: u8][schema: UTF-8]
/// [name_len:   u8][name:   UTF-8]
/// [language:   u8]                        ; 0 = PlPgSql, 1 = MySql
/// [param_count: u16]
///   repeated param_count times:
///     [mode: u8]                          ; 0=IN, 1=OUT, 2=INOUT
///     [pname_len: u8][pname: UTF-8]
///     [type: u8]                          ; ColumnType discriminant
/// [body_len: u32][body: UTF-8]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcedureDef {
    pub schema_name: String,
    pub name: String,
    pub params: Vec<ProcParam>,
    pub language: ProcLanguage,
    /// Raw procedure body source (DECLARE section + BEGIN…END), re-parsed on CALL.
    pub body_sql: String,
}

impl ProcedureDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.name.as_bytes();
        let body = self.body_sql.as_bytes();
        debug_assert!(schema.len() <= u8::MAX as usize, "schema name too long");
        debug_assert!(name.len() <= u8::MAX as usize, "procedure name too long");
        debug_assert!(self.params.len() <= u16::MAX as usize, "too many params");
        debug_assert!(body.len() <= u32::MAX as usize, "body too long");

        let mut buf =
            Vec::with_capacity(1 + schema.len() + 1 + name.len() + 1 + 2 + 4 + body.len());
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(u8::from(self.language));
        buf.extend_from_slice(&(self.params.len() as u16).to_le_bytes());
        for p in &self.params {
            buf.push(u8::from(p.mode));
            let pname = p.name.as_bytes();
            debug_assert!(pname.len() <= u8::MAX as usize, "param name too long");
            buf.push(pname.len() as u8);
            buf.extend_from_slice(pname);
            buf.push(u8::from(p.data_type));
        }
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ProcedureDef bytes".into(),
            position: None,
        };
        let utf8_err = |what: &str| DbError::ParseError {
            message: format!("invalid UTF-8 in procedure {what}"),
            position: None,
        };
        let mut at = 0usize;

        // helper: read a u8-length-prefixed UTF-8 string
        let read_str = |bytes: &[u8], at: &mut usize, what: &str| -> Result<String, DbError> {
            if *at >= bytes.len() {
                return Err(err());
            }
            let len = bytes[*at] as usize;
            *at += 1;
            if bytes.len() < *at + len {
                return Err(err());
            }
            let s = std::str::from_utf8(&bytes[*at..*at + len])
                .map_err(|_| utf8_err(what))?
                .to_string();
            *at += len;
            Ok(s)
        };

        let schema_name = read_str(bytes, &mut at, "schema name")?;
        let name = read_str(bytes, &mut at, "name")?;

        if at >= bytes.len() {
            return Err(err());
        }
        let language = ProcLanguage::try_from(bytes[at])?;
        at += 1;

        if bytes.len() < at + 2 {
            return Err(err());
        }
        let param_count = {
            let mut raw = [0u8; 2];
            raw.copy_from_slice(&bytes[at..at + 2]);
            at += 2;
            u16::from_le_bytes(raw) as usize
        };

        let mut params = Vec::with_capacity(param_count);
        for _ in 0..param_count {
            if at >= bytes.len() {
                return Err(err());
            }
            let mode = ProcParamMode::try_from(bytes[at])?;
            at += 1;
            let pname = read_str(bytes, &mut at, "parameter name")?;
            if at >= bytes.len() {
                return Err(err());
            }
            let data_type = ColumnType::try_from(bytes[at])?;
            at += 1;
            params.push(ProcParam {
                mode,
                name: pname,
                data_type,
            });
        }

        if bytes.len() < at + 4 {
            return Err(err());
        }
        let body_len = {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&bytes[at..at + 4]);
            at += 4;
            u32::from_le_bytes(raw) as usize
        };
        if bytes.len() < at + body_len {
            return Err(err());
        }
        let body_sql = std::str::from_utf8(&bytes[at..at + body_len])
            .map_err(|_| utf8_err("body"))?
            .to_string();
        at += body_len;

        Ok((
            Self {
                schema_name,
                name,
                params,
                language,
                body_sql,
            },
            at,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProcedureDef {
        ProcedureDef {
            schema_name: "public".into(),
            name: "p".into(),
            params: vec![
                ProcParam {
                    mode: ProcParamMode::In,
                    name: "a".into(),
                    data_type: ColumnType::Int,
                },
                ProcParam {
                    mode: ProcParamMode::Out,
                    name: "b".into(),
                    data_type: ColumnType::Text,
                },
                ProcParam {
                    mode: ProcParamMode::InOut,
                    name: "c".into(),
                    data_type: ColumnType::Bool,
                },
            ],
            language: ProcLanguage::PlPgSql,
            body_sql: "BEGIN b := 'x'; END".into(),
        }
    }

    #[test]
    fn roundtrip_proc_with_all_param_modes() {
        let def = sample();
        let bytes = def.to_bytes();
        let (decoded, used) = ProcedureDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn roundtrip_no_params_mysql() {
        let def = ProcedureDef {
            schema_name: "public".into(),
            name: "noop".into(),
            params: vec![],
            language: ProcLanguage::MySql,
            body_sql: "BEGIN END".into(),
        };
        let bytes = def.to_bytes();
        let (decoded, used) = ProcedureDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn roundtrip_unicode_body_and_names() {
        let def = ProcedureDef {
            schema_name: "esquéma".into(),
            name: "procedimiento_ñ".into(),
            params: vec![ProcParam {
                mode: ProcParamMode::In,
                name: "café".into(),
                data_type: ColumnType::Decimal,
            }],
            language: ProcLanguage::MySql,
            body_sql: "BEGIN /* 日本語 */ SET café = café; END".into(),
        };
        let bytes = def.to_bytes();
        let (decoded, _) = ProcedureDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
    }

    #[test]
    fn from_bytes_truncated_is_error_not_panic() {
        let bytes = sample().to_bytes();
        // Truncating at every boundary must error, never panic.
        for cut in 0..bytes.len() {
            let _ = ProcedureDef::from_bytes(&bytes[..cut]); // must not panic
        }
        // A clean truncation one byte short of the full body is an error.
        assert!(ProcedureDef::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn from_bytes_rejects_bad_discriminants() {
        let mut bytes = sample().to_bytes();
        // Corrupt the language byte (right after schema+name length-prefixed strings).
        let lang_pos = 1 + "public".len() + 1 + "p".len();
        bytes[lang_pos] = 0x7f;
        assert!(ProcedureDef::from_bytes(&bytes).is_err());
    }

    #[test]
    fn mode_and_language_u8_roundtrip() {
        for m in [ProcParamMode::In, ProcParamMode::Out, ProcParamMode::InOut] {
            assert_eq!(ProcParamMode::try_from(u8::from(m)).unwrap(), m);
        }
        for l in [ProcLanguage::PlPgSql, ProcLanguage::MySql] {
            assert_eq!(ProcLanguage::try_from(u8::from(l)).unwrap(), l);
        }
    }
}
