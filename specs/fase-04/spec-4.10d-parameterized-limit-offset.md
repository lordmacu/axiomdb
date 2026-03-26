# Spec: 4.10d — Parameterized LIMIT/OFFSET in Prepared Statements

## These files were reviewed before writing this spec

- `db.md`
- `docs/progreso.md`
- `specs/fase-04/spec-4.10.md`
- `specs/fase-05/spec-5.10-prepared-statements.md`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/parser/expr.rs`
- `crates/axiomdb-sql/src/analyzer.rs`
- `crates/axiomdb-sql/src/eval.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `crates/axiomdb-types/src/value.rs`
- `tools/wire-test.py`
- `research/mariadb-server/tests/mysql_client_test.c`
- `research/mariadb-server/mysql-test/main/ps.test`
- `research/mariadb-server/mysql-test/main/ps.result`
- `research/postgres/src/backend/parser/gram.y`
- `research/sqlite/src/select.c`
- `research/duckdb/src/planner/binder/query_node/bind_select_node.cpp`

## Research synthesis

### What we borrow

- MariaDB from `research/mariadb-server/tests/mysql_client_test.c`:
  `LIMIT ?` in binary prepared statements is a real compatibility surface, and
  clients may bind that placeholder as either an integer type or a string type.
- MariaDB from `research/mariadb-server/mysql-test/main/ps.test` and
  `research/mariadb-server/mysql-test/main/ps.result`: placeholders inside the
  LIMIT clause are valid prepared-statement syntax and should behave like the
  same query with a concrete row count.
- PostgreSQL from `research/postgres/src/backend/parser/gram.y`:
  `LIMIT` and `OFFSET` are ordinary expression slots, not a separate AST family
  just for parameterized row counts.
- SQLite from `research/sqlite/src/select.c`: LIMIT/OFFSET remain expressions
  through parsing and are turned into runtime counters during execution, which
  fits AxiomDB's existing `Expr`-based `limit` / `offset` fields.
- DuckDB from
  `research/duckdb/src/planner/binder/query_node/bind_select_node.cpp`:
  row-count expressions should be coerced into an integral counter domain and
  negative values must be rejected explicitly.

### What we reject

- Adding PostgreSQL-style `$1` placeholders. AxiomDB's MySQL prepared-statement
  protocol uses `?`, and 4.10d does not broaden the parser beyond that.
- Adding MySQL's comma syntax `LIMIT ?, ?` in this subphase. AxiomDB 4.10 only
  supports `LIMIT expr [OFFSET expr]`, and 4.10d stays inside that syntax.
- Creating a separate planner or executor operator just for parameterized LIMIT.
- Importing MariaDB's broader coercions for REAL / DECIMAL / TIME row counts in
  4.10d. This subphase only needs exact integral row counts.

### How AxiomDB adapts it

AxiomDB already stores `SelectStmt.limit` and `SelectStmt.offset` as `Expr`,
already parses `?` as `Expr::Param`, and already substitutes prepared params in
those fields in `crates/axiomdb-network/src/mysql/prepared.rs`. 4.10d therefore
does not add syntax or AST nodes. It closes the remaining gap by defining the
row-count coercion contract at execution time:

- accept evaluated row counts of `Int`, `BigInt`, or exact integer `Text`
- reject negatives
- reject non-integral or non-scalar values
- convert to `usize` without silent truncation

This keeps the existing cached-AST prepared path and the existing fallback
string-substitution path on one consistent executor rule.

## What to build (not how)

Support prepared-statement parameters inside AxiomDB's existing
`LIMIT expr [OFFSET expr]` syntax.

This subphase applies to MySQL server-side prepared statements executed through
`COM_STMT_PREPARE` + `COM_STMT_EXECUTE`.

No new parser syntax is added. The query shape remains:

- `... LIMIT ?`
- `... LIMIT ? OFFSET ?`
- `... LIMIT (? + 1)` or other scalar expressions that already parse today

The final evaluated LIMIT/OFFSET value must be treated as a row count with this
contract:

- `Int(n)` where `n >= 0` -> valid
- `BigInt(n)` where `n >= 0` -> valid
- `Text(s)` where `s.trim()` is an exact base-10 integer and the parsed value
  is `>= 0` -> valid

Everything else is invalid as a row count:

- negative integers
- text like `10.1`, `1e3`, `10:10:10`, `abc`
- `NULL`
- `Bool`, `Real`, `Decimal`, `Date`, `Timestamp`, `Bytes`, `Uuid`

To keep the prepared cache-hit path and the prepared fallback path consistent,
this row-count contract is defined at the executor layer. As a result, exact
integer text values are accepted for LIMIT/OFFSET regardless of whether they
came from a prepared parameter or a quoted literal.

## Inputs / Outputs

### Inputs

