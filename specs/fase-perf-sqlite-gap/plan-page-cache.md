# Plan: page-cache

Phase: perf-sqlite-gap — close embedded read gap with SQLite
Task: wire BufferPool into MmapStorage::read_page (Arc-backed PageRef)
Spec: specs/fase-perf-sqlite-gap/spec-page-cache.md
Status: done

## Summary

Five steps, each a clean compilable commit. Order is bottom-up so the workspace
keeps building after every step: (1) make `PageRef` cheap to clone by backing it
with `Arc<Page>`; (2) rewrite `BufferPool` to store `Arc<Page>` with O(1)
clock-sweep eviction; (3) wire the pool into `MmapStorage::read_page` and add
invalidation on write + alloc-reuse; (4) prove the `PageRef` change broke no
caller via the full workspace suite on Lima; (5) wire-smoke + bench to confirm
`point_lookup` improves and `full_scan` does not regress. Steps 1–3 are
unit-testable on the `axiomdb-storage` crate alone.

## Concurrency correctness (read before Step 3)

There is no global `RwLock<Database>` after Phase 40; page access is coordinated
by the per-page `PageLockTable` (S/X latches) — `std::sync::RwLock`, which is
**not** re-entrant for reads. The clustered descent already holds
`page_lock_table().read(pid)` *around* its `read_page(pid)` call
([`search.rs:18-20`](crates/axiomdb-storage/src/clustered_tree/search.rs:18)).
Therefore:

- `read_page` must **NOT** take the page latch itself (would double-lock the
  descent's read guard → deadlock under writer preference).
- The pool inherits the existing latch coordination: an in-place writer holds
  the page **X**-latch during `write_page` → `pwrite` → `invalidate`; a racing
  reader holds the **S**-latch during `read_page` → load → `insert`. X and S on
  the same page are mutually exclusive, so a miss-path insert can never
  interleave with a writer's pwrite+invalidate for the same page. This is the
  same invariant Phase 40.8 (btree-latch-coupling) already relies on.
- Cache hits need no latch: they `Arc::clone` an immutable page; staleness is
  bounded by the writer's `invalidate` (which runs under the X-latch).

Residual risk (an *unlatched* `read_page` caller racing an in-place writer to
the same page could re-cache stale bytes) is covered by a coherence regression
test + the workspace concurrency stress tests (Step 4). See Risk register.

## Dependencies

Must be done first:
- [x] spec-page-cache approved

Blocks (until this plan is done):
- [ ] Optional LeafCursorHint fast-path-free follow-up (its `read_page(leaf)`
      becomes a pool hit automatically once this lands)

## Affected files

Modified files:
- `crates/axiomdb-storage/src/page_ref.rs` — `Box<Page>` → `Arc<Page>`,
  `Clone`, `from_arc`, new `into_page`
- `crates/axiomdb-storage/src/buffer_pool.rs` — store `Arc<Page>`, clock-sweep
  eviction, drop pin/unpin API, update tests
- `crates/axiomdb-storage/src/mmap.rs` — `buffer_pool` field; `read_page` pool
  check; `write_page_inner` / `alloc_page` / `alloc_page_batch` invalidation;
  `buffer_pool_stats` accessor
- `tools/wire-test.py` — add point-lookup correctness assertions (Step 5)

No new files. No `StorageEngine` trait signature change.

---

## Step 1 — Arc-backed PageRef

**Goal:** `PageRef::clone()` is O(1); `into_page()` moves when sole owner, copies when shared.
**Files:** `crates/axiomdb-storage/src/page_ref.rs`
**Approach:** TDD — write failing tests, then flip `Box` → `Arc`.

### Test to add

```rust
// in page_ref.rs #[cfg(test)] mod tests
#[test]
fn clone_is_shared_not_copied() {
    let pr = PageRef::from_bytes([0u8; crate::page::PAGE_SIZE]);
    let pr2 = pr.clone();
    // Both observe the same page; clone did not deep-copy into a new Box.
    assert_eq!(pr.header().page_id, pr2.header().page_id);
    assert_eq!(PageRef::strong_count_for_test(&pr), 2);
}

#[test]
fn into_page_moves_when_sole_owner() {
    let pr = PageRef::from_bytes([0u8; crate::page::PAGE_SIZE]);
    let _page: Page = pr.into_page(); // no panic, no extra clone needed
}

#[test]
fn into_page_copies_when_shared() {
    let pr = PageRef::from_bytes([0u8; crate::page::PAGE_SIZE]);
    let pr2 = pr.clone();
    let _p1 = pr.into_page();   // shared (pr2 alive) → falls back to clone
    let _p2 = pr2.into_page();  // now sole owner → moves
}
```

### Implementation outline

```rust
use std::sync::Arc;
pub struct PageRef { inner: Arc<Page> }

impl PageRef {
    pub fn new(page: Box<Page>) -> Self { Self { inner: Arc::from(page) } }
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self { /* build Box as today, Arc::from */ }
    pub fn from_arc(page: Arc<Page>) -> Self { Self { inner: page } }
    pub fn into_page(self) -> Page {
        Arc::try_unwrap(self.inner).unwrap_or_else(|arc| (*arc).clone())
    }
    #[cfg(test)]
    pub fn strong_count_for_test(p: &Self) -> usize { Arc::strong_count(&p.inner) }
}
impl Clone for PageRef { fn clone(&self) -> Self { Self { inner: Arc::clone(&self.inner) } } }
impl Deref for PageRef { type Target = Page; fn deref(&self) -> &Page { &self.inner } }
```

Requires `Page: Clone` (for the `into_page` shared fallback). Verify `Page`
already derives/impls `Clone`; if not, add it (it is a `repr(C)` byte-array
wrapper — derive is trivial). `Arc::from(Box<Page>)` reuses the existing heap
allocation (no copy).

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage page_ref
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): Arc-back PageRef for O(1) clone

