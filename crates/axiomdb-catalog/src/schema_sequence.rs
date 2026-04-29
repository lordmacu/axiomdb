#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceDef {
    pub schema_name: String,
    pub name: String,
    pub last_value: i64,
    pub start_value: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    pub cache_size: u64,
    pub is_called: bool,
}

impl SequenceDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.name.as_bytes();
        debug_assert!(schema.len() <= 255, "sequence schema too long");
        debug_assert!(name.len() <= 255, "sequence name too long");

        let mut buf = Vec::with_capacity(1 + schema.len() + 1 + name.len() + 8 * 6 + 1);
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.extend_from_slice(&self.last_value.to_le_bytes());
        buf.extend_from_slice(&self.start_value.to_le_bytes());
        buf.extend_from_slice(&self.increment.to_le_bytes());
        buf.extend_from_slice(&self.min_value.to_le_bytes());
        buf.extend_from_slice(&self.max_value.to_le_bytes());
        buf.extend_from_slice(&self.cache_size.to_le_bytes());
        let mut flags = 0u8;
        if self.cycle {
            flags |= 0b0000_0001;
        }
        if self.is_called {
            flags |= 0b0000_0010;
        }
        buf.push(flags);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated SequenceDef bytes".into(),
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
                message: "invalid UTF-8 in sequence schema".into(),
                position: None,
            })?
            .to_string();
        consumed += schema_len;

        let name_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + name_len + 8 * 6 + 1 {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[consumed..consumed + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in sequence name".into(),
                position: None,
            })?
            .to_string();
        consumed += name_len;

        let last_value = read_i64(bytes, &mut consumed)?;
        let start_value = read_i64(bytes, &mut consumed)?;
        let increment = read_i64(bytes, &mut consumed)?;
        let min_value = read_i64(bytes, &mut consumed)?;
        let max_value = read_i64(bytes, &mut consumed)?;
        let cache_size = read_u64(bytes, &mut consumed)?;
        let flags = bytes[consumed];
        consumed += 1;

        Ok((
            Self {
                schema_name,
                name,
                last_value,
                start_value,
                increment,
                min_value,
                max_value,
                cycle: flags & 0b0000_0001 != 0,
                is_called: flags & 0b0000_0010 != 0,
                cache_size,
            },
            consumed,
        ))
    }
}

fn read_i64(bytes: &[u8], consumed: &mut usize) -> Result<i64, DbError> {
    if bytes.len() < *consumed + 8 {
        return Err(DbError::ParseError {
            message: "truncated SequenceDef bytes".into(),
            position: None,
        });
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[*consumed..*consumed + 8]);
    *consumed += 8;
    Ok(i64::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], consumed: &mut usize) -> Result<u64, DbError> {
    if bytes.len() < *consumed + 8 {
        return Err(DbError::ParseError {
            message: "truncated SequenceDef bytes".into(),
            position: None,
        });
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[*consumed..*consumed + 8]);
    *consumed += 8;
    Ok(u64::from_le_bytes(raw))
}

