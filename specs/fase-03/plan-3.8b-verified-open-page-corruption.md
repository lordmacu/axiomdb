# Plan: 3.8b — Verified Open And Early Page-Corruption Detection

## Files to create / modify

- `crates/axiomdb-storage/src/mmap.rs` — verify all pages during open
- `crates/axiomdb-storage/tests/integration_storage.rs` — corruption detected on open, not first read
- `crates/axiomdb-wal/tests/integration_durability.rs` — keep truncated-tail and recovery idempotency coverage
- `crates/axiomdb-network/src/mysql/database.rs` — use `TxnManager::open_with_recovery(...)`
- `crates/axiomdb-embedded/src/lib.rs` — use `TxnManager::open_with_recovery(...)`

## Algorithm / Data structure

### 1. Verified open

Keep the existing startup order:

1. map file
2. verify meta page (page 0)
3. load freelist page (page 1)

Then add:

```rust
for page_id in 2..page_count {
    Self::read_page_from_mmap(&mmap, page_id)?;
}
```

`read_page_from_mmap()` already performs:
- bounds check
- page cast
- checksum verification

So the open path only needs a deterministic full scan.

### 2. Recovery wiring

Replace:

```rust
let txn = TxnManager::open(&wal_path)?;
```

with:

```rust
let (txn, _result) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
```

in both:
- MySQL server open path
- embedded open path

The recovery result does not need to be persisted in Phase 3.8b; it only needs
to run before the database becomes usable.

## Implementation phases

1. Add full-page verification loop to `MmapStorage::open()`.
2. Swap server reopen path to `open_with_recovery(...)`.
3. Swap embedded reopen path to `open_with_recovery(...)`.
4. Add tests for corruption-on-open and keep existing WAL-tail behavior green.

## Tests to write

- unit:
  - none required beyond existing page checksum coverage
- integration:
  - corrupt a data page on disk, then assert `MmapStorage::open()` fails immediately
  - clean reopen still succeeds
  - repeated `open_with_recovery(...)` remains idempotent
- wire:
  - startup with corrupted page fails before the server accepts a connection

## Anti-patterns to avoid

- Do **not** leave corruption detection lazy on `read_page()` only.
- Do **not** bypass `open_with_recovery(...)` in any real reopen path.
- Do **not** silently ignore checksum failures at startup.
- Do **not** expand this subphase into full committed-page redo; that is a separate deferred gap.

## Risks

- Startup becomes O(number_of_pages).
  Mitigation: acceptable for this subphase because the explicit goal is “detect on open”.
- Full scan may duplicate work already done later by queries.
  Mitigation: intentional; fail-fast startup is the required behavior change.
