# Spec: 40.7 — HeapChain Concurrent Access

## What to build (not how)

Make heap page operations (INSERT, DELETE, UPDATE, SCAN) safe for multiple
concurrent transactions by integrating per-page latching, free-space-aware
insert distribution, and MVCC-based reader isolation. Two transactions inserting
into the same table must proceed in parallel when targeting different pages, and
serialize correctly when targeting the same page.

## Research findings

### InnoDB concurrent insert model (clustered index)
- **Mini-transaction (MTR)**: groups related page modifications as an atomic unit.
  All latches acquired during an MTR are released together on `mtr.commit()`.
  This ensures page splits appear atomic to other transactions.
- **Two-phase insert**: optimistic (leaf X-latch only) → pessimistic (tree X-latch
  from root down) if split needed. 95%+ inserts are optimistic (single page latch).
- **Last-insert hint**: each transaction caches the last page it inserted into.
  Reduces contention by avoiding all threads converging on the same tail page.
- **MVCC reader isolation**: `row_search_mvcc()` does NOT acquire S-latches for
  consistent reads. Visibility is determined by comparing `DB_TRX_ID` against the
  transaction's read view. Readers never block writers.

### PostgreSQL concurrent heap access
- **Free Space Map (FSM)**: separate B-tree tracking `block_id → free_bytes`.
  `GetPageWithFreeSpace(needed)` returns a page with sufficient space — distributes
  inserts across multiple pages, avoiding single-page contention.
- **Buffer X-lock per page**: `LockBuffer(buffer, BUFFER_LOCK_EXCLUSIVE)` before
  any modification. Released after modification + dirty mark. Concurrent inserts to
  different pages acquire different locks — full parallelism.
- **RelationGetBufferForTuple()** algorithm:
  1. Try cached target block (per-session `bulk_insert_state`)
  2. Query FSM for block with sufficient free space
  3. X-lock candidate → check actual free space (FSM may be stale)
  4. If insufficient → unlock, update FSM, try next candidate
  5. If all exhausted → extend relation (allocate new pages in bulk)
