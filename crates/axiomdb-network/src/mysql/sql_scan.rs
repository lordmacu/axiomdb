use axiomdb_core::error::DbError;
use axiomdb_types::Value;

fn next_char_len(sql: &str, idx: usize) -> usize {
    sql[idx..]
        .chars()
        .next()
        .map(|ch| ch.len_utf8())
        .unwrap_or(1)
}

fn is_line_comment_start(bytes: &[u8], idx: usize) -> bool {
    bytes[idx] == b'#' || (idx + 1 < bytes.len() && bytes[idx] == b'-' && bytes[idx + 1] == b'-')
}

fn is_block_comment_start(bytes: &[u8], idx: usize) -> bool {
    idx + 1 < bytes.len() && bytes[idx] == b'/' && bytes[idx + 1] == b'*'
}

fn skip_line_comment(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_block_comment(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start + 2;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_delimited_identifier(sql: &str, start: usize, quote: u8) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += next_char_len(sql, i);
        }
    }
    bytes.len()
}

fn decode_single_quoted_string(sql: &str, start: usize) -> (usize, String) {
    let bytes = sql.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return (bytes.len(), out);
                }
                let next = sql[i + 1..].chars().next().unwrap();
                match next {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    'b' => out.push('\x08'),
                    'Z' => out.push('\x1A'),
                    other => out.push(other),
                }
                i += 1 + next.len_utf8();
            }
            b'\'' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    out.push('\'');
                    i += 2;
                } else {
                    return (i + 1, out);
                }
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (bytes.len(), out)
}

fn decode_double_quoted_string(sql: &str, start: usize) -> (usize, String) {
    let bytes = sql.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return (bytes.len(), out);
                }
                let next = sql[i + 1..].chars().next().unwrap();
                match next {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    'b' => out.push('\x08'),
                    'Z' => out.push('\x1A'),
                    other => out.push(other),
                }
                i += 1 + next.len_utf8();
            }
            b'"' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    out.push('"');
                    i += 2;
                } else {
                    return (i + 1, out);
                }
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (bytes.len(), out)
}

pub(crate) fn count_params(sql: &str, ansi_quotes: bool) -> u16 {
    count_question_marks(sql, ansi_quotes).min(u16::MAX as usize) as u16
}

pub(crate) fn count_question_marks(sql: &str, ansi_quotes: bool) -> usize {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = decode_single_quoted_string(sql, i).0,
            b'"' if ansi_quotes => i = skip_delimited_identifier(sql, i, b'"'),
            b'"' => i = decode_double_quoted_string(sql, i).0,
            b'`' => i = skip_delimited_identifier(sql, i, b'`'),
            b'?' => {
                count += 1;
                i += 1;
            }
            _ if is_line_comment_start(bytes, i) => i = skip_line_comment(sql, i),
            _ if is_block_comment_start(bytes, i) => i = skip_block_comment(sql, i),
            _ => i += next_char_len(sql, i),
        }
    }
    count
}

pub(crate) fn substitute_params_with<F>(
    template: &str,
    ansi_quotes: bool,
    mut replace: F,
) -> Result<String, DbError>
where
    F: FnMut(&mut String) -> Result<(), DbError>,
{
    let bytes = template.as_bytes();
    let mut i = 0usize;
    let mut result = String::with_capacity(template.len());

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                let next = decode_single_quoted_string(template, i).0;
                result.push_str(&template[i..next]);
                i = next;
            }
            b'"' if ansi_quotes => {
                let next = skip_delimited_identifier(template, i, b'"');
                result.push_str(&template[i..next]);
                i = next;
            }
            b'"' => {
                let next = decode_double_quoted_string(template, i).0;
                result.push_str(&template[i..next]);
                i = next;
            }
            b'`' => {
                let next = skip_delimited_identifier(template, i, b'`');
                result.push_str(&template[i..next]);
                i = next;
            }
            b'?' => {
                replace(&mut result)?;
                i += 1;
            }
            _ if is_line_comment_start(bytes, i) => {
                let next = skip_line_comment(template, i);
                result.push_str(&template[i..next]);
                i = next;
            }
            _ if is_block_comment_start(bytes, i) => {
                let next = skip_block_comment(template, i);
                result.push_str(&template[i..next]);
                i = next;
            }
            _ => {
                let ch = template[i..].chars().next().unwrap();
                result.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    Ok(result)
}

pub(crate) fn split_sql_statements(sql: &str, ansi_quotes: bool) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut stmts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = decode_single_quoted_string(sql, i).0,
            b'"' if ansi_quotes => i = skip_delimited_identifier(sql, i, b'"'),
            b'"' => i = decode_double_quoted_string(sql, i).0,
            b'`' => i = skip_delimited_identifier(sql, i, b'`'),
            b';' => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    stmts.push(stmt);
                }
                start = i + 1;
                i += 1;
            }
            _ if is_line_comment_start(bytes, i) => i = skip_line_comment(sql, i),
            _ if is_block_comment_start(bytes, i) => i = skip_block_comment(sql, i),
            _ => i += next_char_len(sql, i),
        }
    }

    let tail = sql[start..].trim();
    if !tail.is_empty() {
        stmts.push(tail);
    }
    if stmts.is_empty() {
        stmts.push(sql.trim());
    }
    stmts
}

pub(crate) fn normalize_sql(sql: &str, ansi_quotes: bool) -> (String, Vec<Value>) {
    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut params = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        if b.is_ascii_whitespace() {
            let ch = sql[i..].chars().next().unwrap();
            result.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if b == b'\'' {
            let (next, s) = decode_single_quoted_string(sql, i);
            result.push('?');
            params.push(Value::Text(s));
            i = next;
            continue;
        }

        if b == b'"' {
            if ansi_quotes {
                let next = skip_delimited_identifier(sql, i, b'"');
                result.push_str(&sql[i..next]);
                i = next;
            } else {
                let (next, s) = decode_double_quoted_string(sql, i);
                result.push('?');
                params.push(Value::Text(s));
                i = next;
            }
            continue;
        }

        if b == b'`' {
            let next = skip_delimited_identifier(sql, i, b'`');
            result.push_str(&sql[i..next]);
            i = next;
            continue;
        }

        if is_line_comment_start(bytes, i) {
            let next = skip_line_comment(sql, i);
            result.push_str(&sql[i..next]);
            i = next;
            continue;
        }

        if is_block_comment_start(bytes, i) {
            let next = skip_block_comment(sql, i);
            result.push_str(&sql[i..next]);
            i = next;
            continue;
        }

        if b.is_ascii_digit() || (b == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            let num_start = i;
            let mut is_float = b == b'.';
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'0'..=b'9' => i += 1,
                    b'.' => {
                        is_float = true;
                        i += 1;
                    }
                    b'e' | b'E' => {
                        is_float = true;
                        i += 1;
                        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                            i += 1;
                        }
                    }
                    _ => break,
                }
            }

            if num_start > 0
                && (bytes[num_start - 1].is_ascii_alphanumeric() || bytes[num_start - 1] == b'_')
            {
                result.push_str(&sql[num_start..i]);
                continue;
            }

            let num_str = &sql[num_start..i];
            if is_float {
                if let Ok(f) = num_str.parse::<f64>() {
                    result.push('?');
                    params.push(Value::Real(f));
                } else {
                    result.push_str(num_str);
                }
            } else if let Ok(n) = num_str.parse::<i64>() {
                result.push('?');
                if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                    params.push(Value::Int(n as i32));
                } else {
                    params.push(Value::BigInt(n));
                }
            } else {
                result.push_str(num_str);
            }
            continue;
        }

        let ch = sql[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }

    (result, params)
}
