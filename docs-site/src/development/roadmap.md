# Roadmap and Phases

AxiomDB is developed in phases, each of which adds a coherent vertical slice of
functionality. The design is organized in three blocks:

- **Block 1 (Phases 1–7):** Core engine — storage, indexing, WAL, transactions,
  SQL parsing, and concurrent MVCC.
- **Block 2 (Phases 8–14):** SQL completeness — full query planner, optimizer,
  advanced SQL features, and MySQL wire protocol.
- **Block 3 (Phases 15–34):** Production hardening — replication, backups, distributed
  execution, column store, and AI/ML integration.

---

## Current Status

**Last completed:** Phase 4 (partial) — SQL Parser + Semantic Analyzer

**Active development:** Phase 5 — Executor (planned)

---

## Phase Progress

### Block 1 — Core Engine

| Phase | Name | Status | Key deliverables |
|-------|------|--------|-----------------|
| 1.1 | Workspace setup | ✅ | Cargo workspace, crate structure |
| 1.2 | Page format | ✅ | 16 KB pages, header, CRC32c checksum |
| 1.3 | MmapStorage | ✅ | mmap-backed storage engine |
| 1.4 | MemoryStorage | ✅ | In-memory storage for tests |
| 1.5 | FreeList | ✅ | Bitmap page allocator |
| 1.6 | StorageEngine trait | ✅ | Unified interface + heap pages |
| 2.1 | B+ Tree insert/split | ✅ | CoW insert with recursive splits |
| 2.2 | B+ Tree delete | ✅ | Rebalance, redistribute, merge |
| 2.3 | B+ Tree range scan | ✅ | RangeIter with tree traversal |
| 2.4 | Prefix compression | ✅ | CompressedNode for internal keys |
| 3.1 | WAL entry format | ✅ | Binary format, CRC32c, backward scan |
| 3.2 | WAL writer | ✅ | WalWriter with file header |
| 3.3 | WAL reader | ✅ | Forward and backward iterators |
| 3.4 | TxnManager | ✅ | BEGIN/COMMIT/ROLLBACK, snapshot |
| 3.5 | Checkpoint | ✅ | 5-step checkpoint protocol |
| 3.6 | Crash recovery | ✅ | CRASHED→RECOVERING→REPLAYING→VERIFYING→READY |
| 3.7 | Durability tests | ✅ | 9 crash scenarios |
| 3.8 | Post-recovery checker | ✅ | Heap structural + MVCC invariants |
| 3.9 | Catalog bootstrap | ✅ | axiom_tables, axiom_columns, axiom_indexes |
| 3.10 | Catalog reader | ✅ | MVCC-aware schema lookup |
| 4.1 | SQL AST | ✅ | All statement types |
| 4.2 | SQL lexer | ✅ | logos DFA, ~85 tokens, zero-copy |
| 4.3 | DDL parser | ✅ | CREATE/DROP/ALTER TABLE, CREATE/DROP INDEX |
| 4.4 | DML parser | ✅ | SELECT (all clauses), INSERT, UPDATE, DELETE |
| 4.17 | Expression evaluator | ✅ | Three-valued NULL logic, all operators |
| 4.18 | Semantic analyzer | ✅ | BindContext, col_idx resolution |
| 4.18b | Type coercion matrix | ✅ | coerce(), coerce_for_op(), CoercionMode strict/permissive |
| 4.23 | QueryResult type | ✅ | Row, ColumnMeta, QueryResult (Rows/Affected/Empty) |
| 4.5b | Table engine | ✅ | TableEngine scan/insert/delete/update; TableDef.data_root_page_id |
| 4.5 + 4.5a | Basic executor | ✅ | SELECT/INSERT/UPDATE/DELETE, DDL, txn control, SELECT without FROM |
| 4.25 + 4.7 | Error handling framework | ✅ | Complete SQLSTATE mapping; ErrorResponse{sqlstate,message,detail,hint} |
| 4.8 | JOIN (nested loop) | ✅ | INNER/LEFT/RIGHT/CROSS; USING; multi-table; FULL→NotImplemented |
| 4.9a+4.9c+4.9d | GROUP BY + Aggregates + HAVING | ✅ | COUNT/SUM/MIN/MAX/AVG; hash-based; HAVING; NULL grouping |
| 4.10+4.10b+4.10c | ORDER BY + LIMIT/OFFSET | ✅ | Multi-column; NULLS FIRST/LAST; LIMIT/OFFSET pagination |
| 5 | Executor (advanced) | ⚠️ Planned | JOIN, GROUP BY, ORDER BY, index lookup, aggregate |
| 6 | Query planner | ⚠️ Planned | Cost-based plan selection |
| 7 | Full MVCC | ⚠️ Planned | SSI, write-write conflicts, epoch reclamation |

