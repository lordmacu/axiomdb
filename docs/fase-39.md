# Phase 39 — Clustered Index Storage Engine

## Subfases completed in this session: 39.2, 39.3, 39.4, 39.5, 39.6, 39.7, 39.8, 39.9, 39.10, 39.11, 39.12, 39.13, 39.14, 39.15, 39.16, 39.17, 39.18, 39.19, 39.21, 39.22

## What was built

### 39.2 — Clustered internal page format

`crates/axiomdb-storage/src/clustered_internal.rs` now implements the storage-layer
page primitive for clustered B-tree internal nodes.

The new page uses a slotted layout with variable-size separator keys:

- `PageType::ClusteredInternal`
- `num_cells`
- `cell_content_start`
- `freeblock_offset`
- `leftmost_child`
- cell pointer array sorted by key
- variable-size cells storing:
  - `right_child: u64`
  - `key_len: u16`
  - `key_data`

This keeps the traversal contract compatible with the current internal-node API:

- logical child `0` lives in `leftmost_child`
- logical child `i > 0` lives in the `right_child` of cell `i - 1`
- `find_child_idx(search_key)` still returns the first separator strictly greater
  than the search key

### Storage API added

The new module exposes page-local operations needed by the clustered storage rewrite:

- `init_clustered_internal`
- `num_cells`
- `leftmost_child` / `set_leftmost_child`
- `key_at`
- `child_at`
- `set_child_at`
- `read_cell`
- `find_child_idx`
- `insert_at`
- `remove_at`
- `free_space`
- `defragment`

### Supporting changes

- `crates/axiomdb-storage/src/page.rs`
  - adds `PageType::ClusteredInternal = 6`
  - extends `TryFrom<u8>` and page-type roundtrip tests
- `crates/axiomdb-storage/src/lib.rs`
  - exports `clustered_internal`

### 39.3 — Clustered B-tree insert

`crates/axiomdb-storage/src/clustered_tree.rs` now implements the first
tree-level write path for clustered storage.

The new controller is intentionally separate from the current
`axiomdb-index::BTree`:

- `insert(storage, root_opt, key, row_header, row_data) -> Result<u64, DbError>`
- empty-tree bootstrap into a `ClusteredLeaf` root
- recursive descent through `ClusteredInternal`
- sorted leaf insert for inline `(key, RowHeader, row_data)` cells
- one `defragment()` retry before splitting
- leaf split by cumulative cell byte volume
- internal split by cumulative separator byte volume
- root split into a new `ClusteredInternal` page when needed

The split policy is storage-first and in-place:

- the old page ID stays as the left half
- only the new right sibling is allocated
- the propagated separator is the first key on the new right leaf, or the
  promoted middle separator for internal pages

### Supporting changes for 39.3

- `crates/axiomdb-storage/src/clustered_leaf.rs`
  - adds helper APIs for inline-capacity checks and cell footprint sizing
- `crates/axiomdb-storage/src/clustered_internal.rs`
  - adds separator footprint helpers for byte-volume split planning
- `crates/axiomdb-storage/src/lib.rs`
  - exports `clustered_tree`
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds clustered insert integration coverage over 10K rows

### 39.4 — Clustered B-tree point lookup

`crates/axiomdb-storage/src/clustered_tree.rs` now also implements the first
tree-level read path for clustered storage.

The new API is:

- `lookup(storage, root_opt, key, snapshot) -> Result<Option<ClusteredRow>, DbError>`

Behavior:

- `None` when the clustered tree is empty
- `None` when the key is absent
- `Some(ClusteredRow)` when the key exists and the current inline version is
  visible to the supplied `TransactionSnapshot`
- `None` when the key exists but the current inline version is not visible

This lookup path:

- descends clustered internal pages with binary search
- performs exact-key search on the clustered leaf cell pointer array
- returns owned `key`, `RowHeader`, and `row_data` directly from the clustered
  leaf page
- uses `RowHeader::is_visible(...)` for MVCC filtering

Important boundary for this subphase:

- 39.4 does **not** reconstruct older visible versions yet
- if the current inline version is invisible, lookup returns `None`
- undo/version-chain behavior stays deferred to later clustered MVCC / WAL phases

### Supporting changes for 39.4

- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds `ClusteredRow`
  - adds root-to-leaf lookup descent helper
  - adds snapshot-filtered exact point lookup
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds clustered point lookup integration probes after many inserts and splits

### 39.5 — Clustered B-tree range scan

`crates/axiomdb-storage/src/clustered_tree.rs` now also implements the first
ordered multi-row scan path over clustered storage.

The new API is:

- `range(storage, root_opt, from, to, snapshot) -> Result<ClusteredRangeIter<'_>, DbError>`

Behavior:

- empty-tree scan yields no rows
- full scan yields all visible current inline rows in primary-key order
- bounded scan respects inclusive and exclusive lower / upper bounds
- bounded scan descends directly to the first relevant leaf instead of always
  starting from the leftmost leaf
- multi-leaf scan follows `next_leaf` in `O(1)` per leaf boundary
- invisible current inline versions are skipped

This range path is deliberately storage-first:

- it returns owned `ClusteredRow` values directly from clustered leaves
- it uses `RowHeader::is_visible(...)` for MVCC filtering
- it issues `StorageEngine::prefetch_hint(...)` when advancing to the next leaf
- it does **not** reconstruct older visible versions yet
- it is not wired into the SQL executor yet

### Supporting changes for 39.5

- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds `ClusteredRangeIter`
  - adds `range(...)`
  - adds bound-aware start-leaf descent helpers
  - adds `next_leaf` traversal with prefetch hints
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds 10K-row full-scan and bounded-scan clustered range integration coverage

### 39.6 — Clustered B-tree update in place

`crates/axiomdb-storage/src/clustered_tree.rs` now implements the first
clustered-row mutation path.

The new API is:

- `update_in_place(storage, root_opt, key, new_row_data, txn_id, snapshot) -> Result<bool, DbError>`

Behavior:

- `false` when the clustered tree is empty
- `false` when the key is absent
- `false` when the current inline version is not visible to the supplied snapshot
- `true` when the row is rewritten in the same clustered leaf page
- `HeapPageFull` when the replacement row would require leaving the current leaf

This update path is still deliberately storage-first:

- it keeps the primary key unchanged
- it rewrites only the owning clustered leaf page
- it preserves parent separators and `next_leaf`
- it bumps `row_version` and rewrites `txn_id_created`
- it does **not** do delete+insert tree surgery yet
- it does **not** reconstruct older visible versions after update yet
- it is not wired into the SQL executor yet

### Supporting changes for 39.6

- `crates/axiomdb-storage/src/clustered_leaf.rs`
  - adds `rewrite_cell_same_key(...)`
  - adds same-size overwrite fast path
  - adds same-leaf rebuild fallback when growth still fits after compaction
- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds `update_in_place(...)`
  - adds same-leaf `HeapPageFull` failure mapping
  - adds unit coverage for empty-tree, missing-key, invisible-row, growth-success, and no-fit cases
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds integration coverage for split-tree clustered updates and explicit same-leaf growth failure

### 39.7 — Clustered B-tree delete

`crates/axiomdb-storage/src/clustered_tree.rs` now also implements the first
clustered-row delete path.

The new API is:

- `delete_mark(storage, root_opt, key, txn_id, snapshot) -> Result<bool, DbError>`

Behavior:

- `false` when the clustered tree is empty
- `false` when the key is absent
- `false` when the current inline version is not visible to the supplied snapshot
- `true` when the row is delete-marked in the owning clustered leaf page

This delete path is still deliberately storage-first:

- it stamps only `txn_id_deleted`
- it preserves `txn_id_created`, `row_version`, key bytes, and row payload bytes
- it preserves parent separators and `next_leaf`
- it keeps the physical cell inline so older snapshots can still observe it
- it does **not** purge dead clustered cells yet
- it does **not** merge or rebalance the tree after delete yet
- it does **not** add WAL / undo / recovery semantics yet
- it is not wired into the SQL executor yet

### Supporting changes for 39.7

- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds `delete_mark(...)`
  - reuses exact-leaf descent and same-key rewrite for header-only delete-mark
  - adds unit coverage for empty-tree, missing-key, invisible-row, old-snapshot visibility, and split-tree invariants
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds integration coverage for split-tree delete-mark visibility under old and new snapshots

### 39.8 — Clustered B-tree structural rebalance

`crates/axiomdb-storage/src/clustered_tree.rs` now also implements the first
clustered structural-maintenance layer for variable-size pages.

The new public API is:

- `update_with_relocation(storage, root_opt, key, new_row_data, txn_id, snapshot) -> Result<Option<u64>, DbError>`

Behavior:

- `None` when the clustered tree is empty
- `None` when the key is absent
- `None` when the current inline version is not visible to the supplied snapshot
- `Some(root_pid)` when the row is updated in place
- `Some(root_pid)` when same-leaf rewrite fails and the controller falls back to physical delete + reinsert with structural rebalance

This subphase adds the first private structural delete/rebalance path:

- exact physical cell removal from clustered leaves
- parent separator repair when the minimum key of a non-leftmost child changes
- leaf redistribute / merge by encoded byte volume
- internal redistribute / merge while preserving `n keys -> n + 1 children`
- `next_leaf` preservation across leaf merge
- empty internal root collapse to the only remaining child

`update_with_relocation(...)` is intentionally layered on top of `39.6`:

- fast path = `update_in_place(...)`
- fallback path = exact physical delete + clustered insert
- replacement row bumps `row_version`
- replacement row rewrites `txn_id_created = txn_id`
- old-version reconstruction still does **not** exist

Important boundary for this subphase:

- public clustered delete is still `delete_mark(...)`, not purge
- structural physical delete is private helper logic used by relocate-update
- delete-mark cleanup remains deferred to clustered purge
- relocate-update still rewrites only the current inline version
- secondary-index bookmark maintenance is still deferred
- parent separator repair still assumes the repaired separator fits on the current internal page

### Supporting changes for 39.8

- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds private physical delete result propagation
  - adds byte-volume leaf/internal rebalance helpers
  - adds parent-separator repair and root-collapse helpers
  - adds `update_with_relocation(...)`
  - adds unit coverage for separator repair, leaf merge, internal redistribution, root collapse, and relocate-update
- `crates/axiomdb-storage/src/clustered_leaf.rs`
  - adds `page_capacity_bytes()` for byte-volume rebalance planning
- `crates/axiomdb-storage/src/clustered_internal.rs`
  - adds `page_capacity_bytes()` for byte-volume rebalance planning
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds split-tree integration coverage for relocate-update through lookup and range scan

### 39.9 — Secondary indexes with PK bookmarks

`crates/axiomdb-sql/src/clustered_secondary.rs` now implements the first
clustered-first secondary-index path.

The new API is intentionally separate from the current heap-backed
`index_maintenance` path:

- `ClusteredSecondaryLayout::derive(secondary_idx, primary_idx) -> Result<..., DbError>`
- `entry_from_row(row) -> Result<Option<ClusteredSecondaryEntry>, DbError>`
- `decode_entry_key(physical_key) -> Result<ClusteredSecondaryEntry, DbError>`
- `logical_prefix_bounds(logical_prefix) -> Result<(Vec<u8>, Vec<u8>), DbError>`
- `scan_prefix(storage, root_page_id, logical_prefix) -> Result<Vec<...>, DbError>`
- `insert_row(...)`, `delete_row(...)`, `update_row(...)`

Behavior:

- the physical secondary key is `secondary_logical_key ++ missing_primary_key_columns`
- scanned secondary entries decode back into both the logical secondary key and
  the full primary-key bookmark
- duplicate logical secondary keys remain ordered by appended PK values
- relocate-only updates become secondary no-ops when the logical secondary key
  and primary key stay stable
- the legacy fixed-size `RecordId` payload in `axiomdb-index::BTree` is treated
  only as a compatibility artifact for this path, not as row identity

Important boundary for this subphase:

- the current SQL-visible heap executor still uses `RecordId`-based secondary indexes
- FK enforcement, planner/index scans, and index-integrity rebuilds are not yet
  switched to the clustered bookmark path
- the clustered bookmark path exists so future clustered executor work can probe
  `secondary -> primary key -> clustered row` without depending on physical row location

### Supporting changes for 39.9

- `crates/axiomdb-sql/src/clustered_secondary.rs`
  - adds layout derivation from secondary + primary `IndexDef`
  - adds physical-key encode/decode helpers for `secondary ++ missing_pk_suffix`
  - adds logical-prefix scan bounds without fixed 10-byte RID assumptions
  - adds insert/delete/update maintenance helpers over the existing `BTree`
  - adds unique-logical-key checks for bookmark-bearing secondaries
- `crates/axiomdb-sql/src/lib.rs`
  - exports `clustered_secondary`
- `crates/axiomdb-sql/tests/integration_clustered_secondary.rs`
  - adds integration coverage for duplicate logical keys, bookmark decode, delete, and relocate-stable maintenance

### 39.10 — Overflow pages for large rows

`crates/axiomdb-storage/src/clustered_overflow.rs` now implements clustered-row
overflow-page chains, and `clustered_leaf.rs` / `clustered_tree.rs` now use
that format for large logical rows.

The clustered leaf cell format is no longer inline-only:

- `key_len: u16`
- `total_row_len: u32`
- inline `RowHeader`
- inline primary-key bytes
- inline local row prefix
- optional `overflow_first_page: u64`

Behavior:

- rows below the local inline budget stay fully inline
- rows above that budget spill only their tail bytes to `PageType::Overflow`
  pages
- clustered lookup and range scan reconstruct full logical row bytes
  transparently