Step 1 of specs/fase-perf-sqlite-gap/plan-page-cache.md
```

---

## Step 2 — BufferPool stores Arc<Page>, clock-sweep eviction

**Goal:** `get` returns `Arc<Page>`, eviction is O(1), no manual pin/unpin.
**Files:** `crates/axiomdb-storage/src/buffer_pool.rs`
**Approach:** TDD — rewrite tests for the new API, then the impl.

### Test to add (replace existing pin/unpin tests)

```rust
#[test]
fn hit_returns_shared_page() {
    let pool = BufferPool::with_capacity(16);
    pool.insert(42, Arc::new(make_page(42)));
    let got = pool.get(42).expect("hit");
    assert_eq!(got.header().page_id, 42);
    assert_eq!(pool.stats(), (1, 0));
}

#[test]
fn miss_counts() {
    let pool = BufferPool::with_capacity(16);
    assert!(pool.get(99).is_none());
    assert_eq!(pool.stats(), (0, 1));
}

#[test]
fn clock_sweep_evicts_unreferenced_only() {
    // capacity 2 per shard; 3 pages to same shard; the never-`get`-ed one goes.
    let pool = BufferPool::with_capacity(NUM_SHARDS * 2);
    pool.insert(0, Arc::new(make_page(0)));
    pool.insert(16, Arc::new(make_page(16)));
    let _keep = pool.get(0); // reference bit set on 0 → second chance
    pool.insert(32, Arc::new(make_page(32)));
    assert!(pool.get(0).is_some());   // survived (was referenced)
    assert!(pool.get(32).is_some());  // newest
}

#[test]
fn never_evicts_externally_referenced() {
    let pool = BufferPool::with_capacity(NUM_SHARDS); // 1 per shard
    let live = pool.insert(0, Arc::new(make_page(0))); // hold the Arc
    pool.insert(16, Arc::new(make_page(16)));          // would evict 0, but strong_count>1
    assert!(pool.get(0).is_some());
    drop(live);
}

#[test]
fn invalidate_removes() {
    let pool = BufferPool::with_capacity(16);
    pool.insert(7, Arc::new(make_page(7)));
    pool.invalidate(7);
    assert!(pool.get(7).is_none());
}
```

`make_page` returns `Page` (not `PageRef`) now: `Page::new(PageType::Data, id)`.

### Implementation outline

```rust
struct CacheEntry { page: Arc<Page>, referenced: bool }
struct CacheShard {
    entries: HashMap<u64, CacheEntry>,
    clock: Vec<u64>,     // page_ids in insertion order; hand walks this
    hand: usize,
    capacity: usize,
    hits: u64, misses: u64,
}
// get: entry.referenced = true; hits += 1; Some(Arc::clone(&entry.page))
// insert: insert entry{page, referenced:false}; push id to clock; evict_if_needed
// evict_if_needed: while entries.len() > capacity { sweep() or break if none evictable }
// sweep: advance hand over clock; for the id at hand:
//   - missing entry → remove from clock (compact)
//   - referenced → clear bit, advance (second chance)
//   - !referenced && Arc::strong_count(&page)==1 → evict (remove from entries+clock)
//   - !referenced && strong_count>1 → skip (still in use)
//   bail out after one full revolution with nothing evictable (transient over-capacity OK)
// insert(id) returns Arc::clone of the stored page
// REMOVE: pin_count, unpin()
```

Keep the 16-shard partitioning and the `BufferPool` outer API
(`new`/`with_capacity`/`stats`); only the entry type, `get`/`insert` signatures,
and eviction change. Update the module doc: it is now wired into `read_page`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage buffer_pool
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): BufferPool Arc<Page> + O(1) clock-sweep

Step 2 of specs/fase-perf-sqlite-gap/plan-page-cache.md
```

