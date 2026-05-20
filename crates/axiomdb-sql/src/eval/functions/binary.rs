use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

/// Standard Base64 alphabet (RFC 4648).
const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── BLOB / binary functions (4.19b) ──────────────────────────────────

        // FROM_BASE64(text) → BLOB
        // Decodes a standard base64-encoded string to raw bytes.
        // Returns NULL if the input is NULL or contains invalid base64.
        // MySQL-compatible: FROM_BASE64('aGVsbG8=') → x'68656c6c6f'
        "from_base64" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match crate::eval::eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => match b64_decode(s.trim()) {
                    Some(bytes) => Ok(Value::Bytes(bytes)),
                    None => Ok(Value::Null), // invalid base64 → NULL (MySQL behavior)
                },
                _ => Ok(Value::Null),
            }
        }

        // TO_BASE64(blob) → TEXT
        // Encodes raw bytes to a standard base64 string.
        // Also accepts TEXT (encodes the UTF-8 bytes) and UUID (encodes 16 bytes).
        // MySQL-compatible: TO_BASE64('hello') → 'aGVsbG8='
        "to_base64" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match crate::eval::eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Bytes(b) => Ok(Value::Text(b64_encode(&b))),
                Value::Text(s) => Ok(Value::Text(b64_encode(s.as_bytes()))),
                Value::Uuid(b) => Ok(Value::Text(b64_encode(&b))),
                _ => Ok(Value::Null),
            }
        }

        // ENCODE(blob, format) → TEXT
        // Encodes binary data to a text representation.
        // format: 'base64' or 'hex'
        // PostgreSQL-compatible: ENCODE(E'\\x68656c6c6f', 'hex') → '68656c6c6f'
        "encode" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let data = crate::eval::eval(&args[0], row)?;
            let fmt = crate::eval::eval(&args[1], row)?;
            let bytes = match data {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                Value::Uuid(b) => b.to_vec(),
                _ => return Ok(Value::Null),
            };
            let fmt_str = match fmt {
                Value::Text(s) => s.to_ascii_lowercase(),
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "format string 'base64' or 'hex'".into(),
                        got: "non-text".into(),
                    })
                }
            };
            match fmt_str.as_str() {
                "base64" => Ok(Value::Text(b64_encode(&bytes))),
                "hex" => Ok(Value::Text(hex_encode(&bytes))),
                other => Err(DbError::NotImplemented {
                    feature: format!("ENCODE format '{other}' — supported: 'base64', 'hex'"),
                }),
            }
        }

        // DECODE(text, format) → BLOB
        // Decodes a text representation to binary data.
        // format: 'base64' or 'hex'
        // Returns NULL on invalid input (MySQL behavior for base64; error for invalid hex).
        "decode" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let data = crate::eval::eval(&args[0], row)?;
            let fmt = crate::eval::eval(&args[1], row)?;
            let text = match data {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "text".into(),
                        got: "non-text".into(),
                    })
                }
            };
            let fmt_str = match fmt {
                Value::Text(s) => s.to_ascii_lowercase(),
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "format string 'base64' or 'hex'".into(),
                        got: "non-text".into(),
                    })
                }
            };
            match fmt_str.as_str() {
                "base64" => match b64_decode(text.trim()) {
                    Some(b) => Ok(Value::Bytes(b)),
                    None => Ok(Value::Null),
                },
                "hex" => match hex_decode(&text) {
                    Some(b) => Ok(Value::Bytes(b)),
                    None => Err(DbError::InvalidValue {
                        reason: format!("invalid hex string: '{text}'"),
                    }),
                },
                other => Err(DbError::NotImplemented {
                    feature: format!("DECODE format '{other}' — supported: 'base64', 'hex'"),
                }),
            }
        }

        // ── PostgreSQL byte/bit accessors (Phase 24.5) ───────────────────────
        // GET_BYTE(bytea, idx) → INT — value of the byte at 0-based index `idx`.
        // Raises InvalidValue if idx is out of bounds.
        "get_byte" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let bytes = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Bytes".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let idx = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            if idx < 0 || (idx as usize) >= bytes.len() {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "GET_BYTE: index {idx} out of range for {}-byte bytea",
                        bytes.len()
                    ),
                });
            }
            Ok(Value::Int(bytes[idx as usize] as i32))
        }

        // SET_BYTE(bytea, idx, value) → BYTEA — copy with byte at 0-based `idx`
        // replaced by the low 8 bits of `value`.
        "set_byte" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let mut bytes = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Bytes".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let idx = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let value = match crate::eval::eval(&args[2], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as u32,
                Value::BigInt(n) => n as u32,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            if idx < 0 || (idx as usize) >= bytes.len() {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "SET_BYTE: index {idx} out of range for {}-byte bytea",
                        bytes.len()
                    ),
                });
            }
            bytes[idx as usize] = (value & 0xFF) as u8;
            Ok(Value::Bytes(bytes))
        }

        // GET_BIT(bytea, idx) → INT — value (0/1) of the bit at 0-based `idx`,
        // counting MSB-first within each byte (PostgreSQL convention).
        "get_bit" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let bytes = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Bytes".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let idx = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            if idx < 0 || (idx as usize) >= bytes.len() * 8 {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "GET_BIT: index {idx} out of range for {}-byte bytea ({} bits)",
                        bytes.len(),
                        bytes.len() * 8
                    ),
                });
            }
            let byte_idx = (idx as usize) / 8;
            let bit_idx = 7 - ((idx as usize) % 8); // MSB-first
            Ok(Value::Int(((bytes[byte_idx] >> bit_idx) & 1) as i32))
        }

        // SET_BIT(bytea, idx, value) → BYTEA — copy with single bit at 0-based
        // `idx` set to 0 or 1 (MSB-first).
        "set_bit" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let mut bytes = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Bytes".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let idx = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let value = match crate::eval::eval(&args[2], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n,
                Value::BigInt(n) => n as i32,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            if idx < 0 || (idx as usize) >= bytes.len() * 8 {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "SET_BIT: index {idx} out of range for {}-byte bytea ({} bits)",
                        bytes.len(),
                        bytes.len() * 8
                    ),
                });
            }
            if value != 0 && value != 1 {
                return Err(DbError::InvalidValue {
                    reason: format!("SET_BIT: third arg must be 0 or 1, got {value}"),
                });
            }
            let byte_idx = (idx as usize) / 8;
            let bit_idx = 7 - ((idx as usize) % 8);
            let mask = 1u8 << bit_idx;
            if value == 1 {
                bytes[byte_idx] |= mask;
            } else {
                bytes[byte_idx] &= !mask;
            }
            Ok(Value::Bytes(bytes))
        }

        // BIT_COUNT(bytea | int) → BIGINT — number of set bits ("popcount").
        // Handles both bytea (PostgreSQL) and integer (MySQL) inputs.
        "bit_count" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match crate::eval::eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Bytes(b) => {
                    let total: u64 = b.iter().map(|&byte| byte.count_ones() as u64).sum();
                    Ok(Value::BigInt(total as i64))
                }
                Value::Int(n) => Ok(Value::BigInt(n.count_ones() as i64)),
                Value::BigInt(n) => Ok(Value::BigInt(n.count_ones() as i64)),
                other => Err(DbError::TypeMismatch {
                    expected: "Bytes or Int".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // ── Cryptographic hashes (Phase 24.5) ────────────────────────────────
        // MD5(text|bytea) → TEXT (hex). MySQL/PG-compatible.
        "md5" => hash_to_hex_text(args, row, HashAlgo::Md5),
        "sha1" => hash_to_hex_text(args, row, HashAlgo::Sha1),
        "sha224" => hash_to_hex_text(args, row, HashAlgo::Sha224),
        "sha256" => hash_to_hex_text(args, row, HashAlgo::Sha256),
        "sha384" => hash_to_hex_text(args, row, HashAlgo::Sha384),
        "sha512" => hash_to_hex_text(args, row, HashAlgo::Sha512),

        // Phase 11.2c: MIME_TYPE(blob_col) → detect content type from magic bytes.
        "mime_type" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match crate::eval::eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Bytes(b) => Ok(Value::Text(detect_mime_type(&b).to_string())),
                Value::Text(s) => Ok(Value::Text(detect_mime_type(s.as_bytes()).to_string())),
                _ => Ok(Value::Text("application/octet-stream".to_string())),
            }
        }

        _ => unreachable!("dispatcher routed unsupported binary function"),
    }
}

