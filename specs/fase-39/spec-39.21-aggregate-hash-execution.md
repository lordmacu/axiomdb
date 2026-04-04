# Spec: 39.21 — Aggregate Hash Execution

## What to build (not how)

Replace three root-cause bottlenecks in the hash aggregation path that together
cause a 10× slowdown versus MariaDB on GROUP BY + AVG workloads:

1. **Direct-value arithmetic** — `agg_add` and `agg_compare` currently construct
   heap-allocated `Expr::BinaryOp` AST nodes and call the full `eval()` interpreter
   on every accumulator update. Replace with direct match-on-variant arithmetic
   that never allocates and never recurses through the eval tree.

2. **Type-specialized group table** — introduce a `GroupTable` trait with two
   concrete implementations: `GroupTablePrimitive<K>` for single-column INT/BIGINT
   GROUP BY (uses the native integer directly as the hash key — no serialization),
   and `GroupTableGeneric` for all other cases (multi-column, TEXT, composite).
   `GroupTableGeneric` replaces `std::collections::HashMap` with `hashbrown::HashMap`
   and stores a pre-computed `u64` hash alongside each entry to avoid re-hashing on
   lookup probes.

3. **Eliminate `representative_row` clone** — each `GroupState` currently stores a
   full `row.clone()` as the representative row so that non-aggregate SELECT
   expressions can be evaluated at finalization time. Replace with
   `non_agg_col_values: Vec<Value>` containing only the column values actually
   referenced by non-aggregate SELECT items and HAVING expressions. The set of
   needed column indices is computed once before the scan loop, not per-row.

These three changes address the hot loop from first principles, as DataFusion's
`GroupValuesPrimitive` and PostgreSQL's `nodeAgg` transition-function model demonstrate.

## Inputs / Outputs

**Input to `execute_select_grouped_hash`:**
- `stmt: &SelectStatement` — provides GROUP BY expressions, SELECT items, HAVING
- `combined_rows: Vec<Row>` — pre-scanned, pre-filtered rows (`Row = Vec<Value>`)
- `agg_exprs: &[AggExpr]` — deduplicated aggregate descriptors
- `ctx: &SessionContext` — collation, session state

**Output:** `Vec<Row>` — one output row per group, projected to the SELECT list

**Errors:**
- `DbError::TypeMismatch` — non-numeric value passed to SUM/AVG
- `DbError::Overflow` — integer arithmetic overflow in SUM
- Any error that `eval()` can return for non-aggregate SELECT expressions

## Use cases

1. **Happy path — INT GROUP BY + AVG:**
   ```sql
   SELECT age, COUNT(*), AVG(score) FROM bench_users GROUP BY age
   ```
   - 50K rows, 62 distinct ages → `GroupTablePrimitive<i64>`, no serialization,
     direct float accumulation in `AvgAcc`.
   - Expected: < 15ms (from 60ms).

2. **TEXT GROUP BY:**
   ```sql
   SELECT department, SUM(salary) FROM employees GROUP BY department
   ```
   - `GroupTableGeneric` path. `value_to_session_key_bytes` unchanged.
   - Correctness: collation-aware grouping preserved.

3. **Multi-column GROUP BY:**
   ```sql
   SELECT age, active, COUNT(*) FROM bench_users GROUP BY age, active
   ```
   - `GroupTableGeneric` because key is composite. hashbrown + hash memoization.

4. **Non-aggregate column in SELECT:**
   ```sql
   SELECT name, age, COUNT(*) FROM t GROUP BY age
   ```
   - `name` column index added to `non_agg_col_indices`. Only `name` and `age`
     stored per group, not the full row.

5. **HAVING clause referencing non-agg column:**
   ```sql
   SELECT age, COUNT(*) FROM t GROUP BY age HAVING age > 30
   ```
   - `age` referenced in HAVING → included in `non_agg_col_indices`.

6. **COUNT(*) only — no GROUP BY:**
   ```sql
   SELECT COUNT(*), AVG(score) FROM bench_users
   ```
   - Single-group fast path: one group with empty key, uses `GroupTablePrimitive<()>`
     or inline accumulator (implementor's choice).

7. **NULL in GROUP BY column:**
   - All NULL values must form their own group, consistent with SQL standard and
     existing behavior. `GroupTablePrimitive` encodes NULL as a sentinel
     (`Option<i64>` key or a separate `null_group` slot). `GroupTableGeneric`
     already handles NULL via `value_to_session_key_bytes`.

8. **AVG with all-NULL column:**
   - `count == 0` → finalize returns `Value::Null`. No division by zero.

9. **SUM overflow:**
   - i64 + i64 overflow → `DbError::Overflow` (same as existing `agg_add` behavior,
     now without allocating AST nodes).

## Acceptance criteria

- [ ] `aggregate` benchmark on 50K rows completes in ≤ 15ms (from 60ms baseline).
      Measured with `python3 benches/comparison/local_bench.py --scenario aggregate --rows 50000`.
- [ ] All existing aggregate integration tests pass unchanged.
- [ ] `cargo test -p axiomdb-sql` clean.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] No `unwrap()` in `src/` outside `#[cfg(test)]`.
- [ ] No call to `eval()` inside `AggAccumulator::update` for `Sum`, `Min`, `Max`,
      `Avg` when the argument is a numeric literal or direct column reference
      (verified by code review, not runtime assertion).
