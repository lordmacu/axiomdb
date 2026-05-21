# Spec: insert-preformat-leaf — write encoded_row straight into the clustered leaf (port SQLite BTREE_PREFORMAT)

Phase: perf-sqlite-gap — close the embedded write gap with SQLite
Task: Eliminate the per-row `OwnedLeafCell` allocation+copy on the clustered
rightmost-leaf insert fast path. The already-laid-out `encoded_row` is the
preformatted cell payload — copy it straight into the page, skipping the owned
intermediate.
Status: **REVERTED (2026-05-21)** — implemented + validated (320/320 storage tests
+ a new small/overflow round-trip test, byte-identical pages), but the A/B win was
**marginal: +3.7% `insert_appender_large` 50K (consistent), flat `insert_batch`,
flat `tree_ms`.** Root cause: the per-row row-copy was never the `tree_ms`
bottleneck — our rows are ~50 B (cheap copy + tiny alloc); `tree_ms` is dominated
by the per-row cell-pointer-array shuffle + the **per-batch 16 KB page CRC32c
(`update_checksum`) + mmap `write_page`**. SQLite's `insertCellFast` win was
avoiding a *malloc*, which for us is negligible. User reverted the storage hot-path
change (not worth it for +3.7%). Kept as reference. **The real write levers are
structural** — see `memory/project_insert_perf.md`.

## Context

Per-row insert cost on `insert_batch` is ~7.2 µs vs SQLite ~2.5 µs (macOS, fair:
both `BEGIN; N parsed INSERTs; COMMIT`). The COMMIT B-tree apply (`tree_ms`) is
~0.93 µs/row (13% of total; 37% of the COMMIT). `AXIOMDB_DEBUG_CLUSTERED_INSERT=1`
shows the SQL staging COMMIT hits the rightmost-leaf fast path on ~99% of rows
(`fast_path_hits=1980/2000`); the Appender hits it 100%. So this one path carries
nearly all clustered-insert apply cost.

On that fast path each row is copied **three** times:
1. `encode_row` → `encoded_row: Vec<u8>` — the serialization (necessary), in
   [`clustered_table.rs:106`](crates/axiomdb-sql/src/clustered_table.rs:106).
2. [`materialize_leaf_cell`](crates/axiomdb-storage/src/clustered_tree/page_utils.rs:320)
   does `key.to_vec()` + `row_data[..local_len].to_vec()` → an `OwnedLeafCell`
   (1 alloc + 1 copy for the key, 1 alloc + 1 copy for the row body).
3. [`insert_cell_with_overflow`](crates/axiomdb-storage/src/clef_write.rs:175)
   `copy_from_slice`s those slices into the page (the final placement).

Step 2 is pure waste: `insert_cell_with_overflow` already takes `&[u8]` slices and
performs the single final copy, so the owned cell exists only to be re-copied
immediately. SQLite's `insertCellFast`
([`research/sqlite/src/btree.c:7413`](research/sqlite/src/btree.c)) does **two**
copies (serialize→`pBt->pTmpSpace`, then one `memcpy`→page) with no per-row malloc;
`BTREE_PREFORMAT` ([`btree.c:9569`](research/sqlite/src/btree.c)) lets the caller
hand over an already-formatted payload so `fillInCell` is skipped entirely. Our
`encoded_row` (held in `PreparedClusteredInsertRow` / `RightmostAppendRow`) is
exactly that preformatted payload.

## Goal

On the rightmost-leaf insert fast path, copy `key` and `row_data` straight from the
caller's slices into the page (1 copy = the final placement), with **zero
per-row heap allocations** for no-overflow rows. Copy count 3 → 2; allocs/row −2.

## Non-goals

- **General / split insert path** (`btree_insert.rs:79`, `insert_with_batch`):
  entangled with `collect_leaf_cells` (which legitimately owns cells for
  redistribution). Only ~1% of rows (leaf-fill events). Follow-up.
- **`balance_quick`** (O(1) append split, [`btree.c:7992`](research/sqlite/src/btree.c)):
  separate task. This spec keeps the current split behavior.
- **rewrite / update path** (`mod.rs:1109`, `restore_exact_row_image`): out of scope.
- **On-disk / page format change.** Byte-identical page output is a hard
  requirement — only the *source* of the copy changes (slice vs owned copy).
- **Removing the `OwnedLeafCell` struct.** Split/rebalance reads still use it.
- **The `root_persist` fsync** (42% of COMMIT) — that is project B
  (deferred-checkpoint), load-bearing, out of scope here.