---

## Step 3 — Wire BufferPool into MmapStorage::read_page + invalidation

**Goal:** hits served from RAM (no mmap lock/copy/CRC); writes + alloc-reuse invalidate.
**Files:** `crates/axiomdb-storage/src/mmap.rs`
**Approach:** TDD — coherence + hit-counting tests, then wire.

### Test to add

```rust
// in mmap.rs #[cfg(test)] (uses tempfile + MmapStorage::create)
#[test]
fn second_read_is_cache_hit() {
    let dir = tempfile::tempdir().unwrap();
    let s = MmapStorage::create(&dir.path().join("t.db")).unwrap();
    let pid = s.alloc_page(PageType::Data).unwrap();
    let _ = s.read_page(pid).unwrap();          // miss → populates
    let _ = s.read_page(pid).unwrap();          // hit
    let (hits, _misses) = s.buffer_pool_stats();
    assert!(hits >= 1, "expected a cache hit");
}

#[test]
fn write_then_read_sees_fresh_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let s = MmapStorage::create(&dir.path().join("t.db")).unwrap();
    let pid = s.alloc_page(PageType::Data).unwrap();
    let _ = s.read_page(pid).unwrap();          // cache the old contents
    let mut p = Page::new(PageType::Data, pid);
    p.body_mut()[0] = 0xAB;
    p.update_checksum();
    s.write_page(pid, &p).unwrap();             // must invalidate
    let got = s.read_page(pid).unwrap();
    assert_eq!(got.body()[0], 0xAB);
}

#[test]
fn free_then_realloc_does_not_serve_stale() {
    let dir = tempfile::tempdir().unwrap();
    let s = MmapStorage::create(&dir.path().join("t.db")).unwrap();
    let pid = s.alloc_page(PageType::Data).unwrap();
    let _ = s.read_page(pid).unwrap();          // cache it
    s.free_page(pid).unwrap();
    s.release_deferred_frees(u64::MAX).unwrap();
    let pid2 = s.alloc_page(PageType::Data).unwrap(); // likely same id
    let got = s.read_page(pid2).unwrap();
    // freshly alloc'd page header reflects the new alloc, not stale cached bytes
    assert_eq!(got.header().page_id, pid2);
}
```

### Implementation outline

```rust
// struct field:
buffer_pool: BufferPool,
// in create() and open() struct literals:
buffer_pool: BufferPool::new(),

// read_page:
fn read_page(&self, page_id: u64) -> Result<PageRef, DbError> {
    if page_id >= self.page_count.load(Ordering::Acquire) {
        return Err(DbError::PageNotFound { page_id });
    }
    if let Some(page) = self.buffer_pool.get(page_id) {
        return Ok(PageRef::from_arc(page)); // hit: no mmap lock, no copy, no CRC
    }
    let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
    let page_ref = Self::read_page_from_mmap(&mmap, page_id)?; // copy + CRC verify
    drop(mmap);
    let arc = self.buffer_pool.insert(page_id, Arc::new(page_ref.into_page()));
    Ok(PageRef::from_arc(arc))
}

// write_page_inner: after pwrite_page(page_id, page)? succeeds and before/after dirty.mark:
self.buffer_pool.invalidate(page_id);

// alloc_page: just before `Ok(page_id)` (both freelist-reuse and grow returns):
self.buffer_pool.invalidate(page_id);
// alloc_page_batch: invalidate each id in the returned vec before returning.

// new accessor:
pub fn buffer_pool_stats(&self) -> (u64, u64) { self.buffer_pool.stats() }
```

Note: `read_page_from_mmap` returns a `PageRef`; to avoid an extra copy, either
add a small helper that returns the `[u8; PAGE_SIZE]`+verifies and build the
`Arc<Page>` once, or accept the single `into_page()` move (sole owner → no
copy). Prefer the move (`into_page` on a just-built sole-owner PageRef is free).

Audit `alloc_page` for **all** return paths (freelist hit + grow path) so every
handed-out id is invalidated. `write_page_under_page_lock` already routes
through `write_page_inner`, so the single invalidate there covers both public
write entry points.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): serve read_page from BufferPool

