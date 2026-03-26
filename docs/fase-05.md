# Phase 5 — MySQL Wire Protocol

## Subfases completed in this session: 5.2c

## What was built

### 5.2c — ON_ERROR session behavior

`ON_ERROR` is now a real session variable across both the SQL executor and the
MySQL wire layer.

Supported values:

- `rollback_statement` — default; rollback only the failing statement and keep
  an already-active transaction open
- `rollback_transaction` — eager whole-transaction rollback on error
- `savepoint` — preserves the first implicit `autocommit=0` transaction after a
  failing first DML
- `ignore` — converts ignorable SQL errors into warnings + OK, while
  non-ignorable runtime/storage/WAL failures still return ERR and eagerly roll
  back the active transaction

Implemented surfaces:

- `SET on_error = ...` and `SET on_error = DEFAULT`
- bare identifiers without quotes: `SET on_error = rollback_statement`
- `SELECT @@on_error`
- `SELECT @@session.on_error`
- `SHOW VARIABLES LIKE 'on_error'`
- `COM_RESET_CONNECTION` reset to `rollback_statement`

Implementation highlights:

- `axiomdb-sql/src/session.rs`
  - `OnErrorMode`
  - `parse_on_error_setting(...)`
  - `on_error_mode_name(...)`
  - exhaustive `is_ignorable_on_error(...)`
- `axiomdb-sql/src/executor.rs`
  - mode-aware rollback policy for active transactions
  - `savepoint` support for the first implicit `autocommit=0` DML
  - `ignore` now differentiates ignorable SQL errors from non-ignorable runtime
    failures
- `axiomdb-network/src/mysql/database.rs`
  - applies `on_error` across the full `parse -> analyze -> execute` pipeline
  - converts ignorable errors into warnings + `QueryResult::Empty`
  - eagerly rolls back active txns for non-ignorable failures in `ignore`
- `axiomdb-network/src/mysql/session.rs`
  - stores `on_error` in `ConnectionState`
- `axiomdb-network/src/mysql/handler.rs`
  - `SHOW VARIABLES` now includes `on_error`
  - `COM_RESET_CONNECTION` restores the default mode

## Tests

- Executor integration tests:
  - `crates/axiomdb-sql/tests/integration_executor.rs` — 6 focused `on_error`
    tests for defaults, `SET`, `rollback_statement`, `rollback_transaction`,
    and `savepoint`
- Unit tests:
  - `crates/axiomdb-sql/src/session.rs` — 8 parser/classifier tests
  - `crates/axiomdb-network/src/mysql/database.rs` — 2 pipeline-policy tests
  - `crates/axiomdb-network/src/mysql/handler.rs` — 1 `SHOW VARIABLES` test
  - `crates/axiomdb-network/tests/integration_protocol.rs` — 3 protocol-facing
    session-variable tests
- Wire smoke test:
  - `tools/wire-test.py` now covers default `@@on_error`, `SHOW VARIABLES`,
    `rollback_transaction`, `savepoint`, `ignore`, multi-statement continuation,
    warning propagation, and `COM_RESET_CONNECTION`

## Follow-up subfases still open in Phase 5

- `5.2b` — session-level collation and compat mode
- `5.11b` — `COM_STMT_SEND_LONG_DATA`
- `5.11c` — explicit connection state machine
- `5.15` — DSN parsing
