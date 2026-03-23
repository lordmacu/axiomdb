# Phase 3 — WAL and Transactions

**Status:** ✅ Completed (2026-03-22)
**Crates:** `nexusdb-wal`, `nexusdb-storage`, `nexusdb-catalog`
**Specs/Plans:** `specs/fase-03/`

---

## What was implemented

### WAL layer (3.1 – 3.3)
Binary WAL entry format with LSN, type, table_id, key, old/new values, CRC32c,
and a length trailer for backward scan. `WalWriter` (append-only, global LSN,
fsync on commit, `scan_last_lsn` on open). `WalReader` with `scan_forward`
(streaming) and `scan_backward` (using `entry_len_2` trailer).

### MVCC heap and transactions (3.4 – 3.5)
`RowHeader` (24 bytes, bytemuck::Pod): `txn_id_created`, `txn_id_deleted`,
`row_version`, `_flags`. Slotted heap pages with `insert_tuple`, `delete_tuple`,
`update_tuple`, `scan_visible` (MVCC filter), `mark_slot_dead`, `clear_deletion`.
`TxnManager`: single-writer BEGIN/COMMIT/ROLLBACK with WAL, undo log (physical
page+slot locations), `autocommit` wrapper, `active_snapshot` for read-your-own-writes.

### WAL checkpoint and rotation (3.6 – 3.7)
`Checkpointer`: 5-step protocol (flush → Checkpoint WAL entry → fsync → update
meta → flush). `checkpoint_lsn` stored in meta page body[24]. `WalRotator`:
header v2 with `start_lsn` field; triggers rotation when WAL exceeds
`max_wal_size`; atomic file rename.

### Crash recovery and integrity (3.8 – 3.10)
`CrashRecovery` state machine: `CRASHED → RECOVERING → REPLAYING_WAL →
VERIFYING → READY`. Replays committed transactions from last checkpoint; undoes
in-progress transactions using physical location encoding in WAL payloads.
`IntegrityChecker`: heap structural checks + MVCC invariant validation post-recovery.
9 durability test scenarios with real `MmapStorage` (corrupt checkpoint, partial
page write, truncated WAL, etc.).

### Catalog system (3.11 – 3.14)

#### 3.11 — Catalog Bootstrap
Meta page extended with catalog header at body[32..72]:
- body[32..40]: `catalog_tables_root`
- body[40..48]: `catalog_columns_root`
- body[48..56]: `catalog_indexes_root`
- body[56..60]: `catalog_schema_ver` (0=uninit, 1=v1)
- body[64..68]: `next_table_id` sequence
- body[68..72]: `next_index_id` sequence

Binary row types: `TableDef`, `ColumnDef` (with `ColumnType` enum, 8 variants),
`IndexDef` (with `index_id: u32` field). All have `to_bytes()` / `from_bytes()`
with length-prefixed variable-length string fields.

#### 3.12 — CatalogReader/Writer
`HeapChain`: multi-page linked heap using `PageHeader._reserved[0..8]` as
`next_page_id`. Crash-safe write order: new page written before chain pointer
updated. Sequences `alloc_table_id` / `alloc_index_id` with overflow protection.

`CatalogWriter`: `create_table`, `create_column`, `create_index`, `delete_table`,
`delete_index` — all heap-mutate + WAL-log via `TxnManager::record_insert/delete`.
WAL table_ids for system tables: `u32::MAX-2`, `u32::MAX-1`, `u32::MAX`.

`CatalogReader`: `get_table`, `get_table_by_id`, `list_tables`, `list_columns`
(sorted by col_idx), `list_indexes` — all with `TransactionSnapshot` MVCC filter.

#### 3.13 — Catalog Change Notifier
`SchemaChangeKind` (4 variants), `SchemaChangeEvent { kind, txn_id }`,
`SchemaChangeListener` trait (`Send + Sync`, idempotent + spurious-tolerant contract),
`CatalogChangeNotifier` (RwLock-based subscriber list). Events fire on DDL
execution (before commit — conservative invalidation). `CatalogWriter::with_notifier`
builder method; backward-compatible (optional).

#### 3.14 — Schema Binding
`ResolvedTable { def, columns (sorted), indexes }`. `SchemaResolver`:
`resolve_table(schema, name)`, `resolve_column(table_id, col_name)`,
`table_exists(schema, name)`. Default schema for unqualified names. Qualified
error messages (`"public.users"` in TableNotFound). MVCC-correct via snapshot.

### Storage improvements (3.15 – 3.16)

#### 3.15 — Page Dirty Tracker
`PageDirtyTracker` (HashSet<u64>) embedded in `MmapStorage`. Marks pages dirty
on `write_page` and `alloc_page`. Cleared on `flush()` after msync. Exposes
`dirty_page_count()` for monitoring.