/// Detects MIME type from the first bytes of a blob (magic byte signatures).
///
/// Covers the 10 most common binary formats. Returns `"application/octet-stream"`
/// for unknown content. Zero-allocation (returns &str).
fn detect_mime_type(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 && bytes[..4] == [0x89, 0x50, 0x4E, 0x47] {
        "image/png"
    } else if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        "image/jpeg"
    } else if bytes.len() >= 4 && bytes[..4] == [0x47, 0x49, 0x46, 0x38] {
        "image/gif"
    } else if bytes.len() >= 12
        && bytes[..4] == [0x52, 0x49, 0x46, 0x46]
        && bytes[8..12] == [0x57, 0x45, 0x42, 0x50]
    {
        "image/webp"
    } else if bytes.len() >= 4 && bytes[..4] == [0x25, 0x50, 0x44, 0x46] {
        "application/pdf"
    } else if bytes.len() >= 4 && bytes[..4] == [0x50, 0x4B, 0x03, 0x04] {
        "application/zip"
    } else if bytes.len() >= 2 && bytes[..2] == [0x1F, 0x8B] {
        "application/gzip"
    } else if bytes.first() == Some(&0x7B) {
        "application/json"
    } else if bytes.first() == Some(&0x3C) {
        "text/xml"
    } else {
        "application/octet-stream"
    }
}