### Block 2 — SQL Completeness

| Phase | Name | Status | Key deliverables |
|-------|------|--------|-----------------|
| 8 | Advanced SQL | ⚠️ Planned | Window functions, CTEs, recursive queries |
| 9 | VACUUM / GC | ⚠️ Planned | Dead row cleanup, freelist compaction |
| 10 | MySQL wire protocol | ⚠️ Planned | COM_QUERY, result set packets, handshake |
| 11 | TOAST | ⚠️ Planned | Out-of-line storage for large values |
| 12 | Full-text search | ⚠️ Planned | Inverted index, BM25 ranking |
| 13 | Foreign key checks | ⚠️ Planned | Constraint validation on insert/delete |
| 14 | Vectorized execution | ⚠️ Planned | SIMD scans, morsel-driven pipeline |

### Block 3 — Production Hardening

| Phase | Name | Status |
|-------|------|--------|
| 15 | Connection pooling | ⚠️ Planned |
| 16 | Replication (primary-replica) | ⚠️ Planned |
| 17 | Point-in-time recovery (PITR) | ⚠️ Planned |
| 18 | Online DDL | ⚠️ Planned |
| 19 | Partitioning | ⚠️ Planned |
| 20 | Column store (HTAP) | ⚠️ Planned |
| 21 | VECTOR index (ANN) | ⚠️ Planned |
| 22–34 | Distributed, cloud-native, AI/ML | ⚠️ Future |

---

## Completed Phases — Summary

### Phase 1 — Storage Engine

A generic storage layer with two implementations: `MmapStorage` for production disk
use and `MemoryStorage` for tests. Every higher-level component uses only the
`StorageEngine` trait — storage is pluggable. Pages are 16 KB with a 64-byte header
(magic, page type, CRC32c checksum, page_id, LSN, free pointers). Heap pages use a
slotted format: slots grow from the start, tuples grow from the end toward the center.

### Phase 2 — B+ Tree CoW

A persistent, Copy-on-Write B+ Tree over `StorageEngine`. Keys up to 64 bytes;
ORDER_INTERNAL = 223, ORDER_LEAF = 217 (derived to fill exactly one 16 KB page).
Root is an `AtomicU64` — readers are lock-free by design. Supports insert (with
recursive split), delete (with rebalance/redistribute/merge), and range scan via
`RangeIter`. Prefix compression for internal nodes in memory.

### Phase 3 — WAL and Transactions

Append-only Write-Ahead Log with binary entries, CRC32c checksums, and forward/backward
scan iterators. `TxnManager` coordinates BEGIN/COMMIT/ROLLBACK with snapshot assignment.
Five-step checkpoint protocol. Crash recovery state machine (five states). Catalog
bootstrap creates the three system tables on first open. `CatalogReader` provides
MVCC-consistent schema reads. Nine crash scenario tests with a post-recovery integrity
checker.

### Phase 4 — SQL Processing

SQL AST covering all DML (SELECT, INSERT, UPDATE, DELETE) and DDL (CREATE/DROP/ALTER
TABLE, CREATE/DROP INDEX). logos-based lexer with ~85 tokens, case-insensitive keywords,
zero-copy identifiers. Recursive descent parser with full expression precedence. Expression
evaluator with three-valued NULL logic (AND, OR, NOT, IS NULL, BETWEEN, LIKE, IN).
Semantic analyzer with `BindContext`, qualified/unqualified column resolution, ambiguity
detection, and subquery support. Row codec with null bitmap, u24 string lengths, and
O(n) `encoded_len()`.

---

## Near-Term Priorities (Phase 5)

Phase 5 will implement the executor — the component that interprets the analyzed AST
and produces result rows. Planned sub-phases:

1. **Table scan** — linear scan of heap pages with MVCC visibility filtering.
2. **Index lookup** — point lookup via B+ Tree given a primary key value.
3. **Index range scan** — range predicate via `RangeIter`.
4. **Projection** — evaluate SELECT expressions over rows from the scan.
5. **Filter** — apply WHERE expression using the evaluator from Phase 4.17.
6. **Nested loop join** — INNER JOIN, LEFT JOIN.
7. **Sort** — ORDER BY with NULLS FIRST/LAST.
8. **Limit/Offset** — LIMIT n OFFSET m.
9. **Hash aggregate** — GROUP BY with COUNT, SUM, AVG, MIN, MAX.
10. **INSERT / UPDATE / DELETE** — write path with WAL integration.

The executor will be a simple volcano-model interpreter in Phase 5. Vectorized
execution (morsel-driven, SIMD) is planned for Phase 14.
