# Spec: range-scan-precise-bounds

Phase: perf-sqlite-gap — close embedded read gap with SQLite
Task: Preserve inclusive/exclusive range bounds through planner → `IndexRange`
→ clustered range scan, and skip the per-row WHERE re-evaluation when the range
bounds fully cover the predicate. Ports SQLite's `OP_SeekGE/GT/LE/LT` (exact
bounds) + `disableTerm`/`TERM_CODED` (don't re-check index-covered constraints).
Status: approved

## Context

A clustered-PK range query (`SELECT * FROM t WHERE id >= lo AND id < hi`) is
planned as [`AccessMethod::IndexRange`](crates/axiomdb-sql/src/planner_types.rs:34).
Two problems make it ~1.2–1.6× slower than SQLite (the only consistent read gap
on macOS-native; on Lima the VM's mmap penalty masks it):

1. **Bounds lose strictness.** `extract_range_side`
   ([`planner_select.rs:947`](crates/axiomdb-sql/src/planner_select.rs:947))
   collapses `>`/`>=` into one "lo" and `<`/`<=` into one "hi", discarding
   inclusive vs exclusive. `range_clustered_table`
   ([`table.rs:567`](crates/axiomdb-sql/src/table.rs:567)) then wraps **both** as
   `Bound::Included`. So `id < hi` becomes `id <= hi` — the range over-includes
   the boundary key.

2. **Per-row WHERE re-eval.** Because the range is approximate, the clustered-PK
   `IndexRange` arm in
   [`select_ctx.rs:707`](crates/axiomdb-sql/src/executor/select_ctx.rs:707)
   leaves `where_already_applied = false`, so the executor re-evaluates the full
   WHERE on **every** returned row (building an `ExecSubqueryRunner` + `eval_with`
   per row) just to drop one boundary row — an O(n) cost to exclude O(1) rows.

SQLite does neither: distinct seek opcodes (`OP_SeekGE/GT/LE/LT`,
[`vdbe.c:4824`](research/sqlite/src/vdbe.c)) encode strictness exactly, and
`disableTerm` marks index-covered terms `TERM_CODED`
([`wherecode.c:419`](research/sqlite/src/wherecode.c)) so they are never
re-checked — only genuine residual terms are. `range_callback`
([`clustered_tree/mod.rs:945`](crates/axiomdb-storage/src/clustered_tree/mod.rs:945))
**already accepts `Bound::Excluded`**; the strictness is just being dropped
upstream.

## Goal

Make a clustered-PK range scan honor exact inclusive/exclusive bounds and skip
the per-row WHERE re-evaluation when the range bounds are the entire predicate.

## Non-goals

- **Other `IndexRange` producers** — single-column secondary-index ranges, prefix
  `LIKE`, composite-key ranges, heap-table ranges. They keep today's behavior
  (inclusive bounds + per-row re-eval). Only the clustered-PK two-sided/one-sided
  pure-range path becomes precise + re-eval-free in this task. Extending to the
  others is a follow-up.
- **Residual predicates** — `id >= lo AND id < hi AND active = TRUE` must still
  re-evaluate the WHERE (the range covers only the `id` part). This task must NOT
  set the skip flag for that case.
- **Reverse / DESC range scans** — out of scope.
- **NULL-bound ranges** — `col >= NULL` etc. matches no rows by SQL semantics;
  the planner already does not build a range from a NULL literal. Confirm, don't
  optimize.
- **Lima mmap-in-VM penalty** — a separate measurement/infra concern, not this.

## Behavior

### Public API

`AccessMethod::IndexRange` gains bound strictness + a covering flag:

```rust
// crates/axiomdb-sql/src/planner_types.rs
IndexRange {
    index_def: IndexDef,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    /// Lower bound is inclusive (`>=`) when true, exclusive (`>`) when false.
    lo_inclusive: bool,
    /// Upper bound is inclusive (`<=`) when true, exclusive (`<`) when false.
    hi_inclusive: bool,
    /// True when these bounds reproduce the ENTIRE WHERE predicate, so the
    /// executor may skip the per-row re-evaluation (SQLite `TERM_CODED`).
    covers_predicate: bool,
}
```

The clustered range scan honors strictness via `Bound`:

```rust
// crates/axiomdb-sql/src/table.rs — new signature
pub fn range_clustered_table(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    lo_inclusive: bool,
    hi_inclusive: bool,
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>;
```

`extract_range_side` reports strictness:

```rust
// returns (col_name, bound_value, inclusive)
fn extract_range_side(expr: &Expr) -> Option<(&str, Option<Value>, bool)>;
```

### Semantics

`range_clustered_table`:
- Builds `from = Bound::Excluded(lo)` when `!lo_inclusive`, else `Bound::Included(lo)`
  (and `Unbounded` when `lo` is `None`); symmetrically for `to`/`hi`.
- Postcondition: yields exactly the visible rows whose key satisfies the
  inclusive/exclusive bounds — identical to `range_callback`'s existing
  `Bound` handling.

Planner:
- A top-level WHERE that is exactly `col >= lo AND col < hi` (any mix of
  `>`/`>=` and `<`/`<=` on the same clustered-PK column), or a single-sided
  `col >/>=/</<= lit`, produces `IndexRange` with the exact `lo_inclusive` /
  `hi_inclusive` and `covers_predicate = true`.
- Any other producer sets `lo_inclusive = true`, `hi_inclusive = true`,
  `covers_predicate = false` (current behavior preserved).
- Invariant: `covers_predicate == true` ⇒ the bytes-encoded `[lo,hi]` with the
  given strictness select **exactly** the rows the WHERE selects (no residual).

Executor (clustered-PK `IndexRange` arm, `select_ctx.rs`):
- Passes `lo_inclusive`/`hi_inclusive` to `range_clustered_table`.
- Sets `where_already_applied = covers_predicate`. When true, the per-row WHERE
  re-eval loop is skipped (rows are already exactly the result set).

### Error cases

| Input | Expected | Message |
|-------|----------|---------|
| `covers_predicate` set but bounds inexact | (must be impossible by construction) | — |
| NULL literal bound | planner builds no range (existing behavior) | — |

No new error variants. A mis-set `covers_predicate` is a correctness bug, not a
runtime error — prevented by construction and covered by tests.

## Edge cases

- [ ] `id >= lo AND id < hi` → excludes `hi`, includes `lo`; no row count off-by-one.
- [ ] `id > lo AND id <= hi` → excludes `lo`, includes `hi`.
- [ ] `id > lo AND id < hi` → both endpoints excluded.
- [ ] `id >= lo AND id <= hi` → both included (today's behavior, still correct).
- [ ] single-sided `id >= lo` / `id < hi` → covering, exact.
- [ ] `id >= lo AND id < hi AND active = TRUE` → residual: `covers_predicate=false`,
      WHERE still re-evaluated, correct rows.
- [ ] empty range (`lo > hi`, or exclusive bounds with no key between) → 0 rows.
- [ ] boundary key exists exactly at `hi` with `id < hi` → that row excluded.
- [ ] range over a non-integer clustered PK (TEXT/BIGINT) → ordering + strictness
      still correct (byte-encoded keys preserve order).
- [ ] MVCC: a row in range created/deleted by another snapshot → visibility
      unchanged (sampling happens after `is_visible`).

Each becomes a test in `/plan-task`.

## On-disk format

None. Encoded key bytes and page layout are unchanged. This is planner/executor
behavior only.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| range_scan, 10K-row range (macOS) | ≤ 1.1× SQLite | ≤ 1.3× |
| range_scan, 1K-row range (macOS) | ≤ 1.3× SQLite | no regression vs ~1.6× |
| full_scan / point_lookup / select_where | unchanged | +5% |

Reference: macOS-native A/B this session — range_scan ~1.2–1.4× (10K range),
~1.6× (1K range); full_scan ~0.81× (AxiomDB faster). Mechanism: removing the
per-row `eval_with` + runner build (~30–50% of range-scan time at 10K rows).

## Dependencies

- Depends on: `range_callback` `Bound` support (already present);
  `where_already_applied` machinery in `select_ctx.rs` (already present).
- Blocks: extending precise bounds to secondary / composite / heap ranges (follow-up).

## Open questions (resolved at approval)

- [x] **Carry strictness as two bools vs `Bound`?** RESOLVED: two bools
      (`lo_inclusive`, `hi_inclusive`) — minimal churn to existing `lo/hi:
      Option<Vec<u8>>` producers, which set both `true`.
- [x] **How does the executor know the range covers the WHERE?** RESOLVED: a
      `covers_predicate` flag on `IndexRange`, set only by the pure clustered-PK
      range planner path (SQLite `TERM_CODED` analog). Default `false`.
- [x] **Scope to clustered-PK only?** RESOLVED: yes. Other `IndexRange` producers
      set `lo_inclusive=true, hi_inclusive=true, covers_predicate=false` →
      behavior identical to today.

## Done criteria

- [ ] `AccessMethod::IndexRange` carries `lo_inclusive`, `hi_inclusive`,
      `covers_predicate`; all producers updated (conservative defaults except the
      clustered-PK pure-range path).
- [ ] `range_clustered_table` honors strictness via `Bound::Excluded/Included`.
- [ ] Clustered-PK `IndexRange` executor arm passes strictness and sets
      `where_already_applied = covers_predicate`.
- [ ] Every edge case above has a test (exclusivity correctness + residual still
      filtered + MVCC visibility).
- [ ] `cargo nextest run -p axiomdb-sql` passes (Lima).
- [ ] `cargo nextest run --workspace` passes (Lima).
- [ ] `cargo clippy --workspace -- -D warnings` + `cargo fmt --check` clean.
- [ ] Wire smoke (`tools/wire-test.py`): a range query with an exclusive upper
      bound returns the exact rows (regression assertion).
- [ ] Correctness vs SQLite: `tools/verify-select-where.py`-style exact-row match
      for several range predicates (inclusive/exclusive mixes).
- [ ] Bench: `axiomdb_bench --compare --rows 100000` shows range_scan within
      budget; full_scan/point_lookup not regressed (A/B on macOS native).
- [ ] rustdoc on the new `IndexRange` fields and the changed `range_clustered_table`.

## References

- SQLite: `research/sqlite/src/vdbe.c:4824` (`OP_SeekGE/GT/LE/LT` exact bounds),
  `research/sqlite/src/wherecode.c:419` (`disableTerm` / `TERM_CODED` — index-
  covered constraints are not re-checked, only residual terms)
- Related: [`spec-page-cache.md`](specs/fase-perf-sqlite-gap/spec-page-cache.md)
- Checkpoint: [`docs/checkpoint-sqlite-parity.md`](docs/checkpoint-sqlite-parity.md)
  (range_scan listed as the next read gap)
- Key files: `planner_types.rs` (enum), `planner_select.rs` (`extract_range`,
  `extract_range_side`), `table.rs` (`range_clustered_table`),
  `executor/select_ctx.rs` (clustered-PK `IndexRange` arm + `where_already_applied`),
  `clustered_tree/mod.rs` (`range_callback` — already `Bound`-aware)
