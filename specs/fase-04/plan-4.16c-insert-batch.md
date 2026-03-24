# Plan: 4.16c — INSERT batch execution (HeapChain::insert_batch)

## Files to create / modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-storage/src/heap_chain.rs` | modify | Add `insert_batch()` |
| `crates/axiomdb-sql/src/table.rs` | modify | Add `insert_rows_batch()` |
| `crates/axiomdb-sql/src/executor.rs` | modify | Dispatch multi-row VALUES to batch path |
| `crates/axiomdb-storage/tests/integration_storage.rs` | modify | Add batch insert tests |
| `crates/axiomdb-wal/tests/integration_durability.rs` | modify | Add crash-mid-batch test |

---

## Algorithm / Data structure

### Phase 1 — `HeapChain::insert_batch()`

```
insert_batch(storage, root_page_id, rows: &[Vec<u8>], txn_id):
  if rows.is_empty() → return Ok(vec![])

  last_id   ← Self::last_page_id(storage, root_page_id)
  page      ← Page::from_bytes(*storage.read_page(last_id)?.as_bytes())
  result    ← Vec::with_capacity(rows.len())
  dirty     ← false   // true when page has unsaved rows

  for data in rows:
    match insert_tuple(&mut page, data, txn_id):
      Ok(slot_id):
        result.push((last_id, slot_id))
        dirty = true

      Err(HeapPageFull):
        // ── flush current page ──────────────────────────────────────────────
        page.update_checksum()
        storage.write_page(last_id, &page)?

        // ── allocate new page ───────────────────────────────────────────────
        new_id   ← storage.alloc_page(PageType::Data)?
        new_page ← Page::new(PageType::Data, new_id)

        // ── link: update next_page pointer in the just-written page ─────────
        // Re-read the page we just wrote to get a fresh mutable copy,
        // set the chain pointer, and write again.
        // This is the same two-write pattern as HeapChain::insert() today.
        let raw2 = *storage.read_page(last_id)?.as_bytes()
        let mut prev = Page::from_bytes(raw2)?
        chain_set_next_page(&mut prev, new_id)
        prev.update_checksum()
        storage.write_page(last_id, &prev)?

        // ── switch to new page ───────────────────────────────────────────────
        last_id = new_id
        page    = new_page
        dirty   = false

        // Retry on new (empty) page — guaranteed to fit.
        slot_id ← insert_tuple(&mut page, data, txn_id)?
        result.push((last_id, slot_id))
        dirty = true

      Err(other):
        return Err(other)

  // ── flush last dirty page ────────────────────────────────────────────────
  if dirty:
    page.update_checksum()
    storage.write_page(last_id, &page)?

  return Ok(result)
```

**Key invariant:** for each page, the sequence is:
1. `insert_tuple()` × M (M rows fit)
2. `update_checksum()` once
3. `write_page()` once

No page is read or written more than twice (one write for data + one write for chain pointer update when the page overflows).

---

### Phase 2 — `TableEngine::insert_rows_batch()`

```
insert_rows_batch(storage, txn, table_def, columns, batch: Vec<Vec<Value>>):
  if batch.is_empty() → return Ok(vec![])

  txn_id ← txn.active_txn_id() or Err(NoActiveTransaction)
  col_types ← column_data_types(columns)

  // ── encode all rows first (fail-fast before any heap writes) ──────────────
  encoded_rows: Vec<Vec<u8>> = batch
    .into_iter()
    .map(|values| coerce_values(values, columns).and_then(|c| encode_row(&c, &col_types)))
    .collect::<Result<_, _>>()?

  // ── insert into heap in one batch ─────────────────────────────────────────
  record_ids ← HeapChain::insert_batch(
    storage, table_def.data_root_page_id, &encoded_rows, txn_id
  )?

  // ── WAL: one record_insert per row (same as today) ────────────────────────
  // WAL ordering: write_page() inside insert_batch() already happened.
  // record_insert() appends to BufWriter (not yet durable). Both become
  // durable together at COMMIT. Invariant maintained.
  let mut result = Vec::with_capacity(record_ids.len());
  for ((page_id, slot_id), encoded) in record_ids.iter().zip(encoded_rows.iter()) {
    let key = encode_rid(*page_id, *slot_id);
    txn.record_insert(table_def.id, &key, encoded, *page_id, *slot_id)?;
    result.push(RecordId { page_id: *page_id, slot_id: *slot_id });
  }

  Ok(result)
```

---

### Phase 3 — Executor dispatch (`execute_insert_ctx`)

