# Phase 39 — Clustered Index Storage Engine

## Motivation

AxiomDB currently uses a **heap + separate B-tree index** architecture (like PostgreSQL/MyISAM).
This means every PK-based operation requires **2 I/O paths**: index lookup → RecordId → heap fetch.
InnoDB stores data **inside** the B-tree leaf pages (clustered index), requiring only **1 I/O path**.

**Benchmark evidence:**
- `update_range` (PK range): 0.44x vs MariaDB — double page read is the bottleneck
- `select_pk` (point lookup): 0.74x vs MariaDB — index → heap indirection
- `select_range` (PK range scan): 0.77x vs MariaDB — same double read
- All PK-based operations pay this ~2x penalty structurally

**Architecture comparison:**
```
Current AxiomDB:     PK Index [key → RecordId] → Heap [RecordId → row data]
                     23 pages (index) + 29 pages (heap) = 52 I/Os

InnoDB clustered:    PK Index [key → row data inline]
                     23 pages total = 23 I/Os

Secondary lookup:
  Current:   Sec Index [key → RecordId] → Heap [RecordId → row data]
  Clustered: Sec Index [key → PK value] → PK Index [PK → row data]
```

## What to build

A clustered index storage engine where:
1. The PRIMARY KEY B-tree leaf pages store **full row data inline**
2. Secondary indexes store **PK values** instead of RecordIds
3. The heap is eliminated for clustered tables
4. MVCC works via inline txn_id + undo log (like InnoDB)

## Out of scope (this phase)

- Automatic migration of existing heap tables (manual rebuild required)
- Columnar storage or hybrid formats
- Distributed storage or sharding
- WITHOUT ROWID syntax (always clustered if PK exists)

## Dependencies

- Phase 1-3: Storage, B-tree, WAL ✅
- Phase 4-6: SQL executor, index maintenance ✅
- Phase 7: MVCC basics ✅

---

## Subfases

### 39.1 — Clustered Leaf Page Format

**What:** Design and implement the new B-tree leaf page format that stores full row data inline.

**Current leaf format (217 entries, index-only):**
```
[Header: 16B] [key_lens: 217B] [rids: 2170B] [keys: 13888B]
Total: 16,291B / 16,320B usable
```

**New clustered leaf format:**
```
[Header: 16B]
  - page_type: u8 (new: CLUSTERED_LEAF)
  - num_cells: u16
  - free_start: u16 (cell pointer array end)
  - free_end: u16 (cell content start)
  - next_leaf: u64
[Cell Pointer Array: num_cells × 2B] (grows down from header)
[Free Space]
[Cell Content Area] (grows up from page end)
  Per cell:
    [key_len: u16] [row_len: u16] [RowHeader: 24B] [key_data] [row_data]
```

**Variable-size cells** (unlike current fixed-slot B-tree) because row sizes vary.

**Acceptance criteria:**
- [ ] New page type `CLUSTERED_LEAF` in page_layout
- [ ] Cell insert/delete/lookup on clustered leaf page
- [ ] Overflow to separate pages for rows > ~4KB
- [ ] Binary search via cell pointer array (sorted by key)
- [ ] Free space management within page (free block list)
- [ ] Page compaction when fragmented
- [ ] Unit tests with variable-size rows

### 39.2 — Clustered Internal Page Format

**What:** Internal (non-leaf) pages for the clustered B-tree. These only store keys + child page pointers (same as current B-tree internals, but with variable-size keys to match clustered leaf format).

**Acceptance criteria:**
- [ ] New page type `CLUSTERED_INTERNAL`
- [ ] Variable-size key support (cell pointer array pattern)
- [ ] Child page pointer per key (separator keys)
- [ ] Binary search on internal pages
- [ ] Compatible with existing B-tree traversal logic

### 39.3 — Clustered B-Tree: Insert

**What:** Insert a full row into the clustered B-tree (key + data inline).

**Algorithm (InnoDB-inspired):**
1. Search tree to find target leaf page
2. If row fits on page → insert cell, update cell pointers
3. If page full → try page compaction first
4. If still full → split page (distribute ~50% cells to new page)
5. Propagate separator key + new page pointer to parent
6. Record WAL entry

**Key difference from current B-tree:** Cells are variable-size (row data), so split point is by data volume, not by count.

