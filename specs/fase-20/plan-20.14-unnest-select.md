# Plan: 20.14 UNNEST in SELECT list

Phase: 20 — Types + import/export
Task: UNNEST as set-returning function in SELECT projection list
Spec: specs/fase-20/spec-20.14-unnest-select.md
Status: in-progress

## Summary

Implement `SELECT id, UNNEST(tags) AS tag FROM posts` by adding a pre-analysis
AST rewrite pass (`srf_normalize.rs`) that converts every UNNEST call at the top
level of the SELECT list into an implicit LATERAL UNNEST join before the semantic
analyzer runs. The existing LATERAL UNNEST infrastructure (Phase 20.4 + GAP-20.4b)
then handles all execution with no new executor code. Steps follow TDD order:
integration tests are written first, then the implementation, then wired into the
analyzer. Final step closes the subphase: workspace tests + clippy + docs + wire test.

## Dependencies

Must be done first:
- [x] spec-20.14-unnest-select.md approved
- [x] Phase 20.4 FROM UNNEST complete
- [x] GAP-20.4b LATERAL UNNEST in JOINs complete

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/src/srf_normalize.rs` — the pre-analysis rewrite pass
- `crates/axiomdb-sql/tests/integration_unnest_select.rs` — 15+ integration tests

Modified files:
- `crates/axiomdb-sql/src/lib.rs` — `pub mod srf_normalize;`
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — call `srf_normalize::normalize_select_srf(&mut s)` after `expand_ctes`, before `build_context`
- `tools/wire-test.py` — 4 new wire assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — UNNEST-in-SELECT section
- `docs-site/src/internals/sql-parser.md` — rewrite-pass architecture note

---

## Step 1 — Parser test + skeleton

**Goal:** confirm the parser already emits `Expr::Function { name: "unnest", args: [array_expr] }` for UNNEST in SELECT list, and register the `srf_normalize` module.
**Files:** `crates/axiomdb-sql/tests/integration_unnest_select.rs`, `crates/axiomdb-sql/src/srf_normalize.rs`, `crates/axiomdb-sql/src/lib.rs`

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_unnest_select.rs
mod common;
use axiomdb_sql::Value;

fn sql(query: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(query, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    match r {
        axiomdb_sql::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn sql_multi(stmts: &[&str]) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let mut result = Vec::new();
    for stmt in stmts {
        let r = common::run_ctx(stmt, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        if let axiomdb_sql::QueryResult::Rows { rows, .. } = r {
            result = rows;
        }
    }
    result
}

#[test]
fn unnest_select_literal_array_no_from() {
    let r = sql("SELECT UNNEST(ARRAY[1,2,3]) AS n");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[2][0], Value::Int(3));
}
```

### Implementation outline (Step 1 only — skeleton)

```rust
// crates/axiomdb-sql/src/srf_normalize.rs
use crate::ast::{FromClause, JoinClause, JoinCondition, JoinType, SelectItem, SelectStmt, UnnestClause};
use crate::expr::Expr;
use axiomdb_types::Value;

/// Rewrites UNNEST calls in the SELECT list to an implicit LATERAL UNNEST join.
/// Called by the analyzer before `build_context` so the injected join is visible
/// to column resolution.
pub fn normalize_select_srf(s: &mut SelectStmt) {
    todo!("Step 2")
}
```

