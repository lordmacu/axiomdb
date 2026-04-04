# Plan: 40.7 — HeapChain Concurrent Access

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-storage/src/heap_chain.rs` | Integrate page X-latch on all mutations; HeapInsertHint; atomic chain growth |
| `crates/axiomdb-storage/src/heap.rs` | Page-latch-aware insert/delete/update (acquire before, release after) |
| `crates/axiomdb-storage/src/heap_hint.rs` | **NEW**: HeapInsertHint (AtomicU64 last_page + AtomicU32 estimated_free) |
| `crates/axiomdb-storage/src/lib.rs` | Export heap_hint module |
| `crates/axiomdb-sql/src/table.rs` | TableEngine uses HeapInsertHint for insert distribution |

## Implementation phases

### Phase 1: HeapInsertHint struct
```rust
pub struct HeapInsertHint {
    last_page_with_space: AtomicU64,
    estimated_free: AtomicU32,
}
```
- `suggest_page(&self) -> u64`: load last_page atomically
- `update(&self, page_id: u64, free_bytes: u32)`: store new hint atomically
- `invalidate(&self)`: set to NULL_PAGE (force rescan)
- Per-table, stored alongside TableDef or in a global HashMap<table_id, HeapInsertHint>

### Phase 2: Page X-latch integration in heap.rs
For every mutation function:
- `insert_tuple()`: caller must hold page X-latch (assert or document)
- `delete_tuple()`: caller must hold page X-latch
- `mark_deleted()`: caller must hold page X-latch
- `rewrite_tuple_same_slot()`: caller must hold page X-latch

The latch is acquired OUTSIDE these functions (at HeapChain level) because
the caller needs to make the acquire/release decision (e.g., try page, if full
release and try next).

### Phase 3: HeapChain.insert with latch + hint
```
fn insert(storage: &dyn StorageEngine, page_locks: &PageLockTable,
          hint: &HeapInsertHint, root_page_id: u64,
          data: &[u8], txn_id: TxnId) -> Result<(u64, u16), DbError> {

    // 1. Try hint page first
    let hint_page = hint.suggest_page();
    if hint_page != NULL_PAGE {
        let _latch = page_locks.write(hint_page);
        let mut page = storage.read_page(hint_page)?.into_page();
        if free_space(&page) >= needed {
            let slot_id = insert_tuple(&mut page, data, txn_id)?;
            storage.write_page(hint_page, &page)?;
            hint.update(hint_page, free_space(&page) as u32);
            return Ok((hint_page, slot_id));
        }
        // Hint page full → fall through (latch released by drop)
    }

    // 2. Walk chain from root (or hint page) to find space
    let mut current = if hint_page != NULL_PAGE { hint_page } else { root_page_id };
    while current != 0 {
        let _latch = page_locks.write(current);
        let mut page = storage.read_page(current)?.into_page();
        if free_space(&page) >= needed {
            let slot_id = insert_tuple(&mut page, data, txn_id)?;
            storage.write_page(current, &page)?;
            hint.update(current, free_space(&page) as u32);
            return Ok((current, slot_id));
        }
        let next = chain_next_page(&page);
        current = next;
        // Latch released by drop at end of loop iteration
    }

    // 3. All pages full → allocate new page
    allocate_and_link_new_page(storage, page_locks, hint, root_page_id, data, txn_id)
}
```

### Phase 4: Atomic chain growth
```
fn allocate_and_link_new_page(...) {
    // 1. Allocate new page (Mutex<FreeList> from 40.3)
    let new_page_id = storage.alloc_page(PageType::Data)?;

    // 2. X-latch new page
    let _new_latch = page_locks.write(new_page_id);
    let mut new_page = Page::new(PageType::Data, new_page_id);
    // Initialize as heap page...

    // 3. Find last page in chain, X-latch it
    let last_page_id = find_chain_tail(storage, root_page_id);
    let _last_latch = page_locks.write(last_page_id);
    let mut last_page = storage.read_page(last_page_id)?.into_page();

    // 4. Link: last_page.next = new_page_id
    chain_set_next_page(&mut last_page, new_page_id);
    storage.write_page(last_page_id, &last_page)?;
    // Release last_page latch (drop)

    // 5. Insert tuple into new page
    let slot_id = insert_tuple(&mut new_page, data, txn_id)?;
    storage.write_page(new_page_id, &new_page)?;
    hint.update(new_page_id, free_space(&new_page) as u32);
    // Release new_page latch (drop)

    Ok((new_page_id, slot_id))
}
```

**Latch ordering**: new_page before last_page. new_page is freshly allocated
(no other thread can hold its latch), so no deadlock risk.

### Phase 5: SCAN without page latches
Verify existing `scan_table_filtered()` and `scan_table()` work correctly
under concurrency:
- `read_page()` returns owned copy → immutable after return
- MVCC `is_visible()` check on each row → filters uncommitted/deleted
- No page latches needed — owned copy is a consistent snapshot

Add a comment documenting this guarantee. Add a concurrent test to verify.

### Phase 6: DELETE and UPDATE with latch
```
fn delete(storage: &dyn StorageEngine, page_locks: &PageLockTable,
          page_id: u64, slot_id: u16, txn_id: TxnId) {
    let _latch = page_locks.write(page_id);
    let mut page = storage.read_page(page_id)?.into_page();
    mark_deleted(&mut page, slot_id, txn_id)?;
    storage.write_page(page_id, &page)?;
}
```

### Phase 7: Integration tests
- 4 threads × 1000 inserts → all succeed, no duplicate slots, no corruption
- 2 threads insert to same table → hint distributes across pages
- INSERT during SELECT → reader sees MVCC-consistent snapshot
- DELETE during SELECT → reader still sees old value
- Chain growth under contention → no orphan pages, chain intact

## Tests to write

1. **HeapInsertHint**: suggest → update → suggest returns updated page
2. **Concurrent insert, different pages**: 4 threads, verify parallel (timing)
3. **Concurrent insert, same page**: 2 threads force same page, verify serialized
4. **Chain growth under contention**: fill all pages, 4 threads trigger growth
5. **SCAN during INSERT**: reader thread + writer thread, verify MVCC correctness
6. **DELETE during SCAN**: delete row, concurrent scan still sees it (MVCC)
7. **UPDATE during SCAN**: update row, concurrent scan sees old value (snapshot)
8. **Stress**: 8 threads × mixed operations × 1000 iterations → data integrity check

## Anti-patterns to avoid

- DO NOT hold page X-latch while walking the chain (latch, check, release, move to next)
- DO NOT hold page X-latch during WAL write (release latch first, WAL is separate)
- DO NOT acquire two page X-latches simultaneously (except chain growth with ordering)
- DO NOT use page S-latch for readers (MVCC makes it unnecessary — just read the copy)
- DO NOT modify HeapInsertHint under a lock — it's purely advisory, stale values are fine

## Risks

- **Hint staleness**: HeapInsertHint may point to a full page. Cost: one wasted X-latch
  acquisition + free_space check (~1µs). Acceptable — hint is probabilistic, not exact.
- **Chain walk contention**: if hint is stale and many threads walk the chain, they
  serialize on page X-latches. Mitigation: update hint aggressively so threads converge
  on the correct page quickly.
- **Latch ordering violation in chain growth**: holding new_page + last_page simultaneously.
  Mitigation: strict ordering (new before last) + new_page is freshly allocated (no contention).