**Acceptance criteria:**
- [ ] Insert row into clustered leaf with sorted position
- [ ] Page compaction before split
- [ ] Page split with ~50% data volume distribution
- [ ] Parent update with separator key
- [ ] WAL logging for clustered insert
- [ ] Undo support (mark cell dead on rollback)
- [ ] Tests: insert 10K rows, verify sorted order

### 39.4 — Clustered B-Tree: Point Lookup

**What:** Find a single row by PK value in the clustered B-tree. Returns full row data directly from leaf page (no heap indirection).

**Algorithm:**
1. Binary search internal pages to find leaf
2. Binary search cell pointer array on leaf
3. Read cell data (key + RowHeader + row data)
4. Check MVCC visibility
5. Return row data or follow undo chain for older version

**Acceptance criteria:**
- [ ] O(log n) point lookup returning full row
- [ ] MVCC visibility check on RowHeader
- [ ] Version chain following via undo log (if current version not visible)
- [ ] Benchmark: should be ~1.5x faster than current (eliminates heap fetch)

### 39.5 — Clustered B-Tree: Range Scan

**What:** Scan a contiguous range of PK values, returning rows in PK order directly from leaf pages.

**Algorithm:**
1. Search tree to find starting leaf
2. Scan cells on current leaf (cell pointer array in sorted order)
3. Follow next_leaf chain to continue scan
4. Prefetch next leaves (4 pages ahead)
5. Check MVCC visibility per cell

**Acceptance criteria:**
- [ ] Range scan with start/end bounds
- [ ] next_leaf chain traversal
- [ ] Prefetching for sequential I/O
- [ ] Full table scan via leftmost leaf → chain
- [ ] MVCC visibility filtering
- [ ] Benchmark: should be ~1.8x faster than current

### 39.6 — Clustered B-Tree: Update (In-Place)

**What:** Update non-key columns directly on the clustered leaf page without moving the cell.

**Algorithm (InnoDB optimistic update):**
1. Search tree to find target cell
2. If new row fits in existing cell space → update in-place
3. Update RowHeader (txn_id, version++)
4. Record old values in undo log
5. WAL log the change
6. If row doesn't fit → delete + re-insert (pessimistic)

**Acceptance criteria:**
- [ ] In-place update when row size unchanged or shrinks
- [ ] Fallback to delete+insert when row grows beyond cell capacity
- [ ] RowHeader version increment
- [ ] Undo log for rollback
- [ ] Field-patch fast path for fixed-size columns
- [ ] WAL logging

### 39.7 — Clustered B-Tree: Delete

**What:** Delete a row from the clustered B-tree using delete-mark (soft delete for MVCC).

**Algorithm (InnoDB-inspired):**
1. Search tree to find target cell
2. Set delete flag in RowHeader (txn_id_deleted = current_txn)
3. Record undo entry (UndoDelete: clear txn_id_deleted on rollback)
4. WAL log the delete
5. Physical removal deferred to vacuum/purge

**Acceptance criteria:**
- [ ] Soft delete via txn_id_deleted flag
- [ ] Undo support (restore on rollback)
- [ ] WAL logging
- [ ] Physical purge in vacuum pass (reclaim cell space)
- [ ] Page merge when occupancy < 25%

### 39.8 — Clustered B-Tree: Page Split & Merge

**What:** Handle page overflow (split) and underflow (merge) for variable-size cells.

**Split algorithm:**
1. Find split point by cumulative data volume (~50%)
2. Allocate new page
3. Move upper half cells to new page
4. Rebuild cell pointer arrays on both pages
5. Insert separator key in parent
6. Update next_leaf chain (old.next → new, new.next → old.next_was)

**Merge algorithm:**
1. Detect underflow (page < 25% full after delete)
2. Check sibling: if combined < 75% → merge
3. Move all cells from one sibling to another
4. Remove separator key from parent
5. Free empty page

**Acceptance criteria:**
- [ ] Split by data volume (not cell count)
- [ ] Correct next_leaf chain maintenance during split
- [ ] Merge with sibling when underflow
- [ ] Parent node pointer maintenance
- [ ] Concurrent safety (latching protocol)
- [ ] WAL logging for split/merge operations
- [ ] Stress test: random insert/delete 100K rows

### 39.9 — Secondary Index: PK-Value Bookmarks

**What:** Change secondary indexes to store PK values instead of RecordIds as bookmarks.

**Current secondary index entry:**
```
[index_key_columns] [RecordId: page_id(8) + slot_id(2)] = 10 bytes
```

**New secondary index entry:**
```
[index_key_columns] [PK_value_columns: variable]
```

