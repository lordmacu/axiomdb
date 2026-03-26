//! Transport-charset registry for the MySQL wire layer (Phase 5.2a).
//!
//! AxiomDB stores all text internally as UTF-8. This module handles the
//! conversion between that internal representation and the encoding chosen by
//! the MySQL client at connection time.
//!
//! ## Supported charsets / collations
//!
//! | Charset    | Collation             | ID  |
//! |------------|-----------------------|-----|
//! | utf8mb4    | utf8mb4_0900_ai_ci    | 255 | ← server default
//! | utf8mb4    | utf8mb4_general_ci    |  45 |
//! | utf8mb4    | utf8mb4_bin           |  46 |
//! | utf8mb3    | utf8mb3_general_ci    |  33 |
//! | utf8mb3    | utf8mb3_bin           |  83 |
//! | latin1     | latin1_swedish_ci     |   8 |
//! | latin1     | latin1_bin            |  47 |
//! | (binary)   | binary                |  63 | ← metadata only
//!
//! Everything outside this table is rejected with a clear error.

use std::borrow::Cow;

use axiomdb_core::error::DbError;

// ── Data types ────────────────────────────────────────────────────────────────

/// Describes a supported transport charset.
#[derive(Debug, PartialEq, Eq)]
pub struct CharsetDef {
    /// Canonical charset name used in session variables.
    pub canonical_name: &'static str,
    /// All accepted name aliases (canonical is first).
    pub accepted_names: &'static [&'static str],
    /// Default collation for this charset.
    pub default_collation: &'static CollationDef,
}

/// Describes a supported collation.
#[derive(Debug, PartialEq, Eq)]
pub struct CollationDef {
    /// MySQL numeric collation id (sent in column-definition packets).
    pub id: u16,
    /// Collation name (sent in `@@collation_connection` etc.).
    pub name: &'static str,
    /// The charset this collation belongs to.
    pub charset: &'static CharsetDef,
    /// True for the `binary` pseudo-collation used in blob/bytes metadata.
    pub is_binary: bool,
}

// ── Static registry ───────────────────────────────────────────────────────────
//
// Two-pass layout: CharsetDefs reference CollationDefs and vice-versa, which
// requires unsafe statics. We break the cycle with a forward declaration trick:
// each CollationDef stores the charset via a pointer constant, and each
// CharsetDef stores a pointer to its default collation. All are `'static`.

// --- utf8mb4 ---

pub static UTF8MB4_CHARSET: CharsetDef = CharsetDef {
    canonical_name: "utf8mb4",
    accepted_names: &["utf8mb4"],
    default_collation: &UTF8MB4_0900_AI_CI,
};

pub static UTF8MB4_0900_AI_CI: CollationDef = CollationDef {
    id: 255,
    name: "utf8mb4_0900_ai_ci",
    charset: &UTF8MB4_CHARSET,
    is_binary: false,
};

pub static UTF8MB4_GENERAL_CI: CollationDef = CollationDef {
    id: 45,
    name: "utf8mb4_general_ci",
    charset: &UTF8MB4_CHARSET,
    is_binary: false,
};

pub static UTF8MB4_BIN: CollationDef = CollationDef {
    id: 46,
    name: "utf8mb4_bin",
    charset: &UTF8MB4_CHARSET,
    is_binary: false,
};

// --- utf8mb3 ---

pub static UTF8MB3_CHARSET: CharsetDef = CharsetDef {
    canonical_name: "utf8mb3",
    // "utf8" is the MySQL 5.x alias for utf8mb3.
    accepted_names: &["utf8mb3", "utf8"],
    default_collation: &UTF8MB3_GENERAL_CI,
};

pub static UTF8MB3_GENERAL_CI: CollationDef = CollationDef {
    id: 33,
    name: "utf8mb3_general_ci",
    charset: &UTF8MB3_CHARSET,
    is_binary: false,
};

pub static UTF8MB3_BIN: CollationDef = CollationDef {
    id: 83,
    name: "utf8mb3_bin",
    charset: &UTF8MB3_CHARSET,
    is_binary: false,
};

// --- latin1 ---

pub static LATIN1_CHARSET: CharsetDef = CharsetDef {
    canonical_name: "latin1",
    accepted_names: &["latin1"],
    // MySQL "latin1" is cp1252 (Windows-1252), not strict ISO-8859-1.
    // This affects bytes 0x80–0x9F (e.g., 0x80 = '€' in cp1252).
    default_collation: &LATIN1_SWEDISH_CI,
};

