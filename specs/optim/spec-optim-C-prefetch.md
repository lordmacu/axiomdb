# Spec: Sequential Scan Prefetch Hint (optim-C)

## What to build (not how)

A `prefetch_hint` method on the `StorageEngine` trait that allows callers to
signal intent to read a contiguous range of pages sequentially. The method has
a default no-op implementation so all existing backends compile unchanged.
`MmapStorage` overrides it to call `madvise(MADV_SEQUENTIAL)` on the page range,
which instructs the OS kernel to begin async read-ahead for the pages that follow
the first fault. `HeapChain::scan_visible()`, `scan_rids_visible()`, and
`delete_batch()` each call it once at the start of their scan loop.

The behavioral contract: prefetch_hint is a pure hint. It must never fail
visibly. If the range is partially or fully out of file bounds, the backend
clamps silently. If the syscall is unavailable or unsupported on the current
platform, the method is a no-op.

## Inputs / Outputs

**Trait method signature (added to `StorageEngine`)**

```rust
/// Hint to the storage backend that pages starting at `start_page_id` will be
/// read sequentially. The backend may prefetch `count` pages ahead
/// (0 = use backend-defined default). Implementations that do not support
/// prefetch must provide a no-op default.
fn prefetch_hint(&self, start_page_id: u64, count: u64) {}
```

- Input `start_page_id`: logical page ID of the first page in the sequential range.
- Input `count`: number of pages to hint. `0` means "I don't know the chain
  length; use the backend default." `MmapStorage` default: 64 pages (1 MB at
  16 KB page size).
- Output: `()` — no return value, no error surface.
- Errors: none. Hint failures are silently ignored at every layer.

**Call sites in HeapChain**

| Method | Call |
|---|---|
| `scan_visible()` | `storage.prefetch_hint(root_page_id, 0)` before the loop |
| `scan_rids_visible()` | `storage.prefetch_hint(root_page_id, 0)` before the loop |
| `delete_batch()` | `storage.prefetch_hint(root_page_id, 0)` before the loop |

`root_page_id` is the first page of the heap chain. `count = 0` is always
passed because `HeapChain` does not know total chain length at call time.

## Use cases

1. **Cold cache sequential scan** — A SELECT scans a 100K-row table whose pages
   are not in the OS page cache. Without a hint each page fault stalls the CPU
   until the kernel issues a synchronous read. With `madvise(MADV_SEQUENTIAL)`
   the kernel reads ahead: when page N is first accessed, the kernel schedules
   async I/O for N+1, N+2, … so that those pages are in cache by the time the
   CPU finishes processing N. Result: overlapped disk I/O and CPU work.

2. **Warm cache sequential scan** — Pages are already in the OS page cache.
   `madvise(MADV_SEQUENTIAL)` is still a valid syscall; it simply has no visible
   effect because there is nothing to prefetch. No performance regression occurs.

3. **MemoryStorage backend** — `HashMap`-backed; no file descriptor, no mmap.
   The default no-op implementation is used. Unit tests and benchmarks run
   unchanged with zero overhead.

4. **Multiple consecutive scans** — Each call to `scan_visible()` issues its own
   independent hint (idempotent). Re-hinting the same region is harmless.

5. **Out-of-range hint** — `start_page_id + count` exceeds the current file size
   (e.g., count = 64 but only 10 pages exist). `MmapStorage` clamps `len` to
   `mmap_length - offset` before calling `madvise`. No panic, no error.

6. **count = 0 path** — Caller passes 0. `MmapStorage` substitutes
   `PREFETCH_DEFAULT_PAGES = 64`. This covers the common case where chain length
   is unknown at scan start.

## Acceptance criteria

- [ ] `StorageEngine` trait gains `fn prefetch_hint(&self, start_page_id: u64, count: u64) {}`
      with a default no-op body; all existing backends compile without changes.
