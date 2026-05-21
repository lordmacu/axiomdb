# Spec: page-cache — wire BufferPool into read_page with Arc-backed PageRef

Phase: perf-sqlite-gap — close embedded read gap with SQLite
Task: Wire the existing `BufferPool` (Phase 11.8) into `MmapStorage::read_page`
so repeated B-tree page reads (root + internal nodes on every point lookup)
are served from RAM as `Arc::clone` instead of a 16 KB copy + CRC32c per read.
Status: approved

## Context

Every `StorageEngine::read_page` call in
[`mmap.rs:558`](crates/axiomdb-storage/src/mmap.rs:558) goes straight to
`read_page_from_mmap`: it takes the mmap read lock, **copies 16 KB** out of
the mapping into a fresh `Box<Page>`, and **verifies the CRC32c** of the body
([`mmap.rs:451`](crates/axiomdb-storage/src/mmap.rs:451)). A clustered PK point
lookup descends root → internal → leaf (`descend_to_leaf`,
[`search.rs:10`](crates/axiomdb-storage/src/clustered_tree/search.rs:10)),
paying that copy+CRC at **every level, on every query**. The root and upper
internal nodes are identical across all lookups yet are re-copied and re-CRC'd
each time.

A `BufferPool` (16-shard partitioned LRU) already exists in
[`buffer_pool.rs`](crates/axiomdb-storage/src/buffer_pool.rs) from Phase 11.8
but **is not wired into any production read path** — it has tests only. The
prior cursor-reuse work
([`spec-cursor-reuse-cross-statement.md`](specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md))
explicitly deferred the "multi-leaf cache (LRU per table)" as *Approach B*;
this spec is that follow-up.

SQLite never copies on a cache hit: `sqlite3PcacheFetch`
([`research/sqlite/src/pcache.c`](research/sqlite/src/pcache.c)) returns a
`PgHdr*` pointing into the existing page buffer and increments `nRef`. The
`BtCursor` keeps the whole root→leaf path pinned (`apPage[]`,
[`btreeInt.h:556`](research/sqlite/src/btreeInt.h:556)). SQLite computes a page
checksum **only** for WAL/journal frame validation — never on a normal page
read. We mirror this with a verify-once-then-serve model: CRC on the cold load
from mmap, then serve the verified page from the pool with no re-CRC.

## Goal

On a buffer-pool hit, `read_page` returns the cached page via an `Arc::clone`
(no mmap lock, no 16 KB copy, no CRC), so repeated B-tree descents reuse hot
internal pages instead of re-materializing them.

## Non-goals

- **Stateful B-tree cursor that pins the whole root→leaf path** (SQLite
  `apPage[]` analog). Bigger refactor; revisit after measuring this change.
- **Skipping SQL parse / plan rebuild per query** (PreparedStatement API).
  That is Task 4 of the sprint, a separate spec.
- **young/old or scan-resistant admission** (InnoDB midpoint insertion).
  Plain bounded cache here; add scan resistance only if `full_scan` regresses.
- **Caching `read_page_raw`** (the raw `[u8; PAGE_SIZE]` path used by
  doublewrite/recovery). Stays a direct mmap copy — out of scope.
- **Persisting or warming the cache across process restarts.** In-memory only.
- **Changing the `StorageEngine` trait signature.** `read_page` keeps
  returning `PageRef`; only `PageRef`'s internals change.

## Behavior

### Public API

`PageRef` switches its backing store from `Box<Page>` to `Arc<Page>` and gains a
cheap `Clone`. The `Deref<Target = Page>` and the public method surface are
unchanged for read callers.

```rust
// crates/axiomdb-storage/src/page_ref.rs
pub struct PageRef {
    inner: Arc<Page>,            // was: Box<Page>
}

impl PageRef {
    pub fn new(page: Box<Page>) -> Self;            // unchanged signature; wraps into Arc
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self; // unchanged signature
    pub fn from_arc(page: Arc<Page>) -> Self;       // NEW: wrap a shared page (pool hit), no copy

    /// Owned page for mutation. If this is the only holder, moves out of the
    /// Arc (no copy); if the page is still shared (e.g. live in the pool),
    /// clones the 16 KB once.
    pub fn into_page(self) -> Page;                 // unchanged signature, new impl
}

impl Clone for PageRef {                            // NEW: O(1) Arc::clone
    fn clone(&self) -> Self;
}

impl Deref for PageRef { type Target = Page; /* unchanged */ }
```

