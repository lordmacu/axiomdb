# Spec: 5.18 — Heap insert tail-pointer cache

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-catalog/src/schema.rs`
- `crates/axiomdb-catalog/src/resolver.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `benches/comparison/local_bench.py`
- `crates/axiomdb-index/tests/integration_btree.rs`

These research files were reviewed before writing this spec:

- `research/sqlite/src/btree.h`
- `research/sqlite/src/btree.c`
- `research/postgres/src/include/access/hio.h`
- `research/postgres/src/backend/access/heap/hio.c`
- `research/postgres/src/backend/access/heap/heapam.c`
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
- `research/mariadb-server/storage/innobase/page/page0cur.cc`
- `research/duckdb/src/include/duckdb/storage/table/append_state.hpp`
- `research/duckdb/src/storage/local_storage.cpp`
- `research/oceanbase/src/sql/das/ob_das_insert_op.h`
- `research/oceanbase/src/sql/das/ob_das_insert_op.cpp`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### AxiomDB-first constraints

- `HeapChain::insert(...)` currently calls `last_page_id(root_page_id)` on every
  single-row append.
- `last_page_id(...)` walks the full heap chain from the root page to the tail,
  so repeated inserts on a large table are `O(P)` per row, where `P` is heap
  page count.
- `HeapChain::insert_batch(...)` already avoids this by walking to the tail once
  and then reusing a local page copy across the batch, so the real remaining
  problem is the single-row append path used by:
  - `TableEngine::insert_row(...)`
  - `TableEngine::insert_row_with_ctx(...)`
  - `TableEngine::update_row(...)`
  - `TableEngine::update_row_with_ctx(...)`
  - per-row executor loops that remain necessary when secondary indexes are
    maintained row-by-row
- `TableDef` is catalog metadata, not execution state. Persisting the current
  tail page there would turn a hot local optimization into repeated catalog
  churn, root-cache invalidation, and extra transactional surface.
- Real execution paths already have persistent session state:
  - server connections execute through `SessionContext`
  - embedded `Db` also keeps one `SessionContext` across statements
- Phase 5 is still effectively single-writer, so a runtime append hint is valid
  as long as it is validated before use.

### What we borrow

- `research/sqlite/src/btree.h`
  - borrow: `BTREE_APPEND` explicitly marks high-end insert intent
- `research/sqlite/src/btree.c`
  - borrow: append-heavy inserts should bias toward the current high end instead
    of re-searching from scratch every time
- `research/postgres/src/include/access/hio.h`
  - borrow: a dedicated runtime append/bulk-insert state object is better than
    encoding append state in persistent relation metadata
- `research/postgres/src/backend/access/heap/hio.c`
  - borrow: try the cached target page first, validate it, and fall back to the
    slower locator only when the hint no longer works
- `research/postgres/src/backend/access/heap/heapam.c`
  - borrow: the append target belongs to execution state, not to on-disk schema
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: split optimistic page-local write from the slower structural path
- `research/mariadb-server/storage/innobase/page/page0cur.cc`
  - borrow: keep the hot insert path on the current page whenever space exists
- `research/duckdb/src/include/duckdb/storage/table/append_state.hpp`
  - borrow: append state is a transient runtime structure associated with one
    append stream, not persisted table metadata
- `research/duckdb/src/storage/local_storage.cpp`
  - borrow: initialize append state once, then reuse it across repeated appends
- `research/oceanbase/src/sql/das/ob_das_insert_op.h`
  - borrow: insert operators carry their own write/append state
- `research/oceanbase/src/sql/das/ob_das_insert_op.cpp`
  - borrow: append buffering belongs to the DML operator/runtime path, not to
    schema rows
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
  - borrow: optimization intent should stay explicit and testable, not hidden
    inside unrelated metadata updates

### What we reject

- storing the current heap tail page in `TableDef` or any other catalog row
- updating catalog metadata on every heap-chain extension
- trusting a cached tail blindly without validating that it is still the last
  page in the chain
- forcing every repeated insert path through `insert_batch(...)` just to gain
  tail reuse
- building a full free-space map or page directory in Phase 5

### How AxiomDB adapts it

- `5.18` adds a transient, validated heap-append tail hint
- the hint is cached per table in `SessionContext` for ctx-aware execution
- non-ctx executor loops may also hold a statement-local hint so the same
  optimization works in legacy/non-session paths
- the hint stores both the current `data_root_page_id` and the current tail
  page ID, so root rotation from Phase 5.16 naturally invalidates it
- every use of the hint must validate that the hinted page still has
  `next_page_id == 0`; on mismatch, AxiomDB falls back to a full walk and
  repairs the hint