Step 3 of specs/fase-perf-sqlite-gap/plan-page-cache.md
```

---

## Step 4 — Workspace integration (PageRef change blast radius)

**Goal:** confirm the `Box`→`Arc` `PageRef` change broke none of the ~41
`into_page()` callers or any `read_page` caller; confirm coherence under
concurrency.
**Files:** none (verification only; fix fallout if any).

### Verification

```bash
./tools/vm.sh test --workspace          # run ALONE (concurrent load flakes mmap/btree tests)
./tools/vm.sh clippy --workspace -- -D warnings
./tools/vm.sh fmt-check
```

If a caller breaks: most are `let mut page = storage.read_page(pid)?.into_page();`
which still type-checks (signature unchanged). The only behavioral change is a
possible 16 KB copy when `into_page()` is called on a page still resident in the
pool — correct, just costs one copy on the write path. Fix any genuine breakage
in this step before committing.

### Commit (only if fixes were needed)

```
fix(perf-sqlite-gap): adapt callers to Arc-backed PageRef

Step 4 of specs/fase-perf-sqlite-gap/plan-page-cache.md
```

---

## Step 5 — Wire smoke + bench measurement

**Goal:** correctness over the wire + confirm the perf budget.
**Files:** `tools/wire-test.py` (add point-lookup assertions).

### Wire smoke (pre-flight is mandatory)

```bash
pkill -f axiomdb-server || true
cargo build -p axiomdb-server --release         # macOS binary for wire test
rm -f target/release/axiomdb-server.stale 2>/dev/null || true
python3 tools/wire-test.py                       # point lookups return correct rows
```

Add assertions: insert N rows, `SELECT * ... WHERE id = k` for several k, assert
exact row + values (regression guard for the cache serving correct bytes).

### Engine bench (pure Rust, the true engine signal)

```bash
cargo build --release -p axiomdb-bench-comparison
target/release/axiomdb_bench --compare --rows 10000
```

Record `point_lookup` and `full_scan` ratios before/after. Budget (from spec):
- `point_lookup` ≥ 25K ops/s (was ~13–14K), no regression.
- `full_scan` within 5% of today (~1.0× SQLite must hold).
- `select_where` / `range_scan` not regressed.

If `full_scan` regresses > 5%: investigate clock-sweep overhead under scan churn
(scan pollution) before closing.

### Verification against spec done-criteria

- [ ] `PageRef` Arc-backed, O(1) clone, move-or-copy `into_page`
- [ ] `BufferPool::get` → `Arc<Page>`, O(1) eviction, no `unpin`
- [ ] `read_page` hit path: no mmap lock / copy / CRC
- [ ] write + alloc-reuse invalidate
- [ ] all spec edge cases tested
- [ ] workspace test/clippy/fmt clean (Lima)
- [ ] wire smoke passes
- [ ] bench: point_lookup up, full_scan flat

### Final commit

```
feat(perf-sqlite-gap): page cache for read_page (point_lookup)

Implements specs/fase-perf-sqlite-gap/spec-page-cache.md
Plan: specs/fase-perf-sqlite-gap/plan-page-cache.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Unlatched `read_page` caller races in-place writer → stale entry persists | low | Coherence test (Step 3 `write_then_read`); workspace concurrency stress (Step 4); rely on Phase 40.8 latch-coupling invariant (documented above) |
| `full_scan` regresses from clock-sweep churn (scan pollution) | medium | O(1) sweep keeps per-access cost flat; measure in Step 5; if seen, add scan-bypass or midpoint admission (out of scope, follow-up) |
| `Page` not `Clone` | low | derive `Clone` (trivial byte-array wrapper) in Step 1 |
| `into_page()` now copies when page is pool-resident (write paths) | low | one 16 KB copy on the write path only; writes are not the budget here; acceptable |
| alloc grow-path return not invalidated | low | audit all `alloc_page` returns in Step 3; test `free_then_realloc` |

## Rollback plan

1. `git reset --hard <commit before Step 1>` — the change is contained to
   `axiomdb-storage` (Steps 1–3); Steps 4–5 are verification.
2. Or leave partial work on `abandoned/plan-page-cache-2026-05-20`.
3. Set spec status back to `draft` with a note on what failed.

## Estimated effort

Effort level: **high** (hot-path data-structure change, ~41 `into_page` callers,
concurrency-correctness reasoning).
Total: ~1 day.
- Step 1: 1h · Step 2: 2h · Step 3: 2h · Step 4: 1–2h (fallout) · Step 5: 1h
