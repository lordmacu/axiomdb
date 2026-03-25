//! QueryResult → MySQL wire format (text and binary protocols).
//!
//! Two public serializers:
//! - [`serialize_query_result`] — text protocol (COM_QUERY)
//! - [`serialize_query_result_binary`] — binary protocol (COM_STMT_EXECUTE)
//!
//! Both produce:
//! - `Rows`     → column_count + column_defs + EOF + rows + EOF
//! - `Affected` → OK_Packet
//! - `Empty`    → OK_Packet

use axiomdb_sql::result::{ColumnMeta, QueryResult, Row};
use axiomdb_types::{DataType, Value};

use super::packets::{
    build_eof_packet, build_eof_with_status, build_ok_with_status, write_lenenc_int,
    write_lenenc_str,
};

/// MySQL `SERVER_MORE_RESULTS_EXISTS` status flag (0x0008).
///
/// Set in the final EOF/OK of every intermediate result set in a multi-statement
/// response so the client knows to read the next result set (Phase 5.12).
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;
const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

/// A sequence of `(sequence_id, payload)` packets ready to be sent.
pub type PacketSeq = Vec<(u8, Vec<u8>)>;

/// Converts a `QueryResult` into MySQL wire protocol packets.
///
/// `seq_start` is the sequence_id for the first response packet
/// (usually 1, since the client's COM_QUERY was seq=0).
///
/// `more_results` sets `SERVER_MORE_RESULTS_EXISTS` in the final EOF/OK packet,
/// telling the client that another result set follows (used by COM_QUERY
/// multi-statement responses, Phase 5.12).
pub fn serialize_query_result(result: QueryResult, seq_start: u8) -> PacketSeq {
    serialize_query_result_multi(result, seq_start, false)
}

/// Like [`serialize_query_result`] but with explicit `more_results` flag.
pub fn serialize_query_result_multi(
    result: QueryResult,
    seq_start: u8,
    more_results: bool,
) -> PacketSeq {
    serialize_query_result_multi_warn(result, seq_start, more_results, 0)
}

pub fn serialize_query_result_multi_warn(
    result: QueryResult,
    seq_start: u8,
    more_results: bool,
    warning_count: u16,
) -> PacketSeq {
    let status = if more_results {
        SERVER_STATUS_AUTOCOMMIT | SERVER_MORE_RESULTS_EXISTS
    } else {
        SERVER_STATUS_AUTOCOMMIT
    };

    match result {
        QueryResult::Rows { columns, rows } => serialize_rows(&columns, &rows, seq_start, status),
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            let last_id = last_insert_id.unwrap_or(0);
            vec![(
                seq_start,
                build_ok_with_status(count, last_id, warning_count, status),
            )]
        }
        QueryResult::Empty => {
            vec![(seq_start, build_ok_with_status(0, 0, warning_count, status))]
        }
    }
}

// ── Binary protocol serializer (COM_STMT_EXECUTE) ─────────────────────────────

/// Converts a `QueryResult` into MySQL **binary** protocol packets for
/// `COM_STMT_EXECUTE` responses.
///
/// Row encoding differs from the text protocol:
/// - Row header is `0x00` (not per-cell NULL markers)
/// - NULL values are encoded in a compact null bitmap (offset 2)
/// - Numeric types are fixed-width little-endian, not ASCII text
/// - DATE / TIMESTAMP use compact binary payloads
///
/// Non-row results (`Affected`, `Empty`) still use OK_Packets, identical to the
/// text protocol.
pub fn serialize_query_result_binary(result: QueryResult, seq_start: u8) -> PacketSeq {
    match result {
        QueryResult::Rows { columns, rows } => {
            serialize_rows_binary(&columns, &rows, seq_start, SERVER_STATUS_AUTOCOMMIT)
        }
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            let last_id = last_insert_id.unwrap_or(0);
            vec![(
                seq_start,
                build_ok_with_status(count, last_id, 0, SERVER_STATUS_AUTOCOMMIT),
            )]
        }
        QueryResult::Empty => {
            vec![(
                seq_start,
                build_ok_with_status(0, 0, 0, SERVER_STATUS_AUTOCOMMIT),
            )]
        }
    }
}

