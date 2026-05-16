use axiomdb_core::error::DbError;

/// Persistent exchange-rate definition stored in `axiom_exchange_rates` (Phase 20.17).
///
/// Represents: `from_currency` → `to_currency` at rate `mantissa × 10^(-scale)`.
///
/// Binary layout:
///   [from_len: u8][from: UTF-8 N bytes]
///   [to_len:   u8][to:   UTF-8 M bytes]
///   [mantissa: 16 bytes LE i128]
///   [scale:    1 byte u8]
///
/// Both currency codes are stored upper-cased; max 3 bytes (ISO 4217).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRateDef {
    pub from_currency: String,
    pub to_currency: String,
    pub mantissa: i128,
    pub scale: u8,
}

impl ExchangeRateDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let from = self.from_currency.as_bytes();
        let to = self.to_currency.as_bytes();
        debug_assert!(from.len() <= 3, "from_currency too long");
        debug_assert!(to.len() <= 3, "to_currency too long");

        let mut buf = Vec::with_capacity(1 + from.len() + 1 + to.len() + 16 + 1);
        buf.push(from.len() as u8);
        buf.extend_from_slice(from);
        buf.push(to.len() as u8);
        buf.extend_from_slice(to);
        buf.extend_from_slice(&self.mantissa.to_le_bytes());
        buf.push(self.scale);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ExchangeRateDef bytes".into(),
            position: None,
        };

        let mut consumed = 0usize;

        if bytes.is_empty() {
            return Err(err());
        }
        let from_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + from_len + 1 {
            return Err(err());
        }
        let from_currency = std::str::from_utf8(&bytes[consumed..consumed + from_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in exchange rate from_currency".into(),
                position: None,
            })?
            .to_string();
        consumed += from_len;

        let to_len = bytes[consumed] as usize;
        consumed += 1;
        if bytes.len() < consumed + to_len + 17 {
            return Err(err());
        }
        let to_currency = std::str::from_utf8(&bytes[consumed..consumed + to_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in exchange rate to_currency".into(),
                position: None,
            })?
            .to_string();
        consumed += to_len;

        let mut m_bytes = [0u8; 16];
        m_bytes.copy_from_slice(&bytes[consumed..consumed + 16]);
        let mantissa = i128::from_le_bytes(m_bytes);
        consumed += 16;

        let scale = bytes[consumed];
        consumed += 1;

        Ok((
            Self {
                from_currency,
                to_currency,
                mantissa,
                scale,
            },
            consumed,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_usd_to_eur() {
        let def = ExchangeRateDef {
            from_currency: "USD".into(),
            to_currency: "EUR".into(),
            mantissa: 92,
            scale: 2,
        };
        let bytes = def.to_bytes();
        let (decoded, consumed) = ExchangeRateDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn roundtrip_negative_rate() {
        let def = ExchangeRateDef {
            from_currency: "AAA".into(),
            to_currency: "BBB".into(),
            mantissa: -100,
            scale: 4,
        };
        let bytes = def.to_bytes();
        let (decoded, consumed) = ExchangeRateDef::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn from_bytes_truncated_returns_error() {
        let def = ExchangeRateDef {
            from_currency: "USD".into(),
            to_currency: "EUR".into(),
            mantissa: 92,
            scale: 2,
        };
        let bytes = def.to_bytes();
        let truncated = &bytes[..bytes.len() - 1];
        assert!(ExchangeRateDef::from_bytes(truncated).is_err());
    }
}
