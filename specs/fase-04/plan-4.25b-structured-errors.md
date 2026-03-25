# Plan: 4.25b — Structured Error Responses

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-core/src/error.rs` | Add `position` to ParseError; rename UniqueViolation fields |
| `crates/axiomdb-core/src/error_response.rs` | Populate `position`; update UniqueViolation detail |
| `crates/axiomdb-sql/src/parser/mod.rs` | Add `position` field to all ParseError constructions (11 sites) |
| `crates/axiomdb-sql/src/parser/ddl.rs` | Same (13 sites) |
| `crates/axiomdb-sql/src/parser/dml.rs` | Same (5 sites) |
| `crates/axiomdb-sql/src/parser/expr.rs` | Same (6 sites) |
| `crates/axiomdb-sql/src/index_maintenance.rs` | Update UniqueViolation construction |
| `crates/axiomdb-network/src/mysql/error.rs` | Add `sql: Option<&str>` param; snippet + new UV message |
| `crates/axiomdb-network/src/mysql/json_error.rs` | New — JSON payload builder with serde |
| `crates/axiomdb-network/src/mysql/handler.rs` | Use `build_query_err_packet` helper; pass session |
| `crates/axiomdb-network/src/mysql/mod.rs` | Expose `json_error` module |
| `crates/axiomdb-network/Cargo.toml` | Add `serde = { workspace = true }` + `serde_json = { workspace = true }` |
| `crates/axiomdb-sql/tests/integration_errors.rs` | New — integration tests for error fields |
| `tools/wire-test.py` | Add [4.25b] section |

---

## Algorithm / Data structures

### Step 1 — DbError changes (`error.rs`)

```rust
// BEFORE
ParseError { message: String },
UniqueViolation { table: String, column: String },

// AFTER
ParseError { message: String, position: Option<usize> },
UniqueViolation { index_name: String, value: Option<String> },
```

`position` is a 0-based byte offset into the original SQL string (same as `SpannedToken::span.start`).

### Step 2 — ErrorResponse changes (`error_response.rs`)

In `from_error`:
```rust
DbError::ParseError { message: _, position } => {
    // populate ErrorResponse::position from the structured field
    // (already has position: None default; override here)
}
```

Add a `with_position` builder step: `ErrorResponse::from_error` needs to also accept position.
Simplest: change `from_error` signature slightly OR set position after construction.

Preferred (no signature change, keeps `from_error` clean):
```rust
pub fn from_error(err: &DbError) -> Self {
    let (detail, hint) = derive_detail_hint(err);
    let position = match err {
        DbError::ParseError { position, .. } => *position,
        _ => None,
    };
    Self { sqlstate: ..., severity: ..., message: ..., detail, hint, position }
}
```

In `derive_detail_hint` for UniqueViolation:
```rust
DbError::UniqueViolation { index_name, value } => (
    Some(match value {
        Some(v) => format!("Key (value)=({v}) is already present in index {index_name}."),
        None    => format!("Duplicate key in index {index_name}."),
    }),
    Some(format!(
        "A row with the same value already exists in index {index_name}. \
         Use INSERT ... ON CONFLICT to handle duplicates."
    )),
),
```

### Step 3 — Parser changes (4 files, ~35 sites)

Every `DbError::ParseError { message: format!("... at position {}", p.current_pos()) }` becomes:

```rust
DbError::ParseError {
    message: format!("expected {:?} but found {:?}", expected, self.peek()),
    position: Some(self.current_pos()),
}
```

Key rules:
- Remove `at position {}` from the message text (position is now a structured field)
- All sites where `self` (the Parser) is in scope → `position: Some(self.current_pos())`
- Top-level `parse()` in `mod.rs` also has `p` in scope → `position: Some(p.current_pos())`
- Lexer-level errors (tokenize failures before the parser runs) → `position: None`
  - `tokenize()` in `lexer.rs` returns `DbError::ParseError` for unrecognized characters;
    those do NOT have a parser token position. They should remain with `position: None`.
    Note: `logos` provides byte spans; if lexer errors are already emitting position in
    message text, extract them as `position: Some(span.start)` if accessible.

### Step 4 — index_maintenance.rs

```rust
// BEFORE
return Err(DbError::UniqueViolation {
    table: idx.name.clone(),
    column: dup_val,
});

