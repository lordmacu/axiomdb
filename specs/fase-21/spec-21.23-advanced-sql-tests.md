# Spec: 21.23 — Advanced SQL tests

Phase: 21 — Advanced SQL
Task: 21.23 Advanced SQL tests
Status: implemented

## Context

Phase 21 already landed most of the advanced SQL surface that users interact
with directly: CTEs, recursive CTEs, `MERGE`, `RETURNING`, savepoints,
`DECLARE/FETCH/CLOSE` cursors, `CHECKPOINT`, and grouping sets. Each feature
already has targeted tests, but the project still lacks a deliberate
acceptance-style suite that validates those features together through the SQL
surface instead of only in isolated subphase files.

`docs/progreso.md` currently describes `21.23` as covering CTE, window
functions, `MERGE`, savepoints, and cursors. The codebase does not yet
implement SQL window functions (`OVER (...)` remains a future Phase 13/29
item), so this task must explicitly scope them out instead of inventing new
feature work under a test-only subphase.

## Goal

Add a bounded advanced-SQL regression suite that exercises already-implemented
Phase 21 features and their interactions through stable SQL and wire-visible
paths.

## Non-goals

- Implementing SQL window functions or adding placeholder failing tests for
  `OVER (...)`.
- Replacing existing feature-specific tests such as
  `integration_cte.rs`, `integration_merge.rs`, or `integration_cursors.rs`.
- Reworking executor semantics for any covered feature unless the new suite
  finds a real bug.
- Exhaustive planner or optimizer coverage for query hints beyond existing
  feature-specific tests.
- Full ORM or migration compatibility; that remains `21.24`.

## Behavior

### Public API

No new user-facing API. This task adds regression coverage only.

### Test surface

This subphase adds a new acceptance-style SQL integration suite plus a small
wire smoke extension.

Required coverage areas:

1. Non-recursive + recursive CTEs remain usable in advanced contexts.
2. `MERGE` remains correct in a realistic multi-step workflow.
3. Savepoints continue to work around advanced DML, preserving outer
   transaction state after partial rollback.
4. SQL cursors remain transaction-scoped and interoperable with CTE-backed
   queries.
5. `CHECKPOINT` remains callable through the normal SQL path after Phase 21
   feature growth.
6. At least one grouping-sets query remains covered in the shared acceptance
   suite because `21.21` explicitly blocks on `21.23`.

### Semantics

- The new suite must focus on end-to-end behavior, not parser minutiae already
  covered elsewhere.
- Tests should prefer realistic multi-statement flows over isolated one-line
  assertions. Examples:
  - seed base tables
  - use a CTE or recursive CTE to derive rows
  - apply `MERGE`
  - create a savepoint and rollback only the later step
  - declare/fetch/close a cursor inside the same transaction
  - run `CHECKPOINT` in a clean admin-safe context
- The suite must only cover features that already exist in the repo.
- If the new tests expose a real regression, fixing that bug is in scope for
  `21.23`; broad feature expansion is not.
- Wire coverage should remain smoke-level: enough to prove the user-visible
  protocol path for at least one advanced workflow, without duplicating the
  full Rust integration matrix.

### Error cases

| Case | Expected behavior |
|------|-------------------|
| Attempting to cover non-existent window functions in `21.23` | explicitly treated as out of scope; tracked by later phases |
| `CHECKPOINT` executed while a transaction is active | existing rejection semantics remain unchanged and are asserted |
| `FETCH` after `COMMIT`/`CLOSE` in the acceptance flow | existing cursor-missing error remains unchanged and may be asserted where useful |
| Savepoint rollback in advanced workflow | only post-savepoint work is undone; earlier committed-in-txn state survives |

## Edge cases

- [ ] Recursive CTE acceptance path proves the recursive branch still works in
      the consolidated suite.
- [ ] A `MERGE` flow rolled back to a savepoint preserves pre-savepoint rows.
- [ ] Cursor flow uses a non-trivial query (for example a CTE or grouped
      source), not just a base-table scan.
- [ ] `CHECKPOINT` succeeds in autocommit / no-active-transaction context.
- [ ] `CHECKPOINT` rejection while a transaction is active remains covered.
- [ ] Grouping sets query still returns subtotal/grand-total shape in the
      shared suite.
- [ ] Wire smoke covers at least one advanced multi-step workflow rather than
      only isolated one-statement assertions.

## On-disk format

No on-disk format changes.

Compatibility rule: `21.23` is test-only unless regressions force a contained
bug fix in existing code paths.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| New advanced SQL test suite runtime | targeted integration-only | must stay practical for normal `cargo test -p axiomdb-sql` use |
| Wire smoke additions | small incremental cost | should not materially bloat `tools/wire-test.py` runtime |

## Dependencies

- Depends on completed subphases: `21.2`, `21.3`, `21.5`, `21.10`, `21.20`,
  `21.21`
- Depends on existing SQL session-context execution helpers and wire smoke
  harness
- Blocks: Phase 21 closeout confidence on advanced SQL regressions

## Open questions

- [x] Window functions are deferred out of `21.23` because they are not yet
      implemented in this codebase.
- [x] The suite should be additive and acceptance-oriented, not a rewrite of
      each existing feature test file.
- [x] `CHECKPOINT` and grouping sets stay in scope because recent subphases
      explicitly listed `21.23` as their shared follow-up coverage bucket.

## Done criteria

- [ ] New integration suite exists for advanced SQL acceptance scenarios.
- [ ] The suite covers CTE/recursive CTE, `MERGE`, savepoints, cursors,
      `CHECKPOINT`, and grouping sets.
- [ ] Any real regression exposed by the suite is fixed in the same subphase.
- [ ] `docs/progreso.md` is updated to remove the stale window-functions wording
      from `21.23`.
- [ ] `cargo test -p axiomdb-sql --test integration_advanced_sql` passes.
- [ ] `python3 tools/wire-test.py` includes at least one `21.23` smoke and passes.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` passes.

## References

- `db.md`
- `docs/progreso.md`
- `memory/project_state.md`
- `docs/fase-21.md`
- `crates/axiomdb-sql/tests/integration_cte.rs`
- `crates/axiomdb-sql/tests/integration_recursive_cte.rs`
- `crates/axiomdb-sql/tests/integration_merge.rs`
- `crates/axiomdb-sql/tests/integration_cursors.rs`
- `crates/axiomdb-sql/tests/integration_checkpoint.rs`
- `crates/axiomdb-sql/tests/integration_grouping_sets.rs`
