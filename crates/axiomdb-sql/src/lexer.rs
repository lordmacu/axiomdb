//! SQL lexer — converts a SQL string into a stream of [`SpannedToken`]s.
//!
//! ## Design
//!
//! Uses [`logos`] to generate a DFA lexer. Keywords are matched with
//! `ignore(ascii_case)` so that `SELECT`, `select`, and `Select` all
//! produce the same token. Whitespace and all three MySQL comment styles
//! (`--`, `#`, `/* */`) are skipped automatically.
//!
//! ## Separation of phases (4.2b)
//!
//! [`tokenize`] is the only public entry point. It:
//! 1. Enforces `max_bytes` before scanning (fail-fast for oversized queries).
//! 2. Never panics — all errors become [`DbError::ParseError`].
//! 3. Always appends a [`Token::Eof`] sentinel.
//!
//! String escape processing is handled by [`process_string_literal`], which
//! is called from within the logos callback for `StringLit`.

use axiomdb_core::error::DbError;
use logos::Logos;

// ── Span / SpannedToken ───────────────────────────────────────────────────────

/// Byte offsets of a token within the input string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A SQL token paired with its source position.
///
/// The lifetime `'src` is tied to the input string passed to [`tokenize`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken<'src> {
    pub token: Token<'src>,
    pub span: Span,
}

impl<'src> SpannedToken<'src> {
    fn new(token: Token<'src>, start: usize, end: usize) -> Self {
        Self {
            token,
            span: Span { start, end },
        }
    }
}

// ── Token ─────────────────────────────────────────────────────────────────────