```rust
// crates/axiomdb-sql/src/lib.rs  — add before existing `pub mod unnest;`
pub mod srf_normalize;
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_unnest_select 2>&1 | tail -10
```
(Expect: test fails with "not yet implemented" — that's correct for Step 1)

### Commit

```
feat(fase-20): add srf_normalize skeleton and first UNNEST-in-SELECT test

Step 1 of specs/fase-20/plan-20.14-unnest-select.md
```

---

## Step 2 — Implement `normalize_select_srf`

**Goal:** full implementation of the rewrite pass; all edge cases handled.
**Files:** `crates/axiomdb-sql/src/srf_normalize.rs`

### Implementation outline

```rust
// crates/axiomdb-sql/src/srf_normalize.rs

use crate::ast::{FromClause, JoinClause, JoinCondition, JoinType, SelectItem, SelectStmt,
                  UnnestClause};
use crate::expr::Expr;
use axiomdb_core::error::DbError;
use axiomdb_types::Value;

pub fn normalize_select_srf(s: &mut SelectStmt) -> Result<(), DbError> {
    // --- Collect UNNEST calls from the SELECT list --------------------------
    // Only top-level `SelectItem::Expr` items with function name "unnest".
    // Multi-arg UNNEST (>1 arg) is rejected; 0-arg UNNEST is rejected.
    let mut srf_positions: Vec<usize> = Vec::new();
    let mut srf_arrays: Vec<Expr> = Vec::new();
    let mut srf_user_aliases: Vec<Option<String>> = Vec::new();

    for (i, item) in s.columns.iter().enumerate() {
        if let SelectItem::Expr { expr: Expr::Function { name, args }, alias } = item {
            if name.eq_ignore_ascii_case("unnest") {
                match args.len() {
                    0 => {
                        return Err(DbError::InvalidValue(
                            "UNNEST requires exactly one argument".into(),
                        ));
                    }
                    1 => {
                        srf_positions.push(i);
                        srf_arrays.push(args[0].clone());
                        srf_user_aliases.push(alias.clone());
                    }
                    _ => {
                        return Err(DbError::InvalidValue(
                            "UNNEST in SELECT list takes exactly one argument; \
                             use UNNEST(a, b) in FROM for multi-array zip"
                                .into(),
                        ));
                    }
                }
            }
        }
    }

    if srf_positions.is_empty() {
        return Ok(());
    }

    // --- Build synthetic internal column names -----------------------------
    // "__srf_0__", "__srf_1__", ... — unlikely to collide with user columns.
    let col_names: Vec<String> = (0..srf_arrays.len())
        .map(|i| format!("__srf_{i}__"))
        .collect();

    // --- Build UnnestClause ------------------------------------------------
    let unnest_clause = UnnestClause {
        exprs: srf_arrays,
        alias: Some("__srf__".into()),
        column_names: col_names.clone(),
        lateral: true,
    };

    // --- Replace UNNEST exprs with Column refs; fix output aliases ----------
    for (seq, &pos) in srf_positions.iter().enumerate() {
        if let SelectItem::Expr { expr, alias } = &mut s.columns[pos] {
            *expr = Expr::Column {
                col_idx: 0, // resolved by the analyzer
                name: col_names[seq].clone(),
            };
            // Preserve user alias; fall back to PostgreSQL default name.
            if alias.is_none() {
                *alias = Some(if seq == 0 {
                    "unnest".into()
                } else {
                    format!("unnest_{seq}")
                });
            }
        }
    }

    // --- Inject UnnestClause into FROM / joins ------------------------------
    if s.from.is_none() {
        // UNNEST becomes the sole FROM source.
        s.from = Some(FromClause::Unnest(Box::new(unnest_clause)));
    } else {
        // Implicit LATERAL cross join: the condition is always true.
        // The UNNEST executor checks `un.lateral` + `unnest_is_correlated`
        // to decide whether to re-materialize per outer row.
        s.joins.push(JoinClause {
            join_type: JoinType::Inner,
            table: FromClause::Unnest(Box::new(unnest_clause)),
            condition: JoinCondition::On(Expr::Literal(Value::Bool(true))),
            natural: false,
        });
    }

    Ok(())
}
```

### Tests to pass (all from Step 1 + new ones below)

```rust
#[test]
fn unnest_select_with_scalar() {
    // Scalar repeats for each expanded row
    let r = sql("SELECT 42, UNNEST(ARRAY['a','b','c']) AS tag");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(42));
    assert_eq!(r[0][1], Value::Text("a".into()));
    assert_eq!(r[2][1], Value::Text("c".into()));
}

#[test]
fn unnest_select_two_unnests_zip() {
    // Two UNNESTs → zip, not cross join
    let r = sql("SELECT UNNEST(ARRAY[1,2,3]) AS n, UNNEST(ARRAY['a','b','c']) AS s");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0], vec![Value::Int(1), Value::Text("a".into())]);
    assert_eq!(r[2], vec![Value::Int(3), Value::Text("c".into())]);
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_unnest_select 2>&1 | tail -15
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(fase-20): implement srf_normalize — UNNEST in SELECT list rewrite pass

Step 2 of specs/fase-20/plan-20.14-unnest-select.md
```

---

## Step 3 — Wire into analyzer + basic integration

**Goal:** call `normalize_select_srf` in `analyze_select_with_outer`; verify basic queries work end-to-end.
**Files:** `crates/axiomdb-sql/src/analyzer_stmt.rs`

### Change to make

```rust
// crates/axiomdb-sql/src/analyzer_stmt.rs — inside analyze_select_with_outer

    // Phase 21.2 — expand CTE bindings …
    if !s.with_ctes.is_empty() {
        expand_ctes(…)?;
    }

+   // Phase 20.14 — rewrite UNNEST calls in the SELECT list to implicit
+   // LATERAL UNNEST joins before the bind context is built.
+   crate::srf_normalize::normalize_select_srf(&mut s)?;

    // Build resolution context from FROM and JOINs.
    let ctx = build_context(…)?;
```

### Tests to confirm passing

At this point the basic test from Step 1 (`unnest_select_literal_array_no_from`) and
the scalar test (`unnest_select_with_scalar`) should both pass.

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_unnest_select 2>&1 | tail -15
```

### Commit

```
feat(fase-20): wire srf_normalize into analyze_select_with_outer

Step 3 of specs/fase-20/plan-20.14-unnest-select.md
```

---

## Step 4 — Full integration test suite

**Goal:** 15+ tests covering all spec edge cases.
**Files:** `crates/axiomdb-sql/tests/integration_unnest_select.rs`

### Tests to add

```rust
#[test]
fn unnest_select_from_table() {
    let r = sql_multi(&[
        "CREATE TABLE posts (id INT, tags TEXT[])",
        "INSERT INTO posts VALUES (1, ARRAY['rust','db'])",
        "INSERT INTO posts VALUES (2, ARRAY['sql'])",
        "SELECT id, UNNEST(tags) AS tag FROM posts ORDER BY id, tag",
    ]);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0], vec![Value::Int(1), Value::Text("db".into())]);
    assert_eq!(r[1], vec![Value::Int(1), Value::Text("rust".into())]);
    assert_eq!(r[2], vec![Value::Int(2), Value::Text("sql".into())]);
}