**Secondary lookup flow:**
```
Sec Index lookup → extract PK value → Clustered Index lookup → full row
```

**Migration:** For clustered tables, encode PK value as bookmark. For heap tables (legacy), keep RecordId.

**Acceptance criteria:**
- [ ] Index entry format with PK value bookmark
- [ ] Secondary index → clustered index lookup path
- [ ] Encoding/decoding PK values in secondary entries
- [ ] Support composite PKs as bookmarks
- [ ] Index creation on clustered tables uses PK bookmarks
- [ ] Backward compatible: heap tables still use RecordId

### 39.10 — Overflow Pages for Large Rows

**What:** Handle rows that exceed ~4KB (don't fit on a single 16KB leaf page alongside other cells).

**Algorithm (SQLite-inspired):**
```
Cell on leaf page:
  [key_len: 2B] [total_row_len: 4B] [RowHeader: 24B]
  [key_data: key_len bytes]
  [local_row_data: min(row_len, max_local) bytes]
  [overflow_page_id: 8B if overflowed]

Overflow page chain:
  [next_overflow_page: 8B] [data: PAGE_SIZE - 8 bytes]
```

**max_local** = (PAGE_SIZE - header) / 4 ≈ 4KB — ensures at least 4 cells per leaf page.

**Acceptance criteria:**
- [ ] Overflow detection during insert
- [ ] Overflow page allocation and chaining
- [ ] Read: transparently reconstruct full row from overflow chain
- [ ] Delete: free overflow pages
- [ ] Update: handle overflow transition (small → large, large → small)
- [ ] WAL logging for overflow pages

### 39.11 — WAL Support for Clustered Operations

**What:** New WAL entry types for clustered index operations.

**New WAL entry types:**
- `ClusteredInsert` — insert cell into clustered leaf
- `ClusteredDelete` — delete-mark cell on clustered leaf
- `ClusteredUpdateInPlace` — in-place update on clustered leaf
- `ClusteredPageSplit` — record split operation for crash recovery
- `ClusteredPageMerge` — record merge operation

**Undo operations:**
- `UndoClusteredInsert` — remove cell from leaf
- `UndoClusteredDelete` — clear delete mark
- `UndoClusteredUpdate` — restore old cell data

**Acceptance criteria:**
- [ ] All new EntryType variants
- [ ] Serialization/deserialization
- [ ] Undo handlers in TxnManager
- [ ] Crash recovery handlers in recovery.rs
- [ ] Tests: crash during insert/update/delete/split

### 39.12 — Crash Recovery for Clustered Index

**What:** Ensure crash recovery correctly handles clustered index operations.

**Scenarios:**
1. Crash during insert (cell partially written)
2. Crash during page split (new page allocated but parent not updated)
3. Crash during delete (delete mark set but not committed)
4. Crash during update (old data in undo, new data partial)

**Acceptance criteria:**
- [ ] Recovery correctly undoes uncommitted clustered operations
- [ ] Page split recovery (detect orphan pages, repair parent pointers)
- [ ] Integrity checker validates clustered B-tree structure after recovery
- [ ] 10+ crash scenario tests with real I/O

### 39.13 — Executor Integration: CREATE TABLE with Clustered Index

**What:** When a table has a PRIMARY KEY, create it as a clustered table (data in B-tree, no heap).

**DDL behavior:**
```sql
CREATE TABLE users (
  id INT PRIMARY KEY,     -- → clustered index on id
  name TEXT,
  email TEXT
);
-- Data stored in clustered B-tree leaf pages
-- No heap chain allocated
-- Secondary indexes use PK value as bookmark
```

**Acceptance criteria:**
- [ ] Catalog stores `is_clustered` flag on TableDef
- [ ] CREATE TABLE with PK creates clustered B-tree root
- [ ] No heap chain allocated for clustered tables
- [ ] TableEngine dispatch: clustered vs heap based on flag
- [ ] DROP TABLE frees clustered B-tree pages

### 39.14 — Executor Integration: INSERT into Clustered Table

**What:** Route INSERT through the clustered B-tree instead of heap.

**Acceptance criteria:**
- [ ] INSERT encodes row and inserts into clustered B-tree
- [ ] Auto-increment PK support
- [ ] Duplicate PK detection (unique constraint from B-tree)
- [ ] Secondary index maintenance with PK bookmarks
- [ ] WAL logging
- [ ] Benchmark: should match or exceed current INSERT performance

### 39.15 — Executor Integration: SELECT from Clustered Table

**What:** Route SELECT through clustered B-tree scan or point lookup.

**Three access paths:**
1. **Point lookup (WHERE id = X):** Clustered B-tree search → return cell data
2. **Range scan (WHERE id BETWEEN X AND Y):** Leaf chain scan with prefetch
3. **Full scan (no WHERE or non-PK WHERE):** Leftmost leaf → chain traversal

**Acceptance criteria:**
- [ ] Planner chooses clustered scan for PK predicates
- [ ] Point lookup returns row without heap access
- [ ] Range scan uses leaf chain
- [ ] Full scan traverses all leaves
- [ ] BatchPredicate works on clustered leaf cells
- [ ] Benchmark: select_pk should improve to ~1.0x vs MariaDB

### 39.16 — Executor Integration: UPDATE on Clustered Table

**What:** Route UPDATE through clustered B-tree with in-place patching.

**Two paths:**
1. **In-place (non-key columns, row fits):** Modify cell data directly
2. **Pessimistic (key change or row grows):** Delete old cell + insert new cell

**Acceptance criteria:**
- [ ] In-place update for fixed-size column changes
- [ ] Field-patch fast path on clustered leaf cells
- [ ] Pessimistic fallback for key changes
- [ ] Secondary index maintenance when needed
- [ ] Benchmark: update_range should improve to ~0.75x+ vs MariaDB

### 39.17 — Executor Integration: DELETE from Clustered Table

**What:** Route DELETE through clustered B-tree with delete-mark.

**Acceptance criteria:**
- [ ] DELETE sets txn_id_deleted on clustered cell
- [ ] Secondary index cleanup
- [ ] VACUUM/purge physically removes dead cells and reclaims space
- [ ] Benchmark: should maintain current DELETE performance

### 39.18 — VACUUM for Clustered Index

**What:** Background process to physically remove dead cells from clustered leaf pages and reclaim space.

**Algorithm:**
1. Scan clustered leaves sequentially
2. For each dead cell (txn_id_deleted < oldest_active_snapshot):
   - Remove cell from page
   - Update cell pointer array
   - Reclaim space
3. If page underflow → try merge with sibling
4. Free overflow pages from dead cells

**Acceptance criteria:**
- [ ] Dead cell identification and removal
- [ ] Page compaction after removal
- [ ] Merge with sibling when underflow
- [ ] Overflow page cleanup
- [ ] Safe with concurrent readers (MVCC)
- [ ] WAL logging of vacuum operations

### 39.19 — Table Rebuild: Heap → Clustered Migration

**What:** `ALTER TABLE ... REBUILD` command to convert existing heap table to clustered format.

**Algorithm:**
1. Acquire exclusive lock on table
2. Create new clustered B-tree root
3. Scan old heap in PK order (via PK index)
4. Insert each row into clustered B-tree (bulk load: sorted insert)
5. Rebuild all secondary indexes with PK bookmarks
6. Update catalog: mark table as clustered
7. Free old heap pages and old PK index pages
8. Release lock

**Bulk load optimization:** Since rows arrive sorted by PK, use append-only leaf filling (no splits needed → much faster).

**Acceptance criteria:**
- [ ] ALTER TABLE ... REBUILD converts heap → clustered
- [ ] Sorted bulk load (no random inserts)
- [ ] Secondary index rebuild with PK bookmarks
- [ ] Catalog update (is_clustered flag)
- [ ] Old heap/index pages freed
- [ ] Transactional (rollback on failure)
- [ ] Wire protocol smoke test

### 39.20 — Integration Tests & Benchmarks

**What:** End-to-end validation and performance measurement.

**Tests:**
- CRUD operations on clustered tables
- Transactions (BEGIN/COMMIT/ROLLBACK) on clustered tables
- Crash recovery scenarios
- Mixed workload (clustered + heap tables coexisting)
- Large table (1M+ rows) stress test
- Concurrent access patterns

**Benchmarks:**
- local_bench.py with clustered tables
- Compare all scenarios: insert, select, update, delete, range scans
- Target: all PK-based operations at 🟡 0.75x+ vs MariaDB

**Acceptance criteria:**
- [ ] All existing tests pass with clustered tables
- [ ] 20+ new integration tests for clustered-specific scenarios
- [ ] Benchmark shows improvement for PK operations
- [ ] No regression for non-PK operations
- [ ] Wire protocol smoke test passes