- `update_in_place(...)` now supports inline → overflow, overflow → overflow,
  and overflow → inline transitions
- `delete_mark(...)` keeps the overflow chain reachable because clustered purge
  still belongs to later phases
- physical clustered delete frees the removed row's obsolete overflow chain
- leaf split / rebalance / merge now move physical leaf descriptors without
  reallocating overflow payload

This subphase also closes the original `39.1` acceptance gap: clustered leaf
pages now support large-row overflow instead of rejecting those rows outright.

### Supporting changes for 39.10

- `crates/axiomdb-storage/src/clustered_overflow.rs`
  - adds chained overflow-page write/read/free helpers over `PageType::Overflow`
  - validates page type, early chain termination, trailing pages, and cycle-like
    corruption
- `crates/axiomdb-storage/src/clustered_leaf.rs`
  - changes clustered leaf cells from inline-only `row_len: u16` to
    `total_row_len: u32` + local prefix + optional overflow pointer
  - adds local-row budgeting and physical-footprint helpers for overflow-backed rows
  - adds direct encode/decode support for overflow-backed cell descriptors
- `crates/axiomdb-storage/src/clustered_tree.rs`
  - materializes overflow chains on clustered insert and update
  - reconstructs logical row bytes on clustered lookup and range scan
  - keeps split / rebalance / merge descriptor-oriented instead of logical-row-oriented
  - frees obsolete overflow chains on physical remove and shrink-to-inline update
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
  - adds mixed inline/overflow lookup + range coverage
  - adds update transition coverage across overflow boundaries
  - adds delete-mark coverage proving the overflow chain survives until later purge

### 39.11 — WAL support for clustered operations

`crates/axiomdb-wal` now has the first clustered-row WAL contract, and
`TxnManager` can now undo clustered writes inside a running transaction or
savepoint.

The clustered WAL path is intentionally logical rather than slot-addressed:

- `EntryType::ClusteredInsert = 12`
- `EntryType::ClusteredDeleteMark = 13`
- `EntryType::ClusteredUpdate = 14`
- the WAL `key` stores primary-key bytes, not `(page_id, slot_id)`
- `old_value` / `new_value` store a `ClusteredRowImage`:
  - `root_pid`
  - exact `RowHeader`
  - exact logical row bytes

Behavior:

- clustered insert appends a `ClusteredInsert` record and pushes clustered undo
  as `delete_physical_by_key(...)`
- clustered delete-mark and clustered update append row-image WAL and push
  clustered undo as `restore_exact_row_image(...)`
- `TxnManager` now tracks the latest clustered `root_pid` per `table_id`
  inside the active transaction, so rollback follows the current tree shape even
  after split / merge / relocate-update
- rollback and `ROLLBACK TO SAVEPOINT` now restore logical clustered row state
  by primary key and exact row image, including overflow-backed rows
- clustered recovery now has the row-image contract it needs to undo in-progress
  clustered transactions logically in `39.12`

The rollback invariant is logical row restoration, not exact page-topology
reversal. A relocate-update may still leave the tree physically shaped
differently after rollback as long as the owning primary key, `RowHeader`, and
row bytes match the pre-change state.

### Supporting changes for 39.11

- `crates/axiomdb-wal/src/clustered.rs`
  - adds `ClusteredRowImage` with exact row-image codec for clustered WAL
- `crates/axiomdb-wal/src/entry.rs`
  - adds `EntryType::{ClusteredInsert, ClusteredDeleteMark, ClusteredUpdate}`
  - extends entry-type roundtrip coverage for the clustered payloads
- `crates/axiomdb-wal/src/txn.rs`
  - adds clustered undo variants and clustered `root_pid` tracking per `table_id`
  - adds `record_clustered_insert(...)`, `record_clustered_delete_mark(...)`,
    and `record_clustered_update(...)`
  - wires clustered rollback and savepoint undo through the clustered tree helpers
- `crates/axiomdb-wal/src/recovery.rs`
  - recognizes clustered entry types during WAL scans
  - provides the scan state that `39.12` extends into real clustered crash recovery
- `crates/axiomdb-storage/src/clustered_tree.rs`
  - adds `delete_physical_by_key(...)` for rollback of clustered inserts
  - adds `restore_exact_row_image(...)` for rollback of clustered delete/update
- `crates/axiomdb-wal/tests/integration_clustered_wal.rs`
  - adds rollback/savepoint coverage for insert, delete-mark, update, relocate-update,
    and overflow-backed rows

### 39.12 — Crash recovery for clustered index

`crates/axiomdb-wal/src/recovery.rs` now performs real clustered crash recovery
instead of failing with `NotImplemented`.

The recovery contract keeps the same clustered identity chosen in `39.11`:

- clustered recovery scans `ClusteredInsert`, `ClusteredDeleteMark`, and
  `ClusteredUpdate`
- clustered recovery undoes in-progress clustered writes by primary key plus
  exact `ClusteredRowImage`
- recovery tracks the current clustered `root_pid` per `table_id` while
  applying reverse undo
- recovery restores logical row state, not exact pre-crash page topology
- overflow-backed rows and relocate-update cases recover from WAL row images,
  not from old overflow chains or stale leaf slots

This subphase also closes another storage-first gap: `TxnManager::open(...)`
now reconstructs the latest committed clustered root per table from surviving
WAL history, and `TxnManager::open_with_recovery(...)` seeds those roots from
the post-recovery result.

The remaining boundary is explicit: clustered root persistence still depends on
surviving WAL history. If checkpoint/rotation truncates the clustered WAL
history, later work must persist clustered roots elsewhere instead of pretending
this is already solved.

### Supporting changes for 39.12

- `crates/axiomdb-wal/src/recovery.rs`
  - adds `RecoveryOp::{ClusteredInsert, ClusteredRestore}`
  - replaces the old clustered `NotImplemented` branch with real reverse undo
  - tracks final clustered roots per table in `RecoveryResult`
- `crates/axiomdb-wal/src/txn.rs`
  - changes `open(...)` to rebuild committed clustered roots from WAL
  - changes `open_with_recovery(...)` to seed `last_clustered_roots` from
    `RecoveryResult`
- `crates/axiomdb-wal/tests/integration_clustered_recovery.rs`
  - adds crash recovery coverage for clustered insert, delete-mark, overflow
    update, relocate-update, and clean reopen root reconstruction

## Validation

Targeted validation passed:

- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`

New `clustered_internal` tests cover:

- empty-page initialization
- mixed-size separator inserts
- logical `n keys -> n+1 children` mapping
- binary-search child selection semantics
- left-child vs right-child removal cases
- defragmentation preserving keys and child pointers
- page-full and bounds errors

Targeted validation for `39.3` also passed:

- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`

New clustered insert coverage now includes:

- empty-tree bootstrap
- duplicate-key rejection
- non-split sorted insert
- defrag-before-split
- leaf split and `next_leaf` maintenance
- internal split and root split
- 10K-row sorted verification in both unit and integration tests

