# Storage Engine

The storage engine is the lowest user-accessible layer in AxiomDB. It manages raw
16-kilobyte pages on disk or in memory, provides a freelist for page allocation, and
exposes a simple trait that all higher layers depend on.

---

## The StorageEngine Trait

```rust
pub trait StorageEngine: Send {
    fn read_page(&self, page_id: u64) -> Result<PageRef, DbError>;
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;
    fn flush(&mut self) -> Result<(), DbError>;
    fn page_count(&self) -> u64;
}
```

`read_page` returns an owned `PageRef` — a heap-allocated copy of the 16 KB page data.
This is a deliberate change from the original `&Page` borrow: owned pages survive mmap
remaps (during `grow()`) and page reuse (after `free_page`), which is essential for
concurrent read/write access. The copy cost is ~0.5 us from L2/L3 cache — the same cost
PostgreSQL pays when copying a page from the buffer pool into backend-local memory.

---

## Page Format

Every page is exactly `PAGE_SIZE = 16,384` bytes (16 KB). The first `HEADER_SIZE = 64`
bytes are the page header; the remaining `PAGE_BODY_SIZE = 16,320` bytes are the body.

### Page Header — 64 bytes

```text
Offset   Size   Field            Description
──────── ────── ──────────────── ──────────────────────────────────────
     0      8   magic            `PAGE_MAGIC` — identifies valid pages
     8      1   page_type        PageType enum (see below)
     9      1   flags            page flags (`PAGE_FLAG_ALL_VISIBLE`, future bits)
    10      2   item_count       item/slot count for the page-local format
    12      4   checksum         CRC32c of body bytes `[HEADER_SIZE..PAGE_SIZE]`
    16      8   page_id          This page's own ID (self-identifying)
    24      8   lsn              Log Sequence Number of last write
    32      2   free_start       First free byte offset in the body (format-specific)
    34      2   free_end         Last free byte offset in the body (format-specific)
    36     28   _reserved        Future use
Total:    64 bytes
```

The CRC32c checksum covers only the page body `[HEADER_SIZE..PAGE_SIZE]`, not the
header itself. On every `read_page`, AxiomDB verifies the checksum and returns
`DbError::ChecksumMismatch` if it fails.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — CRC32c Plus Separate Doublewrite</span>
InnoDB historically coupled torn-page repair to an internal doublewrite area in the system tablespace. AxiomDB keeps the page format simpler: per-page CRC32c detects corruption, and the repair copy lives in a separate `.dw` file instead of inside the main database file.
</div>
</div>

### Page Types

```rust
pub enum PageType {
    Meta              = 0,  // page 0: database header + catalog roots
    Data              = 1,  // heap pages holding table rows
    Index             = 2,  // current fixed-slot B+ Tree internal and leaf nodes
    Overflow          = 3,  // continuation pages for large values
    Free              = 4,  // freelist / unused pages
    ClusteredLeaf     = 5,  // slotted clustered leaf: full PK row inline
    ClusteredInternal = 6,  // slotted clustered internal: varlen separators
}
```

### Clustered Page Primitives (Phase 39.1 / 39.2 / 39.3)

The clustered index rewrite is landing in the storage layer first. Two new page
types now exist even though the SQL executor still uses the classic heap +
secondary-index path:

- `ClusteredLeaf` — slotted page with variable-size cells storing:
  - `key_len`
  - `row_len`
  - inline `RowHeader`
  - primary-key bytes
  - row payload bytes
- `ClusteredInternal` — slotted page with variable-size separator cells storing:
  - `right_child`
  - `key_len`
  - separator key bytes

`ClusteredInternal` keeps one extra child pointer in the header as
`leftmost_child`, so logical child access still follows the classical B-tree
rule `n keys -> n + 1 children`.

```text
ClusteredInternal body:
  [16B header: is_leaf | num_cells | cell_content_start | freeblock_offset | leftmost_child]
  [cell pointer array]
  [free gap]
  [cells: right_child | key_len | key_bytes]
```

That design keeps the storage primitive compatible with the current traversal
contract:

- `find_child_idx(search_key)` returns the first separator strictly greater than the key
- `child_at(0)` reads `leftmost_child`
- `child_at(i > 0)` reads the `right_child` of separator cell `i - 1`

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — SQLite-Style Slots, B-Tree Semantics</span>
SQLite-style slotted pages solve variable-size key storage cleanly, but clustered internal pages still need classic B-tree child semantics. AxiomDB adapts the pattern by storing `leftmost_child` in the header and the remaining children inside separator cells, avoiding the fixed 64-byte key cap of the old `InternalNodePage`.
</div>
</div>

### SQL-Visible Clustered DDL Boundary (Phase 39.13)

The storage rewrite is no longer purely internal. `CREATE TABLE` now uses the
clustered root when the SQL definition contains an explicit `PRIMARY KEY`:

- `TableDef.root_page_id` is the generic primary row-store root
- `TableDef.storage_layout` tells higher layers whether that root is heap or clustered
- heap tables still allocate `PageType::Data`
- clustered tables now allocate `PageType::ClusteredLeaf`
- logical PRIMARY KEY metadata on clustered tables points at that same clustered root

The executor still does **not** read or write clustered rows through SQL in
`39.13`; heap-only runtime paths fail explicitly instead of treating a
clustered root page as heap storage.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — No Hidden Clustered Key</span>
AxiomDB only creates clustered SQL tables when the schema has an explicit `PRIMARY KEY`. That mirrors SQLite `WITHOUT ROWID` more closely than InnoDB's fallback `GEN_CLUST_INDEX` path and avoids baking a hidden-key compromise into the first clustered executor boundary.
</div>
</div>

### Clustered Tree Insert Controller (Phase 39.3)

`axiomdb-storage::clustered_tree` now builds the first tree-level write path on
top of these page primitives. The public entry point is:

```rust
pub fn insert(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<u64, DbError>
```

The controller is still storage-first:

1. Bootstrap an empty tree into a `ClusteredLeaf` root when `root_pid` is `None`.
2. Descend through `ClusteredInternal` pages with `find_child_idx()`.
3. Materialize a clustered leaf descriptor:
   - small rows stay fully inline
   - large rows keep a local prefix inline and spill the tail bytes to overflow pages
4. Insert that descriptor into the target leaf in sorted key order.
5. If the descriptor does not fit, defragment once and retry before splitting.
6. Split leaves by cumulative cell byte volume, not by cell count.
7. Propagate `(separator_key, right_child_pid)` upward.
8. Split internal pages by cumulative separator byte volume and create a new
   root if the old root overflows.

Split behavior deliberately keeps the old page ID as the left half and
allocates only the new right sibling. That matches the current no-concurrent-
clustered-writer reality and keeps parent maintenance minimal until the later
MVCC/WAL phases wire clustered pages into the full engine.

Since `39.10`, rows above the local inline budget are no longer rejected. The
leaf keeps the primary key and `RowHeader` inline, stores only a bounded local
row prefix on-page, and spills the remaining tail bytes to a dedicated
`PageType::Overflow` chain.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Split By Bytes, Not Count</span>
InnoDB and SQLite both have to reason about variable-size leaf contents during page split. AxiomDB follows that constraint directly: clustered leaves and internals split by cumulative encoded bytes, because a 3 KB row and a 40-byte row are not equivalent occupancy units.
</div>
</div>

### Clustered Point Lookup (Phase 39.4)

`axiomdb-storage::clustered_tree::lookup(...)` is now the first read path over
the clustered tree:

```rust
pub fn lookup(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    snapshot: &TransactionSnapshot,
) -> Result<Option<ClusteredRow>, DbError>
```

Lookup flow:

1. Return `None` immediately when the tree has no root.
2. Descend clustered internal pages with `find_child_idx()` and `child_at()`.
3. Run exact-key binary search on the target clustered leaf.
4. Read the leaf descriptor `(key, RowHeader, total_row_len, local_prefix, overflow_ptr?)`.
5. Apply `RowHeader::is_visible(snapshot)`.
6. If the row is overflow-backed, reconstruct the logical row bytes by reading
   the overflow-page chain.
7. Return an owned `ClusteredRow` on a visible hit.

In `39.4`, lookup is intentionally conservative about invisible rows: when the
current inline version fails MVCC visibility, it returns `None` instead of
trying to synthesize an older version. Clustered undo/version-chain traversal
for arbitrary snapshots still does not exist; `39.11` adds rollback/savepoint
restore for clustered writes, but not older-version reconstruction on reads.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Invisible Means Absent For Now</span>
PostgreSQL and InnoDB can reconstruct older visible versions because they already have undo/version-chain machinery. AxiomDB deliberately does not fake that in 39.4: even after 39.11 adds rollback-only clustered WAL, an invisible current inline version is still reported as `None` until true older-version reconstruction exists.
</div>
</div>

### Clustered Range Scan (Phase 39.5)

`axiomdb-storage::clustered_tree::range(...)` is now the first ordered multi-row
read path over clustered pages:

```rust
pub fn range<'a>(
    storage: &'a dyn StorageEngine,
    root_pid: Option<u64>,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    snapshot: &TransactionSnapshot,
) -> Result<ClusteredRangeIter<'a>, DbError>
```

Range flow:

1. Return an empty iterator when the tree is empty or the bound interval is empty.
2. For bounded scans, descend to the first relevant leaf with the same clustered
   internal-page search path used by point lookup.
3. For unbounded scans, descend to the leftmost leaf.
4. Start at the first in-range slot within that leaf.
5. Yield owned `ClusteredRow` values in primary-key order.
6. Skip current inline versions that are invisible to the supplied snapshot.
7. Follow `next_leaf` to continue the scan across leaves.
8. Stop immediately when the first key above the upper bound is seen.

The iterator stays lazy: it keeps only the current leaf page id, slot index,
bound copies, and snapshot. It does not materialize the whole range into a
temporary vector.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Seek Once, Then Advance</span>
MariaDB's `read_range_first()` / `read_range_next()` and SQLite's `sqlite3BtreeFirst()` / `sqlite3BtreeNext()` both separate “find the first row” from “advance the cursor”. AxiomDB adapts that same shape to clustered storage: one tree descent to the start leaf, then O(1) `next_leaf` traversal per leaf boundary.
</div>
</div>

When the iterator advances to another leaf, it calls
`StorageEngine::prefetch_hint(next_leaf_pid, 4)`. The 4-page window is
intentionally conservative: large enough to overlap sequential leaf reads, but
small enough not to flood the page cache while clustered scans are still an
internal storage primitive.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Small Prefetch Window</span>
PostgreSQL uses bounded prefetch windows instead of reading arbitrarily far ahead. AxiomDB keeps clustered scan read-ahead at 4 leaves for now: enough to overlap I/O on sequential scans without turning an internal storage walk into a cache-pollution policy.
</div>
</div>

Like `39.4`, this subphase is still honest about missing older-version
reconstruction. If a row's current inline version is invisible, `39.5` skips
it; the new `39.11` rollback support does not change read semantics yet.

### Clustered Overflow Pages (Phase 39.10)

Phase `39.10` adds the first overflow-page primitive dedicated to clustered
rows:

```text
Leaf cell:
  [key_len: u16]
  [total_row_len: u32]
  [RowHeader: 24B]
  [key bytes]
  [local row prefix]
  [overflow_first_page?: u64]

Overflow page body:
  [next_overflow_page: u64]
  [payload bytes...]
```

The contract is intentionally physical:

1. Keep the primary key and `RowHeader` inline in the clustered leaf.
2. Keep only a bounded local row prefix inline.
3. Spill the remaining logical row tail to `PageType::Overflow` pages.
4. Reconstruct the full logical row only on read paths (`lookup`, `range`) or
   update paths that need the logical bytes.
