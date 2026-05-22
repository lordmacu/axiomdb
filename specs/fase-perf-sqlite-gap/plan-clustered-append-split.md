# Plan: clustered-append-split

Phase: perf-sqlite-gap — close embedded write gap with SQLite
Task: Parity lever #2 — O(1) rightmost append split (SQLite `balance_quick`)
Spec: specs/fase-perf-sqlite-gap/spec-clustered-append-split.md
Status: draft

## Summary

Replace the O(N) 50/50 leaf split + wasted slow-path round-trip that every
rightmost-leaf boundary pays during an append with an O(1) `balance_quick`-style
append split. Order: (1) TDD red tests that capture the *behavioral* difference
(old leaf keeps all its cells; append-split is used only for rightmost leaves),
(2) `append_split_leaf` + the routing gate in `insert_into_leaf` (storage,
correctness-isolated), (3) `insert_append_split` (skips the optimistic
defragment) + the one-line executor swap, (4) bench A/B + close. Correctness is
fenced by the gate `next_leaf == NULL_PAGE && insert_pos == num_cells`: only a
true rightmost end-insert ever skips the balanced split, so non-append and
middle-leaf workloads are byte-for-byte unchanged.

## Dependencies

Must be done first:
- [x] spec-clustered-append-split approved (committed 4a15e9ca)
- [x] coarse timers in place (committed 4a15e9ca) — used for the A/B in Step 4

Blocks: nothing.

## Affected files

Modified:
- `crates/axiomdb-storage/src/clustered_tree/btree_insert.rs` — add
  `append_split_leaf`; route to it from `insert_into_leaf`.
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — add public
  `insert_append_split`.
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` — swap
  `insert_with_batch` → `insert_append_split` in the append-biased no-dup arm.
- `crates/axiomdb-storage/tests/clustered_append_split.rs` (NEW) — invariants +
  behavioral red tests.

## Step 1 — TDD red/regression tests (storage)

**Goal:** lock the contract before changing the split.
**Files:** `crates/axiomdb-storage/tests/clustered_append_split.rs` (new).
**Approach:** use `MemoryStorage`; drive `insert_append_split` (Step 2/3 will
make it exist — until then the test file won't compile, which is the red).

### Tests to add

```rust
// Helper: insert strictly-increasing u64 keys via insert_append_split,
// tracking the root. Row payload small + fixed so ~N cells per 16 KiB leaf.

#[test]
fn append_keeps_old_leaf_full_then_new_leaf() {
    // Insert until the rightmost leaf fills + 1 more (forces ONE boundary).
    // Assert: the original leaf still has ALL its cells (append-split does NOT
    // redistribute), next_leaf points to the new leaf, new leaf has exactly 1
    // cell. With the old 50/50 split this FAILS (old leaf would be ~half).
}

#[test]
fn many_boundaries_ordered_scan_and_integrity() {
    // Insert 50k strictly-increasing keys. Assert: scan_visible/range returns
    // all keys in order, count == 50k, IntegrityChecker reports 0 violations,
    // tree height grew as expected.
}

#[test]
fn one_level_tree_grows_via_append_split() {
    // Insert into an empty table until the single root-leaf fills + 1. Assert:
    // root becomes ClusteredInternal with 2 children; both leaves valid; scan
    // ordered.
}

#[test]
fn cascade_split_to_new_root() {
    // Append enough to force a leaf split whose separator overflows the parent
    // → parent split → new root (height +1 in one insert). Assert integrity +
    // ordered scan.
}

#[test]
fn overflow_row_at_boundary() {
    // Make the boundary row large enough to need an overflow chain. Assert it
    // round-trips (lookup returns full row_data) and integrity is clean.
}

#[test]
fn middle_leaf_end_insert_still_5050() {
    // Build a tree, then insert a key that lands at the END of a NON-rightmost
    // leaf (next_leaf != NULL). Assert that leaf was split ~50/50 (NOT
    // append-split): both halves are >25% full. Guards the gate.
}
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test clustered_append_split   # RED (won't compile yet)
```

### Commit
```
test(fase-perf-sqlite-gap): clustered append-split invariants + behavioral tests

