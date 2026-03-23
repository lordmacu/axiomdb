# Progress — dbyo Database Engine

> Automatically updated with `/subfase-completa`
> Legend: ✅ completed | 🔄 in progress | ⏳ pending | ⏸ blocked

---

## BLOCK 1 — Engine Foundations (Phases 1-7)

### Phase 1 — Basic storage `✅` week 1-3
- [x] 1.1 ✅ Workspace setup — Cargo.toml, folder structure, basic CI
- [x] 1.2 ✅ Page format — `struct Page`, `PageType`, CRC32c checksum, align(64)
- [x] 1.3 ✅ MmapStorage — open/create `.db`, `read_page`, `write_page` with mmap
- [x] 1.4 ✅ MemoryStorage — in-RAM implementation for tests (no I/O)
- [x] 1.5 ✅ Free list — `alloc_page`, `free_page`, bitmap of free pages
- [x] 1.6 ✅ Trait StorageEngine — unify Mmap and Memory with interchangeable trait
- [x] 1.7 ✅ Tests + benchmarks — unit, integration, bench for page read/write
- [x] 1.8 ✅ File locking — `fs2::FileExt::try_lock_exclusive()` in `create()` and `open()`; `Drop` releases the lock; `DbError::FileLocked` (SQLSTATE 55006) if already taken; 2 new tests
- [x] 1.9 ✅ Error logging from startup — `tracing_subscriber::fmt()` with `EnvFilter` in `nexusdb-server/main.rs`; `tracing::{info,debug,warn}` in `MmapStorage` (create, open, grow, drop)

### Phase 2 — B+ Tree `✅` week 3-4
- [x] 2.1 ✅ Node structures — `InternalNodePage`, `LeafNodePage`, bytemuck::Pod
- [x] 2.2 ✅ Exact key lookup — O(log n) search from root to leaf
- [x] 2.3 ✅ Insert with split — leaf split and propagation to internal node
- [x] 2.4 ✅ Range scan — lazy iterator with tree traversal (CoW-safe)
- [x] 2.5 ✅ Delete with merge — merge and node redistribution
- [x] 2.6 ✅ Copy-on-Write — atomic root with AtomicU64, lock-free readers by design
- [x] 2.7 ✅ Prefix compression — `CompressedNode` in memory for internal nodes
- [x] 2.8 ✅ Tests + benchmarks — 37 tests, Criterion benchmarks vs std::BTreeMap
- [ ] ⚠️ next_leaf linked list stale in CoW — range scan uses tree traversal instead → revisit in Phase 7 (MVCC + epoch reclamation)
- [x] ✅ 2.5.1 — eliminar heap allocations del hot path de lookup (2026-03-22)
- [x] ✅ 2.5.2 — binary search + in-place inserts; 4.46M lookup ops/s, 222K insert ops/s (2026-03-22)
- [x] ✅ Phase 1 — `expect()` eliminados de código de producción: mmap.rs, freelist.rs, memory.rs (2026-03-22)

### Phase 3 — WAL and transactions `🔄` week 5-10
- [x] 3.1 ✅ WAL entry format — `[LSN|Type|Table|Key|Old|New|CRC]` + backward scan
- [x] 3.2 ✅ WalWriter — append-only, global LSN, fsync on commit, open() with scan_last_lsn
- [x] 3.3 ✅ WalReader — scan_forward(from_lsn) streaming + scan_backward() with entry_len_2
- [ ] 3.4 ⏳ RowHeader — `struct RowHeader { txn_id_created, txn_id_deleted, row_version, deleted_flag }` — prerequisite for 3.5 and Phase 7
- [ ] 3.5 ⏳ BEGIN / COMMIT / ROLLBACK basic — transactions over RowHeader
- [ ] 3.5a ⏳ Autocommit mode — each DML without explicit BEGIN is its own transaction; `autocommit=ON` flag by default (MySQL compatible); `SET autocommit=0` disables it
- [ ] 3.5b ⏳ Implicit transaction start (MySQL mode) — in MySQL, the first DML without autocommit starts a transaction implicitly; required for compatibility with ORMs that do not issue explicit BEGIN
- [ ] 3.5c ⏳ Error semantics mid-transaction — distinguish between: (a) constraint violation → statement rollback, transaction continues; (b) severe error → full transaction rollback; define explicit behavior
- [ ] 3.6 ⏳ WAL Checkpoint — flush dirty pages to disk, truncate WAL up to checkpoint LSN
- [ ] 3.6b ⏳ ENOSPC handling — detect `ENOSPC` (disk full) on WAL and page writes; perform graceful shutdown with error log instead of corrupting the file; alert before reaching the limit (configurable threshold)
- [ ] 3.7 ⏳ WAL rotation — configurable max_wal_size, auto-checkpoint by size
- [ ] 3.8 ⏳ Crash recovery state machine — explicit states: `CRASHED→RECOVERING→REPLAYING_WAL→VERIFYING→READY`; validate checkpoint metadata; recovery modes: `strict` (abort on inconsistency) / `permissive` (best-effort, warn and continue)
- [ ] 3.8b ⏳ Partial page write detection — on DB open, detect pages whose checksum does not match (write interrupted by power loss); in strict mode: reject; in permissive mode: mark as corrupt and restore from WAL if there is a recent entry
- [ ] 3.9 ⏳ Post-recovery integrity check — verify index vs main table consistency after replay; detect and report divergence before accepting connections
- [ ] 3.10 ⏳ Durability tests — write → simulate crash → re-read → verify; cover: corrupt checkpoint, partial page write, truncated WAL, divergent indexes post-crash
- [ ] 3.11 ⏳ Catalog bootstrap — reserved pages (0-N) for system tables on DB create/open
- [ ] 3.12 ⏳ CatalogReader/Writer — API to read/write table, column, constraint, and index definitions
- [ ] 3.13 ⏳ Catalog change notifier — internal pub-sub when DDL changes the schema (DDL writes → subscribers notified); prerequisite for invalidating plan cache (5.14) and stats (6.11)
- [ ] 3.14 ⏳ Schema binding — executor resolves table/column names against the catalog
- [ ] 3.13 ⏳ Page dirty tracker — in-memory bitmap of modified pages pending flush; basis for efficient WAL checkpoint
- [ ] 3.15 ⏳ Page dirty tracker — in-memory bitmap of modified pages pending flush; basis for efficient WAL checkpoint
- [ ] 3.16 ⏳ Basic configuration (dbyo.toml) — parse `page_size`, `max_wal_size`, `data_dir`, `fsync` with `config` crate; safe defaults if file is missing

