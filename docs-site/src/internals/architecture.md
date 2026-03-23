# Architecture Overview

AxiomDB is organized as a Cargo workspace of purpose-built crates. Each crate has a
single responsibility and depends only on crates below it in the stack. The layering
prevents circular dependencies and makes each component independently testable.

---

## Layer Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│                          ENTRY POINTS                               │
│                                                                     │
│  axiomdb-server        axiomdb-embedded                             │
│  (TCP daemon,          (Rust API + C FFI,                           │
│   MySQL wire protocol)  in-process library)                         │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────────┐
│                       QUERY PIPELINE                                │
│                                                                     │
│  axiomdb-sql                                                        │
│  ├── lexer     (logos DFA, zero-copy tokens)                        │
│  ├── parser    (recursive descent, LL(1)/LL(2))                     │
│  ├── ast       (Stmt, Expr, SelectStmt, InsertStmt, ...)            │
│  ├── analyzer  (BindContext, col_idx resolution, catalog lookup)    │
│  ├── eval      (expression evaluator, three-valued NULL logic)      │
│  └── result    (QueryResult, ColumnMeta, Row — executor return type)│
│                                                                     │
│  [executor, query planner — Phase 5]                                │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────────┐
│                    TRANSACTION LAYER                                │
│                                                                     │
│  axiomdb-mvcc          (TxnManager, snapshot isolation, SSI)        │
│  axiomdb-wal           (WalWriter, WalReader, crash recovery)       │
│  axiomdb-catalog       (CatalogBootstrap, CatalogReader, schema)    │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────────┐
│                     INDEX LAYER                                     │
│                                                                     │
│  axiomdb-index         (BTree CoW, RangeIter, prefix compression)   │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────────┐
│                    STORAGE LAYER                                    │
│                                                                     │
│  axiomdb-storage       (StorageEngine trait, MmapStorage,           │
│                         MemoryStorage, FreeList, heap pages)        │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────────┐
│                     TYPE FOUNDATION                                 │
│                                                                     │
│  axiomdb-types         (Value, DataType, row codec)                 │
│  axiomdb-core          (DbError, RecordId, TransactionSnapshot,     │
│                         PageId, LsnId, common types)               │
└─────────────────────────────────────────────────────────────────────┘
                               │
                    ┌──────────▼────────┐
                    │   axiomdb.db      │  ← mmap pages (16 KB each)
                    │   axiomdb.wal     │  ← WAL append-only log
                    └───────────────────┘
```

---

## Crate Responsibilities

### axiomdb-core

The dependency-free foundation. Contains:

- `DbError` — the single error enum used by all other crates, using `thiserror`
- `RecordId` — physical location of a row: `(page_id: u64, slot_id: u16)`, 10 bytes
- `TransactionSnapshot` — snapshot ID and visibility predicate for MVCC
- `PageId`, `LsnId` — type aliases that document intent

No crate in the workspace depends on a crate above `axiomdb-core`.

### axiomdb-types

SQL value representation and binary serialization:

- `Value` — the in-memory enum (`Null`, `Bool`, `Int`, `BigInt`, `Real`, `Decimal`,
  `Text`, `Bytes`, `Date`, `Timestamp`, `Uuid`)
- `DataType` — schema descriptor for a column's type (mirrors `axiomdb-core::DataType`
  but with full type system including parameterized types)
- `encode_row` / `decode_row` — binary codec from `&[Value]` to `&[u8]` and back
- `encoded_len` — O(n) size computation without allocation

### axiomdb-storage

The raw page I/O layer:

- `StorageEngine` trait — `read_page`, `write_page`, `alloc_page`, `free_page`, `flush`
- `MmapStorage` — maps the `.db` file with `memmap2`; pages are directly accessible as
  `&Page` references into the mapped region
- `MemoryStorage` — `Vec<Page>` in RAM for tests and in-memory databases
- `FreeList` — bitmap tracking free pages; scans left-to-right for the first free bit
- `Page` — 16 KB struct with 64-byte header (magic, type, checksum, page_id, LSN,
  free_start, free_end) and 16,320-byte body
- Heap page format — slotted page with null bitmap and tuples growing from the end
  toward the beginning

### axiomdb-index

The Copy-on-Write B+ Tree:

- `BTree` — the public tree type; wraps a `StorageEngine` and an `AtomicU64` root
- `RangeIter` — lazy iterator for range scans; traverses the tree to cross leaf boundaries
- `InternalNodePage` / `LeafNodePage` — `#[repr(C)]` structs with `bytemuck::Pod`
  for zero-copy serialization
- `prefix` module — `CompressedNode` for in-memory prefix compression of internal keys

### axiomdb-wal

Append-only Write-Ahead Log:

- `WalWriter` — appends `WalEntry` records with CRC32c checksums; manages file header
- `WalReader` — stateless; opens a file handle per scan; supports both forward and
  backward iteration (backward scan uses `entry_len_2` at the tail of each record)
- `WalEntry` — binary-serializable record with LSN, txn_id, entry type, table_id,
  key, old_value, new_value, and checksum
- Crash recovery state machine — `CRASHED → RECOVERING → REPLAYING_WAL → VERIFYING → READY`

### axiomdb-catalog

Schema persistence and lookup:

- `CatalogBootstrap` — creates the three system tables (`nexus_tables`, `nexus_columns`,
  `nexus_indexes`) in the meta page on first open
