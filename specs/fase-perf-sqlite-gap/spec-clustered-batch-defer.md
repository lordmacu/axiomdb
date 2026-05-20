# Spec: clustered-batch-defer — defer secondary index maintenance + catalog batching for Appender clustered flush

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 10 v1 — when the embedded Appender flushes a batch of
rows into a clustered table, accumulate ALL secondary index entries
and root changes in memory, then commit them once at end-of-flush
instead of per-row
Status: implemented

## Context

After Attack 7 v1.1, the Appender's clustered path is at 82K ops/s
vs 246K on heap-only — a 110K gap. The clustered path goes through
`apply_clustered_insert_rows` (`crates/axiomdb-sql/src/executor/insert_clustered.rs:230`)
which already has an `append_biased + rightmost_leaf_hint` fast path
for the primary B-Tree. The remaining per-row costs identified in
Attack 10's research brief are:

1. **Per-row secondary index maintenance**
   (`executor/insert_helpers.rs:5-104` →
   `layout.insert_row_visible()` + `BTree::insert_in()` per row, per
   secondary index)
2. **Per-row catalog persistence** when any B-Tree root grows
   (`CatalogWriter::update_index_root()` per index per root change,
   and `update_table_root()` for the primary clustered root)
3. **Per-row WAL append** (`txn.record_clustered_insert()` →
   ClusteredRowImage per row)
4. **Looping limit of `try_insert_rightmost_leaf_batch`**
   (`clustered_tree/mod.rs:361`) — works only until the rightmost
   leaf fills, then falls back to slow path forever; doesn't
   re-arm the hint for the next leaf

This Attack focuses on (1) + (2) + (4) — the cheapest, highest-ROI
fixes. (3) WAL batching is deferred to Attack 11 (deeper, format
change).

## Goal

Reduce the per-row cost of clustered Appender flush by:

- **Deferring secondary index maintenance** — instead of inserting
  each row's secondary entries inline, accumulate them in memory
  during the primary-insert pass, then bulk-insert at end of flush
- **Batching catalog persistence** — `update_table_root` and
  `update_index_root` are called at MOST once per flush per index,
  not per leaf-split
- **Re-arming the rightmost-leaf hint** — when the current rightmost
  leaf fills, immediately move the hint to the new rightmost leaf
  so the next batch of rows continues fast-path inserts

## Non-goals

- **WAL batching** (Attack 11). Each row still writes its own WAL
  record. The WAL append cost is real but changing the format is a
  bigger Attack.
- **Bulk leaf construction** (Attack 11). We still use the existing
  per-row `insert_with_batch` for primary — we just amortize the
  secondary and catalog work AROUND it.
- **Heap path changes**. Attack 10 is clustered-only. The heap path
  is already at 246K ops/s — the bottleneck isn't there.
- **SQL INSERT path changes**. Only the Appender's clustered flush
  is touched. The SQL INSERT path (`execute_clustered_insert_ctx`)
  stays as-is; this Attack lives in `TableEngine::insert_clustered_rows_batch_with_ctx`
  and `apply_clustered_insert_rows`.
- **Non-sorted batches**. The optimization assumes the Appender's
  batch is sorted by PK (which it is when the caller appends in
  order — the common bulk-load case). Non-sorted batches still
  work, just slower.
- **External API change**. `Appender::flush` signature, return type,
  semantics: byte-identical. Only the internal path changes.

## Behavior

### Public API — no surface change

The Appender's public Rust + C FFI API are byte-identical to v1.1.
This Attack is purely an internal perf optimization.

### Internal changes

**1. `apply_clustered_insert_rows` (insert_clustered.rs:230) — split into two passes:**

