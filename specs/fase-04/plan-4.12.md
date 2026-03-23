# Plan: 4.12 — DISTINCT

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | modify | Remove DISTINCT guard; add `apply_distinct`; call it after projection |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add DISTINCT tests |

---

## New function

```rust
/// Deduplicates output rows, keeping the first occurrence of each unique row.
///
/// Two rows are equal if every column value serializes to the same bytes using
/// `value_to_key_bytes` — NULLs are considered equal (producing `[0x00]`),
/// consistent with SQL DISTINCT semantics.
///
/// Preserves insertion order of first occurrences (stable deduplication).
fn apply_distinct(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    for row in rows {
        let key: Vec<u8> = row.iter().flat_map(value_to_key_bytes).collect();
        if seen.insert(key) {
            result.push(row);
        }
    }
    result
}
```

---

## Algorithm

### Step 1 — Remove the DISTINCT guard

In `execute_select`, remove:
```rust
if stmt.distinct {
    return Err(DbError::NotImplemented {
        feature: "DISTINCT — Phase 4.12".into(),
    });
}
```

### Step 2 — Call `apply_distinct` after projection, before LIMIT

**Single-table path** (after `project_row` loop, before `apply_limit_offset`):
```rust
// After: rows = projected rows
if stmt.distinct {
    rows = apply_distinct(rows);
}
// Before: apply_limit_offset
```

**JOIN path** (same position):
```rust
if stmt.distinct {
    rows = apply_distinct(rows);
}
rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;
```

**GROUP BY path** (`execute_select_grouped`, after `rows.push(out_row)` loop):
```rust
if stmt.distinct {
    rows = apply_distinct(rows);
}
rows = apply_order_by(rows, &stmt.order_by)?;
rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;
```

**SELECT without FROM** (the scalar path at the top of `execute_select`):
```rust
// After building out_row (always exactly 1 row — DISTINCT is a no-op here,
// but apply it for correctness):
let rows = if stmt.distinct { apply_distinct(vec![out_row]) } else { vec![out_row] };
return Ok(QueryResult::Rows { columns: out_cols, rows });
```

---

## Implementation order

1. Add `apply_distinct` function (after `apply_limit_offset`, before unit tests).
   `cargo check -p axiomdb-sql`.

2. Remove DISTINCT guard in `execute_select`.

3. Add `apply_distinct` call in all four paths (single-table, JOIN, GROUP BY,
   SELECT without FROM).
   `cargo check -p axiomdb-sql`.

4. Add integration tests.

5. `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Integration tests in `integration_executor.rs`

```
test_distinct_single_column
  — INSERT (1), (2), (1), (3), (2); SELECT DISTINCT val → 3 unique rows

test_distinct_multi_column
  — INSERT (a,1),(b,2),(a,1),(b,3); SELECT DISTINCT dept, role → 3 pairs

test_distinct_null_dedup
  — INSERT (NULL), (1), (NULL), (2); SELECT DISTINCT val
  — exactly 1 NULL row in result + (1,) + (2,)

test_distinct_star
  — duplicate full rows; SELECT DISTINCT * → only unique rows

test_distinct_empty_table
  — SELECT DISTINCT col FROM empty → 0 rows

test_distinct_no_duplicates
  — table already has unique values; SELECT DISTINCT → same as SELECT

test_distinct_with_where
  — INSERT 5 rows, 2 with same dept; WHERE filters to 2; DISTINCT → 1

test_distinct_with_order_by
  — SELECT DISTINCT dept ORDER BY dept ASC → unique + sorted

test_distinct_with_limit
  — SELECT DISTINCT val ORDER BY val ASC LIMIT 2 → 2 smallest unique values

test_distinct_scalar
  — SELECT DISTINCT 1 → 1 row with (1,)

test_distinct_with_group_by
  — SELECT DISTINCT dept, COUNT(*) FROM t GROUP BY dept
  — same as without DISTINCT (groups already unique)
```

---

## Anti-patterns to avoid

- **DO NOT** apply DISTINCT before ORDER BY. Deduplication happens on output
  rows; ORDER BY on source rows (pre-projection) is unaffected by DISTINCT.
- **DO NOT** apply DISTINCT before LIMIT. `LIMIT 3` means "3 distinct rows",
  not "deduplicate from the first 3 rows".
- **DO NOT** duplicate the `value_to_key_bytes` logic. Reuse the existing
  function from the GROUP BY implementation.
- **DO NOT** `unwrap()` anywhere in `src/` code.

---

## Risks

None significant. `apply_distinct` is a pure function with no I/O, no unsafe,
and no interaction with external state. The only risk is applying it in the
wrong order (before projection or after LIMIT), which the plan guards against.
