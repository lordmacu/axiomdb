# Spec: 4.2 + 4.2b — Lexer / Tokenizer + Input Sanitization

## What to build (not how)

A SQL lexer that converts a raw SQL string into a flat stream of typed tokens
with byte-offset spans. Used by the parser (Phase 4.3–4.4) as its input.

Phase 4.2b is implemented together: the lexer is the first and only line of
defense against malformed or oversized input; it must never panic and must
produce clear SQL errors for any invalid input.

---

## Library decision: `logos` for lexing, `nom` for parsing

`logos` generates a compiled DFA from token annotations. `nom` is used in
Phase 4.3–4.4 for parsing the token stream. This separation is standard
in Rust SQL parsers (sqlparser-rs, etc.) and produces faster, cleaner code.

---

## Span and SpannedToken

```rust
/// Byte offsets of a token within the input string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A token paired with its position in the input.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}
```

---

## Token enum

All variants derive `Logos`, `Debug`, `Clone`, `PartialEq`.

### Whitespace and comments (skipped — produce no tokens)

```
Whitespace:        [ \t\r\n]+
Line comment (--): --[^\n]*(\n|$)
Line comment (#):  #[^\n]*(\n|$)         ← MySQL extension
Block comment:     /\*([^*]|\*[^/])*\*/
```

### Keywords (case-insensitive via `ignore(ascii_case)`)

DML:
```
SELECT  FROM    WHERE   INSERT  INTO    VALUES
UPDATE  SET     DELETE
```

DDL:
```
CREATE  TABLE   INDEX   DROP    ALTER   ADD
COLUMN  MODIFY  RENAME  TO      IF      EXISTS
TRUNCATE
```

Constraints:
```
PRIMARY  KEY     UNIQUE   FOREIGN  REFERENCES
CHECK    CONSTRAINT  DEFAULT  NOT  NULL
AUTO_INCREMENT  SERIAL   CASCADE  RESTRICT  ACTION
```

JOIN:
```
JOIN  INNER  LEFT  RIGHT  FULL  CROSS  ON  USING
```

SELECT clauses:
```
DISTINCT  AS  ORDER  GROUP  BY  HAVING
LIMIT  OFFSET  ASC  DESC  NULLS  FIRST  LAST
```

Boolean / predicates:
```
AND  OR  NOT  IS  IN  BETWEEN  LIKE  ESCAPE
```

Literals:
```
TRUE  FALSE  NULL
```

Transaction:
```
BEGIN  COMMIT  ROLLBACK  START  TRANSACTION
```

Utility:
```
SHOW  TABLES  DESCRIBE  DESC
```

CASE:
```
CASE  WHEN  THEN  ELSE  END
```

Set operations:
```
UNION  INTERSECT  EXCEPT  ALL
```

Data types (keywords used in column definitions):
```
INT  INTEGER  BIGINT  REAL  DOUBLE  FLOAT
DECIMAL  NUMERIC  BOOL  BOOLEAN
TEXT  VARCHAR  CHAR  BLOB  BYTEA
DATE  TIMESTAMP  DATETIME  UUID
```

Session / miscellaneous:
```
WITH  NO  AUTOCOMMIT  NAMES
```

### Literals

```rust
/// Integer literal: decimal digits. Unsigned — leading `-` is a UnaryOp::Neg.
/// Parsed with `parse::<i64>()`.
Integer(i64),

/// Floating-point literal: must contain `.` or `e`/`E`.
/// Patterns: `3.14`, `.5`, `1e10`, `1.5E-3`.
Float(f64),

/// String literal delimited by single quotes `'...'`.
/// Escape sequences processed; see String escapes section.
StringLit(String),
```

### Identifiers

