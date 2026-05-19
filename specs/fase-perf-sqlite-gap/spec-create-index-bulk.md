# Spec + Plan: create-index-bulk — bulk-build for CREATE INDEX

Phase: perf-sqlite-gap
Task: Attack 12 — apply `BTree::bulk_load_sorted` to `CREATE INDEX`
on existing tables. Same engine as Attack 11; different call site.
Status: abandoned

## Outcome (2026-05-18) — implemented + measured + reverted

Implementation completed cleanly (both heap + clustered builders
branching on `create_index_bulk_eligible`; tests 3037/3037 green).
Bench on Lima virtio showed NO measurable win — and a small
regression on small tables.

### Bench results

| Rows  | Pre-A12 (per-row) | Post-A12 (bulk-build) | Δ |
|-------|------------------:|----------------------:|--:|
| 5000  | 80ms (62K ops/s)  | 92ms (52K ops/s)      | **-16% REGRESSION** |
| 50000 | 970ms (52K ops/s) | 940ms (53K ops/s)     | +3% (noise) |

### Why it didn't help

- CREATE INDEX's per-row path is already a tight loop of
  `BTree::insert_in` — no catalog write per split (the catalog root
  is updated once at end via `update_index_root`), no per-row WAL
  records for the index (CREATE INDEX runs inside its own txn and
  the leaf pages are written via `storage.write_page` directly,
  same as A11/A12).
- The bulk-build path adds materialization cost: a `Vec<(Vec<u8>,
  RecordId)>` of all entries, plus `sort_unstable`. For tables in
  the K-row range that overhead is comparable to or larger than the
  tree-descent + split cost being eliminated.
- `bulk_load_sorted` allocates pages via `storage.alloc_page` per
  leaf (no `LocalPageBatch` batching). On Lima virtio's cheap I/O
  the savings are small.
- This contrasts with A11 (Appender flush), where the per-row path
  ALSO paid: `txn.record_index_insert` per row, secondary maintenance
  scaffolding per row, bloom-filter updates per row. Bulk-build
  collapsed all of those. CREATE INDEX doesn't pay those costs.

### Decision

Implementation reverted (only the spec file is retained as a record).
The work was honest — the architectural change was correct AND would
likely show a win on:
- Much larger tables (10M+ rows) where descent cost grows
- Real disk where alloc_page I/O is more expensive
- Workloads where `LocalPageBatch` integration could batch the
  per-leaf allocation

If a future bench surface those scenarios, this spec is a starting
point. For now A12 is not on the embedded release critical path.

## Context

`build_index_root_from_heap` (`ddl_create_index.rs:16-73`) and
`build_index_root_from_clustered` (`ddl_create_index.rs:96-164`)
scan the table and insert one entry at a time via
`BTree::insert_in` / `layout.insert_row`. For large tables this is
the slow path. Attack 11 already proved that
`BTree::bulk_load_sorted` collapses per-row inserts into a
bottom-up build for the empty-index case — and CREATE INDEX ALWAYS
starts with an empty index.

## Scope

For each of the two builders:
- Eligible: regular B-Tree (idx.index_type == 0) + NOT UNIQUE +
  no partial-index predicate
- Otherwise: keep the existing per-row path (UNIQUE indexes need
  logical-key dedup, BRIN/FTS/GIN/Trigram have different formats,
  partial indexes evaluate per row)

For eligible:
1. Allocate the empty root leaf (as today — this is the
   `old_root_pid` we'll pass to `bulk_load_sorted`)
2. Scan the table, collect all (encoded_key, rid) pairs into a Vec
3. Sort the Vec by key bytes
4. Call `BTree::bulk_load_sorted(storage, old_root_pid, &pairs,
   fillfactor)` → new root
5. Return `IndexBuildResult { root_page_id: new_root, skipped_key_too_long }`

For ineligible: existing code path unchanged.

## Non-goals

- BRIN/FTS/GIN/Trigram bulk-build (each its own attack)
- UNIQUE index bulk-build (would need logical-key dedup; not worth
  the complexity for a one-shot DDL operation)
- Partial / expression indexes (predicate eval per row)
- CREATE INDEX online / concurrent (separate spec)

## Performance budget

- CREATE INDEX on a 10000-row table: ≥ 4× faster than today
- No regression on UNIQUE / partial / non-B-Tree CREATE INDEX (same
  code path)
- All existing CREATE INDEX integration tests pass

## Implementation steps

### Step 1 — refactor `build_index_root_from_heap`

Add helper `bulk_eligible_for_create_index(idx)` returning bool
(same logic as `is_bulk_build_eligible` from A11 minus the
empty-root check, since CREATE INDEX always allocates a fresh
empty root).

Branch at top of `build_index_root_from_heap`:
```rust
if bulk_eligible_for_create_index(idx) {
    return bulk_build_from_heap(storage, table_def, col_defs, idx, snap);
}
// existing per-row code
```

Where `bulk_build_from_heap` mirrors the existing function but
collects entries into a Vec, sorts, calls bulk_load_sorted.

### Step 2 — same for `build_index_root_from_clustered`

Identical pattern. The clustered scan helper is
`scan_clustered_table` (referenced in current code line 144);
otherwise the row iteration is the same.

### Step 3 — Bench + close

- Add a quick benchmark or use existing CREATE INDEX timing test
- Run workspace tests
- Update docs/perf-sqlite-gap.md "Attack 12" subsection
- Update memory

## Done criteria

- Both builders branch on bulk-eligibility
- Non-eligible cases unchanged (verified by existing tests)
- All `cargo nextest run --workspace` tests pass
- clippy clean
- fmt clean
- One commit per builder + one closing commit
