# Spec: 13.12 — Statement-level triggers

Phase: 13 — Advanced PostgreSQL
Task: 13.12 Statement-level triggers
Status: implemented

## Context

`13.12` is still open in `docs/progreso.md` and is described as a
statement-level trigger slice aimed at batch validation, especially the
double-entry journal case where a multi-row insert must be validated once after
the whole statement finishes.

The current repo has no SQL trigger support in parser, AST, catalog, or
executor. Full trigger semantics are also explicitly deferred to later work in
Phase 16 (`BEFORE/AFTER`, `WHEN`, `SIGNAL`, `INSTEAD OF`, broader tests). That
means `13.12` should not try to deliver a full PostgreSQL/MySQL trigger
language. The bounded cut needs to be useful for statement-level validation
without depending on stored procedures, row transition variables, or a generic
trigger runtime.

## Goal

Deliver a real SQL statement-trigger MVP that fires exactly once after a DML
statement on a base table and can reject that statement by running a validation
query inside the same transaction.

## Non-goals

- Not implementing row-level triggers.
- Not implementing `BEFORE` triggers.
- Not implementing `WHEN`, `SIGNAL`, `INSTEAD OF`, or trigger procedures.
- Not implementing trigger bodies with multiple statements.
- Not implementing triggers on views, materialized views, or system tables.
- Not implementing `REFERENCING OLD/NEW TABLE` transition relations in this
  subphase.
- Not implementing recursive trigger execution or trigger firing on internal
  maintenance statements.

## Public SQL surface

Supported statements:

```sql
CREATE TRIGGER trg_name
AFTER INSERT|UPDATE|DELETE
ON table_name
FOR EACH STATEMENT
AS SELECT ... ;

DROP TRIGGER trg_name ON table_name;

SHOW CREATE TRIGGER trg_name ON table_name;
```

The trigger body is a single `SELECT` validation query.

## Trigger body contract

The body query is executed once after the DML statement has completed its data
changes but before the outer statement is considered successful.

Validation rule:

- If the body query returns zero rows, the trigger succeeds.
- If the body query returns one or more rows, the engine aborts the outer DML
  statement with a trigger validation error.

The error message should include the trigger name and a bounded summary of the
first returned row when available.

This keeps the body declarative and useful for integrity checks without needing
`SIGNAL` or stored-procedure control flow.

Recommended validation shape:

- Prefer `SELECT ... FROM ... GROUP BY/HAVING ...` or another explicit
  `FROM`-based validation query.
- Avoid relying on `SELECT literal WHERE ...` without `FROM` in this MVP.

Example:

```sql
CREATE TRIGGER journal_balanced
AFTER INSERT ON journal
FOR EACH STATEMENT
AS
SELECT 'journal not balanced'
WHERE (
  SELECT COALESCE(SUM(debit), 0) FROM journal
) <> (
  SELECT COALESCE(SUM(credit), 0) FROM journal
);
```

## Semantics

- Only `AFTER INSERT`, `AFTER UPDATE`, and `AFTER DELETE` are supported.
- `FOR EACH STATEMENT` is mandatory.
- Triggers fire once per successful top-level DML statement affecting the target
  table, not once per row.
- Trigger execution happens inside the same transaction and sees the effects of
  the just-finished DML statement.
- If trigger validation fails, the whole outer statement is rolled back under
  the existing statement-rollback machinery.
- In autocommit mode, a failing trigger aborts the implicit transaction and no
  changes become visible.
- In explicit transactions, a failing trigger aborts the statement and leaves
  the transaction open, matching current statement-level rollback behavior.
- Multiple triggers on the same `(table, event)` fire in creation order.
- If an earlier trigger fails, later triggers for that event are not executed.
- Trigger bodies are read-only in this MVP: only `SELECT` is allowed.
- Trigger bodies do not recursively fire other triggers because they cannot
  perform DML.

## Statement metadata

This MVP exposes only aggregated statement metadata, not row transition tables.

Built-in session variables available during trigger-body execution:

- `@@trigger_name`
- `@@trigger_table`
- `@@trigger_event`
- `@@trigger_row_count`

`@@trigger_row_count` is the affected-row count from the outer DML statement.

## Catalog/runtime format

Add trigger metadata owned by the table:

- trigger name
- target event (`INSERT` / `UPDATE` / `DELETE`)
- timing (`AFTER`)
- granularity (`STATEMENT`)
- original body SQL
- creation order ordinal

The body SQL is reparsed/analyzed at execution time in trigger context rather
than compiled to a stored procedure object.

## Error cases

| Input | Expected error |
|-------|----------------|
| `CREATE TRIGGER ... BEFORE ...` | `DbError::NotImplemented` |
| `CREATE TRIGGER ... FOR EACH ROW` | `DbError::NotImplemented` |
| trigger body not a `SELECT` | `DbError::InvalidArgument` |
| duplicate trigger name on same table | `DbError::AlreadyExists` |
| `DROP TRIGGER` for missing trigger | `DbError::TriggerNotFound` |
| trigger body returning rows | `DbError::TriggerValidationFailed` |

## Edge cases

- [x] multi-row `INSERT ... VALUES (...), (...)` fires once
- [ ] `INSERT ... SELECT ...` fires once
- [ ] `UPDATE` touching zero rows still fires once if the statement succeeds
- [ ] `DELETE` touching zero rows still fires once if the statement succeeds
- [x] trigger failure rolls back only the outer statement, not the whole
      explicit transaction
- [ ] savepoint-wrapped statements preserve current statement rollback behavior
- [x] dropped trigger no longer fires
- [x] multiple triggers on one event respect creation order

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| trigger dispatch lookup | O(k) triggers on target event | acceptable for MVP |
| single validation trigger after DML | one extra analyzed `SELECT` | acceptable for MVP |

## Dependencies

- Depends on: parser/AST support for `CREATE TRIGGER` and `DROP TRIGGER`
- Depends on: table-owned trigger metadata in catalog
- Depends on: statement-level DML hooks in executor
- Blocks: honest closeout of `13.12` in `docs/fase-13.md`

## Open questions

- [x] Keep `SHOW CREATE TRIGGER` in-scope.
- [ ] Decide whether zero-row `UPDATE` / `DELETE` should fire in the MVP. The
      recommended behavior is yes, because statement-level semantics are about
      statement execution, not row count.

## Done criteria

- [x] `CREATE TRIGGER ... AFTER ... FOR EACH STATEMENT AS SELECT ...` parses
- [x] trigger metadata persists in catalog and round-trips through restart
- [x] `DROP TRIGGER ... ON table` works
- [x] statement-level `AFTER INSERT/UPDATE/DELETE` dispatch is wired
- [x] validation query can abort the outer statement
- [x] trigger body can read `@@trigger_row_count`
- [x] dedicated SQL integration coverage exists
- [x] wire smoke includes a bounded `13.12` scenario
- [x] `docs/progreso.md`, `docs/fase-13.md`, and `memory/project_state.md`
      reflect the delivered MVP honestly
- [x] `cargo test -p axiomdb-sql` for touched tests passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## References

- Progress tracker: `docs/progreso.md`
- Session / statement rollback machinery: `crates/axiomdb-sql/src/executor/exec_with_ctx.rs`
- Existing session variable support: `crates/axiomdb-sql/src/session.rs`
- DML execution paths: `crates/axiomdb-sql/src/executor/`
