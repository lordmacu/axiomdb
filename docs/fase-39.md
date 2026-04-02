# Phase 39 — Clustered Index Storage Engine

## Subfases completed in this session: 39.2, 39.3, 39.4, 39.5, 39.6

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

## Review notes

- All `39.3` acceptance criteria from the spec are implemented.
- All `39.4` acceptance criteria from the spec are implemented.
- All `39.5` acceptance criteria from the spec are implemented.
- All `39.6` acceptance criteria from the spec are implemented.
- No `unsafe` was introduced in the clustered tree path.
- No production `unwrap()` remains in the touched clustered files.
- Benchmarking remains intentionally deferred: `39.5` finishes the storage-level
  clustered read slice, and `39.6` adds the first clustered mutation slice, but
  end-to-end clustered DML benchmarks still wait for later SQL-visible integration.

## Deferred

- `39.7` — clustered delete-mark semantics
- `39.8` — parent maintenance during split / merge
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
- `39.1` remains open because large-row overflow cells are still deferred to
  `39.10`, even though the clustered leaf groundwork already exists in storage.