Step 1 of specs/fase-perf-sqlite-gap/plan-clustered-append-split.md
```

---

## Step 2 — `append_split_leaf` + routing gate (storage)

**Goal:** O(1) append split, reachable only for rightmost end-inserts.
**Files:** `btree_insert.rs`.

### Implementation outline
```rust
// btree_insert.rs — new fn
fn append_split_leaf(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    pid: u64,
    mut page: Page,        // owned full leaf (already attempted insert)
    cell: OwnedLeafCell,
) -> Result<InsertResult, DbError> {
    let right_pid = batch_alloc_page(storage, batch, PageType::ClusteredLeaf)?;
    let old_next = clustered_leaf::next_leaf(&page);     // NULL for rightmost
    // New right leaf: just the one appended cell.
    let mut right = Page::new(PageType::ClusteredLeaf, right_pid);
    clustered_leaf::init_clustered_leaf(&mut right);
    clustered_leaf::set_next_leaf(&mut right, old_next);
    clustered_leaf::insert_cell_with_overflow(
        &mut right, 0, &cell.key, &cell.row_header,
        cell.total_row_len, &cell.local_row_data, cell.overflow_first_page,
    )?;
    // Old leaf unchanged except next_leaf → right.
    clustered_leaf::set_next_leaf(&mut page, right_pid);
    page.update_checksum();
    storage.write_page_under_page_lock(pid, &page)?;
    right.update_checksum();
    write_page(storage, right_pid, &mut right)?;
    Ok(InsertResult::Split { sep_key: cell.key, right_pid })
}
```

```rust
// insert_into_leaf — route on the FIRST HeapPageFull, before defragment:
Err(DbError::HeapPageFull { .. }) => {
    if insert_pos == clustered_leaf::num_cells(&page) as usize
        && clustered_leaf::next_leaf(&page) == clustered_leaf::NULL_PAGE
    {
        // Rightmost append into a full leaf → O(1), no defragment.
        return append_split_leaf(storage, batch, pid, page, cell);
    }
    clustered_leaf::defragment(&mut page);
    // ... existing retry → split_leaf (50/50) ...
}
```
(Removes the wasted second defragment for appends. `cell.overflow_first_page`
cleanup on the rare post-route error mirrors `split_leaf`'s contract — append
path can only fail on alloc/write, which propagate as `Err`.)

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test clustered_append_split
./tools/vm.sh clippy -p axiomdb-storage
```
Expect: `append_keeps_old_leaf_full…`, `one_level_…`, `cascade_…`,
`overflow_…`, `middle_leaf_…` all green (some need Step 3's wrapper to drive —
if so, drive them through `insert_with_batch` here and switch to
`insert_append_split` in Step 3).

### Commit
```
feat(fase-perf-sqlite-gap): O(1) append split for rightmost clustered leaf

Step 2 of specs/fase-perf-sqlite-gap/plan-clustered-append-split.md
```

---

## Step 3 — `insert_append_split` + executor swap

**Goal:** skip the optimistic-defragment round-trip; wire the executor.
**Files:** `mod.rs`, `insert_clustered.rs`.

### Implementation outline
```rust
// mod.rs — like insert_with_batch but NO try_insert_leaf_optimistically.
pub fn insert_append_split(
    storage: &dyn StorageEngine,
    mut batch: Option<&mut LocalPageBatch>,
    root_pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<u64, DbError> {
    validate_row_payload(key, row_data)?;
    match insert_subtree(storage, batch.as_deref_mut(), root_pid, key, row_header, row_data)? {
        InsertResult::Inserted => Ok(root_pid),
        InsertResult::Split { sep_key, right_pid } => {
            let new_root_pid = batch_alloc_page(storage, batch.as_deref_mut(), PageType::ClusteredInternal)?;
            let mut new_root = Page::new(PageType::ClusteredInternal, new_root_pid);
            clustered_internal::init_clustered_internal(&mut new_root, root_pid);
            clustered_internal::insert_at(&mut new_root, 0, &sep_key, right_pid)?;
            write_page(storage, new_root_pid, &mut new_root)?;
            Ok(new_root_pid)
        }
    }
}
```

