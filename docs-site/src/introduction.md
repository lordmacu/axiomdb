# AxiomDB

**AxiomDB** is a database engine written in Rust, designed to be fast, correct, and modern — while remaining compatible with the MySQL wire protocol so existing applications can connect without driver changes.

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

AxiomDB is under active development. Phases 1–6 are substantially complete:

- ✅ **Storage engine** — mmap-based 16 KB pages, freelist, heap pages, CRC32c checksums
- ✅ **B+ Tree** — Copy-on-Write, lock-free readers, prefix compression, range scan
- ✅ **WAL** — append-only, crash recovery, Group Commit, PageWrite bulk optimization
- ✅ **Catalog** — schema management, DDL change notifications, MVCC-consistent reads
- ✅ **SQL layer** — full DDL + DML parser, expression evaluator, semantic analyzer
- ✅ **Executor** — SELECT/INSERT/UPDATE/DELETE, JOIN, GROUP BY + aggregates, ORDER BY,
  subqueries, CASE WHEN, DISTINCT, TRUNCATE, ALTER TABLE
- ✅ **Secondary indexes** — CREATE INDEX, UNIQUE, query planner (index lookup + range)
- ✅ **MySQL wire protocol** — port 3306, COM_QUERY, prepared statements, pymysql compatible

**Active development:** Phase 7 (full MVCC + concurrent writers) · Phase 5 remaining (plan cache) · Phase 6 remaining (FK, bloom filter)

### Performance highlights

| Operation | AxiomDB | vs competition |
|---|---|---|
| Bulk INSERT (multi-row, 10K rows) | **211K rows/s** | 1.5× faster than MariaDB 12.1 |
| Full-table DELETE (10K rows) | **1M rows/s** | 3× faster than MariaDB, 40× than MySQL 8.0 |
| Full scan SELECT (10K rows) | **212K rows/s** | ≈ MySQL 8.0 |
| Simple SELECT parse | **492 ns** | parity with MySQL |
| Range scan 10K rows | **0.61 ms** | 13× faster than MySQL (45 ms target) |

## What Makes AxiomDB Different

### 1. No double-write buffer

MySQL InnoDB uses a double-write buffer to protect against partial page writes, adding significant write overhead. AxiomDB uses a **WAL-first architecture** — pages are protected by the write-ahead log, eliminating this overhead entirely.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
MySQL InnoDB performs <strong>2× the disk writes</strong> for every page flush — once to the double-write buffer, once to the data file. AxiomDB eliminates this overhead by using the WAL as the crash-safety mechanism, with per-page CRC32c checksums to detect and recover from partial writes.
</div>
</div>

### 2. Lock-free reads

The B+ Tree uses **Copy-on-Write semantics** with an atomic root pointer. Readers never block writers and writers never block readers — there are no read locks at the storage layer.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Concurrency Advantage</span>
MySQL InnoDB requires shared read locks for consistent B+ Tree traversal. AxiomDB readers load an atomic root pointer and traverse without acquiring any lock — read throughput scales linearly with CPU cores even under concurrent writes.
</div>
</div>

### 3. Smart collation out of the box

Most databases require explicit `COLLATE` declarations for correct Unicode sorting. AxiomDB defaults to **UCA root collation** (language-neutral Unicode ordering) and can be configured to behave like MySQL or PostgreSQL for migrations.

### 4. Strict mode always on

AxiomDB rejects data truncation, invalid dates (`0000-00-00`), and silent type coercions that MySQL allows by default. With `SET AXIOM_COMPAT = 'mysql'`, lenient behavior is restored for migration scenarios.

### 5. Structured error messages

Inspired by the Rust compiler, every error includes: what went wrong, which table/column was involved, the offending value, and a hint for how to fix it.

## Parser Performance

AxiomDB's SQL parser is **9–17× faster** than sqlparser-rs (the production standard used by Apache Arrow DataFusion and Delta Lake):

| Query type | AxiomDB | sqlparser-rs | Speedup |
|---|---|---|---|
| Simple SELECT | 492 ns | 4.38 µs | **8.9×** |
| Complex SELECT (multi-JOIN) | 2.74 µs | 27.0 µs | **9.8×** |
| CREATE TABLE | 824 ns | 14.5 µs | **16.6×** |

This is achieved through a zero-copy lexer (identifiers are `&str` slices into the input — no heap allocations) combined with a hand-written recursive descent parser.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Parser Benchmark Context</span>
<code>sqlparser-rs</code> is used by Apache Arrow DataFusion, Delta Lake, and InfluxDB — widely considered the production standard for Rust SQL parsing. The 9–17× speedup is measured single-threaded, parse-only. At 2M simple queries/s, parsing is never the bottleneck for any realistic OLTP workload.
</div>
</div>