pub static LATIN1_SWEDISH_CI: CollationDef = CollationDef {
    id: 8,
    name: "latin1_swedish_ci",
    charset: &LATIN1_CHARSET,
    is_binary: false,
};

pub static LATIN1_BIN: CollationDef = CollationDef {
    id: 47,
    name: "latin1_bin",
    charset: &LATIN1_CHARSET,
    is_binary: false,
};

// --- binary (metadata-only pseudo-collation) ---

pub static BINARY_CHARSET: CharsetDef = CharsetDef {
    canonical_name: "binary",
    accepted_names: &["binary"],
    default_collation: &BINARY_COLLATION,
};

pub static BINARY_COLLATION: CollationDef = CollationDef {
    id: 63,
    name: "binary",
    charset: &BINARY_CHARSET,
    is_binary: true,
};

// ── Well-known constants ──────────────────────────────────────────────────────

/// Server-default collation: `utf8mb4_0900_ai_ci` (id 255).
pub const DEFAULT_SERVER_COLLATION: &CollationDef = &UTF8MB4_0900_AI_CI;

/// Binary collation (id 63) — used for BLOB/Bytes column metadata.
pub const BINARY_COLLATION_DEF: &CollationDef = &BINARY_COLLATION;

// ── Lookup API ────────────────────────────────────────────────────────────────

static ALL_COLLATIONS: &[&CollationDef] = &[
    &UTF8MB4_0900_AI_CI,
    &UTF8MB4_GENERAL_CI,
    &UTF8MB4_BIN,
    &UTF8MB3_GENERAL_CI,
    &UTF8MB3_BIN,
    &LATIN1_SWEDISH_CI,
    &LATIN1_BIN,
    &BINARY_COLLATION,
];

static ALL_CHARSETS: &[&CharsetDef] = &[
    &UTF8MB4_CHARSET,
    &UTF8MB3_CHARSET,
    &LATIN1_CHARSET,
    &BINARY_CHARSET,
];

/// Returns the `CharsetDef` for `name`, accepting aliases.
///
/// `"utf8"` is normalized to `utf8mb3`.
pub fn lookup_charset(name: &str) -> Option<&'static CharsetDef> {
    let lower = name.to_ascii_lowercase();
    ALL_CHARSETS.iter().copied().find(|cs| {
        cs.accepted_names
            .iter()
            .any(|&n| n.eq_ignore_ascii_case(&lower))
    })
}

/// Returns the `CollationDef` for `name` (case-insensitive).
pub fn lookup_collation(name: &str) -> Option<&'static CollationDef> {
    let lower = name.to_ascii_lowercase();
    ALL_COLLATIONS
        .iter()
        .copied()
        .find(|c| c.name.eq_ignore_ascii_case(&lower))
}

/// Returns the `CollationDef` for a MySQL numeric collation id.
pub fn lookup_collation_by_id(id: u16) -> Option<&'static CollationDef> {
    ALL_COLLATIONS.iter().copied().find(|c| c.id == id)
}

// ── Transport encode/decode ───────────────────────────────────────────────────

/// Decodes `bytes` as text in `charset`'s transport encoding into a UTF-8 string.
///
/// - `utf8mb4`: validated UTF-8; 4-byte sequences accepted.
/// - `utf8mb3`: validated UTF-8; 4-byte sequences (> U+FFFF) are rejected.
/// - `latin1`: cp1252-compatible decode via `encoding_rs::WINDOWS_1252`.
/// - `binary`: raw bytes assumed UTF-8 (best-effort; only used for metadata).
///
/// Never performs lossy replacement — returns `DbError` on invalid input.
pub fn decode_text<'a>(
    charset: &'static CharsetDef,
    bytes: &'a [u8],
) -> Result<Cow<'a, str>, DbError> {
    match charset.canonical_name {
        "utf8mb4" | "binary" => {
            std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|_| DbError::InvalidValue {
                    reason: format!(
                        "invalid UTF-8 bytes in {} transport",
                        charset.canonical_name
                    ),
                })
        }
        "utf8mb3" => {
            let s = std::str::from_utf8(bytes).map_err(|_| DbError::InvalidValue {
                reason: "invalid UTF-8 bytes in utf8mb3 transport".into(),
            })?;
            // utf8mb3 cannot represent code points above U+FFFF (4-byte UTF-8).
            for ch in s.chars() {
                if ch as u32 > 0xFFFF {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "character U+{:04X} is not representable in utf8mb3 (max U+FFFF)",
                            ch as u32
                        ),
                    });
                }
            }
            Ok(Cow::Borrowed(s))
        }
        "latin1" => {
            // MySQL "latin1" is Windows-1252 (cp1252), not strict ISO-8859-1.
            // encoding_rs::WINDOWS_1252 decodes all 256 byte values without error,
            // mapping 0x80–0x9F to their cp1252 code points (e.g., 0x80 → '€').
            let (decoded, _, had_error) = encoding_rs::WINDOWS_1252.decode(bytes);
            if had_error {
                // Should never happen: WINDOWS_1252 decodes all bytes.
                return Err(DbError::InvalidValue {
                    reason: "invalid latin1/cp1252 bytes".into(),
                });
            }
            Ok(Cow::Owned(decoded.into_owned()))
        }
        other => Err(DbError::InvalidValue {
            reason: format!("unsupported charset for decode: {other}"),
        }),
    }
}

