# Plan: 4.2 + 4.2b — Lexer / Tokenizer + Input Sanitization

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-sql/src/lexer.rs` | CREATE | `Token`, `Span`, `SpannedToken`, `tokenize`, `process_string_literal` |
| `crates/axiomdb-sql/src/lib.rs` | MODIFY | Add `pub mod lexer` + re-exports |
| `crates/axiomdb-sql/Cargo.toml` | MODIFY | Add `logos = "0.14"` |
| `Cargo.toml` (workspace) | MODIFY | Add `logos = "0.14"` to workspace deps |
| `crates/axiomdb-sql/tests/integration_lexer.rs` | CREATE | Integration tests |

---

## Token enum implementation strategy

`logos` matches tokens in priority order:
1. Exact string tokens (`#[token(...)]`) — highest priority
2. Regex tokens (`#[regex(...)]`) — lower priority
3. `#[error]` variant — catches everything unmatched

This means `SELECT` (exact keyword) takes priority over
`[A-Za-z_][A-Za-z0-9_]*` (identifier regex), so `SELECT` becomes `Token::Select`
and `my_select` becomes `Token::Ident("my_select")`.

### Keyword macro pattern

Use the same `ignore(ascii_case)` modifier for all keywords:
```rust
#[token("SELECT", ignore(ascii_case))]
Select,
```

### Integer parsing callback

```rust
#[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
Integer(i64),
```

`logos` requires the callback to return `Option<T>` — `None` triggers an error.
Integer overflow (e.g., `99999999999999999999`) → `None` → logos error → `tokenize`
converts to `DbError::ParseError`.

### Float parsing

```rust
#[regex(
    r"[0-9]*\.[0-9]+([eE][+-]?[0-9]+)?|[0-9]+[eE][+-]?[0-9]+",
    |lex| lex.slice().parse::<f64>().ok()
)]
Float(f64),
```

Matches: `.5`, `3.14`, `1e10`, `1.5E-3`. Does NOT match `42` (that's `Integer`).

### String literal

```rust
#[regex(r"'([^'\\]|\\.|'')*'", |lex| process_string_literal(lex.slice()))]
StringLit(String),
```

The regex matches:
- `[^'\\]` — any char except quote and backslash
- `\\.` — backslash followed by any char (escape sequence)
- `''` — doubled single quote (SQL standard)

`process_string_literal` processes the raw matched text (including surrounding quotes).

Note: logos regex does not support look-ahead or backreferences. The `''` pattern
must be matched as an alternation: `[^'\\]|\\.|''`. This is logically:
"either a normal char, or an escape, or two consecutive quotes."

### Backtick identifier

```rust
#[regex(r"`[^`]*`", |lex| {
    let s = lex.slice();
    Some(s[1..s.len()-1].to_string())
})]
QuotedIdent(String),
```

### Double-quote identifier

```rust
#[regex(r#""[^"]*""#, |lex| {
    let s = lex.slice();
    Some(s[1..s.len()-1].to_string())
})]
DqIdent(String),
```

### NotEq — two spellings

```rust
#[token("<>")]
#[token("!=")]
NotEq,
```

logos allows multiple `#[token]` attributes on the same variant.

### Whitespace and comment skip

```rust
#[logos(skip r"[ \t\r\n]+")]
#[logos(skip r"--[^\n]*")]
#[logos(skip r"#[^\n]*")]
#[logos(skip r"/\*([^*]|\*[^/])*\*/")]
```

---

## `process_string_literal` algorithm

```
fn process_string_literal(raw: &str) -> Result<String, DbError>:
    // raw includes surrounding single quotes
    let inner = &raw[1..raw.len()-1]
    let mut result = String::with_capacity(inner.len())
    let mut chars = inner.chars().peekable()

    loop:
        match chars.next():
            None → break
            Some('\\') →
                match chars.next():
                    None → return Err(ParseError("unterminated escape"))
                    Some('n')  → result.push('\n')
                    Some('r')  → result.push('\r')
                    Some('t')  → result.push('\t')
                    Some('0')  → result.push('\0')
                    Some('b')  → result.push('\x08')
                    Some('Z')  → result.push('\x1A')
                    Some(c)    → result.push(c)  // \', \", \\, and others
            Some('\'') →
                // Check for '' (SQL doubling)
                if chars.peek() == Some('\''):
                    chars.next()
                    result.push('\'')
                else:
                    // Shouldn't happen (regex ensures balanced quotes)
                    break
            Some(c) → result.push(c)

    return Ok(result)
```

---

## `tokenize` algorithm

```
fn tokenize(input: &str, max_bytes: Option<usize>) -> Result<Vec<SpannedToken>>:
    // 4.2b: size check first
    if let Some(max) = max_bytes:
        if input.len() > max:
            return Err(ParseError { message: format!("query too long: {} bytes (max {})", ...) })

    let mut tokens = Vec::new()
    let mut lex = Token::lexer(input)

    while let Some(result) = lex.next():
        let span = Span { start: lex.span().start, end: lex.span().end }
        match result:
            Ok(token) → tokens.push(SpannedToken { token, span })
            Err(_) →
                // Unrecognized character
                let ch = input[span.start..].chars().next().unwrap_or('?')
                return Err(ParseError { message: format!(
                    "unexpected character '{}' at position {}", ch, span.start
                )})

    // Always append EOF sentinel
    let eof_pos = input.len()
    tokens.push(SpannedToken {
        token: Token::Eof,
        span: Span { start: eof_pos, end: eof_pos },
    })

    Ok(tokens)
```

