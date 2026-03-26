# Spec: 5.2c — ON_ERROR session behavior

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

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
- `crates/axiomdb-wal/src/txn.rs`
- `tools/wire-test.py`
- `specs/fase-05/spec-5.2a-charset-collation-negotiation.md`
- `specs/fase-05/spec-5.3b-5.8-5.9.md`

These research files were reviewed before writing this spec:

- `research/mariadb-server/sql/handler.cc`
- `research/sqlite/src/btree.c`
- `research/postgres/src/backend/utils/errcodes.txt`
- `research/oceanbase/src/sql/session/ob_sql_session_info.cpp`

## What to build (not how)

AxiomDB already has two pieces of the problem:

- Phase `3.5c` already gives statement-level rollback inside an active
  transaction by using `TxnManager::savepoint()` and
  `rollback_to_savepoint(...)`.
- Phase `5.9b` already gives `SHOW WARNINGS` plus `warning_count` in OK packets.

What is still missing is a session-visible policy that lets the user choose how
statement errors affect the transaction and whether certain SQL errors become
warnings instead of ERR packets.

`5.2c` adds one session variable:

```sql
SET on_error = 'rollback_statement' | 'rollback_transaction' | 'savepoint' | 'ignore'
```

The variable is per-connection, survives across statements, is reset by
`COM_RESET_CONNECTION`, and is visible through:

- `SELECT @@on_error`
- `SELECT @@session.on_error`
- `SHOW VARIABLES LIKE 'on_error'`

### Default

The default is:

```sql
on_error = 'rollback_statement'
```

This matches AxiomDB's current explicit-transaction behavior and avoids a
breaking change for existing clients.

### Exact mode semantics

#### 1. `rollback_statement`

This is the default AxiomDB mode.

- If a transaction is already active, the failing statement is rolled back to
  the pre-statement boundary and the transaction stays open.
- If no transaction is active and `autocommit = 1`, the current statement's
  implicit single-statement transaction is rolled back and the error is
  returned.
- If no transaction is active and `autocommit = 0`, the first failing DML
  statement rolls back the just-opened implicit transaction and closes it.

Important distinction:

- `rollback_statement` preserves an already-active transaction.
- It does **not** keep open a brand-new implicit transaction that failed on its
  first DML statement.

#### 2. `rollback_transaction`

This is the fail-fast whole-transaction mode.

- If an error happens while a transaction is active, the whole active
  transaction is rolled back and closed.
- The original statement error is still returned to the client.
- There is **no** PostgreSQL-style "failed transaction latch" in `5.2c`.
  AxiomDB adapts the PostgreSQL idea to eager rollback:
  after the error, `@@in_transaction = 0` and the next statement starts fresh.

This eager rollback is deliberate. It fits the current AxiomDB architecture
without introducing a second "transaction aborted but still open" state machine
into the MySQL wire path.

#### 3. `savepoint`

This mode extends the existing statement-rollback behavior to the
`autocommit = 0` implicit-transaction path.

- If a transaction is already active, behavior is the same as
  `rollback_statement`: rollback only the failing statement and keep the
  transaction open.
- If `autocommit = 0` and a DML statement starts the implicit transaction,
  AxiomDB must create a statement boundary before executing that first DML.
  If the statement fails, only that statement is undone and the implicit
  transaction remains open.

This is the key difference from `rollback_statement`.

Example:

```sql
SET autocommit = 0;
SET on_error = 'savepoint';
INSERT INTO t VALUES (1);   -- fails
SELECT @@in_transaction;    -- must return 1
INSERT INTO t VALUES (2);   -- succeeds in the same open txn
COMMIT;
```

`savepoint` does **not** change MySQL's read-only rule for `SELECT` in
`autocommit = 0`: plain `SELECT` still does not open a lasting implicit
transaction when none existed before.

#### 4. `ignore`

This is the lenient session mode.

- For **ignorable SQL statement errors**, AxiomDB:
  - applies statement rollback semantics equivalent to `savepoint`
  - converts the original error into a session warning
  - returns success instead of ERR
  - serializes the ignored statement as an OK packet with `warning_count > 0`
- For **non-ignorable infrastructure/runtime errors**, AxiomDB must still
  return ERR and must not silently continue.

In `ignore` mode, the warning added to the session must preserve the original
MySQL-visible code and message derived from the underlying `DbError`.

Ignored statements do **not** fabricate an empty result set. They serialize as:

- `QueryResult::Empty`
- `affected_rows = 0`
- `warning_count >= 1`

This applies both to:

- single-statement `COM_QUERY`
- multi-statement `COM_QUERY` split by `split_sql_statements(...)`

For multi-statement `COM_QUERY`, later statements continue executing after an
ignored error.