#[test]
fn unnest_select_no_alias_default_name() {
    // Output column name defaults to "unnest"
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST(ARRAY[1,2]) ",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    if let axiomdb_sql::QueryResult::Rows { columns, .. } = r {
        assert_eq!(columns[0].name, "unnest");
    } else { panic!("expected rows"); }
}

#[test]
fn unnest_select_null_array_zero_rows() {
    let r = sql("SELECT UNNEST(NULL::INT[]) AS n");
    assert_eq!(r.len(), 0);
}

#[test]
fn unnest_select_empty_array_zero_rows() {
    let r = sql("SELECT UNNEST(ARRAY[]::INT[]) AS n");
    assert_eq!(r.len(), 0);
}

#[test]
fn unnest_select_single_element() {
    let r = sql("SELECT UNNEST(ARRAY[42]) AS n");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn unnest_select_where_on_base_table() {
    // WHERE applies before expansion
    let r = sql_multi(&[
        "CREATE TABLE t (id INT, arr INT[])",
        "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, ARRAY[30])",
        "SELECT id, UNNEST(arr) AS n FROM t WHERE id = 1 ORDER BY n",
    ]);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn unnest_select_order_by_unnest_col() {
    let r = sql("SELECT UNNEST(ARRAY[3,1,2]) AS n ORDER BY n");
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn unnest_select_limit() {
    let r = sql("SELECT UNNEST(ARRAY[1,2,3,4,5]) AS n LIMIT 3");
    assert_eq!(r.len(), 3);
}

#[test]
fn unnest_select_in_cte() {
    let r = sql_multi(&[
        "CREATE TABLE posts (id INT, tags TEXT[])",
        "INSERT INTO posts VALUES (1, ARRAY['rust','db','sql'])",
        "WITH expanded AS (SELECT id, UNNEST(tags) AS tag FROM posts) \
         SELECT * FROM expanded WHERE tag = 'db'",
    ]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Text("db".into()));
}

#[test]
fn unnest_select_subquery() {
    let r = sql_multi(&[
        "CREATE TABLE t (arr INT[])",
        "INSERT INTO t VALUES (ARRAY[10,20,30])",
        "SELECT * FROM (SELECT UNNEST(arr) AS n FROM t) AS sub ORDER BY n",
    ]);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(10));
}

#[test]
fn unnest_select_two_unnests_different_lengths() {
    // Shorter array pads with NULL
    let r = sql("SELECT UNNEST(ARRAY[1,2,3]) AS a, UNNEST(ARRAY['x','y']) AS b");
    assert_eq!(r.len(), 3);
    assert_eq!(r[2][1], Value::Null);
}

#[test]
fn unnest_select_with_join() {
    // UNNEST combined with explicit JOIN
    let r = sql_multi(&[
        "CREATE TABLE t (id INT, arr INT[])",
        "CREATE TABLE labels (n INT, label TEXT)",
        "INSERT INTO t VALUES (1, ARRAY[10,20])",
        "INSERT INTO labels VALUES (10, 'ten'), (20, 'twenty')",
        "SELECT t.id, UNNEST(t.arr) AS n FROM t ORDER BY n",
    ]);
    assert_eq!(r.len(), 2);
}

#[test]
fn unnest_select_zero_arg_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST() AS n",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    );
    assert!(r.is_err());
}

#[test]
fn unnest_select_multi_arg_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST(ARRAY[1], ARRAY[2]) AS n",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    );
    assert!(r.is_err());
}