5. Let split / merge / rebalance move the physical descriptor without rewriting
   the overflow payload.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — SQLite Tail Spill, InnoDB Scope</span>
SQLite's B-tree format keeps a local payload prefix inline and spills only the surplus to overflow pages, while InnoDB restricts off-page storage to clustered records rather than secondary entries. AxiomDB adapts both ideas directly in 39.10: keep clustered row identity and MVCC header inline, spill only the row tail, and keep secondary indexes bookmark-only.
</div>
</div>

Phase `39.10` itself intentionally did **not** introduce generic TOAST
references, compression, or crash recovery for overflow chains. `39.11` now
adds in-process clustered WAL/rollback over those row images, but clustered
crash recovery still stays in later phases.

### Clustered WAL and Rollback (Phase 39.11)

Phase `39.11` adds the first WAL contract that understands clustered rows:

```text
key       = primary-key bytes
old_value = ClusteredRowImage?   // exact old row image
new_value = ClusteredRowImage?   // exact new row image
```

Where `ClusteredRowImage` carries:

- the latest clustered `root_pid`
- the exact inline `RowHeader`
- the exact logical row bytes, regardless of whether the row is inline or
  overflow-backed on page

`TxnManager` now tracks the latest clustered root per `table_id` during the
active transaction. Rollback and savepoint undo use that root plus two storage
helpers:

- `delete_physical_by_key(...)` to undo a clustered insert
- `restore_exact_row_image(...)` to undo clustered delete-mark or update

The restore invariant is logical row state, not exact page topology. Split,
merge, or relocate-update may still leave a different physical tree shape after
rollback as long as the old primary key, `RowHeader`, and row bytes are back.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — WAL Follows PK Identity</span>
InnoDB clustered undo also follows clustered-row identity rather than a heap slot. AxiomDB adopts that same constraint in 39.11: clustered pages can defragment and relocate rows, so rollback keys by primary key plus exact row image instead of pretending `(page_id, slot_id)` stays stable.
</div>
</div>

Phase `39.12` now extends that same contract into clustered crash recovery:
`open_with_recovery()` undoes in-progress clustered writes by PK + exact row
image, and `open()` rebuilds committed clustered roots from surviving WAL
history on a clean reopen.

### Clustered Update In Place (Phase 39.6)

`axiomdb-storage::clustered_tree::update_in_place(...)` is now the first
clustered-row write path after insert:

```rust
pub fn update_in_place(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    new_row_data: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<bool, DbError>
```

Update flow:

1. Return `false` when the tree is empty, the key is absent, or the current inline
   version is not visible to the supplied snapshot.
2. Descend to the owning clustered leaf by primary key.
3. Build a new inline `RowHeader` with:
   - `txn_id_created = txn_id`
   - `txn_id_deleted = 0`
   - `row_version = old.row_version + 1`
4. Materialize a replacement descriptor:
   - inline row
   - or local-prefix + overflow chain
5. Ask the leaf primitive to rewrite that exact cell while preserving key order.
6. Persist the leaf if the rewrite stays inside the same page.
7. Free the obsolete overflow chain only after a successful physical rewrite.
8. Return `HeapPageFull` when the replacement row would require leaving the
   current leaf.

The leaf primitive has two rewrite modes:

- **overwrite fast path** when the replacement encoded cell fits the existing
  cell budget
- **same-leaf rebuild fallback** when the row grows, but the leaf can still be
  rebuilt compactly with the replacement row in place

Neither path changes the primary key, pointer-array order, parent separators, or
`next_leaf`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Same Leaf Or Explicit Failure</span>
SQLite has an overwrite optimization for unchanged entry budgets, but AxiomDB stops short of full delete+insert tree surgery in 39.6. If the new row no longer fits in the owning clustered leaf, the engine returns `HeapPageFull` explicitly and leaves structural relocation for later clustered split/overflow phases.
</div>
</div>

This keeps the subphase honest about what now exists:

- clustered insert
- clustered point lookup
- clustered range scan
- clustered same-leaf update
- clustered delete-mark

And what still does not:

- clustered older-version reconstruction/version chains
- clustered root persistence beyond WAL checkpoint/rotation
- clustered physical purge
- clustered SQL executor integration

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — No Fake Old Versions</span>
PostgreSQL HOT chains and InnoDB undo can make an updated row visible to older snapshots. AxiomDB still cannot do that in 39.6, so updating a row rewrites the current inline version only and leaves older-version visibility for later clustered MVCC/version-chain work.
</div>
</div>

### Clustered Delete Mark (Phase 39.7)

`axiomdb-storage::clustered_tree::delete_mark(...)` now adds the first logical
delete path over clustered pages:

```rust
pub fn delete_mark(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<bool, DbError>
```

Delete flow:

1. Return `false` when the tree is empty, the key is absent, or the current
   inline version is not visible to the supplied snapshot.
2. Descend to the owning clustered leaf by primary key.
3. Build a replacement `RowHeader` that preserves:
   - `txn_id_created`
   - `row_version`
   - `_flags`
   and stamps `txn_id_deleted = txn_id`.
4. Rewrite the exact clustered cell in place while preserving key bytes and row
   payload bytes.
5. Persist the leaf page without changing `next_leaf` or parent separators.

The important semantic boundary is that clustered delete is currently a
**header-state transition**, not space reclamation. The physical cell stays on
the leaf page so snapshots older than the delete can still observe it through
the existing `RowHeader::is_visible(...)` rule.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Delete Mark Before Purge</span>
InnoDB delete-marks clustered records first and purges them later; PostgreSQL also separates tuple visibility from later vacuum cleanup. AxiomDB follows that same separation in 39.7: stamp `txn_id_deleted` now, defer physical removal to the future clustered purge phase.
</div>
</div>

### Clustered Structural Rebalance (Phase 39.8)

`axiomdb-storage::clustered_tree::update_with_relocation(...)` adds the first
clustered structural-maintenance path:

```rust
pub fn update_with_relocation(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    new_row_data: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<Option<u64>, DbError>
```

