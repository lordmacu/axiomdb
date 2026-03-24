# Plan: All-Visible Page Flag (Optimization A)

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-storage/src/page.rs` | modify | Add `PAGE_FLAG_ALL_VISIBLE = 0x01` constant + `is_all_visible()`, `set_all_visible()`, `clear_all_visible()` methods |
| `crates/axiomdb-storage/src/heap.rs` | modify | `mark_deleted()` clears flag before stamping `txn_id_deleted` |
| `crates/axiomdb-storage/src/heap_chain.rs` | modify | `scan_visible()` + `scan_rids_visible()` signatures → `&mut dyn StorageEngine`; fast path + lazy-set |
| `crates/axiomdb-catalog/src/reader.rs` | modify | `CatalogReader.storage` field: `&'a dyn` → `&'a mut dyn` |
| `crates/axiomdb-catalog/src/resolver.rs` | modify | Propagate `&mut` into `SchemaResolver` |
| `crates/axiomdb-sql/src/table.rs` | modify | `scan_table()` signature: `&dyn` → `&mut dyn` |
| `crates/axiomdb-storage/tests/heap_all_visible.rs` | create | 5 integration tests |

---

## Algorithm / Data structure

### Step A — `page.rs`: flag constant + three methods

```rust
pub const PAGE_FLAG_ALL_VISIBLE: u8 = 0x01;

impl Page {
    pub fn is_all_visible(&self) -> bool {
        self.header().flags & PAGE_FLAG_ALL_VISIBLE != 0
    }
    pub fn set_all_visible(&mut self) {
        self.header_mut().flags |= PAGE_FLAG_ALL_VISIBLE;
    }
    pub fn clear_all_visible(&mut self) {
        self.header_mut().flags &= !PAGE_FLAG_ALL_VISIBLE;
    }
}
```

Neither `set_all_visible` nor `clear_all_visible` calls `update_checksum()`. Consistent with `chain_set_next_page` convention — caller owns the checksum.

### Step B — `heap.rs`: clear flag in `mark_deleted()`

Insert `page.clear_all_visible()` as the first mutation inside `mark_deleted`, before writing `txn_id_deleted`. Unconditional — no `if is_all_visible()` check (one branch eliminated, same cache line as the subsequent write).

The single `update_checksum()` in `delete_tuple()` covers both the flag clear and the slot stamp.

### Step C — `heap_chain.rs`: updated `scan_visible()` pseudocode

```
pub fn scan_visible(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
    snap: TransactionSnapshot,
) -> Result<Vec<(u64, u16, Vec<u8>)>, DbError>:

  current = root_page_id
  while current != 0:
    raw = *storage.read_page(current)?.as_bytes()   // 16 KB copy
    page = Page::from_bytes(raw)?
    next = chain_next_page(&page)

    if page.is_all_visible():
      // ── Fast path: skip is_visible() ──────────────────────────────
      for slot_id in 0..num_slots(&page):
        entry = read_slot(&page, slot_id)
        if entry.is_dead(): continue
        data = page.as_bytes()[entry.offset..entry.offset+entry.length]
        result.push((current, slot_id, data[size_of::<RowHeader>()..].to_vec()))
    else:
      // ── Slow path: MVCC check + lazy-set ──────────────────────────
      all_vis  = true
      has_alive = false
      page_rows = Vec::new()

      for slot_id in 0..num_slots(&page):
        entry = read_slot(&page, slot_id)
        if entry.is_dead(): continue
        has_alive = true
        bytes = page.as_bytes()[entry.offset..entry.offset+entry.length]
        header = bytemuck::from_bytes::<RowHeader>(&bytes[..size_of::<RowHeader>()])
        if header.txn_id_deleted != 0: all_vis = false
        if !header.is_visible(&snap): all_vis = false; continue
        page_rows.push((slot_id, bytes[size_of::<RowHeader>()..].to_vec()))

      // Lazy-set: one-time write per page (amortized over all future scans)
      if all_vis && has_alive && page.header().item_count > 0:
        page.set_all_visible()
        page.update_checksum()
        storage.write_page(current, &page)?   // propagate I/O errors

      for (slot_id, data) in page_rows:
        result.push((current, slot_id, data))

    current = next

  Ok(result)