- `CatalogReader` — reads schema from the system tables for use by the analyzer
  and executor; uses a `TransactionSnapshot` for MVCC-consistent reads
- Schema types: `TableDef`, `ColumnDef`, `IndexDef`

### axiomdb-mvcc

Transaction management and snapshot isolation:

- `TxnManager` — assigns transaction IDs, tracks active transactions, assigns
  snapshots on `BEGIN`
- `RowHeader` — embedded in each heap row: `(xmin, xmax, deleted)` for visibility
- MVCC visibility function — determines whether a row version is visible to a snapshot

### axiomdb-sql

The SQL processing pipeline:

- `lexer` — logos-based DFA; ~85 tokens; zero-copy `&'src str` identifiers
- `ast` — all statement types: `SelectStmt`, `InsertStmt`, `UpdateStmt`, `DeleteStmt`,
  `CreateTableStmt`, `CreateIndexStmt`, `DropTableStmt`, `DropIndexStmt`, `AlterTableStmt`
- `expr` — `Expr` enum for the expression tree: `BinaryOp`, `UnaryOp`, `Column`,
  `Literal`, `IsNull`, `Between`, `Like`, `In`, `Case`, `Function`
- `parser` — recursive descent; expression sub-parser with full operator precedence
- `analyzer` — `BindContext` / `BoundTable`; resolves `col_idx` for JOINs
- `eval` — expression evaluator with three-valued NULL logic
- `result` — `QueryResult` enum (`Rows` / `Affected` / `Empty`), `ColumnMeta`
  (name, data_type, nullable, table_name), `Row = Vec<Value>`; the contract
  between the executor and all callers (embedded API, wire protocol, CLI)

### axiomdb-server

Entry point for server mode. Sets up a Tokio TCP listener, performs the MySQL handshake,
parses the MySQL wire protocol, dispatches SQL statements to the engine, and serializes
results as MySQL result-set packets.

### axiomdb-embedded

Entry point for embedded mode. Exposes:

- A safe Rust API (`Database::open`, `Database::execute`, `Database::transaction`)
- A C FFI (`axiomdb_open`, `axiomdb_execute`, `axiomdb_close`, `axiomdb_free_string`)

---

## Query Lifecycle — From Wire to Storage

```
1. TCP packet received (server mode) or Rust call (embedded mode)
   │
2. SQL string extracted from MySQL COM_QUERY packet
   │
3. axiomdb-sql::tokenize(sql)
   → Vec<SpannedToken>  (logos DFA, zero-copy)
   │
4. axiomdb-sql::parse(tokens)
   → Stmt  (recursive descent; all col_idx = placeholder 0)
   │
5. axiomdb-sql::analyze(stmt, storage, snapshot)
   → Stmt  (col_idx resolved against catalog; names validated)
   │
6. Executor (Phase 5) interprets the analyzed Stmt
   → reads from axiomdb-index (BTree lookups / range scans)
   → calls axiomdb-types::decode_row on heap page bytes
   → builds Vec<Vec<Value>> result rows
   │
7. WAL write (for INSERT / UPDATE / DELETE)
   → axiomdb-wal::WalWriter::append(WalEntry)
   │
8. Heap page write (for INSERT / UPDATE / DELETE)
   → axiomdb-storage::StorageEngine::write_page
   │
9. Result serialized
   → MySQL result-set packets (server mode)
   → QueryResult struct (embedded mode)
```

---

## Key Architectural Decisions

### mmap over a custom buffer pool

AxiomDB maps the `.db` file with `mmap`. The OS page cache manages eviction (LRU) and
readahead automatically. InnoDB maintains a separate buffer pool on top of the OS page
cache, causing the same data to live in RAM twice. mmap eliminates the second copy.

Trade-off: we give up fine-grained control over eviction policy. The OS uses LRU, which
is good for most database workloads. Custom eviction (e.g., clock-sweep with hot/cold
separation) will be optional in a future phase.

### Copy-on-Write B+ Tree

CoW means a write operation never modifies an existing page in place. Instead, it
creates new pages for every node on the path from root to the modified leaf, then
atomically swaps the root pointer. Readers who loaded the old root before the swap
continue accessing a fully consistent old version with no locking.

Trade-off: writes amplify — modifying one leaf requires copying O(log n) pages. For
a tree of depth 4 (enough for hundreds of millions of rows), this is 4 page copies per
write. At 16 KB per page, that is 64 KB of write amplification per key insert.

### WAL without double-write

The WAL records logical changes (key, old_value, new_value) rather than full page
images. Each WAL record has a CRC32c checksum. On recovery, AxiomDB reads the WAL
forward, identifies committed transactions, and replays their mutations. Pages with
incorrect checksums are rebuilt from WAL records.

This eliminates MySQL's doublewrite buffer (which writes each page twice to protect
against torn writes) at the cost of a slightly more complex recovery algorithm.

### logos for lexing, not nom

logos generates a compiled DFA from the token patterns at build time. The generated
lexer runs in O(n) time with a fixed, small constant (typically 1–3 CPU instructions
per byte). nom builds parser combinators at runtime with dynamic dispatch overhead.
For a lexer processing millions of SQL statements per second, the constant factor
matters: logos achieves 9–17× throughput over sqlparser-rs's nom-based lexer.