/// A SQL token produced by the lexer.
///
/// ## Zero-copy identifiers
///
/// `Ident`, `QuotedIdent`, and `DqIdent` hold `&'src str` slices directly
/// into the input string — no heap allocation. Only `StringLit` allocates
/// a `String` because escape sequences transform the content in place.
///
/// Keywords are case-insensitive: `SELECT`, `select`, and `Select` all
/// produce [`Token::Select`].
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n]+")] // whitespace
#[logos(skip r"--[^\n]*")] // line comment (--)
#[logos(skip r"#[^\n]*")] // MySQL line comment (#)
#[logos(skip r"/\*([^*]|\*[^/])*\*/")] // block comment /* */
pub enum Token<'src> {
    // ── DML keywords ─────────────────────────────────────────────────────────
    #[token("SELECT", ignore(ascii_case))]
    Select,
    #[token("FROM", ignore(ascii_case))]
    From,
    #[token("WHERE", ignore(ascii_case))]
    Where,
    #[token("INSERT", ignore(ascii_case))]
    Insert,
    #[token("INTO", ignore(ascii_case))]
    Into,
    #[token("VALUES", ignore(ascii_case))]
    Values,
    #[token("UPDATE", ignore(ascii_case))]
    Update,
    #[token("SET", ignore(ascii_case))]
    Set,
    #[token("DELETE", ignore(ascii_case))]
    Delete,

    // ── DDL keywords ──────────────────────────────────────────────────────────
    #[token("CREATE", ignore(ascii_case))]
    Create,
    #[token("TABLE", ignore(ascii_case))]
    Table,
    #[token("INDEX", ignore(ascii_case))]
    Index,
    #[token("DROP", ignore(ascii_case))]
    Drop,
    #[token("ALTER", ignore(ascii_case))]
    Alter,
    #[token("ANALYZE", ignore(ascii_case))]
    Analyze,
    #[token("INCLUDE", ignore(ascii_case))]
    Include,
    #[token("ADD", ignore(ascii_case))]
    Add,
    #[token("COLUMN", ignore(ascii_case))]
    Column,
    #[token("MODIFY", ignore(ascii_case))]
    Modify,
    #[token("RENAME", ignore(ascii_case))]
    Rename,
    #[token("TO", ignore(ascii_case))]
    To,
    #[token("IF", ignore(ascii_case))]
    If,
    #[token("EXISTS", ignore(ascii_case))]
    Exists,
    #[token("TRUNCATE", ignore(ascii_case))]
    Truncate,

    // ── Constraints ───────────────────────────────────────────────────────────
    #[token("PRIMARY", ignore(ascii_case))]
    Primary,
    #[token("KEY", ignore(ascii_case))]
    Key,
    #[token("UNIQUE", ignore(ascii_case))]
    Unique,
    #[token("FOREIGN", ignore(ascii_case))]
    Foreign,
    #[token("REFERENCES", ignore(ascii_case))]
    References,
    #[token("CHECK", ignore(ascii_case))]
    Check,
    #[token("CONSTRAINT", ignore(ascii_case))]
    Constraint,
    #[token("DEFAULT", ignore(ascii_case))]
    Default,
    #[token("NOT", ignore(ascii_case))]
    Not,
    #[token("AUTO_INCREMENT", ignore(ascii_case))]
    AutoIncrement,
    #[token("SERIAL", ignore(ascii_case))]
    Serial,
    #[token("CASCADE", ignore(ascii_case))]
    Cascade,
    #[token("RESTRICT", ignore(ascii_case))]
    Restrict,
    #[token("ACTION", ignore(ascii_case))]
    Action,
    #[token("NO", ignore(ascii_case))]
    No,

    // ── JOIN ──────────────────────────────────────────────────────────────────
    #[token("JOIN", ignore(ascii_case))]
    Join,
    #[token("OUTER", ignore(ascii_case))]
    Outer,
    #[token("INNER", ignore(ascii_case))]
    Inner,
    #[token("LEFT", ignore(ascii_case))]
    Left,
    #[token("RIGHT", ignore(ascii_case))]
    Right,
    #[token("FULL", ignore(ascii_case))]
    Full,
    #[token("CROSS", ignore(ascii_case))]
    Cross,
    #[token("ON", ignore(ascii_case))]
    On,
    #[token("USING", ignore(ascii_case))]
    Using,

    // ── SELECT clauses ────────────────────────────────────────────────────────
    #[token("DISTINCT", ignore(ascii_case))]
    Distinct,
    #[token("AS", ignore(ascii_case))]
    As,
    #[token("ORDER", ignore(ascii_case))]
    Order,
    #[token("GROUP", ignore(ascii_case))]
    Group,
    #[token("BY", ignore(ascii_case))]
    By,
    #[token("HAVING", ignore(ascii_case))]
    Having,
    #[token("LIMIT", ignore(ascii_case))]
    Limit,
    #[token("OFFSET", ignore(ascii_case))]
    Offset,
    #[token("ASC", ignore(ascii_case))]
    Asc,
    #[token("DESC", ignore(ascii_case))]
    Desc,
    #[token("NULLS", ignore(ascii_case))]
    Nulls,
    #[token("FIRST", ignore(ascii_case))]
    First,
    #[token("LAST", ignore(ascii_case))]
    Last,

    // ── Boolean / predicates ──────────────────────────────────────────────────
    #[token("AND", ignore(ascii_case))]
    And,
    #[token("OR", ignore(ascii_case))]
    Or,
    #[token("IS", ignore(ascii_case))]
    Is,
    #[token("IN", ignore(ascii_case))]
    In,
    #[token("BETWEEN", ignore(ascii_case))]
    Between,
    #[token("LIKE", ignore(ascii_case))]
    Like,
    #[token("ESCAPE", ignore(ascii_case))]
    Escape,

    // ── Null / boolean literals ───────────────────────────────────────────────
    #[token("NULL", ignore(ascii_case))]
    Null,
    #[token("TRUE", ignore(ascii_case))]
    True,
    #[token("FALSE", ignore(ascii_case))]
    False,

    // ── Transaction ───────────────────────────────────────────────────────────
    #[token("BEGIN", ignore(ascii_case))]
    Begin,
    #[token("COMMIT", ignore(ascii_case))]
    Commit,
    #[token("ROLLBACK", ignore(ascii_case))]
    Rollback,
    #[token("START", ignore(ascii_case))]
    Start,
    #[token("TRANSACTION", ignore(ascii_case))]
    Transaction,

    // ── Utility ───────────────────────────────────────────────────────────────
    #[token("SHOW", ignore(ascii_case))]
    Show,
    #[token("TABLES", ignore(ascii_case))]
    Tables,
    #[token("DESCRIBE", ignore(ascii_case))]
    Describe,
    // DESC is already defined above (sort direction) — same token, different context.

    // ── CASE expression ───────────────────────────────────────────────────────
    #[token("CASE", ignore(ascii_case))]
    Case,
    #[token("WHEN", ignore(ascii_case))]
    When,
    #[token("THEN", ignore(ascii_case))]
    Then,
    #[token("ELSE", ignore(ascii_case))]
    Else,
    #[token("END", ignore(ascii_case))]
    End,

    // ── Set operations ────────────────────────────────────────────────────────
    #[token("UNION", ignore(ascii_case))]
    Union,
    #[token("INTERSECT", ignore(ascii_case))]
    Intersect,
    #[token("EXCEPT", ignore(ascii_case))]
    Except,
    #[token("ALL", ignore(ascii_case))]
    All,

    // ── Data type keywords (in column definitions) ────────────────────────────
    // Prefixed with `Ty` to avoid collision with literal variants Integer/Float.
    #[token("INT", ignore(ascii_case))]
    TyInt,
    #[token("INTEGER", ignore(ascii_case))]
    TyInteger,
    #[token("BIGINT", ignore(ascii_case))]
    TyBigint,
    #[token("REAL", ignore(ascii_case))]
    TyReal,
    #[token("DOUBLE", ignore(ascii_case))]
    TyDouble,
    #[token("FLOAT", ignore(ascii_case))]
    TyFloat,
    #[token("DECIMAL", ignore(ascii_case))]
    TyDecimal,
    #[token("NUMERIC", ignore(ascii_case))]
    TyNumeric,
    #[token("BOOL", ignore(ascii_case))]
    TyBool,
    #[token("BOOLEAN", ignore(ascii_case))]
    TyBoolean,
    #[token("TEXT", ignore(ascii_case))]
    TyText,
    #[token("VARCHAR", ignore(ascii_case))]
    TyVarchar,
    #[token("CHAR", ignore(ascii_case))]
    TyChar,
    #[token("BLOB", ignore(ascii_case))]
    TyBlob,
    #[token("BYTEA", ignore(ascii_case))]
    TyBytea,
    #[token("DATE", ignore(ascii_case))]
    TyDate,
    #[token("TIMESTAMP", ignore(ascii_case))]
    TyTimestamp,
    #[token("DATETIME", ignore(ascii_case))]
    TyDatetime,
    #[token("UUID", ignore(ascii_case))]
    TyUuid,

    // ── Miscellaneous ─────────────────────────────────────────────────────────
    #[token("SEPARATOR", ignore(ascii_case))]
    Separator,
    #[token("WITH", ignore(ascii_case))]
    With,
    #[token("AUTOCOMMIT", ignore(ascii_case))]
    Autocommit,
    #[token("NAMES", ignore(ascii_case))]
    Names,

    // ── Literals ──────────────────────────────────────────────────────────────
    /// Integer literal (unsigned; unary `-` is a separate `Minus` token).
    #[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
    Integer(i64),

    /// Float literal — must contain `.` or `e`/`E`.
    #[regex(
        r"[0-9]*\.[0-9]+([eE][+-]?[0-9]+)?|[0-9]+[eE][+-]?[0-9]+",
        |lex| lex.slice().parse::<f64>().ok()
    )]
    Float(f64),

    /// Single-quoted string literal with escape processing.
    /// `''` inside the string is the SQL-standard doubled-quote escape.
    #[regex(r"'([^'\\]|\\.|'')*'", |lex| process_string_literal(lex.slice()))]
    StringLit(String),

    // ── Identifiers ───────────────────────────────────────────────────────────
    /// Unquoted identifier: does not match any keyword.
    /// Zero-copy: holds a `&'src str` slice directly into the input.
    /// logos keyword tokens have higher priority than this regex.
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice())]
    Ident(&'src str),

    /// Backtick-quoted identifier (MySQL): `` `any content` ``.
    /// Zero-copy: returns a slice of the input with backticks stripped.
    #[regex(r"`[^`]*`", |lex| {
        let s = lex.slice();
        &s[1..s.len() - 1]
    })]
    QuotedIdent(&'src str),

    /// Double-quote-quoted identifier (SQL standard): `"any content"`.
    /// Zero-copy: returns a slice of the input with quotes stripped.
    #[regex(r#""[^"]*""#, |lex| {
        let s = lex.slice();
        &s[1..s.len() - 1]
    })]
    DqIdent(&'src str),

    // ── Operators ─────────────────────────────────────────────────────────────
    #[token("=")]
    Eq,
    /// Both `<>` and `!=` produce `NotEq`.
    #[token("<>")]
    #[token("!=")]
    NotEq,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    /// `*` — used both as multiply and as SELECT wildcard.
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    /// String concatenation operator `||`.
    #[token("||")]
    Concat,
    #[token(".")]
    Dot,

    // ── Punctuation ───────────────────────────────────────────────────────────
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(",")]
    Comma,
    #[token(";")]
    Semicolon,
    #[token(":")]
    Colon,
    /// `?` — positional parameter placeholder in a prepared statement template.
    #[token("?")]
    Question,

    // ── Sentinel ──────────────────────────────────────────────────────────────
    /// End-of-input sentinel added by [`tokenize`]. Never produced by logos.
    Eof,
}