### Phase 4 — SQL Parser + Executor `⏳` week 11-25
<!-- Group A — Executor prerequisites -->
- [ ] 4.0 ⏳ Row codec — encode/decode `Value[]` ↔ bytes with null_bitmap; covers basic types: BOOL, INT, BIGINT, REAL, DOUBLE, DECIMAL, TEXT, VARCHAR, DATE, TIMESTAMP, NULL
<!-- Group B — Parser (AST first, then grammar) -->
- [ ] 4.1 ⏳ AST definitions — syntax tree types (Expr, Stmt, TableRef, ColumnDef nodes)
- [ ] 4.2 ⏳ Lexer/Tokenizer — SQL tokens with `nom`
- [ ] 4.2b ⏳ Input sanitization in parser — validate that malformed SQL returns a clear SQL error, never `panic`; configurable query length limit (`max_query_size`); immediate fuzz-test with random inputs
- [ ] 4.3 ⏳ DDL Parser — `CREATE TABLE`, `CREATE INDEX`, `DROP TABLE`, `DROP INDEX`
- [ ] 4.3a ⏳ Column constraints in DDL — `NOT NULL`, `DEFAULT expr`, `UNIQUE`, `PRIMARY KEY`, `REFERENCES fk`; parsed as part of `CREATE TABLE`; prerequisite for the basic executor
- [ ] 4.3b ⏳ Basic CHECK constraint in DDL — `CHECK (expr)` at column and table level; parsed in `CREATE TABLE`; evaluated in INSERT/UPDATE (moves advanced CHECK with DOMAIN to Phase 21.6)
- [ ] 4.3c ⏳ AUTO_INCREMENT / SERIAL basic — `INT AUTO_INCREMENT` (MySQL) and `SERIAL` (PostgreSQL-compat); generates internal sequence per table; `LAST_INSERT_ID()` returns the last value; prerequisite for the basic executor (do not wait for Phase 24)
- [ ] 4.3d ⏳ Max identifier length — limit of 64 characters for table, column, index names (MySQL/PostgreSQL compatible); clear SQL error when exceeded
- [ ] 4.4 ⏳ DML Parser — `SELECT`, `INSERT`, `UPDATE`, `DELETE`
<!-- Group C — Basic executor -->
- [ ] 4.5 ⏳ Basic executor — connect AST with storage + B+ Tree + catalog (uses 3.12 schema binding); **depends on: 4.1-4.4, 4.18 semantics, 3.12 schema binding**
- [ ] 4.5a ⏳ SELECT without FROM — `SELECT 1`, `SELECT NOW()`, `SELECT VERSION()`; ORMs and tools use this as a health check on connect; requires no table
- [ ] 4.6 ⏳ INSERT ... SELECT — insert query result directly
- [ ] 4.7 ⏳ SQLSTATE codes — standard SQL error codes (23505, 42P01, etc.)
<!-- Group D — Fundamental SQL (needed before wire protocol) -->
- [ ] 4.8 ⏳ JOIN — INNER, LEFT, RIGHT, CROSS with basic nested loop join
- [ ] 4.9a ⏳ GROUP BY hash-based — hash table for grouping; optimal for high cardinality
- [ ] 4.9b ⏳ GROUP BY sort-based — sort first, then stream; optimal when data is already sorted (index)
- [ ] 4.9c ⏳ Aggregate functions — COUNT, SUM, MIN, MAX, AVG, COUNT DISTINCT; implement with state per group
- [ ] 4.9d ⏳ HAVING clause — filter groups post-aggregation; needs to evaluate expression over group states
- [ ] 4.10 ⏳ ORDER BY + LIMIT/OFFSET — in-memory sort + pagination
- [ ] 4.10b ⏳ Multi-column ORDER BY with mixed direction — `ORDER BY a ASC, b DESC, c ASC`; composite comparator that respects direction per column; test with NULLs in each position
- [ ] 4.10c ⏳ NULLS FIRST / NULLS LAST — `ORDER BY price ASC NULLS LAST`; default behavior MySQL (NULLs first in ASC) vs PostgreSQL (NULLs last in ASC); configurable
- [ ] 4.10d ⏳ Parameterized LIMIT/OFFSET — `LIMIT $1 OFFSET $2` in prepared statements; avoid rebuilding plan for each pagination value
- [ ] 4.11 ⏳ Scalar subqueries — `(SELECT MAX(id) FROM t)` in WHERE and SELECT
- [ ] 4.12 ⏳ DISTINCT — `SELECT DISTINCT col1, col2` remove duplicates; implement with hash set or sort; interacts with ORDER BY
- [ ] 4.12b ⏳ CAST + basic type coercion — explicit and implicit conversion between compatible types
<!-- Group E — System functions and DevEx -->
- [ ] 4.13 ⏳ version() / current_user / session_user / current_database() — ORMs call this on connect
- [ ] 4.14 ⏳ LAST_INSERT_ID() / lastval() — get last auto-generated ID (MySQL + PG compat)
- [ ] 4.15 ⏳ Interactive CLI — REPL like `sqlite3` shell
- [ ] 4.15b ⏳ DEBUG/VERBOSE mode — `--verbose` flag in CLI and server; log AST, chosen plan, execution stats per query; needed for debugging during Phases 4-10 development
- [ ] 4.16 ⏳ SQL Tests — full suite: DDL + DML + JOIN + GROUP BY + ORDER BY + subqueries
<!-- Group F — Expression layer and semantics (required by executor for WHERE, SELECT expressions) -->
- [ ] 4.17 ⏳ Expression evaluator — evaluation tree for arithmetic (`+`, `-`, `*`, `/`), booleans (`AND`, `OR`, `NOT`), comparisons (`=`, `<`, `>`), `LIKE`, `BETWEEN`, `IN (list)`, `IS NULL`
- [ ] 4.17b ⏳ Systematic NULL semantics — `NULL + 1 = NULL`, `NULL = NULL → UNKNOWN`, `NULL IN (1,2) = NULL`; the 3 logics (TRUE/FALSE/UNKNOWN); `IS NULL` vs `= NULL`; functions that propagate NULL; without this, aggregation queries silently produce incorrect results
- [ ] 4.18 ⏳ Semantic analyzer — validate table/column existence against catalog, resolve ambiguities, clear SQL error for each violation
- [ ] 4.18b ⏳ Type coercion matrix — explicit rules for when/how to coerce types: `'42'→INT`, `INT→BIGINT`, `DATE→TIMESTAMP`; define MySQL-compatible mode (permissive) vs strict mode; clear errors on invalid conversions
- [ ] 4.19 ⏳ Basic built-in functions — `ABS`, `LENGTH`, `SUBSTR`, `UPPER`, `LOWER`, `TRIM`, `COALESCE`, `NOW()`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `ROUND`, `FLOOR`, `CEIL`
<!-- Group G — Introspection + modification DDL (needed for ORMs and early migrations) -->
- [ ] 4.20 ⏳ SHOW TABLES / SHOW COLUMNS / DESCRIBE — basic introspection; ORMs and GUI clients use this on connect
- [ ] 4.21 ⏳ TRUNCATE TABLE — empty table without per-row WAL entry; faster than DELETE without WHERE
- [ ] 4.22 ⏳ Basic ALTER TABLE — `ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN`, `RENAME TABLE` (blocking, no concurrent); prerequisite for any migration
- [ ] 4.22b ⏳ ALTER TABLE ADD/DROP CONSTRAINT — `ADD CONSTRAINT fk_name FOREIGN KEY`, `DROP CONSTRAINT`, `ADD UNIQUE (col)`, `ADD CHECK (expr)`; without this ORMs cannot modify constraints post-creation
- [ ] 4.24 ⏳ CASE WHEN in any context — `CASE WHEN x THEN a ELSE b END` in SELECT, WHERE, ORDER BY, GROUP BY, HAVING; Phase 28.7 lists it but it is needed from Phase 4 for basic queries from any ORM
- [ ] 4.25 ⏳ Error handling framework — standard SQLSTATE codes (23505, 42P01, 40001), propagation without panic to the client, recovery from constraint and type errors; base for all other modules

### Phase 5 — MySQL Wire Protocol `⏳` week 26-30
- [ ] 5.1 ⏳ TCP listener with Tokio — accept connections on :3306
- [ ] 5.2 ⏳ MySQL handshake — Server Greeting + Client Response
- [ ] 5.2a ⏳ Charset/collation negotiation in handshake — `character_set_client`, `character_set_results`, `collation_connection` sent in Server Greeting; client chooses charset; without this modern MySQL clients cannot connect or display incorrect characters
- [ ] 5.3 ⏳ Authentication — basic `mysql_native_password` (SHA1-based for MySQL 5.x compatibility)
- [ ] 5.3b ⏳ caching_sha2_password — MySQL 8.0+ auth plugin; required by MySQL Workbench, DBeaver and modern clients; full auth + fast auth path
- [ ] 5.4 ⏳ COM_QUERY handler — receive SQL, execute, respond
- [ ] 5.4a ⏳ max_allowed_packet enforcement — limit incoming packet size (default 64MB); reject with error if exceeded; prevent OOM from malicious or accidental query
- [ ] 5.5 ⏳ Result set serialization — columns + rows in wire protocol (text protocol)
- [ ] 5.5a ⏳ Binary result encoding by type — MySQL binary protocol for prepared statements: DATE as `{year,month,day}`, DECIMAL as precision-exact string, BLOB as length-prefixed bytes, BIGINT as little-endian 8 bytes; without this types are corrupted in prepared statement results
- [ ] 5.6 ⏳ Error packets — serialize `DbError` as MySQL error
- [ ] 5.7 ⏳ Test with real client — PHP PDO or Python PyMySQL connects and queries
- [ ] 5.8 ⏳ Protocol unit tests — verify handshake/COM_QUERY/error/result-set packets without external client
- [ ] 5.9 ⏳ Session state — per-connection session variables: current_database, SET/SHOW, autocommit
- [ ] 5.10 ⏳ COM_STMT_PREPARE / COM_STMT_EXECUTE — prepared statements over wire protocol; all ORMs use them, avoid parse overhead per query
- [ ] 5.11 ⏳ COM_PING / COM_QUIT / COM_RESET_CONNECTION / COM_INIT_DB — connection management commands that clients send automatically
- [ ] 5.11b ⏳ COM_STMT_SEND_LONG_DATA — chunked transmission of large parameters (BLOBs, TEXTs) in multiple packets; required for INSERT of images/documents via prepared statements
- [ ] 5.11c ⏳ Explicit connection state machine — states: `CONNECTED→AUTH→IDLE→EXECUTING→CLOSING`; timeout handling per state; detect abruptly closed socket (TCP keepalive)
- [ ] 5.12 ⏳ Multi-statement queries — respond to multiple SELECTs separated by `;` in a single COM_QUERY (PHP legacy, SQL scripts)
- [ ] 5.13 ⏳ Prepared statement plan cache — cache compiled plan by statement_id; reuse without re-parsing on successive executions; subscribe to catalog change notifier (3.13) to invalidate automatically when schema changes; LRU eviction with configurable limit
- [ ] 5.14 ⏳ Throughput benchmarks — measure queries/second with 1, 4, 16, 64 concurrent connections; baseline to compare with MySQL