`BufferPool` stores `Arc<Page>` directly (not `Arc<PageRef>`, which would double
the indirection) and switches eviction from the O(capacity) `VecDeque::position`
LRU to an **O(1) clock-sweep** that relies on `Arc::strong_count` for liveness
(no manual pin/unpin API).

```rust
// crates/axiomdb-storage/src/buffer_pool.rs
impl BufferPool {
    pub fn new() -> Self;                                   // unchanged
    pub fn with_capacity(total_pages: usize) -> Self;       // unchanged

    /// Hit → Arc::clone of the cached page; sets the entry's reference bit.
    pub fn get(&self, page_id: u64) -> Option<Arc<Page>>;   // return type Arc<Page>

    /// Insert a freshly-loaded (already CRC-verified) page.
    pub fn insert(&self, page_id: u64, page: Arc<Page>) -> Arc<Page>;

    /// Drop a cached entry (called after write_page / on free). Idempotent.
    pub fn invalidate(&self, page_id: u64);

    pub fn stats(&self) -> (u64, u64);                      // (hits, misses), unchanged
}
// REMOVED: pub fn unpin(&self, page_id: u64)  — replaced by Arc::strong_count liveness
```

`MmapStorage` gains a `buffer_pool: BufferPool` field. `read_page` consults it
first; `write_page`/free invalidate it.

### Semantics

`read_page(page_id)`:
- Precondition: `page_id < page_count` (existing bounds check stays first).
- Hit: return `PageRef::from_arc(Arc::clone(cached))`. No mmap lock, no copy,
  no CRC.
- Miss: take the mmap read lock, copy 16 KB, **verify CRC32c**, build
  `Arc<Page>`, `pool.insert`, return a `PageRef` wrapping it.
- Postcondition: returned page bytes equal the on-disk page at call time
  (modulo concurrent writers, same as today).
- Invariant: any page resident in the pool has passed CRC verification at
  insert time (verify-once-then-serve).

`write_page(page_id, page)` / `write_page_under_page_lock`:
- After the successful `pwrite`, call `pool.invalidate(page_id)` so the next
  read reloads fresh bytes. Postcondition: no stale page survives a write.

Page free / reuse: when a page is handed back for reuse (freelist alloc or
deferred-free release), its `page_id` must be invalidated so a realloc never
serves stale cached bytes. (Exact hook resolved in Open questions / plan.)

`into_page()`: `Arc::try_unwrap(inner)` on success (sole owner — the common
write-path case); on failure (page still shared, e.g. resident in pool) fall
back to `(*inner).clone()` (one 16 KB copy). Mutating the returned `Page` never
affects the cached copy.

Eviction (clock-sweep): on insert over capacity, the clock hand advances over
entries; an entry with its reference bit set has the bit cleared and is
skipped; the first entry with a clear reference bit **and**
`Arc::strong_count == 1` (only the pool holds it) is evicted. Entries still
referenced elsewhere are never evicted (they are simply skipped).

### Error cases

| Input | Expected error | Message |
|-------|----------------|---------|
| `page_id >= page_count` | `DbError::PageNotFound { page_id }` | (existing) |
| Cold-load CRC mismatch | `DbError::ChecksumMismatch { page_id, expected, got }` | (existing, from `verify_checksum`) |
| Pool hit | never errors | — |

A corrupt page must still surface `ChecksumMismatch` on the cold load; it must
never be inserted into the pool, so a hit can never serve unverified bytes.

## Edge cases

- [ ] Read same page twice in a row → second read is a hit (no mmap lock taken).
- [ ] Write a page, then read it → reader sees the new bytes (invalidate works).
- [ ] Concurrent reads of the same page from two threads → both get clones of
      the same `Arc<Page>`; immutable, no data race.
- [ ] Concurrent read + write of the same page → behavior matches today
      (no new guarantee; invalidate happens under the page write lock path).
- [ ] `into_page()` on a page that is also resident in the pool → copies once,
      caller's mutation does not corrupt the cached page.
- [ ] Free a page, alloc it again with different contents → read returns the new
      contents, never the stale cached page.
- [ ] mmap grow/remap while a `PageRef` is held → safe (PageRef owns its
      `Arc<Page>` copy, independent of the mapping) — unchanged from today.
