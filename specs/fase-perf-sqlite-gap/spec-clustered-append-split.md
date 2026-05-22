# Spec: clustered-append-split — O(1) rightmost append split (SQLite `balance_quick`)

Phase: perf-sqlite-gap — close embedded write gap with SQLite
Task: Parity lever #2 — eliminate the per-leaf-boundary slow round-trip in the
clustered insert apply path by giving the rightmost append an O(1) split that
reuses the existing tree-recursion (latch discipline + split propagation).
Status: draft

## Context

`insert_batch` (50K rows, clustered PK, append-biased) spends `tree_ms ≈ 50ms`
of its ~242ms total. A clean coarse-timer breakdown (one `Instant` pair per
leaf-fill, not per row — see `AXIOMDB_DEBUG_CLUSTERED_INSERT`) shows:

```
tree_ms ≈ 50 ms
├─ fast path  ≈ 13 ms   49 476 rows  → 0.26 µs/row   (setup·encode+slot·crc·write)  CHEAP, amortized
└─ slow path  ≈ 37.7 ms    524 rows  → 71.9 µs/row   ← 75% of tree_ms in 1% of rows
    ├─ split_leaf       ≈ 20 ms (38 µs/split); redistrib (collect+rebuild 50/50) = 6.7 ms (33%)
    └─ re-descent waste ≈ 17.7 ms
```

The 524 slow rows are exactly the **leaf boundaries**: when
`try_insert_rightmost_leaf_batch` (`clustered_tree/mod.rs:361`) fills the
rightmost leaf it breaks and the executor (`insert_clustered.rs`) falls to the
generic `lookup_physical` + `insert_with_batch` for the next row. That generic
path, for a **pure append into a full rightmost leaf**, does almost entirely
wasted work:

1. `try_insert_leaf_optimistically` (`mod.rs:160`): full descent from root +
   16 KiB leaf copy + **`defragment` O(N) of the full leaf** → fails → `None`.
   For a freshly-appended leaf with no deletes, the defragment frees nothing.
2. `insert_subtree`: re-reads root + `child_is_safe_for_insert` re-reads the
   leaf + `insert_into_leaf` re-reads the leaf + **second `defragment` O(N)** →
   `split_leaf`.
3. `split_leaf` (`btree_insert.rs:121`): `collect_leaf_cells` (≈95 cells ×2
   `to_vec` allocs) + two `rebuild_leaf_page` (re-encode ≈95 cells) — a 50/50
   redistribution that, for an append, immediately leaves the new right page
   nearly empty anyway.

SQLite solves this with `balance_quick` (`research/sqlite/src/btree.c:7992`):
when the cursor is at the rightmost entry and the page is full, it allocates a
fresh page, puts only the new cell there, leaves the full page untouched, and
inserts one divider into the parent — O(1), no cell redistribution.

## Goal

Make a strictly-increasing append past a full rightmost leaf cost an O(1) split
(allocate sibling + 1 divider into the parent) instead of the O(N) 50/50 split,
and skip the wasted optimistic-defragment round-trip — capturing the bulk of
the 37.7 ms slow path while keeping the clustered B-tree provably correct.

## Non-goals

- Not changing the per-row fast path (`try_insert_rightmost_leaf_batch`) cell
  encode / slot insert — already 0.26 µs/row.
- Not removing the one re-read of the rightmost leaf that the dedicated path
  incurs (the fast path had it in memory). Threading the in-memory page /
  a cursor path-stack through is a **future** nibble → deferred.
- Not touching the on-disk page format (no migration).
- Not touching `root_persist` (the `update_table_root` fsync) or WAL — separate
  levers.
- Not changing delete/rebalance, secondary indexes, or the heap path.

## Behavior

### Public API (storage — `axiomdb-storage::clustered_tree`)

```rust
/// Inserts `key` (which the caller guarantees is strictly greater than every
/// key currently in the tree — the rightmost-append precondition) using an
/// O(1) `balance_quick`-style split when the rightmost leaf is full. Reuses
/// the normal `insert_subtree` recursion, so latch ordering and upward split
/// propagation are identical to `insert_with_batch`. Skips the optimistic
/// pre-pass (no wasted defragment) and the 50/50 leaf redistribution.
///
/// Returns the (possibly new) root page id, exactly like `insert_with_batch`.
/// Duplicate detection is preserved: an existing key still yields
/// `DbError::DuplicateKey` via the inner `leaf_search_checked`.
pub fn insert_append_split(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    root_pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<u64, DbError>;
```

