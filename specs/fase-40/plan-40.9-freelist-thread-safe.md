# Plan: 40.9 — FreeList Tier-1 (per-connection LocalPageBatch)

## Files to create / modify

### Workspace + dependency
- `Cargo.toml` (root) — add `crossbeam-queue = "0.3"` to `[workspace.dependencies]`
- `crates/axiomdb-storage/Cargo.toml` — pull `crossbeam-queue` into storage

### `axiomdb-storage` (the global allocator side)
- `crates/axiomdb-storage/src/freelist.rs`
  - Add `alloc_batch_sequential(n) -> Vec<u64>` and `free_batch(&[u64])`
- `crates/axiomdb-storage/src/lib.rs` (StorageEngine trait)
  - Add `alloc_page_batch(n, ty)`, `free_page_batch(&[u64])`, `extension_waiters()` with defaults
- `crates/axiomdb-storage/src/mmap.rs`
  - Add `recycle_queue: crossbeam_queue::SegQueue<u64>`, `extension_waiters: AtomicU32`
  - Implement `alloc_page_batch` / `free_page_batch`
  - Update `release_deferred_frees` to drain recycle_queue into bitmap
- `crates/axiomdb-storage/src/memory.rs`
  - Override `alloc_page_batch` / `free_page_batch` for single-lock batching

### `axiomdb-wal` (the per-connection batch side)
- `crates/axiomdb-wal/src/txn.rs`
  - Add `LocalPageBatch { available: VecDeque<u64>, current_type, freed, last_refill_size }` + consts
  - Add field to `ConnectionTxn`
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — init in `begin`, drain in `commit`
- `crates/axiomdb-wal/src/txn_rollback.rs` — drain in `rollback` / `rollback_to_savepoint`
- `Savepoint` struct — add `available_len_at_sp` and `freed_len_at_sp` fields

### Hot-path call site wiring
| Crate | File | Functions |
|---|---|---|
| `axiomdb-storage` | `heap_chain_write.rs` | `insert`, `insert_with_hint` |
| `axiomdb-storage` | `clustered_tree/btree_insert.rs` | `split_leaf`, `split_internal`, `insert_subtree` |
| `axiomdb-storage` | `clustered_tree/mod.rs` | `insert` (root creation) |
| `axiomdb-storage` | `clustered_overflow.rs` | `write_chain` |
| `axiomdb-index` | `tree_insert.rs` | `insert_leaf`, `split_internal`, `alloc_root` |
| `axiomdb-index` | `tree_delete.rs` | `rotate_left`, `rotate_right`, `merge_children` |

All gain `Option<&mut LocalPageBatch>` and route through `pop_or_refill`.

### `axiomdb-sql` (executor wiring)
- `ExecutionContext::alloc_page(ty)` / `free_page(pid)` wrappers

## Algorithm: `pop_or_refill`

```text
1. If batch.current_type != Some(ty):
   a. Drain batch.available → storage.free_page_batch (return to bitmap)
   b. Set current_type = Some(ty), last_refill_size = BATCH_ALLOC_SIZE
2. If batch.available non-empty: pop_front, return
3. Compute n = last_refill_size × max(1, extension_waiters), capped at 256
4. ids = storage.alloc_page_batch(n, ty)
5. last_refill_size = n
6. available.extend(ids.skip(1)), return ids[0]
```

## Algorithm: `MmapStorage::alloc_page_batch`

```text
Phase 1 (lock-free): drain recycle_queue (SegQueue pops) until full or empty
Phase 2 (mutex): take freelist.lock(), alloc_batch_sequential(remaining)
Phase 3 (grow): if still short → grow_lock, do_grow, retry
Set freelist_dirty. Return exactly n IDs or Err(StorageFull).
```

## Algorithm: `FreeList::alloc_batch_sequential`

```text
1. Scan words[] for a contiguous run of n free bits (cross-word boundary OK)
2. If found: mark all n bits USED, return (start..start+n)
3. If not found: fall back to alloc() in a loop, return non-contiguous fill
```

## Drain semantics

