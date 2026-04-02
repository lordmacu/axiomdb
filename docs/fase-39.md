# Phase 39 — Clustered Index Storage Engine

## Subfases completed in this session: 39.2, 39.3, 39.4, 39.5, 39.6, 39.7, 39.8, 39.9, 39.10

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

## Review notes

- All `39.3` acceptance criteria from the spec are implemented.
- All `39.4` acceptance criteria from the spec are implemented.
- All `39.5` acceptance criteria from the spec are implemented.
- All `39.6` acceptance criteria from the spec are implemented.
- All `39.7` acceptance criteria from the spec are implemented.
- All `39.8` acceptance criteria from the spec are implemented.
- All `39.9` acceptance criteria from the spec are implemented.
- All `39.10` acceptance criteria from the spec are implemented.
- No `unsafe` was introduced in the clustered tree path.
- No production `unwrap()` remains in the touched clustered files.
- No production `unwrap()` was introduced in the new clustered-secondary path.
- No production `unwrap()` was introduced in the new clustered-overflow path.
- Benchmarking remains intentionally deferred: `39.5` finishes the storage-level
  clustered read slice, and `39.6` / `39.7` / `39.8` / `39.9` add the first clustered mutation, rebalance, and bookmark slices,
  `39.10` adds overflow-backed row storage, but end-to-end clustered DML benchmarks still wait for later SQL-visible integration.

## Deferred

- `39.18` — physical purge of dead clustered cells
- `39.11+` — WAL and crash recovery for clustered operations
- `39.13+` — executor integration for clustered tables

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
