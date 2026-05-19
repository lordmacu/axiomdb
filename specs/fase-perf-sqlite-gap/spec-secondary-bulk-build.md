# Spec: secondary-bulk-build — use BTree::bulk_load_sorted for empty secondary indexes in clustered Appender flush

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 11 — when the embedded Appender flushes into a clustered
table whose REGULAR B-Tree secondary indexes are EMPTY, build them
bottom-up via the existing `BTree::bulk_load_sorted` helper instead
of inserting row-by-row
Status: approved

## Context

After Attack 10, secondary index maintenance is deferred — root
persistence batches but the per-row `BTree::insert_in` still
descends + may split the tree for every row. On
`insert_appender_indexed` (clustered + 1 secondary B-Tree) we're at
~25K ops/s on Lima virtio; the per-row tree work IS the bottleneck.

The research brief reveals `BTree::bulk_load_sorted` already exists
(`crates/axiomdb-index/src/tree_bulk.rs:20-146`). It takes sorted
`&[(&[u8], RecordId)]`, allocates leaf pages directly via
`storage.alloc_page(PageType::Index)`, packs each leaf to
`ORDER_LEAF = 217` entries at fillfactor, links leaves via
`next_leaf`, builds internal pages bottom-up, and returns the new
root page id. Frees the old root if empty.

**Preconditions:** entries sorted ascending, no duplicates, old tree
at `old_root_pid` is empty.

This Attack hooks `bulk_load_sorted` into the Appender's flush for
the EMPTY-secondary case — the common pattern when an app does
CREATE TABLE → CREATE INDEX (empty) → bulk-load rows.

## Goal

For each secondary index that is:
- **Empty** at Appender open (single leaf with 0 entries), AND
- A **regular B-Tree** (index_type == 0, not BRIN/FTS/GIN/Trigram)

…replace the per-row `BTree::insert_in` work with: collect-entries
during pass-1, sort entries, call `BTree::bulk_load_sorted` once,
update the catalog root via the existing
`flush_deferred_secondary_index_roots`.

## Non-goals

- **Non-empty secondary indexes**. If the index already has data,
  fall back to the current per-row insert path. Merge-with-existing-
  tree is materially harder and out of scope.
- **Non-B-Tree indexes** (BRIN, FTS, GIN, Trigram). Each has its own
  storage format; bulk-build for those is per-attack future work.
- **Primary clustered tree**. Already has
  `try_insert_rightmost_leaf_batch` for sorted appends. Untouched.
- **CREATE INDEX bulk-build**. `ddl_create_index.rs` still does
  per-row insert. Separate target (probably the next obvious Attack).
- **Partial indexes / expression indexes**. The compiled predicates
  pipeline assumes the per-row path; defer those to fallback.
- **UNIQUE constraint detection mid-bulk**. `bulk_load_sorted`
  requires no duplicates in the input; we check before calling and
  fall back to per-row (which surfaces UniqueViolation) on duplicate.

## Behavior

### Public API — no change

Same Appender Rust API and same C FFI. This Attack is internal to
`apply_clustered_insert_rows`.

### Internal changes

**In `apply_clustered_insert_rows`** (`executor/insert_clustered.rs`):

1. **At function entry**, after computing `original_secondary_roots`,
   classify each secondary index into one of three buckets:
   - `Empty B-Tree` (eligible for bulk-build) — empty AND
     `index_type == 0` AND no partial-index predicate AND no
     expression index
   - `Eager` (everything else) — use the current per-row deferred
     path

2. **In the per-row loop**, for `Empty` indexes:
   - Instead of calling
     `maintain_clustered_secondary_inserts_deferred`, collect
     `(encoded_key, rid)` into a per-index `Vec` for later sort+
     bulk-build
   - For `Eager` indexes, keep the current behavior

