# Plan: 21.9 LATERAL joins

Phase: 21 — Advanced SQL
Task: LATERAL subquery joins — production close
Spec: specs/fase-21/spec-21.9-lateral-joins.md
Status: in-progress

## Summary

The LATERAL join feature is functionally complete for SELECT joins (8 tests pass).
This plan cleans up three issues before the official close: (1) removes debug
`eprintln!` statements left in the SELECT executor, (2) fixes the DML join path
which incorrectly executes the subquery with an empty scope to derive column
names instead of inferring them from the AST, and (3) adds UPDATE/DELETE LATERAL
integration tests. All builds and tests run inside the Lima VM `axiomdb`.

## Dependencies

Must be done first:
- [x] spec-21.9-lateral-joins.md approved

Blocks:
- [ ] progreso.md 21.9 ✅ entry

## Affected files

Modified:
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — remove eprintln! debug prints
- `crates/axiomdb-sql/src/executor/dml_join.rs` — fix LATERAL placeholder column inference
- `crates/axiomdb-sql/tests/integration_lateral_join.rs` — add UPDATE/DELETE LATERAL tests

New:
- `specs/fase-21/spec-21.9-lateral-joins.md` (already created)
- `specs/fase-21/plan-21.9-lateral-joins.md` (this file)

---

## Step 1 — Remove debug prints from select_joins_ctx.rs

**Goal:** eliminate all `eprintln!("DEBUG: …")` from production source
**Files:** `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`
**Approach:** delete the 8 debug lines; no behavior change, tests still pass.

Lines to remove (approximate):
- `eprintln!("DEBUG: Subquery {} lateral=…", …)` — setup block
- `eprintln!("DEBUG: After Subquery push …")` — setup block
- `eprintln!("DEBUG combine: i=…")` in the correlated_sub arm
- All four `eprintln!("DEBUG combine: …")` in the non-correlated else arm

### Verification (Lima)
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run \
  -p axiomdb-sql --test integration_lateral_join 2>&1"
```
No DEBUG lines in stderr. All 8 tests pass.

### Commit
```
fix(fase-21): remove LATERAL debug eprintln from select_joins_ctx
```

---

## Step 2 — Fix dml_join.rs LATERAL placeholder column inference

**Goal:** correlated LATERAL subquery in UPDATE/DELETE JOIN must infer column
schema from the AST SELECT list, not by executing with empty scope.

**Files:** `crates/axiomdb-sql/src/executor/dml_join.rs`

**Problem:** current code (lines ~343-362) when `lateral=true`:
```rust
let inner_result = execute_select_ctx((**query).clone(), exec_ctx, conn_txn, ctx)?;
let columns = match inner_result { QueryResult::Rows { columns, .. } => columns, … };
```
This runs the subquery with no outer row substitution. For a correlated subquery
(references `OuterColumn`) it either panics or returns wrong columns.

**Fix:** mirror the select_joins_ctx correlated path exactly — infer
`placeholder_cols` from `query.columns` AST items using alias → fallback name,
store `Some(query.clone())` in `correlated_sub`, skip the execute call.

### Implementation outline

```rust
FromClause::Subquery { query, alias, lateral } => {
    col_offsets.push(running_offset);
    let base_left_cols: usize = all_sources.iter().map(|s| s.columns.len()).sum();
    let is_correlated = *lateral
        && crate::json_table::subquery_is_correlated(query, base_left_cols);

    if is_correlated {
        // Infer output schema from AST — same as select_joins_ctx.rs correlated path.
        let placeholder_cols: Vec<crate::result::ColumnMeta> = query
            .columns
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let name = match item {
                    crate::ast::SelectItem::Expr { alias: Some(a), .. } => a.clone(),
                    crate::ast::SelectItem::Expr { expr, alias: None } => {
                        format!("col{i}_{expr:?}").chars().take(64).collect()
                    }
                    _ => format!("col{i}"),
                };
                crate::result::ColumnMeta::computed(name, axiomdb_types::DataType::Text)
            })
            .collect();
        running_offset += placeholder_cols.len();
        all_sources.push(join_source_schema_from_derived(alias, placeholder_cols));
        scanned.push(Vec::new());
        correlated_jt.push(None);
        correlated_srf.push(None);
        correlated_sub.push(Some(query.clone()));
    } else {
        // Non-correlated: execute once, cache.
        let inner_result = execute_select_ctx((**query).clone(), exec_ctx, conn_txn, ctx)?;
        let (columns, rows) = match inner_result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err(DbError::Internal {
                message: "join-side subquery did not return rows".into(),
            }),
        };
        running_offset += columns.len();
        all_sources.push(join_source_schema_from_derived(alias, columns));
        scanned.push(rows.into_iter().map(|values| DmlJoinRow { values, target: None }).collect());
        correlated_jt.push(None);
        correlated_srf.push(None);
        correlated_sub.push(None);
    }
}
```

Also verify that the DML combine loop already handles `correlated_sub` with
`apply_correlated_subquery_dml_join` (or that it calls
`apply_correlated_subquery_join` adapted for DML rows). If missing, add it.

### Verification (Lima)
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run \
  -p axiomdb-sql --test integration_lateral_join 2>&1"
```

