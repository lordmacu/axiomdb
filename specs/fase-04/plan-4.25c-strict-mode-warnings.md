# Plan: 4.25c — Strict Mode + Warnings System

## Files reviewed before writing this plan

- `db.md`
- `docs/progreso.md`
- `specs/fase-04/spec-4.18b.md`
- `specs/fase-04/spec-4.25.md`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-sql/tests/integration_table.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `tools/wire-test.py`
- `research/mariadb-server/sql/sys_vars.cc`
- `research/mariadb-server/mysql-test/suite/innodb/r/alter_not_null.result`
- `research/mariadb-server/mysql-test/suite/innodb/r/innodb-autoinc.result`
- `research/sqlite/src/insert.c`

## Files to create/modify

- `crates/axiomdb-sql/src/session.rs` — add `strict_mode` to `SessionContext`
  plus shared helpers for strict/sql_mode parsing and normalization
- `crates/axiomdb-sql/src/table.rs` — add ctx-aware assignment coercion helpers
  that emit warnings without changing the strict-only internal APIs
- `crates/axiomdb-sql/src/executor.rs` — route ctx-aware `INSERT` / `UPDATE`
  through the new helpers and implement `Stmt::Set` handling in `dispatch_ctx`
- `crates/axiomdb-network/src/mysql/session.rs` — sync wire `strict_mode` /
  `sql_mode` session variables with the shared parsing rules
- `crates/axiomdb-network/src/mysql/handler.rs` — sync `SessionContext` after
  intercepted `SET` and expose `strict_mode` / `sql_mode` in `SHOW VARIABLES`
- `crates/axiomdb-sql/tests/integration_table.rs` — add table-engine tests for
  ctx-aware permissive assignment warnings
- `crates/axiomdb-sql/tests/integration_executor.rs` — add executor tests for
  `SET strict_mode` / `SET sql_mode` plus INSERT/UPDATE behavior
- `tools/wire-test.py` — add wire-visible strict-mode warning assertions and
  regressions for `SHOW WARNINGS`

## Algorithm / Data structure

### 1. Shared session-setting helpers in `axiomdb-sql::session`

Add these helpers to `crates/axiomdb-sql/src/session.rs` so the executor and
wire session layer use identical rules:

```rust
pub fn parse_boolish_setting(raw: &str) -> Result<bool, DbError>;
pub fn normalize_sql_mode(raw: &str) -> String;
pub fn sql_mode_is_strict(normalized: &str) -> bool;
pub fn apply_strict_to_sql_mode(current: &str, enabled: bool) -> String;
```

Rules:

- `parse_boolish_setting` accepts case-insensitive `1/0`, `on/off`,
  `true/false`
- `normalize_sql_mode`:
  - trims outer quotes
  - splits on `,`
  - trims each token
  - uppercases tokens
  - drops empty tokens
  - removes duplicates while preserving left-to-right order
  - rejoins with `,`
- `sql_mode_is_strict` returns `true` when normalized tokens contain
  `STRICT_TRANS_TABLES` or `STRICT_ALL_TABLES`
- `apply_strict_to_sql_mode` preserves all existing non-strict tokens,
  removes both strict tokens, and inserts `STRICT_TRANS_TABLES` at the front
  when `enabled = true`

`SessionContext` gains:

```rust
pub strict_mode: bool,
```

with default `true`.

No `sql_mode` string is added to `SessionContext`; only the execution-relevant
strict flag lives there. The textual `sql_mode` representation remains a wire
session concern in `ConnectionState`.

### 2. Wire session variable semantics in `ConnectionState`

Update `crates/axiomdb-network/src/mysql/session.rs`:

- Default variables:
  - `sql_mode = "STRICT_TRANS_TABLES"`
  - `strict_mode = "ON"`
- `apply_set(sql)` special-cases:
  - `strict_mode`
  - `sql_mode`
  - existing `autocommit`, charset, packet limit logic
- `SET strict_mode = ...`
  - validate with `parse_boolish_setting`
  - store `strict_mode = "ON"` or `"OFF"`
  - rewrite `sql_mode` via `apply_strict_to_sql_mode`
- `SET strict_mode = DEFAULT`
  - equivalent to strict ON
- `SET sql_mode = ...`
  - normalize with `normalize_sql_mode`
  - store the normalized string
  - derive `strict_mode = "ON"` or `"OFF"` from `sql_mode_is_strict`
- `get_variable("strict_mode")` returns `"ON"` or `"OFF"`
- `get_variable("sql_mode")` returns the normalized string