### Phase 6 — Secondary indexes + FK `⏳` week 31-39
- [ ] 6.1 ⏳ Multiple B+ Trees per table — one tree per index
- [ ] 6.1b ⏳ Composite indexes — multi-column indexes (a, b, c) with lexicographic comparison
- [ ] 6.2 ⏳ CREATE INDEX — create tree and populate from existing data
- [ ] 6.3 ⏳ Basic query planner — choose index vs full scan with simple statistics
- [ ] 6.4 ⏳ Bloom filter per index — avoid I/O for non-existent keys
- [ ] 6.5 ⏳ Foreign key checker — validation on INSERT/UPDATE with reverse index
- [ ] 6.6 ⏳ ON DELETE CASCADE / RESTRICT / SET NULL
- [ ] 6.7 ⏳ Partial UNIQUE index — `UNIQUE WHERE condition` for soft delete
- [ ] 6.8 ⏳ Fill factor — `WITH (fillfactor=70)` for tables with many inserts
- [ ] 6.9 ⏳ FK and index tests — violations, cascades, restrictions
- [ ] 6.10 ⏳ Index statistics bootstrap — on CREATE INDEX: count rows, estimate NDV (distinct values) per column; feeds query planner (6.3)
- [ ] 6.11 ⏳ Auto-update statistics — recalculate stats when INSERT/DELETE exceeds configurable threshold (20% of table); avoids stale plans
- [ ] 6.12 ⏳ ANALYZE SQL command — `ANALYZE [TABLE [column]]` to force manual statistics update
- [ ] 6.13 ⏳ Index-only scans — when SELECT columns are all in the index, do not read the main table (covering scan)
- [ ] 6.14 ⏳ MVCC on secondary indexes — each index entry includes `(key, RecordId, txn_id_visible_from)`; UPDATE of indexed column inserts new version without deleting the old one; vacuum cleans dead index versions
- [ ] 6.15 ⏳ Index corruption detection — on DB open verify index checksums; detect index vs table divergence; automatic `REINDEX` if divergent (recovery mode)

### Phase 7 — Concurrency + MVCC `⏳` week 40-48
- [ ] 7.1 ⏳ MVCC visibility rules — snapshot_id rules over RowHeader (struct defined in 3.4): which rows are visible; implement READ COMMITTED (snapshot per statement) and REPEATABLE READ (snapshot per transaction) explicitly
- [ ] 7.2 ⏳ Transaction manager — global atomic txn_id counter
- [ ] 7.3 ⏳ Snapshot isolation — visibility rules per snapshot_id
- [ ] 7.4 ⏳ Lock-free readers with CoW — verify that reads do not block writes
- [ ] 7.5 ⏳ Writer serialization — only 1 writer at a time per table (improve later)
- [ ] 7.6 ⏳ ROLLBACK — mark txn rows as deleted
- [ ] 7.7 ⏳ Concurrency tests — N simultaneous readers + N writers
- [ ] 7.8 ⏳ Epoch-based reclamation — free CoW pages when no active snapshot references them
- [ ] 7.9 ⏳ Resolve next_leaf CoW gap — linked list between leaves in Copy-on-Write (DEFERRED from 2.8)
- [ ] 7.10 ⏳ Lock timeout — wait for lock with configurable timeout (`lock_timeout`); `LockTimeoutError` if expired; avoids simple deadlocks without a detector
- [ ] 7.11 ⏳ Basic MVCC vacuum — purge dead row versions (txn_id_deleted < oldest_active_snapshot); frees space without blocking reads
- [ ] 7.12 ⏳ Basic savepoints — `SAVEPOINT sp1`, `ROLLBACK TO sp1`, `RELEASE sp1`; ORMs use them for partial errors in long transactions
- [ ] 7.13 ⏳ Isolation tests — verify READ COMMITTED and REPEATABLE READ with concurrent transactions; test dirty reads, non-repeatable reads, phantom reads; use real concurrent transactions (not mocks)
- [ ] 7.14 ⏳ Cascading rollback prevention — if txn A aborts and txn B read data from A (dirty read), B must also abort; verify that READ COMMITTED prevents this structurally
- [ ] 7.15 ⏳ Basic transaction ID overflow prevention — `txn_id` is u64; log warning at 50% and 90% of capacity; plan for VACUUM FREEZE (complete in Phase 34) but detection must be early

---

## BLOCK 2 — Execution Optimizations (Phases 8-10)

### Phase 8 — SIMD Optimizations `⏳` week 19-20
- [ ] 8.1 ⏳ Vectorized filter — evaluate predicates in chunks of 1024 rows
- [ ] 8.2 ⏳ SIMD AVX2 with `wide` — compare 8-32 values per instruction
- [ ] 8.3 ⏳ Improved query planner — selectivity, index vs scan with stats
- [ ] 8.4 ⏳ Basic EXPLAIN — show chosen plan (join type, index or full scan, estimated cost)
- [ ] 8.5 ⏳ SIMD vs MySQL benchmarks — point lookup, range scan, seq scan
- [ ] 8.6 ⏳ SIMD correctness tests — verify that SIMD results are identical to row-by-row without SIMD
- [ ] 8.7 ⏳ Runtime CPU feature detection — detect AVX2/SSE4.2 on startup; select optimal implementation; scalar fallback on old CPUs (ARM, CI)
- [ ] 8.8 ⏳ SIMD vs scalar vs MySQL benchmark — comparison table per operation (filter, sum, count); document real speedup in `docs/fase-8.md`

### Phase 9 — DuckDB-inspired + Join Algorithms `⏳` week 21-23
- [ ] 9.1 ⏳ Morsel-driven parallelism — split into 100K chunks, Rayon
- [ ] 9.2 ⏳ Operator fusion — scan+filter+project in a single lazy loop
- [ ] 9.3 ⏳ Late materialization — cheap predicates first, read expensive columns at the end
- [ ] 9.4 ⏳ Benchmarks with parallelism — measure scaling with N cores
- [ ] 9.5 ⏳ Vectorized correctness tests — verify that fusion/morsel/late-mat produce identical results to the basic executor
<!-- Join algorithms: nested loop (4.8) is O(n*m); hash and sort-merge are essential for real queries -->
- [ ] 9.6 ⏳ Hash join — build phase (small table in hash map) + probe phase (scan large table); O(n+m) vs O(n*m) of nested loop
- [ ] 9.7 ⏳ Sort-merge join — sort both tables by join key + merge; optimal when data is already ordered (index)
- [ ] 9.8 ⏳ Spill to disk — when hash table or sort buffer exceeds `work_mem`, spill to temp files; no OOM on large joins
- [ ] 9.9 ⏳ Adaptive join selection — query planner chooses nested loop / hash / sort-merge based on size and selectivity statistics
- [ ] 9.10 ⏳ Join algorithms benchmarks — compare 3 strategies with different sizes; confirm that hash join beats nested loop with >10K rows

