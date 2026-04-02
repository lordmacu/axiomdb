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
│                        NETWORK LAYER                                │
│                                                                     │
│  axiomdb-network                                                    │
│  └── mysql/                                                         │
│      ├── codec.rs    (MySqlCodec — 4-byte packet framing)           │
│      ├── packets.rs  (HandshakeV10, HandshakeResponse41, OK, ERR)   │
│      ├── auth.rs     (mysql_native_password SHA1 + caching_sha2_password)│
│      ├── charset.rs  (charset/collation registry, encode_text/decode_text)│
│      ├── session.rs  (ConnectionState — typed charset fields,       │
│      │               prepared stmt cache, pending long data)        │
│      ├── handler.rs  (handle_connection — async task per TCP conn)  │
│      ├── result.rs   (QueryResult → result-set packets, charset-aware)│
│      ├── error.rs    (DbError → MySQL error code + SQLSTATE)        │
│      └── database.rs (Arc<RwLock<Database>> wrapper)                │
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
│  ├── eval      (expression evaluator, three-valued NULL logic,      │
│  │              CASE WHEN searched + simple form, short-circuit)    │
│  ├── result    (QueryResult, ColumnMeta, Row — executor return type)│
│  ├── table     (TableEngine — scan/insert/delete/update over heap)  │
│  ├── index_integrity (startup index-vs-heap verifier + rebuild)     │
│  └── executor/ (mod.rs facade + select/insert/update/delete/ddl/   │
│                 join/aggregate/shared modules; same execute() API; │
│                 GROUP BY + HAVING + ORDER BY + LIMIT/OFFSET +      │
│                 INSERT … SELECT)                                   │
│                                                                     │
│  [query planner, optimizer — Phase 6]                               │
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
- `dsn` — shared DSN parser and typed normalized output:
  - `ParsedDsn`
  - `WireEndpointDsn`
  - `LocalPathDsn`
- `RecordId` — physical location of a row: `(page_id: u64, slot_id: u16)`, 10 bytes
- `TransactionSnapshot` — snapshot ID and visibility predicate for MVCC
- `PageId`, `LsnId` — type aliases that document intent

No crate in the workspace depends on a crate above `axiomdb-core`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Shared DSN Core</span>
Borrowing PostgreSQL libpq's parsing boundary and SQLite's reusable URI-normalization idea, AxiomDB parses DSNs once in <code>axiomdb-core</code> and lets each consumer validate only the subset it actually supports. This avoids duplicating URI logic in both <code>axiomdb-server</code> and <code>axiomdb-embedded</code>.
</div>
</div>

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
- Same-slot tuple rewrite helpers — used by the stable-RID UPDATE path to overwrite
  a row in place when the new encoded row still fits inside the existing slot

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
- `EntryType::UpdateInPlace` — stable-RID same-slot UPDATE record used by rollback
  and crash recovery to restore the old tuple image at the same `(page_id, slot_id)`
- Crash recovery state machine — `CRASHED → RECOVERING → REPLAYING_WAL → VERIFYING → READY`

### axiomdb-catalog

Schema persistence and lookup:

- `CatalogBootstrap` — creates the three system tables (`axiom_tables`, `axiom_columns`,
  `axiom_indexes`) in the meta page on first open
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
  `Literal`, `IsNull`, `Between`, `Like`, `In`, `Case`, `Function`,
  `Param { idx: usize }` (positional `?` placeholder resolved at execute time)
- `parser` — recursive descent; expression sub-parser with full operator precedence;
  parses `GROUP BY`, `HAVING`, `ORDER BY` with `NULLS FIRST/LAST`, `LIMIT/OFFSET`,
  `SELECT DISTINCT`, `INSERT … SELECT`, and both forms of `CASE WHEN`
- `analyzer` — `BindContext` / `BoundTable`; resolves `col_idx` for JOINs
- `eval/` — directory module rooted at `eval/mod.rs`; exports the same evaluator
  API as before, but splits internals into `context.rs` (collation and subquery
  runners), `core.rs` (recursive `Expr` evaluation), `ops.rs` (comparisons,
  boolean logic, `IN`, `LIKE`), and `functions/` (scalar built-ins by family)
- `result` — `QueryResult` enum (`Rows` / `Affected` / `Empty`), `ColumnMeta`
  (name, data_type, nullable, table_name), `Row = Vec<Value>`; the contract
  between the executor and all callers (embedded API, wire protocol, CLI)
- `index_integrity` — startup-time verification that compares every
  catalog-visible index against heap-visible rows after WAL recovery and
  rebuilds readable divergent indexes before open returns
