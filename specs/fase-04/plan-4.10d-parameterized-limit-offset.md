# Plan: 4.10d — Parameterized LIMIT/OFFSET in Prepared Statements

## Files reviewed before writing this plan

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

## Research citations

- `research/mariadb-server/tests/mysql_client_test.c`
  - inspiration: prepared `LIMIT ?` is a real MySQL-client compatibility path,
    and clients may bind the parameter as either an integer or a string
- `research/mariadb-server/mysql-test/main/ps.test`
  - inspiration: placeholders in LIMIT clauses are expected prepared-statement
    surface area
- `research/mariadb-server/mysql-test/main/ps.result`
  - inspiration: LIMIT placeholders behave like their literal equivalents; this
    also confirms the broader MariaDB coercion surface that we are explicitly
    narrowing in AxiomDB 4.10d
- `research/postgres/src/backend/parser/gram.y`
  - inspiration: `LIMIT` and `OFFSET` are expression slots, not a special AST
    just for parameters
- `research/sqlite/src/select.c`
  - inspiration: LIMIT/OFFSET stay as expressions until execution-time counter
    setup
- `research/duckdb/src/planner/binder/query_node/bind_select_node.cpp`
  - inspiration: coerce row-count expressions into an integral domain and
    reject negatives explicitly

These ideas are adapted to AxiomDB's existing code shape in
`crates/axiomdb-sql/src/parser/dml.rs`,
`crates/axiomdb-sql/src/parser/expr.rs`,
`crates/axiomdb-sql/src/analyzer.rs`,
`crates/axiomdb-sql/src/executor.rs`,
`crates/axiomdb-network/src/mysql/prepared.rs`, and
`crates/axiomdb-network/src/mysql/handler.rs`.

## Files to create/modify

- `crates/axiomdb-sql/src/executor.rs` — replace the current integer-only
  LIMIT/OFFSET helper with an explicit row-count evaluator that accepts exact
  integer text and performs safe `usize` conversion
- `crates/axiomdb-sql/tests/integration_executor.rs` — add executor-level tests
  for exact integer text row counts, negative rejection, non-integral rejection,
  and overflow safety
- `crates/axiomdb-network/src/mysql/prepared.rs` — add unit tests proving
  LIMIT/OFFSET params are substituted in both AST and SQL-string prepared paths
- `crates/axiomdb-network/tests/integration_protocol.rs` — add protocol-level
  packet parsing coverage for a `MYSQL_TYPE_STRING` prepared parameter carrying
  `"1"`
- `tools/wire-test.py` — add live wire assertions for `COM_STMT_PREPARE` +
  `COM_STMT_EXECUTE` with parameterized LIMIT/OFFSET using integer and
  string-bound params

## Algorithm / Data structure

### 1. Keep the parser/analyzer/prepared-statement structure unchanged

No parser grammar changes:

- `parse_limit_offset(...)` already stores LIMIT/OFFSET as `Expr`
- `Token::Question` already becomes `Expr::Param`
- analyzer already passes `Expr::Param` through
- `subst_select(...)` in `prepared.rs` already substitutes `s.limit` and
  `s.offset`

That means 4.10d does **not** add syntax, AST nodes, or handler branches. The
real work is to define the row-count contract in the executor.

### 2. Replace `eval_as_usize(...)` with a dedicated row-count evaluator

Inside `crates/axiomdb-sql/src/executor.rs`, replace the current helper with:

```rust
fn eval_row_count_as_usize(expr: &Expr) -> Result<usize, DbError>
```

Algorithm:

```text
value = eval(expr, &[])?

match value:
  Int(n) if n >= 0 =>
    Ok(n as usize)

  BigInt(n) if n >= 0 =>
    usize::try_from(n)
      .map_err(|_| row_count_type_mismatch("non-negative integer for LIMIT/OFFSET",
                                           "integer too large"))

  Text(s) =>
    trimmed = s.trim()
    parsed = trimmed.parse::<i64>()
      .map_err(|_| row_count_type_mismatch("integer for LIMIT/OFFSET", "Text"))?
    if parsed < 0:
      Err(row_count_type_mismatch("non-negative integer for LIMIT/OFFSET",
                                  "negative integer"))
    else:
      usize::try_from(parsed)
        .map_err(|_| row_count_type_mismatch("non-negative integer for LIMIT/OFFSET",
                                             "integer too large"))

  Int(_) | BigInt(_) =>
    Err(row_count_type_mismatch("non-negative integer for LIMIT/OFFSET",
                                "negative integer"))

  other =>
    Err(row_count_type_mismatch("integer for LIMIT/OFFSET",
                                other.variant_name()))
```

Use a tiny helper:

```rust
fn row_count_type_mismatch(expected: &str, got: &str) -> DbError
```

so all LIMIT/OFFSET errors stay on one error class and one message family.

Decision:

- do **not** call `coerce(...)` here
- reason: `coerce(...)` would pull assignment-coercion semantics into row-count
  evaluation and would change the error class to `InvalidCoercion`
- instead, implement the narrower 4.10d contract directly in the executor

### 3. Keep `apply_limit_offset(...)` as the single enforcement point

`apply_limit_offset(...)` already runs after ORDER BY and is called from all
the relevant SELECT execution paths.