### Phase 10 — Embedded mode + FFI `⏳` week 24-25
- [ ] 10.1 ⏳ Refactor engine as reusable `lib.rs`
- [ ] 10.2 ⏳ C FFI — `dbyo_open`, `dbyo_execute`, `dbyo_close` with `#[no_mangle]`
- [ ] 10.3 ⏳ Compile as `cdylib` — `.so` / `.dll` / `.dylib`
- [ ] 10.4 ⏳ Python binding — working `ctypes` demo
- [ ] 10.5 ⏳ Embedded test — same DB used from server and from library
- [ ] 10.6 ⏳ Node.js binding (Neon) — native `.node` module for Electron and Node apps; async/await API
- [ ] 10.7 ⏳ Embedded vs server benchmark — compare in-process vs TCP loopback latency to demonstrate embedded advantage

---

> **🏁 MVP CHECKPOINT — week ~50**
> On completing Phase 10, NexusDB must be able to:
> - Accept MySQL connections from PHP/Python/Node
> - Execute DDL (CREATE TABLE, ALTER TABLE, DROP) and DML (SELECT/INSERT/UPDATE/DELETE)
> - Transactions with COMMIT/ROLLBACK/SAVEPOINTS
> - Secondary indexes and FK
> - Full crash recovery
> - Basic vectorized execution
> - Usable as an embedded library from C/Python
>
> **ORM target at this point:** Django ORM and SQLAlchemy with basic queries.

---

## BLOCK 3 — Advanced Features (Phases 11-15)

### Phase 11 — Robustness and indexes `⏳` week 26-27
- [ ] 11.1 ⏳ Sparse index — one entry every N rows for timestamps
- [ ] 11.2 ⏳ TOAST — values >2KB to overflow pages with LZ4
- [ ] 11.3 ⏳ In-memory mode — `open(":memory:")` without disk
- [ ] 11.4 ⏳ Native JSON — JSON type, `->>`  extraction with jsonpath
- [ ] 11.4b ⏳ JSONB_SET — update JSON field without rewriting the entire document
- [ ] 11.4c ⏳ JSONB_DELETE_PATH — remove specific field from JSONB
- [ ] 11.5 ⏳ Partial indexes — `CREATE INDEX ... WHERE condition`
- [ ] 11.6 ⏳ Basic FTS — tokenizer + inverted index + BM25 ranking
- [ ] 11.7 ⏳ Advanced FTS — phrases, booleans, prefixes, stop words
- [ ] 11.8 ⏳ Buffer pool manager — explicit LRU page cache (not just OS mmap); dirty list, flush scheduler, prefetch for seq scan
- [ ] 11.9 ⏳ Page prefetching — when sequential scan is detected, prefetch N pages ahead with `madvise(MADV_SEQUENTIAL)` or own read-ahead
- [ ] 11.10 ⏳ Write combining — group writes to hot pages in a single fsync per commit; reduces IOPS on write-heavy workloads

### Phase 12 — Testing + JIT `⏳` week 28-29
- [ ] 12.1 ⏳ Deterministic simulation testing — `FaultInjector` with seed
- [ ] 12.2 ⏳ EXPLAIN ANALYZE — real times per plan node; JSON output format compatible with PostgreSQL (`{"Plan":{"Node Type":..., "Actual Rows":..., "Actual Total Time":..., "Buffers":{}}}`) and indented text format for psql/CLI; metrics: actual rows, loops, shared/local buffers hit/read, planning time, execution time
- [ ] 12.3 ⏳ Basic JIT with LLVM — compile simple predicates to native code
- [ ] 12.4 ⏳ Final block 1 benchmarks — compare with MySQL and SQLite
- [ ] 12.5 ⏳ SQL parser fuzz testing — `cargo fuzz` on the parser with random inputs; register crashes as regression tests
- [ ] 12.6 ⏳ Storage fuzz testing — pages with random bytes, deliberate corruptions; verify that crash recovery handles corrupted data
- [ ] 12.7 ⏳ ORM compatibility tier 1 — Django ORM and SQLAlchemy connect, run simple migrations and SELECT/INSERT/UPDATE/DELETE queries without errors; document workarounds if any

### Phase 13 — Advanced PostgreSQL `⏳` week 30-31
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — native pub-sub with `DashMap` of channels
- [ ] 13.5 ⏳ Covering indexes — `INCLUDE (col1, col2)` in B+ Tree leaves
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + atomic swap
- [ ] 13.7 ⏳ Row-level locking — lock specific row during UPDATE/DELETE; reduces contention vs per-table lock from 7.5
- [ ] 13.8 ⏳ Deadlock detection — DFS on wait graph when lock_timeout expires; kill the youngest transaction

### Phase 14 — TimescaleDB + Redis inspired `⏳` week 32-33
- [ ] 14.1 ⏳ Table partitioning — `PARTITION BY RANGE/HASH/LIST`
- [ ] 14.2 ⏳ Partition pruning — query planner skips irrelevant partitions
- [ ] 14.3 ⏳ Automatic compression of historical partitions — LZ4 columnar
- [ ] 14.4 ⏳ Continuous aggregates — incremental refresh of only the new delta
- [ ] 14.5 ⏳ TTL per row — `WITH TTL 3600` + background reaper in Tokio
- [ ] 14.6 ⏳ LRU eviction — for in-memory mode with RAM limit
- [ ] 14.7 ⏳ Chunk-level compression statistics — track compression ratio per partition; decides when to compress automatically
- [ ] 14.8 ⏳ Time-series benchmarks — insert 1M rows with timestamp; compare range scan vs TimescaleDB

### Phase 15 — MongoDB + DoltDB + Arrow `⏳` week 34-35
- [ ] 15.1 ⏳ Change streams CDC — tail the WAL, emit Insert/Update/Delete events
- [ ] 15.2 ⏳ Git for data — commits, branches, checkout with snapshot of roots
- [ ] 15.3 ⏳ Git merge — branch merge with conflict detection
- [ ] 15.4 ⏳ Apache Arrow output — results in columnar format for Python/pandas
- [ ] 15.5 ⏳ Flight SQL — Arrow Flight protocol for high-speed columnar transfer (Python, Rust, Java without JDBC)
- [ ] 15.6 ⏳ CDC + Git tests — verify change streams and branch merge with real conflicts

---

## BLOCK 4 — Logic and Security (Phases 16-17)

### Phase 16 — Server logic `⏳` week 36-38
- [ ] 16.1 ⏳ Scalar SQL UDFs — `CREATE FUNCTION ... AS $$ ... $$`
- [ ] 16.2 ⏳ Table SQL UDFs — return multiple rows
- [ ] 16.3 ⏳ BEFORE/AFTER triggers — with `WHEN` condition and `SIGNAL`
- [ ] 16.3b ⏳ INSTEAD OF triggers — INSERT/UPDATE/DELETE logic over views
- [ ] 16.4 ⏳ Lua runtime — `mlua`, EVAL with atomic `query()` and `execute()`
- [ ] 16.5 ⏳ WASM runtime — `wasmtime`, sandbox, memory limits and timeout
- [ ] 16.6 ⏳ CREATE FUNCTION LANGUAGE wasm FROM FILE — load .wasm plugin
- [ ] 16.7 ⏳ Stored procedures — `CREATE PROCEDURE` with flow control (`IF`, `LOOP`, `WHILE`, `BEGIN/END`)
- [ ] 16.8 ⏳ Exception handling in procedures — `DECLARE ... HANDLER FOR SQLSTATE`, re-raise, cleanup handlers
- [ ] 16.9 ⏳ UDF and trigger tests — correctness, error handling, WHEN conditions, INSTEAD OF over views

### Phase 17 — Security `⏳` week 39-40
- [ ] 17.1 ⏳ CREATE USER / CREATE ROLE — user and role model
- [ ] 17.2 ⏳ GRANT / REVOKE — permissions per table and per column
- [ ] 17.3 ⏳ Row-Level Security — policies with `USING` expr applied automatically
- [ ] 17.4 ⏳ Argon2id — password hashing + Scram-SHA-256 in handshake
- [ ] 17.5 ⏳ TLS 1.3 — encrypted connections with `tokio-rustls`
- [ ] 17.6 ⏳ Statement timeout — per user, session and global
- [ ] 17.7 ⏳ Audit trail — `CREATE AUDIT POLICY` with automatic logging
- [ ] 17.8 ⏳ Account lockout — tracking failed attempts + automatic lockout
- [ ] 17.9 ⏳ Password policy — minimum length, complexity, expiration, history
- [ ] 17.10 ⏳ IP allowlist per user — pg_hba.conf with rules per IP/CIDR
- [ ] 17.11 ⏳ Connection rate limiting — max connections per second per user/IP
- [ ] 17.12 ⏳ Log levels and rotation — trace/debug/info/warn/error + daily rotation
- [ ] 17.13 ⏳ SQL injection prevention — mandatory prepared statements in wire protocol; detect and block direct interpolation in internal APIs
- [ ] 17.14 ⏳ Security tests — RLS bypass attempts, brute force, SQL injection, privilege escalation

