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
//! is called from within the logos callback for `StringLit`. Double-quoted
//! fragments are resolved after lexing based on `SqlModeFlags.ansi_quotes`.

use axiomdb_core::error::DbError;
use logos::Logos;

use crate::session::SqlModeFlags;

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
/// `Ident` and `QuotedIdent` hold `&'src str` slices directly into the input
/// string. `StringLit` allocates a `String` because escape sequences transform
/// the content in place. `DqIdent` also allocates because `""` escaping must
/// be decoded when `ANSI_QUOTES` is enabled.
///
/// Keywords are case-insensitive: `SELECT`, `select`, and `Select` all
/// produce [`Token::Select`].
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n]+")] // whitespace
#[logos(skip r"--[^\n]*")] // line comment (--) MySQL also supports # but it collides with #> #>> #- operators
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
    #[token("MERGE", ignore(ascii_case))]
    Merge,
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
    #[token("DATABASE", ignore(ascii_case))]
    Database,
    #[token("DATABASES", ignore(ascii_case))]
    Databases,
    #[token("SCHEMA", ignore(ascii_case))]
    Schema,
    #[token("ALTER", ignore(ascii_case))]
    Alter,
    #[token("ANALYZE", ignore(ascii_case))]
    Analyze,
    #[token("EXPLAIN", ignore(ascii_case))]
    Explain,
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
    #[token("VACUUM", ignore(ascii_case))]
    Vacuum,
    #[token("SAVEPOINT", ignore(ascii_case))]
    SavepointKw,
    #[token("RELEASE", ignore(ascii_case))]
    Release,

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
    #[token("NATURAL", ignore(ascii_case))]
    Natural,
    #[token("APPLY", ignore(ascii_case))]
    Apply,
    #[token("LATERAL", ignore(ascii_case))]
    Lateral,
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
    #[token("USE", ignore(ascii_case))]
    Use,
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
    #[token("FETCH", ignore(ascii_case))]
    Fetch,
    #[token("RETURNING", ignore(ascii_case))]
    Returning,
    #[token("NEXT", ignore(ascii_case))]
    Next,
    #[token("ROW", ignore(ascii_case))]
    Row,
    #[token("ROWS", ignore(ascii_case))]
    Rows,

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
    #[token("JSON", ignore(ascii_case))]
    TyJson,
    #[token("JSONB", ignore(ascii_case))]
    TyJsonb,

    // ── Miscellaneous ─────────────────────────────────────────────────────────
    #[token("SEPARATOR", ignore(ascii_case))]
    Separator,
    #[token("WITH", ignore(ascii_case))]
    With,
    #[token("AUTOCOMMIT", ignore(ascii_case))]
    Autocommit,
    #[token("NAMES", ignore(ascii_case))]
    Names,

    // ── MySQL compatibility keywords ──────────────────────────────────────────
    /// `WORK` — used in `BEGIN WORK`.
    #[token("WORK", ignore(ascii_case))]
    Work,
    /// `READ` — used in `START TRANSACTION READ ONLY/WRITE`.
    #[token("READ", ignore(ascii_case))]
    Read,
    /// `ONLY` — used in `READ ONLY`.
    #[token("ONLY", ignore(ascii_case))]
    Only,
    /// `WRITE` — used in `READ WRITE` and `LOCK TABLES ... WRITE`.
    #[token("WRITE", ignore(ascii_case))]
    Write,
    /// `GLOBAL` — used in `SET GLOBAL var = val`.
    #[token("GLOBAL", ignore(ascii_case))]
    Global,
    /// `SESSION` — used in `SET SESSION var = val`.
    #[token("SESSION", ignore(ascii_case))]
    Session,
    /// `LOCAL` — synonym for SESSION in `SET LOCAL var = val`.
    #[token("LOCAL", ignore(ascii_case))]
    Local,
    /// `LOCK` — statement keyword for `LOCK TABLES`.
    #[token("LOCK", ignore(ascii_case))]
    Lock,
    /// `UNLOCK` — statement keyword for `UNLOCK TABLES`.
    #[token("UNLOCK", ignore(ascii_case))]
    Unlock,
    /// `FLUSH` — statement keyword for `FLUSH TABLES`.
    #[token("FLUSH", ignore(ascii_case))]
    Flush,
    /// `KILL` — statement keyword for `KILL [QUERY|CONNECTION] id`.
    #[token("KILL", ignore(ascii_case))]
    Kill,
    /// `QUERY` — used in `KILL QUERY id`.
    #[token("QUERY", ignore(ascii_case))]
    Query,
    /// `CONNECTION` — used in `KILL CONNECTION id`.
    #[token("CONNECTION", ignore(ascii_case))]
    Connection,
    /// `CALL` — used in `CALL proc(args)` (MySQL stored procedure call → Noop).
    #[token("CALL", ignore(ascii_case))]
    Call,
    /// `DO` — used in `DO expr` (MySQL expression discard → Noop).
    #[token("DO", ignore(ascii_case))]
    Do,
    /// `IGNORE` — used in `INSERT IGNORE INTO ...`.
    #[token("IGNORE", ignore(ascii_case))]
    Ignore,
    /// `FOR` — used in `SELECT ... FOR UPDATE`.
    #[token("FOR", ignore(ascii_case))]
    For,
    /// `SHARE` — used in `SELECT ... LOCK IN SHARE MODE`.
    #[token("SHARE", ignore(ascii_case))]
    Share,
    /// `MODE` — used in `LOCK IN SHARE MODE`.
    #[token("MODE", ignore(ascii_case))]
    Mode,

    // ── Literals ──────────────────────────────────────────────────────────────
    /// Hexadecimal integer literal: `0x1A2B` or `0X1a2b`.
    /// Converted to `i64`; values exceeding `i64::MAX` clamp to `i64::MAX`.
    #[regex(r"0[xX][0-9a-fA-F]+", |lex| {
        i64::from_str_radix(&lex.slice()[2..], 16)
            .or_else(|_| u64::from_str_radix(&lex.slice()[2..], 16).map(|n| n as i64))
            .ok()
    })]
    HexLit(i64),

    /// Integer literal (unsigned; unary `-` is a separate `Minus` token).
    ///
    /// Multiple patterns (4.2d, 4.2e):
    /// - `[0-9]+` — decimal; values > `i64::MAX` clamp to `i64::MAX`
    /// - `0b` / `0B` — binary integer (`0b1010`)
    /// - `b'...'` / `B'...'` — binary bit-string (`b'1010'`)
    #[regex(r"[0-9]+", |lex| {
        let s = lex.slice();
        s.parse::<i64>()
            .or_else(|_| s.parse::<u64>().map(|n| n.min(i64::MAX as u64) as i64))
            .ok()
    })]
    #[regex(r"0[bB][01]+", |lex| {
        i64::from_str_radix(&lex.slice()[2..], 2)
            .or_else(|_| u64::from_str_radix(&lex.slice()[2..], 2).map(|n| n as i64))
            .ok()
    })]
    #[regex(r"[bB]'[01]*'", |lex| {
        let s = lex.slice();
        let inner = &s[2..s.len() - 1];
        if inner.is_empty() { return Some(0i64); }
        i64::from_str_radix(inner, 2)
            .or_else(|_| u64::from_str_radix(inner, 2).map(|n| n as i64))
            .ok()
    })]
    Integer(i64),

    /// Float literal — must contain `.` or `e`/`E`.
    #[regex(
        r"[0-9]*\.[0-9]+([eE][+-]?[0-9]+)?|[0-9]+[eE][+-]?[0-9]+",
        |lex| lex.slice().parse::<f64>().ok()
    )]
    Float(f64),

    /// Single-quoted string literal with escape processing.
    /// `''` inside the string is the SQL-standard doubled-quote escape.
    ///
    /// Also handles hex byte-string `x'AABB'` / `X'AABB'` (4.2e):
    /// decoded as UTF-8 when valid, latin-1 otherwise.
    #[regex(r"'([^'\\]|\\.|'')*'", |lex| process_string_literal(lex.slice()))]
    #[regex(r"[xX]'[0-9a-fA-F]*'", |lex| decode_hex_string_literal(lex.slice()))]
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

    /// Raw double-quoted fragment. Resolved in [`tokenize_with_sql_mode`] into
    /// either `StringLit` (MySQL default) or `DqIdent` (`ANSI_QUOTES`).
    #[regex(r#""([^"\\]|\\.|"")*""#, |lex| lex.slice())]
    RawDoubleQuoted(&'src str),

    /// Double-quote-quoted identifier after `ANSI_QUOTES` decoding.
    DqIdent(String),

    // ── MySQL expression keywords ─────────────────────────────────────────────
    /// `REGEXP` — regular-expression match operator.
    #[token("REGEXP", ignore(ascii_case))]
    Regexp,
    /// `RLIKE` — alias for REGEXP.
    #[token("RLIKE", ignore(ascii_case))]
    Rlike,
    /// `XOR` — boolean exclusive-or.
    #[token("XOR", ignore(ascii_case))]
    Xor,
    /// `DIV` — integer division (`a DIV b` truncates toward zero).
    #[token("DIV", ignore(ascii_case))]
    IntDiv,

    // ── Operators ─────────────────────────────────────────────────────────────
    #[token("=")]
    Eq,
    /// Both `<>` and `!=` produce `NotEq`.
    #[token("<>")]
    #[token("!=")]
    NotEq,
    /// `<=>` — null-safe equality (never returns NULL).
    #[token("<=>")]
    NullSafe,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    /// `<<` — bitwise shift left.
    #[token("<<")]
    ShiftLeft,
    /// `>>` — bitwise shift right.
    #[token(">>")]
    ShiftRight,
    /// `->>` — JSON field extraction returning text/scalar (Phase 11.4).
    /// MUST appear before `->` in the logos attribute list so the longer token wins.
    #[token("->>")]
    JsonExtractText,
    /// `->` — JSON sub-document extraction returning JSONB (Phase 11.16).
    /// MUST appear after `->>` but before `-` and `>` individually.
    #[token("->")]
    JsonExtractSub,
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
    /// String concatenation operator `||` (also doubles as bitwise-OR alternative
    /// when `sql_mode` includes PIPES_AS_CONCAT=OFF, but we keep as concat).
    #[token("||")]
    Concat,
    /// `&` — bitwise AND.
    #[token("&")]
    Amp,
    /// `|` — bitwise OR. Must appear after `||` so logos picks the longer match.
    #[token("|")]
    Pipe,
    /// `^` — bitwise XOR.
    #[token("^")]
    Caret,
    /// `~` — bitwise NOT (unary).
    #[token("~")]
    Tilde,
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
    /// `@@` — MySQL session/system variable prefix.
    #[token("@@")]
    AtAt,
    /// `@>` — JSONB containment operator (Phase 11.17). MUST appear before `@`.
    #[token("@>")]
    JsonContains,
    /// `@?` — JSONB JSONPath existence operator (Phase 11.21b). MUST appear before `@`.
    #[token("@?")]
    JsonbPathExists,
    /// `<@` — JSONB contained-by operator (Phase 11.18a, PG parity).
    /// Reverse of `@>`. Token appears near `@>` so the DFA prefers this
    /// longer match when the input begins with `<`.
    #[token("<@")]
    JsonContainedBy,
    /// `@` — MySQL user-variable prefix.
    #[token("@")]
    At,
    /// `?|` — JSONB any-key existence (Phase 11.18b). MUST appear before `?`.
    #[token("?|")]
    JsonExistsAny,
    /// `?&` — JSONB all-keys existence (Phase 11.18b). MUST appear before `?`.
    #[token("?&")]
    JsonExistsAll,
    /// `#>>` — JSONB path-extract as TEXT (Phase 11.18c). MUST appear before `#>`.
    #[token("#>>")]
    JsonPathExtractText,
    /// `#>` — JSONB path-extract as JSONB (Phase 11.18c).
    #[token("#>")]
    JsonPathExtract,
    /// `#-` — JSONB path-delete (Phase 11.18c).
    #[token("#-")]
    JsonPathDelete,
    /// `?` — positional parameter placeholder in a prepared statement template.
    #[token("?")]
    Question,

    // ── Sentinel ──────────────────────────────────────────────────────────────
    /// End-of-input sentinel added by [`tokenize`]. Never produced by logos.
    Eof,
}