Targeted validation for `39.4` also passed:

- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `mdbook build docs-site`

New clustered lookup coverage now includes:

- empty-tree lookup
- root-as-leaf hit
- missing-key lookup
- invisible current-version behavior
- lookup after internal/root splits
- 10K-row integration probes over a split tree

Targeted validation for `39.5` also passed:

- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `mdbook build docs-site`

New clustered range coverage now includes:

- empty-tree range
- full scan in primary-key order
- inclusive / exclusive bound behavior
- bounded start from a non-leftmost leaf
- invisible current-version skip behavior
- prefetch hints on leaf-chain advance
- 10K-row full and bounded range integration probes across many leaves

Targeted validation for `39.6` also passed:

- `cargo test -p axiomdb-storage clustered_leaf --lib -j1`
- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `mdbook build docs-site`

New clustered update coverage now includes:

- empty-tree update
- missing-key update
- invisible current-version update rejection
- same-leaf growth rewrite with `row_version` bump
- split-tree update preserving leaf identity and `next_leaf`
- explicit `HeapPageFull` on no-fit same-leaf growth
- integration validation through both clustered lookup and clustered range scan after update

Targeted validation for `39.7` also passed:

- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `mdbook build docs-site`

New clustered delete coverage now includes:

- empty-tree delete
- missing-key delete
- invisible current-version delete rejection
- delete-mark hiding the row from the deleting transaction and newer snapshots
- old-snapshot visibility over a delete-marked inline row
- split-tree delete preserving leaf identity and `next_leaf`
- integration validation through both clustered lookup and clustered range scan after delete-mark

Targeted validation for `39.8` also passed:

- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo clippy -p axiomdb-storage -- -D warnings`
- `cargo fmt --check`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `mdbook build docs-site`

New clustered structural coverage now includes:

- parent separator repair after deleting the first key of a non-leftmost leaf
- leaf merge preserving `next_leaf`
- internal redistribution preserving `n keys -> n + 1 children`
- root collapse after structural shrink
- relocate-update fallback after same-leaf `HeapPageFull`
- integration validation through lookup and range scan after relocate-update

Targeted validation for `39.9` also passed:

- `cargo test -p axiomdb-sql clustered_secondary --lib -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_secondary -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `cargo fmt --check`
- `cargo clippy -p axiomdb-sql --lib -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::needless_borrow`
- `mdbook build docs-site`

New clustered secondary bookmark coverage now includes:

- layout derivation when the secondary key already contains part of the primary key
- physical-key encode/decode roundtrip into logical-key + primary-key bookmark values
- duplicate logical keys ordered by appended PK bookmark values
- delete of one bookmark-bearing secondary entry without touching sibling bookmarks
- relocate-only update becoming a no-op when logical secondary key and PK remain stable
- unique-logical-key rejection for bookmark-bearing secondaries

Targeted validation for `39.10` also passed:

- `cargo test -p axiomdb-storage clustered_leaf --lib -j1`
- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo test -p axiomdb-storage --test integration_clustered_tree -j1`
- `cargo test -p axiomdb-storage -j1`
- `cargo check -p axiomdb-index -j1`
- `cargo check -p axiomdb-sql --lib -j1`

New clustered overflow coverage now includes:

- overflow-page write/read roundtrip across one page and multiple pages
- freeing the full overflow chain after physical removal
- leaf encode/decode roundtrip for overflow-backed descriptors
- clustered insert of rows above the old inline-only limit
- mixed inline/overflow lookup and full range scan reconstruction
- update transitions inline → overflow and overflow → inline
- delete-mark preserving the overflow chain until later purge

Targeted validation for `39.11` also passed:

- `cargo test -p axiomdb-wal -j1`
- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo clippy -p axiomdb-wal --tests -- -D warnings`
- `cargo clippy -p axiomdb-storage --lib -- -D warnings`
- `cargo fmt --check`
- `mdbook build docs-site`

New clustered WAL coverage now includes:

- WAL-entry roundtrip for `ClusteredInsert`, `ClusteredDeleteMark`, and `ClusteredUpdate`
- rollback of clustered insert after root changes across many inserts
- rollback of clustered delete-mark restoring the old `RowHeader` and row bytes
- rollback of overflow-backed clustered update restoring the old large row image
- rollback of relocate-update restoring the old logical row even after structural rewrite
- `ROLLBACK TO SAVEPOINT` undoing only the later clustered writes

Targeted validation for `39.12` also passed:

- `cargo test -p axiomdb-wal --test integration_clustered_recovery -j1`
- `cargo test -p axiomdb-wal -j1`
- `cargo test -p axiomdb-storage clustered_tree --lib -j1`
- `cargo clippy -p axiomdb-wal --tests -- -D warnings`
- `cargo clippy -p axiomdb-storage --lib -- -D warnings`
- `cargo fmt --check`
- `mdbook build docs-site`

New clustered crash-recovery coverage now includes:

- undo of uncommitted clustered insert after crash
- restore of clustered delete-mark after crash
- restore of overflow-backed clustered update after crash
- logical recovery of relocate-update after crash
- clean reopen reconstruction of the last committed clustered root from WAL history

### 39.14 — Executor integration: INSERT into clustered table

`crates/axiomdb-sql/src/executor/insert.rs` now has the first SQL-visible
clustered DML branch.

Behavior now exposed through SQL:

- `INSERT` on explicit-`PRIMARY KEY` clustered tables no longer fails with the
  Phase `39.14` guard rail
- both ctx and non-ctx executor paths route clustered tables directly through
  `axiomdb-storage::clustered_tree`
- clustered PK bytes are derived from primary-index metadata order, not raw
  table-column order
- clustered `AUTO_INCREMENT` bootstraps from the current clustered rows instead
  of scanning the heap path
- non-primary indexes on clustered tables are maintained through clustered PK
  bookmarks from `clustered_secondary`, not heap `RecordId` payloads
- explicit-transaction clustered inserts bypass `SessionContext::pending_inserts`
  and remain rollback/savepoint-safe
- pending heap batches are flushed before the clustered statement boundary so a
  later clustered statement failure does not accidentally undo prior staged heap writes

Important executor/WAL detail:

- a fresh clustered key records `ClusteredInsert`
- reusing a snapshot-invisible delete-marked physical clustered key records
  `ClusteredUpdate` instead, so rollback can restore the old tombstone image
  exactly

Current boundary after `39.14`:

- `INSERT` is now SQL-visible on clustered tables
- `SELECT`, `UPDATE`, and `DELETE` on clustered tables are still deferred to
  `39.15`, `39.16`, and `39.17`
- reusing a delete-marked clustered PK still rewrites only the current physical
  version; older-snapshot reconstruction of the superseded tombstone remains
  future clustered MVCC work

Supporting changes for `39.14`:

- `crates/axiomdb-sql/src/clustered_table.rs`
  - clustered row coercion/encoding helpers
  - clustered PK extraction and null validation
  - clustered `AUTO_INCREMENT` bootstrap scan