Internal (not exported), in `btree_insert.rs`:

```rust
/// O(1) leaf split for a rightmost append: old (full) page keeps all its
/// cells, gets `next_leaf = new_pid` + checksum + write; the new page holds
/// only `cell`. Returns Split { sep_key = cell.key, right_pid = new_pid } so
/// the existing `insert_into_internal` / `split_internal` / new-root machinery
/// propagates the divider upward unchanged.
fn append_split_leaf(
    storage, batch, pid, page: &Page, cell: OwnedLeafCell,
) -> Result<InsertResult, DbError>;
```

### Semantics

- **`insert_into_leaf` routing change** (`btree_insert.rs:111`): when the leaf
  is full after a defragment retry, choose the split strategy:
  - **append-split** iff `insert_pos == num_cells(page)` **AND**
    `clustered_leaf::next_leaf(page) == NULL_PAGE` (true rightmost leaf, cell
    appends at the end) → `append_split_leaf`.
  - else → `split_leaf` (existing 50/50). This gate guarantees a middle-leaf
    end-insert (key between this leaf's max and the next leaf's min) still gets
    a balanced split — append-split never fragments a non-rightmost page.
- **`insert_append_split`**: identical to `insert_with_batch` except it **omits
  `try_insert_leaf_optimistically`** (the caller already knows the leaf is full)
  and goes straight to `insert_subtree` + the `Split → new root` growth.
- **Executor change** (`insert_clustered.rs`): in the `append_biased` branch,
  when `try_insert_rightmost_leaf_batch` returns `inserted` with rows still
  remaining (rightmost leaf filled), insert the next row via
  `insert_append_split(.. current_root ..)` instead of the
  `lookup_physical` + `insert_with_batch` slow path; update `current_root`,
  re-arm `rightmost_leaf_hint` to the new rightmost leaf, continue. Non-append
  batches and the dup/restore branch are unchanged.
- Precondition (append key > tree max) is established by the fast path: it only
  breaks on `HeapPageFull`, and every row it processed satisfied
  `row.key > prev_key` where `prev_key` started at the rightmost leaf's last
  key. The boundary row therefore exceeds the tree max → no duplicate possible;
  `leaf_search_checked` nonetheless still runs and would surface a duplicate.
- Postcondition: tree remains a valid clustered B-tree — ordered, correct
  `next_leaf` chain, correct separators, balanced height — and an ordered scan
  returns every inserted row exactly once in key order.
- Invariant: `append_split_leaf` is reachable ONLY for `next_leaf == NULL` +
  end-insert; all other splits stay 50/50.

### Error cases

