# Plan: 11.2d — BLOB Reference Tracking

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_overflow.rs` — add a refcounted
  TOAST/BLOB chain format alongside the existing legacy chain functions.
- `crates/axiomdb-sql/src/table_write.rs` — use the refcounted write/release
  functions for TOAST externalization and cleanup.
- `crates/axiomdb-sql/src/table.rs` — read both refcounted and legacy TOAST
  chains when resolving TOAST placeholders.
- `crates/axiomdb-storage/tests/` or existing storage unit tests — cover the
  refcounted chain primitives and legacy compatibility.
- `crates/axiomdb-sql/tests/` — cover SQL-level large value insert/read/delete
  behavior.
- `docs/progreso.md`, `docs/fase-11.md`, `memory/project_state.md`,
  `memory/architecture.md`, and `memory/lessons.md` — closeout updates after
  implementation and validation.
- `docs-site/src/internals/storage.md`,
  `docs-site/src/internals/row-codec.md`, and
  `docs-site/src/user-guide/sql-reference/data-types.md` — document the
  refcounted TOAST/BLOB chain contract and user-visible large-value behavior.

## Algorithm / Data structure

Research basis:

- PostgreSQL TOAST (`research/postgresql/src/backend/access/common/toast_internals.c`)
  uses a separate chunk relation keyed by a value OID and deletes chunks by
  value OID. It avoids duplicate copies during rewrite by reusing an existing
  value OID when possible, but it does not maintain a page-level refcount.
- SQLite overflow (`research/sqlite/src/btree.c`) uses the leanest linked-page
  shape: first overflow pointer in the cell, next-page pointer in every overflow
  page, and pointer-map acceleration for auto-vacuum. It has no sharing or
  refcount.
- InnoDB external BLOBs (`research/mariadb/storage/innobase/include/btr0cur.h`
  and `research/mariadb/storage/innobase/btr/btr0cur.cc`) encode ownership and
  inherited flags in the inline external-field reference, so only the owner
  record frees a BLOB. That is excellent for MVCC row-version inheritance, but
  it cannot express content-addressed N-to-1 dedup by itself.

Selected option: a versioned refcounted BLOB chain only for TOAST/BLOB-owned
chains. It is more robust than changing the legacy clustered overflow format,
more local than a sidecar catalog table, and more future-proof than an ownership
bit because Phase 14.9 needs true shared physical BLOBs.

Keep the existing legacy overflow body layout unchanged:

```text
legacy overflow body:
  [next_page: u64 LE]
  [payload bytes...]
```

Add a versioned refcounted TOAST/BLOB layout:

```text
refcounted overflow body, first page:
  [magic: 4 bytes = "ABOB"]
  [version: u8 = 1]
  [flags: u8]
  [reserved: u16 LE]
  [next_page: u64 LE]
  [part_len: u32 LE]
  [refcount: u64 LE]
  [payload bytes...]

refcounted overflow body, continuation page:
  [magic: 4 bytes = "ABOB"]
  [version: u8 = 1]
  [flags: u8]
  [reserved: u16 LE]
  [next_page: u64 LE]
  [part_len: u32 LE]
  [refcount: u64 LE = 0]
  [payload bytes...]
```

Only the first page owns the refcount. Continuation pages carry the same magic
and version so the read/free path can detect mixed or corrupt chains. Every page
stores `part_len`, which makes refcounted chains self-delimiting and avoids
reading zero-filled tail bytes from compressed TOAST/BLOB chunks.

Pseudocode:

```rust
fn write_refcounted_chain(storage, batch, payload) -> Result<Option<u64>> {
    if payload.is_empty() { return Ok(None); }
    allocate pages
    for each page:
        write magic/version/next/part_len header
        if first: write refcount = 1 else refcount = 0
        copy payload chunk after header
    return first_page
}

fn read_blob_chain(storage, first_page, expected_len) -> Result<Vec<u8>> {
    page = read first_page
    if has_refcounted_magic(page):
        read each v1 page using part_len
    else:
        read with legacy clustered_overflow::read_chain
}