- `crates/axiomdb-sql/src/executor/insert.rs`
  - clustered ctx/non-ctx insert branches
  - clustered root tracking during statement execution
  - clustered secondary maintenance through bookmark-aware layouts
- `crates/axiomdb-catalog/src/reader.rs`
  - `get_index_by_id(...)` for rollback-time root reloading
- `crates/axiomdb-sql/src/executor/mod.rs`
  - rollback/savepoint index undo now reloads current roots from live catalog metadata
- `crates/axiomdb-sql/tests/integration_clustered_insert.rs`
  - adds SQL-visible clustered insert coverage

Targeted validation for `39.14` passed on the clustered executor surface:

- `cargo test -p axiomdb-sql --test integration_clustered_create_table -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_insert -j1`
- `cargo test -p axiomdb-sql --test integration_autocommit -j1`
- `cargo test -p axiomdb-sql --test integration_mvcc_indexes test_insert_rollback_removes_index_entry -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `cargo clippy -p axiomdb-sql --lib --tests -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::needless_borrow`
- `cargo fmt --check`
- `mdbook build docs-site`

### 39.15 — Executor integration: SELECT from clustered table

`crates/axiomdb-sql/src/executor/select.rs` now exposes the first SQL-visible
clustered read path.

Behavior now exposed through SQL:

- `SELECT` on explicit-`PRIMARY KEY` clustered tables no longer fails with the
  old `39.15` guard rail
- clustered full scans now route through `clustered_tree::range(...)`
- clustered PK equality lookups now route through `clustered_tree::lookup(...)`
- clustered PK range scans now route through `clustered_tree::range(...)`
- clustered secondary lookups now use `secondary key -> PK bookmark -> clustered row`
  instead of `secondary key -> heap RecordId`
- ctx-path covering plans that the planner labels as `IndexOnlyScan` are now
  normalized back to clustered-aware lookup/range plans instead of trying to
  read clustered tables through heap-era index-only semantics
- JOIN pre-scan paths now accept clustered tables through the same
  `scan_table_any_layout(...)` boundary used by heap tables

Current boundary after `39.15`:

- clustered `SELECT` is now SQL-visible on clustered tables
- clustered covering reads still fetch the clustered row body; a true clustered
  index-only optimization remains future work
- clustered `UPDATE` and `DELETE` remain deferred to `39.16` and `39.17`
- standalone clustered maintenance such as `CREATE INDEX`, `ANALYZE`, and
  `VACUUM` still remains deferred

Supporting changes for `39.15`:

- `crates/axiomdb-sql/src/executor/select.rs`
  - normalizes clustered `IndexOnlyScan` plans into clustered-aware lookup/range
  - routes clustered secondary access methods through PK-bookmark lookups
- `crates/axiomdb-sql/src/table.rs`
  - adds clustered secondary lookup/range helpers that decode PK bookmarks and
    fetch clustered rows by primary key
- `crates/axiomdb-sql/tests/integration_clustered_select.rs`
  - adds executor coverage for clustered secondary lookup, clustered secondary
    range, ctx-path covering scans, and MVCC visibility against an older snapshot

Targeted validation for `39.15` passed on the clustered read surface:

- `cargo test -p axiomdb-sql --test integration_clustered_select -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_create_table -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_insert -j1`
- `cargo check -p axiomdb-sql --lib -j1`

### 39.16 — Executor integration: UPDATE on clustered table

`crates/axiomdb-sql/src/executor/update.rs` now exposes the first SQL-visible
clustered rewrite path.

Behavior now exposed through SQL:

- `UPDATE` on explicit-`PRIMARY KEY` clustered tables no longer fails with the
  old `39.16` guard rail
- clustered candidate discovery now uses the clustered planner boundary:
  PK lookup, PK range, secondary bookmark probe, or full clustered scan
- non-key updates now stay on `clustered_tree::update_in_place(...)` when the
  row still fits in its owning leaf and fall back to
  `clustered_tree::update_with_relocation(...)` when it must be rewritten
- PK-changing updates now delete-mark the old clustered row, insert the new PK
  row, and rewrite bookmark-bearing secondary indexes
- rollback and savepoint undo now restore both the clustered base row and any
  rewritten secondary bookmark entries
- the non-ctx executor path now supports clustered `UPDATE` too; it no longer
  returns `NotImplemented`

Supporting changes for `39.16`:

- `crates/axiomdb-sql/src/executor/update.rs`
  - collects clustered candidates through clustered-aware access methods instead
    of forcing a whole-tree scan
  - records exact clustered row images in WAL for in-place, relocate, and
    PK-change updates
  - rewrites clustered secondary bookmark entries with undo-safe index
    insert/delete tracking
- `crates/axiomdb-wal/src/txn.rs`
  - adds index-delete undo records so rollback can reinsert old secondary
    bookmark entries after clustered UPDATE rewrites them
- `crates/axiomdb-sql/src/executor/mod.rs`
  - extends rollback/savepoint index undo handling to both delete newly inserted
    keys and restore deleted clustered-secondary bookmark keys
- `crates/axiomdb-sql/tests/integration_clustered_update.rs`
  - adds coverage for rollback, PK change, secondary-key rewrite, relocation,
    and non-ctx clustered UPDATE

Targeted validation for `39.16` passed on the clustered update surface:

- `cargo test -p axiomdb-sql --test integration_clustered_update -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_insert -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_select -j1`
- `cargo test -p axiomdb-wal -j1`
- `cargo clippy -p axiomdb-wal --tests -- -D warnings`
- `cargo clippy -p axiomdb-sql --lib --test integration_clustered_update --test integration_clustered_insert --test integration_clustered_select -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::needless_borrow`

### 39.17 — Executor integration: DELETE from clustered table

`crates/axiomdb-sql/src/executor/delete.rs` now exposes the first SQL-visible
clustered delete path.

Behavior now exposed through SQL:

- `DELETE` on explicit-`PRIMARY KEY` clustered tables no longer fails with the
  old `39.17` guard rail
- clustered candidate discovery now uses the clustered planner boundary:
  PK lookup, PK range, secondary bookmark probe, or full clustered scan
- parent-side FK enforcement now runs before the first clustered delete-mark so
  `RESTRICT` can abort cleanly without partially mutating the clustered tree
- clustered delete now records exact old/new clustered row images in WAL via
  `EntryType::ClusteredDeleteMark`, so `ROLLBACK` and savepoints restore the
  prior row header and payload bytes exactly
- the non-ctx executor path now supports clustered `DELETE` too; it no longer
  returns `NotImplemented`
- clustered secondary bookmark entries remain physically present after delete;
  reads hide them through clustered-row visibility and `39.18` will handle
  physical cleanup

Current boundary after `39.17`:

- clustered `DELETE` is now SQL-visible on clustered tables
- clustered delete is still logical delete-mark, not purge
- FK enforcement on clustered **child** tables remains deferred
- standalone clustered maintenance such as `CREATE INDEX`, `ANALYZE`, and
  `VACUUM` still remains deferred

Supporting changes for `39.17`:

- `crates/axiomdb-sql/src/executor/delete.rs`
  - collects clustered delete candidates through clustered-aware access methods
    instead of forcing a whole-tree scan
  - records exact clustered row images in WAL for delete-mark undo/recovery
  - reuses parent-side FK enforcement before applying the first clustered
    delete-mark
- `crates/axiomdb-sql/tests/integration_clustered_delete.rs`
  - adds executor coverage for secondary-predicate delete, rollback restore,
    parent-side FK `RESTRICT`, deferred secondary bookmark retention, and the
    non-ctx delete path
- `tools/wire-test.py`
  - adds MySQL wire smoke coverage for clustered delete, clustered secondary
    delete, and clustered delete rollback

Targeted validation for `39.17` passed on the clustered delete surface:

- `cargo test -p axiomdb-sql --test integration_clustered_delete -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_select -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_update -j1`
- `cargo check -p axiomdb-sql --lib -j1`

### 39.18 — VACUUM for clustered index

`crates/axiomdb-sql/src/vacuum.rs` now exposes the first clustered maintenance
path at SQL level.

Behavior now exposed through SQL:

- `VACUUM table_name` on an explicit-`PRIMARY KEY` clustered table no longer
  falls back to the heap-only contract
- clustered vacuum descends once to the leftmost clustered leaf and walks the
  `next_leaf` chain
- safe delete-marked clustered cells are physically removed when
  `txn_id_deleted < oldest_safe_txn`
- overflow-backed clustered rows free their overflow-page chains during purge
- clustered leaves are defragmented when freeblock waste exceeds the local threshold
- clustered secondary bookmark cleanup now decodes each PK bookmark and keeps
  only entries whose clustered row still exists physically after leaf purge
- secondary `BTree::delete_many_in(...)` root rotations are now persisted back
  into the catalog instead of leaving stale roots behind
- uncommitted clustered deletes remain untouched by `VACUUM`; the bookmark
  stays alive until the row is physically purgeable

Current boundary after `39.18`:

- clustered `VACUUM` is now SQL-visible on clustered tables
- clustered delete still begins as delete-mark; purge happens later through `VACUUM`
- clustered child-table FK enforcement remains deferred
- standalone clustered `CREATE INDEX` and `ANALYZE` remain deferred
- clustered table rewrite / migration still belongs to `39.19`

Supporting changes for `39.18`:

- `crates/axiomdb-sql/src/vacuum.rs`
  - routes clustered tables into a clustered leaf-purge path
  - frees clustered overflow chains during purge
  - cleans clustered secondary bookmarks by clustered physical existence after purge
  - persists any bulk-delete root rotation back into the catalog for both shared
    and clustered vacuum index cleanup
- `crates/axiomdb-storage/src/clustered_tree.rs`
  - exports `descend_to_leaf_pub(...)` for executor-side clustered batching
- `crates/axiomdb-sql/tests/integration_clustered_vacuum.rs`
  - adds coverage for secondary cleanup, uncommitted-delete safety, overflow-page reuse,
    and post-cleanup clustered secondary queries
- `tools/wire-test.py`
  - adds MySQL wire smoke coverage for clustered `VACUUM` committed purge and
    uncommitted-delete safety

Targeted validation for `39.18` passed on the clustered vacuum surface:

- `cargo test -p axiomdb-sql --test integration_clustered_vacuum -j1`
- `cargo test -p axiomdb-sql test_persist_index_root_if_changed_updates_catalog --lib -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `cargo clippy -p axiomdb-storage --lib -- -D warnings`
- `cargo clippy -p axiomdb-sql --lib --test integration_clustered_vacuum -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::needless_borrow`
- `cargo fmt --check`
- `cargo build --bin axiomdb-server -j1`
- `python3 tools/wire-test.py`