fn serialize_rows_binary(
    cols: &[ColumnMeta],
    rows: &[Row],
    seq_start: u8,
    final_status: u16,
) -> PacketSeq {
    let mut packets = PacketSeq::new();
    let mut seq = seq_start;

    // 1. Column count
    let mut col_count_buf = Vec::with_capacity(2);
    write_lenenc_int(&mut col_count_buf, cols.len() as u64);
    packets.push((seq, col_count_buf));
    seq += 1;

    // 2. Column definition packets (shared with text path)
    for col in cols {
        packets.push((seq, build_column_def(col)));
        seq += 1;
    }

    // 3. EOF after column defs
    packets.push((seq, build_eof_packet()));
    seq += 1;

    // 4. Binary row packets
    for row in rows {
        packets.push((seq, build_binary_row_packet(cols, row)));
        seq += 1;
    }

    // 5. Final EOF
    packets.push((seq, build_eof_with_status(final_status)));

    packets
}

/// Builds one binary row packet payload.
///
/// Layout:
/// ```text
/// 0x00                        (row header)
/// null_bitmap[bitmap_len]     (MySQL offset-2 null bitmap)
/// value_1 ... value_n         (only non-null values, in column order)
/// ```
fn build_binary_row_packet(cols: &[ColumnMeta], row: &[Value]) -> Vec<u8> {
    debug_assert_eq!(cols.len(), row.len());

    let bitmap_len = (cols.len() + 7 + 2) / 8;
    let mut buf = Vec::with_capacity(1 + bitmap_len + cols.len() * 8);
    buf.push(0x00); // binary row header

    let bitmap_start = buf.len();
    buf.resize(bitmap_start + bitmap_len, 0);

    for (idx, (col, value)) in cols.iter().zip(row.iter()).enumerate() {
        if matches!(value, Value::Null) {
            set_binary_null_bit(&mut buf[bitmap_start..bitmap_start + bitmap_len], idx);
            continue;
        }
        encode_binary_cell(&mut buf, col.data_type, value);
    }

    buf
}

/// Sets the null-bitmap bit for `field_index` using MySQL's prepared-row
/// offset of 2 (bits 0 and 1 are reserved).
fn set_binary_null_bit(bitmap: &mut [u8], field_index: usize) {
    let shifted = field_index + 2;
    let byte = shifted / 8;
    let bit = shifted % 8;
    bitmap[byte] |= 1 << bit;
}

/// Encodes one non-null cell value using the MySQL binary row protocol.
fn encode_binary_cell(buf: &mut Vec<u8>, data_type: DataType, value: &Value) {
    match (data_type, value) {
        (DataType::Bool, Value::Bool(v)) => buf.push(u8::from(*v)),
        (DataType::Int, Value::Int(v)) => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::BigInt, Value::BigInt(v)) => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::Real, Value::Real(v)) => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::Decimal, Value::Decimal(m, s)) => {
            write_lenenc_str(buf, format_decimal(*m, *s).as_bytes())
        }
        (DataType::Text, Value::Text(s)) => write_lenenc_str(buf, s.as_bytes()),
        (DataType::Bytes, Value::Bytes(b)) => write_lenenc_str(buf, b),
        (DataType::Date, Value::Date(days)) => encode_binary_date(buf, *days),
        (DataType::Timestamp, Value::Timestamp(ts)) => encode_binary_timestamp(buf, *ts),
        (DataType::Uuid, Value::Uuid(u)) => write_lenenc_str(buf, format_uuid(u).as_bytes()),
        // Any mismatch is a QueryResult invariant violation — not a user-visible error.
        (_, other) => unreachable!("binary cell type/value mismatch: {other:?}"),
    }
}