Change only the helper it calls:

```rust
let offset_n = offset.as_ref().map(eval_row_count_as_usize).transpose()?.unwrap_or(0);
let limit_n = limit.as_ref().map(eval_row_count_as_usize).transpose()?;
```

This is deliberate:

- cached prepared AST path and fallback prepared SQL-string path already both
  converge into normal statement execution
- using the shared executor enforcement point keeps those paths identical
- it also intentionally makes `LIMIT '2'` and `OFFSET '1'` legal in plain SQL,
  which is the cleanest way to preserve identical semantics between both
  prepared execution paths

### 4. No handler changes

`crates/axiomdb-network/src/mysql/handler.rs` already does the right thing:

```text
COM_STMT_EXECUTE
  -> parse_execute_packet(body, stmt)
  -> substitute_params_in_ast(cached, &exec.params) on cache hit
  -> execute_stmt(...)

fallback:
  -> substitute_params(&sql_template, &exec.params)
  -> execute_query(...)
```

No new handler branches are needed. The implementation stays centered in the
executor and tests.

### 5. Tests needed to close the semantic gaps

#### SQL executor tests

In `crates/axiomdb-sql/tests/integration_executor.rs`:

- `SELECT a FROM t ORDER BY a LIMIT '2' OFFSET '1'` returns the same rows as
  `LIMIT 2 OFFSET 1`
- `LIMIT '-1'` errors
- `LIMIT '10.1'` errors
- `LIMIT '10:10:10'` errors
- `LIMIT NULL` errors
- `LIMIT` with a positive `BigInt` beyond `usize` range errors without wrap

#### Prepared substitution tests

In `crates/axiomdb-network/src/mysql/prepared.rs`:

- AST substitution test:
  `SELECT * FROM t ORDER BY a LIMIT ? OFFSET ?` ->
  `limit = Literal(Int/BigInt/Text)`, `offset = Literal(...)`
- SQL-string substitution test:
  `SELECT * FROM t ORDER BY a LIMIT ? OFFSET ?` with string params ->
  `"SELECT * FROM t ORDER BY a LIMIT '2' OFFSET '1'"`

This ensures both prepared execution paths are covered explicitly.

#### Protocol packet test

In `crates/axiomdb-network/tests/integration_protocol.rs`:

- build a one-param `COM_STMT_EXECUTE` payload with:
  - `new_params_bound_flag = 1`
  - type code `MYSQL_TYPE_STRING`
  - lenenc payload `"1"`
- assert `parse_execute_packet(...)` decodes the param as `Value::Text("1")`

This proves the wire path can actually deliver the compatibility case from the
MariaDB client test.

#### Live wire smoke test

In `tools/wire-test.py`:

1. Create a small table with deterministic ordered rows.
2. Prepare:

   ```sql
   SELECT a FROM t_param_limit ORDER BY a LIMIT ? OFFSET ?
   ```

3. Execute once with integer params:
   - `LIMIT 2`
   - `OFFSET 1`
   - expect rows `2, 3`
4. Execute once with string-bound LIMIT:
   - `LIMIT "2"` as `MYSQL_TYPE_STRING`
   - `OFFSET 1` as integer
   - expect rows `2, 3`
5. Execute once with invalid string LIMIT:
   - `LIMIT "10.1"` as `MYSQL_TYPE_STRING`
   - expect ERR packet / client error
6. Keep existing prepared-statement binary-result assertions from 5.5a as
   regressions.

## Implementation phases

1. Update the executor row-count helper and its direct tests.
2. Add prepared substitution + protocol unit tests for LIMIT/OFFSET params.
3. Add live wire prepared-statement coverage in `tools/wire-test.py`.

## Tests to write

- unit: row-count helper accepts `Int`, `BigInt`, exact integer `Text`
- unit: row-count helper rejects negative, non-integral text, `NULL`, and
  oversized positive integers
- unit: `substitute_params_in_ast(...)` reaches `limit` and `offset`
- unit: `parse_execute_packet(...)` decodes `MYSQL_TYPE_STRING` row-count params
- integration: executor pagination with exact integer text row counts
- integration: live prepared statement over MySQL wire with integer and
  string-bound row-count params
- bench: none for 4.10d; this subphase is semantic closure over existing paths

## Anti-patterns to avoid

- Do **not** add `$1` placeholders or `LIMIT ?, ?` parser support here.
- Do **not** add a special-case prepared LIMIT executor path in `handler.rs`.
- Do **not** use `as usize` on `BigInt`; it can silently truncate on narrower
  platforms.
- Do **not** pull full assignment coercion or permissive MySQL numeric parsing
  into LIMIT/OFFSET semantics.
- Do **not** leave the cached-AST path and fallback prepared path with
  different row-count rules.

## Risks

- Shared executor coercion broadens plain SQL to accept exact integer text in
  LIMIT/OFFSET.
  - mitigation: make that behavior explicit in the spec and cover it with
    executor tests
- `usize` conversion bugs could become platform-dependent.
  - mitigation: use `usize::try_from(...)` and test the overflow case
- Prepared string-bound LIMIT may work on one path but not the other if tests
  only cover cache hits.
  - mitigation: test both AST substitution and SQL-string substitution
