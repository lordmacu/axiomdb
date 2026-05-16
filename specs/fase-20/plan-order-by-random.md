# Plan: order-by-random

Phase: 20 — Types + import/export
Task: ORDER BY RANDOM() — random row ordering
Spec: specs/fase-20/spec-order-by-random.md
Status: done

## Summary

Two-step implementation, all changes confined to `axiomdb-sql`. Step 1 fixes
`RAND()`/`RANDOM()` to use the `rand` crate's `thread_rng` instead of the current
LCG/subsec_nanos PRNG, and adds arity validation. Step 2 modifies `apply_order_by`
and `apply_order_by_top_n` in `shared.rs` to detect RANDOM() in ORDER BY items and
take the correct path: pure shuffle (O(n)) for `ORDER BY RANDOM()`, or pre-materialized
random keys for mixed ORDER BY. All call sites stay unchanged.

## Dependencies

Must be done first:
- [x] spec-order-by-random.md approved

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_order_by_random.rs` — integration tests

Modified files:
- `crates/axiomdb-sql/src/eval/functions/numeric.rs` — fix RAND()/RANDOM() PRNG + arity
- `crates/axiomdb-sql/src/executor/shared.rs` — is_rand_call + apply_order_by changes
- `tools/wire-test.py` — 2+ new assertions
- `docs/progreso.md` — mark 20.12 ✅
- `docs-site/src/user-guide/sql-reference/expressions.md` — RANDOM() docs
- `memory/project_state.md` — update state

---

## Step 1 — Fix RAND()/RANDOM() scalar function

**Goal:** replace LCG/subsec_nanos with `rand::random::<f64>()` and validate arity.

**Files:** `crates/axiomdb-sql/src/eval/functions/numeric.rs`

### Current code (line 290)

```rust
"rand" | "random" => {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let r = (seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) as f64)
        / (u32::MAX as f64 + 1.0);
    Ok(Value::Real(r))
}
```

### Replacement

```rust
"rand" | "random" => {
    if !args.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "RAND takes no arguments".into(),
        });
    }
    Ok(Value::Real(rand::random::<f64>()))
}
```

### Test (integration_order_by_random.rs)

```rust
#[test]
fn rand_returns_real_in_range() {
    let v = eval("RAND()");
    match v {
        Value::Real(f) => assert!(f >= 0.0 && f < 1.0),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn rand_wrong_arity_errors() {
    let err = eval_err("RAND(1)");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn random_returns_real_in_range() {
    let v = eval("RANDOM()");
    match v {
        Value::Real(f) => assert!(f >= 0.0 && f < 1.0),
        other => panic!("expected Real, got {other:?}"),
    }
}
```

### Verification

```bash
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 1 — fix RAND()/RANDOM() to use rand::thread_rng, add arity check
```

---

## Step 2 — apply_order_by: detect RANDOM() + shuffle/pre-materialize

**Goal:** `ORDER BY RANDOM()` shuffles correctly; `ORDER BY col, RANDOM()` sorts by col
first with random tie-breaking; `apply_order_by_top_n` falls back to full shuffle for
RANDOM() cases.

**Files:** `crates/axiomdb-sql/src/executor/shared.rs`

### Helper to add

```rust
/// Returns true if `expr` is a zero-arg call to RAND or RANDOM.
fn is_rand_call(expr: &Expr) -> bool {
    matches!(expr, Expr::Function { name, args }
        if args.is_empty()
            && matches!(name.to_ascii_lowercase().as_str(), "rand" | "random"))
}

/// Returns true if any ORDER BY item is a RAND/RANDOM() call.
fn order_by_has_random(order_items: &[OrderByItem]) -> bool {
    order_items.iter().any(|item| is_rand_call(&item.expr))
}
```

### Changes to apply_order_by

Insert at the top of `apply_order_by`, before the existing `if order_items.is_empty()` check:

```rust
fn apply_order_by(mut rows: Vec<Row>, order_items: &[OrderByItem]) -> Result<Vec<Row>, DbError> {
    if order_items.is_empty() {
        return Ok(rows);
    }

    // Pure RANDOM() — single ORDER BY item that is RAND()/RANDOM().
    // Use Fisher-Yates shuffle: O(n), correct (one key per row, not per comparison).
    if order_items.len() == 1 && is_rand_call(&order_items[0].expr) {
        use rand::seq::SliceRandom;
        rows.shuffle(&mut rand::thread_rng());
        return Ok(rows);
    }

    // Mixed ORDER BY containing RANDOM() — pre-materialize one f64 per row per
    // RANDOM() item so the sort comparator sees stable keys.
    // Build a Vec<(row, random_key_per_random_item)> and sort using those.
    if order_by_has_random(order_items) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        // Attach a random f64 per RANDOM() position to each row.
        let random_positions: Vec<usize> = order_items
            .iter()
            .enumerate()
            .filter(|(_, item)| is_rand_call(&item.expr))
            .map(|(i, _)| i)
            .collect();
        // rows_with_keys: (row, Vec<Option<f64>>) — Some(f64) for RANDOM() positions.
        let mut rows_with_keys: Vec<(Row, Vec<f64>)> = rows
            .into_iter()
            .map(|row| {
                let keys: Vec<f64> = random_positions.iter().map(|_| rng.gen::<f64>()).collect();
                (row, keys)
            })
            .collect();

        let mut sort_err: Option<DbError> = None;
        rows_with_keys.sort_by(|(row_a, keys_a), (row_b, keys_b)| {
            if sort_err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            let mut rand_pos_iter_a = keys_a.iter();
            let mut rand_pos_iter_b = keys_b.iter();
            for item in order_items {
                let (key_a, key_b) = if is_rand_call(&item.expr) {
                    let a = Value::Real(*rand_pos_iter_a.next().unwrap());
                    let b = Value::Real(*rand_pos_iter_b.next().unwrap());
                    (a, b)
                } else {
                    match (eval(&item.expr, row_a), eval(&item.expr, row_b)) {
                        (Ok(a), Ok(b)) => (a, b),
                        (Err(e), _) | (_, Err(e)) => {
                            sort_err = Some(e);
                            return std::cmp::Ordering::Equal;
                        }
                    }
                };
                let ord = compare_sort_values(&key_a, &key_b, item.order, item.nulls);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        if let Some(e) = sort_err {
            return Err(e);
        }
        return Ok(rows_with_keys.into_iter().map(|(row, _)| row).collect());
    }

    // Existing fast + slow paths (no RANDOM() in ORDER BY) unchanged...
```

### Changes to apply_order_by_top_n

Insert at the top, after the early return for empty cases:

```rust
fn apply_order_by_top_n(
    rows: Vec<Row>,
    order_items: &[OrderByItem],
    top_n: usize,
) -> Result<Vec<Row>, DbError> {
    if order_items.is_empty() || rows.is_empty() || top_n == 0 {
        return Ok(Vec::new());
    }

    // RANDOM() in ORDER BY is incompatible with heap-sort (heap requires stable comparator).
    // Fall back to full shuffle/sort then slice.
    if order_by_has_random(order_items) {
        let mut sorted = apply_order_by(rows, order_items)?;
        sorted.truncate(top_n);
        return Ok(sorted);
    }

    // Existing implementation unchanged...
```

### Tests (integration_order_by_random.rs)

```rust
#[test]
fn order_by_random_is_permutation()  // sorted result == all rows
fn order_by_random_limit()           // exactly N rows returned
fn order_by_random_offset_limit()    // correct slice
fn order_by_random_empty_table()     // empty → empty
fn order_by_random_single_row()      // 1 row → that row
fn order_by_col_then_random()        // col1 ordering respected, random within ties
fn order_by_random_limit_zero()      // LIMIT 0 → empty
fn order_by_random_limit_exceeds_rows() // LIMIT 100 on 5-row table → 5 rows
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_order_by_random
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 2 — ORDER BY RANDOM() shuffle + pre-materialized random keys
```

---

## Step 3 — Close: workspace tests, wire smoke, docs

**Goal:** pass all workspace gates, add wire assertions, update docs.

### Wire assertions

```python
# [20.12a] ORDER BY RANDOM() returns all rows (permutation check via COUNT)
cur.execute("CREATE TABLE IF NOT EXISTS _wire_random (v INT)")
cur.execute("DELETE FROM _wire_random")
for i in range(10): cur.execute(f"INSERT INTO _wire_random VALUES ({i})")
conn.commit()
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_random ORDER BY RANDOM()) sub")
ok("[20.12a order_by_random_count] ORDER BY RANDOM() returns all rows", cur.fetchone()[0] == 10)

# [20.12b] ORDER BY RANDOM() LIMIT 3 returns exactly 3
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_random ORDER BY RANDOM() LIMIT 3) sub")
ok("[20.12b order_by_random_limit] ORDER BY RANDOM() LIMIT 3 → 3 rows", cur.fetchone()[0] == 3)
```

### Docs to update

`docs-site/src/user-guide/sql-reference/expressions.md`: add note under the RAND section explaining that `ORDER BY RANDOM()` is now supported and uses Fisher-Yates shuffle. Note that `RAND(n)` with arguments is invalid.

### Verification against spec done criteria

- [x] RAND()/RANDOM() use rand::random (not LCG)
- [x] ORDER BY RANDOM() returns permutation of all rows
- [x] ORDER BY RANDOM() LIMIT N returns N rows
- [x] ORDER BY col1, RANDOM() respects col1 ordering
- [x] ORDER BY RANDOM() OFFSET M LIMIT N returns correct slice
- [x] RAND(1) returns error
- [x] Empty table → empty result
- [x] cargo nextest run --workspace passes
- [x] cargo clippy clean
- [x] cargo fmt --check clean
- [x] Wire: 2+ new assertions (571 → 573+)

### Final commit

```
feat(fase-20): complete subphase 20.12 — ORDER BY RANDOM() + RAND() fix

- RAND()/RANDOM(): now uses rand::random::<f64>(), arity validated
- apply_order_by: pure RANDOM() → Fisher-Yates O(n) shuffle
- apply_order_by: mixed ORDER BY → pre-materialized random keys per row
- apply_order_by_top_n: falls back to full shuffle for RANDOM() cases
- Tests: N integration tests
- Wire: 573+/573+ assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Mixed ORDER BY comparator lifetime issues with borrowed rand keys | low | `rows_with_keys` owns keys as `Vec<f64>`, no lifetimes involved |
| Clippy complains about `rand_pos_iter_a/b` consumed in closure | low | convert to indexed access if needed |

## Rollback plan

1. `git reset --hard HEAD~N` where N = commits from Step 1 forward, or
2. Branch `abandoned/plan-order-by-random-<date>` + spec status → `draft`

## Estimated effort

Total: ~1.5 hours
Per step: step 1: 20min, step 2: 50min, step 3: 20min