---

## BLOCK 5 — High Availability (Phases 18-19)

### Phase 18 — High availability `⏳` week 41-43
- [ ] 18.1 ⏳ Streaming replication — send WAL in real time to replica
- [ ] 18.2 ⏳ Replica apply — receive and apply WAL entries
- [ ] 18.3 ⏳ Configurable synchronous commit — off, local, remote_write, remote_apply
- [ ] 18.4 ⏳ Cascading replication — replica retransmits to sub-replicas
- [ ] 18.5 ⏳ Hot standby — reads from replica while applying WAL
- [ ] 18.6 ⏳ PITR — restore to the exact second using archived WAL
- [ ] 18.7 ⏳ Hot backup — `BACKUP DATABASE` without locking
- [ ] 18.8 ⏳ WAL archiving — copy WAL segments to external storage (S3/local) automatically; prerequisite for PITR (18.6)
- [ ] 18.9 ⏳ Replica lag monitoring — `replication_lag_bytes` and `replication_lag_seconds` metrics exposed in virtual system `sys.replication_status`
- [ ] 18.10 ⏳ Basic automatic failover — detect primary down + promote standby; minimal configuration without Raft

### Phase 19 — Maintenance + observability `⏳` week 44-46
- [ ] 19.1 ⏳ Auto-vacuum — background task in Tokio, configurable threshold per table
- [ ] 19.2 ⏳ VACUUM CONCURRENTLY — compact without blocking reads or writes
- [ ] 19.3 ⏳ Deadlock detection — DFS on wait graph every 100ms
- [ ] 19.4 ⏳ Statement fingerprinting — normalize SQL (remove literals, replace with `$1`, `$2`); hash the result to group identical queries with different parameters; prerequisite for pg_stat_statements and slow query log
- [ ] 19.4b ⏳ pg_stat_statements — fingerprint (via 19.4) + calls + total/min/max/stddev time + cache hits/misses per query
- [ ] 19.5 ⏳ Slow query log — JSON with execution plan
- [ ] 19.6 ⏳ Connection pooling — Semaphore + built-in idle pool
- [ ] 19.7 ⏳ pg_stat_activity — view and cancel running queries
- [ ] 19.8 ⏳ pg_stat_progress_vacuum — real-time vacuum progress
- [ ] 19.9 ⏳ lock_timeout — error if waiting for a lock more than N ms
- [ ] 19.10 ⏳ deadlock_timeout — how long to wait before running deadlock detector
- [ ] 19.11 ⏳ idle_in_transaction_session_timeout — kill abandoned transactions
- [ ] 19.12 ⏳ pg_stat_user_tables — seq_scan, idx_scan, n_live_tup, n_dead_tup per table
- [ ] 19.13 ⏳ pg_stat_user_indexes — idx_scan, idx_tup_read per index
- [ ] 19.14 ⏳ Table/index bloat detection — dead_tup/live_tup ratio with alert threshold
- [ ] 19.15 ⏳ REINDEX TABLE / INDEX / DATABASE — rebuild corrupt or bloated indexes
- [ ] 19.16 ⏳ REINDEX CONCURRENTLY — rebuild index without blocking writes
- [ ] 19.17 ⏳ Prometheus metrics endpoint — `/metrics` HTTP on configurable port; expose ops/s, p99 latency, cache hit rate, replication lag
- [ ] 19.18 ⏳ Health check endpoint — `/health` and `/ready` for load balancers; verify WAL, storage and replicas
- [ ] 19.19 ⏳ pg_stat_wal — bytes written, syncs, sync time; detect WAL as bottleneck
- [ ] 19.20 ⏳ Audit trail infrastructure — write audit logs async (circular buffer, without blocking writer); JSON format with: user, IP, SQL, bind params, rows_affected, duration, result; daily rotation; prerequisite for 17.7 (CREATE AUDIT POLICY)

---

## BLOCK 6 — Complete Types and SQL (Phases 20-21)

### Phase 20 — Types + import/export `⏳` week 47-48
- [ ] 20.1 ⏳ Regular views — `CREATE VIEW` and updatable views
- [ ] 20.2 ⏳ Sequences — `CREATE SEQUENCE`, `NEXTVAL`, `CURRVAL`
- [ ] 20.3 ⏳ ENUMs — `CREATE TYPE ... AS ENUM` with validation and semantic order
- [ ] 20.4 ⏳ Arrays — `TEXT[]`, `FLOAT[]`, `ANY()`, `@>`
- [ ] 20.5 ⏳ COPY FROM/TO — import/export CSV, JSON, JSONL
- [ ] 20.6 ⏳ Parquet — direct `READ_PARQUET()` + export with `crate parquet`
- [ ] 20.7 ⏳ Incremental backup — diff from last backup + full restore
- [ ] 20.8 ⏳ COPY streaming — import CSV/JSON line-by-line without loading into memory; support files >RAM
- [ ] 20.9 ⏳ Parquet write — export query result to Parquet with Snappy/Zstd compression; useful for data pipelines

### Phase 21 — Advanced SQL `⏳` week 49-51
- [ ] 21.1 ⏳ Savepoints — `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`
- [ ] 21.2 ⏳ CTEs — `WITH` queries
- [ ] 21.3 ⏳ Recursive CTEs — `WITH RECURSIVE` for trees and hierarchies
- [ ] 21.4 ⏳ RETURNING — in INSERT, UPDATE, DELETE
- [ ] 21.5 ⏳ MERGE / UPSERT — `ON CONFLICT DO UPDATE` + standard `MERGE`
- [ ] 21.6 ⏳ CHECK constraints + DOMAIN types
- [ ] 21.7 ⏳ TEMP and UNLOGGED tables
- [ ] 21.8 ⏳ Expression indexes — `CREATE INDEX ON users(LOWER(email))`
- [ ] 21.9 ⏳ LATERAL joins
- [ ] 21.10 ⏳ Cursors — `DECLARE`, `FETCH`, `CLOSE`
- [ ] 21.11 ⏳ Query hints — `/*+ INDEX() HASH_JOIN() PARALLEL() */`
- [ ] 21.12 ⏳ DISTINCT ON — first row per group `SELECT DISTINCT ON (user_id) *`
- [ ] 21.13 ⏳ NULLS FIRST / NULLS LAST — `ORDER BY price ASC NULLS LAST`
- [ ] 21.14 ⏳ CREATE TABLE AS SELECT — create table from query result
- [ ] 21.15 ⏳ CREATE TABLE LIKE — clone structure from another table
- [ ] 21.16 ⏳ DEFERRABLE constraints — `DEFERRABLE INITIALLY DEFERRED/IMMEDIATE`; buffer of pending violations per transaction; verify all on COMMIT; full rollback if any fail; prerequisite for bulk imports without FK ordering
- [ ] 21.17 ⏳ IS DISTINCT FROM / IS NOT DISTINCT FROM — NULL-safe comparison (1 IS DISTINCT FROM NULL → true)
- [ ] 21.18 ⏳ NATURAL JOIN — automatic join on columns with the same name
- [ ] 21.19 ⏳ FETCH FIRST n ROWS ONLY / OFFSET n ROWS — standard SQL alias for LIMIT
- [ ] 21.20 ⏳ CHECKPOINT — force WAL write to disk manually
- [ ] 21.21 ⏳ GROUPING SETS / ROLLUP / CUBE — aggregate multiple GROUP BY levels in a single query
- [ ] 21.22 ⏳ VALUES as inline table — `SELECT * FROM (VALUES (1,'a'), (2,'b')) AS t(id, name)`
- [ ] 21.23 ⏳ Advanced SQL tests — suite covering CTE, window functions, MERGE, savepoints, cursors
- [ ] 21.24 ⏳ ORM compatibility tier 2 — Prisma and ActiveRecord connect; migrations with RETURNING, GENERATED IDENTITY and deferred FK; document incompatibilities

---

