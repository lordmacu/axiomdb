# Plan: 21.5 — MERGE / UPSERT

Phase: 21 — Advanced SQL
Task: 21.5 MERGE / UPSERT
Spec: specs/fase-21/spec-21.5-merge-upsert.md
Status: completed (2026-04-19)

## Summary

Implement PostgreSQL `INSERT ... ON CONFLICT` first, then SQL-standard
`MERGE` on top of the same heap-table write primitives. The order keeps the
lowest-level conflict lookup/update semantics isolated and gives MERGE a
small set of tested helpers to call instead of duplicating constraint and
index maintenance logic.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.5-merge-upsert.md` approved.
- [x] Phase 21.4b `RETURNING` helpers available in `executor/returning.rs`.
- [x] MySQL ODKU heap helper exists in `executor/odku_helpers.rs`.

Blocks:
- [x] Closing Phase 21.5.
- [x] ORM-style PostgreSQL UPSERT parity.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_on_conflict.rs` — PostgreSQL UPSERT coverage.
- `crates/axiomdb-sql/tests/integration_merge.rs` — MERGE coverage.
- `crates/axiomdb-sql/src/executor/on_conflict_helpers.rs` — conflict lookup and DO UPDATE / DO NOTHING helpers shared by INSERT and MERGE where useful.
- `crates/axiomdb-sql/src/executor/merge.rs` — MERGE heap executor.

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — add `MERGE` token only; parse `CONFLICT`, `NOTHING`, and `MATCHED` as identifiers to avoid keyword churn.
- `crates/axiomdb-sql/src/ast.rs` — add `OnConflictClause`, `OnConflictAction`, `MergeStmt`, `MergeAction`, `MergeActionKind`, `Stmt::Merge`, and `Expr::ExcludedValue`.
- `crates/axiomdb-sql/src/parser/dml.rs` — parse `ON CONFLICT` tails and `MERGE INTO`.
- `crates/axiomdb-sql/src/parser/mod.rs` — route `Token::Merge` to DML parser and recognize `EXCLUDED.col` as a qualified column form before analyzer resolution.
- `crates/axiomdb-sql/src/analyzer_ddl.rs` — resolve ON CONFLICT targets, EXCLUDED expressions, and MERGE source/target scopes.
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — route `Stmt::Merge`.
- `crates/axiomdb-sql/src/plan_deps.rs` — visit new AST nodes for plan cache dependencies.
- `crates/axiomdb-sql/src/executor/mod.rs` — include new helper/executor files.
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — route `Stmt::Merge` and treat it as a write barrier.
- `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` — integrate ON CONFLICT in the heap INSERT loops and remove the 21.4b RETURNING rejection for conflict-resolution paths.
- `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` — reject `ON CONFLICT` on clustered targets with a clear NotImplemented.
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — support EXPLAIN over MERGE and new Insert fields.
- `tools/wire-test.py` — add one DO NOTHING, one DO UPDATE RETURNING, and one MERGE smoke assertion.

## Step 1 — AST and parser for ON CONFLICT

Status: completed.

**Goal:** parse PostgreSQL UPSERT syntax without executor behavior.
**Files:** `ast.rs`, `lexer.rs`, `parser/dml.rs`, `parser/mod.rs`, `tests/integration_on_conflict.rs`.
**Approach:** TDD parser tests first.

### Tests to add

```rust
#[test]
fn parses_on_conflict_do_nothing_forms() {
    parse("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING", None).unwrap();
    parse("INSERT INTO t VALUES (1) ON CONFLICT (id) DO NOTHING", None).unwrap();
}

#[test]
fn parses_on_conflict_do_update_with_excluded() {
    let stmt = parse(
        "INSERT INTO t VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
        None,
    ).unwrap();
    assert!(matches!(stmt, Stmt::Insert(_)));
}
```

### Implementation outline

- Add `InsertStmt.on_conflict: Option<OnConflictClause>`.
- Add `Expr::ExcludedValue { col_idx, name }`.
- Parse insert tail in this order:
  - no `ON` -> no conflict clause;
  - `ON DUPLICATE` -> existing ODKU path;
  - `ON CONFLICT` -> new PG path;
  - other `ON` -> leave for caller / parse error as today.
- Reject REPLACE + ON CONFLICT and ODKU + ON CONFLICT.
- Keep `RETURNING` after conflict tails.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_on_conflict
```

## Step 2 — Analyze ON CONFLICT

Status: completed.

**Goal:** resolve conflict targets, assignment expressions, `EXCLUDED.col`, and `DO UPDATE WHERE`.
**Files:** `analyzer_ddl.rs`, `analyzer_stmt.rs`, `plan_deps.rs`, `tests/integration_on_conflict.rs`.
**Approach:** semantic error tests first.

### Tests to add

```rust
#[test]
fn on_conflict_rejects_missing_target_column() { ... }