```

`page_rows` buffers slow-path results so the lazy-set write (needing `&mut page`) executes after all borrows of `page.as_bytes()` are dropped.

### Step D — `scan_rids_visible()` pseudocode

Mirrors Step C: same fast/slow logic, collects `(u64, u16)` instead of `(u64, u16, Vec<u8>)`. No `page_rows` buffer needed — `(current, slot_id)` pairs pushed directly. Lazy-set write is the same.

### Step E — `CatalogReader` and `SchemaResolver`

Change `CatalogReader<'a>.storage: &'a dyn` → `&'a mut dyn`. Propagate into `SchemaResolver`. All call sites in `executor.rs` already hold `&mut dyn StorageEngine` — no second-order cascades.

### Step F — `TableEngine::scan_table`

Change `storage: &dyn` → `storage: &mut dyn`. Every call site in `executor.rs` already holds `&mut dyn StorageEngine`.

---

## Implementation phases (ordered)

1. **`page.rs`** — constant + 3 methods + unit tests. `cargo test -p axiomdb-storage` passes.
2. **`heap.rs`** — `clear_all_visible()` in `mark_deleted()`. `cargo test -p axiomdb-storage` passes.
3. **`heap_chain.rs`** — signature change + fast path + lazy-set in both scan functions. Fix test call sites in same file. `cargo test -p axiomdb-storage` passes.
4. **`reader.rs` + `resolver.rs`** — propagate `&mut`. `cargo build -p axiomdb-catalog`; fix every compile error. `cargo test -p axiomdb-catalog` passes.
5. **`table.rs`** — `scan_table` signature. `cargo test -p axiomdb-sql` passes.
6. **WAL test fixes** — change `&storage` → `&mut storage` at 5 `scan_rids_visible` call sites in `axiomdb-wal` tests. `cargo test --workspace` passes.
7. **Integration tests** — `crates/axiomdb-storage/tests/heap_all_visible.rs`.
8. **Benchmark** — before/after numbers recorded.

---

## Tests to write

### Unit tests — `page.rs`

- `test_all_visible_flag_set_and_clear` — set + checksum → is_all_visible true, checksum valid; clear + checksum → false, checksum valid.
- `test_all_visible_does_not_affect_other_flags` — start flags=0xFF, clear → 0xFE, set → 0xFF.
- `test_checksum_covers_flag_change` — set + checksum → valid; flip bit manually → invalid.

### Unit tests — `heap_chain.rs`

- `test_all_visible_not_set_for_active_txn_rows` — rows with txn_id=5, scan snapshot_id=5 → flag NOT set.
- `test_all_visible_not_set_for_empty_page` — root page, no inserts, scan → flag not set.

### Integration tests — `tests/heap_all_visible.rs`

- `test_scan_visible_sets_flag_after_committed_insert` — insert N rows (txn 1, committed snap), first scan → N rows returned AND flag set on all pages; second scan (fast path) → still N rows.
- `test_mark_deleted_clears_flag` — insert, scan (flag set), delete one slot → page flag cleared.
- `test_flag_cleared_page_rescanned_correctly` — insert N, scan (flag set), delete 1 (flag clear), scan → N-1 rows, flag re-set on page if remaining slots are all-visible.
- `test_crash_safety_flag_unset_on_new_page` — committed rows, flags=0 (simulate crash) → scan returns correct rows AND sets flag.
- `test_empty_page_never_gets_flag` — allocate page, zero inserts, scan → flag stays unset.

---

## Anti-patterns to avoid

- **DO NOT** set the flag for pages with `item_count == 0`.
- **DO NOT** make `clear_all_visible()` conditional on `is_all_visible()` inside `mark_deleted()`.
- **DO NOT** add WAL entries for the flag — it is a derived hint, not authoritative.
- **DO NOT** modify `CrashRecovery` — flag absence after recovery is the safe conservative state.
- **DO NOT** call `update_checksum()` inside `set_all_visible()` or `clear_all_visible()` — caller owns the checksum, consistent with `chain_set_next_page` and all other `header_mut()` callers.
- **DO NOT** defer the lazy-set write to end-of-chain. Write immediately after setting so errors propagate before advancing.

---

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| `CatalogReader` `&mut` propagation scope is large | Medium | Compiler enumerates every site; all are mechanical `&storage` → `&mut storage` |
| Scans now write pages (new side effect) | Low | Document in `HeapChain::scan_visible` doc comment; revisit in Phase 7 |
| 16 KB copy per page added to slow path | Low | Same cost as all write paths; amortized over all future fast-path scans |
| Test-only mutable borrow changes in WAL crate | Low | 5 sites in `#[cfg(test)]` blocks with local variables; purely syntactic |