#### 3.16 — Basic Configuration
`DbConfig` (serde::Deserialize): `data_dir`, `max_wal_size_mb` (256),
`fsync` (true), `log_level` ("info"). `DbConfig::load(Option<&Path>)`: returns
defaults on None or missing file; `ParseError` on invalid TOML; partial TOML
fills missing fields from defaults.

---

## Files created / key crates

```
crates/nexusdb-wal/src/
  entry.rs       — WalEntry binary format, CRC32c
  writer.rs      — WalWriter append-only
  reader.rs      — WalReader scan_forward + scan_backward
  txn.rs         — TxnManager BEGIN/COMMIT/ROLLBACK + undo log
  checkpoint.rs  — Checkpointer 5-step protocol
  rotation.rs    — WalRotator with start_lsn header v2
  recovery.rs    — CrashRecovery state machine
crates/nexusdb-wal/tests/
  integration_wal_entry.rs, integration_wal_reader.rs
  integration_wal_writer.rs, integration_durability.rs

crates/nexusdb-storage/src/
  heap.rs        — RowHeader, SlotEntry, insert/delete/scan_visible
  heap_chain.rs  — HeapChain multi-page linked list
  meta.rs        — meta page r/w + sequences (alloc_table_id/index_id)
  dirty.rs       — PageDirtyTracker
  config.rs      — DbConfig (dbyo.toml)
  integrity.rs   — IntegrityChecker + post-recovery checks
crates/nexusdb-storage/tests/
  integration_storage.rs

crates/nexusdb-catalog/src/
  bootstrap.rs   — CatalogBootstrap + catalog header in meta page
  schema.rs      — TableDef, ColumnDef, IndexDef, ColumnType
  reader.rs      — CatalogReader with MVCC snapshots
  writer.rs      — CatalogWriter with WAL + notifier
  notifier.rs    — CatalogChangeNotifier + SchemaChangeListener
  resolver.rs    — SchemaResolver (schema binding)
crates/nexusdb-catalog/tests/
  integration_catalog_rw.rs     — 18 tests
  integration_catalog_notifier.rs — 10 tests
  integration_schema_binding.rs   — 13 tests
```

---

## Technical decisions

| Decision | Choice | Reason |
|---|---|---|
| Physical location in WAL | page_id:u8 + slot_id:u16 prepended to new/old value | Crash recovery without in-memory state |
| MVCC visibility | `txn_id_created < snap_id AND txn_id_deleted == 0 OR >= snap_id` | Standard MVCC rule; correct for snapshot isolation |
| Heap chain pointer | `PageHeader._reserved[0..8]` = next_page_id | No slot waste; reserved field exists for this |
| ID sequences | Meta page body[64..72] as u32 LE counters | Same page as catalog header; atomic read-modify-write |
| Notifier firing time | On DDL execution (before commit) | Conservative: spurious invalidation is safe, non-invalidation is not |
| CatalogWriter WAL table_ids | `u32::MAX - {0,1,2}` | Unreachable from user sequence (starts at 1) |
| Case sensitivity | Case-sensitive in Phase 3 | Deferred to Phase 5 (session charset/collation) |
| Config parsing | serde + toml crate | Workspace already has serde; toml is minimal dep |

---

## Quality metrics

- **Tests:** 345 pass (0 fail)
  - nexusdb-wal: 103 unit + 8+13+18+11 integration = 153
  - nexusdb-storage: 76 unit + 28 integration = 104
  - nexusdb-catalog: 27+10+13 unit + 18+10+13 integration = 91
- **Clippy:** 0 errors (`-D warnings`)
- **Fmt:** clean
- **Benchmarks:** compile and run; page checksum 18 GiB/s (verify), 9.5 GiB/s (update)
- **unwrap() in src/:** 0
- **unsafe without SAFETY:** 0

---

## Deferred items

| Item | Deferred to |
|---|---|
| Autocommit (`SET autocommit=0`), implicit txn start | Phase 5 (session state) |
| ENOSPC handling | Phase 5 |
| Partial page write detection on open | Phase 5 |
| Per-page msync (flush_range) | After profiling |
| Index-backed catalog lookups | Phase 4.x (bootstrap cycle) |
| ColumnAdded/ColumnDropped events | Phase 4.22 (ALTER TABLE) |
| Post-commit notifications | Phase 5.14 (plan cache) |
| Case-insensitive identifier resolution | Phase 5 (session charset) |

---

## Next phase

**Phase 4 — SQL Parser + Executor**

Phase 3 provides the full foundation Phase 4 needs:
- `SchemaResolver` for name resolution in the executor
- `CatalogWriter/Reader` for DDL (CREATE TABLE, DROP TABLE) execution
- `TxnManager` for transaction management per query
- `HeapChain` for data page scanning (table full-scan)
- `DbConfig` for engine configuration at startup