Control flow:

1. Validate that the replacement row still fits inline on a clustered leaf.
2. Try `update_in_place(...)` first.
3. If the same-leaf rewrite returns `HeapPageFull`, reload the visible current
   row and enter the structural path.
4. Physically remove the exact clustered cell from the tree.
5. Bubble `underfull` and `min_changed` upward:
   - repair the parent separator when a non-leftmost child changes its minimum key
   - redistribute or merge clustered leaf siblings by encoded byte volume
   - redistribute or merge clustered internal siblings while preserving
     `n keys -> n + 1 children`
6. Collapse an empty internal root to its only child.
7. Reinsert the replacement row with bumped `row_version`.

The key design boundary is that `39.8` introduces **private structural delete**
only for relocate-update. Public clustered delete is still `delete_mark(...)`,
so snapshot-safe purge remains a later concern.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Rebalance By Bytes</span>
SQLite triggers rebalance from page occupancy, not from a fixed `MIN_KEYS` rule, and InnoDB also reasons about merge feasibility in bytes after page reorganization. AxiomDB adopts that same rule in 39.8: variable-size clustered siblings redistribute and merge by encoded byte volume, not by raw key count.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Relocation Still Is Not Undo</span>
PostgreSQL and InnoDB can preserve older visible versions through undo/version chains. AxiomDB still cannot do that in 39.8, so relocate-update rewrites only the current inline version and leaves old-version reconstruction for later clustered MVCC/version-chain work.
</div>
</div>

Current limitations:

- `delete_mark(...)` still keeps dead clustered cells inline; `39.8` does not
  expose purge to SQL or storage callers yet.
- relocate-update still rewrites only the current inline version.
- parent separator repair currently assumes the repaired separator still fits in
  the existing internal page budget; split-on-separator-repair is deferred.

### Clustered Secondary Bookmarks (Phase 39.9)

Phase `39.9` adds the first clustered-first secondary-index layout in
`axiomdb-sql/src/clustered_secondary.rs`.

The physical key is:

```text
secondary_logical_key ++ missing_primary_key_columns
```

Where:

1. `secondary_logical_key` is the ordered value vector of the secondary index columns.
2. `missing_primary_key_columns` are only the PK columns that are not already
   present in the secondary key.

That means the physical secondary entry now carries enough information to
recover the owning clustered row by primary key without depending on a heap
`RecordId`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Bookmark In The Key</span>
InnoDB secondary records carry clustered-key fields, and SQLite `WITHOUT ROWID` appends the table key to secondary indexes. AxiomDB adapts that same idea in 39.9 by embedding the missing PK columns in the physical secondary key instead of inventing a clustered-only side payload.
</div>
</div>

The dedicated helpers now provide:

- layout derivation from `(secondary_idx, primary_idx)`
- encode/decode of bookmark-bearing secondary keys
- logical-prefix bounds without a fixed 10-byte RID suffix
- insert/delete/update maintenance where relocate-only updates become no-ops if
  the logical secondary key and primary key stay stable

Current boundary:

- this path is **not** wired into the heap-backed SQL executor yet
- FK enforcement and index-integrity rebuilds still use the old
  `RecordId`-based secondary path
- the legacy `RecordId` payload in `axiomdb-index::BTree` remains only a
  compatibility artifact for this path

---

## MmapStorage — Memory-Mapped File

`MmapStorage` uses a hybrid I/O model inspired by SQLite: **read-only mmap for reads,
`pwrite()` for writes**. The mmap is opened with `memmap2::Mmap` (not `MmapMut`),
making it structurally impossible to write through the mapped region.

```
Physical file (axiomdb.db):
┌──────────┬──────────┬──────────┬──────────┬──────────┐
│  Page 0  │  Page 1  │  Page 2  │  Page 3  │  ...     │
│ (Meta)   │ (Data)   │ (Index)  │ (Data)   │          │
└──────────┴──────────┴──────────┴──────────┴──────────┘
     ↑           ↑                     ↓
     │           └── read_page(1): copy 16KB from mmap → owned PageRef
     └── mmap (read-only, MAP_SHARED)
                                  write_page(3): pwrite() to file descriptor
```

### Read path: mmap + PageRef copy

`read_page(page_id)` computes `mmap_ptr + page_id * 16384`, copies 16 KB into a
heap-allocated `PageRef`, verifies the CRC32c checksum, and returns the owned copy.
The copy cost (~0.5 us from L2/L3 cache) is the same price PostgreSQL pays when
copying a buffer pool page into backend-local memory.

### Write path: pwrite() to file descriptor