No new `bool` field is added to `ConnectionState`; the wire layer keeps a
single textual source of truth and derives the execution bit from it.

### 3. Session sync in `handler.rs`

After a successful intercepted `SET` in `intercept_special_query(...)`,
`handle_connection(...)` already syncs:

```rust
session.autocommit = conn_state.autocommit;
```

Extend that same sync point to:

```rust
session.strict_mode =
    axiomdb_sql::session::sql_mode_is_strict(
        conn_state.get_variable("sql_mode").unwrap_or_default().as_str()
    );
```

This keeps the existing architecture:

- `ConnectionState` owns wire-visible variables
- `SessionContext` owns executor-visible session behavior
- sync happens after `SET`, before the next statement reaches the engine

Also extend `show_variables_result(...)` so the rowset includes both:

- `strict_mode`
- `sql_mode`

using `conn_state.get_variable(...)` so the visible values stay consistent with
`SELECT @@...`.

### 4. ctx-aware assignment helpers in `table.rs`

Keep existing strict-only public APIs unchanged for internal callers:

- `insert_row`
- `insert_rows_batch`
- `update_row`
- `update_rows_batch`

Add ctx-aware companions for real session execution:

```rust
pub fn insert_row_with_ctx(..., ctx: &mut SessionContext, values: Vec<Value>) -> Result<RecordId, DbError>;
pub fn insert_rows_batch_with_ctx(..., ctx: &mut SessionContext, batch: &[Vec<Value>]) -> Result<Vec<RecordId>, DbError>;
pub fn update_row_with_ctx(..., ctx: &mut SessionContext, record_id: RecordId, new_values: Vec<Value>) -> Result<RecordId, DbError>;
pub fn update_rows_batch_with_ctx(..., ctx: &mut SessionContext, updates: Vec<(RecordId, Vec<Value>)>) -> Result<u64, DbError>;
```

Add private helpers:

```rust
fn coerce_values_strict(values: Vec<Value>, columns: &[ColumnDef]) -> Result<Vec<Value>, DbError>;
fn coerce_values_with_ctx(
    values: Vec<Value>,
    columns: &[ColumnDef],
    ctx: &mut SessionContext,
    row_num: usize,
) -> Result<Vec<Value>, DbError>;
```

`coerce_values_with_ctx(...)` algorithm:

```text
for each (value, column) in declaration order:
  target = column -> DataType

  if ctx.strict_mode:
    out.push(coerce(value, target, Strict)?)
    continue

  try strict = coerce(value.clone(), target, Strict)
  if strict succeeds:
    out.push(strict_value)
    continue

  try permissive = coerce(value, target, Permissive)
  if permissive succeeds:
    ctx.warn(1265, format!("Data truncated for column '{}' at row {}", column.name, row_num))
    out.push(permissive_value)
    continue

  return Err(permissive_error)
```

Important fixed choices:

- warning emission lives in `table.rs`, not in `axiomdb-types`
- warning emission happens only when strict failed and permissive succeeded
- if both modes fail, return the permissive-mode error
- one warning is emitted per coerced column, not per row
- row numbering is passed in from the batch/update loop as 1-based

### 5. Executor routing

Update `crates/axiomdb-sql/src/executor.rs`:

- `dispatch_ctx(...)` handles `Stmt::Set(stmt)` directly via
  `execute_set_ctx(stmt, ctx)`
- `dispatch(...)` stays unchanged; the non-ctx path remains the old stub

`execute_set_ctx(...)` behavior:

```text
match stmt.variable.to_ascii_lowercase():
  "autocommit" =>
    ctx.autocommit = parse_boolish_setting(value)?
    return Empty

  "strict_mode" =>
    if value == DEFAULT: ctx.strict_mode = true
    else ctx.strict_mode = parse_boolish_setting(value)?
    return Empty

  "sql_mode" =>
    if value == DEFAULT:
      ctx.strict_mode = true
    else:
      normalized = normalize_sql_mode(literal_string)?
      ctx.strict_mode = sql_mode_is_strict(&normalized)
    return Empty

  _ =>
    return Empty   // keep existing stub behavior for unrelated variables
```

For `sql_mode` in the ctx path:

- accept only string literals or `DEFAULT`
- reject non-literal expressions with `DbError::InvalidValue`

Then switch ctx-aware DML call sites:

- `execute_insert_ctx(...)`
  - replace `TableEngine::insert_row(...)` with `insert_row_with_ctx(...)`
  - replace `insert_rows_batch(...)` with `insert_rows_batch_with_ctx(...)`