### Commit
```
fix(fase-21): fix LATERAL placeholder columns in dml_join for correlated subqueries
```

---

## Step 3 — Add UPDATE/DELETE LATERAL integration tests

**Goal:** confirm LATERAL in UPDATE JOIN and DELETE JOIN works end-to-end.
**Files:** `crates/axiomdb-sql/tests/integration_lateral_join.rs`

### Tests to add

```rust
// lateral_update_join
// UPDATE target t JOIN LATERAL (SELECT ...) sub ON sub.id = t.id
// SET t.val = sub.computed
// Verify updated rows reflect subquery output.

// lateral_delete_join
// DELETE t FROM target t JOIN LATERAL (SELECT ...) sub ON sub.id = t.id
// Verify only rows matched by correlated subquery are deleted.
```

### Verification (Lima)
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run \
  -p axiomdb-sql --test integration_lateral_join 2>&1"
```
All 10+ tests pass.

### Commit
```
test(fase-21): add UPDATE/DELETE LATERAL join integration tests
```

---

## Step 4 — Wire smoke + full workspace check

**Goal:** add 2 LATERAL wire assertions, verify workspace is clean.
**Files:** `tools/wire-test.py`

### Wire assertions to add
```python
# [21.9 LATERAL] basic inner
cur.execute("SELECT t.id, sub.val FROM t, LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub")
rows = cur.fetchall()
assert len(rows) == 2, f"[21.9 LATERAL inner] expected 2 rows, got {rows}"

# [21.9 LATERAL] left join null pad
cur.execute("SELECT t.id, sub.val FROM t LEFT JOIN LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub ON true")
rows = cur.fetchall()
assert len(rows) == 3, f"[21.9 LATERAL left] expected 3 rows, got {rows}"
```

### Full workspace verification (Lima)
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run --workspace 2>&1 | tail -5"

limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo clippy --workspace -- -D warnings 2>&1 | tail -10"

limactl shell axiomdb -- bash -c "source ~/.cargo/env && \
  CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo fmt --check 2>&1"
```

### Commit
```
feat(fase-21): wire smoke + workspace clean for 21.9 LATERAL joins
```

---

## Step 5 — Docs + progreso close

**Goal:** update docs-site and mark 21.9 ✅ in progreso.md.

### docs-site changes

`docs-site/src/user-guide/sql-reference/dml.md`:
- Add "LATERAL joins" section with syntax examples and semantics table

`docs-site/src/internals/sql-parser.md`:
- Add "LATERAL join executor design" note: correlation detection via
  `subquery_is_correlated`, `lateral_accum_cols` chaining, `substitute_outer`
  per-row substitution, `apply_correlated_subquery_join` in joins.rs

### progreso.md entry

```markdown
- [x] ✅ 21.9 LATERAL joins — `lateral: bool` on `FromClause::Subquery`;
  parser consumes `LATERAL` keyword; SELECT executor detects correlation via
  `subquery_is_correlated(query, effective_left_cols)` (includes chained
  lateral accumulation); correlated path stores AST + infers placeholder schema
  from SELECT list, runs `substitute_outer` + `execute_select_ctx` per outer
  row via `apply_correlated_subquery_join`; non-correlated LATERAL materializes
  once (same as derived table); LEFT JOIN null-pads unmatched outer rows;
  RIGHT/FULL LATERAL → NotImplemented (PG-compatible); DML join path mirrors
  SELECT with placeholder columns for correlated LATERAL; 10 integration tests
  in `tests/integration_lateral_join.rs`.
```

### Commit
```
docs(fase-21): update docs-site + progreso.md for 21.9 LATERAL joins
```

---

## Step 6 — Final commit + push

```bash
git push origin main
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| dml_join combine loop missing correlated_sub arm | medium | check before Step 2 |
| placeholder col names differ from real execution names | low | tests assert by position, not name |
| wire-test.py needs t/other tables pre-created | low | add setup in wire test preamble |

## Rollback plan

Each step is a clean commit. If Step 2 breaks something, `git revert` the
Step 2 commit and open an issue for the dml_join fix separately.

## Estimated effort

Total: ~2 hours
- Step 1: 10 min (delete lines)
- Step 2: 40 min (fix + verify)
- Step 3: 30 min (write tests)
- Step 4: 20 min (wire + workspace)
- Step 5: 20 min (docs)