```rust
/// Unquoted identifier: `[A-Za-z_][A-Za-z0-9_]*` that does not match a keyword.
Ident(String),

/// Backtick-quoted identifier (MySQL): `` `any chars including spaces` ``.
/// The backticks are stripped; content is returned as-is.
QuotedIdent(String),

/// Double-quote-quoted identifier (SQL standard): `"any chars"`.
/// The quotes are stripped.
DqIdent(String),
```

### Operators

```
Eq         =
NotEq      <> or !=   (both produce the same token)
Lt         <
LtEq       <=
Gt         >
GtEq       >=
Plus       +
Minus      -
Star       *           (also SELECT wildcard — disambiguated in parser)
Slash      /
Percent    %
Concat     ||
Dot        .
```

### Punctuation

```
LParen     (
RParen     )
Comma      ,
Semicolon  ;
Colon      :
```

### Sentinel

```rust
/// Explicit end-of-input sentinel added by `tokenize()` after all tokens.
/// Simplifies parser termination: the parser matches `Eof` instead of
/// checking `Option::None` from an iterator.
Eof,
```

### Error

```rust
/// Produced by logos for any character it cannot match.
/// `tokenize()` converts this into `DbError::ParseError`.
#[error]
Error,
```

---

## String escape sequences

`process_string_literal(raw: &str) -> Result<String, DbError>`

Input is the raw content including surrounding single quotes.

| Sequence | Result |
|---|---|
| `\\` | `\` |
| `\'` | `'` |
| `\"` | `"` |
| `\n` | newline (0x0A) |
| `\r` | carriage return (0x0D) |
| `\t` | tab (0x09) |
| `\0` | null byte (0x00) |
| `\b` | backspace (0x08) |
| `\Z` | Ctrl-Z (0x1A, Windows EOF) |
| `''` | `'` (SQL standard doubling) |

Unknown escape sequences `\x` where `x` is not in the table above:
→ return the character `x` literally (MySQL lenient behavior).

---

## `tokenize` function

```rust
/// Tokenizes `input` into a flat stream of [`SpannedToken`]s.
///
/// Always appends a [`Token::Eof`] sentinel as the last element.
///
/// ## Input limits (4.2b)
///
/// If `max_bytes` is `Some(n)` and `input.len() > n`, returns
/// `Err(DbError::ParseError)` immediately without scanning.
/// Pass `None` to skip the check (useful in tests).
///
/// ## Error handling (4.2b)
///
/// - Never panics on any input.
/// - Unrecognized characters → `Err(DbError::ParseError)` with position.
/// - Unterminated string literals → `Err(DbError::ParseError)`.
/// - Integer overflow (`parse::<i64>()` fails) → `Err(DbError::ParseError)`.
pub fn tokenize(input: &str, max_bytes: Option<usize>) -> Result<Vec<SpannedToken>, DbError>
```

### Default `max_bytes`

Server and CLI callers should pass `Some(1_048_576)` (1 MB). This matches
the MySQL default `max_allowed_packet` direction. Tests pass `None`.

---

## Inputs / Outputs

| Input | Output | Errors |
|---|---|---|
| `"SELECT 1"` | `[Select, Integer(1), Eof]` | — |
| `"WHERE name = 'Alice'"` | `[Where, Ident("name"), Eq, StringLit("Alice"), Eof]` | — |
| `"  -- comment\nFROM"` | `[From, Eof]` | — |
| Oversized input | error | `DbError::ParseError { "query too long" }` |
| `"WHERE id = @"` | error | `DbError::ParseError { "unexpected '@'" }` |
| `"'unterminated"` | error | `DbError::ParseError { "unterminated string" }` |

---

## Use cases

