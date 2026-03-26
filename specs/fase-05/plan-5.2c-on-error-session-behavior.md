# Plan: 5.2c — ON_ERROR session behavior

## Files to create/modify

- `crates/axiomdb-sql/src/session.rs`
  - add the typed `OnErrorMode` enum
  - add parse/format helpers shared by executor and wire layer
  - add the shared `is_ignorable_on_error(...)` classifier used by executor and
    database
  - extend `SessionContext` with `on_error`
- `crates/axiomdb-sql/src/executor.rs`
  - extend `execute_set_ctx(...)` to support `SET on_error = ...`
  - branch statement-failure rollback policy on `ctx.on_error`
  - make `savepoint` observably different from `rollback_statement` in the
    `autocommit = 0` first-DML path
- `crates/axiomdb-network/src/mysql/session.rs`
  - persist `on_error` in `ConnectionState`
  - parse `SET on_error = ...` in the intercepted wire path
  - expose `@@on_error`
- `crates/axiomdb-network/src/mysql/handler.rs`
  - sync `ConnectionState.on_error` into `SessionContext.on_error` after
    intercepted `SET`
  - keep `COM_RESET_CONNECTION` aligned with the new default
  - allow multi-statement `COM_QUERY` to continue after ignored errors
- `crates/axiomdb-network/src/mysql/database.rs`
  - apply `on_error` to parse/analyze/execute failures across the full pipeline
  - convert ignorable errors into warnings + `QueryResult::Empty` in `ignore`
- `crates/axiomdb-network/src/mysql/error.rs`
  - expose a helper that turns a `DbError` into the MySQL warning code/message
    used by `ignore`
- `crates/axiomdb-sql/tests/integration_autocommit.rs`
  - extend transaction-semantic tests for `rollback_transaction` and
    `savepoint`
- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add executor/session-variable tests for `SET on_error = ...`
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - add wire-layer tests for `@@on_error`, reset behavior, and ignored errors
- `tools/wire-test.py`
  - add live regressions for all four modes and keep existing warnings/txn tests

## Reviewed first

These files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-core/src/error.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/error.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/tests/integration_autocommit.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `tools/wire-test.py`
- `specs/fase-05/spec-5.2c-on-error-session-behavior.md`

Research reviewed before writing this plan:

- `research/mariadb-server/sql/handler.cc`
- `research/sqlite/src/btree.c`
- `research/postgres/src/backend/utils/errcodes.txt`
- `research/oceanbase/src/sql/session/ob_sql_session_info.cpp`

## Research synthesis

### AxiomDB-first constraints

- `database.rs` owns the full `parse -> analyze -> execute_with_ctx` pipeline,
  so `on_error` cannot live only in the executor.
- `handler.rs` intercepts `SET` before the engine, so a wire-only session mode
  would leave embedded/tests inconsistent.
- `executor.rs` already has the correct low-cost primitive for statement
  rollback: `Savepoint` is just an undo-log index, not a persisted object.
- `SHOW WARNINGS` is already session-backed, so `ignore` should reuse it.

### What we borrow

- `research/mariadb-server/sql/handler.cc`
  - borrow: a statement can be atomic inside a larger transaction
- `research/sqlite/src/btree.c`
  - borrow: statement rollback is naturally modeled as an anonymous savepoint
- `research/postgres/src/backend/utils/errcodes.txt`
  - borrow: some users explicitly want whole-transaction failure semantics
- `research/oceanbase/src/sql/session/ob_sql_session_info.cpp`
  - borrow: implicit-savepoint behavior should be tied to session state

### What we reject

- copying PostgreSQL's full aborted-transaction state machine into this phase
- adding `on_error` only to `ConnectionState`
- making `ignore` swallow storage/WAL/runtime failures

### How AxiomDB adapts it

- `rollback_transaction` becomes eager whole-txn rollback
- `savepoint` means "preserve even the first implicit `autocommit=0` txn after
  a failing DML"
- `ignore` is layered on top of the existing warning system and serializes as
  OK-with-warning, not as a synthetic result set

## Algorithm / Data structure

### 1. Add a shared typed session enum

In `crates/axiomdb-sql/src/session.rs` add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnErrorMode {
    RollbackStatement,
    RollbackTransaction,
    Savepoint,
    Ignore,
}

