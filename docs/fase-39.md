# Phase 39 — Clustered Index Storage Engine

## Subfases completed in this session: 39.2, 39.3

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

## Review notes

- All `39.3` acceptance criteria from the spec are implemented.
- No `unsafe` was introduced in the clustered tree path.
- No production `unwrap()` remains in the touched clustered files.
- Benchmarking is intentionally deferred: `39.3` does not expose lookup/scan yet,
  so the benchmark gate starts after `39.4` / `39.5`.

## Deferred

- `39.4` / `39.5` — lookup and range scan over clustered pages
- `39.8` — parent maintenance during split / merge
- `39.11+` — WAL and crash recovery for clustered operations
- `39.13+` — executor integration for clustered tables

## Notes

- `39.2` is intentionally storage-first. It does **not** replace the current
  `axiomdb-index::BTree` yet.
- `39.3` keeps that same boundary: clustered inserts now work in storage, but no
  SQL path creates or writes clustered tables yet.
- `39.1` remains open because large-row overflow cells are still deferred to
  `39.10`, even though the clustered leaf groundwork already exists in storage.
