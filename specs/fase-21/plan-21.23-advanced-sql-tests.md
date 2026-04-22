# Plan: 21.23 — Advanced SQL tests

Phase: 21 — Advanced SQL
Task: 21.23 Advanced SQL tests
Spec: specs/fase-21/spec-21.23-advanced-sql-tests.md
Status: done

## Summary

Implement `21.23` as a bounded acceptance/regression layer on top of already
completed Phase 21 features. First align the written scope by removing the
stale window-functions promise from progress tracking. Then add a dedicated SQL
integration suite that composes CTEs, recursive CTEs, `MERGE`, savepoints,
cursors, `CHECKPOINT`, and grouping sets in realistic multi-step flows. Finish
with a light wire smoke extension and the usual subphase closeout. If the new
suite exposes a real defect, fix that defect in-place rather than widening the
scope.

## Dependencies

Must be done first:
- [ ] `specs/fase-21/spec-21.23-advanced-sql-tests.md` accepted.
- [ ] Existing per-feature tests for CTE, `MERGE`, cursors, checkpoint, and
      grouping sets stay green as the baseline.

Blocks:
- [ ] Phase 21 acceptance coverage for advanced SQL interactions.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_advanced_sql.rs` — consolidated
  acceptance scenarios across advanced SQL features.
- `specs/fase-21/spec-21.23-advanced-sql-tests.md`
- `specs/fase-21/plan-21.23-advanced-sql-tests.md`

Modified files:
- `tools/wire-test.py` — add one bounded `21.23` protocol smoke.
- `docs/progreso.md` — replace stale `window functions` wording in `21.23`.
- `memory/project_state.md` — track active subphase and closeout once done.
- `docs/fase-21.md`, `memory/architecture.md`, `memory/lessons.md` — closeout.
- Feature source files only if the new suite reveals a real regression.

## Step 1 — Align scope and choose scenarios

**Goal:** make the subphase contract match the actual codebase.
**Files:** `docs/progreso.md`, spec/plan files.
**Approach:** update the wording for `21.23` so it covers implemented features
only, and pin down 2-4 acceptance scenarios before writing tests.

### Scenario candidates

```text
1. Recursive CTE drives a MERGE, then savepoint rollback undoes only the later merge.
2. Cursor declared over a CTE / grouped query fetches windows correctly and dies on COMMIT.
3. CHECKPOINT succeeds outside transactions and rejects inside one.
4. Grouping sets query returns subtotal/grand-total rows in the shared suite.
```

### Verification

```bash
rg -n "21\\.23" docs/progreso.md memory/project_state.md specs/fase-21
```

## Step 2 — Add SQL acceptance suite

**Goal:** codify the advanced SQL workflows in one dedicated integration test file.
**Files:** `crates/axiomdb-sql/tests/integration_advanced_sql.rs`
**Approach:** TDD — add failing end-to-end tests first, then fix any real bug
they expose with the smallest possible patch.

### Tests to add

```rust
#[test]
fn recursive_cte_merge_savepoint_flow_preserves_pre_savepoint_rows() { ... }

#[test]
fn cursor_over_cte_or_grouped_query_fetches_and_closes_on_commit() { ... }

#[test]
fn checkpoint_acceptance_flow_matches_transaction_rules() { ... }

#[test]
fn grouping_sets_acceptance_flow_returns_expected_rollups() { ... }
```

### Implementation outline

- Reuse the existing `tests/common.rs` / session-context helpers.
- Keep each test scenario multi-statement and outcome-oriented.
- If a test reveals a bug:
  - patch the narrowest relevant executor/session/planner file
  - add or refine assertions so the regression stays covered

### Verification

```bash
cargo test -p axiomdb-sql --test integration_advanced_sql
```

## Step 3 — Extend wire smoke minimally

**Goal:** prove at least one advanced workflow over the MySQL wire path.
**Files:** `tools/wire-test.py`
**Approach:** add a small `21.23` block that exercises one multi-step scenario
without duplicating all Rust-side assertions.

### Wire scope

```python
# candidate shape
cur.execute("BEGIN")
cur.execute("DECLARE adv CURSOR FOR WITH ... SELECT ...")
cur.execute("FETCH 1 FROM adv")
cur.execute("CLOSE adv")
cur.execute("COMMIT")
```

Or, if the more valuable path is DML/admin:

```python
cur.execute("BEGIN")
cur.execute("SAVEPOINT sp1")
cur.execute("MERGE INTO ...")
cur.execute("ROLLBACK TO SAVEPOINT sp1")
cur.execute("COMMIT")
```

### Verification

```bash
python3 tools/wire-test.py
```

## Step 4 — Full validation and closeout

**Goal:** run the final gates and close the subphase.
**Files:** closeout docs/memory files.

### Verification

```bash
cargo fmt --check
cargo test -p axiomdb-sql --test integration_advanced_sql
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| The new suite tries to test features that do not exist, especially window functions | high | explicitly scope them out in spec/progreso before coding |
| Acceptance tests become redundant copies of per-feature suites | medium | focus on interactions and multi-step workflows only |
| A new acceptance scenario uncovers a latent executor/session bug | medium | keep fixes narrow and tied to the failing scenario |
| Wire smoke grows too much | low | add one compact workflow instead of mirroring every Rust test |

## Estimated effort

Total: medium

- Step 1: 20-30 min
- Step 2: 60-120 min
- Step 3: 20-40 min
- Step 4: 30-45 min