- [ ] Cache capacity exceeded by a full table scan → eviction stays O(1) per
      page; `full_scan` throughput does not regress.
- [ ] Pool full of pages all still referenced (strong_count > 1) → eviction
      skips them, cache may transiently exceed capacity, no deadlock/spin.
- [ ] Corrupt on-disk page → `ChecksumMismatch` on first read, page not cached.

Each becomes a test in `/plan-task`.

## On-disk format

None. This change is purely in-memory; the page byte layout, CRC field, and
file format are untouched.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `read_page` cache hit | ≤ 50 ns | 100 ns |
| `read_page` cache miss | ~ today (copy + CRC) | +5% over today |
| `point_lookup` (bench, 10K rows) | ≥ 25K ops/s | no regression vs ~13–14K |
| `full_scan` (bench, 10K rows) | ~1.0× SQLite (today) | no regression > 5% |
| `select_where` / `range_scan` | ≥ today | no regression |

Reference: checkpoint table — `point_lookup` ~13–14K ops/s (~5.6–6× slower than
SQLite ~77K); `full_scan` ~1.0× must hold. This task is one of two levers
(PreparedStatement is the other); full ~1× parity is not expected from the page
cache alone.

## Dependencies

- Depends on: existing `BufferPool` (Phase 11.8), `PageRef`, `MmapStorage`.
- Blocks: optional follow-up that makes `LeafCursorHint`'s fast path free
  (its `read_page(leaf)` becomes a pool hit automatically once this lands).
- Independent of: PreparedStatement API (sprint Task 4).

## Open questions (resolved at approval)

- [x] **Free/reuse invalidation hook.** RESOLVED: invalidate on **both** alloc
      and free (fail-safe). `free`/`release_deferred_frees` invalidates when a
      page leaves service; `alloc_page` invalidates the id just handed out so a
      realloc never serves stale cached bytes. Exact line-level hooks confirmed
      against `mmap.rs` during `/plan-task`.
- [x] **Always-on vs capacity tuning.** RESOLVED: always-on, keep default 1024
      pages (16 MB). Acceptable for the embedded single-file use case; revisit
      only if memory pressure is observed.
- [x] **Clock-sweep vs keep-LRU-but-make-get-O(1).** RESOLVED: clock-sweep
      (PostgreSQL `bufmgr.c` style) — guarantees O(1) `get` so `full_scan`
      cannot regress from per-access LRU bookkeeping.

## Done criteria

- [ ] `PageRef` backed by `Arc<Page>`; `Clone` is O(1); `into_page()` moves when
      sole owner, copies when shared.
- [ ] `BufferPool::get` returns `Arc<Page>`, eviction is O(1), no `unpin` API.
- [ ] `MmapStorage::read_page` serves hits from the pool (no mmap lock / copy /
      CRC on hit); misses verify CRC and populate the pool.
- [ ] `write_page` and the free/reuse path invalidate the pool entry.
- [ ] All edge cases above have a test.
- [ ] `cargo nextest run -p axiomdb-storage` passes (Lima VM).
- [ ] `cargo nextest run --workspace` passes (Lima VM) — no caller of
      `read_page`/`into_page` broken by the `PageRef` change.
- [ ] `cargo clippy --workspace -- -D warnings` clean (Lima VM).
- [ ] `cargo fmt --check` clean.
- [ ] Wire smoke test (`tools/wire-test.py`) passes — point lookups over the
      wire return correct rows.
- [ ] Bench: `axiomdb_bench --compare --rows 10000` shows `point_lookup`
      improved and `full_scan` not regressed.
- [ ] rustdoc on every changed public item; `buffer_pool.rs` module doc updated
      to reflect it is now wired into `read_page`.

## References

- Prior related spec: [`spec-cursor-reuse-cross-statement.md`](specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md)
  (deferred the multi-page LRU as Approach B — this is it)
- Checkpoint: [`docs/checkpoint-sqlite-parity.md`](docs/checkpoint-sqlite-parity.md)
  (point_lookup diagnosis, verify-once-then-serve)
- SQLite: `research/sqlite/src/pcache.c` (`sqlite3PcacheFetch` zero-copy hit),
  `research/sqlite/src/btreeInt.h:556` (`apPage[]` pinned path),
  `research/sqlite/src/pager.c` (checksum only for journal frames)
- PostgreSQL: `research/postgres` `bufmgr.c` (clock-sweep replacement)
