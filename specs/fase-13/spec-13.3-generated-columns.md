# Spec: 13.3 — Generated columns

Phase: 13 — Advanced PostgreSQL
Task: 13.3 generated columns
Status: closed 2026-04-23

## Context

Phase `13.3` appears in the roadmap as `GENERATED ALWAYS AS ... STORED/VIRTUAL`,
but the repo already shipped a substantial generated-columns slice in `21.5f`.
Today AxiomDB persists generated-column metadata in `axiom_columns`, computes
`STORED` columns on write across the main DML paths, and rejects `VIRTUAL` plus
`ALTER TABLE ... GENERATED` explicitly. This subphase should close the real,
already-delivered contract instead of pretending full parity that the executor
does not have yet.

Related design note: `db.md` lists generated columns as PostgreSQL-inspired
surface and the product example in `docs/progreso.md` is “auto-slug from title”.

## Goal

Close `13.3` around the generated-columns behavior that AxiomDB actually
supports today: catalog-backed `STORED` generated columns created in `CREATE
TABLE`, recomputed on write, documented with explicit non-goals for `VIRTUAL`
and `ALTER TABLE`.

## Non-goals

- Not implementing read-time `VIRTUAL` generated columns in this subphase.
- Not implementing `ALTER TABLE ... ADD/ALTER COLUMN ... GENERATED`.
- Not implementing PostgreSQL identity columns; that remains `24.1c`.
- Not extending generated-column expressions to windows, aggregates, or
  subqueries.
- Not changing the on-disk catalog format beyond what `21.5f` already added.

## Behavior

### Public API

```sql
CREATE TABLE t (
  a INT,
  b INT,
  total INT GENERATED ALWAYS AS (a + b) STORED
);
```

Supported contract for `13.3`:

- `CREATE TABLE ... GENERATED ALWAYS AS (expr) STORED`
- generated metadata persisted in catalog and visible to internal schema paths
- write-time recomputation on `INSERT`, `UPDATE`, `ON CONFLICT`, ODKU, `MERGE`,
  and other existing write paths already covered by `21.5f`
- explicit user assignment rejected except `DEFAULT` in the already-supported
  code paths

Explicitly out of scope but required to fail clearly:

- `... GENERATED ALWAYS AS (...) VIRTUAL`
- `ALTER TABLE ... ADD COLUMN ... GENERATED ...`

### Semantics

- Precondition: the generated expression may reference only base columns from
  the same table.
- Precondition: generated columns may not declare `DEFAULT`,
  `AUTO_INCREMENT`, or `ON UPDATE`.
- Postcondition: persisted row values for `STORED` generated columns equal the
  generated expression evaluated against the final input row for that write.
- Invariant: generated metadata round-trips through `axiom_columns` as
  `generated_expr` + `generated_stored`.
- Invariant: unsupported forms fail explicitly with `DbError::NotImplemented`
  rather than silently degrading.

### Error cases

| Input | Expected error | Message |
|-------|----------------|---------|
| `... GENERATED ALWAYS AS (...) VIRTUAL` | `DbError::NotImplemented` | contains `"virtual generated columns"` |
| `ALTER TABLE ... ADD COLUMN ... GENERATED ...` | `DbError::NotImplemented` | contains `"ALTER TABLE generated columns"` |
| generated column with `DEFAULT` | `DbError::InvalidValue` | contains `"DEFAULT"` |
| generated column with `AUTO_INCREMENT` | `DbError::InvalidValue` | contains `"AUTO_INCREMENT"` |
| generated column with `ON UPDATE` | `DbError::InvalidValue` | contains `"ON UPDATE"` |
| self-reference or generated-to-generated reference | `DbError::InvalidValue` | contains `"self"` or `"another generated column"` |
| unknown referenced column | `DbError::ColumnNotFound` | referenced name present |
| subquery / aggregate / window expression in generated expr | `DbError::NotImplemented` | feature-specific message |

## Edge cases

- [x] Catalog round-trip preserves `generated_expr` and `generated_stored`
- [x] Positional `INSERT` recomputes stored values
- [x] `UPDATE` recomputes stored values after assignments
- [x] conflict-update paths (`ON CONFLICT`, ODKU, `MERGE`) see recomputed values
- [x] explicit writes to generated columns are rejected except `DEFAULT`
- [x] unsupported `VIRTUAL` and `ALTER TABLE` fail clearly
- [x] generated columns can participate in downstream checks/index maintenance

## On-disk format

No new format work is required in `13.3`.

Existing catalog contract from `21.5f`:

- `axiom_columns.flags bit6` => `generated_expr` payload is present
- `axiom_columns.flags bit7` => generated kind marker (`0 = STORED`,
  `1 = VIRTUAL`)

Compatibility rule: legacy rows without bit6 continue to decode as ordinary
base columns.

## Performance budget

No new budget beyond the existing write-path contract.

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| Insert/update on tables with generated STORED cols | no obvious regression vs current `21.5f` baseline | no blocker beyond normal CI scope |

## Dependencies

- Depends on: the implemented `21.5f` generated-columns machinery
- Depends on: `docs/progreso.md`, `docs/fase-13.md`, `memory/project_state.md`
- Blocks: Phase 13 closeout accuracy for generated columns

## Open questions

- [x] `13.3` closes as a status-alignment/acceptance subphase over the
      existing `21.5f` implementation instead of re-implementing `VIRTUAL`.
- [x] A dedicated bounded Phase-13 wire smoke exists even though `21.5f`
      already covered the main happy path.

## Done criteria

- [x] `13.3` scope is aligned with the real repo state: `STORED` yes,
      `VIRTUAL`/`ALTER TABLE` deferred
- [x] Dedicated `13.3` spec and plan exist
- [x] Generated-columns acceptance coverage remains green
- [x] Wire smoke includes an explicit `13.3`-visible check
- [x] `docs/progreso.md`, `docs/fase-13.md`, and `memory/project_state.md`
      reflect the true scope
- [x] `cargo test -p axiomdb-sql --test integration_generated_columns` passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## References

- Phase document: `docs/fase-13.md`
- Progress tracker: `docs/progreso.md`
- Existing implementation notes: `memory/architecture.md` and
  `memory/lessons.md` sections for `21.5f`
- Existing test suite: `crates/axiomdb-sql/tests/integration_generated_columns.rs`
- Existing implementation: `crates/axiomdb-sql/src/executor/ddl_create_table.rs`,
  `crates/axiomdb-sql/src/executor/insert_helpers.rs`
- Design overview: `db.md`
