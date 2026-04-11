# AxiomDB — Design Summary (session orientation)

> For detailed specs, grep `db.md`. This file is the fast-orientation read.

## Core Decisions

| Decision | Choice | Reason |
|---|---|---|
| Language | **Rust** | No GC, memory control, maximum speed |
| Wire protocol | **MySQL** | PHP/Python connect without custom drivers |
| Embedded | **C FFI + cdylib** | In-process like SQLite, zero latency |
| Storage | **mmap + 8 KB pages** | No double-buffering |
| Index | **Copy-on-Write B+ Tree** | Lock-free readers |
| Durability | **WAL append-only** | No double-write buffer |
| Concurrency | **Tokio async** | Thousands of connections |
| Queries | **Vectorized + SIMD** | 10-50× on scans |

## Architecture (layers)

```
MySQL wire  →  axiomdb-network
               ↓
SQL Parser + Planner + Executor  →  axiomdb-sql
               ↓
Catalog (schema, stats)  →  axiomdb-catalog
               ↓
MVCC + Transactions  →  axiomdb-mvcc
               ↓
B+ Tree CoW  →  axiomdb-index
               ↓
WAL + Storage (mmap, free list, TOAST)  →  axiomdb-wal + axiomdb-storage
               ↓
Value types + row codec  →  axiomdb-types
               ↓
Errors + core traits  →  axiomdb-core
```

## Crates (active/implemented)

| Crate | Purpose |
|---|---|
| `axiomdb-core` | DbError, traits — zero deps |
| `axiomdb-types` | Value enum, DataType, codec, JSONB layout |
| `axiomdb-storage` | mmap pages, free list, TOAST, overflow chains |
| `axiomdb-wal` | WAL writer/reader, crash recovery, TxnManager |
| `axiomdb-index` | B+ Tree CoW, FTS inverted index |
| `axiomdb-mvcc` | Snapshot isolation, savepoints, VACUUM |
| `axiomdb-catalog` | Schema, ColumnType, information_schema |
| `axiomdb-sql` | Parser (logos/nom), AST, executor, functions |
| `axiomdb-network` | MySQL wire protocol, prepared statements |
| `axiomdb-embedded` | cdylib entry point, C FFI |
| `axiomdb-server` | TCP daemon binary |

## Key Design Invariants

- **Row codec**: `encode_row` / `decode_row` in `axiomdb-types/src/codec.rs`. Discriminants are stable on disk.
- **TOAST**: blobs >2KB spill to overflow pages via `clustered_overflow.rs`. Sentinel `__toast__:page_id:compressed:raw_len`.
- **Clustered tables**: `CREATE TABLE ... PRIMARY KEY` → clustered tree (Phase 39). Heap tables remain for tables without explicit PK.
- **MVCC**: `TxnManager` in `axiomdb-wal`. Snapshot isolation. `txn_id_created` / `txn_id_deleted` in clustered row headers.
- **ColumnType vs DataType**: `ColumnType` is the compact `repr(u8)` stored on disk. `DataType` is the in-memory executor type. Always keep them in sync when adding types.

## Phase Status (as of 2026-04-10)

**Completed:** Phases 1–10, 22b, 39, 40 (all closed)

**Active: Phase 11 — Advanced Types**

| Subphase | Status |
|---|---|
| 11.2d BLOB refcount | ✅ |
| 11.4 Native JSON | ✅ |
| 11.7 Advanced FTS | ✅ |
| 11.8 Buffer Pool | ✅ |
| 11.9+11.10 Prefetch + Write Combining | ✅ |
| **11.16 Binary JSONB + JSONPath** | 🔄 in progress |
| 11.17 GIN index for JSONB | ⬜ next |

**Current task:** 11.16 JSONB — spec + plan written, implementation underway.
- `crates/axiomdb-types/src/jsonb.rs` — binary layout, encoder, decoder
- `crates/axiomdb-sql/tests/integration_jsonb.rs` — all 20 tests passing
- `ColumnType::Jsonb = 10`, `DataType::Jsonb`, `Value::Jsonb(Arc<Vec<u8>>)`

## For deeper detail

- Architecture decisions: `db.md` (grep the section you need)
- Phase specs: `specs/fase-11/`
- Progress: `docs/progreso.md`
- Lessons / invariants: `memory/architecture.md`, `memory/lessons.md`