```
// In execute_insert_ctx, replace the Values loop:

InsertSource::Values(rows) => {
  let mut count = 0u64;
  let mut first_generated: Option<u64> = None;

  // ── Pre-resolve AUTO_INCREMENT for all rows ────────────────────────────
  let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
  for value_exprs in &rows {
    let provided: Vec<Value> = ...eval each expr...;
    let mut full = col_positions mapping...;

    // AUTO_INCREMENT: same logic as today
    if let Some(ai_col) = auto_inc_col {
      if full[ai_col] == Value::Null {
        let id = next_auto_inc(...)?;
        full[ai_col] = Value::Int(id as i32); // or BigInt
        if first_generated.is_none() { first_generated = Some(id); }
      }
    }

    full_batch.push(full);
  }

  // ── Single row: use existing path (no Vec allocation overhead) ─────────
  if full_batch.len() == 1 {
    TableEngine::insert_row(storage, txn, &resolved.def, schema_cols,
                            full_batch.pop().unwrap())?;
    count = 1;
  } else {
    // ── Multi-row: batch path ───────────────────────────────────────────
    let n = full_batch.len() as u64;
    TableEngine::insert_rows_batch(storage, txn, &resolved.def, schema_cols,
                                   full_batch)?;
    count = n;
  }

  Ok(QueryResult::Affected { count, last_insert_id: first_generated.unwrap_or(0) })
}
```

---

## Implementation phases (ordered)

### Step 1 — `HeapChain::insert_batch()` (heap_chain.rs)

1. Copy signature of `HeapChain::insert()` as starting point
2. Add outer loop over `rows: &[Vec<u8>]`
3. Track `dirty: bool` and `last_id`/`page` across iterations
4. Handle `HeapPageFull`: flush + alloc + link + retry
5. Final flush after loop

**Anti-patterns to avoid:**
- DO NOT call `Self::last_page_id()` inside the loop — call it once before the loop
- DO NOT call `storage.read_page()` for each row — only on page transitions
- DO NOT forget to flush the final page after the loop

### Step 2 — `TableEngine::insert_rows_batch()` (table.rs)

1. Encode all rows (fail-fast before heap modifications)
2. Call `HeapChain::insert_batch()`
3. Call `txn.record_insert()` for each returned `(page_id, slot_id)`

### Step 3 — Executor dispatch (executor.rs)

1. Change `execute_insert_ctx` to pre-collect all rows with AUTO_INCREMENT resolved
2. N == 1: call `insert_row()` (unchanged path)
3. N > 1: call `insert_rows_batch()`

### Step 4 — Tests

**Unit tests (heap_chain.rs):**
- `test_insert_batch_empty`: empty slice → Ok(vec![])
- `test_insert_batch_single_page`: 5 rows, all fit on 1 page → same result as 5 individual inserts
- `test_insert_batch_multi_page`: enough rows to cross 3 page boundaries → chain has 3 pages, all rows present

**Integration tests (integration_storage.rs):**
- `test_batch_vs_individual_equivalence`: insert 500 rows via batch, scan, compare to 500 individual inserts

**Crash recovery (integration_durability.rs):**
- `test_crash_mid_batch_insert`: begin, batch-insert 1K rows, crash without commit, recover → 0 rows

---

## Tests to write

### Unit tests

```rust
// heap_chain.rs
#[test]
fn test_insert_batch_produces_same_heap_as_individual_inserts() {
    // Build two identical MemoryStorage instances.
    // Insert 500 rows via insert_batch into one, 500 individual inserts into other.
    // Scan both and assert identical results.
}

#[test]
fn test_insert_batch_multi_page() {
    // Insert enough rows to fill 3 pages.
    // Verify chain length == 3 and all rows are present via scan.
}
```

### Integration test

```rust
// integration_storage.rs
#[test]
fn test_batch_insert_crash_recovery() {
    // 1. Open DB, CREATE TABLE, BEGIN
    // 2. INSERT 1000 rows via batch path (executor or TableEngine::insert_rows_batch)
    // 3. Drop DB without commit (simulate crash)
    // 4. Open DB with crash recovery
    // 5. SELECT COUNT(*) → 0
}
```

---

## Anti-patterns to avoid

- **DO NOT** change the WAL entry format — each row still gets its own `WalEntry::Insert`. Phase 3.18 changes that.
- **DO NOT** buffer WAL entries and flush them at the end of the batch — the WAL ordering (write_page before record_insert at page granularity) must be maintained.
- **DO NOT** change the single-row path — keep `TableEngine::insert_row()` for N==1 to avoid Vec allocation overhead.
- **DO NOT** remove the `dirty` flag — if the last page was not written (all rows fit exactly with no overflow), we must still flush it.
- **DO NOT** call `update_checksum()` inside the hot row-insertion loop — only call it once per page flush.

---

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Chain pointer update corrupts chain on crash | Low | Same two-write pattern as `HeapChain::insert()`. 5 existing crash recovery tests catch this. |
| WAL entries recorded after page already written | Low | This is the same ordering as today. BufWriter flushes together at COMMIT. |
| `dirty=false` bug: last page not flushed | Medium | Test: insert exactly PAGE_CAPACITY rows → all rows present on scan |
| AUTO_INCREMENT pre-computation has N=0 edge case | Low | Empty batch returns early before the loop |
| `encode_row()` failure mid-batch leaves partial Vec | Low | Encoding phase is entirely before heap writes; error path is clean |

---

## Expected performance

Based on the analysis:
- 10K rows → ~50 pages × 2 writes each = 100 write_page() calls (vs 10K today)
- Elimination of 9,900 redundant 16KB copies
- Wire protocol INSERT: estimated 30K–60K/s (vs 6K/s today, toward MariaDB's 141K/s)

✅ Spec written. You can now switch to `/effort high` for the Plan → Implementation phases.