## BLOCK 7 — Product Features (Phases 22-23)

### Phase 22 — Vector search + advanced search `⏳` week 52-54
- [ ] 22.1 ⏳ Vector similarity — `VECTOR(n)`, operators `<=>`, `<->`, `<#>`
- [ ] 22.2 ⏳ HNSW index — `CREATE INDEX USING hnsw(col vector_cosine_ops)`
- [ ] 22.3 ⏳ Fuzzy search — `SIMILARITY()`, trigrams, `LEVENSHTEIN()`
- [ ] 22.4 ⏳ ANN benchmarks — compare HNSW vs pgvector vs FAISS on recall@10 and QPS; document quality/speed tradeoff
- [ ] 22.5 ⏳ IVFFlat alternative index — lower RAM option than HNSW for collections >10M vectors

### Phase 22b — Platform features `⏳` week 55-57
- [ ] 22b.1 ⏳ Scheduled jobs — `cron_schedule()` with `tokio-cron-scheduler`
- [ ] 22b.2 ⏳ Foreign Data Wrappers — HTTP + PostgreSQL as external sources
- [ ] 22b.3 ⏳ Multi-database — `CREATE DATABASE`, `USE`, cross-db queries
- [ ] 22b.4 ⏳ Schema namespacing — `CREATE SCHEMA`, `schema.table`
- [ ] 22b.5 ⏳ Schema migrations CLI — `dbyo migrate up/down/status`
- [ ] 22b.6 ⏳ FDW pushdown — push SQL predicates to remote origin when possible; avoid fetching unnecessary rows

### Phase 22c — Native GraphQL API `⏳` week 58-60
- [ ] 22c.1 ⏳ GraphQL server on port `:3308` — schema auto-discovered from catalog
- [ ] 22c.2 ⏳ GraphQL queries and mutations — mapped to point lookups and range scans on B+ Tree
- [ ] 22c.3 ⏳ GraphQL subscriptions — WAL as event stream, WebSocket, no polling
- [ ] 22c.4 ⏳ GraphQL DataLoader — automatic batch loading, eliminates N+1 problem
- [ ] 22c.5 ⏳ GraphQL introspection — full schema for Apollo Studio, Postman, codegen
- [ ] 22c.6 ⏳ GraphQL persisted queries — pre-registered query hash; avoids transmitting the full document in production
- [ ] 22c.7 ⏳ GraphQL end-to-end tests — queries, mutations, subscriptions with real client (gqlgen/graphql-request)

### Phase 22d — Native OData v4 `⏳` week 61-63
- [ ] 22d.1 ⏳ HTTP endpoint `:3309` — compatible with PowerBI, Excel, Tableau, SAP without drivers
- [ ] 22d.2 ⏳ OData `$metadata` — EDMX document auto-discovered from catalog (PowerBI consumes it on connect)
- [ ] 22d.3 ⏳ OData queries — `$filter`, `$select`, `$orderby`, `$top`, `$skip`, `$count` mapped to SQL
- [ ] 22d.4 ⏳ OData `$expand` — JOINs by FK: `/odata/orders?$expand=customer` without manual SQL
- [ ] 22d.5 ⏳ OData batch requests — multiple operations in a single HTTP request (`$batch`)
- [ ] 22d.6 ⏳ OData authentication — Bearer token + Basic Auth for enterprise connectors
- [ ] 22d.7 ⏳ OData end-to-end tests — connect real Excel/PowerBI + automated $filter/$expand/$batch suite

### Phase 23 — Backwards compatibility `⏳` week 64-66
- [ ] 23.1 ⏳ Native SQLite reader — parse binary `.db`/`.sqlite` format
- [ ] 23.2 ⏳ ATTACH sqlite — `ATTACH 'file.sqlite' AS src USING sqlite`
- [ ] 23.3 ⏳ Migrate from MySQL — `dbyo migrate from-mysql` with `mysql_async`
- [ ] 23.4 ⏳ Migrate from PostgreSQL — `dbyo migrate from-postgres` with `tokio-postgres`
- [ ] 23.5 ⏳ PostgreSQL wire protocol — port 5432, psql and psycopg2 connect
- [ ] 23.6 ⏳ Both protocols simultaneously — :3306 MySQL + :5432 PostgreSQL
- [ ] 23.7 ⏳ ORM compatibility tests — Django ORM, SQLAlchemy, ActiveRecord, Prisma connect without changes
- [ ] 23.8 ⏳ Dump / restore compatibility — read dumps from `mysqldump` and `pg_dump --format=plain`
- [ ] 23.9 ⏳ ORM compatibility tier 3 — Typeorm (async), psycopg3 (Python), SQLx (Rust compile-time) connect; benchmark queries/s vs native PostgreSQL

---

> **🏁 PRODUCTION-READY CHECKPOINT — week ~67**
> On completing Phase 23, NexusDB must be able to:
> - MySQL + PostgreSQL wire protocols simultaneously
> - All major ORMs (Django, SQLAlchemy, Prisma, ActiveRecord, Typeorm, psycopg3)
> - Schema migrations with standard tools (Alembic, Rails migrate, Prisma migrate)
> - Import existing DBs from MySQL/PostgreSQL/SQLite
> - Full observability (metrics, logs, EXPLAIN ANALYZE in JSON)
>
> **ORM target at this point:** all tier 3 ORMs without workarounds.

---

## BLOCK 8 — Complete Type System (Phases 24-26)

### Phase 24 — Complete types `⏳` week 67-69
- [ ] 24.1 ⏳ Integers: TINYINT, SMALLINT, BIGINT, HUGEINT + U variants
- [ ] 24.1b ⏳ SERIAL / BIGSERIAL — convenient auto-increment types (INT + SEQUENCE + DEFAULT)
- [ ] 24.1c ⏳ GENERATED ALWAYS AS IDENTITY — modern SQL standard for auto-increment
- [ ] 24.2 ⏳ REAL/FLOAT4 separate from DOUBLE — `f32` vs `f64`
- [ ] 24.3 ⏳ Exact DECIMAL — `rust_decimal` with fast path `i64+scale`
- [ ] 24.4 ⏳ CITEXT — automatic case-insensitive comparisons
- [ ] 24.5 ⏳ BYTEA/BLOB — binary with automatic TOAST
- [ ] 24.6 ⏳ BIT(n) / VARBIT(n) — bit strings with `bitvec`
- [ ] 24.7 ⏳ TIMESTAMPTZ — always UTC internally, convert on display
- [ ] 24.8 ⏳ INTERVAL — months/days/µs separated with calendar arithmetic
- [ ] 24.9 ⏳ UUID v4/v7 — `[u8;16]`, v7 sortable for PKs
- [ ] 24.10 ⏳ INET, CIDR, MACADDR — network types with operators
- [ ] 24.11 ⏳ RANGE(T) — `int4range`, `daterange`, `tsrange` with `@>` and `&&`
- [ ] 24.12 ⏳ COMPOSITE types — `CREATE TYPE ... AS (fields)`
- [ ] 24.13 ⏳ Domain types — `CREATE DOMAIN email AS TEXT CHECK (VALUE ~ '^.+@.+$')` with constraint inheritance
- [ ] 24.14 ⏳ Complete type tests — coercion, overflow, DECIMAL precision, timezone conversions

### Phase 25 — Type optimizations `⏳` week 70-72
- [ ] 25.1 ⏳ VarInt encoding — 1-9 byte integers by value + zigzag for negatives
- [ ] 25.2 ⏳ Binary JSONB — offset table for O(log k) access without parsing
- [ ] 25.3 ⏳ VECTOR quantization — f16 (2x savings) and int8 (4x savings)
- [ ] 25.4 ⏳ PAX layout — columnar within each 8KB page
- [ ] 25.5 ⏳ Per-column statistics — histogram, correlation, most_common
- [ ] 25.6 ⏳ ANALYZE — update statistics manually and automatically
- [ ] 25.7 ⏳ Zero-copy rkyv — B+ Tree nodes without deserializing from mmap
- [ ] 25.8 ⏳ Compression by type — Delta, BitPack, LZ4, ZSTD by column
- [ ] 25.9 ⏳ Encoding benchmarks — compare VarInt vs fixed, PAX vs NSM, zero-copy vs deserialize