// AFTER
return Err(DbError::UniqueViolation {
    index_name: idx.name.clone(),
    value: Some(dup_val),
});
```

`dup_val` is already constructed just above: `key_vals.first().map(|v| format!("{v}")).unwrap_or_default()`.
Wrap in `Some(...)`.

### Step 5 — `dberror_to_mysql` + snippet builder (`error.rs` in network)

**Add `sql: Option<&str>` parameter:**
```rust
pub fn dberror_to_mysql(e: &DbError, sql: Option<&str>) -> MysqlError
```

**`build_error_snippet(sql: &str, pos: usize) -> String`** (new private fn):
```
if pos >= sql.len(): return ""
line_start = sql[..pos].rfind('\n').map(|i| i+1).unwrap_or(0)
line_end   = sql[pos..].find('\n').map(|i| pos+i).unwrap_or(sql.len())
line = &sql[line_start..line_end]
col  = pos - line_start   // 0-based offset within line

MAX_LINE = 120
if line.len() > MAX_LINE:
    // keep col within truncated slice
    col = col.min(MAX_LINE - 1)
    line = &line[..MAX_LINE]

return format!("\n  {line}\n  {}^", " ".repeat(col))
```

**Updated ParseError arm:**
```rust
DbError::ParseError { message, position } => {
    let snippet = position
        .and_then(|pos| sql.map(|s| build_error_snippet(s, pos)))
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let full_msg = format!("You have an error in SQL syntax: {message}{snippet}");
    (1064, b"42000", full_msg)
}
```

**Updated UniqueViolation arm:**
```rust
DbError::UniqueViolation { index_name, value } => (
    1062,
    b"23000",
    match value {
        Some(v) => format!("Duplicate entry '{v}' for key '{index_name}'"),
        None    => format!("Duplicate entry for key '{index_name}'"),
    },
),
```

### Step 6 — JSON error module (`json_error.rs` in network)

New file `crates/axiomdb-network/src/mysql/json_error.rs`:

```rust
use axiomdb_core::error::DbError;
use axiomdb_core::error_response::ErrorResponse;
use serde::Serialize;

#[derive(Serialize)]
struct JsonErrorPayload<'a> {
    code: u16,
    sqlstate: &'a str,
    severity: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    position: Option<usize>,   // always present (null if None)
}