- [ ] `agg_add` and `agg_compare` replaced or made `#[deprecated]`; no callers remain
      in the aggregation hot path.
- [ ] `GroupTablePrimitive<i64>` is selected when `GROUP BY` is a single INT or
      BIGINT column (verified by integration test that checks no `Vec<u8>` key is
      serialized — acceptable to verify via benchmark improvement alone).
- [ ] `representative_row` field removed from `GroupState` (or `GroupState` removed
      entirely if superseded by the new group table design).
- [ ] `non_agg_col_indices` computed once before the scan loop, not recalculated
      per row.
- [ ] NULL GROUP BY values form their own group (regression test).
- [ ] AVG with all-NULL input returns NULL (regression test).
- [ ] SUM / MIN / MAX over mixed Int / BigInt / Real inputs produces correct results
      (regression test for type coercion that previously went through eval).

## Out of scope

- Vectorized / morsel-driven batch processing (future Phase 10+).
- Spill-to-disk for aggregations that exceed available memory.
- ROLLUP / CUBE / GROUPING SETS.
- Changes to the sorted aggregation path (`execute_select_grouped_sorted`).
- Two-phase (partial + final) aggregation for distributed execution.
- Changes to `GROUP_CONCAT` accumulator logic.
- Window function aggregates.
- `DISTINCT` inside aggregates (e.g., `COUNT(DISTINCT col)`).
- Parallelized aggregation (Rayon).

## Dependencies

- `hashbrown` crate must be available in `axiomdb-sql`'s `Cargo.toml`. Check
  if already present transitively; add explicitly if not.
- `AggExpr`, `AggAccumulator`, `GroupState`, `execute_select_grouped_hash` in
  `crates/axiomdb-sql/src/executor/aggregate.rs` — all modified in place.
- `value_to_session_key_bytes` and `value_to_key_bytes` — read-only, not modified.
- `compare_values_null_last` — already implements direct comparison without `eval()`;
  reuse for `Min` / `Max` accumulator update.

## Key design contracts

**`value_agg_add(a: Value, b: Value) -> Result<Value, DbError>`**
- Handles: `(Int, Int)` → `Int` (checked add), `(BigInt, BigInt)` → `BigInt`
  (checked add), `(Real, Real)` → `Real`, `(Int|BigInt, Real)` and reverse →
  `Real`, `(Decimal, Decimal)` → `Decimal` (same scale required; error otherwise).
- Null inputs: caller must skip NULLs before calling (consistent with current behavior).
- Must not call `eval()`, must not allocate `Box<Expr>`.

**`GroupTable` trait**
```
trait GroupTable {
    /// Look up or insert a group for the given row.
    /// Returns (group_index, is_new).
    fn get_or_insert(&mut self, row: &[Value], key_exprs: ...) -> (usize, bool);

    /// Number of groups accumulated so far.
    fn len(&self) -> usize;

    /// Drain groups in insertion order for finalization.
    fn drain_groups(self) -> impl Iterator<Item = GroupEntry>;
}
```
- `GroupTablePrimitive<i64>` — single INT/BIGINT column: key is `i64` directly,
  backed by `hashbrown::HashMap<i64, usize>`. NULL gets a dedicated `null_group: Option<usize>`.
- `GroupTableGeneric` — all other cases: key is `Vec<u8>` (existing serialization),
  backed by `hashbrown::HashMap<u64, SmallVec<[usize; 2]>>` where the u64 is the
  pre-computed hash and the SmallVec holds group indices for collision resolution.

**`GroupEntry`** (returned by `drain_groups`):
- `key_values: Vec<Value>` (the GROUP BY key values, for output row construction)
- `non_agg_col_values: Vec<Value>` (values for non-aggregate SELECT columns)
- `accumulators: Vec<AggAccumulator>` (finalized by the caller)

**`non_agg_col_indices` computation:**
Before the scan loop, walk the SELECT list and HAVING clause. For each
`Expr::Column { col_idx, .. }` that is not inside an aggregate call, record
`col_idx`. Deduplicate. Store as `Vec<usize>` in the executor frame.
At group creation, copy only `row[idx]` for each `idx` in `non_agg_col_indices`.

## Performance contract

| Scenario | Baseline | Target | Max acceptable |
|---|---|---|---|
| aggregate 50K rows (GROUP BY age + AVG) | 60ms | 10ms | 15ms |
| insert_multi_values 50K rows | ~185ms | no regression | +5% |
| select 50K rows (full scan) | ~275ms | no regression | +5% |

These are measured with `local_bench.py --scenario X --rows 50000`.
