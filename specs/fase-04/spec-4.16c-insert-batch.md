# Spec: 4.16c — INSERT batch execution (HeapChain::insert_batch)

## What to build (not how)

A batch-aware INSERT path for multi-row `INSERT INTO t VALUES (r1),(r2),...(rN)` that
loads each heap page exactly once, writes multiple rows into it, and writes it back once —
instead of the current O(N × page_size) copy pattern where every row individually
reads, modifies, and writes the full 16 KB page.

The result must be functionally identical to executing N individual `insert_row()` calls:
same MVCC metadata, same WAL entries, same crash-recovery behavior. Performance is the
only thing that changes.

---

## Inputs / Outputs

### `HeapChain::insert_batch()`

- **Input:**
  - `storage: &mut dyn StorageEngine`
  - `root_page_id: u64` — chain root (from `TableDef.data_root_page_id`)
  - `rows: &[Vec<u8>]` — pre-encoded row payloads (without `RowHeader`); empty slice is a no-op
  - `txn_id: TxnId` — active transaction identifier

- **Output:** `Result<Vec<(u64, u16)>, DbError>`
  — one `(page_id, slot_id)` per input row, in the same order as `rows`

- **Errors:**
  - Any `StorageEngine` I/O error
  - `DbError::HeapPageFull` is never returned to the caller; it triggers chain growth internally
  - If any single row is too large to fit in an empty page (`encoded_len > PAGE_BODY_SIZE - RowHeader - SlotEntry overhead`): `DbError::TypeMismatch` (row too large)

---

### `TableEngine::insert_rows_batch()`

- **Input:**
  - `storage: &mut dyn StorageEngine`
  - `txn: &mut TxnManager`
  - `table_def: &TableDef`
  - `columns: &[ColumnDef]`
  - `batch: Vec<Vec<Value>>` — multiple rows, each a full column vector already with AUTO_INCREMENT resolved

- **Output:** `Result<Vec<RecordId>, DbError>` — one `RecordId` per inserted row

- **Errors:** propagates from `coerce_values`, `encode_row`, `HeapChain::insert_batch`, `txn.record_insert`

---

### Executor changes (`execute_insert_ctx`)

- `InsertSource::Values` with **N > 1 rows**: use `TableEngine::insert_rows_batch()`
- `InsertSource::Values` with **N == 1 row**: keep existing `TableEngine::insert_row()` path (no Vec overhead for the common single-row case)
- `InsertSource::Select`: unchanged (already batched by the SELECT result set)

---

## Use cases

### 1. Happy path — multi-row INSERT, all rows fit in pages already in chain

```sql
INSERT INTO orders (user_id, amount, status)
VALUES (1, 99.99, 'pending'),
       (2, 49.99, 'pending'),
       ...(10,000 rows)...;
```

Heap chain starts with 1 page. Batch fills page 1 (~200 rows), allocates page 2, fills it, …, until all 10K rows are inserted.

Expected: 50 page reads + 50 page writes (one per page), not 10K each.

---

### 2. Chain growth within a batch

Some rows fit on the current last page; then the page overflows mid-batch. The implementation must:
1. Write the current page (with rows inserted so far)
2. Allocate a new page and link it into the chain
3. Write the new page's `next_page` pointer into the old page (already written — need a second write to update the link)
4. Continue inserting remaining rows into the new page

The chain pointer update (step 3) requires a re-read + re-write of the last page that was just written. This is a known two-write pattern (same as `HeapChain::insert` today).

---

### 3. Single row — no regression

```sql
INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
```

Continues to use `TableEngine::insert_row()` unchanged. No Vec allocation overhead.

---

### 4. AUTO_INCREMENT within a batch

```sql
INSERT INTO users (name) VALUES ('Alice'), ('Bob'), ('Carlos');
```

AUTO_INCREMENT IDs are assigned in the executor loop **before** calling `insert_rows_batch()`. Each row in `batch` already has its final `Value::Int(id)` in the auto-increment column position. The batch function is unaware of AUTO_INCREMENT.

---

### 5. CHECK constraint within a batch

If a CHECK constraint fails on row 3 of a 10-row batch: the error propagates immediately. Rows 1–2 were already encoded but NOT yet inserted (encoding and HeapChain insertion are separate phases). The transaction is rolled back normally.

Actually — encoding happens before HeapChain::insert_batch. If encoding of row 3 fails, rows 1–2 encoded bytes are in the Vec but no heap writes have happened yet. Clean error path.

---

### 6. Crash recovery — batch is not committed

```
BEGIN
  INSERT ... VALUES (10K rows)   ← heap pages modified, WAL entries written
[CRASH]
```

Recovery reads WAL, finds 10K Insert entries without a Commit. Undoes each via `UndoInsert { page_id, slot_id }` (marks slots dead). The `undo_ops` Vec accumulated in the transaction is not used — crash recovery works purely from the WAL. **Invariant: each `record_insert()` call happens after `write_page()` for that row's page, same as today.**

---

## Acceptance criteria

- [ ] `HeapChain::insert_batch(&mut storage, root, &rows, txn_id)` inserts N rows, returns N `(page_id, slot_id)` pairs
- [ ] Pages are loaded at most once per page per batch call (no redundant reads within the batch)
- [ ] All 5 existing crash recovery integration tests pass unchanged
- [ ] All 1105+ existing tests pass
- [ ] New unit test: `insert_batch` of 500 rows into an empty table produces the same heap state as 500 individual `insert` calls (same page layout, same slot contents)
- [ ] New integration test: crash mid-batch → recovery undoes all rows → table is empty
- [ ] Benchmark `cargo bench --bench btree` and `cargo bench --bench storage` show no regression
- [ ] New benchmark `benches/comparison/axiomdb_bench`: INSERT 10K rows via wire protocol improves from ~6K/s toward ~30K+/s

---

## Out of scope

- Sorting rows by page before insertion (unnecessary for append-only inserts to a new table)
- COPY protocol (different command path, Phase 35)
- Parallel page writes (single-writer constraint, Phase 7)
- Changing the WAL format (WAL entries remain per-row, not per-page — that is Phase 3.18)
- UPDATE or DELETE batch paths (different complexity, different phases)

---

## Dependencies

- `HeapChain::insert()` (Phase 1) — exists, this builds on it
- `TxnManager::record_insert()` — exists with `append_with_buf` optimization (Phase 3.17)
- `TableEngine::insert_row()` — exists (Phase 4.5b)
- Executor `execute_insert_ctx` — exists (Phase 4.5)
- Wire protocol server — exists (Phase 5), used to validate via benchmark

---

## ⚠️ DEFERRED

- `HeapChain::insert_batch()` currently computes one CRC32c per page write. If the page is partially filled (last page), CRC is still computed. This is identical to today's behavior. A future optimization could defer CRC to checkpoint time (Phase 7+).
- WAL record per page instead of per row (Phase 3.18) is explicitly out of scope here; this spec only batches at the HeapChain level, not the WAL level.