- `executor/` — directory module rooted at `executor/mod.rs`; the facade still exports
  `execute`, `execute_with_ctx`, and `last_insert_id_value`, but the implementation is
  now split into `shared.rs`, `select.rs`, `joins.rs`, `aggregate.rs`, `insert.rs`,
  `update.rs`, `delete.rs`, `bulk_empty.rs`, `ddl.rs`, and `staging.rs`. Capabilities remain the same:
  `GROUP BY` with hash-based aggregation (`COUNT(*)`, `COUNT(col)`, `SUM`, `MIN`, `MAX`,
  `AVG` with proper NULL exclusion), `HAVING` post-filter, `ORDER BY` with multi-column
  sort keys and per-column `NULLS FIRST/LAST` control, `LIMIT n OFFSET m` for pagination,
  `SELECT DISTINCT` with NULL-equality dedup (two NULL values are considered equal for
  deduplication), and `INSERT … SELECT` for bulk copy and aggregate materialization
- Stable-RID UPDATE fast path — same-slot heap rewrite that preserves `RecordId`
  when the new encoded row fits and makes untouched-index skipping safe
- UPDATE apply fast path — indexed UPDATE now batches candidate heap reads,
  filters no-op rows before heap mutation, batches `UpdateInPlace` WAL append,
  and groups per-index delete+insert/root persistence on the remaining rows
- Transactional INSERT staging — explicit transactions can buffer consecutive
  `INSERT ... VALUES` rows in `SessionContext`, then flush them through one
  grouped heap/index pass at the next barrier statement or `COMMIT`
- Indexed multi-row INSERT batch path — the immediate `INSERT ... VALUES (...), (... )`
  path now reuses the same grouped physical apply helpers as staged flushes even
  when the table has PRIMARY KEY or secondary indexes; the immediate path keeps
  strict same-statement UNIQUE checking and therefore does not reuse the staged
  `committed_empty` shortcut

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Split Without API Drift</span>
PostgreSQL and SQLite both keep executor logic separated by statement family instead of
one source file. AxiomDB now adopts the same responsibility split, but keeps the existing
`crate::executor` facade intact so sibling modules and external callers do not pay a refactor tax.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Share Batch Apply, Not Bulk-Load Semantics</span>
PostgreSQL's <code>heap_multi_insert()</code> and DuckDB's appender both inspired the shared grouped-write layer. AxiomDB adapts that physical apply pattern, but rejects reusing the staged bulk-load shortcut on immediate multi-row INSERT because duplicate keys inside one SQL statement must still fail atomically and before any partial batch becomes visible.
</div>
</div>

### axiomdb-network

The MySQL wire protocol implementation. Lives in `crates/axiomdb-network/src/mysql/`:

| Module | Responsibility |
|---|---|
| `codec.rs` | `MySqlCodec` — `tokio_util` framing codec; reads/writes the 4-byte header (`u24 LE` payload length + `u8` sequence ID) |
| `packets.rs` | Builders for HandshakeV10, HandshakeResponse41, OK, ERR, EOF; length-encoded integer/string helpers |
| `auth.rs` | `gen_challenge` (20-byte CSPRNG), `verify_native_password` (SHA1-XOR), `is_allowed_user` allowlist |
| `charset.rs` | Static charset/collation registry; `decode_text`/`encode_text` using `encoding_rs`; supports utf8mb4, utf8mb3, latin1 (cp1252), binary |
| `session.rs` | `ConnectionState` — typed `client_charset`, `connection_collation`, `results_collation` fields; `SET NAMES`; `decode_client_text`/`encode_result_text` |
| `handler.rs` | `handle_connection` — async task per TCP connection; explicit `CONNECTED → AUTH → IDLE → EXECUTING → CLOSING` lifecycle |
| `result.rs` | `serialize_query_result` — `QueryResult` → `column_count + column_defs + EOF + rows + EOF` packets; charset-aware row encoding |
| `error.rs` | `dberror_to_mysql` — maps every `DbError` variant to a MySQL error code + SQLSTATE |
| `database.rs` | `Database` wrapper — owns storage + txn, runs WAL recovery and startup index verification, exposes `execute_query` |

#### Connection lifecycle

```
TCP accept
  │
  ▼  (seq 0)
Server → HandshakeV10
  │       20-byte random challenge, capabilities, server version
  │       auth_plugin_name = "caching_sha2_password"
  │
  ▼  (seq 1)
Client → HandshakeResponse41
  │       username, auth_response (SHA1-XOR token or caching_sha2 token),
  │       capabilities, auth_plugin_name
  │
  ▼  (seq 2)  — two paths depending on the plugin negotiated:
  │
  │  mysql_native_password path:
  │  └── Server → OK  (permissive mode: username in allowlist → accepted)
  │
  │  caching_sha2_password path (MySQL 8.0+ default):
  │  ├── Server → AuthMoreData(0x03)  ← fast_auth_success indicator
  │  ├── Client → empty ack packet    ← pymysql sends this automatically
  │  └── Server → OK
  │
  ▼  COMMAND LOOP
  │
  ├── COM_QUERY (0x03)        → parse SQL → intercept? → execute → result packets
  ├── COM_PING  (0x0e)        → OK
  ├── COM_INIT_DB (0x02)      → updates current_database in ConnectionState + OK
  ├── COM_RESET_CONNECTION (0x1f) → resets ConnectionState, preserves transport lifecycle metadata + OK
  ├── COM_STMT_PREPARE (0x16) → parse SQL with ? placeholders → stmt_ok packet
  ├── COM_STMT_SEND_LONG_DATA (0x18) → append raw bytes to stmt-local buffers, no reply
  ├── COM_STMT_EXECUTE (0x17) → merge long data + decode params → substitute → execute → result packets
  ├── COM_STMT_RESET (0x1a)   → clear stmt-local long-data state → OK
  ├── COM_STMT_CLOSE (0x19)   → remove from cache, no response
  └── COM_QUIT  (0x01)        → close
```