### 39.19 — Table rebuild: heap to clustered migration

`crates/axiomdb-sql/src/executor/ddl.rs` now closes the first clustered
migration bridge for legacy heap tables that already have PRIMARY KEY metadata.

Behavior now exposed through SQL:

- `ALTER TABLE t REBUILD` now accepts legacy heap tables that already own a
  PRIMARY KEY index
- the rebuild walks the old PRIMARY KEY B-Tree in logical key order, batch-reads
  the heap rows behind those `RecordId`s, and feeds the rows into a fresh
  clustered tree
- the table root then flips from heap to clustered in the catalog, and the
  PRIMARY KEY metadata is updated to point at that clustered root
- every non-primary index is rebuilt as a clustered secondary bookmark index,
  so post-rebuild secondary probes resolve as `secondary key -> PK bookmark ->
  clustered row`
- old heap and old index pages are not freed inline during the metadata swap;
  they are queued through `txn.defer_free_pages(...)` and reclaimed only after
  the DDL commit path completes

Current boundary after `39.19`:

- the SQL-visible rebuild path is for legacy heap+PK tables only
- new tables with explicit `PRIMARY KEY` still start clustered directly at
  `CREATE TABLE` time
- the rebuild path flushes newly built roots before the metadata swap and cleans
  pending new roots best-effort on statement failure
- fully generic rollback accounting for freshly allocated pages still remains a
  later cross-cutting transactional concern
- phase closeout, broader clustered integration coverage, and benchmarks still
  belong to `39.20`

Supporting changes for `39.19`:

- `crates/axiomdb-sql/src/executor/ddl.rs`
  - replaces the placeholder rebuild path with a real legacy heap→clustered migration
  - treats dead slots referenced by the old PK index as index-integrity failure
    instead of silently dropping rows
  - flushes the rebuilt clustered / secondary roots before the catalog swap
  - uses `txn.defer_free_pages(...)` for old heap/index pages instead of
    immediate `free_page()` during the swap
  - removes `expect`-based empty-root bootstrap and adds best-effort cleanup for
    newly built clustered artifacts on pre-commit errors
- `crates/axiomdb-sql/tests/integration_clustered_rebuild.rs`
  - seeds real legacy heap+PK fixtures
  - verifies post-rebuild clustered metadata, reclaimed old roots, secondary
    bookmark decoding, and post-rebuild `UPDATE` / `DELETE` / `VACUUM`
- `tools/wire-test.py`
  - adds MySQL wire smoke coverage for `ALTER TABLE ... REBUILD` syntax and guard rails

Targeted validation for `39.19` passed on the clustered rebuild surface:

- `cargo test -p axiomdb-sql --test integration_clustered_rebuild -j1`
- `cargo test -p axiomdb-sql --test integration_clustered_create_table -j1`
- `cargo check -p axiomdb-sql --lib -j1`
- `cargo clippy -p axiomdb-sql --lib --test integration_clustered_rebuild -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::needless_borrow`
- `cargo build --bin axiomdb-server -j1`
- `python3 tools/wire-test.py`