- **Readers don't latch**: SELECT scans pages without content locks. Tuple visibility
  via `xmin`/`xmax` headers (inline MVCC, like AxiomDB's `RowHeader`).

### Key patterns both databases share
1. **Page latch, not global lock**: concurrent modifications to different pages are parallel
2. **Hot-page avoidance**: distribute inserts across pages with free space
3. **MVCC for readers**: readers see consistent data without blocking writers
4. **Atomic multi-page ops**: page splits use latch coupling (hold parent + child simultaneously)

### Design choice for AxiomDB: PostgreSQL-inspired with InnoDB elements
- **Page X-latch from PageLockTable (40.3)**: already designed, per-page RwLock
- **Free space tracking**: lightweight per-chain hint (simpler than PostgreSQL's full FSM)
- **MVCC reader isolation**: already have RowHeader.txn_id_created/deleted + TransactionSnapshot
- **Mini-transaction-like batching**: group page latch + modify + WAL into atomic unit

## Current AxiomDB heap architecture (what changes)

### HeapChain today
```rust
// All methods take &mut dyn StorageEngine — single writer assumed
pub fn insert(storage: &mut dyn StorageEngine, root_page_id: u64,
              data: &[u8], txn_id: TxnId) -> Result<(u64, u16), DbError>
```

**Insert algorithm today:**
1. Start at `root_page_id`
2. Walk chain via `chain_next_page()` until finding page with space
3. If no page has space → allocate new page, link to chain
4. Insert tuple on found page
5. Return `(page_id, slot_id)`

**Problem for concurrency:**
- Walking the chain is a linear scan — all threads start at root
- No free space tracking — every thread scans from root every time
- Chain growth (allocating new page) is not atomic
- Two threads inserting on same page would corrupt slot directory

### HeapChain after 40.7

**Three key changes:**

#### Change 1: Page X-latch during modification
Every heap page mutation (insert, delete, update) acquires the page's exclusive
lock from PageLockTable (40.3) before modifying, releases after.

```
INSERT:
  1. Find target page (see Change 2)
  2. Acquire page X-latch
  3. Insert tuple
  4. Mark dirty
  5. Release page X-latch
```

Two inserts to different pages: different X-latches → parallel.
Two inserts to same page: same X-latch → serialized. Second waits ~1µs.

#### Change 2: Per-chain insert hint (hot-page avoidance)
Instead of PostgreSQL's full FSM, use a simpler approach:

```rust
/// Per-table insert hint: last page with known free space.
/// Avoids all threads starting at root and scanning the entire chain.
pub struct HeapInsertHint {
    /// Last page that had free space (atomic for concurrent access).
    last_page_with_space: AtomicU64,
    /// Estimated free bytes on that page (may be stale).
    estimated_free: AtomicU32,
}
```

**Insert algorithm with hint:**
1. Load `last_page_with_space` (atomic, Relaxed)
2. X-latch that page
3. Check actual free space
   - If enough → insert, update hint with new free estimate
   - If not enough → release, walk forward in chain from this page
4. If no page found → allocate new page, link to chain end
5. Update hint to new page

**Why simpler than FSM:**
- AxiomDB tables are typically <10K pages — linear scan from hint is fast
- FSM adds complexity (separate B-tree per table) without proportional benefit
- Hint alone eliminates 90%+ of unnecessary chain scans

#### Change 3: Atomic chain growth
When a new page must be added to the chain:

```
1. Acquire allocator lock (Mutex from 40.3)
2. Allocate new page
3. X-latch new page
4. Initialize page (header, zero slots)
5. X-latch last page in chain (the one that will point to new page)
6. Set last_page.next_page = new_page_id
7. Release last_page X-latch
8. Insert tuple into new page
9. Release new page X-latch
10. Update hint to new page
```

**Latch ordering**: new_page latch before last_page latch (prevents deadlock:
new page is freshly allocated, no one else can hold its latch).

#### Change 4: MVCC reader isolation (no change needed)
Readers already use `RowHeader.is_visible(snapshot)` — no page latches needed.
The `scan_table_filtered()` path reads pages and checks visibility per-slot.
Under concurrency, a reader might see a partially-filled page (new slots added
after reader started), but MVCC ensures it only sees committed rows.

**One subtlety:** reader must handle the case where a slot was allocated (slot
entry written) but the row is not yet visible (txn not committed). The existing
`is_visible()` check handles this correctly — uncommitted rows have
`txn_id_created > snapshot_id`.

## Detailed operation protocols

### INSERT with row-level lock

```
1. Acquire IX(table) from LockManager (40.5)
2. Find target page via HeapInsertHint
3. Acquire page X-latch (PageLockTable)
4. Check free space
   - Insufficient? → release X-latch, try next page in chain
5. Insert tuple (write RowHeader + data into slot)
6. Acquire X(row) from LockManager (for the new slot)
7. Mark page dirty
8. Release page X-latch
9. Record WAL entry (via ConnectionTxn wal scratch)
10. Update HeapInsertHint
11. Return (page_id, slot_id)
```

### DELETE with row-level lock

```
1. Acquire IX(table) from LockManager
2. Acquire X(row) from LockManager (wait if another txn holds S or X)
3. Acquire page X-latch
4. Set RowHeader.txn_id_deleted = current_txn_id
5. Mark page dirty
6. Release page X-latch
7. Record WAL entry
```

### UPDATE (in-place) with row-level lock

```
1. Acquire IX(table) from LockManager
2. Acquire X(row) from LockManager
3. Acquire page X-latch
4. Modify row data in-place (field patch or full rewrite if fits)
5. Increment RowHeader.row_version
6. Mark page dirty
7. Release page X-latch
8. Record WAL entry
```

### SCAN (SELECT) — no page latch, MVCC only

```
1. Acquire IS(table) from LockManager
2. For each page in chain:
   a. Read page (no latch — StorageEngine.read_page returns owned copy)
   b. For each slot:
      - Check RowHeader.is_visible(snapshot)
      - If visible → decode and include in results
      - Optionally acquire S(row) for REPEATABLE READ (prevents concurrent delete)
3. Follow chain_next_page to next page
```

**Why no page latch for read:**
- `read_page()` returns an owned `Page` copy (or `PageRef`)
- The copy is a consistent snapshot of the page at read time
- MVCC handles visibility of individual rows
- A concurrent writer modifying the same page after the read doesn't affect the copy

## Concurrency guarantees

| Scenario | Behavior | Mechanism |
|---|---|---|
| 2 INSERTs, different pages | **Parallel** | Different page X-latches |
| 2 INSERTs, same page | **Serialized** | Same page X-latch (~1µs wait) |
| INSERT + SELECT, same page | **Parallel** | INSERT: page X-latch. SELECT: reads copy, no latch |
| 2 UPDATEs, same row | **Serialized** | Row X-lock from LockManager (40.5) |
| 2 UPDATEs, different rows same page | **Serialized at page level** | Same page X-latch (brief) |
| DELETE + SELECT, same row | **Parallel** | DELETE marks txn_id_deleted. SELECT sees old value via MVCC |
| Chain growth during scan | **Safe** | New page appended at end. Scanner at earlier page unaffected |
| Chain growth during insert | **Serialized** | Allocator Mutex (40.3) + page latches |

## Use cases

1. **Bulk INSERT (single table, 8 threads):**
   Thread 1 fills page 100 → hint=100. Thread 2 sees hint=100, finds full, moves to 101.
   Thread 3 starts at 101 (hint updated). Threads spread across pages 100-107.
   8 pages modified in parallel → ~8× throughput vs single writer.

2. **SELECT during INSERT:**
   Reader scans pages 1-50. Writer inserts on page 51. No contention — different pages,
   reader uses MVCC for visibility.

3. **Two UPDATEs on same row:**
   Txn A acquires X(row 42). Txn B requests X(row 42) → waits in LockManager queue.
   Txn A commits → LockManager grants X to Txn B → Txn B proceeds.

4. **INSERT when all pages full:**
   Thread sees hint page is full. Scans forward, finds all full. Acquires allocator Mutex,
   allocates new page, links to chain. Other threads see updated hint → go to new page.

## Acceptance criteria

- [ ] Heap INSERT acquires page X-latch from PageLockTable before modification
- [ ] Heap DELETE acquires row X-lock + page X-latch before marking deleted
- [ ] Heap UPDATE acquires row X-lock + page X-latch before modifying
- [ ] Heap SCAN works without page latches (MVCC-only reader isolation)
- [ ] HeapInsertHint distributes inserts across pages (avoids hot-page contention)
- [ ] Chain growth is atomic (allocate + link under proper latching)
- [ ] Latch ordering: allocator Mutex < new_page X-latch < existing_page X-latch
- [ ] 4 threads × 1000 inserts to same table → no corruption, all rows inserted
- [ ] 2 threads insert to different pages → verified parallel execution
- [ ] Concurrent INSERT + SELECT → reader sees consistent MVCC snapshot
- [ ] All existing heap/heap_chain tests pass
- [ ] Stress test: 8 threads mixed INSERT/DELETE/UPDATE/SELECT → no corruption

## Out of scope

- Full Free Space Map (PostgreSQL-style separate B-tree) — HeapInsertHint suffices
- Page compaction under concurrency (vacuum handles this offline)
- TOAST / overflow pages concurrent access (Phase 39.10)
- Clustered index concurrent access (same latch protocol applies, but separate spec)

## Dependencies

- 40.3 (StorageEngine interior mutability) — PageLockTable provides per-page RwLocks
- 40.5 (Lock Manager) — row-level S/X locks for DML operations
- 40.2 (Per-connection txn) — each connection holds its own lock list and undo log
