# Spec: 4.25b — Structured Error Responses

## What to build (not how)

Three independent improvements to error reporting:

1. **Parse error position** — `DbError::ParseError` gains a structured `position: Option<usize>` field (1-based byte offset). The MySQL ERR message includes a visual snippet with a `^` marker under the offending token.

2. **UniqueViolation offending value** — Fix the semantic confusion in `DbError::UniqueViolation` (currently `{ table, column }` but actually stores `{ index_name, duplicate_value }`). Rename to `{ index_name: String, value: Option<String> }` and produce a MySQL-compatible message: `"Duplicate entry 'bob@x.com' for key 'users_email_idx'"`.

3. **JSON error format** — `SET error_format = 'json'` causes subsequent ERR packets to carry a JSON payload instead of a plain text message. The JSON contains all `ErrorResponse` fields: `code`, `sqlstate`, `severity`, `message`, `detail`, `hint`, `position`.

---

## Inputs / Outputs

### Parse error position

- Input: any SQL string that produces a `DbError::ParseError`
- Output:
  - `DbError::ParseError { message: String, position: Option<usize> }` — `position` is the 1-based byte offset of the first unexpected token
  - MySQL ERR message (text mode):
    ```
    You have an error in SQL syntax near 'TOKEN':
      SELECT * FORM t
               ^
    ```
  - MySQL ERR message (json mode):
    ```json
    {"code":1064,"sqlstate":"42000","severity":"ERROR","message":"...","detail":null,"hint":null,"position":10}
    ```

### UniqueViolation offending value

- Input: attempt to insert a row that violates a UNIQUE index constraint
- Old struct: `DbError::UniqueViolation { table: String, column: String }`
  - Semantically confused: `table` was the index name, `column` was the duplicate value
- New struct: `DbError::UniqueViolation { index_name: String, value: Option<String> }`
- MySQL ERR message: `"Duplicate entry 'bob@x.com' for key 'users_email_idx'"` — exactly MySQL 8 format
- `ErrorResponse::detail` for UniqueViolation: `"Key (value)=(bob@x.com) is already present in index users_email_idx."`

### JSON error format

- Input: `SET error_format = 'json'` — recognized by `ConnectionState::apply_set`
- Output: subsequent ERR packets carry a JSON message string:
  ```json
  {
    "code": 1062,
    "sqlstate": "23000",
    "severity": "ERROR",
    "message": "Duplicate entry 'bob@x.com' for key 'users_email_idx'",
    "detail": "Key (value)=(bob@x.com) is already present in index users_email_idx.",
    "hint": "A row with the same value already exists. Use INSERT ... ON CONFLICT to handle duplicates.",
    "position": null
  }
  ```
- `SET error_format = 'text'` restores the default MySQL text format
- No serde on `axiomdb-core` — JSON is built by a local serializer in `axiomdb-network`

---

## Use cases

1. **Happy path — text mode:** `SELECT * FORM t` → ERR packet with visual snippet. pymysql client sees `"You have an error in SQL syntax near 'FORM': SELECT * FORM t\n           ^"`

2. **Happy path — JSON mode:** client sends `SET error_format = 'json'`. Next syntax error → ERR packet whose message is a JSON string. Client parses JSON, reads `position: 10`, highlights char 10 in its UI.

3. **UniqueViolation — exact value in message:** `INSERT INTO users VALUES (1, 'bob@x.com')` on a table with UNIQUE(email). ERR message: `"Duplicate entry 'bob@x.com' for key 'users_email_idx'"`.

4. **Lexer-level parse error (no position):** some errors from the lexer pre-date token position tracking. These produce `ParseError { message: "...", position: None }`. No snippet is shown; plain `"You have an error in SQL syntax: ..."` is used.

5. **Position beyond end of input:** position ≥ sql.len() → no snippet, just the message with `" at position N"` appended.

6. **Multi-statement batch:** each statement is processed independently; position is relative to the start of the failing statement (not the full batch string).