Warnings remain statement-scoped exactly as they are today:

- the ignored statement's OK packet carries `warning_count > 0`
- a later statement in the same multi-statement packet may clear
  `session.warnings` before the client runs `SHOW WARNINGS`

So in `5.2c`, `SHOW WARNINGS` after a multi-statement packet still reflects the
most recent statement, not an accumulated list for the whole packet.

### Ignorable vs non-ignorable errors

`ignore` only applies to SQL/user-facing statement errors.

Ignorable classes in `5.2c`:

- parse errors
- semantic errors (`table not found`, `column not found`, ambiguity, etc.)
- constraint/integrity errors reachable from SQL (`unique`, `foreign key`,
  `not null`, `check`)
- coercion/value/expression errors (`invalid coercion`, `division by zero`,
  `overflow`, `invalid value`, `type mismatch`)
- user-visible transaction errors (`no active transaction`,
  `transaction already active`)
- SQL feature gaps (`not implemented`)
- DDL semantic errors (`table already exists`, `column already exists`,
  `index already exists`, `index key too long`)

Non-ignorable classes in `5.2c`:

- storage/runtime corruption or invariants
- WAL/runtime durability failures
- `DiskFull`
- generic `Io`
- internal errors

Those errors must still surface as ERR even if `on_error = 'ignore'`.

### Pipeline coverage

`on_error` applies to the whole statement pipeline, not only executor writes.

That means the chosen mode affects errors raised during:

- `parse(...)`
- `analyze(...)` / `analyze_cached(...)`
- `execute_with_ctx(...)`

This matters because syntax and semantic errors happen before the executor, but
the session policy still needs to decide whether the active transaction stays
open, is eagerly rolled back, or is converted into a warning.

### Session reset semantics

`COM_RESET_CONNECTION` must restore:

- `on_error = 'rollback_statement'`
- `autocommit = 1`
- existing strict/charset/session defaults already defined by earlier subphases

## Research synthesis

### AxiomDB-first constraints

- `crates/axiomdb-sql/src/executor.rs` already implements statement-level
  rollback with `Savepoint`, but only as fixed behavior, not as a session mode.
- `crates/axiomdb-network/src/mysql/database.rs` owns the full
  `parse -> analyze -> execute_with_ctx` pipeline and is therefore the only
  place that can apply `on_error` consistently to parse/analyze failures.
- `crates/axiomdb-network/src/mysql/handler.rs` intercepts `SET` before the
  engine, so session state has to exist both in `ConnectionState` and
  `SessionContext`.
- `crates/axiomdb-sql/src/session.rs` already has the warning buffer used by
  `SHOW WARNINGS`, so `ignore` should reuse that system rather than invent a
  second warning channel.

### What we borrow

- MariaDB statement transaction model:
  - `research/mariadb-server/sql/handler.cc`
  - borrowed idea: one statement can be atomic inside a larger transaction
    without forcing a full transaction rollback
- SQLite statement sub-transaction technique:
  - `research/sqlite/src/btree.c`
  - borrowed idea: statement rollback is effectively an anonymous savepoint
- PostgreSQL whole-transaction failure as the contrasting behavior:
  - `research/postgres/src/backend/utils/errcodes.txt`
  - borrowed idea: some users want a fail-fast mode where one error loses the
    whole transaction
- OceanBase session ownership of implicit savepoint state:
  - `research/oceanbase/src/sql/session/ob_sql_session_info.cpp`
  - borrowed idea: implicit savepoint behavior belongs to session state, not to
    a one-off local variable in the handler

### What we reject

- Copying PostgreSQL's exact `25P02` failed-transaction latch into `5.2c`
- inventing a handler-only solution while transaction semantics still live in
  the executor
- inventing a brand-new warning transport when `SHOW WARNINGS` already exists

### How AxiomDB adapts it

- MariaDB/SQLite provide the savepoint model
- PostgreSQL provides the fail-fast whole-transaction inspiration
- AxiomDB keeps its current lightweight transaction model and chooses eager
  rollback for `rollback_transaction` instead of a persistent aborted-txn state
- `ignore` is implemented as "warning + success + continue" for SQL errors,
  not as a copy of MySQL `INSERT IGNORE`

## Inputs / Outputs

- Input:
  - `SET on_error = ...` through the intercepted MySQL wire path
  - `SET on_error = ...` through the executor `SET` path used by embedded/tests
  - statements that may fail in parse, analyze, or execute stages
  - current transaction state (`txn.active_txn_id()`) plus session
    `autocommit`