```rust
// Pass 1: primary B-Tree inserts + accumulate secondary entries
let mut deferred_secondary_entries: Vec<(usize, Vec<u8>, RecordId)> = Vec::new();
let mut deferred_root_changes: Vec<(IndexId, u64)> = Vec::new();
let mut deferred_table_root_change: Option<u64> = None;

for row in &rows {
    // primary insert as today, BUT capture root changes locally
    let pre_root = current_root;
    insert_primary(...)?;
    if current_root != pre_root {
        deferred_table_root_change = Some(current_root);
    }
    // re-arm rightmost_leaf_hint if it advanced (see fix #3)

    // secondary: instead of inserting now, build the key bytes and
    // remember them for pass 2.
    for (idx_pos, secondary) in secondary_indexes.iter().enumerate() {
        let key = build_secondary_key(secondary, row)?;
        deferred_secondary_entries.push((idx_pos, key, row.rid));
    }
}

// Pass 2: bulk-insert secondary entries
for (idx_pos, entries_for_idx) in group_by(deferred_secondary_entries, |e| e.0) {
    let pre_root = secondary_indexes[idx_pos].root_page_id;
    for (_, key, rid) in entries_for_idx {
        BTree::insert_in(storage, &root_pid, &key, rid, ...);
    }
    let new_root = ...;
    if new_root != pre_root {
        deferred_root_changes.push((secondary_indexes[idx_pos].index_id, new_root));
    }
}

// Pass 3: single catalog write — one update_table_root + N
// update_index_root calls (one per CHANGED index, not per split).
let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
if let Some(new_root) = deferred_table_root_change {
    writer.update_table_root(table_def.id, new_root)?;
}
for (index_id, new_root) in deferred_root_changes {
    writer.update_index_root(index_id, new_root)?;
}
```

**2. `try_insert_rightmost_leaf_batch` re-arming**: after the
existing fast path returns N inserted, if N < rows.len() AND the
remaining rows are still sorted, the caller should advance the
rightmost-leaf hint to the NEW rightmost leaf and retry. Today the
caller falls back to slow path on the first fill.

```rust
// Pseudo:
loop {
    let inserted = try_insert_rightmost_leaf_batch(..., &hint, rows_remaining);
    if inserted == 0 { break; } // hint invalid or rows not sorted
    rows_remaining = &rows_remaining[inserted..];
    if rows_remaining.is_empty() { break; }
    // Re-arm: the leaf just filled. The new rightmost is the leaf
    // we just allocated (held in `local_page_batch`). Look it up.
    hint = find_rightmost_leaf_after_fill(...);
}
// Fall through to the existing slow path for whatever's left.
```

### Semantics

- **Correctness**: every row visible to a subsequent query after the
  Appender's `finish()` commits. Same as today.
- **Atomicity**: a failure mid-flush rolls back the whole batch (the
  txn is per-Appender; rollback aborts everything). Same as today.
- **Crash safety**: WAL still records every row insert. Crash before
  commit → recovery rolls back. Same as today.
- **Concurrent readers**: no semantic change. Snapshot isolation
  still applies; the deferred catalog write happens within the same
  txn so other transactions see either pre- or post-commit state
  (never intermediate).

### Error cases

| Failure | Existing behavior | After Attack 10 |
|---------|-------------------|-----------------|
| Primary B-Tree insert fails on row N | Error returned, prior N-1 rows are part of the appender's uncommitted txn (rolled back on Drop) | Same — the failure happens in pass 1, before any deferred work runs |
| Secondary index insert fails on row N (e.g. UNIQUE violation) | Error returned mid-pass-2; same rollback semantics | Same — the failure happens in pass 2; the txn is uncommitted, so Drop rolls back |
| Catalog persist fails | Error returned mid-pass-3 | Same — failure in pass 3 still rolls back |
| Out-of-disk during pass 1 | I/O error | Same |

## Edge cases

- [ ] Empty batch → no-op (early return as today)
- [ ] Batch with 1 row → falls through to existing code paths;
  deferred passes do trivial single-row inserts