#### Explicit lifecycle state machine (5.11c)

`5.11c` moved transport/runtime concerns out of `ConnectionState` into
`mysql/lifecycle.rs`. `ConnectionState` still owns SQL session variables,
prepared statements, warnings, and session counters. `ConnectionLifecycle`
owns only:

- current transport phase
- client capability flags relevant to lifecycle policy
- timeout policy per phase
- socket-level configuration (`TCP_NODELAY`, `SO_KEEPALIVE`)

| Phase | Entered when | Timeout policy |
|---|---|---|
| `CONNECTED` | socket accepted, before first packet | no read yet; greeting write uses auth timeout |
| `AUTH` | handshake/auth exchange starts | fixed 10s auth timeout for reads/writes |
| `IDLE` | between commands | `interactive_timeout` if `CLIENT_INTERACTIVE`, otherwise `wait_timeout` |
| `EXECUTING` | after a command packet is accepted | packet writes use `net_write_timeout`; any future in-flight reads use `net_read_timeout` |
| `CLOSING` | `COM_QUIT`, EOF, timeout, or transport error | terminal state before handler return |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Transport State Split</span>
MariaDB and PostgreSQL both separate connection lifecycle from SQL session semantics. AxiomDB adopts the same boundary: timeout and socket policy live in `ConnectionLifecycle`, while `ConnectionState` remains purely SQL/session state.
</div>
</div>

`COM_RESET_CONNECTION` recreates `ConnectionState::new()` and resets session timeout
variables to their defaults, but it does not recreate `ConnectionLifecycle`. That
means the connection remains interactive or non-interactive according to the
original handshake, even after reset.

#### Prepared statements (prepared.rs)

Prepared statements allow a client to send SQL once and execute it many times with
different parameters, avoiding repeated parsing and enabling binary parameter encoding
that is more efficient than string escaping.

**Protocol flow:**

```
Client → COM_STMT_PREPARE  (SQL with ? placeholders)
  │
Server reads the SQL, counts ? placeholders, assigns a stmt_id.
  │
Server → Statement OK packet
  │       stmt_id: u32
  │       num_columns: u16  (columns in the result set, or 0 for DML)
  │       num_params:  u16  (number of ? placeholders)
  │       followed by num_params parameter-definition packets + EOF
  │       followed by num_columns column-definition packets + EOF
  │
Client → COM_STMT_SEND_LONG_DATA (optional, repeatable)
  │       stmt_id: u32
  │       param_id: u16
  │       raw chunk bytes
  │
Server appends raw bytes to stmt-local state, sends no response.
  │
Client → COM_STMT_EXECUTE
  │       stmt_id: u32
  │       flags: u8  (0 = CURSOR_TYPE_NO_CURSOR)
  │       iteration_count: u32  (always 1)
  │       null_bitmap: ceil(num_params / 8) bytes  (one bit per param)
  │       new_params_bound_flag: u8  (1 = type list follows)
  │       param_types: [u8; num_params * 2]  (type byte + unsigned flag)
  │       param_values: binary-encoded values for non-NULL params
  │
Server → result set packets  (same text-protocol format as COM_QUERY)
  │
Client → COM_STMT_CLOSE (stmt_id)   — no response expected
```

**Binary parameter decoding (`decode_binary_value`):**

Each parameter is decoded according to its MySQL type byte:

| MySQL type byte | Type name | Decoded as |
|---|---|---|
| `0x01` | TINY | `i8` → `Value::Int` |
| `0x02` | SHORT | `i16` → `Value::Int` |
| `0x03` | LONG | `i32` → `Value::Int` |
| `0x08` | LONGLONG | `i64` → `Value::BigInt` |
| `0x04` | FLOAT | `f32` → `Value::Real` |
| `0x05` | DOUBLE | `f64` → `Value::Real` |
| `0x0a` | DATE | 4-byte packed date → `Value::Date` |
| `0x07` / `0x0c` | TIMESTAMP / DATETIME | 7-byte packed datetime → `Value::Timestamp` |
| `0xfd` / `0xfe` / `0x0f` | VAR_STRING / STRING / VARCHAR | lenenc bytes → `Value::Text` |
| `0xf9` / `0xfa` / `0xfb` / `0xfc` | TINY_BLOB / MEDIUM_BLOB / LONG_BLOB / BLOB | lenenc bytes → `Value::Bytes` |