// ── String escape processing ──────────────────────────────────────────────────

/// Decodes a MySQL hex byte-string literal `x'AABB'` / `X'AABB'` (4.2e).
///
/// Returns the decoded bytes as a `String`:
/// - If the bytes are valid UTF-8, returns `from_utf8` directly.
/// - Otherwise, maps each byte to its latin-1 char equivalent (MySQL behavior).
pub(crate) fn decode_hex_string_literal(raw: &str) -> Option<String> {
    // Strip the leading `x'` / `X'` and trailing `'`.
    let inner = &raw[2..raw.len() - 1];
    if inner.is_empty() {
        return Some(String::new());
    }
    if !inner.len().is_multiple_of(2) {
        return None; // Odd hex digits are invalid.
    }
    let bytes: Option<Vec<u8>> = inner
        .as_bytes()
        .chunks(2)
        .map(|chunk| {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect();
    let bytes = bytes?;
    Some(
        String::from_utf8(bytes.clone())
            .unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect()),
    )
}

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

/// Processes escape sequences in a raw double-quoted SQL string literal.
///
/// Mirrors [`process_string_literal`] but uses `"` as the delimiter and `""`
/// as the SQL-standard doubled-quote escape.
fn process_double_quoted_string_literal(raw: &str) -> Option<String> {
    let inner = &raw[1..raw.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                None => return None,
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('0') => result.push('\0'),
                Some('b') => result.push('\x08'),
                Some('Z') => result.push('\x1A'),
                Some(other) => result.push(other),
            },
            '"' => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    result.push('"');
                }
            }
            other => result.push(other),
        }
    }

    Some(result)
}