// ── String escape processing ──────────────────────────────────────────────────

/// Processes escape sequences in a raw single-quoted SQL string literal.
///
/// `raw` must include the surrounding single quotes (e.g. `'hello\nworld'`).
/// Returns the unescaped string content.
///
/// Recognized escapes: `\\` `\'` `\"` `\n` `\r` `\t` `\0` `\b` `\Z`.
/// Unknown escapes `\x` → returns `x` literally (MySQL lenient behavior).
/// SQL standard `''` doubling → returns `'`.
pub(crate) fn process_string_literal(raw: &str) -> Option<String> {
    // Strip surrounding single quotes.
    let inner = &raw[1..raw.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                None => return None, // unterminated escape (shouldn't happen with valid regex)
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('0') => result.push('\0'),
                Some('b') => result.push('\x08'),
                Some('Z') => result.push('\x1A'),
                Some(other) => result.push(other), // \', \", \\, and unknown escapes
            },
            '\'' => {
                // SQL standard: '' = single quote.
                // The regex guarantees that '' inside the string is matched as
                // a unit, so if we see a lone ' here it must be the doubled form.
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    result.push('\'');
                }
                // Lone ' at end (shouldn't reach here with correct regex).
            }
            other => result.push(other),
        }
    }

    Some(result)
}