NULL parameters are identified by the null-bitmap before the type list is read;
they produce `Value::Null` without consuming any bytes from the value region.

**Long-data buffering (`COM_STMT_SEND_LONG_DATA`):**

`PreparedStatement` owns stmt-local pending buffers:

```rust
pub struct PreparedStatement {
    // ...
    pub pending_long_data: Vec<Option<Vec<u8>>>,
    pub pending_long_data_error: Option<String>,
}
```

Rules:

- chunks are appended as raw bytes in `handler.rs`
- `COM_STMT_SEND_LONG_DATA` never takes the `Database` mutex
- the next `COM_STMT_EXECUTE` consumes pending long data before inline values
- long data wins over both the inline execute payload and the null bitmap
- state is cleared immediately after every execute attempt
- `COM_STMT_RESET` clears only this long-data state, not the cached plan

AxiomDB follows MariaDB's `COM_STMT_SEND_LONG_DATA` model here: accumulate raw
bytes per placeholder and decode them only at execute time. That keeps chunked
multibyte text correct without dragging the command through the engine path.

**Parameter substitution — AST-level plan cache (`substitute_params_in_ast`):**

`COM_STMT_PREPARE` runs parse + analyze **once** and stores the resulting `Stmt` in
`PreparedStatement.analyzed_stmt`. On each `COM_STMT_EXECUTE`, `substitute_params_in_ast`
walks the cached AST and replaces every `Expr::Param { idx }` node with
`Expr::Literal(params[idx])` in a single O(n) tree walk (~1 µs), then calls
`execute_stmt()` directly — bypassing parse and analyze entirely.

The `?` token is recognized by the lexer as `Token::Question` and emitted by the parser
as `Expr::Param { idx: N }` (0-based position). The semantic analyzer passes `Expr::Param`
through unchanged because the type is not yet known; type resolution happens at execute
time once the binary-encoded parameter values are decoded from the `COM_STMT_EXECUTE`
packet.

`value_to_sql_literal` converts each decoded `Value` to the appropriate `Expr::Literal`
variant:
- `Value::Null` → `Expr::Literal(Value::Null)`
- `Value::Int` / `BigInt` / `Real` → numeric literal node
- `Value::Text` → text literal node (single-quote escaping preserved at the protocol
  boundary, not needed in the AST)
- `Value::Date` / `Timestamp` → date/timestamp literal node

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — AST cache vs string substitution</span>
The initial prepared-statement implementation substituted parameters by replacing
<code>?</code> markers in the original SQL text and then running the full parse + analyze
pipeline on each <code>COM_STMT_EXECUTE</code> call (~1.5 µs per execution). Phase 5.13
replaces this with an AST-level plan cache: parse + analyze run once at
<code>COM_STMT_PREPARE</code> time; each execute performs only a tree walk to splice in
the decoded parameter values (~1 µs). MySQL and PostgreSQL use the same strategy —
parsing and planning are separated from execution precisely so that repeated executions
avoid repeated parse overhead.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Text-Protocol Response for Prepared Statement Results</span>
<code>COM_STMT_EXECUTE</code> responses use the same text-protocol result-set format as
<code>COM_QUERY</code> (column defs + EOF + text-encoded rows + EOF), not the MySQL
binary result-set format. The binary result-set format requires a separate
<code>CLIENT_PS_MULTI_RESULTS</code> serialization path for every column type and adds
substantial protocol complexity with marginal benefit for typical workloads. The
text-protocol response is fully accepted by PyMySQL, SQLAlchemy, and the <code>mysql</code>
CLI. Binary result-set serialization is deferred to subphase 5.5a when a concrete
performance need arises.
</div>
</div>

**`ConnectionState` — per-connection session state:**

```rust
pub struct ConnectionState {
    pub current_database: String,
    pub autocommit: bool,
    // Typed charset state — negotiated at handshake, updated by SET NAMES
    client_charset: &'static CharsetDef,
    connection_collation: &'static CollationDef,
    results_collation: &'static CollationDef,
    pub variables: HashMap<String, String>,
    pub prepared_statements: HashMap<u32, PreparedStatement>,
    pub next_stmt_id: u32,
}
```

The three charset fields are typed references into the static `charset.rs` registry.
`from_handshake_collation_id(id: u8)` initializes all three from the collation id the
client sends in the `HandshakeResponse41` packet. Unsupported ids are rejected before
auth with ERR 1115 (ER_UNKNOWN_CHARACTER_SET). `SET NAMES <charset>` updates all three;
individual `SET character_set_client = …` updates only the relevant field.

`decode_client_text(&[u8]) -> Result<String, DbError>` decodes inbound SQL/identifiers.
`encode_result_text(&str) -> Result<Vec<u8>, DbError>` encodes outbound text columns.
Both are non-lossy — they return `DbError::InvalidValue` rather than replacement characters.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
The engine stays UTF-8 internally. Only the MySQL wire boundary gains transcoding — a
clean transport-charset layer. This is the same approach PostgreSQL uses with its
<code>client_encoding</code> / server-encoding split, but without the per-column collation
complexity that PostgreSQL adds. All AxiomDB storage is UTF-8; charset negotiation is
purely a wire-layer concern.
</div>
</div>