- `SelectStmt.limit: Option<Expr>`
- `SelectStmt.offset: Option<Expr>`
- prepared statements whose analyzed AST contains `Expr::Param` inside one or
  both of those fields
- `COM_STMT_EXECUTE` parameter values decoded by
  `crates/axiomdb-network/src/mysql/prepared.rs`

### Outputs

- Same `QueryResult::Rows` shape and ordering semantics as 4.10
- Same pagination semantics as literal LIMIT/OFFSET from 4.10
- No protocol shape change for prepared statements; only the row window changes

### Errors

- Invalid row-count type or format -> `DbError::TypeMismatch`
- Negative row count -> `DbError::TypeMismatch`
- Positive row count too large for the current platform `usize` ->
  `DbError::TypeMismatch`
- Existing prepared-statement packet/parameter-count errors remain unchanged
- Existing parser errors for unsupported syntax remain unchanged

## Use cases

1. Prepared integer LIMIT + OFFSET

   `SELECT a FROM t ORDER BY a LIMIT ? OFFSET ?` with bound values `2, 1`
   returns the same rows as `LIMIT 2 OFFSET 1`.

2. Prepared string-bound LIMIT works

   A MySQL client that binds `LIMIT ?` as `MYSQL_TYPE_STRING` with value `"1"`
   still gets one row, matching MariaDB client behavior.

3. Prepared string-bound OFFSET works

   `LIMIT 2 OFFSET ?` with bound string `"3"` skips the first three rows and
   returns the next two.

4. Cached analyzed prepared statements need no new parse rules

   `COM_STMT_EXECUTE` on a cached analyzed statement works because
   `substitute_params_in_ast(...)` already substitutes LIMIT/OFFSET fields.

5. Prepared fallback path stays consistent

   If the prepared execution path falls back to SQL-string substitution, the
   resulting SQL still uses the same executor-side row-count rules.

6. Invalid prepared row counts error

   `LIMIT ?` bound to `-1`, `"10.5"`, `"abc"`, or `NULL` errors instead of
   truncating or silently treating the value as zero.

## Acceptance criteria

- [ ] No new placeholder syntax is added; `?` remains the only prepared
      placeholder syntax in 4.10d.
- [ ] `LIMIT ?` in a prepared statement executes successfully when the bound
      value is a non-negative integer.
- [ ] `LIMIT ? OFFSET ?` in a prepared statement executes successfully when the
      bound values are non-negative integers.
- [ ] `LIMIT ?` in a prepared statement executes successfully when the bound
      value is a string containing an exact non-negative decimal integer.
- [ ] `OFFSET ?` in a prepared statement executes successfully when the bound
      value is a string containing an exact non-negative decimal integer.
- [ ] Leading and trailing ASCII whitespace in an exact integer text row count
      is ignored.
- [ ] Negative integers are rejected for LIMIT/OFFSET.
- [ ] Non-integral text values such as `"10.1"` and `"1e3"` are rejected for
      LIMIT/OFFSET.
- [ ] Temporal-looking text such as `"10:10:10"` is rejected for LIMIT/OFFSET
      in 4.10d.
- [ ] `NULL` is rejected for LIMIT/OFFSET in 4.10d.
- [ ] Positive `BigInt` values that do not fit in `usize` are rejected instead
      of being silently truncated or wrapped.
- [ ] The same row-count coercion rules are used by the cached-AST prepared
      path and the fallback SQL-string prepared path.
- [ ] `LIMIT ?, ?` remains unsupported syntax in AxiomDB 4.10d.
- [ ] PostgreSQL-style `$1` / `$2` placeholders are not added by 4.10d.
- [ ] ORDER BY + LIMIT/OFFSET pagination semantics from 4.10 remain unchanged.
- [ ] This subphase does not add implicit arithmetic coercion inside LIMIT or
      OFFSET expressions beyond what expression evaluation already supports.

## ⚠️ DEFERRED

- MySQL comma syntax `LIMIT ?, ?` remains deferred.
- Broader MariaDB/MySQL row-count coercions from REAL / DECIMAL / TIME remain
  deferred.
- `LIMIT ALL` / PostgreSQL FETCH syntax remain outside 4.10d.
- Named placeholders or PostgreSQL `$n` placeholders remain outside 4.10d.

## Out of scope

- New parser grammar for LIMIT/OFFSET
- New AST node types for row counts
- A separate LIMIT executor just for prepared statements
- Changing ORDER BY, GROUP BY, or projection semantics from 4.10
- Adding cursor support or other prepared-statement protocol changes

## Dependencies

- `db.md`
- `specs/fase-04/spec-4.10.md`
- `specs/fase-05/spec-5.10-prepared-statements.md`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/parser/expr.rs`
- `crates/axiomdb-sql/src/analyzer.rs`
- `crates/axiomdb-sql/src/eval.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
