# Plan: 5.18 — Heap insert tail-pointer cache

## Files to create/modify

- `crates/axiomdb-storage/src/heap_chain.rs`
  - add a transient heap-append hint type
  - add hint-aware insert helpers
  - keep the current no-hint APIs as wrappers
  - add storage-level tests for stale-hint recovery and chain-growth updates
- `crates/axiomdb-sql/src/session.rs`
  - add per-table heap-tail cache state
  - clear it on `invalidate_table(...)` and `invalidate_all()`
  - add unit tests for root mismatch and invalidation behavior
- `crates/axiomdb-sql/src/table.rs`
  - add hint-aware insert/update helpers
  - wire ctx-aware insert/update paths to the session tail cache
  - keep current public wrappers for no-hint callers
- `crates/axiomdb-sql/src/executor.rs`
  - add statement-local tail-hint reuse in non-ctx repeated insert/update loops
  - keep batch paths unchanged unless they opt into the same hint helper
- `crates/axiomdb-storage/src/lib.rs`
  - re-export the new hint type if needed by `axiomdb-sql`
- `crates/axiomdb-storage/tests/` or `heap_chain.rs` inline tests
  - add read-count / stale-hint regression coverage
- `benches/comparison/local_bench.py`
  - keep `insert` scenario as the main regression target for 100K-row append

## Reviewed first

These files were reviewed before writing this plan:

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
- `specs/fase-05/spec-5.18-heap-insert-tail-pointer-cache.md`

Research reviewed before writing this plan:

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

- The hot cost today is `HeapChain::last_page_id(...)` inside
  `HeapChain::insert(...)`, not the actual tuple write itself.
- `insert_batch(...)` already proves the right local optimization strategy:
  resolve tail once, reuse local page state, then flush.
- Real long-running write paths already carry stable runtime state in
  `SessionContext`, both in the wire server and in the embedded API.
- Persisting tail state in `TableDef` would be the wrong layer:
  it adds catalog churn, extra invalidation, and root-rotation complexity for a
  purely runtime optimization.

### What we borrow / reject / adapt

- `research/postgres/src/include/access/hio.h`
  - borrow: a dedicated append-state object
  - adapt: a much smaller `HeapAppendHint` instead of full bulk-insert state
- `research/postgres/src/backend/access/heap/hio.c`
  - borrow: try cached target first, validate, fallback, self-heal
  - adapt: validate with `next_page_id == 0` instead of FSM and free-space map
- `research/sqlite/src/btree.h`
  - borrow: explicit append-intent bias
  - adapt: explicit hint-aware insert helpers in `HeapChain`
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: optimistic hot path vs structural fallback
  - adapt: optimistic tail-page append vs chain-growth path
- `research/duckdb/src/include/duckdb/storage/table/append_state.hpp`
  - borrow: append state belongs to runtime execution state
  - adapt: store per-table append state in `SessionContext`
- `research/oceanbase/src/sql/das/ob_das_insert_op.h`
  - borrow: DML operator-local write buffering/state
  - adapt: non-ctx executor loops get a statement-local tail hint
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
  - borrow: keep optimization intent explicit and testable
  - adapt: dedicated tests for hint reuse vs fallback

## Algorithm / Data structures

### 1. New transient append-hint type

Add a small runtime-only hint in `heap_chain.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapAppendHint {
    pub root_page_id: u64,
    pub tail_page_id: u64,
}
```

This is not persisted anywhere. It is only a runtime locator hint.

### 2. Hint-aware tail resolution

Add a private helper in `HeapChain`:

```rust
fn resolve_tail_page_id(
    storage: &dyn StorageEngine,
    root_page_id: u64,
    hint: Option<&mut HeapAppendHint>,
) -> Result<u64, DbError>
```

Behavior:
1. If there is no hint, call `last_page_id(...)`.
2. If `hint.root_page_id != root_page_id`, ignore the hint and call `last_page_id(...)`.
3. If the hint matches the root:
   - read `hint.tail_page_id`
   - if `chain_next_page(page) == 0`, accept it as the tail
   - otherwise treat it as stale and call `last_page_id(...)`
4. Whenever fallback happens, rewrite the hint to the actual tail page.

This keeps correctness even if the hint is stale.

### 3. Hint-aware insert APIs

Add new APIs rather than breaking the old ones:

```rust
pub fn insert_with_hint(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
    data: &[u8],
    txn_id: TxnId,
    hint: Option<&mut HeapAppendHint>,
) -> Result<(u64, u16), DbError>

pub fn insert_batch_with_hint(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
    rows: &[Vec<u8>],
    txn_id: TxnId,
    hint: Option<&mut HeapAppendHint>,
) -> Result<Vec<(u64, u16)>, DbError>
```

Existing `insert(...)` and `insert_batch(...)` remain as wrappers:

```rust
insert(...) = insert_with_hint(..., None)
insert_batch(...) = insert_batch_with_hint(..., None)
```

### 4. Chain-growth contract

In `insert_with_hint(...)`:

```text
tail = resolve_tail_page_id(...)
try insert into tail
if fits:
    write tail page
    hint.tail_page_id = tail
else:
    alloc new page
    write new page first
    update next_page_id in old tail
    write old tail
    hint.tail_page_id = new_page_id
```

The current crash-safety order stays unchanged:
1. write new page first
2. then link it from the previous tail

### 5. SessionContext cache shape

Add a per-table heap-tail cache entry in `SessionContext`:

```rust
struct HeapTailCacheEntry {
    root_page_id: u64,
    tail_page_id: u64,
}

heap_tail_cache: HashMap<u32, HeapTailCacheEntry> // keyed by table_id
```

Add helpers:

```rust
pub fn get_heap_tail_hint(&self, table_id: u32, root_page_id: u64) -> Option<HeapAppendHint>
pub fn set_heap_tail_hint(&mut self, table_id: u32, root_page_id: u64, tail_page_id: u64)
pub fn invalidate_heap_tail(&mut self, table_id: u32)
```

Rules:
- root mismatch returns `None`
- `invalidate_table(...)` must also clear any heap-tail hint for that table if
  the cached `ResolvedTable` is present
- `invalidate_all()` clears both schema cache and heap-tail cache

### 6. TableEngine integration

Add internal hint-aware helpers in `table.rs`:

```rust
fn insert_encoded_row_with_hint(...)
pub fn insert_row_with_hint(...)
pub fn insert_rows_batch_with_hint(...)
pub fn update_row_with_hint(...)
pub fn update_rows_batch_with_hint(...)
```

Keep current APIs as wrappers that pass `None`.

Ctx-aware variants:
- load the hint from `SessionContext`
- pass it into `HeapChain::*_with_hint`
- write the updated hint back into `SessionContext`

### 7. Executor integration

Ctx path:
- no extra local state required beyond `SessionContext`

Non-ctx path:
- add a statement-local `Option<HeapAppendHint>` inside repeated single-row
  insert/update loops
- reuse it across rows of the same statement
- this covers:
  - `execute_insert(...)` per-row fallback with secondary indexes
  - `INSERT ... SELECT`
  - `execute_update(...)` reinserts

Batch paths may use the same `_with_hint` helper for consistency, but the main
goal is the single-row hot path.

## Implementation phases

1. Add `HeapAppendHint` and hint validation helpers in `heap_chain.rs`.
2. Add `insert_with_hint(...)` and `insert_batch_with_hint(...)`, keep old
   wrappers intact.
3. Add heap-tail cache fields and invalidation helpers in `SessionContext`.
4. Add hint-aware insert/update helpers in `TableEngine`.
5. Wire ctx-aware insert/update paths to session tail cache.
6. Wire non-ctx repeated insert/update loops to a statement-local hint.
7. Add regression tests for stale hints, root mismatch, and linearized page-read
   behavior.

## Tests to write

- unit (`heap_chain.rs`):
  - valid hint skips `last_page_id(...)` walk
  - stale hint (`next_page_id != 0`) falls back and self-heals
  - chain growth updates the hint to the new tail
  - `insert_batch_with_hint(...)` preserves row order and updates final tail
- unit (`session.rs`):
  - root mismatch returns no hint
  - `invalidate_all()` clears heap-tail cache
  - per-table invalidation clears the matching tail hint
- integration / executor:
  - repeated single-row INSERT in one statement/session reuses the same tail hint
  - per-row fallback with secondary indexes still reuses the tail hint
  - UPDATE reinserts reuse the hint on the insert half
  - root rotation/truncate invalidates the old hint
- perf oracle:
  - use a `CountingStorage` wrapper to count `read_page(...)`
  - prove that repeated appends no longer perform one full tail walk per row
- bench:
  - run `benches/comparison/local_bench.py --scenario insert --rows 100000`
  - compare before/after ops/s for AxiomDB

## Anti-patterns to avoid

- Do not add a persistent `tail_page_id` field to `TableDef`.
- Do not trust cached tail state without checking `next_page_id == 0`.
- Do not clear only schema cache and leave heap-tail cache alive.
- Do not regress `insert_batch(...)` just to share code.
- Do not introduce a free-space map, heap compaction, or tail reclamation in
  this subphase.

## Risks

| Risk | Mitigation |
|---|---|
| Root rotation from Phase 5.16 makes the cached tail point into an old chain | Store `root_page_id` in the cache entry and reject mismatches |
| A stale tail hint silently appends to a non-tail page | Always validate `chain_next_page(page) == 0` before using the hint |
| Hint bookkeeping adds API churn to many call sites | Keep old APIs as wrappers and localize the new logic to hint-aware variants |
| Optimization helps only ctx-aware paths | Also add statement-local hint reuse in non-ctx executor loops |
| Validation read erases the benefit | One tail-page validation read is still far cheaper than walking hundreds or thousands of heap pages |

## Assumptions

- Phase 5 remains single-writer, so append hints do not need cross-writer
  coordination.
- Heap pages remain append-only linked-list pages; no mid-chain free-space
  placement is introduced here.
- Embedded and server hot paths both keep a long-lived `SessionContext`, so the
  session cache is a real optimization surface in both products.