3. **After the per-row loop**, for each `Empty` index:
   - Sort the collected entries by `encoded_key`
   - Check for duplicate keys; if any, fall back to per-row insert
     for THAT index (single-pass over the now-sorted entries — fast)
   - Call `BTree::bulk_load_sorted(storage, idx.root_page_id,
     &sorted_entries, idx.fillfactor)` → new root
   - Update `idx.root_page_id` and the bloom filter for each entry
   - (`flush_deferred_secondary_index_roots` after this still works
     — it sees the changed root and emits one
     `CatalogWriter::update_index_root`)

4. **WAL handling**: `BTree::bulk_load_sorted` writes pages via
   `storage.alloc_page` + `storage.write_page` directly. The
   transaction's local_page_batch is bypassed for these allocations
   (per the storage helper's design). On rollback, the orphan pages
   are leaked (acceptable — same as today for `CREATE INDEX` which
   also bypasses the batch). On commit, the catalog
   `update_index_root` makes them reachable.

### Semantics

- **Correctness**: every appended row visible after `finish()` via
  primary AND secondary lookups.
- **Atomicity**: failure mid-bulk-build (e.g. allocation error)
  rolls back the appender txn. Allocated-but-orphan pages on rollback
  are leaked (same as today for non-batched paths).
- **Concurrent readers**: snapshot isolation; readers see either
  pre- or post-commit state. The catalog `update_index_root` is
  txn-visible.
- **Crash safety**: the bulk-built pages are written to storage
  before commit; on crash before commit, recovery rolls back and the
  pages are orphan. On crash after commit, the catalog points at the
  new root and the pages are reachable.

### Error cases

| Failure | Behavior |
|---|---|
| Duplicate key in collected entries | Fall back to per-row insert for THAT index (which surfaces UniqueViolation if the index is UNIQUE; otherwise the per-row insert handles duplicates correctly via the existing path) |
| Allocation failure in bulk_load_sorted | Error returned; appender txn rolls back |
| Sort comparison panic | N/A — sorting `Vec<u8>` is total |
| Index opened empty but populated by another writer mid-flush | Impossible in v1: Appender holds the only txn; no concurrent writers |

## Edge cases

- [ ] Single secondary entry → bulk_load_sorted handles 1 entry (one
  leaf with one cell)
- [ ] Empty batch (0 rows) → no entries collected; flush_deferred
  sees no change; nothing to bulk-build
- [ ] Mixed: 2 secondary indexes, one empty + one populated → empty
  index goes through bulk-build, populated through per-row
- [ ] Mixed: 2 empty secondaries, one regular + one GIN → regular
  goes through bulk-build, GIN through per-row
- [ ] Empty index with partial-index predicate → falls back to
  per-row (predicate evaluation per row)
- [ ] Empty index with expression index → falls back to per-row
- [ ] Index becomes non-empty mid-batch by virtue of having
  inserted entries → not possible in v1 (single txn, single writer)
- [ ] PartialEq on `Vec<u8>` duplicate detection — only adjacent
  entries (since we sorted)
- [ ] Bloom filter still updated for each entry, just AFTER the
  bulk-build (not interleaved with each insert)

## On-disk format

**No change.** Same leaf and internal page formats. Same WAL records
for the post-commit catalog update. Same row encoding.

## Performance budget

| Metric | Target |
|---|---:|
| `insert_appender_indexed` (clustered + 1 empty secondary) | 25K → ≥ 100K ops/s (≥ 4× improvement) |
| `insert_appender` (no secondaries) | unchanged ±5% |
| `insert_appender_heap` | unchanged ±5% |
| Workspace tests | unchanged (correctness via existing helpers) |

If we hit ≥ 100K we declare success. If we hit only ~50-60K we know
WAL or other per-row work is dominant; Attack 12 would target that.

## Dependencies

Depends on:
- Attack 10's deferred secondary infrastructure
  (`flush_deferred_secondary_index_roots`,
  `maintain_clustered_secondary_inserts_deferred`)
- `BTree::bulk_load_sorted` — already pub in
  `crates/axiomdb-index/src/tree_bulk.rs:20`
- `CatalogWriter::update_index_root` — already used in A10

Blocks:
- Attack 12 (CREATE INDEX bulk-build) — same `bulk_load_sorted`
  applied to `ddl_create_index.rs`
- BRIN / FTS / GIN / Trigram bulk-build (each its own Attack)

## Open questions

- [x] Should we still call `txn.record_index_insert` per entry for
  bulk-built indexes? → **No.** The catalog `update_index_root` IS
  the visible change. Recovery sees either old or new root. The
  current per-row `record_index_insert` is used for undo on
  individual row rollback, which doesn't apply here.
- [x] What happens if the appender's batch has duplicate secondary
  keys (e.g. two rows with the same email, indexed UNIQUE)? →
  **Detected after sort**; fall back to per-row insert for that
  index → per-row surfaces the UniqueViolation.
- [x] What about the bloom filter? → Updated AFTER bulk-build, in a
  single loop. Bloom is just a probabilistic accelerator; lookups
  still correct without it.
- [ ] Do we need to write any WAL record for the bulk-built leaf
  pages, or is `update_index_root` sufficient for recovery?

  Recommendation: rely on `update_index_root` (catalog-level
  WAL). The pages themselves are written via `storage.write_page`
  which is the same path used by everything else. On crash before
  commit, the pages are orphaned (acceptable leak — same as `CREATE
  INDEX` which also bypasses the per-row WAL for the leaf pages it
  builds). On crash after commit, the catalog points at them and
  they're reachable.

## Done criteria

- [ ] `apply_clustered_insert_rows` classifies secondaries at
  start: `Empty B-Tree (eligible)` vs `Eager (per-row fallback)`
- [ ] Empty-eligible secondaries collect `(encoded_key, rid)`
  entries during pass 1 instead of inserting per row
- [ ] After pass 1, eligible secondaries are sorted +
  duplicate-checked + bulk-built via `BTree::bulk_load_sorted`
- [ ] Eager secondaries continue using
  `maintain_clustered_secondary_inserts_deferred` (Attack 10 path)
- [ ] All existing integration tests pass
- [ ] New integration test in `axiomdb-embedded`:
  `appender_clustered_empty_secondary_bulk_built` — verify a
  10000-row batch into a table with 1 empty index works AND the
  index is queryable
- [ ] Test for mixed empty/populated secondaries
- [ ] Test for duplicate-key fallback (UNIQUE index)
- [ ] `cargo nextest run --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean on touched
- [ ] `cargo fmt --check` clean
- [ ] `insert_appender_indexed` ≥ 100K ops/s on Lima
- [ ] No regression > 5% on `insert_appender` or
  `insert_appender_heap`
- [ ] docs/perf-sqlite-gap.md: "Attack 11" subsection
- [ ] memory/project_sqlite_baseline.md: Attack 11 entry

## References

- `crates/axiomdb-index/src/tree_bulk.rs:20-146`
  (`BTree::bulk_load_sorted` — the engine we're hooking up)
- `crates/axiomdb-index/src/page_layout.rs:36,42,239-248`
  (`MAX_KEY_LEN`, `ORDER_LEAF=217`, leaf layout)
- `crates/axiomdb-sql/src/executor/insert_clustered.rs:230`
  (`apply_clustered_insert_rows` — function being extended)
- `crates/axiomdb-sql/src/executor/insert_helpers.rs`
  (`maintain_clustered_secondary_inserts_deferred`,
  `flush_deferred_secondary_index_roots` — Attack 10 infra reused)
- `crates/axiomdb-catalog/src/writer.rs:1427-1440`
  (`CatalogWriter::update_index_root` — the catalog write)
- `crates/axiomdb-sql/src/executor/ddl_create_index.rs:16-72,
  96-164` (per-row insert today; future Attack 12 target)
- Attack 10 close — `docs/perf-sqlite-gap.md`