/// Processes a raw double-quoted identifier in `ANSI_QUOTES` mode.
///
/// MySQL/MariaDB delimited identifiers use doubled quotes (`""`) to embed a
/// literal quote inside the identifier name.
fn process_double_quoted_identifier(raw: &str) -> Option<String> {
    let inner = &raw[1..raw.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                result.push('"');
            } else {
                return None;
            }
        } else {
            result.push(c);
        }
    }

    Some(result)
}

// ── MySQL `#` comment stripper ───────────────────────────────────────────────

/// Strips MySQL `#` line comments from `input` before tokenization.
///
/// This is done here rather than via a logos `skip` rule because `#` conflicts
/// with the JSONB operators `#>`, `#>>`, `#-` (Phase 11.18c).
fn strip_hash_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Only treat `#` as a comment at the start of a line (after \n or \r or start)
        if bytes[i] == b'#'
            && (i == 0
                || bytes[i.saturating_sub(1)] == b'\n'
                || bytes[i.saturating_sub(1)] == b'\r')
        {
            // Skip until end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else {
            // Decode a full UTF-8 code point starting at bytes[i] so that
            // multi-byte sequences (e.g. `é` = 0xC3 0xA9) are not split into
            // two surrogate chars.
            let rest = &input[i..];
            let c = rest.chars().next().unwrap_or('\0');
            result.push(c);
            i += c.len_utf8();
        }
    }
    result
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
    tokenize_with_sql_mode(input, max_bytes, SqlModeFlags::default())
}