### Phase 26 — Full collation `⏳` week 73-75
- [ ] 26.1 ⏳ CollationEngine with ICU4X — Primary/Secondary/Tertiary levels
- [ ] 26.2 ⏳ _ci / _cs / _ai / _as / _bin suffixes per column
- [ ] 26.3 ⏳ Cascading configuration — server → DB → table → column → query
- [ ] 26.4 ⏳ Unicode Normalization — NFC on save, NFKC for search
- [ ] 26.5 ⏳ Sort keys in B+ Tree — correct `memcmp` with collation
- [ ] 26.6 ⏳ Locale-aware UPPER/LOWER — `icu_casemap`, not simple ASCII
- [ ] 26.7 ⏳ LENGTH in codepoints — not in bytes
- [ ] 26.8 ⏳ LIKE respects collation — `jos%` finds `José González`
- [ ] 26.9 ⏳ Legacy encodings — latin1, utf16 with conversion via `encoding_rs`
- [ ] 26.10 ⏳ ~20 configured collations — es_419, en_US, pt_BR, fr_FR, ar...
- [ ] 26.11 ⏳ Collation overhead benchmark — cost of ICU4X vs simple memcmp; document when full collation is worth it

---

## BLOCK 9 — Professional SQL (Phases 27-30)

### Phase 27 — Real Query Optimizer `⏳` week 76-78
- [ ] 27.1 ⏳ Join ordering — dynamic programming, 2^N subsets
- [ ] 27.2 ⏳ Predicate pushdown — move filters close to the data
- [ ] 27.3 ⏳ Subquery unnesting — convert correlated subqueries to JOINs
- [ ] 27.4 ⏳ Join elimination — FK guarantees uniqueness, remove unnecessary JOIN
- [ ] 27.5 ⏳ Cardinality estimation — histograms + column correlations
- [ ] 27.6 ⏳ Calibrated cost model — seq_page_cost, random_page_cost
- [ ] 27.7 ⏳ Parallel query planning — split plan into sub-plans executable in Rayon from the optimizer
- [ ] 27.8 ⏳ Plan caching and reuse — reuse plan for structurally identical queries (prepared statements)
- [ ] 27.9 ⏳ Optimizer benchmarks — measure planning time vs plan quality with TPC-H queries
- [ ] 27.10 ⏳ Adaptive cardinality estimation — correct estimations at end of execution with real statistics; update histograms automatically; avoid bad plans on repeated queries
- [ ] 27.11 ⏳ OR-to-UNION rewrite — `WHERE a=1 OR b=2` → `SELECT WHERE a=1 UNION SELECT WHERE b=2`; allows using two different indexes vs full scan

### Phase 28 — SQL completeness `⏳` week 79-81
- [ ] 28.1 ⏳ Isolation levels — READ COMMITTED, REPEATABLE READ, SERIALIZABLE (SSI)
- [ ] 28.2 ⏳ SELECT FOR UPDATE / FOR SHARE / SKIP LOCKED / NOWAIT
- [ ] 28.3 ⏳ LOCK TABLE — ACCESS SHARE, ROW EXCLUSIVE, ACCESS EXCLUSIVE modes
- [ ] 28.4 ⏳ Advisory locks — `pg_advisory_lock` / `pg_try_advisory_lock`
- [ ] 28.5 ⏳ UNION / UNION ALL / INTERSECT / EXCEPT
- [ ] 28.6 ⏳ EXISTS / NOT EXISTS / IN subquery / correlated subqueries
- [ ] 28.7 ⏳ Simple and searched CASE — in SELECT, WHERE, ORDER BY
- [ ] 28.8 ⏳ TABLESAMPLE SYSTEM and BERNOULLI with REPEATABLE
- [ ] 28.9 ⏳ Serializable Snapshot Isolation (SSI) — write-read dependency graph between transactions; DFS to detect cycles; automatic rollback of the youngest transaction on cycle detection; prerequisite: 7.1 (MVCC visibility)
- [ ] 28.10 ⏳ Isolation level tests — dirty read, non-repeatable read, phantom read; each test uses real concurrent transactions; verify that each level prevents exactly what it should and no more
- [ ] 28.11 ⏳ SELECT FOR UPDATE / FOR SHARE with skip locked — required by job queues (Celery, Sidekiq, Resque); without this feature task ORMs do not work

### Phase 29 — Complete functions `⏳` week 82-84
- [ ] 29.1 ⏳ Advanced aggregations — `STRING_AGG`, `ARRAY_AGG`, `JSON_AGG`
- [ ] 29.2 ⏳ Statistical aggregations — `PERCENTILE_CONT`, `MODE`, `FILTER`
- [ ] 29.3 ⏳ Complete window functions — `NTILE`, `PERCENT_RANK`, `CUME_DIST`, `FIRST_VALUE`
- [ ] 29.4 ⏳ Text functions — `REGEXP_*`, `LPAD`, `RPAD`, `FORMAT`, `TRANSLATE`
- [ ] 29.5 ⏳ Date functions — `AT TIME ZONE`, `AGE`, `TO_CHAR`, `TO_DATE`
- [ ] 29.6 ⏳ Timezone database — embedded tzdata, portable without depending on the OS
- [ ] 29.7 ⏳ Math functions — trigonometry, logarithms, `GCD`, `RANDOM`
- [ ] 29.8 ⏳ COALESCE / NULLIF / GREATEST / LEAST — basic comparison functions
- [ ] 29.9 ⏳ GENERATE_SERIES — numeric and date sequence generator
- [ ] 29.10 ⏳ UNNEST — expand array to individual rows
- [ ] 29.11 ⏳ ARRAY_TO_STRING / STRING_TO_ARRAY — array ↔ text conversion
- [ ] 29.12 ⏳ JSON_OBJECT / JSON_ARRAY / JSON_BUILD_OBJECT — JSON constructors
- [ ] 29.13 ⏳ WIDTH_BUCKET — assign values to buckets for histograms
- [ ] 29.14 ⏳ TRIM LEADING/TRAILING/BOTH — `TRIM(LEADING ' ' FROM str)`
- [ ] 29.15 ⏳ pg_sleep(n) — pause N seconds (useful for tests and simulations)
- [ ] 29.16 ⏳ COPY binary protocol — bulk load in binary format (faster than CSV)
- [ ] 29.17 ⏳ Network functions — `HOST()`, `NETWORK()`, `BROADCAST()`, `MASKLEN()` for INET/CIDR types
- [ ] 29.18 ⏳ Function tests — suite covering all function types: text, date, math, JSON, array

### Phase 30 — Pro infrastructure `⏳` week 85-87
- [ ] 30.1 ⏳ GIN indexes — for arrays, JSONB and trigrams
- [ ] 30.2 ⏳ GiST indexes — for ranges and geometry
- [ ] 30.3 ⏳ BRIN indexes — huge tables with ordered data, minimum space
- [ ] 30.4 ⏳ Hash indexes — O(1) for exact equality
- [ ] 30.5 ⏳ CREATE INDEX CONCURRENTLY — without blocking writes
- [ ] 30.6 ⏳ Complete information_schema — tables, columns, constraints
- [ ] 30.7 ⏳ Basic pg_catalog — pg_class, pg_attribute, pg_index
- [ ] 30.8 ⏳ DESCRIBE / SHOW TABLES / SHOW CREATE TABLE
- [ ] 30.9 ⏳ Two-phase commit — `PREPARE TRANSACTION` / `COMMIT PREPARED`
- [ ] 30.10 ⏳ DDL Triggers — `CREATE EVENT TRIGGER ON ddl_command_end`
- [ ] 30.11 ⏳ TABLESPACES — `CREATE TABLESPACE`, tiered storage
- [ ] 30.12 ⏳ NOT VALID + VALIDATE CONSTRAINT — constraints without downtime
- [ ] 30.13 ⏳ GUC — `SET/SHOW/ALTER SYSTEM`, dynamic configuration
- [ ] 30.14 ⏳ Native R-Tree index — for geospatial types and multidimensional ranges (complements GiST from 30.2)
- [ ] 30.15 ⏳ Alternative index benchmarks — GIN/GiST/BRIN/Hash vs B+ Tree on specific workloads

---

## BLOCK 10 — Final Features and AI (Phases 31-34)

