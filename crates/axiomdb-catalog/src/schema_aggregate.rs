use axiomdb_types::DataType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateHelperKind {
    Median,
}

impl From<AggregateHelperKind> for u8 {
    fn from(value: AggregateHelperKind) -> Self {
        match value {
            AggregateHelperKind::Median => 1,
        }
    }
}

impl TryFrom<u8> for AggregateHelperKind {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Median),
            other => Err(DbError::ParseError {
                message: format!("invalid aggregate helper kind byte {other}"),
                position: None,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateDef {
    pub schema_name: String,
    pub name: String,
    pub arg_types: Vec<DataType>,
    pub sfunc: String,
    pub stype: String,
    pub finalfunc: Option<String>,
    pub helper_kind: AggregateHelperKind,
}

impl AggregateDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.name.as_bytes();
        let sfunc = self.sfunc.as_bytes();
        let stype = self.stype.as_bytes();
        let finalfunc = self.finalfunc.as_deref().unwrap_or("").as_bytes();
        debug_assert!(schema.len() <= 255, "aggregate schema too long");
        debug_assert!(name.len() <= 255, "aggregate name too long");
        debug_assert!(self.arg_types.len() <= 255, "too many aggregate args");
        debug_assert!(sfunc.len() <= 255, "aggregate sfunc too long");
        debug_assert!(stype.len() <= 255, "aggregate stype too long");
        debug_assert!(finalfunc.len() <= 255, "aggregate finalfunc too long");

        let mut buf = Vec::with_capacity(
            1 + schema.len() + 1 + name.len() + 1 + self.arg_types.len() + 1 + sfunc.len() + 1
                + stype.len()
                + 1
                + finalfunc.len()
                + 1,
        );
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(self.arg_types.len() as u8);
        for ty in &self.arg_types {
            buf.push(data_type_tag(ty.clone()));
        }
        buf.push(sfunc.len() as u8);
        buf.extend_from_slice(sfunc);
        buf.push(stype.len() as u8);
        buf.extend_from_slice(stype);
        buf.push(finalfunc.len() as u8);
        buf.extend_from_slice(finalfunc);
        buf.push(self.helper_kind.into());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated AggregateDef bytes".into(),
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
                message: "invalid UTF-8 in aggregate schema".into(),
                position: None,
            })?
            .to_string();
        consumed += schema_len;

        let name_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + name_len + 1 {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[consumed..consumed + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in aggregate name".into(),
                position: None,
            })?
            .to_string();
        consumed += name_len;

        let arg_count = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + arg_count + 1 {
            return Err(err());
        }
        let mut arg_types = Vec::with_capacity(arg_count);
        for _ in 0..arg_count {
            arg_types.push(data_type_from_tag(bytes[consumed])?);
            consumed += 1;
        }

        let sfunc_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + sfunc_len + 1 {
            return Err(err());
        }
        let sfunc = std::str::from_utf8(&bytes[consumed..consumed + sfunc_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in aggregate sfunc".into(),
                position: None,
            })?
            .to_string();
        consumed += sfunc_len;

        let stype_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + stype_len + 1 {
            return Err(err());
        }
        let stype = std::str::from_utf8(&bytes[consumed..consumed + stype_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in aggregate stype".into(),
                position: None,
            })?
            .to_string();
        consumed += stype_len;

        let finalfunc_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + finalfunc_len + 1 {
            return Err(err());
        }
        let finalfunc = if finalfunc_len == 0 {
            None
        } else {
            Some(
                std::str::from_utf8(&bytes[consumed..consumed + finalfunc_len])
                    .map_err(|_| DbError::ParseError {
                        message: "invalid UTF-8 in aggregate finalfunc".into(),
                        position: None,
                    })?
                    .to_string(),
            )
        };
        consumed += finalfunc_len;

        let helper_kind = AggregateHelperKind::try_from(bytes[consumed])?;
        consumed += 1;

        Ok((
            Self {
                schema_name,
                name,
                arg_types,
                sfunc,
                stype,
                finalfunc,
                helper_kind,
            },
            consumed,
        ))
    }
}

fn data_type_tag(ty: DataType) -> u8 {
    match ty {
        DataType::Bool => 1,
        DataType::Int => 2,
        DataType::BigInt => 3,
        DataType::Real => 4,
        DataType::Text => 5,
        DataType::Bytes => 6,
        DataType::Timestamp => 7,
        DataType::Uuid => 8,
        DataType::Json => 9,
        DataType::Jsonb => 10,
        DataType::Decimal => 11,
        DataType::Date => 12,
        DataType::Array(_) => 13,
        DataType::Range(_) => 14,
        DataType::Money => 15,
        DataType::Composite(_) => 16,
        DataType::Ltree => 17,
    }
}

fn data_type_from_tag(tag: u8) -> Result<DataType, DbError> {
    match tag {
        1 => Ok(DataType::Bool),
        2 => Ok(DataType::Int),
        3 => Ok(DataType::BigInt),
        4 => Ok(DataType::Real),
        5 => Ok(DataType::Text),
        6 => Ok(DataType::Bytes),
        7 => Ok(DataType::Timestamp),
        8 => Ok(DataType::Uuid),
        9 => Ok(DataType::Json),
        10 => Ok(DataType::Jsonb),
        11 => Ok(DataType::Decimal),
        12 => Ok(DataType::Date),
        13 => Ok(DataType::Array(Box::new(DataType::Text))),
        14 => Ok(DataType::Range(Box::new(DataType::Int))),
        15 => Ok(DataType::Money),
        16 => Ok(DataType::Composite(vec![])),
        17 => Ok(DataType::Ltree),
        other => Err(DbError::ParseError {
            message: format!("invalid aggregate arg type byte {other}"),
            position: None,
        }),
    }
}