| Event | `available` | `freed` |
|---|---|---|
| **Commit** | → bitmap via `free_page_batch` (steal protection) | → `recycle_queue` for fast reuse |
| **Rollback** | → bitmap via `free_page_batch` | dropped (rows restored by undo → pages in use) |
| **Savepoint rollback** | unchanged | truncated to `sp.freed_len_at_sp` |
| **Connection drop without commit/rollback** | → bitmap via `Drop` impl on `ConnectionTxn` | leaked (same as today) |
| **flush() / release_deferred_frees** | N/A | recycle_queue drained → bitmap |

## Implementation phases (16 tasks)

1. **T1** — `LocalPageBatch` struct + constants. Pure data.
2. **T2** — Trait methods with default impls. `cargo check --workspace`.
3. **T3** — `FreeList::alloc_batch_sequential` + `free_batch`. Unit tests.
4. **T4** — `MmapStorage` fields: `recycle_queue` (SegQueue) + `extension_waiters` (AtomicU32). Compile.
5. **T5** — `MmapStorage::alloc_page_batch` + `free_page_batch` + recycle fold-back. Storage integration tests.
6. **T6** — `LocalPageBatch::pop_or_refill` + `take_for_commit` / `take_for_rollback`.
7. **T7** — `ExecutionContext::alloc_page` / `free_page` wrappers.
8. **T8** — Wire `heap_chain_write::insert{,_with_hint}`.
9. **T9** — Wire `axiomdb-index::tree_insert` split paths.
10. **T10** — Wire `axiomdb-index::tree_delete` rotate/merge paths.
11. **T11** — Wire `clustered_tree::btree_insert` + `mod::insert`.
12. **T12** — Wire `clustered_overflow::write_chain`.
13. **T13** — Commit/rollback drain in `TxnManager` + `Savepoint` extension.
14. **T14** — Concurrent stress: 8 × 10K alloc+free.
15. **T15** — Criterion benchmark. Verify ≥5× speedup.
16. **T16** — Closing protocol (test, clippy, fmt, docs, commit, push).

## Tests to write

### Unit
- `alloc_batch_sequential`: contiguous run, no run (non-contiguous), exhausted
- `free_batch`: single pass, double-free error
- `pop_or_refill`: basic pop, refill on empty, type mismatch, sticky bulk, adaptive

### Integration
- commit drains freed → recycle, available → bitmap
- rollback drains available → bitmap, keeps freed
- savepoint rollback truncates freed only
- recycle_queue fold-back during flush
- 8 threads × 10K alloc+free, all unique, no lost pages
- mixed page-type stress
- crash with non-empty recycle_queue → bitmap still consistent

### Bench
- `bench_alloc_page_batch_vs_per_page` — 8 threads × 10K allocs. Target: ≥5×.

## Anti-patterns to avoid

- ❌ `Mutex<VecDeque>` for recycle_queue — defeats lock-free draining. Use SegQueue.
- ❌ `Send + Sync` on `LocalPageBatch` — it's per-connection, exclusive ownership.
- ❌ Page init inside `alloc_page_batch` — callers do `Page::new + write_page`.
- ❌ `free_page_batch` on recycle_queue pages — already USED in bitmap; double-free.
- ❌ Changing `MmapStorage::alloc_page` semantics — DDL/vacuum/recovery keep using it.
- ❌ Fixing savepoint-rollback page leakage — out of scope for 40.9.

## Risks

| Risk | Mitigation |
|---|---|
| `release_deferred_frees` runs concurrently with `recycle_queue.pop()` | The fold-back path drains SegQueue under freelist mutex; phase-1 (lock-free pop) runs outside the mutex; after fold-back the queue is empty so phase-1 finds nothing |
| Connection aborts without commit/rollback leaks `available` | `Drop` impl on `ConnectionTxn` returns `available` to bitmap via `free_page_batch`. Log-and-forget on I/O error. |
| `extension_waiters` underflows on panic | Guard struct: `WaiterGuard { fetch_add in new(), fetch_sub in Drop }`. Panic-safe. |
| Adaptive sizing creates positive feedback loop | Capped at `MAX_BATCH_SIZE = 256`. Waiters decay naturally after the burst. |
| Type mismatch drains dominate on mixed workloads | One drain per INSERT statement boundary (heap→index). The heap benefit (~hundreds of pages) dominates. Defer per-type sub-batches until profiling shows otherwise. |
| Crash with non-empty recycle_queue leaks pages | Bounded by flush interval. Documented as "leaked until next bitmap scan or VACUUM". |
