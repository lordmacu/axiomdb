//! QueryResult → MySQL text protocol wire format.
//!
//! Converts `axiomdb_sql::QueryResult` into a sequence of MySQL packets:
//! - `Rows`     → column_count + column_defs + EOF + rows + EOF
//! - `Affected` → OK_Packet
//! - `Empty`    → OK_Packet

use axiomdb_sql::result::{ColumnMeta, QueryResult, Row};
use axiomdb_types::{DataType, Value};

use super::packets::{build_eof_packet, build_ok_packet, write_lenenc_int, write_lenenc_str};

/// A sequence of `(sequence_id, payload)` packets ready to be sent.
pub type PacketSeq = Vec<(u8, Vec<u8>)>;

/// Converts a `QueryResult` into MySQL wire protocol packets.
///
/// `seq_start` is the sequence_id for the first response packet
/// (usually 1, since the client's COM_QUERY was seq=0).
pub fn serialize_query_result(result: QueryResult, seq_start: u8) -> PacketSeq {
    match result {
        QueryResult::Rows { columns, rows } => serialize_rows(&columns, &rows, seq_start),
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            let last_id = last_insert_id.unwrap_or(0);
            vec![(seq_start, build_ok_packet(count, last_id, 0))]
        }
        QueryResult::Empty => {
            vec![(seq_start, build_ok_packet(0, 0, 0))]
        }
    }
}

fn serialize_rows(cols: &[ColumnMeta], rows: &[Row], seq_start: u8) -> PacketSeq {
    let mut packets = PacketSeq::new();
    let mut seq = seq_start;

    // 1. Column count packet
    let mut col_count_buf = Vec::with_capacity(2);
    write_lenenc_int(&mut col_count_buf, cols.len() as u64);
    packets.push((seq, col_count_buf));
    seq += 1;

    // 2. Column definition packets
    for col in cols {
        packets.push((seq, build_column_def(col)));
        seq += 1;
    }

    // 3. EOF after column defs
    packets.push((seq, build_eof_packet()));
    seq += 1;

    // 4. Row data packets
    for row in rows {
        packets.push((seq, build_row_packet(row)));
        seq += 1;
    }

    // 5. EOF after rows
    packets.push((seq, build_eof_packet()));

    packets
}

fn build_column_def(col: &ColumnMeta) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);

    write_lenenc_str(&mut buf, b"def"); // catalog
    write_lenenc_str(&mut buf, b""); // schema
    write_lenenc_str(&mut buf, b""); // table
    write_lenenc_str(&mut buf, b""); // org_table
    write_lenenc_str(&mut buf, col.name.as_bytes()); // name
    write_lenenc_str(&mut buf, col.name.as_bytes()); // org_name

    // Fixed-length section (12 bytes, introduced by lenenc_int = 0x0c)
    write_lenenc_int(&mut buf, 0x0c);
    // character_set = 255 (utf8mb4_0900_ai_ci)
    buf.extend_from_slice(&255u16.to_le_bytes());
    // column_length (display width) — use type-dependent default
    let col_len = column_display_len(col.data_type);
    buf.extend_from_slice(&col_len.to_le_bytes());
    // type byte
    buf.push(datatype_to_mysql_type(col.data_type));
    // flags
    buf.extend_from_slice(&column_flags(col.data_type).to_le_bytes());
    // decimals
    buf.push(column_decimals(col.data_type));
    // filler
    buf.extend_from_slice(&0u16.to_le_bytes());

    buf
}

fn build_row_packet(row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in row {
        match value {
            Value::Null => buf.push(0xfb), // NULL indicator
            v => {
                let s = value_to_text(v);
                write_lenenc_str(&mut buf, s.as_bytes());
            }
        }
    }
    buf
}

// ── Type mappings ─────────────────────────────────────────────────────────────

