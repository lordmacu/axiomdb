# Spec: 20.2 — Sequences

Phase: 20 — Types + import/export
Task: 20.2 Sequences
Status: approved

## Context

Phase 20.1 added catalog-backed regular views. Phase 20.2 adds standalone SQL
sequence objects so users can generate numeric IDs without tying generation to
an `AUTO_INCREMENT` table column.

AxiomDB already has per-table `AUTO_INCREMENT` counters, but those are executor
state attached to table inserts. This subphase introduces named sequence
objects with SQL DDL, expression functions, persisted metadata, and per-session
`currval` tracking.

## Goal

Implement `CREATE SEQUENCE`, `DROP SEQUENCE`, `NEXTVAL`, and `CURRVAL` for
named BIGINT sequence objects.

## Non-goals

- Not wiring `SERIAL` / `BIGSERIAL` columns to standalone sequence objects in
  this subphase; existing `AUTO_INCREMENT` behavior remains unchanged.
- Not implementing `ALTER SEQUENCE`, `OWNED BY`, `RESTART`, `SETVAL`, sequence
  privileges, or dependency tracking.
- Not implementing schema search-path complexity beyond the current default
  schema resolution model.
- Not implementing sequence cache preallocation beyond accepting `CACHE 1`.
- Not implementing cycling as the default; `CYCLE` may be parsed only if fully
  executed correctly.

## Public SQL Surface

Accepted DDL:

```sql
CREATE SEQUENCE s;
CREATE SEQUENCE IF NOT EXISTS s;
CREATE SEQUENCE s START WITH 10 INCREMENT BY 5;
CREATE SEQUENCE s MINVALUE 1 MAXVALUE 100 NO CYCLE CACHE 1;
DROP SEQUENCE s;
DROP SEQUENCE IF EXISTS s;
```

Accepted expression functions:

```sql
SELECT NEXTVAL('s');
SELECT CURRVAL('s');
SELECT NEXTVAL('public.s');
```

Function names are case-insensitive. The sequence-name argument must be a text
literal or expression that evaluates to text.

## Semantics

- `CREATE SEQUENCE` creates a named sequence in the current schema.
- Defaults are PostgreSQL-like for ascending BIGINT sequences:
  - `START WITH 1`
  - `INCREMENT BY 1`
  - `MINVALUE 1`
  - `MAXVALUE i64::MAX`
  - `NO CYCLE`
  - `CACHE 1`
- Negative increments are accepted when explicit bounds make the next value
  valid; default descending bounds are out of scope unless implemented fully.
- `NEXTVAL(name)` atomically returns the next sequence value and advances the
  stored value.
- Sequence advancement is not rolled back by transaction rollback. Once a value
  has been returned, gaps are allowed.
- `CURRVAL(name)` returns the last value returned by `NEXTVAL(name)` in the
  current session and errors if this session has not called `NEXTVAL` for that
  sequence.
- `DROP SEQUENCE` removes the sequence metadata. Dropping a missing sequence
  without `IF EXISTS` errors.
- `NEXTVAL` on an exhausted non-cycling sequence errors and must not advance
  state.

## Catalog Metadata

Persist one row per sequence with at least:

- schema name
- sequence name
- last value
- start value
- increment
- min value
- max value
- cycle flag
- cache size
- whether `NEXTVAL` has been called at least once

Catalog serialization must be backward compatible for databases created before
20.2; legacy databases lazily initialize the sequence catalog root.

## Error Cases

| Input | Expected error |
|-------|----------------|
| duplicate `CREATE SEQUENCE s` | object already exists |
| `NEXTVAL('missing')` | sequence not found |
| `CURRVAL('s')` before session `NEXTVAL('s')` | currval is not yet defined in this session |
| `INCREMENT BY 0` | invalid sequence increment |
| `MINVALUE > MAXVALUE` | invalid sequence bounds |
| start outside min/max | invalid sequence start |
| exhausted `NO CYCLE` sequence | sequence reached min/max value |
| `DROP SEQUENCE` on a table/view name | object is not a sequence |

## Edge Cases

- [ ] `NEXTVAL` works in a plain projection without a table.
- [ ] `NEXTVAL` works once per output row when selected from a table.
- [ ] `CURRVAL` is session-local and does not become defined from another
      connection's `NEXTVAL`.
- [ ] Sequence values continue after database reopen.
- [ ] Transaction rollback does not reuse consumed sequence values.
- [ ] Duplicate create and missing drop follow `IF NOT EXISTS` / `IF EXISTS`.
- [ ] Bounds and exhaustion are enforced without off-by-one errors.
- [ ] `NEXTVAL('public.s')` and `NEXTVAL('s')` resolve consistently for the
      default schema.

## Performance Budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `NEXTVAL` single session | 250K ops/s | 100K ops/s |
| `CURRVAL` session lookup | 1M ops/s | 500K ops/s |

`NEXTVAL` is allowed to be slower than `AUTO_INCREMENT` in this subphase because
it is a named, durable SQL object. It must still avoid full-table scans of user
tables.

## Dependencies

- Depends on Phase 20.1 catalog DDL patterns.
- Depends on existing expression function dispatch.
- Blocks future `SERIAL` ownership, `ALTER SEQUENCE`, and identity-column work.

## Done Criteria

- [ ] Parser / AST accept `CREATE SEQUENCE` and `DROP SEQUENCE`.
- [ ] Catalog persists sequence definitions and current state.
- [ ] `NEXTVAL(text)` advances and returns durable BIGINT values.
- [ ] `CURRVAL(text)` returns session-local last values and errors before
      `NEXTVAL`.
- [ ] `DROP SEQUENCE [IF EXISTS]` works and rejects missing objects correctly.
- [ ] Exhaustion and invalid option errors are deterministic.
- [ ] Rollback does not reuse a consumed `NEXTVAL`.
- [ ] Integration coverage exists for DDL, function use, bounds, persistence,
      rollback, and error paths.
- [ ] Wire smoke includes a bounded `[20.2 sequences]` scenario.
- [ ] User docs and internals docs describe the sequence contract.
- [ ] `cargo test -p axiomdb-sql --test integration_sequences` passes.
- [ ] `python3 tools/wire-test.py` passes.
- [ ] `cargo test --workspace` passes at subphase close.
- [ ] `cargo clippy --workspace -- -D warnings` passes at subphase close.

## References

- `docs/fase-20.md`
- `docs/progreso.md` Phase 20.2
- DuckDB `research/duckdb/src/catalog/catalog_entry/sequence_catalog_entry.cpp`
- PostgreSQL local research tree: sequence behavior and `currval` error model