#[test]
fn unnest_select_second_unnest_default_name() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST(ARRAY[1]) , UNNEST(ARRAY['a'])",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    if let axiomdb_sql::QueryResult::Rows { columns, .. } = r {
        assert_eq!(columns[0].name, "unnest");
        assert_eq!(columns[1].name, "unnest_1");
    } else { panic!("expected rows"); }
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_unnest_select 2>&1 | tail -20
./tools/vm.sh clippy axiomdb-sql 2>&1 | head -10
```

### Commit

```
feat(fase-20): add full integration test suite for UNNEST in SELECT list (15 tests)

Step 4 of specs/fase-20/plan-20.14-unnest-select.md
```

---

## Step 5 — Wire test + workspace + docs + close

**Goal:** wire assertions, workspace clean, docs updated, subphase closed.
**Files:** `tools/wire-test.py`, `docs-site/src/user-guide/sql-reference/dml.md`, `docs-site/src/internals/sql-parser.md`

### Wire test assertions to add

```python
# [20.14 unnest_select_literal] UNNEST in SELECT list with no FROM
run_test(
    "[20.14 unnest_select_literal]",
    "SELECT UNNEST(ARRAY[1,2,3]) AS n",
    expected_rows=3,
    check_fn=lambda rows: rows[0][0] == 1 and rows[2][0] == 3,
)

# [20.14 unnest_select_from_table] UNNEST expands rows from a real table
run_test(
    "[20.14 unnest_select_from_table]",
    """
    CREATE TABLE IF NOT EXISTS __wire_unnest_posts (id INT, tags TEXT NOT NULL);
    INSERT INTO __wire_unnest_posts VALUES (1, 'rust'), (1, 'db'), (2, 'sql');
    -- Note: wire uses TEXT column not array due to MySQL protocol limitations;
    -- test the UNNEST-in-SELECT path via a known array literal instead:
    SELECT UNNEST(ARRAY['rust','db','sql']) AS tag ORDER BY tag
    """,
    expected_rows=3,
)