7. **error_format persists across statements** within a connection until explicitly reset.

---

## Acceptance criteria

- [ ] `DbError::ParseError { message, position }` — `position` is `Some(byte_offset)` for all parser errors that have a known token position; `None` for lexer-level and other parse errors
- [ ] ERR packet message (text mode) includes a visual snippet with `^` marker when position is available and sql is available
- [ ] `DbError::UniqueViolation` renamed to `{ index_name, value }` — no references to old `{ table, column }` remain in production code
- [ ] MySQL ERR message for UniqueViolation: `"Duplicate entry '{value}' for key '{index_name}'"` exactly
- [ ] `ErrorResponse::detail` for UniqueViolation includes the offending value
- [ ] `SET error_format = 'json'` → subsequent ERR packets are valid JSON strings
- [ ] `SET error_format = 'text'` → reverts to plain MySQL text errors
- [ ] `ErrorResponse::position` is populated from `ParseError::position`
- [ ] All existing unit tests in `error_response.rs` pass after the rename
- [ ] Integration test: parse error returns position and snippet
- [ ] Integration test: unique violation message contains the offending value
- [ ] Wire test: 15 new assertions covering all three features
- [ ] `cargo test --workspace` clean; `cargo clippy -- -D warnings` clean

---

## Out of scope

- Warning system (`SET sql_mode = 'STRICT_...'`) — Phase 4.25c (requires session-level warning collection across multiple statements)
- Internal position (`internalposition`) — PostgreSQL concept; not needed for MySQL compat
- Schema/table/column name fields in `ErrorResponse` — Phase 6+
- `RAISE EXCEPTION`/`SIGNAL SQLSTATE` — stored procedures (Phase 17)
- OData error format — Phase 4.25x

---

## Dependencies

- `axiomdb-core` error types already exist (`DbError`, `ErrorResponse`)
- `ConnectionState::apply_set` already exists and handles session variables
- `serde_json` is already in the workspace root `Cargo.toml`
- `axiomdb-network` already depends on `axiomdb-core` — a local `JsonError` struct with `serde::Serialize` can live entirely in `axiomdb-network` without touching `axiomdb-core`'s Cargo.toml

---

## Key design decisions

### `DbError::UniqueViolation` rename rationale

The current field names `table` and `column` are semantically wrong: `table` holds the index name (not the table name), and `column` holds the duplicate value (not the column name). This mismatch causes incorrect error messages like `"...in table 'users_email_idx'"`. The rename to `{ index_name, value }` fixes the semantics and enables the MySQL-compatible message format.

### Why `value: Option<String>` and not `value: String`

At the raise site (`index_maintenance.rs`), the duplicate value is constructed from `key_vals.first()`. For composite indexes or edge cases, it is possible that `key_vals` is empty. `Option<String>` covers this gracefully: the error message becomes `"Duplicate entry '' for key ..."` (empty) or we use `"(unknown)"` as the fallback.

### Why a local JSON struct in `axiomdb-network`, not serde on `axiomdb-core`

`axiomdb-core` is a foundational crate with no network dependencies. Adding `serde` there would: (a) increase compilation time for all downstream crates, (b) couple the core domain model to a presentation concern. The JSON payload is a wire-level concern — it belongs in `axiomdb-network`. A local `#[derive(Serialize)] struct JsonErrorPayload` in `handler.rs` or a new `json_error.rs` module keeps the layers clean.

### Visual snippet construction

```
build_error_snippet(sql: &str, position: usize) -> String
  // position is 0-based byte offset internally; display as 1-based
  // find the line containing the byte offset
  // construct: "\n  {line}\n  {spaces}^"
  // truncate the line to 120 chars max, adjusting ^ if truncated
  // if position >= sql.len(): return empty string (no snippet)
```

The handler uses this: when `error_format = Text` and the error is `ParseError { position: Some(pos) }`, the snippet is appended to the MySQL error message.
