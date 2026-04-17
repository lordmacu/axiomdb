# Plan: 21.12 DISTINCT ON

Phase: 21 — Advanced SQL
Task: DISTINCT ON — first row per group
Spec: specs/fase-21/spec-21.12-distinct-on.md
Status: in-progress

## Summary

Four ordered steps. Step 1 adds the AST field and migrates all struct-literal and match sites
(builds clean before any logic lands). Step 2 extends the parser to recognize `DISTINCT ON (…)`.
Step 3 wires the analyzer + all walker sites. Step 4 implements the executor helper and hooks it
into all four SELECT paths, then closes with tests, docs, and commit.

## Dependencies

Must be done first:
- [x] spec-21.12-distinct-on.md approved

Blocks:
- nothing

## Affected files

Modified:
- `crates/axiomdb-sql/src/ast.rs` — add `distinct_on: Vec<Expr>` to `SelectStmt`, update `Default` impls
- `crates/axiomdb-sql/src/parser/dml.rs` — parse `DISTINCT ON (…)`
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — resolve `distinct_on` exprs
- `crates/axiomdb-sql/src/executor/agg_group_table.rs` — add `apply_distinct_on` helper
- `crates/axiomdb-sql/src/executor/select_core.rs` — wire DISTINCT ON in 4 paths
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — wire DISTINCT ON
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — wire DISTINCT ON
- `crates/axiomdb-sql/src/executor/agg_hash.rs` — wire DISTINCT ON (GROUP BY path)
- `crates/axiomdb-sql/src/plan_deps.rs` — `distinct_on` traversal
- `crates/axiomdb-sql/src/executor/exec_subquery.rs` — `distinct_on` in `expr_has_outer_ref` / `subst_expr`
- (others as compilation reveals)

New:
- `crates/axiomdb-sql/tests/integration_distinct_on.rs` — integration tests

---

## Step 1 — AST field + migrate struct literals

**Goal:** Add `distinct_on: Vec<Expr>` to `SelectStmt`; all existing callsites compile.
**Files:** `ast.rs`

### Implementation outline

```rust
// ast.rs — SelectStmt
pub distinct: bool,
/// Phase 21.12 — DISTINCT ON key exprs. Non-empty ⟹ distinct == false.
pub distinct_on: Vec<Expr>,
```

Update all three `SelectStmt` `Default`/`new` impls (around lines 1007, 1033, 1232):
```rust
distinct_on: vec![],
```

### Verification

```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo build -p axiomdb-sql 2>&1 | head -30"
```

### Commit

```
feat(fase-21): add SelectStmt.distinct_on field (21.12 step 1)
```

---

## Step 2 — Parser: `SELECT DISTINCT ON (…)`

**Goal:** Parse `SELECT DISTINCT ON (expr, …)` and set `distinct_on`; existing `SELECT DISTINCT`
unchanged.
**Files:** `parser/dml.rs`

### Test to add (TDD)

```rust
// tests/integration_distinct_on.rs
#[test]
fn test_distinct_on_parse_basic() {
    // Verify parser accepts DISTINCT ON and that plain DISTINCT still works.
    let result = execute_sql(
        "CREATE TABLE t (id INT, grp INT, val INT); \
         INSERT INTO t VALUES (1,1,10),(2,1,20),(3,2,30); \
         SELECT DISTINCT ON (grp) grp, val FROM t ORDER BY grp, val DESC;",
    );
    // Expect one row per grp, highest val (val DESC → first = 20, 30)
    assert_eq!(result_rows(&result), vec![vec!["1","20"], vec!["2","30"]]);
}
```

### Implementation outline

In `parse_select_stmt` (after consuming `SELECT`):

