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

/// True if `kw` (ASCII) appears at `bytes[i]` case-insensitively with identifier
/// word boundaries on both sides (so `END` matches but `ENDING` / `xEND` do not).
fn is_keyword_at(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    if i + kw.len() > bytes.len() {
        return false;
    }
    if !bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    let is_ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let before_ok = i == 0 || !is_ident(bytes[i - 1]);
    let after_ok = i + kw.len() >= bytes.len() || !is_ident(bytes[i + kw.len()]);
    before_ok && after_ok
}

/// If a PostgreSQL dollar-quote opener (`$$` or `$tag$`) begins at `bytes[i]`,
/// returns the byte index just past the matching close; otherwise `None`.
fn dollar_quote_skip(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    debug_assert_eq!(bytes[i], b'$');
    let mut j = i + 1;
    while j < bytes.len() && bytes[j] != b'$' {
        let b = bytes[j];
        if !(b == b'_' || b.is_ascii_alphanumeric()) {
            return None;
        }
        j += 1;
    }
    if j >= bytes.len() {
        return None; // no closing `$` of the opener
    }
    let close = &sql[i..j + 1]; // `$tag$` (or `$$`)
    let body_start = j + 1;
    sql[body_start..]
        .find(close)
        .map(|rel| body_start + rel + close.len())
}

/// True if `s` (a statement, possibly leading-trimmed) begins a routine
/// definition (`CREATE [OR REPLACE] PROCEDURE|FUNCTION …`), whose `BEGIN … END`
/// body or `$$ … $$` block must not be split on inner `;`.
fn stmt_starts_routine(s: &str) -> bool {
    let t = s.trim_start().to_ascii_lowercase();
    let rest = t.strip_prefix("create ").map(str::trim_start);
    let rest = rest.map(|r| {
        r.strip_prefix("or replace ")
            .map(str::trim_start)
            .unwrap_or(r)
    });
    matches!(rest, Some(r) if r.starts_with("procedure") || r.starts_with("function"))
}

pub(crate) fn split_sql_statements(sql: &str, ansi_quotes: bool) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut stmts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    // Routine-body awareness: inside a `CREATE PROCEDURE/FUNCTION` definition,
    // `;` within the `BEGIN … END` block (or a `$$ … $$` body) is part of the
    // body, not a statement separator. `CASE` also closes with `END`, so it is
    // counted as a block opener too.
    let mut in_routine = stmt_starts_routine(&sql[start..]);
    let mut body_depth: usize = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = decode_single_quoted_string(sql, i).0,
            b'"' if ansi_quotes => i = skip_delimited_identifier(sql, i, b'"'),
            b'"' => i = decode_double_quoted_string(sql, i).0,
            b'`' => i = skip_delimited_identifier(sql, i, b'`'),
            b'$' if dollar_quote_skip(sql, i).is_some() => {
                i = dollar_quote_skip(sql, i).unwrap();
            }
            b';' if body_depth == 0 => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    stmts.push(stmt);
                }
                start = i + 1;
                i += 1;
                in_routine = stmt_starts_routine(&sql[start..]);
            }
            _ if is_line_comment_start(bytes, i) => i = skip_line_comment(sql, i),
            _ if is_block_comment_start(bytes, i) => i = skip_block_comment(sql, i),
            _ if in_routine
                && (is_keyword_at(bytes, i, b"BEGIN") || is_keyword_at(bytes, i, b"CASE")) =>
            {
                body_depth += 1;
                i += if bytes[i] | 0x20 == b'b' { 5 } else { 4 };
            }
            _ if in_routine && body_depth > 0 && is_keyword_at(bytes, i, b"END") => {
                body_depth -= 1;
                i += 3;
            }
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

#[cfg(test)]
mod proc_split_tests {
    use super::split_sql_statements;

    #[test]
    fn does_not_split_inside_mysql_begin_end_body() {
        let sql =
            "CREATE PROCEDURE p(IN x INT) BEGIN INSERT INTO t VALUES (x); UPDATE t SET v = 1; END";
        let stmts = split_sql_statements(sql, false);
        assert_eq!(
            stmts.len(),
            1,
            "procedure body must stay one statement: {stmts:?}"
        );
        assert!(stmts[0].trim_end().ends_with("END"));
    }

    #[test]
    fn does_not_split_inside_nested_begin_or_case() {
        let sql = "CREATE PROCEDURE p() BEGIN UPDATE t SET x = CASE WHEN x>0 THEN 1 ELSE 0 END; INSERT INTO t VALUES (9); END";
        assert_eq!(split_sql_statements(sql, false).len(), 1);
    }

    #[test]
    fn does_not_split_inside_dollar_quoted_body() {
        let sql = "CREATE PROCEDURE p() LANGUAGE plpgsql AS $$ BEGIN INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END $$";
        assert_eq!(split_sql_statements(sql, false).len(), 1);
    }

    #[test]
    fn still_splits_ordinary_multi_statements() {
        let sql = "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); SELECT * FROM t";
        assert_eq!(split_sql_statements(sql, false).len(), 3);
    }

    #[test]
    fn splits_call_then_select_after_a_procedure() {
        let sql =
            "CREATE PROCEDURE p() BEGIN INSERT INTO t VALUES (1); END; CALL p(); SELECT * FROM t";
        let stmts = split_sql_statements(sql, false);
        assert_eq!(stmts.len(), 3, "{stmts:?}");
        assert!(stmts[1].eq_ignore_ascii_case("CALL p()"));
    }
}