```rust
pub struct PreparedStatement {
    pub stmt_id: u32,
    pub sql_template: String,         // original SQL with ? placeholders
    pub param_count: u16,
    pub analyzed_stmt: Option<Stmt>,  // cached parse+analyze result (plan cache)
    pub compiled_at_version: u64,
    pub last_used_seq: u64,
    pub pending_long_data: Vec<Option<Vec<u8>>>,
    pub pending_long_data_error: Option<String>,
}
```

`analyzed_stmt` is populated by `COM_STMT_PREPARE` after parse + analyze succeed. On
`COM_STMT_EXECUTE`, if `analyzed_stmt` is `Some`, the handler calls
`substitute_params_in_ast` on the cached `Stmt` and invokes `execute_stmt()` directly,
skipping the parse and analyze steps entirely. If `analyzed_stmt` is `None` (should not
occur in normal operation), the handler falls back to the full parse + analyze path.

Each connection maintains its own `HashMap<u32, PreparedStatement>`. Statement IDs are
assigned by incrementing `next_stmt_id` (starting at 1) and are local to the connection
— the same ID on two connections refers to two different statements. `COM_STMT_CLOSE`
removes the entry; subsequent `COM_STMT_EXECUTE` calls for the closed ID return an
`Unknown prepared statement` error. `COM_STMT_RESET` leaves the entry in place and
clears only the stmt-local long-data buffers plus any deferred long-data error.

#### Packet framing and size enforcement (codec.rs — subphase 5.4a)

Every MySQL message in both directions — client to server and server to client — uses
the same 4-byte envelope:

```
[payload_length: u24 LE] [sequence_id: u8] [payload: payload_length bytes]
```

`MySqlCodec` implements `tokio_util::codec::{Decoder, Encoder}`. It holds a
configurable `max_payload_len` (default 64 MiB) that matches the session variable
`@@max_allowed_packet`.

**Two-phase decoder algorithm:**

1. **Scan phase** — walk physical packet headers without consuming bytes, accumulating
   `total_payload`. If `total_payload > max_payload_len`, return
   `MySqlCodecError::PacketTooLarge { actual, max }` before any buffer allocation.
   If any fragment is missing, return `Ok(None)` (backpressure).
2. **Consume phase** — advance the buffer and return `(seq_id, Bytes)`. For a single
   physical fragment this is a zero-copy `split_to` into the existing `BytesMut`. For
   multi-fragment logical packets one contiguous `BytesMut` is allocated with
   `capacity = total_payload` to avoid per-fragment copies.

**Multi-packet reassembly.** MySQL splits commands larger than 16,777,215 bytes
(`0xFF_FFFF`) across multiple physical packets. A fragment with
`payload_length = 0xFF_FFFF` signals continuation; the final fragment has
`payload_length < 0xFF_FFFF`. The limit applies to the **reassembled logical payload**,
not to each individual fragment.

**Live per-connection limit.** `handle_connection` calls
`reader.decoder_mut().set_max_payload_len(n)`:
- After auth (from `conn_state.max_allowed_packet_bytes()`)
- After a valid `SET max_allowed_packet = N`
- After `COM_RESET_CONNECTION` (restores `DEFAULT_MAX_ALLOWED_PACKET`)

**Oversize behavior.** On `PacketTooLarge`, the handler sends MySQL ERR
`1153 / SQLSTATE 08S01` ("Got a packet bigger than 'max_allowed_packet' bytes") and
breaks the connection loop. The stream is never re-used — re-synchronisation after an
oversize packet is unsafe.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Framing-layer enforcement</span>
The limit is enforced in <code>MySqlCodec::decode()</code>, before the payload reaches
UTF-8 decoding, SQL parsing, or binary-protocol decoding. MySQL 8 and MariaDB enforce
<code>max_allowed_packet</code> at the network I/O layer for the same reason: a SQL
parser that receives an oversized payload has already spent memory allocating it.
Rejecting at the codec boundary means zero heap allocation for oversized inputs.
</div>
</div>

#### Result set serialization (result.rs — subphase 5.5a)

AxiomDB has two result serializers sharing the same `column_count + column_defs + EOF`
framing but differing in row encoding:

| Serializer | Used for | Row format |
|---|---|---|
| `serialize_query_result` | `COM_QUERY` | Text protocol — NULL = `0xfb`, values as lenenc ASCII strings |
| `serialize_query_result_binary` | `COM_STMT_EXECUTE` | Binary protocol — null bitmap + fixed-width/lenenc values |

Both paths produce the same packet sequence shape:

```
column_count   (lenenc integer)
column_def_1   (lenenc strings: catalog, schema, table, org_table, name, org_name
                + 12-byte fixed section: charset, display_len, type_byte, flags, decimals)
…
column_def_N
EOF
row_1
…
row_M
EOF
```