/// Tokenizes `input` with parser-affecting SQL mode flags.
///
/// `ANSI_QUOTES = false` (MySQL default) maps `"..."` to `StringLit`.
/// `ANSI_QUOTES = true` maps `"..."` to `DqIdent`.
pub fn tokenize_with_sql_mode<'src>(
    input: &'src str,
    max_bytes: Option<usize>,
    sql_mode: SqlModeFlags,
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

    // Phase 11.18c fix: explicitly strip MySQL `#` comments before tokenization.
    // The `#` character conflicts with the #> #>> #- JSONB operators,
    // so we handle it here in the tokenize function rather than in the logos skip rule.
    let stripped_input = strip_hash_comments(input);
    let stripped_input: &'src str = Box::leak(stripped_input.into_boxed_str());

    let mut lex = Token::lexer(stripped_input);

    while let Some(result) = lex.next() {
        let logos_span = lex.span();
        let start = logos_span.start;
        let end = logos_span.end;

        match result {
            Ok(Token::RawDoubleQuoted(raw)) => {
                let token = if sql_mode.ansi_quotes {
                    Token::DqIdent(process_double_quoted_identifier(raw).ok_or_else(|| {
                        DbError::ParseError {
                            message: "invalid double-quoted identifier".into(),
                            position: Some(start),
                        }
                    })?)
                } else {
                    Token::StringLit(process_double_quoted_string_literal(raw).ok_or_else(
                        || DbError::ParseError {
                            message: "invalid double-quoted string literal".into(),
                            position: Some(start),
                        },
                    )?)
                };
                tokens.push(SpannedToken::new(token, start, end));
            }
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

// ── MySQL version-conditional comment preprocessing (4.1f) ────────────────────

/// Expands MySQL version-conditional comments in `input` (4.1f).
///
/// `/*!NNNNN SQL*/` — if NNNNN ≤ 80000 (MySQL 8.0.0), replaces with ` SQL `.
/// `/*!SQL*/` (no version number) — always includes the SQL.
/// Regular `/* */` block comments are left unchanged (handled by the logos `skip` rule).
///
/// Returns a `String` only when `/*!` is present; otherwise returns the input unchanged.
/// The caller stores the returned `String` so that tokens can borrow from it.
pub fn expand_version_comments(input: &str) -> Option<String> {
    if !input.contains("/*!") {
        return None; // Fast path: no version comments, no allocation needed.
    }

    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `/*!` (version comment start).
        if i + 2 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' && bytes[i + 2] == b'!' {
            // Find the matching `*/`.
            let search_start = i + 3;
            let close = find_block_comment_end(bytes, search_start);
            match close {
                None => {
                    // Unterminated comment — copy rest as-is and stop.
                    result.push_str(&input[i..]);
                    i = bytes.len();
                }
                Some(end) => {
                    let inner = &input[search_start..end];
                    // Parse optional leading version number (1–5 digits).
                    let ver_end = inner
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(inner.len());
                    let content = if ver_end > 0 {
                        let ver: u32 = inner[..ver_end].parse().unwrap_or(u32::MAX);
                        if ver <= 80000 {
                            &inner[ver_end..]
                        } else {
                            "" // Version too new — suppress.
                        }
                    } else {
                        inner // No version number — always include.
                    };
                    result.push(' ');
                    result.push_str(content);
                    result.push(' ');
                    i = end + 2; // Skip past `*/`.
                }
            }
        } else {
            // SAFETY: `bytes[i]` is valid ASCII or the start of a multi-byte char.
            // For non-ASCII, copy the full char.
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            result.push(ch);
            i += ch.len_utf8();
        }
    }

    Some(result)
}