/// Encodes `bytes` to standard base64 (with `=` padding).
fn b64_encode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_CHARS[((n >> 18) & 0x3F) as usize]);
        out.push(B64_CHARS[((n >> 12) & 0x3F) as usize]);
        out.push(if chunk.len() > 1 {
            B64_CHARS[((n >> 6) & 0x3F) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            B64_CHARS[(n & 0x3F) as usize]
        } else {
            b'='
        });
    }
    // SAFETY: output contains only ASCII base64 characters + '='.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Decodes a base64 string. Returns `None` on invalid input.
///
/// Accepts standard base64 with or without `=` padding and ignores embedded
/// whitespace (newlines inserted by MySQL's TO_BASE64 for long strings).
fn b64_decode(input: &str) -> Option<Vec<u8>> {
    // Build reverse lookup table: ASCII → 6-bit value (0xFF = invalid).
    let mut rev = [0xFFu8; 256];
    for (i, &c) in B64_CHARS.iter().enumerate() {
        rev[c as usize] = i as u8;
    }

    let bytes: Vec<u8> = input
        .bytes()
        .filter(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .collect();

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i + 3 < bytes.len() {
        let c0 = rev[bytes[i] as usize];
        let c1 = rev[bytes[i + 1] as usize];
        let c2 = if bytes[i + 2] == b'=' {
            0u8
        } else {
            rev[bytes[i + 2] as usize]
        };
        let c3 = if bytes[i + 3] == b'=' {
            0u8
        } else {
            rev[bytes[i + 3] as usize]
        };
        if c0 == 0xFF || c1 == 0xFF || c2 == 0xFF || c3 == 0xFF {
            return None;
        }
        let n = ((c0 as u32) << 18) | ((c1 as u32) << 12) | ((c2 as u32) << 6) | (c3 as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if bytes[i + 2] != b'=' {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if bytes[i + 3] != b'=' {
            out.push((n & 0xFF) as u8);
        }
        i += 4;
    }
    if i != bytes.len() {
        return None; // input length not a multiple of 4
    }
    Some(out)
}

/// Encodes `bytes` to lowercase hex string (e.g. `"deadbeef"`).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodes a lowercase or uppercase hex string. Returns `None` on invalid input.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    // Accept optional "0x" / "\\x" prefix (PostgreSQL bytea hex format).
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix("\\x"))
        .unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16)? as u8;
            let lo = char::from(pair[1]).to_digit(16)? as u8;
            Some((hi << 4) | lo)
        })
        .collect()
}

// ── Hashing (Phase 24.5) ────────────────────────────────────────────────────

/// Cryptographic-hash family covered by MD5/SHA-1/SHA-2 SQL functions.
#[derive(Clone, Copy)]
enum HashAlgo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

/// Common dispatch for hash functions: NULL → NULL, Text/Bytes/Uuid → hex digest.
fn hash_to_hex_text(args: &[Expr], row: &[Value], algo: HashAlgo) -> Result<Value, DbError> {
    let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
        expected: "1 arg".into(),
        got: "0".into(),
    })?;
    let bytes: Vec<u8> = match crate::eval::eval(arg, row)? {
        Value::Null => return Ok(Value::Null),
        Value::Text(s) => s.into_bytes(),
        Value::Bytes(b) => b,
        Value::Uuid(u) => u.to_vec(),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "Text or Bytes".into(),
                got: other.variant_name().into(),
            })
        }
    };
    Ok(Value::Text(compute_hash(&bytes, algo)))
}

/// Hex-encoded digest of `bytes` under `algo`. Pure-Rust implementations from
/// `md-5` / `sha1` / `sha2` keep the dependency footprint small.
fn compute_hash(bytes: &[u8], algo: HashAlgo) -> String {
    use sha1::Digest as _;
    match algo {
        HashAlgo::Md5 => {
            let mut h = md5::Md5::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
        HashAlgo::Sha1 => {
            let mut h = sha1::Sha1::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
        HashAlgo::Sha224 => {
            let mut h = sha2::Sha224::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
        HashAlgo::Sha256 => {
            let mut h = sha2::Sha256::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
        HashAlgo::Sha384 => {
            let mut h = sha2::Sha384::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
        HashAlgo::Sha512 => {
            let mut h = sha2::Sha512::new();
            h.update(bytes);
            hex_encode(&h.finalize())
        }
    }
}