fn datatype_to_mysql_type(dt: DataType) -> u8 {
    match dt {
        DataType::Bool => 0x10,      // BIT (rendered as TINYINT)
        DataType::Int => 0x03,       // LONG
        DataType::BigInt => 0x08,    // LONGLONG
        DataType::Real => 0x05,      // DOUBLE
        DataType::Decimal => 0x00,   // DECIMAL
        DataType::Text => 0xfd,      // VAR_STRING
        DataType::Bytes => 0xfc,     // BLOB
        DataType::Date => 0x0a,      // DATE
        DataType::Timestamp => 0x07, // TIMESTAMP
        DataType::Uuid => 0xfd,      // VAR_STRING
    }
}

fn column_display_len(dt: DataType) -> u32 {
    match dt {
        DataType::Bool => 1,
        DataType::Int => 11,
        DataType::BigInt => 20,
        DataType::Real => 22,
        DataType::Decimal => 65,
        DataType::Text => 16_777_215,
        DataType::Bytes => 16_777_215,
        DataType::Date => 10,
        DataType::Timestamp => 19,
        DataType::Uuid => 36,
    }
}

fn column_flags(dt: DataType) -> u16 {
    match dt {
        DataType::Bool => 0x0020, // UNSIGNED
        DataType::Int => 0x0000,
        DataType::BigInt => 0x0000,
        _ => 0x0000,
    }
}

fn column_decimals(dt: DataType) -> u8 {
    match dt {
        DataType::Real => 31,
        DataType::Decimal => 10,
        _ => 0,
    }
}

// ── Value → text encoding ─────────────────────────────────────────────────────

fn value_to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(), // should not be called (handled above)
        Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => {
            // Use enough precision for round-trip; avoid scientific notation for small numbers.
            if f.abs() < 1e15 && f.abs() > 1e-4 || *f == 0.0 {
                format!("{f}")
            } else {
                format!("{f:e}")
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Decimal(m, s) => format_decimal(*m, *s),
        Value::Date(d) => format_date(*d),
        Value::Timestamp(t) => format_timestamp(*t),
        Value::Uuid(u) => format_uuid(u),
    }
}

fn format_decimal(mantissa: i128, scale: u8) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let s = mantissa.unsigned_abs().to_string();
    let scale = scale as usize;
    let sign = if mantissa < 0 { "-" } else { "" };
    if s.len() <= scale {
        let zeros = "0".repeat(scale - s.len() + 1);
        format!("{sign}{zeros}.{s}")
    } else {
        let (int_part, frac_part) = s.split_at(s.len() - scale);
        format!("{sign}{int_part}.{frac_part}")
    }
}

fn format_date(days_since_epoch: i32) -> String {
    // days_since_epoch: days since 1970-01-01 (same as chrono NaiveDate)
    // Simple implementation using arithmetic (avoids chrono dependency).
    let d = i64::from(days_since_epoch);
    let (year, month, day) = days_to_ymd(d);
    format!("{year:04}-{month:02}-{day:02}")
}

fn format_timestamp(micros: i64) -> String {
    let secs = micros / 1_000_000;
    let (year, month, day) = days_to_ymd(secs / 86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Converts days since Unix epoch to (year, month, day).
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Algorithm: https://howardhinnant.github.io/date_algorithms.html#civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_date_epoch() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1), "1970-01-02");
        assert_eq!(format_date(365), "1971-01-01");
    }

    #[test]
    fn test_format_decimal() {
        assert_eq!(format_decimal(12345, 2), "123.45");
        assert_eq!(format_decimal(-100, 2), "-1.00");
        assert_eq!(format_decimal(5, 0), "5");
    }

    #[test]
    fn test_lenenc_int_boundaries() {
        let mut buf = Vec::new();
        write_lenenc_int(&mut buf, 0);
        assert_eq!(buf, [0x00]);

        let mut buf = Vec::new();
        write_lenenc_int(&mut buf, 250);
        assert_eq!(buf, [0xfa]);

        let mut buf = Vec::new();
        write_lenenc_int(&mut buf, 251);
        assert_eq!(buf, [0xfc, 0xfb, 0x00]); // 251 as u16 LE = 0xfb 0x00
    }
}