- `execute_update_ctx(...)`
  - replace `update_row(...)` with `update_row_with_ctx(...)`
  - replace `update_rows_batch(...)` with `update_rows_batch_with_ctx(...)`

Do **not** change:

- `execute_insert(...)` non-ctx path
- `execute_update(...)` non-ctx path
- `rewrite_rows(...)`
- `fk_enforcement.rs`

Those paths intentionally stay strict-only in 4.25c.

## Implementation phases

1. Add shared parsing / normalization helpers and `SessionContext.strict_mode`
   in `crates/axiomdb-sql/src/session.rs`.
2. Update `ConnectionState` defaults, `apply_set`, and `get_variable` so
   `strict_mode` / `sql_mode` stay synchronized.
3. Extend `handler.rs` sync-after-SET logic and `SHOW VARIABLES` rowset.
4. Add ctx-aware `TableEngine` write variants and private coercion helper with
   warning emission.
5. Update `dispatch_ctx`, add `execute_set_ctx`, and route ctx-aware
   `INSERT` / `UPDATE` through the new `TableEngine` APIs.
6. Add unit/integration/wire tests for strict default, permissive fallback,
   warning messages, `sql_mode` aliasing, and regression coverage.

## Tests to write

- unit: `crates/axiomdb-network/src/mysql/session.rs`
  - `SET strict_mode = OFF` -> `strict_mode=OFF`, `sql_mode` strict token removed
  - `SET strict_mode = ON` -> `strict_mode=ON`, `sql_mode` contains `STRICT_TRANS_TABLES`
  - `SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'` -> strict on, full normalized string preserved
  - `SET sql_mode = ''` -> strict off
  - invalid `SET strict_mode = maybe` -> `DbError::InvalidValue`

- unit/integration: `crates/axiomdb-sql/tests/integration_table.rs`
  - strict ON + `'42abc'` into `INT` -> error
  - strict OFF + `'42abc'` into `INT` -> stores `42`, one warning `1265`
  - strict OFF + `'abc'` into `INT` -> stores `0`, one warning `1265`
  - strict OFF + permissive failure (overflow) -> error, no warning

- integration: `crates/axiomdb-sql/tests/integration_executor.rs`
  - `SET strict_mode = OFF` via `execute_with_ctx` affects subsequent `INSERT`
  - `SET sql_mode = ''` via `execute_with_ctx` affects subsequent `UPDATE`
  - `SET sql_mode = DEFAULT` / `SET strict_mode = DEFAULT` restore strict ON
  - `SELECT CAST('abc' AS INT)` still errors when strict is OFF
  - multi-row `INSERT` warning messages use `row 1`, `row 2`

- wire: `tools/wire-test.py`
  - `SET strict_mode = OFF`
  - insert/update with lossy coercion succeeds
  - `SHOW WARNINGS` returns `1265` rows
  - `SELECT @@strict_mode` and `SELECT @@sql_mode` reflect toggles
  - regression: no warnings after a clean statement

## Anti-patterns to avoid

- Do not change `axiomdb-types::coerce(...)` to return warnings or session-aware
  metadata.
- Do not replace all `TableEngine` write APIs with ctx-bearing signatures;
  internal DDL/FK paths must stay strict-only in this subphase.
- Do not infer strictness from a second boolean in `ConnectionState`; keep the
  wire source of truth in `sql_mode` + derived `strict_mode`.
- Do not advertise full MySQL `sql_mode` parity. In 4.25c only strict tokens
  affect execution.
- Do not broaden strict-mode behavior to `CAST`, expression evaluation, or
  internal row rewrites.
- Do not emit warnings when both strict and permissive coercion fail.
- Do not use the existing `SHOW VARIABLES` substring matcher as evidence of full
  LIKE parity; tests for this subphase must use exact-name visibility.

## Risks

- Divergent parsing rules between executor `SET` and wire `SET`
  -> Mitigation: shared helpers live in `axiomdb-sql/src/session.rs` and are
     reused by both layers.

- Accidentally changing internal DDL/FK maintenance behavior
  -> Mitigation: add ctx-aware `TableEngine` variants instead of mutating the
     existing strict-only APIs.

- Wrong warning counts in batch paths
  -> Mitigation: emit warnings inside the row/column coercion loop with
     explicit 1-based row numbers and cover both single-row and multi-row tests.

- Claiming MySQL compatibility for behaviors AxiomDB still errors on
  -> Mitigation: keep overflow/clamping explicitly deferred and test that it
     still errors in both modes.
