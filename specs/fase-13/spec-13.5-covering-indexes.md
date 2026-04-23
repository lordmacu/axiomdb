# Spec: 13.5 — Covering indexes

Phase: 13 — Advanced PostgreSQL
Task: 13.5 Covering indexes
Status: draft

## Context

AxiomDB already has the first half of covering indexes from `6.13`:

- parser support for `CREATE INDEX ... INCLUDE (...)`
- catalog persistence in `IndexDef.include_columns`
- planner/executor support for `IndexOnlyScan`

But the current `IndexOnlyScan` path only covers **key columns**. Included
columns are not stored in secondary-index leaves, so a query like:

```sql
CREATE INDEX idx_user_cover ON orders(user_id) INCLUDE (total, status);
SELECT total, status FROM orders WHERE user_id = 7;
```

still cannot be satisfied as a real covering scan. `13.5` closes that gap.

## Goal

Deliver real covering indexes for heap tables by storing INCLUDE-column payloads
in secondary-index leaf entries and teaching planner/executor paths to use those
payloads for `IndexOnlyScan`.

## Non-goals

- Not changing clustered-table secondary leaf layout in this subphase.
- Not implementing INCLUDE for primary indexes.
- Not adding new cost-model math beyond the existing `IndexOnlyScan` gate.
- Not making `SELECT *` eligible for covering scans.
- Not changing GIN or expression-index storage layouts.

## Public SQL surface

No new syntax is introduced. The delivered contract is:

```sql
CREATE INDEX idx_name ON t(key_col[, ...]) INCLUDE (cover_col[, ...]);
```

with real runtime effect on eligible heap-table plans.

## Semantics

- `INCLUDE (...)` columns are stored only in secondary-index leaf payloads.
- Included columns do not participate in ordering or uniqueness.
- `IndexOnlyScan` is eligible when every projected plain column needed by the
  query is covered by either:
  - an index key column, or
  - an included column
- Included columns are available for projection after the index scan without
  reading heap row data, except that the existing MVCC slot-visibility check
  still happens.
- `WHERE` recheck can read key columns exactly as today; non-key included
  columns are for projection coverage, not predicate lookup selection.
- INSERT / UPDATE / DELETE must maintain included payload bytes in the index.
- `ALTER TABLE` rewrite/rebuild paths that already rebuild indexes must rebuild
  included payloads too.

## Scope boundary

### In scope

- heap-table secondary indexes
- point lookup and range `IndexOnlyScan`
- DML maintenance for included payloads
- planner coverage detection across key + include columns
- SQL + wire validation

### Out of scope

- clustered-table covering scans
- `ORDER BY` semantics on included columns
- index-only eligibility for arbitrary expressions
- planner support for using included columns to satisfy filter predicates

## Technical contract

- Secondary-index entry format gains an optional INCLUDE payload suffix for leaf
  entries.
- Existing indexes without INCLUDE payload remain readable.
- Catalog format should not need a new logical field; `include_columns` already
  exists. Only index leaf encoding/decoding and maintenance paths should change.

## Edge cases

- [ ] duplicate included column names are rejected or normalized consistently
- [ ] UPDATE of an included column rewrites the secondary index entry
- [ ] UPDATE of a key column rewrites both key and included payload
- [ ] DELETE removes entries with included payload correctly
- [ ] NULL included values round-trip correctly
- [ ] index-only scans still honor MVCC visibility
- [ ] non-covering projections still fall back to heap access
- [ ] legacy indexes without INCLUDE payload continue to scan correctly

## Done criteria

- [ ] `IndexOnlyScan` coverage detection includes `include_columns`
- [ ] heap secondary-index leaf entries store/recover included values
- [ ] DML maintenance keeps included payloads correct
- [ ] executor can project included values without heap row decode
- [ ] legacy non-INCLUDE indexes remain backward-compatible
- [ ] dedicated SQL tests cover point, range, NULL, update, delete, and fallback
- [ ] wire smoke includes a bounded `13.5` scenario
- [ ] docs/memory updated honestly
- [ ] `cargo test -p axiomdb-sql` touched suites pass
- [ ] `python3 tools/wire-test.py` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

## References

- `docs/progreso.md`
- `specs/fase-06/spec-6.13-index-only-scans.md`
- `crates/axiomdb-sql/src/planner_select.rs`
- `crates/axiomdb-sql/src/executor/select_ctx.rs`
- `crates/axiomdb-catalog/src/schema_index.rs`