/// Encodes a UTF-8 string `text` into the transport encoding of `charset`.
///
/// - `utf8mb4`: borrows the original UTF-8 bytes.
/// - `utf8mb3`: encodes as UTF-8 but rejects code points above U+FFFF.
/// - `latin1`: encodes via cp1252; returns `DbError` for unrepresentable scalars.
/// - `binary`: borrows the original UTF-8 bytes.
///
/// Never performs lossy replacement — returns `DbError` on unencodable input.
pub fn encode_text<'a>(
    charset: &'static CharsetDef,
    text: &'a str,
) -> Result<Cow<'a, [u8]>, DbError> {
    match charset.canonical_name {
        "utf8mb4" | "binary" => Ok(Cow::Borrowed(text.as_bytes())),
        "utf8mb3" => {
            for ch in text.chars() {
                if ch as u32 > 0xFFFF {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "character U+{:04X} ({ch}) cannot be encoded in utf8mb3",
                            ch as u32
                        ),
                    });
                }
            }
            Ok(Cow::Borrowed(text.as_bytes()))
        }
        "latin1" => {
            let (encoded, _, had_unmappable) = encoding_rs::WINDOWS_1252.encode(text);
            if had_unmappable {
                // Find the first unrepresentable character for a helpful error.
                let bad = text
                    .chars()
                    .find(|&ch| {
                        let s: String = std::iter::once(ch).collect();
                        let (_, _, unmap) = encoding_rs::WINDOWS_1252.encode(&s);
                        unmap
                    })
                    .unwrap_or('\u{FFFD}');
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "character U+{:04X} ({bad}) cannot be encoded in latin1/cp1252",
                        bad as u32
                    ),
                });
            }
            Ok(Cow::Owned(encoded.into_owned()))
        }
        other => Err(DbError::InvalidValue {
            reason: format!("unsupported charset for encode: {other}"),
        }),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Registry lookups ──────────────────────────────────────────────────────

    #[test]
    fn test_lookup_charset_utf8mb4() {
        let cs = lookup_charset("utf8mb4").unwrap();
        assert_eq!(cs.canonical_name, "utf8mb4");
    }

    #[test]
    fn test_lookup_charset_utf8_alias_normalizes_to_utf8mb3() {
        let cs = lookup_charset("utf8").unwrap();
        assert_eq!(cs.canonical_name, "utf8mb3");
    }

    #[test]
    fn test_lookup_charset_utf8mb3() {
        let cs = lookup_charset("utf8mb3").unwrap();
        assert_eq!(cs.canonical_name, "utf8mb3");
    }

    #[test]
    fn test_lookup_charset_latin1() {
        let cs = lookup_charset("latin1").unwrap();
        assert_eq!(cs.canonical_name, "latin1");
    }

    #[test]
    fn test_lookup_charset_unknown_is_none() {
        assert!(lookup_charset("cp1252").is_none());
        assert!(lookup_charset("ascii").is_none());
    }

    #[test]
    fn test_lookup_collation_by_id_255() {
        let c = lookup_collation_by_id(255).unwrap();
        assert_eq!(c.name, "utf8mb4_0900_ai_ci");
        assert_eq!(c.charset.canonical_name, "utf8mb4");
    }

    #[test]
    fn test_lookup_collation_by_id_8_is_latin1_swedish_ci() {
        let c = lookup_collation_by_id(8).unwrap();
        assert_eq!(c.name, "latin1_swedish_ci");
        assert_eq!(c.charset.canonical_name, "latin1");
    }

    #[test]
    fn test_lookup_collation_by_id_33_is_utf8mb3_general_ci() {
        let c = lookup_collation_by_id(33).unwrap();
        assert_eq!(c.name, "utf8mb3_general_ci");
    }

    #[test]
    fn test_lookup_collation_by_id_63_is_binary() {
        let c = lookup_collation_by_id(63).unwrap();
        assert_eq!(c.name, "binary");
        assert!(c.is_binary);
    }

    #[test]
    fn test_lookup_collation_by_id_unknown_is_none() {
        assert!(lookup_collation_by_id(0).is_none());
        assert!(lookup_collation_by_id(999).is_none());
    }

    #[test]
    fn test_lookup_collation_by_name_latin1_bin() {
        let c = lookup_collation("latin1_bin").unwrap();
        assert_eq!(c.id, 47);
    }

    // ── latin1 decode ─────────────────────────────────────────────────────────

    #[test]
    fn test_latin1_decode_euro_sign() {
        // 0x80 in cp1252 is '€' (U+20AC), not a control character as in ISO-8859-1.
        let bytes = &[0x80u8];
        let s = decode_text(&LATIN1_CHARSET, bytes).unwrap();
        assert_eq!(s.as_ref(), "€");
    }

    #[test]
    fn test_latin1_decode_cafe() {
        // "café" in latin1/cp1252: 'c'=0x63, 'a'=0x61, 'f'=0x66, 'é'=0xE9
        let bytes = &[0x63u8, 0x61, 0x66, 0xE9];
        let s = decode_text(&LATIN1_CHARSET, bytes).unwrap();
        assert_eq!(s.as_ref(), "café");
    }

    // ── latin1 encode ─────────────────────────────────────────────────────────

    #[test]
    fn test_latin1_encode_euro_sign() {
        let encoded = encode_text(&LATIN1_CHARSET, "€").unwrap();
        assert_eq!(encoded.as_ref(), &[0x80u8]);
    }

    #[test]
    fn test_latin1_encode_cafe() {
        let encoded = encode_text(&LATIN1_CHARSET, "café").unwrap();
        assert_eq!(encoded.as_ref(), &[0x63u8, 0x61, 0x66, 0xE9]);
    }

    #[test]
    fn test_latin1_encode_emoji_errors() {
        // Emoji is not representable in latin1/cp1252.
        let err = encode_text(&LATIN1_CHARSET, "hello 🎉").unwrap_err();
        assert!(
            err.to_string().contains("cannot be encoded in latin1"),
            "error: {err}"
        );
    }

    // ── utf8mb3 decode/encode ─────────────────────────────────────────────────

    #[test]
    fn test_utf8mb3_decode_rejects_4byte_codepoint() {
        // '🎉' is U+1F389, encoded as 4 bytes in UTF-8.
        let emoji = "🎉";
        let err = decode_text(&UTF8MB3_CHARSET, emoji.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("utf8mb3"), "error: {err}");
    }

    #[test]
    fn test_utf8mb3_decode_accepts_3byte_codepoint() {
        // '€' is U+20AC, encoded as 3 bytes in UTF-8.
        let euro = "€";
        let s = decode_text(&UTF8MB3_CHARSET, euro.as_bytes()).unwrap();
        assert_eq!(s.as_ref(), "€");
    }

    #[test]
    fn test_utf8mb3_encode_rejects_emoji() {
        let err = encode_text(&UTF8MB3_CHARSET, "hello 🎉").unwrap_err();
        assert!(err.to_string().contains("utf8mb3"), "error: {err}");
    }

    #[test]
    fn test_utf8mb3_encode_accepts_bmp_text() {
        let encoded = encode_text(&UTF8MB3_CHARSET, "hello €").unwrap();
        assert_eq!(encoded.as_ref(), "hello €".as_bytes());
    }

    // ── utf8mb4 encode/decode ─────────────────────────────────────────────────

    #[test]
    fn test_utf8mb4_encode_accepts_emoji() {
        let encoded = encode_text(&UTF8MB4_CHARSET, "hello 🎉").unwrap();
        assert_eq!(encoded.as_ref(), "hello 🎉".as_bytes());
    }

    #[test]
    fn test_utf8mb4_decode_invalid_bytes_errors() {
        let bad = &[0xFF, 0xFE]; // not valid UTF-8
        let err = decode_text(&UTF8MB4_CHARSET, bad).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "error: {err}");
    }

    // ── Default collation ─────────────────────────────────────────────────────

    #[test]
    fn test_default_server_collation_is_utf8mb4_0900_ai_ci() {
        assert_eq!(DEFAULT_SERVER_COLLATION.id, 255);
        assert_eq!(DEFAULT_SERVER_COLLATION.name, "utf8mb4_0900_ai_ci");
    }

    #[test]
    fn test_binary_collation_id_is_63() {
        assert_eq!(BINARY_COLLATION_DEF.id, 63);
        assert!(BINARY_COLLATION_DEF.is_binary);
    }
}
