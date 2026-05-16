use axiomdb_core::error::DbError;

/// Persistent definition of a country holiday calendar (Phase 20.16).
///
/// Binary layout:
///   [code_len: u8][country_code: UTF-8 N bytes][count: u16 LE][holidays: i32 LE × count]
///
/// `country_code` is always stored upper-cased. `holidays` are days since
/// Unix epoch (1970-01-01 = 0), sorted ascending, deduplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolidayCalendarDef {
    /// Upper-cased country code, max 16 bytes.
    pub country_code: String,
    /// Sorted, deduplicated holiday dates as days-since-Unix-epoch.
    pub holidays: Vec<i32>,
}

impl HolidayCalendarDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let code = self.country_code.as_bytes();
        debug_assert!(code.len() <= 16, "country code too long");
        debug_assert!(
            self.holidays.len() <= u16::MAX as usize,
            "too many holidays"
        );

        let mut buf = Vec::with_capacity(1 + code.len() + 2 + 4 * self.holidays.len());
        buf.push(code.len() as u8);
        buf.extend_from_slice(code);
        buf.extend_from_slice(&(self.holidays.len() as u16).to_le_bytes());
        for &day in &self.holidays {
            buf.extend_from_slice(&day.to_le_bytes());
        }
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated HolidayCalendarDef bytes".into(),
            position: None,
        };
        let mut consumed = 0usize;

        if bytes.is_empty() {
            return Err(err());
        }
        let code_len = bytes[consumed] as usize;
        consumed += 1;

        if bytes.len() < consumed + code_len + 2 {
            return Err(err());
        }
        let country_code = std::str::from_utf8(&bytes[consumed..consumed + code_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in holiday calendar country code".into(),
                position: None,
            })?
            .to_string();
        consumed += code_len;

        let count = {
            let mut raw = [0u8; 2];
            raw.copy_from_slice(&bytes[consumed..consumed + 2]);
            consumed += 2;
            u16::from_le_bytes(raw) as usize
        };

        if bytes.len() < consumed + count * 4 {
            return Err(err());
        }
        let mut holidays = Vec::with_capacity(count);
        for _ in 0..count {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&bytes[consumed..consumed + 4]);
            consumed += 4;
            holidays.push(i32::from_le_bytes(raw));
        }

        Ok((
            Self {
                country_code,
                holidays,
            },
            consumed,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_calendar() {
        let def = HolidayCalendarDef {
            country_code: "XX".into(),
            holidays: vec![],
        };
        let bytes = def.to_bytes();
        let (decoded, consumed) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn roundtrip_with_holidays() {
        let def = HolidayCalendarDef {
            country_code: "CO".into(),
            holidays: vec![20000, 20005, 20010, 20020],
        };
        let bytes = def.to_bytes();
        let (decoded, consumed) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn roundtrip_uppercase_code() {
        let def = HolidayCalendarDef {
            country_code: "US".into(),
            holidays: vec![19723],
        };
        let bytes = def.to_bytes();
        let (decoded, _) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.country_code, "US");
    }

    #[test]
    fn from_bytes_truncated_returns_error() {
        let def = HolidayCalendarDef {
            country_code: "CO".into(),
            holidays: vec![20000],
        };
        let bytes = def.to_bytes();
        // Truncate the last byte
        let truncated = &bytes[..bytes.len() - 1];
        assert!(HolidayCalendarDef::from_bytes(truncated).is_err());
    }
}