pub fn build_json_error(code: u16, e: &DbError, sql: Option<&str>) -> String {
    let resp = ErrorResponse::from_error(e);
    // For ParseError, the snippet is in the text message; position is separate in JSON.
    // We want the clean message without the snippet for JSON consumers.
    // Use resp.message (which comes from err.to_string() — does NOT include snippet).
    let sev = resp.severity.to_string();
    let payload = JsonErrorPayload {
        code,
        sqlstate: &resp.sqlstate,
        severity: &sev,
        message: &resp.message,
        detail: resp.detail.as_deref(),
        hint: resp.hint.as_deref(),
        position: resp.position,
    };
    serde_json::to_string(&payload)
        .unwrap_or_else(|_| format!(r#"{{"message":"{}"}}"#, resp.message.replace('"', "\\\"")))
}
```

Note: in JSON mode, `message` is the clean error text (no snippet), and `position` is the numeric offset. The client uses `position` to highlight in its UI.

### Step 7 — handler.rs

Add a private helper to consolidate error-packet construction:

```rust
/// Builds an ERR packet for a database error that occurred while processing `stmt_sql`.
/// Respects the `error_format` session variable ('text' or 'json').
fn build_query_err_packet(
    e: &DbError,
    stmt_sql: &str,
    session: &ConnectionState,
) -> Vec<u8> {
    let me = dberror_to_mysql(e, Some(stmt_sql));
    let error_format = session.variables
        .get("error_format")
        .map(|s| s.as_str())
        .unwrap_or("text");
    if error_format == "json" {
        let json_msg = build_json_error(me.code, e, Some(stmt_sql));
        build_err_packet(me.code, &me.sql_state, &json_msg)
    } else {
        build_err_packet(me.code, &me.sql_state, &me.message)
    }
}
```

Replace all `dberror_to_mysql` + `build_err_packet` pairs in query/commit error paths with `build_query_err_packet(&e, &stmt_sql, &session)`.

Auth and protocol errors (no SQL context) keep using `build_err_packet` directly.

### Step 8 — integration tests

New file `crates/axiomdb-sql/tests/integration_errors.rs`:

```rust
// Test: ParseError position is populated
fn parse_error_has_position() {
    // "SELECT * FORM t" — FORM is at byte offset 9
    let err = parse("SELECT * FORM t", None).unwrap_err();
    match err {
        DbError::ParseError { position: Some(pos), .. } => {
            assert!(pos > 0, "position should be nonzero for mid-query token");
        }
        other => panic!("expected ParseError with position, got {other:?}"),
    }
}

// Test: UniqueViolation value is populated
fn unique_violation_has_value() {
    // Insert duplicate and check the error carries the offending value
    // ... setup table with UNIQUE index, insert same value twice
    match err {
        DbError::UniqueViolation { value: Some(v), .. } => {
            assert_eq!(v, "duplicate_value");
        }
        other => panic!("expected UniqueViolation with value, got {other:?}"),
    }
}

// Test: ErrorResponse position populated from ParseError
fn error_response_position_from_parse_error() {
    let err = parse("SELECT 1 FORM", None).unwrap_err();
    let resp = ErrorResponse::from_error(&err);
    assert!(resp.position.is_some());
}
```

### Step 9 — wire-test.py additions

**[4.25b] section (15 assertions):**

```python
# Parse error message contains visual snippet
# Unique violation message contains offending value
# JSON format: SET error_format = 'json', cause syntax error → response is valid JSON
# JSON format: unique violation → JSON with code, sqlstate, message, position:null
# Reset: SET error_format = 'text' → back to plain text errors
# error_format persists across statements in same connection
# error_format does not affect other connections
```

---

## Implementation phases

1. **`error.rs` changes** — `ParseError` + `UniqueViolation` field rename (15 min)
2. **`error_response.rs` changes** — populate `position`, update UV detail (15 min)
3. **Parser sites (35 sites)** — mechanical: add `position: Some(self.current_pos())`, remove "at position {}" from message text (30 min)
4. **`index_maintenance.rs`** — wrap `dup_val` in `Some(...)` (5 min)
5. **`mysql/error.rs`** — add `sql` param, `build_error_snippet`, update ParseError arm, update UV arm (20 min)
6. **`mysql/json_error.rs`** — new module (15 min)
7. **`crates/axiomdb-network/Cargo.toml`** — add serde deps (2 min)
8. **`mysql/handler.rs`** — add `build_query_err_packet` helper, replace error paths (15 min)
9. **Integration tests** — 5 targeted tests (20 min)
10. **wire-test.py** — [4.25b] section (20 min)

---

## Tests to write

**Unit (`error_response.rs`):**
- ParseError with position → ErrorResponse.position is Some
- ParseError without position → ErrorResponse.position is None
- UniqueViolation { index_name, value: Some("x") } → detail contains "x"
- UniqueViolation { index_name, value: None } → detail uses fallback

**Unit (`mysql/error.rs`):**
- `build_error_snippet("SELECT * FORM t", 9)` → contains "^" at correct column
- `build_error_snippet("", 0)` → returns ""
- `build_error_snippet("abc", 99)` → returns "" (out of bounds)
- `dberror_to_mysql(ParseError { position: Some(9) }, Some(sql))` → message contains "^"
- `dberror_to_mysql(ParseError { position: None }, None)` → plain message, no snippet

**Integration (`integration_errors.rs`):**
- parse error `position` field is Some and correct for mid-query token
- parse error `position` is None for very early lexer errors
- UniqueViolation from real INSERT → `value` field is populated

**Wire:**
- Text mode: unique violation message matches MySQL format `Duplicate entry 'X' for key 'Y'`
- Text mode: parse error message contains `^` snippet
- JSON mode: `SET error_format = 'json'` → subsequent ERR is parseable JSON
- JSON mode: JSON contains `code`, `sqlstate`, `message`, `position` fields
- JSON mode: syntax error `position` field in JSON is a number
- Reset to text: after `SET error_format = 'text'` → plain text again

---

## Anti-patterns to avoid

- **DO NOT** modify `axiomdb-core` to depend on `serde`. The JSON builder lives in `axiomdb-network`.
- **DO NOT** add the visual snippet to the `message` field of `ErrorResponse` — that field stays clean for programmatic use. The snippet is a wire-layer formatting concern only.
- **DO NOT** change `ErrorResponse::position` semantics: it stores the raw 0-based offset from the parser (`span.start`). The `POSITION: N` line in Display is for human reading; JSON consumers get the raw offset.
- **DO NOT** use `unwrap()` in production code. `build_error_snippet` must never panic (bounds-check position before indexing).
- **DO NOT** skip the `build_query_err_packet` helper pattern. The 3-way logic (sql, format, DbError) must not be duplicated across multiple handler arms.

---

## Risks

| Risk | Mitigation |
|---|---|
| 35 parser sites: forgetting one → compilation error on `DbError::ParseError { message }` missing `position` | `DbError::ParseError` uses named fields — any site not updated → compile error → caught immediately |
| `UniqueViolation` rename: any site not updated → compile error | Same: named field destructuring → compile error |
| `build_error_snippet` panicking on multi-byte UTF-8 at split point | Use `is_char_boundary` check before slicing, or only operate on ASCII-safe byte offsets (parser positions are always at token boundaries, which are UTF-8 safe) |
| JSON message in ERR packet confusing legacy MySQL clients | ERR message is just a string — any client that doesn't parse it is unaffected; no behavioral change for text mode clients |
| `serde_json::to_string` theoretically infallible for this struct | The `unwrap_or_else` fallback in `build_json_error` covers it anyway |
