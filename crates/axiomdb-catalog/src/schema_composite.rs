use axiomdb_core::error::DbError;

/// Persisted definition of a `CREATE TYPE … AS (…)` composite type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeTypeDef {
    pub schema_name: String,
    pub name: String,
    /// Ordered list of `(field_name, sql_type_name)` pairs, e.g.
    /// `[("city", "TEXT"), ("zip", "INT")]`.
    pub fields: Vec<CompositeField>,
}

/// One field in a composite type definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeField {
    pub name: String,
    /// SQL type name stored as a string (e.g. `"TEXT"`, `"INT"`, `"BIGINT"`).
    pub type_name: String,
}

impl CompositeTypeDef {
    /// Serialises into a compact byte representation.
    ///
    /// Layout:
    /// ```text
    /// [u8  schema_len][schema bytes]
    /// [u8  name_len  ][name bytes  ]
    /// [u16 field_count]
    ///   repeated field_count times:
    ///     [u16 fname_len][fname bytes]
    ///     [u16 tname_len][tname bytes]
    /// ```
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.name.as_bytes();
        debug_assert!(schema.len() <= 255, "composite schema name too long");
        debug_assert!(name.len() <= 255, "composite type name too long");
        debug_assert!(
            self.fields.len() <= u16::MAX as usize,
            "too many composite fields"
        );

        let fields_len: usize = self
            .fields
            .iter()
            .map(|f| 2 + f.name.len() + 2 + f.type_name.len())
            .sum();
        let mut buf = Vec::with_capacity(1 + schema.len() + 1 + name.len() + 2 + fields_len);

        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.extend_from_slice(&(self.fields.len() as u16).to_le_bytes());

        for field in &self.fields {
            let fn_bytes = field.name.as_bytes();
            let tn_bytes = field.type_name.as_bytes();
            debug_assert!(fn_bytes.len() <= u16::MAX as usize, "field name too long");
            debug_assert!(
                tn_bytes.len() <= u16::MAX as usize,
                "field type name too long"
            );
            buf.extend_from_slice(&(fn_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(fn_bytes);
            buf.extend_from_slice(&(tn_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(tn_bytes);
        }
        buf
    }

    /// Deserialises from bytes, returning `(Self, bytes_consumed)`.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated CompositeTypeDef bytes".into(),
            position: None,
        };
        let mut pos = 0usize;

        // schema_name
        if bytes.len() < pos + 1 {
            return Err(err());
        }
        let schema_len = bytes[pos] as usize;
        pos += 1;
        if bytes.len() < pos + schema_len + 1 {
            return Err(err());
        }
        let schema_name = std::str::from_utf8(&bytes[pos..pos + schema_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in composite schema name".into(),
                position: None,
            })?
            .to_string();
        pos += schema_len;

        // type name
        let name_len = bytes[pos] as usize;
        pos += 1;
        if bytes.len() < pos + name_len + 2 {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[pos..pos + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in composite type name".into(),
                position: None,
            })?
            .to_string();
        pos += name_len;

        // field count
        let field_count = read_u16(bytes, &mut pos)? as usize;
        let mut fields = Vec::with_capacity(field_count);

        for _ in 0..field_count {
            // field name
            let fn_len = read_u16(bytes, &mut pos)? as usize;
            if bytes.len() < pos + fn_len {
                return Err(err());
            }
            let fname = std::str::from_utf8(&bytes[pos..pos + fn_len])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in composite field name".into(),
                    position: None,
                })?
                .to_string();
            pos += fn_len;

            // field type name
            let tn_len = read_u16(bytes, &mut pos)? as usize;
            if bytes.len() < pos + tn_len {
                return Err(err());
            }
            let tname = std::str::from_utf8(&bytes[pos..pos + tn_len])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in composite field type".into(),
                    position: None,
                })?
                .to_string();
            pos += tn_len;

            fields.push(CompositeField {
                name: fname,
                type_name: tname,
            });
        }

        Ok((
            Self {
                schema_name,
                name,
                fields,
            },
            pos,
        ))
    }
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16, DbError> {
    if bytes.len() < *pos + 2 {
        return Err(DbError::ParseError {
            message: "truncated CompositeTypeDef bytes".into(),
            position: None,
        });
    }
    let mut raw = [0u8; 2];
    raw.copy_from_slice(&bytes[*pos..*pos + 2]);
    *pos += 2;
    Ok(u16::from_le_bytes(raw))
}