**Binary row packet layout:**

```
0x00                      row header (always)
null_bitmap[ceil((N+2)/8)]  MySQL offset-2 null bitmap: column i → bit (i+2)
value_0 ... value_k         non-null values in column order (no per-cell headers)
```

The null bitmap uses MySQL's prepared-row offset of 2 — bits 0 and 1 are reserved.
Column 0 → bit 2, column 1 → bit 3, and so on.

**Binary cell encoding per type:**

| AxiomDB type | Encoding |
|---|---|
| `Bool` | 1 byte: `0x00` or `0x01` |
| `Int` | 4-byte signed LE |
| `BigInt` | 8-byte signed LE |
| `Real` | 8-byte IEEE-754 LE (`f64`) |
| `Decimal` | lenenc ASCII decimal string (exact, no float rounding) |
| `Text` | lenenc UTF-8 bytes |
| `Bytes` | lenenc raw bytes (no UTF-8 conversion) |
| `Date` | `[4][year u16 LE][month u8][day u8]` |
| `Timestamp` | `[7][year u16 LE][month][day][h][m][s]` or `[11][...][micros u32 LE]` |
| `Uuid` | lenenc canonical UUID string |

**Column type codes** (shared between both serializers):

| AxiomDB type | MySQL type byte | MySQL name |
|---|---|---|
| `Int` | `0x03` | LONG |
| `BigInt` | `0x08` | LONGLONG |
| `Real` | `0x05` | DOUBLE |
| `Decimal` | `0xf6` | NEWDECIMAL |
| `Text` | `0xfd` | VAR_STRING |
| `Bytes` | `0xfc` | BLOB |
| `Bool` | `0x01` | TINY |
| `Date` | `0x0a` | DATE |
| `Timestamp` | `0x07` | TIMESTAMP |
| `Uuid` | `0xfd` | VAR_STRING |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Single column-definition builder</span>
Both the text and binary serializers share one <code>build_column_def()</code> function
and one <code>datatype_to_mysql_type()</code> mapping. This guarantees that the type
byte in column metadata always agrees with the wire encoding of the row values.
A divergence (e.g., advertising <code>LONGLONG</code> but sending ASCII digits) would
cause silent data corruption on the client — a class of bug that is impossible when
there is only one mapping.
</div>
</div>

#### ORM query interception (handler.rs)

MySQL drivers and ORMs send several queries automatically before any user SQL:
`SET NAMES`, `SET autocommit`, `SELECT @@version`, `SELECT @@version_comment`,
`SELECT DATABASE()`, `SELECT @@sql_mode`, `SELECT @@lower_case_table_names`,
`SELECT @@max_allowed_packet`, `SHOW WARNINGS`, `SHOW DATABASES`.

`intercept_special_query` matches these by prefix/content and returns pre-built
packet sequences without touching the engine. Without this interception, most clients
fail to connect because they receive ERR packets for mandatory queries.

#### ON_ERROR session behavior (executor.rs, database.rs, subphase 5.2c)

`ON_ERROR` is implemented as one typed session enum shared by both layers that
own statement execution:

| Layer | State owner | Responsibility |
|---|---|---|
| SQL executor | `SessionContext.on_error` | Controls rollback policy for executor-time failures |
| Wire/session layer | `ConnectionState.on_error` | Exposes `SET on_error`, `@@on_error`, `SHOW VARIABLES`, and reset semantics |

This split is required by the current AxiomDB architecture. `handler.rs`
intercepts `SET` and `SELECT @@var` before the engine, but `database.rs` owns
the full `parse -> analyze -> execute_with_ctx` pipeline. A wire-only flag would
leave embedded execution inconsistent; an executor-only flag would make the MySQL
session variables lie.

**Execution modes:**

| Mode | Active transaction error | First failing DML with `autocommit=0` | Parse/analyze failure |
|---|---|---|---|
| `rollback_statement` | rollback to statement boundary, txn stays open | full rollback, txn closes | return ERR, txn state unchanged |
| `rollback_transaction` | eager full rollback, txn closes | eager full rollback, txn closes | eager full rollback if txn active |
| `savepoint` | same as `rollback_statement` | keep implicit txn open after rolling back the failing DML | return ERR, txn state unchanged |
| `ignore` | ignorable SQL errors -> warning + continue; non-ignorable runtime errors -> eager full rollback + ERR | ignorable SQL errors -> warning + continue; non-ignorable runtime errors -> eager full rollback + ERR | same split as active txn |

`ignore` reuses the existing `SHOW WARNINGS` path. For ignorable SQL/user
errors, `database.rs` maps the original `DbError` to the corresponding MySQL
warning code/message and returns `QueryResult::Empty`, which the serializer
turns into an OK packet with `warning_count > 0`. For non-ignorable errors
(`DiskFull`, WAL failures, storage/runtime corruption), the error still
surfaces as ERR and the transaction is eagerly rolled back if one is active.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Borrowed Savepoint Model</span>
AxiomDB borrows the "statement as anonymous savepoint" idea from MariaDB and SQLite, but adapts PostgreSQL's fail-fast use case into eager rollback instead of a persistent aborted-transaction latch. That keeps MySQL compatibility where it matters while avoiding a second long-lived txn state machine in the Phase 5 wire path.
</div>
</div>

