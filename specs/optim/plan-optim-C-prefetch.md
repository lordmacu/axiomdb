# Plan: Sequential Scan Prefetch Hint (optim-C)

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-storage/Cargo.toml` | modify | Add `libc = "0.2"` to `[dependencies]` |
| `crates/axiomdb-storage/src/engine.rs` | modify | Add `prefetch_hint(&self, start_page_id: u64, count: u64) {}` default no-op to trait |
| `crates/axiomdb-storage/src/mmap.rs` | modify | Add `PREFETCH_DEFAULT_PAGES = 64`; implement `prefetch_hint` with `madvise` + `#[cfg]` gate |
| `crates/axiomdb-storage/src/heap_chain.rs` | modify | Call `prefetch_hint` at start of `scan_visible`, `scan_rids_visible`, `delete_batch`; add `root_page_id: u64` param to `delete_batch` |
| `crates/axiomdb-sql/src/table.rs` | modify | Update `delete_batch` call (1 site): pass `table_def.data_root_page_id` |
| `crates/axiomdb-sql/src/executor.rs` | modify | Update `delete_batch` calls (2 sites): pass `resolved.def.data_root_page_id` |
| `crates/axiomdb-wal/src/recovery.rs` | modify | Update `delete_batch` call (1 site): pass `root_page_id` |
| `crates/axiomdb-wal/src/txn.rs` | modify | Update `delete_batch` calls (2 sites): pass `root_page_id` |

`memory.rs` requires **no changes** (default no-op is inherited).

---

## Algorithm / Data structure

### `madvise` address arithmetic

```
mmap_len        = self.mmap.len()
offset          = (start_page_id as usize).checked_mul(PAGE_SIZE)
                  → return early if overflow
if offset >= mmap_len: return

effective_count = if count == 0 { PREFETCH_DEFAULT_PAGES } else { count }
requested_len   = (effective_count as usize).saturating_mul(PAGE_SIZE)
clamped_len     = requested_len.min(mmap_len - offset)

ptr = self.mmap.as_ptr().add(offset)   // SAFETY: offset < mmap_len verified above
let _ = libc::madvise(ptr as *mut c_void, clamped_len, libc::MADV_SEQUENTIAL)
```

### SAFETY invariant for the `unsafe` block

- `ptr` is derived from a live `MmapMut` whose mapping outlives this call (guaranteed by `&self` borrow).
- `offset < mmap_len` verified before `add`.
- `clamped_len <= mmap_len - offset` so `[ptr, ptr+clamped_len)` lies entirely within the mapping.
- `madvise` does not read or write through the pointer — kernel hint only.
- No Rust aliasing rules violated: `&self` is shared immutable; `madvise` mutates no Rust state.

---

## Implementation phases (ordered)

1. **`Cargo.toml`** — Add `libc = "0.2"`. `cargo check -p axiomdb-storage` compiles.
2. **`engine.rs`** — Add `prefetch_hint` default no-op. `cargo test --workspace` passes (zero callers change).
3. **`mmap.rs`** — Add constant + `prefetch_hint` implementation + `use libc`. `cargo clippy --workspace -- -D warnings` passes.
4. **`heap_chain.rs`** — Add `root_page_id` to `delete_batch` signature + hint calls in all 3 scan functions. Fix test call sites in same file. `cargo test -p axiomdb-storage` passes.
5. **Call sites** — Update 6 `delete_batch` callers in `axiomdb-sql` and `axiomdb-wal`. `cargo build --workspace` compiles clean.
6. **Tests** — Add unit tests in `engine.rs` and `mmap.rs`; integration tests in `heap_chain.rs`.
7. **Benchmark** — record before/after on cold-cache scenario.

---

## Tests to write

### `crates/axiomdb-storage/src/engine.rs` — unit tests

- `test_prefetch_hint_noop_on_memory_storage` — call on `MemoryStorage` with 0, 64, large IDs, u64::MAX → no panic.

### `crates/axiomdb-storage/src/mmap.rs` — unit tests

- `test_prefetch_hint_count_zero_uses_default` — count=0 → exercises `PREFETCH_DEFAULT_PAGES` branch, no panic.
- `test_prefetch_hint_out_of_range_clamped` — `start_page_id` beyond file end → early return, no panic; `count = u64::MAX` → saturating clamp, no panic.
- `test_prefetch_hint_start_at_last_page` — exactly last valid page, large count → clamped to 1 page, no panic.

### `crates/axiomdb-storage/src/heap_chain.rs` — integration tests

- `test_scan_visible_with_mmap_calls_prefetch_hint` — insert 20 rows into MmapStorage, scan → correct 20 rows returned, no panic.
- `test_scan_rids_visible_with_mmap_calls_prefetch_hint` — same, using `scan_rids_visible`.

---

## Anti-patterns to avoid

- **DO NOT** propagate `madvise` return value. Always `let _ = ...`.
- **DO NOT** call `prefetch_hint` from random-access paths (insert, B+ tree, point lookup).
- **DO NOT** make `prefetch_hint` take `&mut self` — `scan_visible` holds `&dyn StorageEngine`.
- **DO NOT** place the hint inside the scan loop — one call before `while` is correct.
- **DO NOT** add `libc` to `[workspace.dependencies]` — only `axiomdb-storage` uses it.
- **DO NOT** use `start_page_id as usize * PAGE_SIZE` directly — use `checked_mul` to guard 32-bit overflow.

---

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| `madvise` unavailable on non-POSIX targets | Low | `#[cfg(any(target_os = "linux", target_os = "macos"))]` gate; trait default no-op is fallback |
| `*const u8` → `*mut c_void` cast raises clippy lint | Low | Cast is explicit `ptr as *mut libc::c_void`; SAFETY comment explains it |
| 6 `delete_batch` call sites must be updated | Low | All have root page ID in scope; mechanical change; compiler catches omissions |
| Hint on chains shorter than default 64 pages | None | `clamped_len` clamps to file bounds; over-hinting is harmless |
