# Plan: 4.8b — FULL OUTER JOIN

## Files to create/modify

- `crates/axiomdb-sql/src/executor.rs` — implement `JoinType::Full` in `apply_join`, remove executor-side FULL guards, and replace `is_outer_nullable` with chain-aware nullability propagation
- `crates/axiomdb-sql/tests/integration_executor.rs` — add FULL JOIN semantic tests (`ON` vs `WHERE`, duplicates, metadata nullability, multi-join, `USING`)
- `tools/wire-test.py` — add live FULL JOIN smoke/regression assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — document `FULL OUTER JOIN` as an AxiomDB SQL feature available over the MySQL wire protocol
- `docs-site/src/internals/architecture.md` — describe FULL join execution and metadata nullability propagation

## Algorithm / Data structure

### 1. Keep the current progressive nested-loop architecture

Do not introduce a new executor path.

`execute_select_with_joins(...)` already does:

1. resolve all tables
2. scan all tables under one snapshot
3. progressively join:
   - stage 0 = rows of `FROM`
   - stage 1 = `apply_join(stage0, join1.table, ...)`
   - stage 2 = `apply_join(stage1, join2.table, ...)`
4. apply `WHERE`
5. run aggregate/sort/projection/dedup/limit

`4.8b` keeps this shape and only teaches `apply_join(...)` one more join type.

### 2. Implement FULL in `apply_join(...)`

Use the existing LEFT and RIGHT logic as the base, but combine them in one pass
with a matched-right bitmap:

```rust
JoinType::Full => {
    let null_left: Row = vec![Value::Null; left_col_count];
    let null_right: Row = vec![Value::Null; right_col_count];
    let mut matched_right = vec![false; right_rows.len()];
    let mut result = Vec::new();

    for left in &left_rows {
        let mut matched = false;
        for (i, right) in right_rows.iter().enumerate() {
            let combined = concat_rows(left, right);
            if eval_join_cond(
                condition,
                &combined,
                left_schema,
                right_col_offset,
                right_columns,
            )? {
                result.push(combined);
                matched = true;
                matched_right[i] = true;
            }
        }
        if !matched {
            result.push(concat_rows(left, &null_right));
        }
    }

    for (i, right) in right_rows.iter().enumerate() {
        if !matched_right[i] {
            result.push(concat_rows(&null_left, right));
        }
    }

    Ok(result)
}
```

This preserves:
- duplicates from multiple matches
- current `ON` semantics from `eval_join_cond(...)`
- current `WHERE`-after-join pipeline
- progressive joining when the left side is already a combined row

### 3. Replace `is_outer_nullable(...)` with join-chain propagation

The current helper only understands:
- table 0 nullable if first join is RIGHT
- table i nullable if join i-1 is LEFT

That is insufficient for FULL joins and too weak for mixed outer-join chains.

Replace it with a precomputed per-table vector:

```rust
fn compute_outer_nullable(table_count: usize, joins: &[JoinClause]) -> Vec<bool> {
    let mut nullable = vec![false; table_count];

    for (join_idx, join) in joins.iter().enumerate() {
        let right_table = join_idx + 1;
        match join.join_type {
            JoinType::Inner | JoinType::Cross => {}
            JoinType::Left => {
                nullable[right_table] = true;
            }
            JoinType::Right => {
                for idx in 0..=join_idx {
                    nullable[idx] = true;
                }
            }
            JoinType::Full => {
                for idx in 0..=join_idx {
                    nullable[idx] = true;
                }
                nullable[right_table] = true;
            }
        }
    }

    nullable
}
```

Use this vector in:
- wildcard / qualified-wildcard `ColumnMeta`
- direct column projection type inference in join context

Do not leave metadata nullability split between two different rules.

### 4. Keep `USING` in the executor

Do not move `JoinCondition::Using(...)` into the analyzer in this subphase.

Reuse the existing `eval_join_cond(...)` path unchanged for FULL JOIN. The only
required guarantee in `4.8b` is that FULL reuses the same `ON`/`USING`
predicate evaluation contract as INNER/LEFT/RIGHT today.

### 5. Keep post-join pipeline unchanged

Do not special-case FULL join for:
- `WHERE`
- `GROUP BY`
- `HAVING`
- `ORDER BY`
- `DISTINCT`
- `LIMIT/OFFSET`

The existing pipeline must continue to consume `combined_rows` after the join
stage, regardless of whether they came from INNER, LEFT, RIGHT, or FULL.

## Implementation phases

1. Remove the early `JoinType::Full` `NotImplemented` guards from the join path in `executor.rs`.
2. Extend `apply_join(...)` with the FULL branch based on a matched-right bitmap.
3. Replace `is_outer_nullable(...)` with `compute_outer_nullable(...)` and thread it through metadata/type inference.
4. Add executor integration tests for:
   - basic FULL join with unmatched rows on both sides
   - duplicate matches
   - `ON` vs `WHERE`
   - `USING`
   - `SELECT *` nullability metadata
   - a join chain that includes FULL
5. Add live wire assertions in `tools/wire-test.py`.
6. Update user and technical docs for the new join type.

## Tests to write

- unit:
  - none isolated; the logic is tightly coupled to executor row assembly and metadata
- integration:
  - `SELECT * FROM users FULL OUTER JOIN orders ON ...` returns matched + unmatched left + unmatched right
  - FULL join with one-to-many match emits all matching rows
  - FULL join with `ON predicate` vs `WHERE predicate` proves the semantic difference
  - FULL join with `USING(id)` works through the current executor-side `USING` path
  - `ColumnMeta.nullable` is true for both sides of `SELECT *` over FULL join
  - `a FULL JOIN b ... LEFT JOIN c ...` keeps the join chain working
- wire:
  - `tools/wire-test.py` runs a FULL JOIN query over the live server and verifies unmatched rows from both sides
- bench:
  - none new in 4.8b; join algorithm class remains nested loop

## Anti-patterns to avoid

- Do **not** implement FULL JOIN as `LEFT JOIN UNION RIGHT JOIN`.
- Do **not** add a new hash/merge join subsystem in this subphase.
- Do **not** move `USING` binding into the analyzer as part of 4.8b.
- Do **not** forget metadata nullability; executor correctness here is not only about row contents.
- Do **not** write order-sensitive tests without an explicit `ORDER BY`.
- Do **not** document this as “MySQL-compatible SQL”; `FULL OUTER JOIN` is an AxiomDB extension over the MySQL wire protocol.

## Risks

- Mixed outer-join chains may produce correct rows but wrong metadata nullability.
  Mitigation: replace the current local helper with one chain-aware nullable vector.

- A naive FULL implementation can accidentally drop unmatched right rows.
  Mitigation: explicit `matched_right` bitmap and dedicated unmatched-right emit pass.

- `ON` vs `WHERE` semantics can be regressed by filtering too early.
  Mitigation: keep all filtering in the existing post-join `WHERE` stage and add regression tests modeled on `sqlite/test/join7.test`.

- FULL JOIN may be “functionally correct” but untested over the wire.
  Mitigation: add `tools/wire-test.py` coverage in the same subphase.