## Behavior

### New storage helper

Add a preformat insert that takes borrowed payload and does no owned-cell alloc:

```rust
// crates/axiomdb-storage/src/clustered_tree/page_utils.rs  (or clef_write.rs)
//
// Writes one cell into `page` at `pos` directly from `key`/`row_data` slices.
// For rows larger than the leaf-local capacity, spills the tail to an overflow
// chain first (same as materialize_leaf_cell), then writes the local prefix.
// Returns Err(HeapPageFull) without mutating the page when space is insufficient.
pub(super) fn insert_preformatted_leaf_cell(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    page: &mut Page,
    pos: usize,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<(), DbError> {
    validate_row_payload(key, row_data)?;
    let local_len = clustered_leaf::local_row_len(key.len(), row_data.len());
    let overflow_first_page = if row_data.len() > local_len {
        clustered_overflow::write_chain(storage, batch, &row_data[local_len..])?
    } else {
        None
    };
    // Single copy into the page; key + &row_data[..local_len] are slices of the
    // caller's encoded_row — no .to_vec().
    clef_write::insert_cell_with_overflow(
        page, pos, key, row_header, row_data.len(),
        &row_data[..local_len], overflow_first_page,
    )
    // On HeapPageFull after a chain was written, free it (mirror the existing
    // try_insert_rightmost_leaf_batch error handling) — see Edge cases.
}
```

### Wiring

In [`try_insert_rightmost_leaf_batch`](crates/axiomdb-storage/src/clustered_tree/mod.rs:361),
replace the `materialize_leaf_cell` + `insert_cell_with_overflow` pair (and its
defrag-retry block) with `insert_preformatted_leaf_cell`, passing
`row.key` / `row.row_header` / `row.row_data` directly. The defrag retry re-calls
the same helper with the same slices (no owned cell to carry between attempts).

The overflow-chain bookkeeping (write before page insert; `free_chain` if the page
insert ultimately fails) is preserved — see Edge cases.

## Edge cases

- **No-overflow row (common):** `local_len == row_data.len()` → pass the whole
  `row_data`, no chain, zero allocs.
- **Overflow row:** `write_chain` spills `row_data[local_len..]`; pass
  `&row_data[..local_len]`. If `insert_cell_with_overflow` then returns
  `HeapPageFull` (even after defrag), `free_chain(overflow_first_page)` and bubble
  the `HeapPageFull` so the caller falls back to the split path — same as today.
- **Defrag-retry:** first `HeapPageFull` → `clustered_leaf::defragment(page)` →
  retry the helper. The chain is written once (before the first attempt); the
  retry must NOT re-write it. Implementation: write the chain once, then attempt
  `insert_cell_with_overflow` (retrying after defrag) with the same
  `overflow_first_page`.
- **Key ordering guard** (`row.key <= prev_key` → break) is unchanged and still
  runs before the cell write.
- **Byte-identical output:** the page bytes (key len, total_row_len, row_header,
  key, local body, overflow ptr) are written by the same `insert_cell_with_overflow`
  as before — identical bytes, different source slice.

## Validation / Done criteria

1. `try_insert_rightmost_leaf_batch` performs **0** `key.to_vec()` /
   `row_data.to_vec()` per row for no-overflow rows (verify by reading the diff;
   the `OwnedLeafCell` construction is gone from that path).
2. **A/B (macOS, one-timer-per-loop, medians):**
   - `insert_appender_large --rows 50000` (fast path 100%) — expect a measurable
     `tree_ms` drop; this is the clearest signal.
   - `insert_batch --rows 10000` — expect a smaller total improvement.
   - `AXIOMDB_DEBUG_CLUSTERED_INSERT=1` shows `tree_ms` lower, `fast_path_hits`
     unchanged.
3. **Byte-identical pages:** existing clustered-tree suites pass unchanged —
   `tests_insert`, `tests_lookup`, `tests_range`, `tests_delete`, `tests_update`.
4. **Crash recovery unaffected:** WAL `ROW_INSERT` replay reconstructs identical
   pages (`axiomdb-network::integration_open_integrity` green).
5. Lima `nextest run --workspace` + `clippy --workspace -- -D warnings` +
   `fmt --check` clean.
6. No read regression (`--compare` reads unchanged — this is write-path only).

## Effort

Implementation: **high** — touches the clustered B-tree leaf insert (storage hot
path, page memory). Contained (one fast-path function + one helper), no `unsafe`
beyond existing `bytemuck` slice writes, no format/durability change.