## Review notes

- All `39.3` acceptance criteria from the spec are implemented.
- All `39.4` acceptance criteria from the spec are implemented.
- All `39.5` acceptance criteria from the spec are implemented.
- All `39.6` acceptance criteria from the spec are implemented.
- All `39.7` acceptance criteria from the spec are implemented.
- All `39.8` acceptance criteria from the spec are implemented.
- All `39.9` acceptance criteria from the spec are implemented.
- All `39.10` acceptance criteria from the spec are implemented.
- All `39.11` acceptance criteria from the spec are implemented.
- All `39.12` acceptance criteria from the spec are implemented.
- All `39.13` acceptance criteria from the spec are implemented.
- All `39.14` acceptance criteria from the spec are implemented.
- All `39.15` acceptance criteria from the spec are implemented except the
  intentionally deferred clustered index-only optimization.
- All `39.16` acceptance criteria from the spec are implemented.
- All `39.17` acceptance criteria from the spec are implemented.
- All `39.18` acceptance criteria from the spec are implemented.
- All `39.19` acceptance criteria from the spec are implemented except the
  explicitly deferred general page-allocation rollback tracking.
- No `unsafe` was introduced in the clustered tree path.
- No production `unwrap()` remains in the touched clustered files.
- No production `unwrap()` was introduced in the new clustered-secondary path.
- No production `unwrap()` was introduced in the new clustered-overflow path.
- No production `unwrap()` was introduced in the new clustered WAL / rollback path.
- No production `unwrap()` was introduced in the clustered crash recovery path.
- No production `unwrap()` was introduced in the clustered DDL / runtime-guard path.
- No production `unwrap()` remains in the new clustered rebuild path.
- Benchmarking remains intentionally deferred: `39.5` finishes the storage-level
  clustered read slice, and `39.6` / `39.7` / `39.8` / `39.9` add the first clustered mutation, rebalance, and bookmark slices,
  `39.10` adds overflow-backed row storage, `39.11` adds internal WAL/rollback support, `39.12` adds internal clustered crash recovery,
  `39.13` exposes the first SQL-visible clustered DDL boundary, `39.14`
  exposes the first SQL-visible clustered INSERT path, `39.15` exposes the
  first SQL-visible clustered read path, `39.16` exposes clustered UPDATE,
  `39.17` exposes clustered DELETE, `39.18` exposes clustered VACUUM, and
  `39.19` now exposes legacy heap→clustered rebuild, but broader clustered
  integration expansion and benchmarks still wait for `39.20`.

### 39.21 — Aggregate hash execution

**What was built**

A complete hash-based aggregate execution engine for `GROUP BY` queries with `COUNT`, `SUM`,
`AVG`, `MIN`, `MAX`, and `GROUP_CONCAT`, plus a zero-allocation clustered scan path that
unlocks the full performance potential of the aggregate executor.

**Architecture — two-layer design**

The aggregate engine uses two specialized hash table types to avoid generic overhead:

- `GroupTablePrimitive` — single-column `GROUP BY` on integer-like values (`INT`, `BIGINT`,
  `DOUBLE`, `Bool`). Maps `i64` key → `GroupEntry` via `hashbrown::HashMap<i64, usize>`.
  Avoids serialization: key comparison is a single integer comparison.

- `GroupTableGeneric` — multi-column `GROUP BY`, text columns, mixed types, and the global
  no-GROUP-BY case. Serializes group keys into a `Vec<u8>` reused across rows (zero alloc
  if capacity fits), maps `&[u8]` → `GroupEntry` via `hashbrown::HashMap<Box<[u8]>, usize>`.

Each `GroupEntry` holds:
- `key_values: Vec<Value>` — evaluated GROUP BY expression values (stored for output)
- `non_agg_col_values: Vec<Value>` — sparse slice of non-aggregate SELECT columns
- `accumulators: Vec<AggAccumulator>` — one per aggregate function in the query

**Accumulator fast path**

`value_agg_add` replaces the `eval()`-based dispatch for `SUM`, `MIN`, `MAX`, and `COUNT`:
direct arithmetic on `Value` variants, no expression evaluation overhead. `finalize_avg`
divides `sum / count` using exact `f64` arithmetic, returning `Value::Null` when count = 0.

**Column decode mask**

Before scanning, `collect_expr_columns` walks all expressions in SELECT, WHERE, GROUP BY,
HAVING, and ORDER BY to build a `Vec<bool>` decode mask. Only columns referenced by at least
one expression are decoded from the row bytes — unused columns (e.g., large TEXT fields in an
`AVG(score)` query) are skipped at the codec level. The mask is passed as `Option<&[bool]>` to
`scan_clustered_table_masked`, which forwards it to `decode_row_masked`.

**Zero-allocation clustered scan (`scan_all_callback`)**

`axiomdb_storage::clustered_tree::scan_all_callback` walks B-tree leaves directly via the
`leftmost_leaf_pid` + `next_leaf` linked list, bypassing `ClusteredRangeIter` entirely.
For inline rows (the common case), it provides a `&[u8]` slice directly from the leaf page
memory — no `ClusteredRow` struct, no `cell.key.to_vec()`, no `reconstruct_row_data` copy.
This reduces allocations from ~3 per row to ~1 per row (just the `Vec<Value>` for decode).

**Performance impact (bench_users — 50K rows, clustered, MmapStorage, wire protocol)**

| Query | Before | After | vs MariaDB | vs MySQL |
|---|---|---|---|---|
| GROUP BY age + AVG(score) | 57 ms | 4.0 ms | **1.6× faster** (6.5ms) | **2.2× faster** (8.9ms) |
| COUNT(*) no GROUP BY | 0.8 ms | 0.8 ms | — | — |

14.25× improvement. Bottleneck was the scan phase (3 allocs/row × 50K rows),
not the aggregate computation. The aggregate hash tables add ~0.3ms overhead over a raw
scan.

**Correctness verification**

- `SELECT SUM(score) FROM bench_users` via `CountStart + AVG × COUNT` cross-check: 0.0%
  relative error across all 62 age groups
- 11 integration tests (`crates/axiomdb-sql/tests/integration_aggregate_hash.rs`): INT GROUP BY,
  COUNT(*) empty table, MIN/MAX, HAVING SUM, NULL group, AVG all-NULL → Null, non-agg col in
  SELECT, SUM INT, multi-column GROUP BY, TEXT GROUP BY, HAVING non-agg column
- 9 wire protocol smoke tests added to `tools/wire-test.py` (section `39.21`)

**Acceptance criteria**

- All `39.21` acceptance criteria from the spec are implemented.
- No production `unwrap()` was introduced.
- No `unsafe` was introduced.
- `GroupConcat` columns correctly handled by `collect_expr_columns` (fixed a fallthrough bug
  that would have decoded GROUP_CONCAT columns as Null when the decode mask was active).

## Deferred