#[test]
fn on_conflict_do_update_requires_matching_unique_index() { ... }

#[test]
fn excluded_missing_column_is_column_not_found() { ... }
```

### Implementation outline

- Resolve target columns against target table columns.
- Validate `DO UPDATE` has a matching primary/unique index by column set.
- Reuse `resolve_odku_expr` shape but split it into a generic conflict-expression resolver:
  - unqualified `Column` -> existing target row;
  - `ExcludedValue` -> proposed row.
- Resolve `DO UPDATE WHERE` with the same dual-row expression model.
- Leave `DO NOTHING` without target valid.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_on_conflict
```

## Step 3 — Execute ON CONFLICT DO NOTHING

Status: completed.

**Goal:** implement heap-table conflict skip semantics and clustered rejection.
**Files:** `executor/on_conflict_helpers.rs`, `executor/insert_heap_ctx.rs`, `executor/insert_clustered_ctx.rs`, `executor/mod.rs`, `tests/integration_on_conflict.rs`.
**Approach:** failing executor tests first.

### Tests to add

```rust
#[test]
fn do_nothing_without_target_skips_pk_and_unique_conflicts() { ... }

#[test]
fn do_nothing_with_target_only_skips_matching_target() { ... }

#[test]
fn null_key_components_do_not_conflict() { ... }
```

### Implementation outline

- Extract a conflict locator from the ODKU/REPLACE pattern:
  - iterate primary/unique indexes;
  - honor optional conflict target;
  - honor partial-index predicate;
  - skip MATCH SIMPLE NULL keys;
  - use bloom and BTree lookup;
  - decode visible conflicting row.
- Integrate into all heap insert row paths before calling `TableEngine::insert_row_with_ctx`.
- For `DO NOTHING`, do not insert, do not count, and do not push RETURNING rows.
- In clustered insert executor, return `DbError::NotImplemented` when `stmt.on_conflict.is_some()`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_on_conflict
cargo test -p axiomdb-sql integration_insert_on_dup
```

## Step 4 — Execute ON CONFLICT DO UPDATE and RETURNING

Status: completed.

**Goal:** update conflicting heap rows with PG affected-count and RETURNING semantics.
**Files:** `executor/on_conflict_helpers.rs`, `executor/insert_heap_ctx.rs`, `executor/returning.rs`, `tests/integration_on_conflict.rs`.
**Approach:** write tests for post-resolution rows before implementation.

### Tests to add

```rust
#[test]
fn do_update_uses_target_and_excluded_values() { ... }

#[test]
fn do_update_where_false_skips_count_and_returning() { ... }

#[test]
fn returning_reports_inserted_and_updated_rows_only() { ... }

#[test]
fn same_target_row_updated_twice_errors() { ... }
```

### Implementation outline

- Generalize `eval_odku_assignment_rhs` into a helper that can evaluate:
  - existing target row values;
  - proposed/excluded row values;
  - literals, casts, unary/binary/function expressions.
- Reuse the ODKU update pipeline:
  - text constraints;
  - CHECK constraints;
  - FK child update checks;
  - parent-side FK update enforcement;
  - `TableEngine::update_row`;
  - `apply_update_index_maintenance`;
  - `ctx.stats.on_rows_changed`.
- Return an outcome containing:
  - inserted / skipped / updated;
  - optional post-resolution row for RETURNING;
  - updated target RID for duplicate-update detection.
- Count updated rows as `1`, not MySQL ODKU `2`.
- Remove the existing INSERT RETURNING conflict-resolution rejection in `insert_heap_ctx.rs`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_on_conflict
cargo test -p axiomdb-sql --test integration_returning
cargo test -p axiomdb-sql --test integration_insert_on_dup
```

## Step 5 — AST and parser for MERGE

Status: completed.

**Goal:** parse supported MERGE syntax and reject explicitly out-of-scope `BY SOURCE`.
**Files:** `lexer.rs`, `ast.rs`, `parser/dml.rs`, `parser/mod.rs`, `tests/integration_merge.rs`.
**Approach:** parser tests first.

### Tests to add

```rust
#[test]
fn parses_merge_update_delete_insert_do_nothing() { ... }

#[test]
fn merge_not_matched_by_source_is_not_implemented() { ... }
```

### Implementation outline

- Add `Token::Merge`.
- Add `Stmt::Merge(MergeStmt)`.
- Parse:
  - `MERGE INTO target [AS alias]`;
  - `USING <from_item>`;
  - `ON <expr>`;
  - one or more `WHEN ... THEN ...` actions.