| Input | Expected error | Notes |
|-------|----------------|-------|
| boundary key already present (shouldn't happen given precondition, defense-in-depth) | `DbError::DuplicateKey` | via `leaf_search_checked` in `insert_into_leaf` |
| row too large / key too long | `DbError::ValueTooLarge` / `KeyTooLong` | via `validate_row_payload` (unchanged) |
| corrupt page type during descent | `DbError::BTreeCorrupted` | unchanged |

## Edge cases

- [ ] **1-level tree** (root IS the full rightmost leaf): append-split returns
  `Split` → `insert_append_split` grows a new internal root (2 levels).
- [ ] **Cascade**: leaf split → parent full → `split_internal` → root full →
  new root, all in one append (height +1).
- [ ] **Overflow row at boundary** (row needs overflow pages): the
  `OwnedLeafCell` carries `overflow_first_page`; new leaf stores it; on a
  later failure the overflow chain is freed (mirror existing cleanup).
- [ ] **Middle-leaf end-insert** (`insert_pos == num_cells` but
  `next_leaf != NULL`): MUST take the 50/50 `split_leaf`, never append-split.
- [ ] **Non-append-biased batch**: executor never calls `insert_append_split`;
  `insert_into_leaf` append gate still correct (only rightmost end-inserts).
- [ ] **Strictly-increasing batch spanning many leaves**: every boundary uses
  append-split; final tree integrity-clean; scan ordered.
- [ ] **Concurrent writers** (multi-writer engine): latch acquisition order is
  inherited from `insert_subtree` (parent-before-child, early-release on safe
  descent) — no new lock-ordering introduced.
- [ ] **Crash mid-append-split**: WAL frame redo + page LSN idempotence already
  cover page writes; append-split writes the same set of pages a normal split
  would (old leaf, new leaf, parent) so recovery is unchanged.

## On-disk format

No change. Same `ClusteredLeaf` / `ClusteredInternal` page layout, same
`next_leaf` header field, same cell encoding. Existing databases keep working;
append-split produces byte-identical page contents to what a 50/50 split would
have produced *had it placed all old cells left and the one new cell right*.

## Performance budget

| Metric (insert_batch, 50K, append-biased) | Current | Target | Max acceptable |
|---|---|---|---|
| per-boundary cost | 71.9 µs | ≤ 25 µs | ≤ 40 µs |
| `slow_tree_ms` | 37.7 ms | ≤ 13 ms | ≤ 22 ms |
| `tree_ms` | 50 ms | ≤ 28 ms | ≤ 38 ms |
| throughput | 206K rows/s | ≥ 230K | ≥ 206K (no regression) |

Reference: SQLite `insert_batch` (prepared) ≈ 362.7K rows/s = 2.76 µs/row.
This lever moves the gap from ~1.75× toward ~1.55×; parity needs it + the
autocommit-redo lever + the codec nibble (separate tasks).

Regression guard: random-insert workloads (non-append) must show **no**
`tree_ms` change and **no** page-utilization drop (append-split gate keeps
them on the 50/50 path).

## Dependencies

- Depends on: existing `insert_subtree` / `split_internal` / `insert_into_leaf`
  (`btree_insert.rs`), `clustered_leaf::{next_leaf,set_next_leaf,num_cells}`,
  the `AXIOMDB_DEBUG_CLUSTERED_INSERT` coarse timers (already added).
- Blocks: nothing; orthogonal to the redo + codec levers.

## Open questions

All resolved:

- **Avoid the parent re-descent with a cursor path-stack?** → Deferred. v1
  reuses `insert_subtree` (one clean descent) for correctness/latch reuse; the
  path-stack optimization is a future nibble (the re-descent reads are buffer-
  pool hits, cheap vs the 52 µs we remove).
- **Gate by "rightmost leaf" or by the executor's append context?** → Both: the
  storage gate (`next_leaf == NULL` + end-insert) makes `append_split_leaf`
  safe even on the generic path; the executor additionally skips the optimistic
  defragment via `insert_append_split`.
- **Separator key = first key of new page?** → Yes (`cell.key`), matching
  `split_leaf`'s `cells[split_at].key` convention so search routing is
  unchanged.

## Done criteria

- [ ] `insert_append_split` + `append_split_leaf` implemented; `insert_into_leaf`
  routes rightmost end-inserts to append-split.
- [ ] Executor boundary calls `insert_append_split` (append-biased) + re-arms
  hint; non-append paths unchanged.
- [ ] New storage tests: 1-level grow, cascade split, overflow-at-boundary,
  middle-leaf end-insert stays 50/50, many-boundary append integrity + ordered
  scan, duplicate defense.
- [ ] `IntegrityChecker` reports zero violations after a large append.
- [ ] `cargo nextest run -p axiomdb-storage` + `-p axiomdb-sql` pass (Lima).
- [ ] `cargo nextest run --workspace` clean (Lima, at close).
- [ ] `cargo clippy --workspace -- -D warnings` + `cargo fmt --check` clean.
- [ ] Bench A/B (`AXIOMDB_DEBUG_CLUSTERED_INSERT=1 --diagnose-prepared-insert
  --scenario insert_batch --rows 50000`) shows `slow_tree_ms` ≤ 13 ms and
  `tree_ms` ≤ 28 ms; throughput ≥ 230K rows/s, random-insert no regression.
- [ ] rustdoc on every new public item.

## References

- `research/sqlite/src/btree.c:7992` — `balance_quick` (O(1) append split).
- `research/sqlite/src/btree.c:7465` — `insertCellFast` (append cell path).
- `crates/axiomdb-storage/src/clustered_tree/btree_insert.rs` — split machinery.
- `crates/axiomdb-storage/src/clustered_tree/mod.rs:361` —
  `try_insert_rightmost_leaf_batch` (fast path) + the coarse timers.
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` — the apply loop.
- Prior art: `specs/fase-perf-sqlite-gap/spec-clustered-batch-defer.md` (flagged
  this boundary fallback as gap #4).