## What to build (not how)

Implement a transient heap tail-pointer cache for append-heavy single-row heap
writes so AxiomDB no longer walks the full heap chain from the root on every
single-row INSERT or UPDATE-as-delete+insert.

### Surface 1: runtime heap tail hint

AxiomDB must introduce a runtime tail hint for heap append operations.

The hint must:
- identify which heap chain it belongs to
- remember the current tail page ID for that chain
- be safe to discard at any time
- never become part of the persisted catalog or WAL format

### Surface 2: validated fast path

When a tail hint exists for a heap chain, AxiomDB must:
- try the hinted tail page first
- validate that it is still the tail (`next_page_id == 0`)
- append directly there if space exists

If the hint is stale, AxiomDB must:
- fall back to the existing `last_page_id(...)` walk
- repair the hint with the actual tail page ID

### Surface 3: chain growth updates the hint

When the current tail page is full and the heap chain grows:
- the new page must still be written before linking it from the old tail
- after the link is persisted, the tail hint must point to the new page

### Surface 4: session-aware reuse

Ctx-aware execution paths must reuse the heap tail hint across repeated writes
to the same table in the same session.

This includes:
- repeated single-row `INSERT`
- per-row fallback inside multi-row inserts with secondary-index maintenance
- `INSERT ... SELECT`
- update paths that reinsert rows into the heap

### Surface 5: invalidation rules

The heap tail cache must be cleared whenever the session schema cache is
invalidated, and any root-page mismatch must invalidate the old tail hint for
that table.

## Inputs / Outputs

- Input:
  - heap chain root page ID (`TableDef.data_root_page_id`)
  - encoded row payload bytes
  - active transaction ID
  - optional runtime tail hint
  - optional `SessionContext` carrying per-table tail hints
- Output:
  - unchanged physical row location result: `(page_id, slot_id)` / `RecordId`
  - updated runtime tail hint after successful append
- Errors:
  - unchanged heap/storage/WAL errors from the current insert path
  - no new SQL syntax, protocol behavior, or user-visible error surface

## Use cases

1. A connection inserts 100,000 rows into the same table one by one.
   After the first tail resolution, subsequent inserts reuse the cached tail
   instead of walking the full heap chain on each row.

2. A multi-row INSERT with secondary indexes still falls back to per-row index
   maintenance.
   The heap append path must still reuse the same tail hint across those rows.

3. `UPDATE` rewrites rows as delete+insert.
   The insert half must reuse the same tail hint across repeated updated rows.

4. A table is truncated or bulk-emptied, rotating `data_root_page_id`.
   The old hint must not be reused for the new root.

5. A stale hint points to a page that is no longer the tail.
   AxiomDB must fall back to the current full walk and self-heal the hint.

## Acceptance criteria

- [ ] Single-row heap append no longer walks the full chain from the root on
      every row when repeated writes target the same table in the same session
      or statement.
- [ ] `HeapChain::insert_batch(...)` semantics remain unchanged; `5.18` does
      not regress the already-optimized batch path.
- [ ] The tail hint is transient runtime state only; no `TableDef` or catalog
      row gains a persisted tail-page field in this subphase.
- [ ] Any hinted tail page is validated before use; stale hints fall back to
      full traversal and repair themselves.
- [ ] Heap-chain growth updates the hint to the newly appended tail page.
- [ ] Session cache invalidation also clears heap tail hints.
- [ ] Root-page mismatch invalidates the old hint for that table/chain.
- [ ] Ctx-aware single-row insert/update paths reuse the heap tail hint.
- [ ] Non-ctx executor loops that perform repeated single-row appends can reuse
      a statement-local tail hint.
- [ ] The benchmarked 100K-row append path no longer exhibits `O(N²)` heap-tail
      traversal behavior.

## Out of scope

- Persistent tail pointer in catalog metadata
- Free-space map or page directory for arbitrary-page heap placement
- Heap-chain shrinking or tail reclamation after deletes/rollback
- Global/shared append target for future concurrent writers
- Public Rust prepared-statement API changes (Phase 10.8)

## Dependencies

- Current `HeapChain` linked-page append semantics
- Current `SessionContext` lifetime in server and embedded APIs
- Phase 5 single-writer execution model
- Phase 5.16 root rotation for bulk-empty/truncate invalidation scenarios

## ⚠️ DEFERRED

- Cross-session/global append-target sharing → pending in Phase 7 concurrency work
- Smarter free-space selection beyond append-only tail reuse → pending in a
  later heap/free-space subphase
- Public embedded API specialized bulk append state → pending after Phase 10.8