#### SHOW STATUS — server and session counters (status.rs, subphase 5.9c)

MySQL clients, ORMs, and monitoring tools (PMM, Datadog MySQL integration, ProxySQL)
call `SHOW STATUS` on connect or periodically to query server health. Returning an
error or empty result breaks these integrations.

**Counter architecture:**

Two independent counter stores keep telemetry decoupled from correctness:

| Store | Type | Scope | Reset policy |
|---|---|---|---|
| `StatusRegistry` | `Arc<StatusRegistry>` with `AtomicU64` fields | Server-wide, shared across all connections | Only on server restart |
| `SessionStatus` | Plain `u64` fields inside `ConnectionState` | Per-connection | On `COM_RESET_CONNECTION` (which recreates `ConnectionState`) |

`Database` owns an `Arc<StatusRegistry>`. Each `handle_connection` task clones
the `Arc` once at connect time — the same pattern used by `schema_version`. The
`SHOW STATUS` intercept never acquires the `Database` mutex; it reads directly
from the cloned `Arc<StatusRegistry>` and the local `SessionStatus`. This means
the query cannot block other connections.

**RAII guards:**

```rust
// Increments threads_connected +1 after auth; drops −1 on disconnect (even on error).
let _connected_guard = ConnectedGuard::new(Arc::clone(&status));

// Increments threads_running +1 for the duration of COM_QUERY / COM_STMT_EXECUTE.
let _running = RunningGuard::new(&status);
```

`threads_connected` and `threads_running` are always accurate with no manual bookkeeping
because Rust's drop guarantees run on early returns and panics.

**Counters tracked:**

| Variable name | Scope | Description |
|---|---|---|
| `Bytes_received` | Session + Global | Bytes received from client (payload + 4-byte header) |
| `Bytes_sent` | Session + Global | Bytes sent to client |
| `Com_insert` | Session + Global | `INSERT` statement count |
| `Com_select` | Session + Global | `SELECT` statement count |
| `Innodb_buffer_pool_read_requests` | Global | Best-effort mmap access counter |
| `Innodb_buffer_pool_reads` | Global | Physical page reads (compatibility alias) |
| `Questions` | Session + Global | All statements executed (any command type) |
| `Threads_connected` | Global | Active authenticated connections |
| `Threads_running` | Session + Global | Connections actively executing a command |
| `Uptime` | Global | Seconds since server start |

**SHOW STATUS syntax:**

All four MySQL-compatible forms are intercepted before hitting the engine:

```sql
SHOW STATUS
SHOW SESSION STATUS
SHOW LOCAL STATUS
SHOW GLOBAL STATUS
-- Any of the above with LIKE filter:
SHOW STATUS LIKE 'Com_%'
SHOW GLOBAL STATUS LIKE 'Threads%'
```

LIKE filtering reuses `like_match` from `axiomdb-sql` (proper `%` / `_` wildcard
semantics, case-insensitive against variable names). Results are always returned in
ascending alphabetical order.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Lock-Free Status Reads</span>
<code>SHOW STATUS</code> reads <code>AtomicU64</code> counters directly from a cloned
<code>Arc</code> — it never acquires the <code>Database</code> mutex. MySQL InnoDB
reads status from the engine layer, which requires acquiring internal mutexes under
high concurrency. AxiomDB's design means monitoring queries cannot interfere with
query execution at any load level.
</div>
</div>

#### DB lock strategy

The MySQL handler stores the opened engine in `Arc<tokio::sync::RwLock<Database>>`.

- read-only statements acquire `db.read()`
- mutating statements and transaction control acquire `db.write()`
- multiple reads run concurrently
- all writes are still serialized at whole-database granularity

This is the current runtime model. It is more advanced than the old Phase 5
`Mutex<Database>` design because read-only queries can now overlap, but it is
still below MySQL/InnoDB and PostgreSQL for write concurrency because row-level
locking is not implemented yet.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Next Concurrency Milestone</span>
MySQL/InnoDB and PostgreSQL both lock at row granularity for ordinary UPDATE and
DELETE statements. AxiomDB's next concurrency step is Phase 13.7 (row-level
locking), followed by 13.8 (deadlock detection) and 13.8b (`FOR UPDATE`,
`SKIP LOCKED`, `NOWAIT`).
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Permissive Auth — Phase 5 Design Decision</span>
Phase 5 implements the full <code>mysql_native_password</code> SHA1 challenge-response
handshake (the same algorithm used by MySQL 5.x clients) but ignores the password
result for users in the allowlist (<code>root</code>, <code>axiomdb</code>, <code>admin</code>).
This lets any MySQL-compatible client connect during development without credential
management. The <code>verify_native_password</code> function is fully correct — it is
called and its result logged — but the decision to accept or reject is based solely
on the username allowlist until Phase 13 (Security) adds stored credentials and real
enforcement.
</div>
</div>