`write_page(page_id, page)` calls `pwrite()` on the underlying file descriptor at
offset `page_id * 16384`. The mmap (MAP_SHARED) automatically reflects the change
on subsequent reads. Note that a 16 KB `pwrite()` is **not** crash-atomic on 4 KB-block
filesystems — the [Doublewrite Buffer](#doublewrite-buffer) protects against torn pages.

### Flush: doublewrite + fsync

`flush()` follows a two-phase write protocol:

1. **Doublewrite phase:** all dirty pages (plus pages 0 and 1) are serialized to a
   `.dw` file and fsynced. This creates a durable copy of the committed state.
2. **Main fsync:** the freelist is pwritten (if modified) and the main `.db` file is
   fsynced. If this fsync is interrupted by a crash, the `.dw` file provides repair
   data on the next startup.
3. **Cleanup:** the `.dw` file is deleted. If deletion fails, the next `open()` finds
   all pages valid and removes it.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Read-Only Mmap + pwrite (SQLite Model)</span>
No production database uses mmap for writes. PostgreSQL uses pwrite + buffer pool,
InnoDB uses pwrite + doublewrite buffer, DuckDB uses pwrite exclusively, and SQLite
uses mmap for reads + pwrite for writes. AxiomDB follows the SQLite model: mmap gives
zero-copy reads from the OS page cache, while pwrite provides coherent page writes
visible through the mmap. A 16 KB pwrite is NOT crash-atomic on 4 KB-block filesystems
(ext4, APFS, XFS) — a crash mid-write leaves a torn page. AxiomDB detects torn pages
via CRC32c checksums and repairs them from the doublewrite buffer on startup.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">No Double-Buffer Overhead</span>
MySQL InnoDB keeps every hot page in RAM twice — once in the OS page cache, once in
the InnoDB buffer pool. AxiomDB's mmap approach uses the OS page cache directly. For a
working set that fits in RAM, this roughly halves the memory footprint of the storage
layer.
</div>
</div>

**Trade-offs:**
- We cannot control which pages stay hot in memory (the OS uses LRU).
- On 32-bit systems, the address space limits the maximum database size.
  On 64-bit, the address space is effectively unlimited.
- `PageRef` copies add ~0.5 us per page read vs. direct pointer access, but this
  eliminates use-after-free risks from mmap remap and page reuse.

### Deferred Page Free Queue

When `free_page(page_id)` is called, the page does **not** return to the freelist
immediately. Instead it enters an epoch-tagged queue: `deferred_frees: Vec<(page_id,
freed_at_snapshot)>`. Each entry records the snapshot epoch at which the page became
unreachable. `release_deferred_frees(oldest_active_snapshot)` only releases pages
whose `freed_at_snapshot <= oldest_active_snapshot` — pages freed more recently remain
queued because a concurrent reader might still hold a snapshot that references them.

Under the current `Arc<RwLock<Database>>` architecture, `flush()` passes `u64::MAX`
(release all) because the writer holds exclusive access and no readers are active.
When snapshot slot tracking is added (Phase 7.8), the actual oldest active snapshot
will be used instead. The queue is capped at 4096 entries with a tracing warning
to detect snapshot leaks.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Deferred Frees — Simplified Epoch Reclamation</span>
PostgreSQL uses buffer pins to prevent eviction while a backend reads a page. DuckDB
uses block reference counts. AxiomDB's deferred free queue achieves the same safety
with less complexity: freed pages are quarantined until all concurrent readers that
could reference them have completed.
</div>
</div>

---

## Doublewrite Buffer

A 16 KB `pwrite()` is **not** crash-atomic on any modern filesystem with 4 KB
internal blocks (APFS, ext4, XFS, ZFS). A power failure mid-write leaves a **torn
page**: the first N×4 KB contain new data, the remainder holds the previous state.
CRC32c detects this corruption on startup, but without a repair source the database
cannot open.

The **doublewrite (DW) buffer** solves this. Before every `flush()`, all dirty pages
are serialized to a `.dw` file alongside the main `.db` file:

```text
database.db      ← main data file
database.db.dw   ← doublewrite buffer (transient, exists only during flush)
```

### DW File Format

```text
[Header: 16 bytes]
  magic:       "AXMDBLWR"  (8 bytes)
  version:     u32 LE = 1
  slot_count:  u32 LE

[Slots: slot_count × 16,392 bytes each]
  page_id:     u64 LE
  page_data:   [u8; 16384]

[Footer: 8 bytes]
  file_crc:    CRC32c(header || all slots)
  sentinel:    0xDEAD_BEEF
```

### Flush Protocol

```text
1. Collect dirty pages + pages 0 and 1 from the mmap
2. Write all to .dw file → single sequential write
3. fsync .dw file                    ← committed copy durable
4. pwrite freelist to main file
5. fsync main file                   ← main data durable
6. Delete .dw file                   ← cleanup (non-fatal on failure)
```

### Startup Recovery

On `MmapStorage::open()`, if a `.dw` file exists:

1. Validate the DW file (magic, version, size, CRC, sentinel)
2. For each slot: read the corresponding page from the main file
3. If CRC is invalid (torn page) → restore from DW copy
4. fsync the main file → repairs durable
5. Delete the `.dw` file

Recovery is **idempotent**: if interrupted, the DW file is still valid and the next
startup reruns recovery. Pages already repaired have valid CRCs and are skipped.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Separate DW File (MySQL 8.0.20+ Model)</span>
InnoDB originally embedded the doublewrite buffer inside the system tablespace (2 × 64
pages = 2 MB). MySQL 8.0.20 moved it to a separate <code>#ib_*.dblwr</code> file for
better sequential I/O and zero impact on the tablespace format. AxiomDB follows this
newer approach: the DW file is sequential-write-only, does not change the main file
format, and requires no migration for existing databases.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Orthogonal to WAL — Protects All Pages</span>
PostgreSQL's full-page writes (FPW) only protect WAL-logged pages and inflate the WAL
by up to 16 KB per page per checkpoint cycle. AxiomDB's doublewrite buffer protects
<strong>all</strong> pages — data, index, meta, and freelist — without changing the WAL
format or increasing WAL size. The extra cost is one additional fsync per flush plus 2×
write amplification for dirty pages, the same trade-off InnoDB has made since its
inception.
</div>
</div>

---

## Dirty Page Tracking and Targeted Flush

`MmapStorage` tracks every page written since the last `flush()` in a
`PageDirtyTracker` (an in-memory `HashSet<u64>`). On `flush()`, instead of
calling `mmap.flush()` (which issues `msync` over the entire file), AxiomDB
coalesces the dirty page IDs into contiguous runs and issues one `flush_range`
call per run.

### Coalescing algorithm

`PageDirtyTracker::contiguous_runs()` sorts the dirty IDs and merges adjacent
IDs into `(start_page, run_length)` pairs:

```rust
// Dirty pages: {2, 3, 5, 6, 7}  →  runs: [(2, 2), (5, 3)]
// Byte ranges: [(2*16384, 32768), (5*16384, 49152)]
```

The merge is O(n log n) on the number of dirty pages and produces the minimum
number of `msync` syscalls for any given dirty set.

### Freelist integration

When the freelist changes (`alloc_page`, `free_page`), `freelist_dirty` is set.
On `flush()`, the freelist bitmap is serialized into page 1 first, and page 1 is
added to the effective flush set even if it was not already in the dirty tracker.
Only after **all** targeted flushes succeed are `freelist_dirty` and the dirty
tracker cleared. A partial failure leaves both intact so the next `flush()` can
retry safely.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Sub-file msync</span>
SQLite and PostgreSQL issue `fsync` over the entire data file on every checkpoint or WAL sync. AxiomDB's targeted `flush_range` (backed by `msync(MS_SYNC)`) touches only the pages that actually changed. On workloads where a small fraction of pages are written per checkpoint, this reduces I/O proportionally to the dirty-page ratio.
</div>
</div>

### Disk-full error classification

Every durable I/O call in `flush()` (and in `create()`/`grow()`) passes its
`std::io::Error` through `classify_io()` before returning:

```rust
// axiomdb-core/src/error.rs
pub fn classify_io(err: std::io::Error, operation: &'static str) -> DbError {
    // ENOSPC (28) and EDQUOT (69/122) → DbError::DiskFull { operation }
    // All other errors → DbError::Io(err)
}
```

When a `DiskFull` error propagates out of `MmapStorage`, the server runtime
transitions to **read-only degraded mode** — all subsequent mutating statements
are rejected immediately without re-entering the storage layer.

### Invariants

- `flush()` returns `Ok(())` only after all dirty pages are durable.
- Dirty tracking is cleared only on success — never on failure.
- The freelist page (page 1) is always included when `freelist_dirty` is set,
  regardless of whether it appears in the tracker.
- `dirty_page_count()` always reflects the count since the last successful flush.
- `ENOSPC`/`EDQUOT` errors are always surfaced as `DbError::DiskFull`, never
  silently wrapped in `DbError::Io`.

---

## Verified Open — Corruption Detection at Startup

`MmapStorage::open()` validates every **allocated** page before making the
database available. The startup sequence is:

1. Map the file and verify page 0 (meta) — magic, version, page count.
2. Load the freelist from page 1 and verify its checksum.
3. Scan pages `2..page_count`, skipping any page the freelist marks as free.
   For each allocated page, call `read_page_from_mmap()` which re-computes
   the CRC32c of the body and compares it to the stored `header.checksum`.

```rust
for page_id in 2..page_count {
    if !freelist.is_free(page_id) {
        Self::read_page_from_mmap(&mmap, page_id)?;
    }
}
```

If any page fails, `open()` returns `DbError::ChecksumMismatch { page_id, expected, got }`
immediately. No connection is accepted and no `Db` handle is returned.

Free pages are skipped because they are never written by the storage engine
and therefore have no valid page header or checksum. Scanning them would
produce false positives on a freshly created or partially filled database.

### Recovery wiring

Both the network server (`Database::open`) and the embedded handle (`Db::open`)
route through `TxnManager::open_with_recovery()` on every reopen:

```rust
let (txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
```

This ensures WAL replay runs before the first query is executed, even if the
only change in this subphase is the corruption scan. Bypassing
`open_with_recovery()` with the older `TxnManager::open()` was an oversight
that this subphase closes.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Scan only allocated pages</span>
Free pages contain no valid page header — `file.set_len()` zero-initializes them, giving `checksum = 0`. The CRC32c of an all-zero body is non-zero, so scanning free pages would produce a spurious `ChecksumMismatch` on every fresh or sparsely-used database. The freelist (already in memory by step 2) provides the allocation bitmap at zero extra I/O cost.
</div>
</div>

---

## MemoryStorage — In-Memory for Tests

`MemoryStorage` stores pages in a `Vec<Box<Page>>`. It implements the same
`StorageEngine` trait as `MmapStorage`. All unit tests for the B+ Tree, WAL,
and catalog use `MemoryStorage`, so they run without touching the filesystem.

```rust
let mut storage = MemoryStorage::new();
let id = storage.alloc_page(PageType::Data)?;
let mut page = Page::new(PageType::Data, id);
page.body_mut()[0] = 0xAB;
page.update_checksum();
storage.write_page(id, &page)?;
let read_back = storage.read_page(id)?;
assert_eq!(read_back.body()[0], 0xAB);
```

---

## FreeList — Page Allocation

The FreeList tracks which pages are free using a bitmap. The bitmap is stored in a
dedicated page (or pages, for large databases). Each bit corresponds to one page:
`1 = free`, `0 = in use`.

### Allocation

Scans left-to-right for the first `1` bit, clears it, and returns the page ID.

```
Bitmap: 1110 1101 ...
         ↑
         First free: page 0 (bit 0 = 1)
```

After allocation: `0110 1101 ...`

### Deallocation

Sets the bit corresponding to `page_id` back to `1`. Returns
`DbError::DoubleFree` if the bit was already `1` (guard against bugs in the
caller).

### Invariants

- No page appears twice in the freelist.
- No page can be both allocated and in the freelist simultaneously.
- The freelist bitmap is itself stored in allocated pages (and tracked recursively
  during bootstrap).

---

## Heap Pages — Slotted Format

Table rows (heap tuples) are stored in `PageType::Data` pages using a slotted page
layout. The slot array grows from the start of the body; tuples grow from the end
toward the center.

```
Body (16,320 bytes):
┌─────────────────────────────────────────────────────────────┐
│ Slot[0] │ Slot[1] │ ... │ free space │ ... │ Tuple[1] │ Tuple[0] │
└──────────────────────────────────────────────────────────────┘
↑                           ↑           ↑
free_start              free area      free_end (decreases)
```

`free_start` points to the first unused byte after the last slot entry.
`free_end` points to the first byte of the last tuple written (counting from the
end of the body).

### SlotEntry — 4 bytes

```text
Offset  Size  Field
     0     2  offset   — byte offset of the tuple within the body (0 = empty slot)
     2     2  length   — total length of the tuple in bytes
```

A slot with `offset = 0` and `length = 0` is an empty (deleted) slot. Deleted slots
are reused when the page is compacted (VACUUM, planned Phase 9).

### RowHeader — 24 bytes

Every heap tuple begins with a RowHeader that stores MVCC visibility metadata:

```text
Offset  Size  Field
     0     8  xmin      — txn_id of the transaction that inserted this row
     8     8  xmax      — txn_id of the transaction that deleted/updated this row (0 = live)
    16     1  deleted   — 1 if this row has been logically deleted
    17     7  _pad      — alignment
Total: 24 bytes
```

After the RowHeader comes the null bitmap and the encoded column data (see [Row Codec](row-codec.md)).

### Null Bitmap in Heap Rows

The null bitmap is stored immediately after the `RowHeader`. It occupies
`ceil(n_cols / 8)` bytes. Bit `i` (zero-indexed) being `1` means column `i` is NULL.

```
5 columns → ceil(5/8) = 1 byte = 8 bits (bits 5-7 unused, always 0)
11 columns → ceil(11/8) = 2 bytes
```

---

## Page 0 — The Meta Page

Page 0 is the `PageType::Meta` page. It is written during database creation
(bootstrap) and read during `open()`. Its body contains:

```text
Offset  Size  Field
     0     8  format_version     — AxiomDB file format version
     8     8  catalog_root_page  — Page ID of the catalog root (axiom_tables B+ Tree root)
    16     8  freelist_root_page — Page ID of the freelist bitmap root
    24     8  next_txn_id        — Next transaction ID to assign
    32     8  checkpoint_lsn     — LSN of the last successful checkpoint
    40   rest _reserved          — Future extensions
```

On crash recovery, the `checkpoint_lsn` tells the WAL reader where to start replaying.
All WAL entries with LSN > `checkpoint_lsn` and belonging to committed transactions
are replayed.

---

## Batch Delete Operations

AxiomDB implements three optimizations for DELETE workloads that dramatically reduce
page I/O and CRC32c computation overhead.

### HeapChain::delete_batch()

`delete_batch()` accepts a slice of `(page_id, slot_id)` pairs and groups them by
`page_id` before touching any page. For each unique page it reads the page once,
marks all targeted slots dead in a single pass, then writes the page back once.

```
Naive per-row delete path (before delete_batch):
  for each of N rows:
    read_page(page_id)          ← 1 read
    mark slot dead              ← 1 mutation
    update_checksum(page)       ← 1 CRC32c over 16 KB
    write_page(page_id, page)   ← 1 write
  Total: 3N page operations

Batch path (delete_batch):
  group rows by page_id → P unique pages
  for each page:
    read_page(page_id)          ← 1 read
    mark all M slots dead       ← M mutations (M rows on this page)
    update_checksum(page)       ← 1 CRC32c (once per page, not per row)
    write_page(page_id, page)   ← 1 write
  Total: 2P page operations
```

At 200 rows/page, deleting 10,000 rows hits 50 pages. The naive path requires 30,000
page operations; `delete_batch()` requires 100.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">300× Fewer Page Operations Than InnoDB Per-Row Buffer Pool Hits</span>
MySQL InnoDB processes each DELETE row individually: it pins the page in the buffer pool, applies the undo log entry, updates the row's delete-mark, and releases the pin — once per row. For a 10K-row full-table DELETE, AxiomDB performs 100 page operations (read + write per page); InnoDB performs 10,000+ buffer pool pin/unpin cycles plus 10,000 undo log entries.
</div>
</div>

### mark_deleted() vs delete_tuple() — Splitting Checksum Work

`heap::mark_deleted()` is an internal function that stamps the slot as dead without
recomputing the page checksum. `delete_tuple()` (the single-row public API) calls
`mark_deleted()` followed immediately by `update_checksum()` — behavior is unchanged
for callers.

The batch path calls `mark_deleted()` N times (once per slot on a given page), then
calls `update_checksum()` exactly once when all slots on that page are done.

```rust
// Single-row path (public, unchanged):
pub fn delete_tuple(page: &mut Page, slot_id: u16) -> Result<(), DbError> {
    mark_deleted(page, slot_id)?;   // stamp dead
    page.update_checksum();          // 1 CRC32c
    Ok(())
}

// Batch path (called by delete_batch for each page):
for &slot_id in slots_on_this_page {
    mark_deleted(page, slot_id)?;   // stamp dead, no checksum
}
page.update_checksum();             // 1 CRC32c for all N slots on this page
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Deferred Checksum in Batch Paths</span>
CRC32c over a 16 KB page costs roughly 4–8 µs on modern hardware. Calling it once per deleted slot instead of once per page wastes N-1 full-page hashes per batch. Splitting <code>mark_deleted</code> from <code>update_checksum</code> makes the cost O(P) in the number of pages, not O(N) in the number of rows. The same split was applied to <code>insert_batch</code> in Phase 3.17.
</div>
</div>

### scan_rids_visible()

`HeapChain::scan_rids_visible()` is a variant of `scan_visible()` that returns only
`(page_id, slot_id)` pairs — no row data is decoded or copied.

```rust
pub fn scan_rids_visible(
    &self,
    storage: &dyn StorageEngine,
    snapshot: &TransactionSnapshot,
    self_txn_id: u64,
) -> Result<Vec<(u64, u16)>, DbError>
```

This is used by DELETE without a WHERE clause and TRUNCATE TABLE: both operations
need to locate every live slot but neither needs to decode the row's column values.
Avoiding `Vec<u8>` allocation for each row's payload cuts memory allocation to near
zero for full-table deletes.

### HeapChain::clear_deletions_by_txn()

`clear_deletions_by_txn(txn_id)` is the undo helper for `WalEntry::Truncate`. It
scans the entire heap chain and, for every slot where `txn_id_deleted == txn_id`,
clears the deletion stamp (sets `txn_id_deleted = 0`, `deleted = 0`).

This is used during ROLLBACK and crash recovery when a `WalEntry::Truncate` must be
undone. The cost is O(P) page reads and writes for P pages in the chain — identical
to a full-table scan. Because recovery and rollback are infrequent relative to inserts
and deletes, this trade-off is acceptable (see WAL internals for the corresponding
`WalEntry::Truncate` design decision).

---

## All-Visible Page Flag (Optimization A)

### What it is

Bit 0 of `PageHeader.flags` (`PAGE_FLAG_ALL_VISIBLE = 0x01`). When set, it
asserts that every alive slot on the page was inserted by a **committed** transaction
and none have been deleted. Sequential scans can skip per-slot MVCC
`txn_id_deleted` tracking for those pages entirely.

Inspired by PostgreSQL's all-visible map (`src/backend/storage/heap/heapam.c:668`),
but implemented as an in-page bit rather than a separate VM file — a single
cache-line read suffices.

### API

```rust
pub const PAGE_FLAG_ALL_VISIBLE: u8 = 0x01;

impl Page {
    pub fn is_all_visible(&self) -> bool { ... }   // reads bit 0 of flags
    pub fn set_all_visible(&mut self) { ... }       // sets bit 0; caller updates checksum
    pub fn clear_all_visible(&mut self) { ... }     // clears bit 0; caller updates checksum
}
```

### Lazy-set during scan

`HeapChain::scan_visible()` sets the flag after verifying that all alive slots
on a page satisfy:
- `txn_id_created <= max_committed` (committed transaction)
- `txn_id_deleted == 0` (not deleted)

This is a one-time write per page per table lifetime. After the first slow-path
scan, every subsequent scan takes the fast path and skips per-slot checks.

### Clearing on delete

`heap::mark_deleted()` clears the flag **unconditionally** as its very first
mutation — before stamping `txn_id_deleted`. Both changes land in the same
`update_checksum()` + `write_page()` call. There is no window where the flag is
set while a slot is deleted.

### Read-only variant for catalog scans

`HeapChain::scan_visible_ro()` takes `&dyn StorageEngine` (immutable) and never
sets the flag. Used by `CatalogReader` and other callers that hold only a shared
reference. Catalog tables are small (a few pages) and not hot enough to warrant
the lazy-set write.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
After the first scan on a stable table, SELECT skips N per-slot MVCC comparisons
(4 u64 comparisons each) and replaces them with 1 bit-check per page.
At 200 rows/page, a 10K-row scan goes from 10,000 visibility checks to 50 flag reads.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
In-page bit vs. separate visibility map file (PostgreSQL's approach): the in-page
bit requires no additional file I/O and is covered by the existing page checksum.
The trade-off is that clearing the flag on any delete requires a page write — the
same write already happening for the slot stamp, so no additional I/O is incurred.
</div>
</div>

---

## Sequential Scan Prefetch Hint (Optimization C)

### What it is

`StorageEngine::prefetch_hint(start_page_id, count)` — a hint method telling
the backend that pages starting at `start_page_id` will be read sequentially.
Implementations that do not support prefetch provide a default no-op.

Inspired by PostgreSQL's `read_stream.c` adaptive lookahead.

### API

```rust
// Default no-op in the trait — all existing backends compile unchanged
fn prefetch_hint(&self, start_page_id: u64, count: u64) {}
```

`MmapStorage` overrides this with `madvise(MADV_SEQUENTIAL)` on macOS and Linux:

```rust
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prefetch_hint(&self, start_page_id: u64, count: u64) {
    // SAFETY: ptr derived from live MmapMut, offset < mmap_len verified,
    // clamped_len <= mmap_len - offset. madvise is a pure hint.
    let _ = unsafe { libc::madvise(ptr, clamped_len, libc::MADV_SEQUENTIAL) };
}
```

`count = 0` uses the backend default (`PREFETCH_DEFAULT_PAGES = 64`, 1 MB).

### Call sites

`HeapChain::scan_visible()`, `scan_rids_visible()`, and `delete_batch()` each
call `storage.prefetch_hint(root_page_id, 0)` once before their scan loop. This
tells the OS kernel to begin async read-ahead for the pages that follow,
overlapping disk I/O with CPU processing of the current page.

### When it helps

The hint has measurable impact on cold-cache workloads (data not in OS page
cache). On warm cache (mmap pages already faulted in), `madvise` is accepted
but the kernel takes no additional action — no performance regression.

---

## Lazy Column Decode (Optimization B)

### What it is

`decode_row_masked(bytes, schema, mask)` — a variant of `decode_row` that accepts
a boolean mask. When `mask[i] == false`, the column's wire bytes are skipped
(cursor advanced, no allocation) and `Value::Null` is placed in the output slot.

Inspired by PostgreSQL's selective column access in the executor.

### API

```rust
pub fn decode_row_masked(
    bytes: &[u8],
    schema: &[DataType],
    mask: &[bool],      // mask.len() must equal schema.len()
) -> Result<Vec<Value>, DbError>
```

For skipped columns:
- Fixed-length types (Bool=1B, Int/Date=4B, BigInt/Real/Timestamp=8B, Decimal=17B, Uuid=16B):
  `ensure_bytes` is called then `pos` advances — no allocation.
- Variable-length types (Text, Bytes): the 3-byte length prefix is read to advance
  `pos` by `3 + len` — the payload is never copied or parsed.
- NULL columns (bitmap bit set): no wire bytes, cursor unchanged regardless of mask.

### Column mask computation

The executor computes the mask via `collect_column_refs(expr, mask)`, which walks
the AST and marks every `Expr::Column { col_idx }` reference. It does not recurse
into subquery bodies (different row scope).

`SELECT *` (Wildcard/QualifiedWildcard) always produces `None` — `decode_row()`
is used directly with no overhead.

When all mask bits are `true`, `scan_table` also uses `decode_row()` directly.

### Where it applies

- `execute_select_ctx` (single-table SELECT): mask covers SELECT list + WHERE + ORDER BY + GROUP BY + HAVING
- `execute_delete_ctx` (DELETE with WHERE): mask covers the WHERE clause only (no-WHERE path uses `scan_rids_visible` — no decode at all)