// ── tokenize ─────────────────────────────────────────────────────────────────

/// Tokenizes `input` into a flat stream of [`SpannedToken`]s.
///
/// Always appends a [`Token::Eof`] sentinel as the last element.
///
/// ## Input limits (4.2b)
///
/// If `max_bytes` is `Some(n)` and `input.len() > n`, returns
/// `Err(DbError::ParseError)` immediately without scanning.
/// Pass `None` to disable the check (useful in tests).
///
/// ## Error guarantee (4.2b)
///
/// This function never panics on any input, including:
/// - Unrecognized characters (`@`, `$`, `^`, …)
/// - Unterminated string literals
/// - Integer literals that overflow `i64`
pub fn tokenize<'src>(
    input: &'src str,
    max_bytes: Option<usize>,
) -> Result<Vec<SpannedToken<'src>>, DbError> {
    // 4.2b: reject oversized queries before scanning.
    if let Some(max) = max_bytes {
        if input.len() > max {
            return Err(DbError::ParseError {
                message: format!(
                    "query too long: {} bytes (maximum {} bytes)",
                    input.len(),
                    max
                ),
                position: None,
            });
        }
    }

    let mut tokens: Vec<SpannedToken<'src>> = Vec::new();
    let mut lex = Token::lexer(input);

    while let Some(result) = lex.next() {
        let logos_span = lex.span();
        let start = logos_span.start;
        let end = logos_span.end;

        match result {
            Ok(token) => tokens.push(SpannedToken::new(token, start, end)),
            // logos 0.13+: unrecognized input produces Err(()) (the default error type).
            Err(()) => {
                let ch = input[start..].chars().next().unwrap_or('\u{FFFD}');
                return Err(DbError::ParseError {
                    message: format!("unexpected character '{}' at position {}", ch, start),
                    position: None,
                });
            }
        }
    }

    // EOF sentinel — positioned at end of input.
    let eof_pos = input.len();
    tokens.push(SpannedToken::new(Token::Eof, eof_pos, eof_pos));

    Ok(tokens)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(input: &str) -> Vec<Token<'_>> {
        tokenize(input, None)
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    fn tok_err(input: &str) -> DbError {
        tokenize(input, None).unwrap_err()
    }

    // ── Keyword case-insensitivity ────────────────────────────────────────────

    #[test]
    fn test_keyword_uppercase() {
        assert_eq!(tok("SELECT"), vec![Token::Select, Token::Eof]);
    }

    #[test]
    fn test_keyword_lowercase() {
        assert_eq!(tok("select"), vec![Token::Select, Token::Eof]);
    }

    #[test]
    fn test_keyword_mixed_case() {
        assert_eq!(tok("Select"), vec![Token::Select, Token::Eof]);
        assert_eq!(tok("sElEcT"), vec![Token::Select, Token::Eof]);
    }

    // ── Identifiers ───────────────────────────────────────────────────────────

    #[test]
    fn test_identifier_not_keyword() {
        assert!(matches!(
            &tok("my_table")[0],
            Token::Ident(s) if *s == "my_table"
        ));
    }

    #[test]
    fn test_identifier_starts_with_underscore() {
        assert!(matches!(
            &tok("_col")[0],
            Token::Ident(s) if *s == "_col"
        ));
    }

    // ── Literals ─────────────────────────────────────────────────────────────

    #[test]
    fn test_integer_literal() {
        assert_eq!(tok("42")[0], Token::Integer(42));
    }

    #[test]
    fn test_float_dot() {
        assert!(matches!(
            tok("3.14")[0],
            Token::Float(f) if (f - (314.0 / 100.0)).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn test_float_leading_dot() {
        assert_eq!(tok(".5")[0], Token::Float(0.5));
    }

    #[test]
    fn test_float_exponent() {
        assert_eq!(tok("1e10")[0], Token::Float(1e10));
    }

    // ── String literals ───────────────────────────────────────────────────────

    #[test]
    fn test_string_simple() {
        assert_eq!(tok("'hello'")[0], Token::StringLit("hello".into()));
    }

    #[test]
    fn test_string_empty() {
        assert_eq!(tok("''")[0], Token::StringLit(String::new()));
    }

    #[test]
    fn test_string_escape_newline() {
        assert_eq!(tok(r"'\n'")[0], Token::StringLit("\n".into()));
    }

    #[test]
    fn test_string_escape_tab() {
        assert_eq!(tok(r"'\t'")[0], Token::StringLit("\t".into()));
    }

    #[test]
    fn test_string_escape_quote() {
        assert_eq!(tok(r"'\''")[0], Token::StringLit("'".into()));
    }

    #[test]
    fn test_string_sql_doubling() {
        assert_eq!(tok("'it''s'")[0], Token::StringLit("it's".into()));
    }

    // ── Operators ────────────────────────────────────────────────────────────

    #[test]
    fn test_noteq_diamond() {
        assert_eq!(tok("<>")[0], Token::NotEq);
    }

    #[test]
    fn test_noteq_bang() {
        assert_eq!(tok("!=")[0], Token::NotEq);
    }

    #[test]
    fn test_concat_operator() {
        assert_eq!(tok("||")[0], Token::Concat);
    }

    // ── Comments ─────────────────────────────────────────────────────────────

    #[test]
    fn test_line_comment_stripped() {
        assert_eq!(tok("-- comment\nSELECT"), vec![Token::Select, Token::Eof]);
    }

    #[test]
    fn test_hash_comment_stripped() {
        assert_eq!(tok("#comment\nSELECT"), vec![Token::Select, Token::Eof]);
    }

    #[test]
    fn test_block_comment_stripped() {
        assert_eq!(tok("/* block */ SELECT"), vec![Token::Select, Token::Eof]);
    }

    // ── Errors ───────────────────────────────────────────────────────────────

    #[test]
    fn test_error_unexpected_char() {
        let e = tok_err("@");
        assert!(matches!(e, DbError::ParseError { .. }));
    }

    #[test]
    fn test_error_query_too_long() {
        let input = "a".repeat(100);
        let e = tokenize(&input, Some(10)).unwrap_err();
        assert!(matches!(e, DbError::ParseError { .. }));
    }

    // ── Sentinel ─────────────────────────────────────────────────────────────

    #[test]
    fn test_empty_input_eof_only() {
        assert_eq!(tok(""), vec![Token::Eof]);
    }

    #[test]
    fn test_whitespace_only_eof_only() {
        assert_eq!(tok("   \t\n  "), vec![Token::Eof]);
    }

    #[test]
    fn test_eof_always_last() {
        let tokens = tok("SELECT 1");
        assert_eq!(*tokens.last().unwrap(), Token::Eof);
    }
}