#### caching_sha2_password (MySQL 8.0+)

MySQL 8.0 changed the default authentication plugin from `mysql_native_password` to
`caching_sha2_password`. When a client using the new default (e.g., PyMySQL ≥ 1.0,
MySQL Connector/Python, mysql2 for Ruby) connects, the server must complete a 5-packet
handshake instead of the 3-packet one:

| Seq | Direction | Packet | Notes |
|-----|-----------|--------|-------|
| 0 | S → C | HandshakeV10 | includes 20-byte challenge |
| 1 | C → S | HandshakeResponse41 | `auth_plugin_name = "caching_sha2_password"` |
| 2 | S → C | AuthMoreData(0x03) | fast_auth_success — byte `0x03` signals that password verification is skipped in permissive mode |
| 3 | C → S | empty ack | client acknowledges the fast-auth signal before expecting OK |
| 4 | S → C | OK | connection established |

The critical implementation detail is that the ack packet at seq=3 **must be read**
before sending OK. If the server sends OK at seq=2 instead, the client has already
queued the empty ack packet. The server then reads that empty packet as a `COM_QUERY`
command (command byte `0x00` = COM_SLEEP, or simply an unknown command), which causes
the connection to close silently — no error is reported to the application.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">caching_sha2_password Sequence Number Gotcha</span>
MySQL 8.0 clients send an empty ack packet (seq=3) after receiving <code>AuthMoreData(fast_auth_success)</code>.
If the server skips reading that ack and sends OK immediately at seq=2, the client's
buffered ack arrives in the command loop, where it is misread as a <code>COM_QUERY</code>
(command byte <code>0x00</code> = COM_SLEEP). The connection closes silently with no error
visible to the application. The fix is one extra <code>read_packet()</code> call before
writing OK.
</div>
</div>

### axiomdb-server

Entry point for server mode. Parses CLI flags (`--data-dir`, `--port`), opens the
`axiomdb-network::Database`, starts a Tokio TCP listener, and spawns one
`handle_connection` task per accepted connection, passing each task a clone of the
`Arc<RwLock<Database>>`.

### axiomdb-embedded

Entry point for embedded mode. Exposes:

- A safe Rust API (`Database::open`, `Database::execute`, `Database::transaction`)
- A C FFI (`axiomdb_open`, `axiomdb_execute`, `axiomdb_close`, `axiomdb_free_string`)

---

## Query Lifecycle — From Wire to Storage

```
1. TCP bytes arrive on the socket
   │
2. axiomdb-network::mysql::codec::MySqlCodec decodes the 4-byte header
   → (sequence_id, payload)
   │
3. handler.rs inspects payload[0] (command byte)
   ├── 0x01 COM_QUIT  → close
   ├── 0x02 COM_INIT_DB → OK
   ├── 0x0e COM_PING  → OK
   ├── 0x16 COM_STMT_PREPARE → parse + analyze → store in PreparedStatement.analyzed_stmt → stmt_ok
   ├── 0x17 COM_STMT_EXECUTE → substitute_params_in_ast(cached_stmt, params) → execute_stmt() ↓ (step 9)
   └── 0x03 COM_QUERY → continue ↓
   │
4. intercept_special_query(sql) — ORM/driver stubs
   ├── match → return pre-built packet sequence  (no engine call)
   └── no match → continue ↓
   │
5. db.lock() → execute_query(sql, &mut session)
   │
6. axiomdb-sql::tokenize(sql)
   → Vec<SpannedToken>  (logos DFA, zero-copy)
   │
7. axiomdb-sql::parse(tokens)
   → Stmt  (recursive descent; all col_idx = placeholder 0)
   │
8. axiomdb-sql::analyze(stmt, storage, snapshot)
   → Stmt  (col_idx resolved against catalog; names validated)
   │
9. Executor interprets the analyzed Stmt
   → reads from axiomdb-index (BTree lookups / range scans)
   → calls axiomdb-types::decode_row on heap page bytes
   → builds Vec<Vec<Value>> result rows
   │
10. WAL write (for INSERT / UPDATE / DELETE)
    → axiomdb-wal::WalWriter::append(WalEntry)
    │
11. Heap page write (for INSERT / UPDATE / DELETE)
    → axiomdb-storage::StorageEngine::write_page
    │
12. db.lock() released
    │
13. result::serialize_query_result(QueryResult, seq=1)
    → column_count + column_defs + EOF + rows + EOF  (Rows)
    → OK packet with affected_rows + last_insert_id  (Affected)
    │
14. MySqlCodec encodes each packet with 4-byte header → TCP send
```

For embedded mode, steps 1–4 and 12–14 are replaced by a direct Rust function call
that returns a `QueryResult` struct.

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
