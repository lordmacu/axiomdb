#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumTypeDef {
    pub schema_name: String,
    pub name: String,
    pub labels: Vec<String>,
}

impl EnumTypeDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.name.as_bytes();
        debug_assert!(schema.len() <= 255, "enum schema too long");
        debug_assert!(name.len() <= 255, "enum name too long");
        debug_assert!(self.labels.len() <= u16::MAX as usize, "too many enum labels");

        let labels_len: usize = self.labels.iter().map(|s| 2 + s.len()).sum();
        let mut buf = Vec::with_capacity(1 + schema.len() + 1 + name.len() + 2 + labels_len);
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.extend_from_slice(&(self.labels.len() as u16).to_le_bytes());
        for label in &self.labels {
            let bytes = label.as_bytes();
            debug_assert!(bytes.len() <= u16::MAX as usize, "enum label too long");
            buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated EnumTypeDef bytes".into(),
            position: None,
        };
        let mut consumed = 0usize;

        if bytes.len() < consumed + 1 {
            return Err(err());
        }
        let schema_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + schema_len + 1 {
            return Err(err());
        }
        let schema_name = std::str::from_utf8(&bytes[consumed..consumed + schema_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in enum schema".into(),
                position: None,
            })?
            .to_string();
        consumed += schema_len;

        let name_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + name_len + 2 {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[consumed..consumed + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in enum name".into(),
                position: None,
            })?
            .to_string();
        consumed += name_len;

        let label_count = read_u16(bytes, &mut consumed)? as usize;
        let mut labels = Vec::with_capacity(label_count);
        for _ in 0..label_count {
            let len = read_u16(bytes, &mut consumed)? as usize;
            if bytes.len() < consumed + len {
                return Err(err());
            }
            let label = std::str::from_utf8(&bytes[consumed..consumed + len])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in enum label".into(),
                    position: None,
                })?
                .to_string();
            consumed += len;
            labels.push(label);
        }

        Ok((
            Self {
                schema_name,
                name,
                labels,
            },
            consumed,
        ))
    }
}

fn read_u16(bytes: &[u8], consumed: &mut usize) -> Result<u16, DbError> {
    if bytes.len() < *consumed + 2 {
        return Err(DbError::ParseError {
            message: "truncated EnumTypeDef bytes".into(),
            position: None,
        });
    }
    let mut raw = [0u8; 2];
    raw.copy_from_slice(&bytes[*consumed..*consumed + 2]);
    *consumed += 2;
    Ok(u16::from_le_bytes(raw))
}
