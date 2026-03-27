use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── UUID functions (4.19c) ───────────────────────────────────────────

        // gen_random_uuid() / uuid_generate_v4() — UUID v4 (random)
        "gen_random_uuid" | "uuid_generate_v4" | "random_uuid" | "newid" => {
            use rand::RngCore;
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            // Set version = 4 (bits 12-15 of octet 6)
            bytes[6] = (bytes[6] & 0x0F) | 0x40;
            // Set variant = RFC 4122 (bits 6-7 of octet 8)
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            Ok(Value::Uuid(bytes))
        }

        // uuid_generate_v7() — UUID v7 (time-ordered, monotonic)
        // Format: [48-bit unix_ms][4-bit ver=7][12-bit rand][2-bit var][62-bit rand]
        // Better B-Tree index locality than v4 because keys are time-ordered.
        "uuid_generate_v7" | "uuid7" => {
            use rand::RngCore;
            use std::time::{SystemTime, UNIX_EPOCH};
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            // Embed 48-bit timestamp in the first 6 bytes
            bytes[0] = ((ms >> 40) & 0xFF) as u8;
            bytes[1] = ((ms >> 32) & 0xFF) as u8;
            bytes[2] = ((ms >> 24) & 0xFF) as u8;
            bytes[3] = ((ms >> 16) & 0xFF) as u8;
            bytes[4] = ((ms >> 8) & 0xFF) as u8;
            bytes[5] = (ms & 0xFF) as u8;
            // Set version = 7
            bytes[6] = (bytes[6] & 0x0F) | 0x70;
            // Set variant = RFC 4122
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            Ok(Value::Uuid(bytes))
        }

        // is_valid_uuid(text) → BOOL — returns true if text is a valid UUID string
        "is_valid_uuid" | "is_uuid" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match crate::eval::eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Bool(parse_uuid_str(&s).is_some())),
                Value::Uuid(_) => Ok(Value::Bool(true)),
                _ => Ok(Value::Bool(false)),
            }
        }

        _ => unreachable!("dispatcher routed unsupported UUID function"),
    }
}

/// Parses a UUID string in the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
/// format and returns the 16 raw bytes, or `None` if the format is invalid.
fn parse_uuid_str(s: &str) -> Option<[u8; 16]> {
    // Accept both hyphenated (36 chars) and compact (32 chars) forms.
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = char::from(chunk[0]).to_digit(16)? as u8;
        let lo = char::from(chunk[1]).to_digit(16)? as u8;
        bytes[i] = (hi << 4) | lo;
    }
    Some(bytes)
}