1. **Basic SELECT**: `SELECT * FROM users WHERE id = 1` → correct token stream.
2. **String with escape**: `'it''s'` → `StringLit("it's")`.
3. **String with backslash escape**: `'hello\nworld'` → `StringLit("hello\nworld")`.
4. **Backtick identifier**: `` `my table` `` → `QuotedIdent("my table")`.
5. **Keywords as case-insensitive**: `select`, `SELECT`, `Select` all → `Token::Select`.
6. **Comments stripped**: `SELECT -- comment\n1` → `[Select, Integer(1), Eof]`.
7. **Block comment stripped**: `SELECT /* block */ 1` → `[Select, Integer(1), Eof]`.
8. **MySQL # comment**: `SELECT #comment\n1` → `[Select, Integer(1), Eof]`.
9. **Float literals**: `3.14`, `1e10`, `.5` → `Float(...)`.
10. **Integer literal**: `42` → `Integer(42)`.
11. **Unrecognized char returns error**: `@`, `$`, `^` → `Err(ParseError)`.
12. **Oversized input returns error**: input > max_bytes → `Err(ParseError)`.
13. **Unterminated string returns error**: `'abc` → `Err(ParseError)`.
14. **EOF sentinel always present**: last element is always `Eof`.
15. **Span is correct**: `SELECT` has span `{start:0, end:6}`.
16. **Empty input**: `""` → `[Eof]`.
17. **Whitespace-only**: `"   "` → `[Eof]`.
18. **Multi-statement**: `SELECT 1; SELECT 2` → includes `Semicolon` token.

---

## Acceptance criteria

- [ ] `Token` enum has all keyword, literal, identifier, operator, punctuation variants
- [ ] `Token` derives `Logos`, `Debug`, `Clone`, `PartialEq`
- [ ] Keywords are case-insensitive (`select`, `SELECT`, `Select` produce the same token)
- [ ] Whitespace and all 3 comment styles are skipped (no token produced)
- [ ] `Integer(i64)` captures decimal integer literals
- [ ] `Float(f64)` captures decimal float literals (`.5`, `1e10`, `3.14`)
- [ ] `StringLit(String)` captures single-quoted strings with escape processing
- [ ] `Ident(String)` captures unquoted identifiers that are not keywords
- [ ] `QuotedIdent(String)` captures backtick-quoted identifiers (backticks stripped)
- [ ] `DqIdent(String)` captures double-quote-quoted identifiers (quotes stripped)
- [ ] `Eof` is always the last token in the output
- [ ] `tokenize(input, None)` never panics on any input
- [ ] `tokenize` returns `Err(ParseError)` for unrecognized characters (with position)
- [ ] `tokenize` returns `Err(ParseError)` for unterminated string literals
- [ ] `tokenize` returns `Err(ParseError)` when `input.len() > max_bytes`
- [ ] `process_string_literal` handles all 10 escape sequences in the table
- [ ] `process_string_literal` handles `''` as `'` (SQL standard doubling)
- [ ] `Span { start, end }` is correct for every token
- [ ] `<>` and `!=` both produce `Token::NotEq`
- [ ] `*` produces `Token::Star`
- [ ] `||` produces `Token::Concat`
- [ ] Empty input → `[Eof]`
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- `ILIKE` keyword (case-insensitive LIKE) — Phase 5.9 (session charset)
- `SIMILAR TO` — Phase 4.x
- `$$`-quoted strings (PostgreSQL) — Phase 4.x
- Unicode escape sequences `\uXXXX` in strings — Phase 4.x
- Hex literals `0xFF` → `Integer` — Phase 4.x
- `\N` as MySQL NULL in strings — Phase 4.x
- Configurable max_query_size via DbConfig — already in DbConfig (Phase 3.16);
  caller reads it and passes as `max_bytes`

---

## Out of scope

- Parsing tokens into AST — Phase 4.3–4.4
- Keyword-as-identifier disambiguation — Phase 4.3 (parser handles this
  by accepting certain keywords in identifier positions)

---

## Dependencies

- `logos = "0.14"` — add to `axiomdb-sql/Cargo.toml` and workspace
- `axiomdb-core`: `DbError` (uses `ParseError` variant)
- `axiomdb-sql/src/lexer.rs` — new file