- [ ] `MmapStorage::prefetch_hint` calls `madvise(addr, len, MADV_SEQUENTIAL)` on
      macOS and Linux (`#[cfg(any(target_os = "macos", target_os = "linux"))]`).
      On all other platforms the method is a no-op (cfg fallthrough to default).
- [ ] `madvise` errors (EAGAIN, EINVAL, etc.) are silently ignored — the method
      never propagates an OS error to the caller.
- [ ] `ptr` is computed as `mmap_base + start_page_id * PAGE_SIZE`; `len` is
      `min(count_or_default * PAGE_SIZE, mmap_len - offset)`. If `offset >=
      mmap_len` the call is skipped entirely.
- [ ] `HeapChain::scan_visible()` calls `storage.prefetch_hint(root_page_id, 0)`
      once, before the page-walk loop begins.
- [ ] `HeapChain::scan_rids_visible()` calls `storage.prefetch_hint(root_page_id, 0)`
      once, before the page-walk loop begins.
- [ ] `HeapChain::delete_batch()` calls `storage.prefetch_hint(root_page_id, 0)`
      once, before the page-walk loop begins.
- [ ] `PREFETCH_DEFAULT_PAGES` constant defined in `MmapStorage` with value `64`
      (1 MB at 16 KB page size); it is `pub(crate)` so benches can reference it.
- [ ] All 1152+ existing tests (`cargo test --workspace`) pass unchanged.
- [ ] `cargo clippy --workspace -- -D warnings` passes with no new warnings.
- [ ] No `unwrap()` added in production code paths (`src/`).
- [ ] Note: a measurable throughput improvement requires a cold-cache end-to-end
      test with 100K+ rows and a real MmapStorage backend; unit tests with
      MemoryStorage will not demonstrate the gain. Verifying the hint is issued is
      sufficient for the acceptance bar of this spec.

## Out of scope

- **Adaptive lookahead** (PostgreSQL `read_stream.c` dynamic distance adjustment):
  the distance is fixed at `PREFETCH_DEFAULT_PAGES`. Dynamic adjustment based on
  scan speed or I/O latency is deferred to a future optimization phase.
- **`posix_fadvise`**: macOS does not support `posix_fadvise` on mmap'd regions;
  `madvise` is the correct and portable call for the mmap case on both macOS and
  Linux. `posix_fadvise` would only apply to a `read()`-based backend.
- **Prefetch for random-access patterns**: point lookups (`read_page` by arbitrary
  ID) do not benefit from sequential hints. `prefetch_hint` is only called from
  sequential scan paths.
- **Prefetch for index scans**: B+ tree leaf traversal is a separate sequential
  access pattern not covered here. That path may be addressed in a dedicated
  index-scan optimization subphase.
- **Explicit prefetch scheduling / io_uring**: async I/O submission is not part of
  this spec; `madvise` delegates scheduling entirely to the OS kernel.
- **Exposing hint count as a user-configurable parameter**: `PREFETCH_DEFAULT_PAGES`
  is an internal constant, not a runtime setting.

## Dependencies

- `StorageEngine` trait (`crates/axiomdb-storage/src/engine.rs`) — must exist
  and be stable before this method is added. (Already exists as of Phase 4.)
- `MmapStorage` (`crates/axiomdb-storage/src/mmap_storage.rs`) — must hold a
  valid mmap base pointer and total mmap length to compute the address range.
- `HeapChain` (`crates/axiomdb-storage/src/heap_chain.rs`) — must have a reference
  to the `StorageEngine` implementor and know `root_page_id` at scan start. (Both
  are true as of Phase 4.)
- `PAGE_SIZE` constant — must be accessible in the address arithmetic
  (`crates/axiomdb-storage/src/lib.rs` or equivalent). (Already defined.)
- `libc` crate — required for `libc::madvise` and `libc::MADV_SEQUENTIAL`.
  Add to `[dependencies]` of `axiomdb-storage` if not already present.