```rust
// insert_clustered.rs — append-biased no-dup arm (the `else` at ~line 453):
let new_root = if append_biased {
    axiomdb_storage::clustered_tree::insert_append_split(
        storage, Some(&mut conn_txn.local_page_batch), current_root,
        &row.primary_key_bytes, &new_header, &row.encoded_row,
    )?
} else {
    axiomdb_storage::clustered_tree::insert_with_batch(
        storage, Some(&mut conn_txn.local_page_batch), Some(current_root),
        &row.primary_key_bytes, &new_header, &row.encoded_row,
    )?
};
```
WAL (`record_clustered_insert`), secondary maintenance, and hint re-arm
(`descend_to_leaf_pub`) below are unchanged — `insert_append_split` returns the
same `new_root` contract, so no atomicity/cleanup logic is duplicated. The
storage gate guarantees correctness even if a non-rightmost append-biased row
reaches here (it falls back to 50/50 internally).

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test clustered_append_split
./tools/vm.sh test -p axiomdb-sql -E 'test(insert)'
./tools/vm.sh clippy -p axiomdb-storage -p axiomdb-sql
```

### Commit
```
feat(fase-perf-sqlite-gap): route clustered append boundary to append-split

Step 3 of specs/fase-perf-sqlite-gap/plan-clustered-append-split.md
```

---

## Step 4 — Bench A/B + close

**Goal:** confirm the win + spec done-criteria; close cleanly.
**Files:** none (measurement) + docs/memory at close.

### Verification against spec
```bash
cargo build --release -p axiomdb-bench-comparison      # macOS native (perf exception)
AXIOMDB_DEBUG_CLUSTERED_INSERT=1 ./target/release/axiomdb_bench \
  --diagnose-prepared-insert --scenario insert_batch --rows 50000
# expect: slow_tree_ms ≤ 13, tree_ms ≤ 28, throughput ≥ 230K, splits unchanged
./tools/vm.sh test --workspace        # clean
./tools/vm.sh clippy --workspace      # -D warnings clean
./tools/vm.sh fmt-check               # clean
```
- [ ] random-insert scenario: no `tree_ms` regression (gate holds)
- [ ] docs-site `internals/btree.md` — document append-split / balance_quick
- [ ] `memory/project_insert_perf.md` — record the measured win + that the
  slow-path round-trip (not per-row encode/CRC) was the lever-2 bottleneck
- [ ] `docs/progreso.md` — mark the task

### Final commit
```
perf(fase-perf-sqlite-gap): complete clustered append-split (lever #2)

Implements specs/fase-perf-sqlite-gap/spec-clustered-append-split.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Tree corruption (bad separator / next_leaf) | low | Step 1 integrity + ordered-scan tests run before merge; reuse `insert_subtree` propagation |
| Append-split fires on non-rightmost leaf → fragmentation | low | gate `next_leaf==NULL && insert_pos==num_cells`; `middle_leaf_end_insert_still_5050` test |
| Concurrency deadlock | low | no new lock order — `insert_append_split` reuses `insert_subtree`'s latch discipline |
| Win below target (re-descent reads remain) | medium | acceptable; path-stack/cursor reuse is a documented future nibble |

## Rollback plan

Each step is a standalone commit. To abandon: `git revert` Steps 2-3 (the gate +
`append_split_leaf` + executor swap); Step 1 tests stay as regression guards.

## Estimated effort

Total: impl max (B-tree hot path + correctness). Step 1 ~1h, Step 2 ~1.5h,
Step 3 ~1h, Step 4 ~1h.
```