- [ ] Batch with rows NOT sorted by PK → primary fast path doesn't
  activate; we still use the slow path for primary; secondary
  deferral still applies (Attack 10's win is partial in this case)
- [ ] Mix of new + existing secondary indexes (some have data, some
  don't) → both go through the deferred pass uniformly
- [ ] Primary insert causes the root to split through multiple
  levels → still only one `update_table_root` at the end
- [ ] Secondary index also splits through multiple levels → still
  only one `update_index_root` per index at the end
- [ ] FK on the table (already validated per-row in `append_row` —
  not in `flush`) — unchanged
- [ ] UNIQUE secondary index where the violating row is the FIRST
  one in the batch → pass-1 succeeds (primary), pass-2 fails fast
  (we visit secondaries in batch order); whole txn rolls back

## On-disk format

**No change.** Same WAL records (one per row), same heap/clustered
page format, same catalog table format. Only the ORDER of writes
changes within a single flush.

## Performance budget

| Metric | Target |
|---|---:|
| Clustered Appender on `bench_users` (PK) | 82K → ≥ 130K ops/s (≥ 50% improvement) |
| Heap Appender on `bench_users_heap` | unchanged (≤ 5% regression budget) |
| Workspace test runtime | unchanged (deferred passes are no slower than the eager path; the win is from amortization) |

If we hit ≥ 130K we declare success and move to Attack 11 for the
remaining gap. If we hit only ~100K we know secondary maintenance
was a smaller cost than the per-row catalog persist, and Attack 11
should target WAL batching instead of bulk-leaf.

## Dependencies

Depends on:
- Attack 7 v1.1 Appender clustered path
  (`crates/axiomdb-embedded/src/appender.rs`,
  `crates/axiomdb-sql/src/table_ctx.rs::insert_clustered_rows_batch_with_ctx`)
- Existing `apply_clustered_insert_rows` in
  `crates/axiomdb-sql/src/executor/insert_clustered.rs:230`
- `try_insert_rightmost_leaf_batch` in
  `crates/axiomdb-storage/src/clustered_tree/mod.rs:361`

Blocks:
- Attack 11 (WAL batching + bulk-leaf construction): once we have
  numbers we know which to attack next

## Open questions

- [x] Should the deferred secondary entries be sorted before bulk
  insert? → **No** for v1. The existing `BTree::insert_in` doesn't
  benefit from sort order for arbitrary keys; sorting would add
  cost without help. v2 could sort + bulk-build leaves.
- [x] Should we re-arm the rightmost-leaf hint as part of v1?
  → **Yes** — it's a small change with measurable impact on bulk
  loads. Otherwise the primary fast path is one-shot.
- [ ] If the SQL INSERT path also uses
  `apply_clustered_insert_rows`, do we want to push these changes
  there too? Probably yes — the change is transparent. But the SQL
  path benefits less because each statement is one row. v1: change
  the function in place (benefits both paths automatically).

## Done criteria

- [ ] `apply_clustered_insert_rows` refactored to deferred-pass
  structure (3 passes: primary, secondary, catalog)
- [ ] `try_insert_rightmost_leaf_batch` re-arming loop added in the
  caller (still inside `apply_clustered_insert_rows`)
- [ ] No public API change in axiomdb-sql or axiomdb-embedded
- [ ] All existing tests pass:
  - `cargo nextest run -p axiomdb-sql` — clustered INSERT semantics
  - `cargo nextest run -p axiomdb-embedded` — appender tests including
    clustered (38 tests must remain green)
  - `cargo nextest run --workspace` — full smoke
- [ ] `cargo clippy --workspace -- -D warnings` clean on touched
- [ ] `cargo fmt --check` clean
- [ ] New unit test in `axiomdb-sql` integration tests:
  - clustered INSERT with secondary index + 1000-row batch, query
    via secondary lookup to verify all rows visible
  - clustered INSERT with UNIQUE secondary violation mid-batch →
    whole batch rolls back
- [ ] Bench `insert_appender` (clustered) hits ≥ 130K ops/s on Lima
- [ ] Bench `insert_appender_heap` ≤ 5% regression (unchanged code
  path BUT a shared helper, sanity check)
- [ ] docs/perf-sqlite-gap.md: "Attack 10" subsection with the new
  clustered numbers
- [ ] memory/project_sqlite_baseline.md: Attack 10 entry

## References

- Research brief from the planning conversation (cited file:line
  for every claim about the current code)
- `crates/axiomdb-sql/src/executor/insert_clustered.rs:230`
  (`apply_clustered_insert_rows` — the function being refactored)
- `crates/axiomdb-storage/src/clustered_tree/mod.rs:361`
  (`try_insert_rightmost_leaf_batch` — the fast path we're going to
  re-arm in a loop)
- `crates/axiomdb-sql/src/executor/insert_helpers.rs:5`
  (the per-row secondary insert function we'll defer)
- `crates/axiomdb-catalog/src/writer.rs::update_index_root` and
  `update_table_root` (the catalog calls being batched)
- Attack 7 v1.1 numbers — `docs/perf-sqlite-gap.md` "Attack 7 v1.1"
- PostgreSQL's COPY FROM bulk-load pattern (skip per-row index +
  catalog work, do it once at end) — same idea, smaller scope