pub fn parse_on_error_setting(raw: &str) -> Result<OnErrorMode, DbError>;
pub fn on_error_mode_name(mode: OnErrorMode) -> &'static str;
```

Accepted forms:

- `'rollback_statement'`
- `'rollback_transaction'`
- `'savepoint'`
- `'ignore'`
- unquoted identifiers with the same names
- `DEFAULT` resets to `RollbackStatement`

Do **not** store this as a stringly-typed source of truth in `SessionContext`.
Store the enum and derive the visible string when answering `@@on_error`.

### 2. Extend SessionContext

Add:

```rust
pub struct SessionContext {
    ...
    pub on_error: OnErrorMode,
}
```

`SessionContext::new()` must initialize:

```rust
on_error: OnErrorMode::RollbackStatement
```

### 3. Extend ConnectionState and SET parsing

`ConnectionState` needs the same typed field:

```rust
pub struct ConnectionState {
    ...
    on_error: OnErrorMode,
    pub variables: HashMap<String, String>,
}
```

Canonical visible variable value:

```rust
variables["on_error"] = "rollback_statement" | ...
```

`apply_set(...)` decision:

- parse `SET on_error = ...` with `parse_on_error_setting(...)`
- update both:
  - `self.on_error`
  - `self.variables["on_error"]`
- invalid values return `DbError::InvalidValue`
- `get_variable("on_error")` returns the canonical lowercase name

This keeps `SELECT @@on_error` working through the existing generic `@@var`
interception path.

### 4. Close the bare-identifier gap in executor-side SET

`execute_set_ctx(...)` currently works well for boolish/string settings but
`SET on_error = rollback_statement` would otherwise arrive as an identifier
expression instead of a string literal.

Add one local helper in `executor.rs`:

```rust
fn set_value_to_setting_string(value: &SetValue) -> Result<Option<String>, DbError>
```

Rules:

- `SetValue::Default` => `Ok(None)`
- `SetValue::Expr(Expr::Literal(Value::Text(s)))` => `Ok(Some(s.clone()))`
- `SetValue::Expr(Expr::Literal(Value::Bool(b)))` / ints => stringify as today
- `SetValue::Expr(Expr::Column { name, .. })` => `Ok(Some(name.clone()))`
- any other expression => `Err(DbError::InvalidValue { ... })`

Use this helper for `on_error` and leave the current `autocommit` /
`strict_mode` / `sql_mode` logic intact unless the shared helper makes the code
smaller without changing behavior.

This explicitly closes the "quoted string vs bare identifier" ambiguity.

### 5. Executor rollback policy by mode

#### Active transaction already open

This covers:

- explicit `BEGIN`
- implicit `autocommit = 0` transaction after the first successful DML

Use this policy table:

| Mode | On statement error |
|---|---|
| `RollbackStatement` | `rollback_to_savepoint(sp)` and keep txn open |
| `RollbackTransaction` | `rollback(storage)` and close txn |
| `Savepoint` | `rollback_to_savepoint(sp)` and keep txn open |
| `Ignore` | if error is ignorable: `rollback_to_savepoint(sp)` and keep txn open; else `rollback(storage)` and return ERR |

Implementation detail:

- still create `Savepoint` before dispatch for all modes except
  `RollbackTransaction`
- `RollbackTransaction` may skip `savepoint()` because it always rolls back the
  full txn on error

#### No active transaction, `autocommit = true`

Leave the existing single-statement transaction wrapper unchanged except for
`Ignore`:

- `RollbackStatement` / `RollbackTransaction` / `Savepoint`
  - current behavior: begin, dispatch, commit or rollback whole statement txn
- `Ignore`
  - if the error is ignorable: rollback the statement txn, let `database.rs`
    convert the error into a warning + `QueryResult::Empty`
  - if the error is non-ignorable: normal ERR

`Savepoint` is intentionally the same as current autocommit behavior here.
There is no larger transaction to preserve.

#### No active transaction, `autocommit = false`

This is where `savepoint` must differ from `rollback_statement`.

For `Stmt::Select(_)`:

- keep current behavior for **all** modes
- read-only `SELECT` does not open a lasting implicit txn when none existed

For DDL:

- keep current MySQL-style implicit-commit/autocommit behavior
- `on_error` does not override DDL transaction boundaries in `5.2c`

For first DML statement:

| Mode | On first DML error with no prior active txn |
|---|---|
| `RollbackStatement` | begin txn, dispatch, full `rollback(storage)`, txn closes |
| `RollbackTransaction` | same as above |
| `Savepoint` | begin txn, create savepoint immediately, dispatch, `rollback_to_savepoint(sp)`, txn stays open |
| `Ignore` | if ignorable: same as `Savepoint`; else full `rollback(storage)` and ERR |

This is the only new executor-path behavior required to make `savepoint`
observably distinct.

### 6. Apply ON_ERROR to parse/analyze failures too

`database.rs` must wrap the full pipeline, not only executor errors.

Add one helper in `database.rs`:

```rust
fn apply_on_error_pipeline_failure(
    &mut self,
    sql: &str,
    session: &mut SessionContext,
    err: DbError,
) -> Result<(QueryResult, Option<CommitRx>), DbError>
```

This helper is called from all three failure points:

1. `parse(sql, None)` fails
2. `analyze_cached(...)` fails
3. `execute_with_ctx(...)` fails

Rules:

- `RollbackStatement`
  - leave any active txn open for parse/analyze failures
  - return ERR
- `RollbackTransaction`
  - if `txn.active_txn_id().is_some()`, call `txn.rollback(&mut self.storage)`
  - return ERR
- `Savepoint`
  - same as `RollbackStatement` for parse/analyze failures
  - executor already handled execution-time rollback
- `Ignore`
  - if `is_ignorable_on_error(&err)`:
    - map `DbError` to MySQL warning code/message
    - `session.warn(code, message)`
    - return `Ok((QueryResult::Empty, None))`
  - else:
    - if a txn is active, eagerly `rollback(storage)`
    - return ERR

This keeps the mode semantics consistent across syntax, semantic, and execution
errors.

### 7. Make ignorable classification explicit and exhaustive

Add one helper in `crates/axiomdb-sql/src/session.rs` and keep it as an
exhaustive `match DbError`:

```rust
fn is_ignorable_on_error(err: &DbError) -> bool
```

Positive list:

- `ParseError`
- `TableNotFound`
- `ColumnNotFound`
- `AmbiguousColumn`
- `UniqueViolation`
- `ForeignKeyViolation`
- `ForeignKeyParentViolation`
- `ForeignKeyCascadeDepth`
- `ForeignKeySetNullNotNullable`
- `ForeignKeyNoParentIndex`
- `NotNullViolation`
- `CheckViolation`
- `TypeMismatch`
- `InvalidValue`
- `InvalidCoercion`
- `DivisionByZero`
- `Overflow`
- `NoActiveTransaction`
- `TransactionAlreadyActive`
- `CardinalityViolation`
- `ColumnAlreadyExists`
- `TableAlreadyExists`
- `IndexAlreadyExists`
- `IndexKeyTooLong`
- `NotImplemented`

Everything else is non-ignorable in `5.2c`.

Do **not** use a default `_ => true` or `_ => false` arm silently. Keep the
match exhaustive so new `DbError` variants force a conscious decision.

### 8. Reuse MySQL error mapping for warnings

In `mysql/error.rs`, add:

```rust
pub fn dberror_to_mysql_warning(e: &DbError, sql: Option<&str>) -> (u16, String)
```

Implementation:

- call the existing `dberror_to_mysql(...)`
- return `(code, message)`

`ignore` warnings must reuse the same message users would have seen in an ERR
packet, including parse snippets when available.

### 9. Handler call sites and multi-statement continuation

There are three concrete handler changes:

1. After intercepted `SET ...` success in the single-statement `COM_QUERY`
   branch, sync:

```rust
session.autocommit = conn_state.autocommit;
session.strict_mode = sql_mode_is_strict(...);
session.on_error = conn_state.on_error();
```

2. Do the same sync inside the split multi-statement loop after intercepted
   `SET ...`.

3. In the multi-statement loop, when `guard.execute_query(...)` returns
   `Ok((QueryResult::Empty, None))` because an ignorable error was swallowed,
   continue as normal and do **not** break the statement loop.

No new handler return type is needed. The continuation signal is implicit in the
successful `Ok(...)` result.

Do **not** change the existing warning lifetime model in this phase:

- `execute_query(...)` still clears warnings per statement
- therefore the ignored statement's OK packet is the authoritative warning
  carrier inside a multi-statement packet
- `SHOW WARNINGS` after later statements still shows only the most recent
  statement's warnings

### 10. SHOW VARIABLES integration

Extend `show_variables_result(...)` to include:

```text
on_error
```

No separate special-case path is needed because `get_variable("on_error")`
already answers `SELECT @@on_error`.

## Implementation phases

1. Add `OnErrorMode` + shared parse/format helpers in `session.rs`.
2. Extend `SessionContext` and `ConnectionState` defaults plus `SET on_error`.
3. Add executor-side `SET on_error` support and the bare-identifier helper.
4. Change executor rollback policy tables for:
   - active txn
   - autocommit=true
   - autocommit=false first DML
5. Add exhaustive `is_ignorable_on_error(...)`.
6. Add `dberror_to_mysql_warning(...)`.
7. Wrap parse/analyze/execute failures in `database.rs` with
   `apply_on_error_pipeline_failure(...)`.
8. Sync `session.on_error` in handler call sites and keep `COM_RESET_CONNECTION`
   aligned.
9. Extend `SHOW VARIABLES` and `@@on_error`.
10. Add unit/integration/wire tests, then update `tools/wire-test.py`.

## Tests to write

- unit:
  - `parse_on_error_setting("rollback_statement")`
  - `parse_on_error_setting("ROLLBACK_TRANSACTION")`
  - invalid `on_error` value rejected
  - `ConnectionState::apply_set("SET on_error = savepoint")`
  - `get_variable("@@on_error")` returns canonical lowercase value
  - `is_ignorable_on_error(...)` accepts SQL/user errors and rejects
    `DiskFull` / `Io` / `Wal*` / `Internal`
- integration:
  - default `rollback_statement` preserves active txn after duplicate-key error
  - `rollback_transaction` closes the txn and discards prior writes
  - `savepoint` keeps the first implicit `autocommit=0` txn open after failing
    DML
  - executor-side `SET on_error = ...` updates `SessionContext`
  - `ignore` converts duplicate-key and parse errors into warnings + success
  - `ignore` does not swallow non-ignorable errors
- wire:
  - `SELECT @@on_error`
  - `SHOW VARIABLES LIKE 'on_error'`
  - `COM_RESET_CONNECTION` resets `@@on_error`
  - `SET on_error='rollback_transaction'` + failing txn leaves
    `@@in_transaction = 0`
  - `SET autocommit=0; SET on_error='savepoint'` + first failing DML leaves
    `@@in_transaction = 1`
  - `SET on_error='ignore'` returns OK + warning_count and `SHOW WARNINGS`
    exposes the original code/message
  - multi-statement `BEGIN; INSERT ok; INSERT duplicate; INSERT ok2; COMMIT`
    with `ignore` commits `ok` and `ok2`
  - multi-statement ignored warning is present on the ignored statement's OK
    packet even though `SHOW WARNINGS` later follows the existing
    last-statement-only rule
- bench:
  - no new benchmark required
  - verify no measurable regression on the existing `5.14` statement path
    caused by the extra mode branch in default `rollback_statement`

## Anti-patterns to avoid

- Do **not** add `on_error` only to `ConnectionState`; embedded/tests would
  diverge from wire behavior.
- Do **not** implement `rollback_transaction` as a PostgreSQL-style failed-txn
  latch in this phase.
- Do **not** use string comparisons in the hot executor path once the session
  mode has been parsed into an enum.
- Do **not** classify ignorable errors with a catch-all wildcard arm.
- Do **not** serialize ignored `SELECT` failures as fake empty rowsets; use
  `QueryResult::Empty` -> OK packet with warnings.
- Do **not** swallow `DiskFull`, `Io`, WAL failures, corruption, or `Internal`.

## Risks

- `ignore` may accidentally swallow a serious runtime error.
  - Mitigation: exhaustive positive-list `is_ignorable_on_error(...)`.
- `savepoint` may become indistinguishable from `rollback_statement`.
  - Mitigation: explicit test on first failing DML under `autocommit = 0`
    asserting `@@in_transaction = 1`.
- parse/analyze failures could bypass the policy if only executor code changes.
  - Mitigation: centralize failure handling in `database.rs`.
- wire and embedded paths could diverge on accepted `SET on_error` forms.
  - Mitigation: reuse `parse_on_error_setting(...)` in both paths and add tests
    for quoted and bare-identifier values.