- Reuse existing `parse_from_item`, `parse_assignment_list`, and expression parser.
- Use identifiers for `MATCHED`, `SOURCE`, and `NOTHING` to keep lexer changes small.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_merge
```

## Step 6 — Analyze MERGE

Status: completed.

**Goal:** resolve MERGE target/source columns and action expressions.
**Files:** `analyzer_ddl.rs`, `analyzer_stmt.rs`, `plan_deps.rs`, `tests/integration_merge.rs`.
**Approach:** tests cover qualified and unqualified references.

### Tests to add

```rust
#[test]
fn merge_resolves_source_and_target_qualified_columns() { ... }

#[test]
fn merge_rejects_ambiguous_unqualified_column() { ... }
```

### Implementation outline

- Build a `BindContext` with target columns first and source columns after.
- Materialize derived/VALUES/JSON_TABLE/SRF source schemas using existing helpers where possible.
- Resolve the `ON` predicate, action conditions, UPDATE assignments, and INSERT values.
- For INSERT actions, resolve values against source+target context but write only target columns.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_merge
```

## Step 7 — Execute MERGE

Status: completed.

**Goal:** implement heap-table MERGE actions with normal write pipelines.
**Files:** `executor/merge.rs`, `executor/exec_dispatch.rs`, `executor/mod.rs`, `executor/exec_explain.rs`, `tests/integration_merge.rs`.
**Approach:** executor tests first, action by action.

### Tests to add

```rust
#[test]
fn merge_when_matched_updates_target() { ... }

#[test]
fn merge_when_matched_deletes_target() { ... }

#[test]
fn merge_when_not_matched_inserts_target() { ... }

#[test]
fn merge_action_order_first_match_wins() { ... }

#[test]
fn merge_rejects_multiple_source_rows_for_one_target() { ... }
```

### Implementation outline

- Flush pending inserts before MERGE through dispatch barrier handling.
- Reject clustered target tables.
- Materialize the source rows once using existing SELECT/FROM materialization paths.
- Start with a generic nested evaluation path:
  - scan target rows once;
  - evaluate MERGE `ON` against combined target+source rows;
  - record matched target RID;
  - detect duplicate target matches.
- For simple unique-key equality, optionally add a conflict-locator fast path after correctness tests pass.
- Dispatch actions through normal write helpers:
  - UPDATE action uses update row + index maintenance pipeline;
  - DELETE action uses delete row + index maintenance pipeline;
  - INSERT action uses insert materialization + constraint pipeline.
- Return `QueryResult::Affected { count, last_insert_id }`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_merge
cargo test -p axiomdb-sql --test integration_lateral_join
cargo test -p axiomdb-sql --test integration_json_table_dml
```

## Step 8 — Wire smoke, regression, and docs hooks

Status: completed.

**Goal:** make the feature visible through the MySQL wire protocol and prepare subphase closure.
**Files:** `tools/wire-test.py`, `docs/progreso.md`, `memory/project_state.md`, maybe `docs/fase-21.md`.
**Approach:** update wire smoke after SQL tests are green.

### Tests to add

- `ON CONFLICT DO NOTHING` through wire.
- `ON CONFLICT DO UPDATE RETURNING` through wire.
- `MERGE INTO ... WHEN MATCHED THEN UPDATE WHEN NOT MATCHED THEN INSERT` through wire.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_on_conflict
cargo test -p axiomdb-sql --test integration_merge
cargo test -p axiomdb-sql
python3 tools/wire-test.py
```

Final subphase closure required:

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

Completed closure verification:

- `cargo fmt --check`
- `cargo test -p axiomdb-sql`
- `cargo clippy -p axiomdb-sql -- -D warnings`
- `python3 tools/wire-test.py` (417/417)
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `EXCLUDED.col` resolution collides with normal qualified-column resolution | medium | Keep a distinct `Expr::ExcludedValue` and only produce it in ON CONFLICT contexts |
| ON CONFLICT duplicate-source-row semantics are easy to miss | medium | Track updated conflicting RIDs per statement and test repeated source rows |
| MERGE source materialization can duplicate SELECT/JOIN logic | high | Prefer reuse of existing `execute_select_ctx` / FROM materialization helpers, even if the first implementation is not the fastest |
| MERGE UPDATE/DELETE index maintenance diverges from existing DML | medium | Call the same low-level update/delete helper paths used by ODKU and DELETE |
| Full MERGE performance on large sources may be O(source * target) at first | medium | Correctness first; add unique-key fast path only after green tests |

## Rollback plan

If implementation is abandoned mid-way:

1. Revert only the files touched by this plan.
2. Keep `specs/fase-21/spec-21.5-merge-upsert.md` and this plan if they remain accurate.
3. Update `memory/project_state.md` with the failed step and exact next action.

## Estimated effort

Total: 2-4 days.

- Step 1: 1-2 hours.
- Step 2: 2-3 hours.
- Step 3: 3-4 hours.
- Step 4: 4-6 hours.
- Step 5: 2-3 hours.
- Step 6: 3-4 hours.
- Step 7: 6-10 hours.
- Step 8: 2-4 hours.