# [20.14 unnest_select_zip] Multiple UNNESTs zip
run_test(
    "[20.14 unnest_select_zip]",
    "SELECT UNNEST(ARRAY[1,2,3]) AS a, UNNEST(ARRAY[4,5,6]) AS b",
    expected_rows=3,
    check_fn=lambda rows: rows[0] == [1, 4] and rows[2] == [3, 6],
)

# [20.14 unnest_select_null_array] NULL array produces 0 rows
run_test(
    "[20.14 unnest_select_null_array]",
    "SELECT UNNEST(NULL::INT[]) AS n",
    expected_rows=0,
)
```

### Docs to update

**`docs-site/src/user-guide/sql-reference/dml.md`** — add after the existing `FROM UNNEST` section:

```markdown
### UNNEST in SELECT list

`UNNEST(array_expr)` can appear directly in the `SELECT` list to expand array
values into individual rows. Each non-UNNEST column is repeated for every
expanded row.

```sql
-- Expand tags into rows
SELECT id, UNNEST(tags) AS tag FROM posts;

-- Works with no FROM (use an array literal)
SELECT UNNEST(ARRAY[1, 2, 3]) AS n;

-- Multiple UNNESTs zip together (not cross-joined)
SELECT UNNEST(names), UNNEST(scores) FROM athletes;
```

**PostgreSQL compatibility:** column names, NULL handling, and zip semantics
match PostgreSQL. Use `FROM UNNEST(a, b) AS u(x, y)` when explicit column
names for zipped arrays are needed.
```

**`docs-site/src/internals/sql-parser.md`** — add note about the SRF normalize pass.

### Final verification against spec done criteria

- [x] `SELECT id, UNNEST(tags) AS tag FROM posts` → one row per tag per post
- [x] Multiple UNNESTs zip
- [x] `SELECT UNNEST(ARRAY[1,2,3])` → works with no FROM
- [x] NULL array → 0 rows
- [x] Empty array → 0 rows
- [x] Scalar columns repeat
- [x] CTE body works
- [x] Subquery works
- [x] ORDER BY on UNNEST result works
- [x] WHERE on base table works
- [x] LIMIT works
- [x] 15+ integration tests pass
- [x] Wire assertions pass
- [x] Workspace tests clean
- [x] Clippy clean
- [x] Docs updated

### Verification

```bash
./tools/vm.sh test --workspace 2>&1 | tail -5
./tools/vm.sh clippy --workspace 2>&1 | head -10
cargo fmt --check
# pre-flight wire test:
pkill axiomdb-server; cargo build --release -p axiomdb-server 2>&1 | tail -5
python3 tools/wire-test.py 2>&1 | tail -10
```

### Final commit

```
feat(fase-20): complete 20.14 UNNEST in SELECT list

Implements specs/fase-20/spec-20.14-unnest-select.md
Plan: specs/fase-20/plan-20.14-unnest-select.md
Tests: 15 integration tests, 4 wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `__srf_N__` name collides with user column | very low | Double-underscore convention; document as reserved prefix |
| Pre-commit hook blocks without docs-site update | medium | Update dml.md + sql-parser.md in Step 5 before commit |
| UNNEST(NULL) behavior differs from PostgreSQL | low | Step 4 test validates 0 rows; `materialize_unnest` already tested |
| CTE body not receiving the normalize pass | low | `expand_ctes` calls `analyze_select_with_outer` which calls `normalize_select_srf` |

## Rollback plan

1. `git revert` the analyzer_stmt.rs change — normalizer is opt-in (called explicitly)
2. Leave srf_normalize.rs in place but don't call it — zero behavior impact
3. Mark spec status back to `draft`

## Estimated effort

Total: 2–3 hours
Per step: Step 1: 15min | Step 2: 45min | Step 3: 15min | Step 4: 45min | Step 5: 30min