### Phase 31 — Final features `⏳` week 88-90
- [ ] 31.1 ⏳ Encryption at rest — AES-256-GCM per page
- [ ] 31.2 ⏳ Data masking — `MASK_EMAIL()`, `MASK_PHONE()`, policies per role
- [ ] 31.3 ⏳ PREPARE / EXECUTE — compiled and reusable plan
- [ ] 31.4 ⏳ Extended statistics — column correlations (`CREATE STATISTICS`)
- [ ] 31.5 ⏳ FULL OUTER JOIN
- [ ] 31.6 ⏳ Custom aggregates — `CREATE AGGREGATE MEDIAN(...)`
- [ ] 31.7 ⏳ Geospatial — `POINT`, `ST_DISTANCE_KM`, R-Tree index (`rstar`)
- [ ] 31.8 ⏳ Query result cache — automatic invalidation by table
- [ ] 31.9 ⏳ Strict mode — no silent coercion, errors on truncation
- [ ] 31.10 ⏳ Logical replication — `CREATE PUBLICATION` + `CREATE SUBSCRIPTION`
- [ ] 31.11 ⏳ mTLS + pg_hba.conf equivalent
- [ ] 31.12 ⏳ Connection string DSN — `dbyo://user:pass@host:port/dbname?param=val`; `postgres://` and `mysql://` as aliases
- [ ] 31.13 ⏳ Read replicas routing — automatically route read-only queries to replicas from the connection pool

### Phase 32 — Final architecture `⏳` week 91-93
- [ ] 32.1 ⏳ Complete workspace refactor — 18+ specialized crates
- [ ] 32.2 ⏳ Interchangeable StorageEngine trait — Mmap, Memory, Encrypted, Fault
- [ ] 32.3 ⏳ Interchangeable Index trait — BTree, Hash, Gin, Gist, Brin, Hnsw, Fts
- [ ] 32.4 ⏳ Central engine with complete pipeline — cache→parse→rbac→plan→opt→exec→audit
- [ ] 32.5 ⏳ WAL as event bus — replication, CDC, cache, triggers, audit
- [ ] 32.6 ⏳ Release profiles — LTO fat, codegen-units=1, panic=abort
- [ ] 32.7 ⏳ CI/CD — GitHub Actions with test + clippy + bench on each PR
- [ ] 32.8 ⏳ Stable plugin API — version public API with semver; ABI guarantees for extensions
- [ ] 32.9 ⏳ Regression test suite — reproduce historical bugs; safety net for the final refactor

### Phase 33 — AI embeddings + hybrid search `⏳` week 94-99
- [ ] 33.1 ⏳ AI_EMBED() — local Ollama (primary) + OpenAI (fallback) + cache
- [ ] 33.2 ⏳ VECTOR GENERATED ALWAYS AS (AI_EMBED(col)) STORED
- [ ] 33.3 ⏳ Hybrid search — BM25 + HNSW + RRF in a single query
- [ ] 33.4 ⏳ Re-ranking — cross-encoder for more accurate results

### Phase 33b — AI functions `⏳` week 100-101
- [ ] 33b.1 ⏳ AI_CLASSIFY(), AI_EXTRACT(), AI_SUMMARIZE(), AI_TRANSLATE()
- [ ] 33b.2 ⏳ AI_DETECT_PII() + AI_MASK_PII() — automatic privacy
- [ ] 33b.3 ⏳ AI function tests — deterministic mocks of Ollama/OpenAI for CI; verify latency and fallback
- [ ] 33b.4 ⏳ AI function rate limiting — throttle calls to the external model; token budget per role/session

### Phase 33c — RAG + Model Store `⏳` week 102-103
- [ ] 33c.1 ⏳ RAG Pipeline — `CREATE RAG PIPELINE` + `RAG_QUERY()`
- [ ] 33c.2 ⏳ Feature Store — `CREATE FEATURE GROUP` + point-in-time correct
- [ ] 33c.3 ⏳ Model Store ONNX — `CREATE MODEL` + `PREDICT()` + `PREDICT_AB()`
- [ ] 33c.4 ⏳ RAG evaluation — precision/recall metrics of RAG pipeline; compare with BM25 search baseline

### Phase 33d — AI intelligence + privacy `⏳` week 104-106
- [ ] 33d.1 ⏳ Adaptive indexing — automatic index suggestions based on query history
- [ ] 33d.2 ⏳ Text-to-SQL — `NL_QUERY()`, `NL_TO_SQL()`, `NL_EXPLAIN()`
- [ ] 33d.3 ⏳ Anomaly detection — `ANOMALY_SCORE()` + `CREATE ANOMALY DETECTOR`
- [ ] 33d.4 ⏳ Differential privacy — `DP_COUNT`, `DP_AVG` with budget per role
- [ ] 33d.5 ⏳ Data lineage — `DATA_LINEAGE()` + GDPR Right to be Forgotten

### Phase 34 — Distributed infrastructure `⏳` week 107-110
- [ ] 34.1 ⏳ Sharding — `DISTRIBUTED BY HASH/RANGE/LIST` across N nodes
- [ ] 34.2 ⏳ Scatter-gather — execute plan on shards in parallel + merge
- [ ] 34.3 ⏳ Shard rebalancing — without downtime
- [ ] 34.4 ⏳ Logical decoding API — `pg_logical_slot_get_changes()` as JSON
- [ ] 34.5 ⏳ Standard DSN — `dbyo://`, `postgres://`, `DATABASE_URL` env var
- [ ] 34.6 ⏳ Extensions system — `CREATE EXTENSION` + `pg_available_extensions`
- [ ] 34.7 ⏳ WASM extensions — `CREATE EXTENSION FROM FILE '*.wasm'`
- [ ] 34.8 ⏳ VACUUM FREEZE — prevent Transaction ID Wraparound
- [ ] 34.9 ⏳ Parallel DDL — `CREATE TABLE AS SELECT WITH PARALLEL N`
- [ ] 34.10 ⏳ pgbench equivalent — `dbyo-bench` with standard OLTP scenarios
- [ ] 34.11 ⏳ Final benchmarks — full comparison vs MySQL, PostgreSQL, SQLite, DuckDB
- [ ] 34.12 ⏳ Consensus protocol (basic Raft) — for automatic failover in cluster; replaces manual failover from 18.10
- [ ] 34.13 ⏳ Distributed transactions — two-phase commit between shards; cross-shard consistency

### Phase 35 — Deployment and DevEx `⏳` week 111-113
- [ ] 35.1 ⏳ Multi-stage Dockerfile — Rust builder + debian-slim runtime
- [ ] 35.2 ⏳ docker-compose.yml — complete setup with volumes and env vars
- [ ] 35.3 ⏳ systemd service file — `dbyo.service` for Linux production
- [ ] 35.4 ⏳ Complete dbyo.toml — network, storage, logging, AI, TLS configuration
- [ ] 35.5 ⏳ Log levels and rotation — trace/debug/info/warn/error + daily/size rotation
- [ ] 35.6 ⏳ dbyo-client crate — official Rust SDK with connection pool
- [ ] 35.7 ⏳ Python package — `pip install dbyo-python` with psycopg2-style API
- [ ] 35.8 ⏳ Homebrew formula — `brew install dbyo` for macOS
- [ ] 35.9 ⏳ GitHub Actions CI — test + clippy + bench + fuzz on each PR
- [ ] 35.10 ⏳ Performance tuning guide — which parameters to adjust for each workload
- [ ] 35.11 ⏳ Kubernetes operator — `NexusDBCluster` CRD with replica management and auto-scaling
- [ ] 35.12 ⏳ Helm chart — K8s deployment with production defaults
- [ ] 35.13 ⏳ TPC-H production benchmark — run full TPC-H and publish results; public reference point
- [ ] 35.14 ⏳ Public API documentation — complete reference of SQL dialect, wire protocol extensions, C FFI, configuration; auto-generated from code + hand-written where needed
- [ ] 35.15 ⏳ External security audit — review attack surfaces before release: SQL injection, auth bypass, path traversal in COPY, buffer overflows in parser; use `cargo-audit` + manual review of unsafe

---

> **🏁 FEATURE-COMPLETE CHECKPOINT — week ~113**
> On completing Phase 35, NexusDB is a complete production database engine:
> - MySQL + PostgreSQL + OData + GraphQL simultaneously
> - AI-native (embeddings, hybrid search, RAG)
> - Horizontal distribution (sharding + Raft)
> - Deploy on Docker/K8s/systemd
> - Complete documentation and TPC-H published