- Output:
  - session-visible `on_error` mode
  - transaction behavior selected by that mode
  - warnings surfaced through `warning_count` and `SHOW WARNINGS` when
    `on_error = 'ignore'`
  - inspectable session variable value via `@@on_error` / `SHOW VARIABLES`
- Errors:
  - invalid `SET on_error` value returns ERR and leaves the previous mode
    unchanged
  - non-ignorable runtime/infrastructure errors still return ERR even in
    `ignore`

## Use cases

1. Default behavior:
   `BEGIN; INSERT ok; INSERT duplicate; INSERT ok2; COMMIT` with default
   `rollback_statement` commits `ok` and `ok2`, but not the duplicate row.
2. Whole-transaction fail-fast:
   `SET on_error='rollback_transaction'; BEGIN; INSERT ok; INSERT duplicate`
   rolls back the whole transaction and leaves `@@in_transaction = 0`.
3. Implicit transaction survives first error:
   `SET autocommit=0; SET on_error='savepoint'; INSERT bad; SELECT @@in_transaction`
   returns `1`.
4. Embedded path parity:
   `execute_with_ctx(Stmt::Set(... on_error ...))` updates `SessionContext`
   without going through the MySQL handler.
5. Lenient mode warning:
   `SET on_error='ignore'; INSERT duplicate` returns success and
   `SHOW WARNINGS` exposes the original duplicate-key code/message.
6. Multi-statement leniency:
   `SET on_error='ignore'; BEGIN; INSERT ok; INSERT duplicate; INSERT ok2; COMMIT`
   executes all later statements and only the duplicate becomes a warning.
7. Non-ignorable safety:
   `SET on_error='ignore'` does not hide `DiskFull` or WAL/runtime failures.
8. Reset behavior:
   after `COM_RESET_CONNECTION`, `SELECT @@on_error` returns
   `rollback_statement`.

## Acceptance criteria

- [ ] `SessionContext` has a typed `on_error` mode with default
      `rollback_statement`
- [ ] `ConnectionState` stores and exposes the same `on_error` mode
- [ ] `SET on_error = 'rollback_statement'|'rollback_transaction'|'savepoint'|'ignore'`
      works through the intercepted wire path
- [ ] `SET on_error = ...` also works through `execute_set_ctx(...)` so the
      embedded/test path is not left behind
- [ ] invalid `SET on_error = ...` values return ERR and preserve the previous
      session value
- [ ] `SELECT @@on_error` and `SELECT @@session.on_error` return the current
      canonical lowercase mode name
- [ ] `SHOW VARIABLES LIKE 'on_error'` returns exactly the live session mode
- [ ] in default `rollback_statement`, an error inside an already-active
      transaction rolls back only that statement and the transaction stays open
- [ ] in `rollback_transaction`, an error inside an active transaction rolls
      back the whole transaction and `@@in_transaction` becomes `0`
- [ ] `rollback_transaction` in `5.2c` uses eager rollback, not a PostgreSQL
      `25P02` failed-transaction latch
- [ ] `savepoint` is observably different from `rollback_statement`:
      with `autocommit = 0`, the first failing DML leaves the implicit
      transaction open
- [ ] `savepoint` does not change the existing rule that plain `SELECT` in
      `autocommit = 0` does not open a lasting implicit transaction when none
      existed before
- [ ] `ignore` converts ignorable SQL errors into warnings and serializes the
      statement as success instead of ERR
- [ ] ignored statements serialize as OK/Empty with warnings, not as synthetic
      empty result sets
- [ ] in multi-statement `COM_QUERY`, `ignore` continues executing later
      statements after an ignorable error
- [ ] `SHOW WARNINGS` after an ignored statement contains the original MySQL
      code and message derived from the swallowed `DbError`
- [ ] in multi-statement `COM_QUERY`, the ignored statement's own OK packet
      carries the warning, but `SHOW WARNINGS` after later statements still
      follows the existing "last statement only" rule
- [ ] non-ignorable runtime/infrastructure errors still return ERR even in
      `ignore`
- [ ] `COM_RESET_CONNECTION` restores `on_error` to `rollback_statement`

## Out of scope

- SQL syntax for named savepoints: `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`
  (Phase `7.12`)
- PostgreSQL-style persistent failed-transaction state with `25P02`
- statement-level `INSERT IGNORE`, `UPDATE IGNORE`, or other per-statement SQL
  syntax
- server-global or database-global `on_error` defaults
- collation/compat-mode behavior from `5.2b`

## Dependencies

- `Phase 3.5c` savepoint-based statement rollback already implemented in
  `crates/axiomdb-sql/src/executor.rs` and `crates/axiomdb-wal/src/txn.rs`
- `Phase 5.9b` warning storage and `SHOW WARNINGS`
- `Phase 5.9` session variable storage and `@@variable` interception