- later clustered root persistence — today `39.12` still rebuilds roots from surviving WAL history, so checkpoint/rotation persistence is not solved yet
- later clustered FK work — child-side FK enforcement still rejects clustered child tables
- later transactional page-allocation tracking — `39.19` cleans newly built
  rebuild roots best-effort inside the statement path, but generic rollback of
  freshly allocated pages remains a broader future concern
- `39.20` — clustered integration expansion and benchmarks

## Notes

- `39.2` is intentionally storage-first. It does **not** replace the current
  `axiomdb-index::BTree` yet.
- `39.3` keeps that same boundary: clustered inserts now work in storage, but no
  SQL path creates or writes clustered tables yet.
- `39.4` keeps the same boundary on reads: clustered point lookup exists in
  storage, but no SQL `SELECT` path uses it yet.
- `39.5` extends that same boundary to ordered reads: clustered range scan now
  exists in storage, but it is still not reachable from SQL.
- `39.6` extends the storage rewrite to mutation: clustered same-leaf update now
  exists in storage, but no SQL `UPDATE` path uses it yet.
- `39.7` extends that same storage rewrite to logical delete: clustered
  delete-mark now exists in storage, but no SQL `DELETE` path uses it yet.
- `39.8` extends that same storage rewrite to structural maintenance:
  clustered rebalance and relocate-update now exist in storage, but no SQL
  `UPDATE` / `DELETE` path uses them yet.
- `39.9` extends that same clustered rewrite to secondary identity:
  bookmark-bearing secondary keys now exist as a dedicated path, but the SQL
  executor, FK enforcement, and index-integrity rebuilds still use the classic
  heap `RecordId` secondary model.
- `39.10` closes the old clustered-leaf gap:
  large logical rows no longer stop at an explicit reject path; they now spill
  their tail bytes to dedicated overflow pages while keeping PK bytes and
  `RowHeader` inline in the clustered leaf.
- `39.11` keeps the same storage-first boundary:
  clustered rollback and savepoint undo now exist inside `axiomdb-wal`, but the
  SQL executor still does not expose clustered tables.
- `39.12` extends that same boundary to crash safety:
  clustered WAL entries now recover correctly on reopen, but clustered roots are
  still reconstructed from WAL history rather than a catalog/meta-page source,
  so checkpoint/rotation persistence remains future work.
- `39.13` is the first SQL-visible clustered cut:
  `CREATE TABLE` with an explicit `PRIMARY KEY` now creates a clustered table
  root plus logical PK metadata, but heap-only executor paths still fail
  explicitly on clustered tables until `39.14` through `39.17`.
- `39.14` opens the first clustered DML write path:
  `INSERT` now works on clustered tables and writes directly through the
  clustered tree plus clustered secondary bookmarks, but clustered SQL reads and
  later mutators still remain deferred.
- `39.22` eliminates 5 heap allocations per matched UPDATE row:
  `fused_clustered_scan_patch` now reads and writes field bytes directly in the
  page buffer via `patch_field_in_place` (InnoDB `btr_cur_upd_rec_in_place`
  equivalent), `FieldDelta` uses `[u8;8]` inline arrays instead of `Vec<u8>`,
  and ROLLBACK is handled by the new `UndoClusteredFieldPatch` undo op which
  reverses only the changed bytes without restoring a full row image. Overflow
  rows fall back to the existing full-rewrite path.

### 39.22 — UPDATE in-place zero-alloc patch

**Root cause eliminated:** `fused_clustered_scan_patch` previously made 5 heap
allocations per matched row:
1. `local_row_data: cell.row_data.to_vec()` — full row clone for phase-1 offset scan
2. `patched_data = ...clone()` — full row clone for mutation
3. `encode_cell_image()` in `rewrite_cell_same_key_with_overflow` — new Vec
4. `FieldDelta.old_bytes: Vec<u8>` — per-field WAL old bytes
5. `FieldDelta.new_bytes: Vec<u8>` — per-field WAL new bytes

For 25K rows this was ~125K allocations per UPDATE statement.

**New fast path (inline cells — >99% of rows):**

```
Phase 1: Collect PatchInfo (immutable borrow on scan loop)
         - No row_data clone
         - Stores changed_fields: Vec<(col_pos, new_value)>

Phase 2: Per-page patch loop (one read+write per leaf page)
   Read phase (immutable borrow):
     cell_row_data_abs_off(&page, idx) → (abs_off, key_len)  // no clone
     compute_field_location_runtime(row_slice, bitmap) → FieldLocation
     MAYBE_NOP: old_bytes == new_encoded? → skip field
     Capture: old_buf: [u8;8] from page bytes
   Write phase (mutable borrow):
     patch_field_in_place(&mut page, field_abs, new_bytes)
     update_row_header_in_place(&mut page, idx, &new_header)
   WAL: FieldDelta { offset: u16, size: u8, old_bytes: [u8;8], new_bytes: [u8;8] }
```

**New page primitives** (`crates/axiomdb-storage/src/clustered_leaf.rs`):
- `cell_row_data_abs_off(page, idx)` → `(row_data_abs_off, key_len)`: computes
  absolute page offset of row_data without decoding the cell
- `patch_field_in_place(page, field_abs_off, bytes)`: writes bytes at exact page offset
- `update_row_header_in_place(page, idx, header)`: writes RowHeader without cloning

**`FieldDelta` change** (`crates/axiomdb-wal/src/clustered.rs`):
```rust
// Before
pub struct FieldDelta { pub offset: u16, pub size: u8, pub old_bytes: Vec<u8>, pub new_bytes: Vec<u8> }
// After
pub struct FieldDelta { pub offset: u16, pub size: u8, pub old_bytes: [u8;8], pub new_bytes: [u8;8] }
```
WAL serialization is byte-identical (only `size` bytes are written); existing
recovery code works unchanged.

**ROLLBACK via `UndoClusteredFieldPatch`** (`crates/axiomdb-wal/src/txn.rs`):
Previous path stored `UndoClusteredRestore` with `old_row_data: Vec::new()`,
causing ROLLBACK to write empty rows (corrupting the clustered cell). New variant:
```rust
UndoClusteredFieldPatch {
    table_id: u32,
    key: Vec<u8>,
    old_header: RowHeader,
    field_deltas: Vec<FieldDelta>,  // [u8;8] per field — no heap per field
}
```
Handler: descend to leaf → search for key → for each delta, write `old_bytes`
back at `row_data_abs_off + delta.offset`, then restore header.

**Test coverage:**
- `clustered_update_inplace_fixed_rollback` — ROLLBACK via UndoClusteredFieldPatch
- `clustered_update_inplace_maybe_nop_same_bytes` — Value-level NOP detection
- `clustered_update_inplace_mixed_schema_text_before_target` — runtime offset scan
- `clustered_update_inplace_multiple_fixed_columns` — multi-field WAL + rollback
- 7 wire-test assertions covering single-field patch, multi-field patch,
  ROLLBACK restore, and TEXT-before-INT schema