fn free_blob(storage, first_page) -> Result<()> {
    page = read first_page
    if !has_refcounted_magic(page):
        return free_chain(storage, first_page)

    refcount = read_refcount(first_page)
    if refcount == 0: return corruption
    if refcount > 1:
        write_refcount(first_page, refcount - 1)
        return Ok(())
    free every page in chain
}
```

```rust
fn incref_blob(storage, first_page) -> Result<u64> {
    read first page
    validate refcounted magic/version
    checked_add refcount
    write first page checksum
    return new count
}
```

Implement `incref_blob()` now because it is the only way to test the
`free_blob()` decrement-without-free branch before Phase 14.9 adds
content-addressed lookup.

## Implementation phases

1. Add constants and helpers in `clustered_overflow.rs`.
   - Magic/version constants.
   - Header-size helpers for first vs continuation pages.
   - `is_refcounted_chain_page()`.

2. Add refcounted chain primitives.
   - `write_refcounted_chain(...)`.
   - `read_blob_chain(...)` with legacy fallback.
   - `free_blob(...)`.
   - `incref_blob(...)` for the Phase 14.9 reuse path and current tests.

3. Wire TOAST write path.
   - Replace `clustered_overflow::write_chain(...)` in `toast_row_if_needed()`
     with `write_refcounted_chain(...)`.
   - Keep the inline TOAST sentinel encoding unchanged.

4. Wire TOAST read and cleanup paths.
   - Replace TOAST reads in `detoast_row()` with `read_blob_chain(...)`.
   - Replace `free_toast_chains_in_encoded()` release with `free_blob(...)`.
   - Keep clustered-row overflow callers on the legacy functions.

5. Add tests.
   - Storage unit tests for refcounted read/write/free.
   - Storage unit test where `incref_blob()` plus `free_blob()` does not free
     until the final release, if `incref_blob()` is implemented now.
   - Storage unit test for corrupt/underflow refcount.
   - Compatibility test reading and freeing a legacy chain through the new
     TOAST/BLOB helpers.
   - SQL integration test for large `TEXT` or `BYTES` insert/read/delete.

6. Update docs and memory after validation.
   - Update 11.2d status in `docs/progreso.md`; mark closed only after the full
     close gates pass.
   - Extend `docs/fase-11.md` with 11.2d.
   - Update docs-site user and internals pages.
   - Update memory files with the new storage contract.

## Tests to write

- unit: `write_refcounted_chain` + `read_blob_chain` round-trip for one page.
- unit: multi-page refcounted chain round-trip.
- unit: `free_blob` frees when refcount is `1`.
- unit: `free_blob` decrements and preserves pages when refcount is `2`, if
  `incref_blob` is added.
- unit: legacy `write_chain` result can be read/freed through
  `read_blob_chain` / `free_blob`.
- unit: corrupt first-page refcount `0` returns corruption.
- integration: large value insert/read/delete still works through SQL.

Benchmarks:

- No dedicated performance benchmark is required for 11.2d unless the SQL
  integration shows a regression. The hot read path adds only one first-page
  magic check per externalized value, and small inline values are unaffected.

## Anti-patterns to avoid

- Do not change the inline row-codec TOAST sentinel layout.
- Do not rewrite `clustered_overflow::read_chain()` to auto-detect refcounted
  pages for all callers; clustered-row overflow must remain on the legacy
  contract.
- Do not free a refcounted chain directly with `free_chain()`.
- Do not use a sidecar catalog table for the refcount; the progress requirement
  says the counter lives in the overflow page header.
- Do not claim content-addressed dedup is implemented. This subphase only adds
  the storage primitive required by Phase 14.9.

## Risks

- Header overhead reduces payload per overflow page.
  Mitigation: the overhead is small relative to 16 KB pages and applies only to
  externalized large values.

- Legacy and refcounted chains share `PageType::Overflow`.
  Mitigation: refcounted chains carry magic/version; new TOAST helpers fall back
  to the legacy reader/free path if the magic is absent.

- A caller may still use `free_chain()` on a refcounted chain.
  Mitigation: keep `free_blob()` private to the TOAST/BLOB path where possible,
  document the contract, and update all TOAST cleanup callsites in this subphase.

- Crash during refcount decrement can leave a stale refcount.
  Mitigation: 11.2d does not introduce dedup sharing yet, so the refcount is
  usually `1`; full content-store crash semantics belong with Phase 14.9/14.10
  once shared blobs can actually exist.
