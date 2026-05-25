# Stored Procedures

AxiomDB supports stored procedures via `CREATE PROCEDURE` / `CALL` / `DROP
PROCEDURE`, in **both** the PostgreSQL (PL/pgSQL) and MySQL dialects, executed by
a **tree-walking interpreter** (Phase 16.7).

> This page documents the *foundations* deliverable (16.7.1 + 16.7.2). Control
> flow (`IF`/`LOOP`/`WHILE`), exception handling (`RAISE`), and cursors are
> tracked as later subphases.

## Syntax

**PL/pgSQL dialect** — dollar-quoted body, `DECLARE` before `BEGIN`:

```sql
CREATE OR REPLACE PROCEDURE raise_salary(IN emp_id INT, IN pct INT)
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE employees SET salary = salary + salary * pct / 100 WHERE id = emp_id;
END
$$;
```

**MySQL dialect** — `BEGIN … END` body, `DECLARE` inside:

```sql
CREATE PROCEDURE total_orders(IN cust INT, OUT n INT)
BEGIN
    DECLARE tmp INT DEFAULT 0;
    SET tmp = (SELECT COUNT(*) FROM orders WHERE customer_id = cust);
    SET n = tmp;
END
```

**Call and drop** (common to both):

```sql
CALL raise_salary(42, 10);
CALL total_orders(7, NULL);     -- returns a one-row result set: { n }
DROP PROCEDURE IF EXISTS raise_salary;
```

## Parameters

| Mode    | Bound from CALL arg | Returned to caller |
|---------|---------------------|--------------------|
| `IN`    | yes (read-only)     | no                 |
| `OUT`   | no (starts `NULL`)  | yes                |
| `INOUT` | yes                 | yes                |

`OUT`/`INOUT` parameters are surfaced as a **one-row result set** whose columns
are the parameter names (PostgreSQL `CALL` semantics). A procedure with no
`OUT`/`INOUT` parameters returns "OK" (empty result). Arguments are positional,
one per parameter (an `OUT` parameter takes an ignored placeholder argument).

## Body language (v1)

A procedure body is a `DECLARE` section followed by an ordered list of statements:

- **`DECLARE name type [DEFAULT expr | := expr]`** — typed local variable.
- **Assignment** — `var := expr` (PL/pgSQL) or `SET var = expr` (MySQL). To read
  a value from a query, use a scalar subquery: `total := (SELECT count(*) FROM t)`.
- **Embedded DML** — `INSERT` / `UPDATE` / `DELETE`, and nested `CALL`.

Not yet supported in bodies (return `NotImplemented`): control flow
(`IF`/`LOOP`/`WHILE`/`FOR`), `RAISE`, cursors, `RETURN`/`RETURNS TABLE`, and
result-set-returning bare `SELECT` (use `SELECT … INTO` / a scalar subquery).

## Execution model

1. **Resolve** the procedure from the catalog (schema-qualified, or via the
   session `search_path`). An unknown `CALL` errors with `ProcedureNotFound`.
   <div class="callout-design">Unlike the earlier placeholder, <code>CALL</code>
   never silently succeeds — a missing procedure is a real error, so an
   application can't lose work to a no-op.</div>
2. **Bind** the variable frame: `IN`/`INOUT` from evaluated arguments, `OUT` to
   `NULL`, then `DECLARE` locals (their `init` expressions evaluated in order).
3. **Re-parse** the stored body text into statements (mirrors triggers/views; no
   compiled-AST cache yet).
4. **Run** each statement in the **caller's transaction**: frame variables are
   substituted (as literals) into the statement's expressions before execution,
   so embedded SQL never resolves a variable name as a column. Assignment
   right-hand sides are evaluated with full subquery support.
5. **Return** the `OUT`/`INOUT` row (or empty).

Because the body runs in the caller's transaction, a `CALL` inside `BEGIN … END`
participates in it — an outer `ROLLBACK` undoes the procedure's effects. Errors
mid-body propagate out of the `CALL`.

### Recursion

A procedure may `CALL` another procedure (or itself). Call nesting is bounded by
`MAX_PROC_CALL_DEPTH` (16); exceeding it errors rather than overflowing the
stack, since each level re-enters the dispatcher + body parser + evaluator.

## Catalog & introspection

Procedures are a **non-table catalog object** (`axiom_procedures`, like holiday
calendars / exchange rates), keyed by `(schema, name)`, persisted with a
length-prefixed binary codec and crash-safe via the WAL. `CREATE OR REPLACE`
upserts; `DROP PROCEDURE [IF EXISTS]` removes.

`information_schema.routines` lists procedures
(`ROUTINE_NAME`, `ROUTINE_SCHEMA`, `ROUTINE_TYPE = 'PROCEDURE'`,
`ROUTINE_DEFINITION`, `EXTERNAL_LANGUAGE`).

## Implementation map

| Concern | Location |
|---|---|
| Lexer (`PROCEDURE`, `$$…$$`) | `crates/axiomdb-sql/src/lexer.rs` |
| AST | `crates/axiomdb-sql/src/ast.rs` (`CreateProcedureStmt`, `ProcBody`, …) |
| Parser (both dialects) | `crates/axiomdb-sql/src/parser/ddl.rs` |
| Body sub-parser | `crates/axiomdb-sql/src/parser/proc_body.rs` |
| Catalog type + codec | `crates/axiomdb-catalog/src/schema_procedure.rs` |
| Catalog persistence | `bootstrap.rs` / `reader.rs` / `writer.rs` |
| DDL execution | `crates/axiomdb-sql/src/executor/ddl_procedure.rs` |
| CALL interpreter | `crates/axiomdb-sql/src/executor/procedure.rs` |
| `information_schema.routines` | `executor/information_schema_exec.rs` |
