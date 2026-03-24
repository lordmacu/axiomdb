# INSERT Performance Analysis — 2026-03-24

## Benchmark setup
- Hardware: Apple M2 Pro (native, no Docker)
- Storage: MmapStorage + WAL (fsync=true)
- Dataset: 10,000 rows, 1 transaction
- Reference: MariaDB 12.1 (same hardware, native)

## Results history

| Date | Change | AxiomDB ops/s | vs MariaDB |
|---|---|---|---|
| 2026-03-24 | Baseline (no cache) | 20,694/s | 6.8× slower |
| 2026-03-24 | + SchemaCache in analyze_cached() | 28,702/s | 4.9× slower |
| 2026-03-24 | + SessionContext in execute_with_ctx() | **29,887/s** | 4.7× slower |
| — | MariaDB 12.1 reference | 140,926/s | — |

## Profiling breakdown (10K rows, after caches)

```
parse()             × 10K =   ~5ms   (500ns/call)
analyze_cached()    × 10K =   ~1ms   (warm cache, hashmap lookup only)
execute_with_ctx()  × 10K =  ~330ms  breakdown:
  resolve_table_cached()     ~1ms    (warm SessionContext)
  coerce_values() + encode_row()  ~50ms   (~5µs/row)
  HeapChain::insert()        ~150ms  (~15µs/row — find page + write slot)
  txn.record_insert()        ~130ms  (~13µs/row — WalEntry alloc + serialize + BufWriter)
txn.commit() (fsync)          ~50ms  (1 fsync total, fixed cost)
```

## Root causes of remaining gap

### 1. Per-row HeapChain::insert() — ~15µs/row
Each row must:
- Find the right heap page with free space (may scan HeapChain linked list)
- Acquire write access to the mmap region
- Write RowHeader (24 bytes) + encoded row data
- Update slot directory (SlotEntry)
- Update free_start/free_end in page header

MariaDB has the same work but 18 years of micro-optimization on the page layout code.

### 2. Per-row WalEntry serialization — ~13µs/row
Each INSERT creates:
- `Vec::with_capacity()` for key + new_value
- `WalEntry::new()` struct allocation
- `entry.to_bytes()` → serializes header + CRC32c + payload
- `BufWriter::write_all()` (cheap, in RAM)
- `undo_ops.push(UndoInsert)` → Vec push

The BufWriter already batches writes — fsync only happens at COMMIT.
The cost is allocation + serialization, not I/O.

### 3. Per-row parse() — ~5µs/row (was ~500ns before, now 5ms total)
10,000 separate SQL strings each need full lexer + parser.
Multi-row VALUES would reduce this to 1 parse for N rows.

## Next steps

### Phase 4.16c — Multi-row VALUES (easier, higher impact on this benchmark)
```sql
-- Current benchmark: 10,000 separate statements
INSERT INTO bench_users VALUES (1, 'user_000001', ...);
INSERT INTO bench_users VALUES (2, 'user_000002', ...);
...

-- After 4.16c: 1 statement for all rows
INSERT INTO bench_users VALUES
  (1, 'user_000001', ...),
  (2, 'user_000002', ...),
  ...(9999 more rows)...;
```
Expected: parse+analyze cost drops from 5ms to ~0.5ms. Execute still has per-row cost.

### Phase 3.17 — WAL batch append
Add `WalWriter::append_batch(entries)` that serializes all entries to a single
`Vec<u8>` and writes once, instead of per-entry `write_all()`.

### Phase 3.18 — WAL record per page (like PostgreSQL COPY)
Instead of `WalEntry::Insert { row_data }` per row, buffer rows until the page is full
and emit `WalEntry::PageWrite { page_id, page_bytes }`.

```
Current:  10K rows → 10K WalEntry::Insert → 10K BufWriter writes
With 3.18: 10K rows → ~50 pages → 50 WalEntry::PageWrite → 50 BufWriter writes
```

Recovery becomes: read page image from WAL → write to .db file (no row-by-row redo).
This is structurally similar to PostgreSQL's full-page writes on first modification.

Expected improvement: ~3-5× on bulk insert throughput.

## What will NOT help much

- Larger BufWriter capacity: already 64KB, fsync is the bottleneck at COMMIT
- More aggressive mmap hints: page is hot after first write, mmap_advice won't help
- Removing CRC32c: ~1µs/entry, negligible vs 13µs total WAL cost