/// Returns the byte offset of `*` in the `*/` that closes a block comment,
/// starting the search at `from` (the position after `/*`).
fn find_block_comment_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return Some(i);
        }
        i += 1;
    }
    None
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

    #[test]
    fn test_double_quoted_string_default_mode() {
        assert_eq!(tok(r#""hello""#)[0], Token::StringLit("hello".into()));
    }

    #[test]
    fn test_double_quoted_identifier_ansi_quotes_mode() {
        let toks = tokenize_with_sql_mode(r#""my""col""#, None, SqlModeFlags { ansi_quotes: true })
            .unwrap();
        assert_eq!(toks[0].token, Token::DqIdent(r#"my"col"#.into()));
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

    #[test]
    fn test_json_extract_text_operator() {
        assert_eq!(tok("->>")[0], Token::JsonExtractText);
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
        let e = tok_err("$");
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

    // ── Integer overflow (4.2d) ───────────────────────────────────────────────

    #[test]
    fn test_integer_normal() {
        assert_eq!(tok("42")[0], Token::Integer(42));
    }

    #[test]
    fn test_integer_overflow_clamped() {
        // 9999999999999999999 > i64::MAX (9223372036854775807) — should clamp
        assert_eq!(tok("9999999999999999999")[0], Token::Integer(i64::MAX));
    }

    #[test]
    fn test_integer_i64_max() {
        assert_eq!(
            tok("9223372036854775807")[0],
            Token::Integer(9223372036854775807)
        );
    }

    // ── Binary literals (4.2e) ────────────────────────────────────────────────

    #[test]
    fn test_binary_prefix_0b() {
        assert_eq!(tok("0b1010")[0], Token::Integer(10));
    }

    #[test]
    fn test_binary_prefix_0b_uppercase() {
        assert_eq!(tok("0B1111")[0], Token::Integer(15));
    }

    #[test]
    fn test_binary_bit_string() {
        assert_eq!(tok("b'1010'")[0], Token::Integer(10));
    }

    #[test]
    fn test_binary_bit_string_uppercase() {
        assert_eq!(tok("B'1111'")[0], Token::Integer(15));
    }

    #[test]
    fn test_binary_bit_string_empty() {
        assert_eq!(tok("b''")[0], Token::Integer(0));
    }

    // ── Hex string literals (4.2e) ────────────────────────────────────────────

    #[test]
    fn test_hex_string_literal_ascii() {
        // x'48656c6c6f' = b"Hello"
        assert_eq!(tok("x'48656c6c6f'")[0], Token::StringLit("Hello".into()));
    }

    #[test]
    fn test_hex_string_literal_uppercase_x() {
        assert_eq!(tok("X'48656c6c6f'")[0], Token::StringLit("Hello".into()));
    }

    #[test]
    fn test_hex_string_literal_empty() {
        assert_eq!(tok("x''")[0], Token::StringLit(String::new()));
    }

    // ── Version-conditional comments (4.1f) ───────────────────────────────────

    #[test]
    fn test_version_comment_included() {
        // /*!40101 SET NAMES ... */ — version 40101 ≤ 80000, SQL is included
        let result = expand_version_comments("/*!40101 SELECT 1*/");
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.contains("SELECT 1"), "SQL should be included: {s}");
    }

    #[test]
    fn test_version_comment_excluded() {
        // /*!99999 SELECT 1*/ — version 99999 > 80000, SQL suppressed
        let result = expand_version_comments("/*!99999 SELECT 1*/");
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(!s.contains("SELECT"), "SQL should be suppressed: {s}");
    }

    #[test]
    fn test_version_comment_no_version() {
        // /*! SQL */ — no version number, always included
        let result = expand_version_comments("/*! SELECT 1 */");
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.contains("SELECT 1"), "SQL should be included: {s}");
    }

    #[test]
    fn test_version_comment_no_version_comments_fast_path() {
        // No /*! in input — returns None (no allocation)
        let result = expand_version_comments("SELECT 1");
        assert!(result.is_none(), "Should be None for no version comments");
    }

    #[test]
    fn test_version_comment_mixed() {
        // Regular block comment left as-is (handled by logos skip)
        let result = expand_version_comments("/* regular */ /*!50001 SELECT 1*/");
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.contains("SELECT 1"));
        // The regular comment is preserved (logos will skip it)
        assert!(s.contains("/* regular */"));
    }
}
