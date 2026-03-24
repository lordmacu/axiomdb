//! ASCII table formatter for query results.
//!
//! Renders a `QueryResult::Rows` as a bordered table:
//!
//! ```text
//! +----+-------+-----+
//! | id | name  | age |
//! +----+-------+-----+
//! |  1 | Alice |  30 |
//! |  2 | Bob   |  25 |
//! +----+-------+-----+
//! 2 rows (3ms)
//! ```
//!
//! Alignment rules:
//! - Numeric types (`INT`, `BIGINT`, `REAL`, `DOUBLE`) → right-aligned.
//! - All others → left-aligned.
//!
//! Long values are truncated to 64 characters with a `…` suffix to keep the
//! table readable in narrow terminals.

use std::time::Duration;

use axiomdb_sql::result::{ColumnMeta, Row};
use axiomdb_types::{DataType, Value};

const MAX_CELL: usize = 64;

/// Formats a result set as a printable ASCII table string.
///
/// The returned string includes the border, header, rows, and a summary line
/// (`N rows (Xms)`). The trailing newline is included.
pub fn format_table(cols: &[ColumnMeta], rows: &[Row], elapsed: Duration) -> String {
    if cols.is_empty() {
        return String::new();
    }

    // ── Render all cell values to strings ────────────────────────────────────
    let headers: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(cols.iter())
                .map(|(v, _col)| render_value(v))
                .collect()
        })
        .collect();

    // ── Compute column widths ─────────────────────────────────────────────────
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rendered {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    // ── Build the table ───────────────────────────────────────────────────────
    let sep = make_separator(&widths);
    let mut out = String::new();

    out.push_str(&sep);
    out.push_str(&make_header_row(&headers, &widths));
    out.push_str(&sep);

    for row in &rendered {
        out.push_str(&make_data_row(row, cols, &widths));
    }

    out.push_str(&sep);

    // Summary line
    let ms = elapsed.as_millis();
    let n = rows.len();
    let row_word = if n == 1 { "row" } else { "rows" };
    out.push_str(&format!("{n} {row_word} ({ms}ms)\n"));

    out
}

/// Formats a single value for display in a cell.
fn render_value(v: &Value) -> String {
    let s = match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Uuid(bytes) => format_uuid(bytes),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Text(s) => s.clone(),
        _ => format!("{v}"),
    };
    // Truncate long values to keep the table readable.
    if s.len() > MAX_CELL {
        let mut truncated = s.chars().take(MAX_CELL - 1).collect::<String>();
        truncated.push('…');
        truncated
    } else {
        s
    }
}

/// Returns `true` if the column type should be right-aligned (numeric).
fn is_numeric(col: &ColumnMeta) -> bool {
    matches!(
        col.data_type,
        DataType::Int | DataType::BigInt | DataType::Real
    )
}

/// Builds a separator row: `+----+-------+-----+\n`
fn make_separator(widths: &[usize]) -> String {
    let mut s = String::from("+");
    for &w in widths {
        for _ in 0..w + 2 {
            s.push('-');
        }
        s.push('+');
    }
    s.push('\n');
    s
}

/// Builds the header row: `| id | name  | age |\n`
fn make_header_row(headers: &[String], widths: &[usize]) -> String {
    let mut s = String::from("|");
    for (h, &w) in headers.iter().zip(widths.iter()) {
        s.push_str(&format!(" {:<width$} |", h, width = w));
    }
    s.push('\n');
    s
}

/// Builds a data row with proper alignment per column type.
fn make_data_row(cells: &[String], cols: &[ColumnMeta], widths: &[usize]) -> String {
    let mut s = String::from("|");
    for (i, cell) in cells.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(cell.len());
        let col = cols.get(i);
        let right_align = col.map(is_numeric).unwrap_or(false);
        if right_align {
            s.push_str(&format!(" {:>width$} |", cell, width = w));
        } else {
            s.push_str(&format!(" {:<width$} |", cell, width = w));
        }
    }
    s.push('\n');
    s
}

/// Formats 16 UUID bytes as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_types::Value;

    fn col(name: &str, dt: DataType) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            data_type: dt,
            nullable: true,
            table_name: None,
        }
    }

    #[test]
    fn test_format_empty_rows() {
        let cols = vec![col("id", DataType::Int), col("name", DataType::Text)];
        let out = format_table(&cols, &[], Duration::from_millis(1));
        assert!(out.contains("| id | name |"), "header missing in: {out}");
        assert!(out.contains("0 rows (1ms)"), "summary missing in: {out}");
    }

    #[test]
    fn test_format_two_rows() {
        let cols = vec![col("id", DataType::Int), col("name", DataType::Text)];
        let rows: Vec<Row> = vec![
            vec![Value::Int(1), Value::Text("Alice".into())],
            vec![Value::Int(2), Value::Text("Bob".into())],
        ];
        let out = format_table(&cols, &rows, Duration::from_millis(2));
        assert!(out.contains("Alice"), "Alice missing");
        assert!(out.contains("Bob"), "Bob missing");
        assert!(out.contains("2 rows (2ms)"), "summary missing");
    }

    #[test]
    fn test_null_display() {
        let cols = vec![col("val", DataType::Text)];
        let rows: Vec<Row> = vec![vec![Value::Null]];
        let out = format_table(&cols, &rows, Duration::ZERO);
        assert!(out.contains("NULL"), "NULL missing in: {out}");
    }

    #[test]
    fn test_bytes_display() {
        let cols = vec![col("data", DataType::Bytes)];
        let rows: Vec<Row> = vec![vec![Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])]];
        let out = format_table(&cols, &rows, Duration::ZERO);
        assert!(out.contains("<4 bytes>"), "bytes display missing in: {out}");
    }

    #[test]
    fn test_numeric_column_right_aligned() {
        let cols = vec![col("n", DataType::Int)];
        let rows: Vec<Row> = vec![vec![Value::Int(42)]];
        let out = format_table(&cols, &rows, Duration::ZERO);
        // Right-aligned: " 42 |" (space before value)
        assert!(out.contains(" 42 |"), "right-align missing in: {out}");
    }

    #[test]
    fn test_long_value_truncated() {
        let cols = vec![col("s", DataType::Text)];
        let long = "a".repeat(100);
        let rows: Vec<Row> = vec![vec![Value::Text(long)]];
        let out = format_table(&cols, &rows, Duration::ZERO);
        assert!(out.contains('…'), "truncation marker missing in: {out}");
    }

    #[test]
    fn test_bool_display() {
        let cols = vec![col("flag", DataType::Bool)];
        let rows: Vec<Row> = vec![vec![Value::Bool(true)], vec![Value::Bool(false)]];
        let out = format_table(&cols, &rows, Duration::ZERO);
        assert!(out.contains("true"), "true missing");
        assert!(out.contains("false"), "false missing");
    }
}
