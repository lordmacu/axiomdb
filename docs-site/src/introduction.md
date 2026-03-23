# NexusDB

**NexusDB** is a database engine written in Rust, designed to be fast, correct, and modern — while remaining compatible with the MySQL wire protocol so existing applications can connect without driver changes.

## Goals

| Goal | How |
|---|---|
| **Faster than MySQL** for read-heavy workloads | Copy-on-Write B+ Tree with lock-free readers |
| **Crash-safe** without the MySQL double-write buffer overhead | Append-only WAL, no double-write |
| **Drop-in compatible** with MySQL clients | MySQL wire protocol on port 3306 |
| **Embeddable** like SQLite | C FFI, no daemon required (Phase 10) |
| **Modern SQL** out of the box | Unicode-correct collation, strict mode, structured errors |

## Two Usage Modes

```
┌─────────────────────┐         ┌──────────────────────────┐
│   SERVER MODE       │         │   EMBEDDED MODE          │
│                     │         │                          │
│  TCP :3306          │         │  Direct function call    │
│  MySQL wire proto   │         │  C FFI / Rust API        │
│  PHP, Python, Node  │         │  No network, no daemon   │
└─────────────────────┘         └──────────────────────────┘
          └─────────────────┬─────────────────┘
                            │
                    Same Rust engine
```

## Current Status

NexusDB is under active development. The following layers are complete and production-quality:

- ✅ **Storage engine** — mmap-based 16KB pages, freelist, heap pages
- ✅ **B+ Tree** — Copy-on-Write, lock-free reads, prefix compression
- ✅ **WAL** — append-only, crash recovery, MVCC
- ✅ **Catalog** — schema management, DDL change notifications
- ✅ **SQL layer** — lexer, parser (DDL + DML), expression evaluator, semantic analyzer

The executor (which runs queries) is in active development in Phase 4.

## What Makes NexusDB Different

### 1. No double-write buffer

MySQL InnoDB uses a double-write buffer to protect against partial page writes, adding significant write overhead. NexusDB uses a **WAL-first architecture** — pages are protected by the write-ahead log, eliminating this overhead entirely.

### 2. Lock-free reads

The B+ Tree uses **Copy-on-Write semantics** with an atomic root pointer. Readers never block writers and writers never block readers — there are no read locks at the storage layer.

### 3. Smart collation out of the box

Most databases require explicit `COLLATE` declarations for correct Unicode sorting. NexusDB defaults to **UCA root collation** (language-neutral Unicode ordering) and can be configured to behave like MySQL or PostgreSQL for migrations.

### 4. Strict mode always on

NexusDB rejects data truncation, invalid dates (`0000-00-00`), and silent type coercions that MySQL allows by default. With `SET NEXUS_COMPAT = 'mysql'`, lenient behavior is restored for migration scenarios.

### 5. Structured error messages

Inspired by the Rust compiler, every error includes: what went wrong, which table/column was involved, the offending value, and a hint for how to fix it.

## Parser Performance

NexusDB's SQL parser is **9–17× faster** than sqlparser-rs (the production standard used by Apache Arrow DataFusion and Delta Lake):

| Query type | NexusDB | sqlparser-rs | Speedup |
|---|---|---|---|
| Simple SELECT | 492 ns | 4.38 µs | **8.9×** |
| Complex SELECT (multi-JOIN) | 2.74 µs | 27.0 µs | **9.8×** |
| CREATE TABLE | 824 ns | 14.5 µs | **16.6×** |

This is achieved through a zero-copy lexer (identifiers are `&str` slices into the input — no heap allocations) combined with a hand-written recursive descent parser.