/// Encodes a DATE value as the MySQL binary date payload.
///
/// Format: `[4][year u16 LE][month u8][day u8]`
fn encode_binary_date(buf: &mut Vec<u8>, days_since_epoch: i32) {
    let (year, month, day) = days_to_ymd(i64::from(days_since_epoch));
    buf.push(4);
    buf.extend_from_slice(&(year as u16).to_le_bytes());
    buf.push(month as u8);
    buf.push(day as u8);
}

/// Encodes a TIMESTAMP value as the MySQL binary datetime payload.
///
/// - 7 bytes when microseconds are zero:  `[7][year u16][month][day][h][m][s]`
/// - 11 bytes when microseconds are non-zero: same + `[micros u32 LE]`
fn encode_binary_timestamp(buf: &mut Vec<u8>, micros: i64) {
    let secs = micros.div_euclid(1_000_000);
    let micros_part = micros.rem_euclid(1_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = days_to_ymd(days);
    let hour = (rem / 3_600) as u8;
    let min = ((rem % 3_600) / 60) as u8;
    let sec = (rem % 60) as u8;

    if micros_part == 0 {
        buf.push(7);
    } else {
        buf.push(11);
    }
    buf.extend_from_slice(&(year as u16).to_le_bytes());
    buf.push(month as u8);
    buf.push(day as u8);
    buf.push(hour);
    buf.push(min);
    buf.push(sec);
    if micros_part != 0 {
        buf.extend_from_slice(&micros_part.to_le_bytes());
    }
}

// ── Text protocol serializer (COM_QUERY) ──────────────────────────────────────

fn serialize_rows(
    cols: &[ColumnMeta],
    rows: &[Row],
    seq_start: u8,
    final_status: u16,
) -> PacketSeq {
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

    // 3. EOF after column defs (always normal — MORE_RESULTS only on the last EOF)
    packets.push((seq, build_eof_packet()));
    seq += 1;

    // 4. Row data packets
    for row in rows {
        packets.push((seq, build_row_packet(row)));
        seq += 1;
    }

    // 5. Final EOF — carries MORE_RESULTS flag when there are more statements
    packets.push((seq, build_eof_with_status(final_status)));

    packets
}

pub(crate) fn build_column_def_pub(col: &ColumnMeta) -> Vec<u8> {
    build_column_def(col)
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
        DataType::Bool => 0x01,      // TINY
        DataType::Int => 0x03,       // LONG
        DataType::BigInt => 0x08,    // LONGLONG
        DataType::Real => 0x05,      // DOUBLE
        DataType::Decimal => 0xf6,   // NEWDECIMAL
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

pub(crate) fn format_decimal_pub(mantissa: i128, scale: u8) -> String {
    format_decimal(mantissa, scale)
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

pub(crate) fn format_date_pub(days_since_epoch: i32) -> String {
    format_date(days_since_epoch)
}

fn format_date(days_since_epoch: i32) -> String {
    // days_since_epoch: days since 1970-01-01 (same as chrono NaiveDate)
    // Simple implementation using arithmetic (avoids chrono dependency).
    let d = i64::from(days_since_epoch);
    let (year, month, day) = days_to_ymd(d);
    format!("{year:04}-{month:02}-{day:02}")
}

pub(crate) fn format_timestamp_pub(micros: i64) -> String {
    format_timestamp(micros)
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
    use axiomdb_sql::result::ColumnMeta;

    // ── Text protocol helpers ─────────────────────────────────────────────────

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

    // ── Binary row packet ─────────────────────────────────────────────────────

    fn make_col(name: &str, dt: DataType) -> ColumnMeta {
        ColumnMeta::computed(name.to_string(), dt)
    }

    #[test]
    fn test_binary_row_header_is_0x00() {
        let cols = vec![make_col("x", DataType::Int)];
        let row = vec![Value::Int(1)];
        let pkt = build_binary_row_packet(&cols, &row);
        assert_eq!(pkt[0], 0x00, "binary row header must be 0x00");
    }

    #[test]
    fn test_binary_null_bitmap_offset_2() {
        // Column 0 NULL → bit position 2 in byte 0 (offset 2 means bit 2 of byte 0)
        let cols = vec![make_col("a", DataType::Int)];
        let row = vec![Value::Null];
        let pkt = build_binary_row_packet(&cols, &row);
        // bitmap byte 0 = bit 2 set = 0x04
        assert_eq!(
            pkt[1], 0x04,
            "null at col 0 must set bit 2 of bitmap byte 0"
        );
        // no value bytes after the bitmap
        assert_eq!(
            pkt.len(),
            2,
            "null row must have only header + 1 bitmap byte"
        );
    }

    #[test]
    fn test_binary_null_bitmap_col1() {
        // Column 1 NULL → bit position 3 in byte 0
        let cols = vec![make_col("a", DataType::Int), make_col("b", DataType::Int)];
        let row = vec![Value::Int(42), Value::Null];
        let pkt = build_binary_row_packet(&cols, &row);
        // col 1 → shifted = 3 → byte 0 bit 3 = 0x08
        assert_eq!(
            pkt[1] & 0x08,
            0x08,
            "null at col 1 must set bit 3 of bitmap byte 0"
        );
    }

    #[test]
    fn test_binary_bigint_is_8_byte_le() {
        let cols = vec![make_col("n", DataType::BigInt)];
        let row = vec![Value::BigInt(0x0102030405060708_i64)];
        let pkt = build_binary_row_packet(&cols, &row);
        // header(1) + bitmap(1) + value(8) = 10 bytes
        assert_eq!(pkt.len(), 10);
        let val = i64::from_le_bytes(pkt[2..10].try_into().unwrap());
        assert_eq!(val, 0x0102030405060708_i64);
    }

    #[test]
    fn test_binary_int_is_4_byte_le() {
        let cols = vec![make_col("n", DataType::Int)];
        let row = vec![Value::Int(-1)];
        let pkt = build_binary_row_packet(&cols, &row);
        assert_eq!(pkt.len(), 6); // 1 + 1 + 4
        let val = i32::from_le_bytes(pkt[2..6].try_into().unwrap());
        assert_eq!(val, -1);
    }

    #[test]
    fn test_binary_bool_is_one_byte() {
        let cols = vec![make_col("b", DataType::Bool)];
        let row_t = vec![Value::Bool(true)];
        let row_f = vec![Value::Bool(false)];
        let pkt_t = build_binary_row_packet(&cols, &row_t);
        let pkt_f = build_binary_row_packet(&cols, &row_f);
        assert_eq!(pkt_t.len(), 3); // 1 + 1 + 1
        assert_eq!(pkt_t[2], 0x01);
        assert_eq!(pkt_f[2], 0x00);
    }

    #[test]
    fn test_binary_decimal_is_lenenc_ascii() {
        let cols = vec![make_col("d", DataType::Decimal)];
        let row = vec![Value::Decimal(12345, 2)]; // 123.45
        let pkt = build_binary_row_packet(&cols, &row);
        // After header(1) + bitmap(1): lenenc length byte + "123.45"
        assert_eq!(pkt[2], 6); // lenenc length = 6
        assert_eq!(&pkt[3..9], b"123.45");
    }

    #[test]
    fn test_binary_bytes_preserved_raw() {
        let cols = vec![make_col("b", DataType::Bytes)];
        let raw = vec![0x00, 0xff, 0x42]; // contains null byte
        let row = vec![Value::Bytes(raw.clone())];
        let pkt = build_binary_row_packet(&cols, &row);
        assert_eq!(pkt[2], 3); // lenenc length = 3
        assert_eq!(&pkt[3..6], raw.as_slice());
    }

    #[test]
    fn test_binary_date_payload() {
        // 2024-01-15: days since epoch = 19737
        let cols = vec![make_col("d", DataType::Date)];
        let row = vec![Value::Date(19737)];
        let pkt = build_binary_row_packet(&cols, &row);
        // header(1) + bitmap(1) + length_byte(1) + year_u16(2) + month(1) + day(1) = 7
        assert_eq!(pkt.len(), 7);
        assert_eq!(pkt[2], 4); // length byte = 4
        let year = u16::from_le_bytes([pkt[3], pkt[4]]);
        let month = pkt[5];
        let day = pkt[6];
        assert_eq!(year, 2024);
        assert_eq!(month, 1);
        assert_eq!(day, 15);
    }

    #[test]
    fn test_binary_timestamp_7_bytes_when_no_micros() {
        // 2024-01-15 10:30:00 UTC = 19737 days * 86400 + 10*3600 + 30*60 = 1705314600 secs
        let micros: i64 = 1_705_314_600 * 1_000_000;
        let cols = vec![make_col("ts", DataType::Timestamp)];
        let row = vec![Value::Timestamp(micros)];
        let pkt = build_binary_row_packet(&cols, &row);
        // header(1) + bitmap(1) + len(1) + year(2) + month(1) + day(1) + h(1) + m(1) + s(1) = 10
        assert_eq!(pkt.len(), 10);
        assert_eq!(pkt[2], 7); // length byte = 7 (no micros)
        let year = u16::from_le_bytes([pkt[3], pkt[4]]);
        assert_eq!(year, 2024);
        assert_eq!(pkt[5], 1); // month
        assert_eq!(pkt[6], 15); // day
        assert_eq!(pkt[7], 10); // hour
        assert_eq!(pkt[8], 30); // min
        assert_eq!(pkt[9], 0); // sec
    }

    #[test]
    fn test_binary_timestamp_11_bytes_when_micros_nonzero() {
        let micros: i64 = 1_705_314_600 * 1_000_000 + 123_456;
        let cols = vec![make_col("ts", DataType::Timestamp)];
        let row = vec![Value::Timestamp(micros)];
        let pkt = build_binary_row_packet(&cols, &row);
        // header(1) + bitmap(1) + len(1) + year(2) + month(1) + day(1) + h(1) + m(1) + s(1) + micros(4) = 14
        assert_eq!(pkt.len(), 14);
        assert_eq!(pkt[2], 11); // length byte = 11
        let micros_decoded = u32::from_le_bytes(pkt[10..14].try_into().unwrap());
        assert_eq!(micros_decoded, 123_456);
    }

    // ── Type-code alignment ───────────────────────────────────────────────────

    #[test]
    fn test_bool_advertises_tiny_not_bit() {
        assert_eq!(
            datatype_to_mysql_type(DataType::Bool),
            0x01,
            "Bool must advertise TINY (0x01)"
        );
    }

    #[test]
    fn test_decimal_advertises_newdecimal() {
        assert_eq!(
            datatype_to_mysql_type(DataType::Decimal),
            0xf6,
            "Decimal must advertise NEWDECIMAL (0xf6)"
        );
    }

    // ── Text protocol regression ──────────────────────────────────────────────

    #[test]
    fn test_text_null_still_0xfb() {
        let cols = vec![ColumnMeta::computed("x".to_string(), DataType::Text)];
        let rows = vec![vec![Value::Null]];
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        let packets = serialize_query_result(qr, 1);
        let row_pkt = packets.iter().find(|(seq, _)| *seq == 4).unwrap();
        assert_eq!(row_pkt.1[0], 0xfb, "text protocol NULL must remain 0xfb");
    }

    #[test]
    fn test_binary_framing_packet_sequence() {
        // 1 column, 1 row → col_count(1) + col_def(2) + EOF(3) + row(4) + EOF(5)
        let cols = vec![ColumnMeta::computed("n".to_string(), DataType::Int)];
        let rows = vec![vec![Value::Int(1)]];
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        let packets = serialize_query_result_binary(qr, 1);
        let seqs: Vec<u8> = packets.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
    }
}