---

## Implementation phases

### Phase 1 — Cargo.toml
1. Add `logos = "0.14"` to workspace `Cargo.toml`.
2. Add `logos = { workspace = true }` to `axiomdb-sql/Cargo.toml`.

### Phase 2 — lexer.rs: Span, SpannedToken
1. Define `Span { start: usize, end: usize }` with `Copy`.
2. Define `SpannedToken { token: Token, span: Span }`.
3. Implement `impl SpannedToken { fn new(token, start, end) -> Self }`.

### Phase 3 — lexer.rs: Token enum
1. Add `logos` derive and skip attributes.
2. Add all keyword variants (≈ 80 keywords).
3. Add `Integer(i64)` with parse callback.
4. Add `Float(f64)` with parse callback.
5. Add `StringLit(String)` with `process_string_literal` callback.
6. Add `Ident(String)` with slice-to-string callback.
7. Add `QuotedIdent(String)` (backtick) and `DqIdent(String)` (double-quote).
8. Add all operator and punctuation variants.
9. Add `Eof` and `#[error] Error`.

### Phase 4 — lexer.rs: process_string_literal
1. Implement per the algorithm above.
2. Handle all 10 escape sequences + `''` doubling.
3. Unknown escapes → return char literally (MySQL lenient mode).

### Phase 5 — lexer.rs: tokenize
1. Implement per the algorithm above.
2. max_bytes check.
3. Error conversion for unrecognized chars and integer overflow.
4. Eof sentinel.

### Phase 6 — lib.rs
1. Add `pub mod lexer;`.
2. Re-export: `Span`, `SpannedToken`, `Token`, `tokenize`.

### Phase 7 — Integration tests
File: `crates/axiomdb-sql/tests/integration_lexer.rs`

Tests:
```
Keywords:
  test_select_keyword_case_insensitive (select/SELECT/Select)
  test_all_dml_keywords
  test_all_ddl_keywords
  test_boolean_keywords (TRUE, FALSE, NULL)
  test_transaction_keywords

Literals:
  test_integer_literal
  test_float_literals (3.14, .5, 1e10, 1.5E-3)
  test_string_literal_simple
  test_string_escape_backslash_n
  test_string_escape_tab
  test_string_escape_quote
  test_string_doubling_sql_standard
  test_string_backslash_unknown_passthrough

Identifiers:
  test_plain_identifier
  test_backtick_identifier
  test_double_quote_identifier
  test_identifier_with_digits

Operators:
  test_comparison_operators (=, <>, !=, <, <=, >, >=)
  test_arithmetic_operators (+, -, *, /, %)
  test_concat_operator (||)
  test_dot_operator

Comments:
  test_line_comment_stripped
  test_mysql_hash_comment_stripped
  test_block_comment_stripped

Spans:
  test_span_select_keyword
  test_span_integer_literal
  test_span_multiple_tokens

Special:
  test_eof_always_present
  test_empty_input_eof_only
  test_whitespace_only_eof_only
  test_semicolon_in_multi_statement

Error cases (4.2b):
  test_error_unexpected_char_at_sign
  test_error_unexpected_char_caret
  test_error_query_too_long
  test_never_panics_on_random_bytes (sample of "random" inputs)

Full queries:
  test_full_select_query
  test_full_insert_query
  test_create_table_query
```

---

## Anti-patterns to avoid

- **DO NOT** use `unwrap()` anywhere in `src/` — logos callbacks return `Option<T>`,
  which logos handles. The `tokenize` function uses `?` for all fallible operations.
- **DO NOT** assume logos error spans are always valid UTF-8 char boundaries —
  use `chars().next()` to get the offending char from the slice.
- **DO NOT** rely on logos token ordering for priority — always use `#[token]` for
  keywords (higher priority than `#[regex]`).
- **DO NOT** skip `Eof` — the parser depends on it for termination.
- **DO NOT** process escapes in the regex — logos captures the raw string; a
  separate function (`process_string_literal`) handles escape processing.

---

## Risks

| Risk | Mitigation |
|---|---|
| logos regex for string literal misses `''` doubling | Test `'it''s'` explicitly |
| Block comment regex catastrophic backtracking | Use `[^*]|\*[^/]` pattern (linear) |
| Integer overflow not caught | `parse::<i64>().ok()` returns None → logos error |
| Keyword `NOT` conflicts with `NOT IN`, `NOT NULL`, `NOT LIKE` | All are separate tokens — parser handles compound keywords |
| logos 0.14 API changes | Pin to `logos = "0.14"` exactly |
| `\Z` escape (Ctrl-Z) Windows-specific | Include as documented, even if rarely used |