```rust
// existing: let distinct = p.eat(&Token::Distinct);
let distinct = p.eat(&Token::Distinct);
let mut distinct_on: Vec<Expr> = vec![];
if distinct && p.eat(&Token::On) {
    p.expect(&Token::LParen)?;
    if matches!(p.peek(), Token::RParen) {
        return Err(DbError::ParseError(
            "DISTINCT ON requires at least one expression".into()
        ));
    }
    loop {
        distinct_on.push(parse_expr(p)?);
        if !p.eat(&Token::Comma) { break; }
    }
    p.expect(&Token::RParen)?;
    // distinct_on is set; distinct = false (DISTINCT ON ≠ plain DISTINCT)
}
let distinct = distinct && distinct_on.is_empty(); // plain DISTINCT only if no ON(…)
```

Set in `SelectStmt`:
```rust
SelectStmt {
    distinct,
    distinct_on,
    // ...
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo test -p axiomdb-sql --test integration_distinct_on 2>&1 | tail -20"
```

### Commit

```
feat(fase-21): parse DISTINCT ON (21.12 step 2)
```

---

## Step 3 — Analyzer + walker arms

**Goal:** Resolve `distinct_on` expressions in the analyzer; add match arms everywhere the
compiler requires.
**Files:** `analyzer_stmt.rs`, `plan_deps.rs`, `executor/exec_subquery.rs`, others as needed.

### Implementation outline

**`analyzer_stmt.rs`** — in `analyze_select` after SELECT columns are resolved:
```rust
let resolved_distinct_on = s.distinct_on.into_iter()
    .map(|e| resolve_expr_full(e, &ctx, outer_scopes, &mut state))
    .collect::<Result<Vec<_>, _>>()?;
s.distinct_on = resolved_distinct_on;
```

**`plan_deps.rs`** — `PlanDepsVisitor::visit_select`:
```rust
for e in &s.distinct_on { self.visit_expr(e)?; }
```

**`executor/exec_subquery.rs`** — `expr_has_outer_ref` + `subst_expr` for SelectStmt:
```rust
// expr_has_outer_ref walks stmt.distinct_on
// subst_expr substitutes into stmt.distinct_on
```

(Other files as `cargo build` reveals.)

### Verification

```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo build -p axiomdb-sql 2>&1 | grep -E 'error|warning.*unused' | head -20"
```

### Commit

```
feat(fase-21): resolve distinct_on in analyzer (21.12 step 3)
```

---

## Step 4 — Executor helper + full integration tests + docs + close

**Goal:** Implement `apply_distinct_on` and wire into all SELECT executor paths; write all tests;
update docs; close the subphase.
**Files:** `executor/agg_group_table.rs`, `executor/select_core.rs`, `executor/select_ctx.rs`,
`executor/select_joins_ctx.rs`, `executor/agg_hash.rs`, `tests/integration_distinct_on.rs`,
`docs-site/`, `docs/progreso.md`

### Implementation outline

**`executor/agg_group_table.rs`** — new helper below `apply_distinct_with_session`:

```rust
/// Phase 21.12 — Deduplicates `rows` keeping the first row per DISTINCT ON key group.
///
/// Algorithm:
/// 1. Sort by (distinct_on exprs all ASC NULLS LAST, then order_by items).
/// 2. Walk sorted rows; for each row serialize the DISTINCT ON key and emit
///    only the first occurrence.
///
/// The result is already in correct ORDER BY sequence — no second sort needed.
pub(crate) fn apply_distinct_on(
    rows: Vec<Row>,
    distinct_on: &[Expr],
    order_by: &[OrderByItem],
) -> Result<Vec<Row>, DbError> {
    // Build combined sort: distinct_on exprs as ASC NULLS LAST, then order_by
    let mut combined_ob: Vec<OrderByItem> = distinct_on
        .iter()
        .map(|e| OrderByItem { expr: e.clone(), asc: true, nulls_first: false })
        .collect();
    combined_ob.extend_from_slice(order_by);

    let mut sorted = apply_order_by(rows, &combined_ob)?;

    // Deduplicate: keep first per DISTINCT ON key
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    sorted.retain(|row| {
        let key: Vec<u8> = distinct_on
            .iter()
            .flat_map(|e| {
                let v = crate::eval::eval(e, row).unwrap_or(Value::Null);
                value_to_session_key_bytes(&v)
            })
            .collect();
        seen.insert(key)
    });
    Ok(sorted)
}
```

**Wire in `select_core.rs`** — replace the ORDER BY + `apply_distinct_with_session` block in each
of the 4 paths:

```rust
// Before (4× in select_core.rs):
combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
// ...
if stmt.distinct { rows = apply_distinct_with_session(rows); }

// After — when distinct_on non-empty:
if !stmt.distinct_on.is_empty() {
    combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob)?;
    // project after dedup
    rows = combined_rows.iter().map(|v| project_row(&stmt.columns, v)).collect::<Result<_,_>>()?;
} else {
    combined_rows = apply_order_by(combined_rows, &resolved_ob)?;  // existing
    rows = combined_rows.iter().map(|v| project_row(&stmt.columns, v)).collect::<Result<_,_>>()?;
    if stmt.distinct { rows = apply_distinct_with_session(rows); }
}
```

Same pattern in `select_ctx.rs` and `select_joins_ctx.rs`.

### Tests (10 minimum)

```rust
// tests/integration_distinct_on.rs
fn test_distinct_on_latest_per_group()          // classic latest-per-group
fn test_distinct_on_no_order_by()               // arbitrary first row (just check count)
fn test_distinct_on_multiple_key_cols()         // DISTINCT ON (a, b)
fn test_distinct_on_expr_not_in_select()        // DISTINCT ON (LOWER(name))
fn test_distinct_on_null_keys_treated_equal()   // two NULL keys → one row
fn test_distinct_on_with_limit()                // LIMIT applied after dedup
fn test_distinct_on_with_where()                // WHERE filters before dedup
fn test_distinct_on_positional()                // DISTINCT ON (1) → first SELECT col
fn test_distinct_on_empty_parens_error()        // DISTINCT ON () → parse error
fn test_plain_distinct_still_works()            // regression: SELECT DISTINCT unchanged
fn test_distinct_on_single_col_all_unique()     // no dedup needed → all rows
fn test_distinct_on_subquery()                  // FROM (SELECT ...) AS s
```

### Verification against spec

- [ ] `SelectStmt.distinct_on: Vec<Expr>` — ✅
- [ ] Parser: `DISTINCT ON (e1,e2)` — ✅
- [ ] Parser: `DISTINCT ON ()` → parse error — ✅
- [ ] Analyzer: exprs resolved — ✅
- [ ] Executor: sort-then-first algorithm — ✅
- [ ] Executor: plain DISTINCT regression — ✅
- [ ] 12+ integration tests pass — ✅
- [ ] `cargo test --workspace` — ✅
- [ ] clippy clean — ✅
- [ ] Wire smoke assertions — ✅
- [ ] Docs updated — ✅
- [ ] `docs/progreso.md` updated — ✅

### Final commit

```
feat(fase-21): implement 21.12 DISTINCT ON

- SelectStmt.distinct_on: Vec<Expr> field
- Parser: SELECT DISTINCT ON (e1, e2, ...) syntax
- Analyzer: resolves distinct_on against source scope
- Executor: apply_distinct_on helper (sort-then-first, pre-projection)
  wired into all 4 SELECT paths
- 12 integration tests in tests/integration_distinct_on.rs
- Wire smoke assertions for DISTINCT ON
- Docs: dml.md + sql-parser.md updated

Phase 21/34. Spec: specs/fase-21/spec-21.12-distinct-on.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `apply_distinct_on` called on projected rows instead of pre-projection | medium | verify test: DISTINCT ON expr not in SELECT passes |
| `eval` inside `retain` panics on error | low | use `unwrap_or(Value::Null)` + test NULL path |
| ORDER BY remap collides with DISTINCT ON sort | low | DISTINCT ON sort runs independently before remap |

## Rollback plan

If abandoned: `git reset --hard` before step 1 commit; `distinct_on` was a pure addition.

## Estimated effort

Total: ~3h
Step 1: 20min / Step 2: 30min / Step 3: 40min / Step 4: 90min
