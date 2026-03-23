# Progress — AxiomDB Database Engine

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
- [x] 1.9 ✅ Error logging from startup — `tracing_subscriber::fmt()` with `EnvFilter` in `axiomdb-server/main.rs`; `tracing::{info,debug,warn}` in `MmapStorage` (create, open, grow, drop)

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

### Phase 3 — WAL and transactions `✅` week 5-10
- [x] 3.1 ✅ WAL entry format — `[LSN|Type|Table|Key|Old|New|CRC]` + backward scan
- [x] 3.2 ✅ WalWriter — append-only, global LSN, fsync on commit, open() with scan_last_lsn
- [x] 3.3 ✅ WalReader — scan_forward(from_lsn) streaming + scan_backward() with entry_len_2
- [x] 3.4 ✅ RowHeader — `struct RowHeader { txn_id_created, txn_id_deleted, row_version, _flags }` + slotted heap pages + TransactionSnapshot
- [x] 3.5 ✅ BEGIN / COMMIT / ROLLBACK — TxnManager with WAL + undo log; autocommit wrapper
- [x] 3.6 ✅ WAL Checkpoint — flush + Checkpoint WAL entry + checkpoint_lsn in meta page
- [x] 3.7 ✅ WAL Rotation — header v2 + start_lsn + WalRotator; max_wal_size trigger
- [x] 3.8 ✅ Crash Recovery — undo in-progress txns + physical location encoding; CRASHED→READY state machine
- [x] 3.9 ✅ Post-recovery integrity checker — heap structural + MVCC checks
- [x] 3.10 ✅ Durability tests — 9 crash + recovery scenarios with MmapStorage real I/O
- [x] 3.11 ✅ Catalog bootstrap — meta page extension + schema types (TableDef/ColumnDef/IndexDef) + CatalogBootstrap
- [x] 3.12 ✅ CatalogReader/Writer — HeapChain multi-page + ID sequences + WAL-logged DDL + MVCC snapshots
- [x] 3.13 ✅ Catalog change notifier — SchemaChangeKind/Event/Listener trait + CatalogChangeNotifier + CatalogWriter::with_notifier
- [x] 3.14 ✅ Schema binding — SchemaResolver: resolve_table/column/table_exists with default schema + MVCC
- [x] 3.15 ✅ Page dirty tracker — PageDirtyTracker in MmapStorage; mark on write/alloc, clear on flush
- [x] 3.16 ✅ Basic configuration — DbConfig from axiomdb.toml (serde+toml); safe defaults; partial TOML accepted
- [ ] ⚠️ 3.5a Autocommit mode (`SET autocommit=0`) → Phase 5 (session state, wire protocol)
- [ ] ⚠️ 3.5b Implicit transaction start (MySQL ORM compat) → Phase 5
- [ ] ⚠️ 3.5c Error semantics mid-transaction (statement vs txn rollback) → Phase 4.25
- [ ] ⚠️ 3.6b ENOSPC handling — graceful shutdown on disk full → Phase 5
- [ ] ⚠️ 3.8b Partial page write detection on open → deferred to Phase 5
- [ ] ⚠️ Per-page msync optimization (flush_range) → deferred pending profiling

### Phase 4 — SQL Parser + Executor `🔄` week 11-25
<!--
  DEPENDENCY ORDER (must be respected when planning subfases):

  Group A (foundations, no deps between them — can parallelize):
    4.0 Row codec  ←  4.17 Expression evaluator  ←  4.17b NULL semantics

  Group B (parser, parallel with Group A):
    4.1 AST  →  4.2 Lexer  →  4.2b sanitization
               4.2  →  4.3 (DDL: 4.3a, 4.3b, 4.3c, 4.3d)  →  4.4 DML

  Group C (semantic layer, needs B + catalog from Phase 3):
    4.18 semantic analyzer  →  4.18b type coercion  →  4.23 QueryResult type

  Group D (basic executor, needs A + B + C):
    4.5  →  4.5a  →  4.5b (table engine)  →  4.25 error handling  →  4.7 SQLSTATE

  Group E (core SQL, needs executor):
    4.8 JOIN  |  4.9a-d GROUP BY+agg  |  4.10-4.10d ORDER BY
    4.11 subqueries  |  4.12 DISTINCT  |  4.12b CAST  |  4.24 CASE WHEN  |  4.6 INSERT..SELECT

  Group F (functions, needs executor):
    4.13 system funcs  |  4.14 LAST_INSERT_ID  |  4.19 built-ins

  Group G (DevEx, parallel with E+F):
    4.15 CLI  |  4.15b DEBUG mode

  Group H (introspection + DDL, needs executor):
    4.20 SHOW TABLES  |  4.21 TRUNCATE  |  4.22 ALTER TABLE  |  4.22b ALTER CONSTRAINT

  Group I (validation, last):
    4.16 SQL tests suite  |  4.16b INSERT benchmark real I/O
-->

<!-- ── Group A — Foundations (no dependencies, can start immediately) ── -->
- [x] 4.0 ✅ Row codec — encode/decode `Value[]` ↔ bytes with null_bitmap; covers: BOOL, INT, BIGINT, REAL, DOUBLE, DECIMAL, TEXT, VARCHAR, DATE, TIMESTAMP, NULL
- [x] 4.17 ✅ Expression evaluator — evaluation tree for arithmetic (`+`,`-`,`*`,`/`), booleans (`AND`,`OR`,`NOT`), comparisons (`=`,`<`,`>`), `LIKE`, `BETWEEN`, `IN (list)`, `IS NULL`; **prerequisite for 4.5 — must come before the executor**
- [x] 4.17b ✅ Systematic NULL semantics — `NULL+1=NULL`, `NULL=NULL→UNKNOWN`, `NULL IN(1,2)=NULL`; 3-valued logic (TRUE/FALSE/UNKNOWN); `IS NULL` vs `= NULL`; without this, aggregation queries produce silent wrong results; **prerequisite for 4.5**

<!-- ── Group B — Parser (parallel with Group A) ── -->
- [x] 4.1 ✅ AST definitions — syntax tree types (Expr, Stmt, TableRef, ColumnDef nodes)
- [x] 4.2 ✅ Lexer/Tokenizer — logos DFA, ~85 tokens, zero-copy &'src str identifiers
- [x] 4.2b ✅ Input sanitization in parser — malformed SQL → clear SQL error, never `panic`; configurable `max_query_size`; fuzz-test immediately after implementation
- [x] 4.3 ✅ DDL Parser — `CREATE TABLE`, `CREATE INDEX`, `DROP TABLE`, `DROP INDEX`
- [x] 4.3a ✅ Column constraints in DDL — `NOT NULL`, `DEFAULT expr`, `UNIQUE`, `PRIMARY KEY`, `REFERENCES fk`; parsed as part of `CREATE TABLE`
- [x] 4.3b ✅ Basic CHECK constraint in DDL — `CHECK (expr)` at column and table level; evaluated in INSERT/UPDATE
- [x] 4.3c ✅ AUTO_INCREMENT / SERIAL — `INT AUTO_INCREMENT` (MySQL) and `SERIAL` (PG-compat); internal sequence per table; `LAST_INSERT_ID()` returns last value
- [x] 4.3d ✅ Max identifier length — 64-char limit for table/column/index names; clear SQL error when exceeded
- [x] 4.4 ✅ DML Parser — `SELECT`, `INSERT`, `UPDATE`, `DELETE`

<!-- ── Group C — Semantic layer (needs Group B + Phase 3 catalog) ── -->
- [x] 4.18 ✅ Semantic analyzer — validate table/column existence against catalog (uses SchemaResolver from 3.14), resolve ambiguities, clear SQL error per violation; **prerequisite for 4.5**
- [x] 4.18b ✅ Type coercion matrix — rules for `'42'→INT`, `INT→BIGINT`, `DATE→TIMESTAMP`; MySQL-compatible permissive mode vs strict mode; errors on invalid conversions
- [x] 4.23 ✅ QueryResult type — unified executor return: `Rows{columns: Vec<ColumnMeta>, rows: Vec<Row>}` for SELECT, `Affected{count, last_insert_id}` for DML, `Empty` for DDL; basis for Phase 5 wire protocol serialization

<!-- ── Group D — Basic executor (needs Groups A + B + C) ── -->
- [x] 4.5 ✅ Basic executor — connects AST→semantic→storage: executes CREATE/DROP TABLE, INSERT, SELECT (with WHERE), UPDATE, DELETE; autocommit per statement
- [x] 4.5a ✅ SELECT without FROM — `SELECT 1`, `SELECT NOW()`; included in 4.5
- [x] 4.5b ✅ Table engine — row storage interface: `scan_table(snap)→RowIter`, `insert_row(values)→RecordId`, `delete_row(rid)`, `update_row(rid, values)`; wraps HeapChain + Row codec + catalog; used by the executor for all DML on heap tables
- [x] 4.25 ✅ Error handling framework — complete SQLSTATE mapping + ErrorResponse{sqlstate,severity,message,detail,hint} with hints for 15 variants
- [ ] 4.25b ⏳ Structured error responses — position (byte offset), offending value in UniqueViolation, JSON format via SET error_format='json'
- [ ] 4.25c ⏳ Strict mode always on + warnings system — requires session state (Phase 5)
- [x] 4.7 ✅ SQLSTATE codes — all DbError variants mapped; SQL-reachable errors have precise 5-char codes

<!-- ── Group E — Core SQL (needs executor) ── -->
- [x] 4.8 ✅ JOIN — INNER, LEFT, RIGHT, CROSS with nested loop; USING; multi-table; FULL → NotImplemented
- [x] 4.9a ✅ GROUP BY hash-based — HashMap<key_bytes, GroupState>; value_to_key_bytes; NULL keys group correctly
- [ ] 4.9b ⏳ GROUP BY sort-based — sort first, then stream; optimal when data is pre-sorted by index (deferred)
- [x] 4.9c ✅ Aggregate functions — COUNT(*), COUNT(col), SUM, MIN, MAX, AVG (→ Real); skip NULL; finalize
- [x] 4.9d ✅ HAVING clause — eval_with_aggs intercepts aggregate calls; representative_row for col refs
- [x] 4.10 ✅ ORDER BY + LIMIT/OFFSET — in-memory sort; stable sort_by; sort_err pattern
- [x] 4.10b ✅ Multi-column ORDER BY with mixed direction — composite comparator, left-to-right
- [x] 4.10c ✅ NULLS FIRST / NULLS LAST — ASC→NULLS LAST, DESC→NULLS FIRST (PG defaults); explicit override
- [ ] 4.10d ⏳ Parameterized LIMIT/OFFSET — `LIMIT $1 OFFSET $2` in prepared statements (deferred to Phase 5)
- [x] 4.11 ✅ Scalar subqueries — scalar `(SELECT ...)`, `IN (SELECT ...)`, `EXISTS/NOT EXISTS`, correlated subqueries, derived tables `FROM (SELECT ...)`, `SubqueryRunner` trait + `eval_with`; 14 integration tests
- [x] 4.12 ✅ DISTINCT — HashSet dedup on projected output rows; NULL=NULL for grouping; pre-LIMIT
- [x] 4.12b ✅ CAST + basic type coercion — explicit and implicit conversion between compatible types
- [x] 4.24 ✅ CASE WHEN — searched + simple form; NULL semantics; nested; SELECT/WHERE/ORDER BY/GROUP BY
- [x] 4.6 ✅ INSERT ... SELECT — execute_select + col_map + insert_row; MVCC prevents self-reads

<!-- ── Group F — Functions (needs executor) ── -->
- [x] 4.13 ✅ version() / current_user / session_user / current_database() — ORMs call these on connect; required for Phase 5 compatibility
- [x] 4.14 ✅ LAST_INSERT_ID() / lastval() — AUTO_INCREMENT execution + per-table thread-local sequence; ColumnDef.auto_increment flag (bit1 of existing flags byte); LAST_INSERT_ID()/lastval() in eval_function
- [x] 4.19 ✅ Basic built-in functions — `ABS`, `LENGTH`, `SUBSTR`, `UPPER`, `LOWER`, `TRIM`, `COALESCE`, `NOW()`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `ROUND`, `FLOOR`, `CEIL`
- [ ] 4.19b ⏳ BLOB functions — `FROM_BASE64(text)→BLOB` auto-decodes Base64 on insert (eliminates 33% overhead); `TO_BASE64(blob)→TEXT`; `OCTET_LENGTH(blob)→INT`; `ENCODE(blob,'hex'/'base64')→TEXT`; `DECODE(text,'hex'/'base64')→BLOB`; foundation for content-addressed storage in Phase 14
- [ ] 4.19c ⏳ UUID generation functions — `gen_random_uuid()` returns UUID v4; `uuid_generate_v7()` returns UUID v7 (time-ordered, better for B+Tree index locality); `IS_VALID_UUID(text)→BOOL`; nearly every modern app uses UUIDs as primary keys and the DB must be able to generate them server-side without depending on application code

<!-- ── Group G — DevEx (parallel with E+F) ── -->
- [ ] 4.15 ⏳ Interactive CLI — REPL like `sqlite3` shell; connects directly to storage
- [ ] 4.15b ⏳ DEBUG/VERBOSE mode — `--verbose` flag: log AST, chosen plan, execution stats per query; critical for Phases 4–10 development

<!-- ── Group H — Introspection + DDL modification (needs executor) ── -->
- [x] 4.20 ✅ SHOW TABLES / SHOW COLUMNS / DESCRIBE — parser + executor using CatalogReader; MySQL-compatible 6-column output; Extra shows auto_increment
- [x] 4.21 ✅ TRUNCATE TABLE — delete-all + AUTO_INCREMENT sequence reset; MySQL convention (returns count=0)
- [ ] 4.22 ⏳ Basic ALTER TABLE — `ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN`, `RENAME TABLE` (blocking, no concurrent); prerequisite for migrations
- [ ] 4.22b ⏳ ALTER TABLE ADD/DROP CONSTRAINT — `ADD CONSTRAINT fk`, `DROP CONSTRAINT`, `ADD UNIQUE`, `ADD CHECK`; ORMs need this post-creation

<!-- ── Group I — Validation (last, closes the phase) ── -->
- [ ] 4.16 ⏳ SQL tests — full suite: DDL + DML + JOIN + GROUP BY + ORDER BY + subqueries; covers error cases and edge conditions
- [ ] 4.16b ⏳ INSERT throughput benchmark (MmapStorage + WAL real) — validate 180K ops/s budget with real I/O; **NEW — gap identified in Phase 3 benchmark review; blocker for closing Phase 4**

### Phase 5 — MySQL Wire Protocol `⏳` week 26-30
- [ ] 5.1 ⏳ TCP listener with Tokio — accept connections on :3306
- [ ] 5.2 ⏳ MySQL handshake — Server Greeting + Client Response
- [ ] 5.2a ⏳ Charset/collation negotiation in handshake — `character_set_client`, `character_set_results`, `collation_connection` sent in Server Greeting; client chooses charset; without this modern MySQL clients cannot connect or display incorrect characters
- [ ] 5.2c ⏳ ON_ERROR session behavior — `SET ON_ERROR = 'rollback_statement'` rolls back only the failing statement and continues the transaction (fixes the PostgreSQL silent-abort problem where developers lose all previous work); `SET ON_ERROR = 'rollback_transaction'` is PostgreSQL-standard behavior; `SET ON_ERROR = 'savepoint'` automatically creates a savepoint before each statement so errors can be recovered without losing the whole transaction; `SET ON_ERROR = 'ignore'` matches MySQL lenient behavior; solves the real production pain of `BEGIN ... multiple statements ... one fails ... COMMIT does ROLLBACK without warning`
- [ ] 5.2b ⏳ Session-level collation and compat mode — `SET collation = 'es'` changes default sort for the session; `SET AXIOM_COMPAT = 'mysql'` makes the session behave like MySQL (CI+AI default); `SET AXIOM_COMPAT = 'postgresql'` for PG behavior; these settings propagate to all subsequent queries in the session and are reset on reconnect; foundation for the per-database compat mode in Phase 13
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
- [ ] 7.11 ⏳ Basic MVCC vacuum — purge dead row versions (txn_id_deleted < oldest_active_snapshot); frees space without blocking reads; aggressive autovacuum defaults out-of-the-box (unlike PostgreSQL where bad defaults cause table bloat in production); `SELECT * FROM axiom_bloat` system view shows: table_name, live_rows, dead_rows, bloat_ratio, last_vacuum_at, estimated_space_wasted; when bloat_ratio > 20% the engine logs a recommendation; no manual tuning required for the common case
- [ ] 7.12 ⏳ Basic savepoints — `SAVEPOINT sp1`, `ROLLBACK TO sp1`, `RELEASE sp1`; ORMs use them for partial errors in long transactions
- [ ] 7.13 ⏳ Isolation tests — verify READ COMMITTED and REPEATABLE READ with concurrent transactions; test dirty reads, non-repeatable reads, phantom reads; use real concurrent transactions (not mocks)
- [ ] 7.14 ⏳ Cascading rollback prevention — if txn A aborts and txn B read data from A (dirty read), B must also abort; verify that READ COMMITTED prevents this structurally
- [ ] 7.15 ⏳ Basic transaction ID overflow prevention — `txn_id` is u64; log warning at 50% and 90% of capacity; plan for VACUUM FREEZE (complete in Phase 34) but detection must be early
- [ ] 7.16 ⏳ Historical reads — `BEGIN READ ONLY AS OF TIMESTAMP '2023-12-31 23:59:59'` anchors the snapshot to a past point; MVCC already has the data, this adds the SQL syntax and executor support; critical for auditing financial data at a specific date without exporting first
- [ ] 7.17 ⏳ Optimistic locking / Compare-And-Swap — `UPDATE t SET qty=qty-1, ver=ver+1 WHERE id=? AND ver=? AND qty>0` returns 0 rows if version changed; explicit `SELECT ... FOR UPDATE SKIP LOCKED` for job queues; gaming inventory, e-commerce checkout, concurrent reservations all depend on this pattern without full serializable isolation
- [ ] 7.18 ⏳ Cancel / kill query — `SELECT axiom_cancel_query(pid)` sends a cancellation signal to a running query (like `pg_cancel_backend`); `axiom_terminate_session(pid)` forcibly closes a connection; without this, a long-running accidental `SELECT * FROM logs` (millions of rows) has no way to be stopped from another session without restarting the server
- [ ] 7.19 ⏳ Lock contention visibility — `SELECT * FROM axiom_lock_waits` system view shows: waiting_pid, blocking_pid, waiting_query, lock_type, wait_duration; `SELECT * FROM axiom_locks` shows all currently held locks; essential for diagnosing deadlocks and performance problems in production without guessing
- [ ] 7.20 ⏳ Autonomous transactions — `PRAGMA AUTONOMOUS_TRANSACTION` on a stored procedure (Phase 16.7) makes it run in an independent transaction; `COMMIT` inside the autonomous procedure commits only that procedure's changes, never the outer transaction; if the outer transaction does `ROLLBACK`, the autonomous transaction's changes are preserved; critical use case: audit logging that persists even when the main operation fails — `log_error()` that always records what was attempted even on rollback; no other open-source database has this built-in

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
- [ ] 10.2 ⏳ C FFI — `axiomdb_open`, `axiomdb_execute`, `axiomdb_close` with `#[no_mangle]`
- [ ] 10.3 ⏳ Compile as `cdylib` — `.so` / `.dll` / `.dylib`
- [ ] 10.4 ⏳ Python binding — working `ctypes` demo
- [ ] 10.5 ⏳ Embedded test — same DB used from server and from library
- [ ] 10.6 ⏳ Node.js binding (Neon) — native `.node` module for Electron and Node apps; async/await API
- [ ] 10.7 ⏳ Embedded vs server benchmark — compare in-process vs TCP loopback latency to demonstrate embedded advantage

---

> **🏁 MVP CHECKPOINT — week ~50**
> On completing Phase 10, AxiomDB must be able to:
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
- [ ] 11.2 ⏳ TOAST — values >2KB to overflow pages with LZ4; small blobs (≤ threshold) stay inline
- [ ] 11.2b ⏳ BLOB_REF storage format — replace flat `u24+bytes` encoding in row codec with a 1-byte header that distinguishes: `0x00`=inline, `0x01`=TOAST pointer (8B page_id chain), `0x02`=content-hash (32B SHA256, Phase 14); this abstraction is the foundation that makes TOAST and content-addressed storage swappable without changing the executor or SQL layer
- [ ] 11.2c ⏳ MIME_TYPE auto-detection — on BLOB insert, read first 16 magic bytes to detect PNG/JPEG/WebP/PDF/GIF/ZIP/etc.; cache as 1-byte enum alongside the BLOB_REF in the row; expose as `MIME_TYPE(col)→TEXT` SQL function; zero overhead on read (metadata is in the row)
- [ ] 11.2d ⏳ BLOB reference tracking — reference count per BLOB page chain (for TOAST GC); counter lives in the overflow page header; `free_blob(page_id)` decrements and chains free only when count reaches 0; prerequisite for content-addressed dedup in Phase 14
- [ ] 11.2e ⏳ Unicode NFC normalization on store — every TEXT value is normalized to NFC (Canonical Decomposition followed by Canonical Composition) before being written to disk; `'café'` (NFD: 6 bytes) and `'café'` (NFC: 5 bytes) become identical on store, making `=` always correct for visually identical strings; zero API surface change — completely transparent to the application; this is what DuckDB does and it eliminates an entire class of invisible Unicode bugs that cause `'García' = 'García'` to return FALSE when one was typed and one was pasted from a different source
- [ ] 11.3 ⏳ In-memory mode — `open(":memory:")` without disk
- [ ] 11.4 ⏳ Native JSON — single JSON type (always binary storage like PostgreSQL's jsonb, never text-json); no `json` vs `jsonb` confusion; JSONPath SQL:2016 standard syntax; automatic GIN index on JSON columns (opt-out via `WITHOUT INDEX`); `->` and `->>` operators for compatibility with both MySQL and PostgreSQL syntax; `JSON_SET`, `JSON_REMOVE`, `JSON_MERGE_PATCH` for atomic updates without rewriting the full document; comparison: PostgreSQL has two confusing types (json/jsonb), MySQL has non-standard operators — we have one type that does everything correctly
- [ ] 11.4b ⏳ JSONB_SET — update JSON field without rewriting the entire document
- [ ] 11.4c ⏳ JSONB_DELETE_PATH — remove specific field from JSONB
- [ ] 11.4b ⏳ Trigram indexes for substring search — `CREATE INDEX ON productos (nombre) USING trigram`; makes `WHERE nombre LIKE '%García%'` use the index instead of full table scan; `WHERE nombre ILIKE '%garcia%'` also indexed (case-insensitive); PostgreSQL requires installing pg_trgm extension manually and it is not enabled by default — we include trigram support built-in; the query planner automatically suggests `CREATE INDEX ... USING trigram` in EXPLAIN output when it detects frequent `LIKE '%...%'` patterns causing sequential scans
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
- [ ] 12.8 ⏳ Unified axiom_* observability system — all system views use consistent naming, types, and join keys; `SELECT * FROM axiom_queries` shows running queries with pid, duration, state, sql_text, plan_hash; `SELECT * FROM axiom_bloat` shows table bloat (from 7.11); `SELECT * FROM axiom_slow_queries` is auto-populated when query exceeds `slow_query_threshold` (default 1s); `SELECT * FROM axiom_stats` shows database-wide metrics (cache hit rate, rows read/written, lock waits); `SELECT * FROM axiom_index_usage` shows which indexes are used/unused; unlike MySQL's inconsistent SHOW commands and PostgreSQL's complex pg_catalog joins, every axiom_* view is self-documented, joinable, and has the same timestamp/duration formats
- [ ] 12.9 ⏳ Date/time validation strictness — `'0000-00-00'` is always rejected with a clear error (MySQL allows this invalid date); `TIMESTAMP WITH TIME ZONE` is the single timestamp type with explicit timezone; no silent timezone conversion based on column type; `'2024-02-30'` is always an error; `'2024-13-01'` is always an error; retrocompatible: `SET AXIOM_COMPAT='mysql'` re-enables MySQL's lenient date behavior for migration

### Phase 13 — Advanced PostgreSQL `⏳` week 30-31
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — native pub-sub with `DashMap` of channels
- [ ] 13.5 ⏳ Covering indexes — `INCLUDE (col1, col2)` in B+ Tree leaves
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + atomic swap
- [ ] 13.7 ⏳ Row-level locking — lock specific row during UPDATE/DELETE; reduces contention vs per-table lock from 7.5
- [ ] 13.8 ⏳ Deadlock detection — DFS on wait graph when lock_timeout expires; kill the youngest transaction
- [ ] 13.9 ⏳ Immutable / append-only tables — `CREATE TABLE journal IMMUTABLE`; the engine physically rejects UPDATE and DELETE on that table at the storage layer (not just a trigger); WAL still accepts new inserts; errors on any modification attempt with SQLSTATE 42000; critical for accounting, compliance, and audit logs where data must never be altered — only corrected via compensating inserts
- [ ] 13.10 ⏳ Gapless sequences — `CREATE SEQUENCE inv_num GAPLESS START 1`; unlike AUTO_INCREMENT (which skips numbers on rollback), a gapless sequence uses a dedicated lock + WAL entry to guarantee no gaps even across failures; `NEXTVAL('inv_num')` blocks until the sequence number is committed; required by tax law in most countries for invoice numbering; `LAST_VALUE`, `RESET TO n` for administration
- [ ] 13.11 ⏳ Fiscal period locking — `LOCK FISCAL PERIOD '2023'`; after locking, INSERT/UPDATE/DELETE of rows with any date column falling within that period returns an error; `UNLOCK FISCAL PERIOD '2023'` for corrections; stored in a system table `axiom_locked_periods`; the executor checks against it for tables that have a designated date column (`CREATE TABLE t (..., WITH FISCAL_DATE = created_at)`)
- [ ] 13.12 ⏳ Statement-level triggers — `CREATE TRIGGER t AFTER INSERT ON journal FOR EACH STATEMENT`; fires once after the entire DML statement, not once per row; receives aggregated counts; enables double-entry validation: after a batch of journal inserts, verify that SUM(debits) = SUM(credits) within the same transaction, rejecting the commit if not balanced
- [ ] 13.13 ⏳ Collation system — layered, smart, cross-compatible
  <!--
  Design: 6 layers, each overrides the previous:
    L1: Storage     — NFC normalization always (Phase 11.2e)
    L2: Session     — SET collation / SET AXIOM_COMPAT (Phase 5.2b)
    L3: Database    — CREATE DATABASE db COMPAT='mysql'|'postgresql'|'standard'
    L4: Table       — CREATE TABLE t COLLATE 'unicode'
    L5: Column      — CREATE TABLE t (name TEXT COLLATE 'es_ES')
    L6: Query       — ORDER BY nombre COLLATE 'binary'  (highest priority)
  -->
- [ ] 13.13a ⏳ UCA root as default — replace byte-order comparison with Unicode Collation Algorithm Level 1 as the database default; `ñ` sorts after `n`, Arabic/Hebrew/CJK each in correct Unicode order, without any declaration; compatible with PostgreSQL CS/AS behavior; DuckDB does this — no OLTP database does it yet; `ORDER BY apellido` just works for every human language without configuration
- [ ] 13.13b ⏳ Per-database COMPAT mode — `CREATE DATABASE axiomdb COMPAT = 'mysql'` makes all text comparisons in that database behave like MySQL utf8mb4_unicode_ci (CI+AI): `'garcia' = 'García'` is TRUE; `CREATE DATABASE axiomdb COMPAT = 'postgresql'` uses byte order like PostgreSQL C locale; the same server can host a MySQL-compat database and a PostgreSQL-compat database simultaneously — no other database engine offers this; critical for migration scenarios where you cannot change application code
- [ ] 13.13c ⏳ axiom_collations registry — system table listing all available collations: `name`, `algorithm`, `case_sensitive`, `accent_sensitive`, `icu_locale`, `description`; includes cross-system aliases: `'utf8mb4_unicode_ci'` is an alias for MySQL CI+AI behavior; `'en-US-x-icu'` is an alias for PostgreSQL ICU syntax; `'C'` is an alias for binary/byte-order; apps migrating from MySQL or PostgreSQL use their existing collation names without changes
- [ ] 13.13d ⏳ COLLATE 'auto' per-column script detection — when a column is declared `TEXT COLLATE 'auto'`, AxiomDB analyzes the Unicode script property of stored data (Latin, Arabic, CJK, Cyrillic, etc.) and caches the dominant script in column metadata; subsequent `ORDER BY` uses the appropriate CLDR tailoring for that script automatically; `SELECT * FROM axiom_column_collations` shows detected scripts; no other database does this — inspired by how mobile OS keyboards auto-detect language
- [ ] 13.13e ⏳ Full ICU integration — link against libicu for industry-standard Unicode collation; `COLLATE 'de_DE'` applies German phone-book order (ß → ss); `COLLATE 'ja_JP'` handles Japanese kana/kanji ordering; `COLLATE 'tr_TR'` handles Turkish dotted/dotless I correctly; `CREATE COLLATION my_custom (BASE='es_ES', CASE_SENSITIVE=false)` for custom rules; exact same behavior as PostgreSQL ICU collations but with zero configuration for the common case
- [ ] 13.14 ⏳ Custom aggregate functions — `CREATE AGGREGATE median(FLOAT) (SFUNC=median_state, STYPE=FLOAT[], FINALFUNC=median_final)`; user-defined aggregates beyond SUM/COUNT/AVG/MAX/MIN; enables: weighted average, geometric mean, mode, P95 latency, Gini coefficient, domain-specific business metrics; Phase 16.1 has scalar UDFs but aggregates have different execution semantics (called once per row, finalized once per group)
- [ ] 13.15 ⏳ Filtered LISTEN/NOTIFY — `SUBSCRIBE TO orders WHERE status = 'pending' AND total > 1000 ON CHANGE`; current LISTEN/NOTIFY (13.4) notifies any change to the entire table; real-time dashboards need selective subscriptions — "notify me only about high-value pending orders" — without this the client receives all changes and filters in application code, wasting network bandwidth
- [ ] 13.16 ⏳ Transactional reservations with auto-release
- [ ] 13.17 ⏳ Recycle Bin for DROP TABLE — `DROP TABLE clientes` moves the table to the recycle bin instead of deleting it immediately; `FLASHBACK TABLE clientes TO BEFORE DROP` restores it completely with all data, indexes, and constraints intact; `SELECT * FROM axiom_recyclebin` lists dropped objects; `PURGE TABLE clientes` permanently deletes from the bin; configurable `recyclebin_retention = '30 days'`; eliminates the most common DBA emergency ("someone accidentally dropped the wrong table in production") without requiring a full database restore; Oracle introduced this in 10g and it became one of the most appreciated features
- [ ] 13.18 ⏳ Bi-temporal tables (SQL:2011) — first-class DDL for two-time-dimension data: `PERIOD FOR validity (valid_from, valid_until)` (application time: when the fact was true in reality) + `PERIOD FOR system_time` (transaction time: when it was recorded); `SELECT * FROM salaries FOR PERIOD OF validity AS OF DATE '2023-01-01' AS OF SYSTEM TIME '2023-02-15'` answers "what salary did Alice have on Jan 1 according to the records as they existed on Feb 15?"; extends Phase 7.16 (read-only AS OF) to a full SQL:2011 bitemporal model with DDL support; critical for accounting, insurance, HR, legal — any domain where both "when it happened" and "when we knew about it" matter independently — `INSERT INTO reservations (resource_id, session_id) VALUES (42, 'sess_abc') ON CONFLICT DO NOTHING RETURNING CASE WHEN id IS NULL THEN 'unavailable' ELSE 'reserved' END`; plus automatic release when session expires or connection drops; hotel booking, concert tickets, parking spots, inventory hold — "hold this item for 15 minutes while the user checks out"

### Phase 14 — TimescaleDB + Redis + Content-addressed BLOB `⏳` week 32-33
- [ ] 14.1 ⏳ Table partitioning — `PARTITION BY RANGE/HASH/LIST`
- [ ] 14.2 ⏳ Partition pruning — query planner skips irrelevant partitions
- [ ] 14.3 ⏳ Automatic compression of historical partitions — LZ4 columnar
- [ ] 14.4 ⏳ Continuous aggregates — incremental refresh of only the new delta
- [ ] 14.5 ⏳ TTL per row — `WITH TTL 3600` + background reaper in Tokio
- [ ] 14.6 ⏳ LRU eviction — for in-memory mode with RAM limit
- [ ] 14.7 ⏳ Chunk-level compression statistics — track compression ratio per partition; decides when to compress automatically
- [ ] 14.8 ⏳ Time-series benchmarks — insert 1M rows with timestamp; compare range scan vs TimescaleDB
- [ ] 14.9 ⏳ Content-addressed BLOB store — SHA256 of blob bytes = content key; separate content-store area in the .db file (beyond the heap); on BLOB insert: compute SHA256 → lookup in content index → if found: increment ref_count + store only the 32-byte hash in the BLOB_REF (header=0x02) → if not found: write bytes once + ref_count=1; two rows with identical photo share exactly one copy on disk; transparent to SQL layer — `SELECT photo` returns the full bytes regardless of backend
- [ ] 14.10 ⏳ BLOB garbage collector — periodic scan of content store ref_counts; blobs with ref_count=0 are reclaimed; integrates with MVCC vacuum cycle (runs after dead-tuple vacuum so rollback of inserts correctly decrements); safe under concurrent reads (ref_count never drops to 0 while a snapshot can see the blob)
- [ ] 14.11 ⏳ BLOB dedup metrics — `SELECT * FROM axiom_blob_stats` returns: `total_blobs`, `unique_blobs`, `dedup_ratio`, `bytes_saved`, `avg_blob_size`; helps users understand storage efficiency and decide whether to enable/disable dedup per table (`WITH (blob_dedup = off)`)
- [ ] 14.12 ⏳ IoT: LAST(value ORDER BY ts) aggregate — returns the most recent value per group ordered by timestamp; `SELECT device_id, LAST(temperature ORDER BY recorded_at) FROM readings GROUP BY device_id`; different from MAX; essential for "current state" dashboards of sensors, vehicles, wearables
- [ ] 14.13 ⏳ IoT: Dead-band / change-only recording — `CREATE TABLE sensors WITH (dead_band_col = temp, dead_band = 0.5)`; engine skips INSERT when value differs from previous by less than threshold; reduces storage 80-95% for slowly-changing sensors without any application changes
- [ ] 14.14 ⏳ IoT: Gap filling and interpolation — `INTERPOLATE(value, 'locf' | 'linear' | 'step')` fills NULL gaps from sensor disconnections; LOCF = last observation carried forward; essential for charting and ML pipelines that require continuous time series
- [ ] 14.15 ⏳ IoT: EVERY interval syntax — `SELECT AVG(temp) EVERY '5 minutes' FROM sensors WHERE ts > NOW() - INTERVAL '1 day'`; declarative downsampling without explicit GROUP BY FLOOR(EXTRACT(EPOCH FROM ts)/300); reduces query complexity for time-bucketed analytics

### Phase 15 — MongoDB + DoltDB + Arrow `⏳` week 34-35
- [ ] 15.1 ⏳ Change streams CDC — tail the WAL, emit Insert/Update/Delete events
- [ ] 15.2 ⏳ Git for data — commits, branches, checkout with snapshot of roots
- [ ] 15.3 ⏳ Git merge — branch merge with conflict detection
- [ ] 15.4 ⏳ Apache Arrow output — results in columnar format for Python/pandas
- [ ] 15.5 ⏳ Flight SQL — Arrow Flight protocol for high-speed columnar transfer (Python, Rust, Java without JDBC)
- [ ] 15.6 ⏳ CDC + Git tests — verify change streams and branch merge with real conflicts
- [ ] 15.7 ⏳ CDC with full OLD/NEW row — `REPLICA IDENTITY FULL` equivalent;
- [ ] 15.8 ⏳ Flashback Table — `FLASHBACK TABLE empleados TO TIMESTAMP NOW() - INTERVAL '2 hours'` restores the table to its state at that point in time using WAL history; different from Phase 7.16 AS OF (which is read-only): Flashback Table actually replaces current data with historical data; `FLASHBACK TABLE pedidos TO SCN 1234567` using the WAL sequence number for precision; requires retaining enough WAL history (configurable retention window); use case: "I accidentally ran UPDATE without WHERE on production — restore the table to 5 minutes ago"; extends Phase 15.2 (Git for data) to a SQL-native restore operation; Oracle Flashback Technology (2003) is still unique in databases — no PostgreSQL or MySQL equivalent exists UPDATE events include the complete before-image (all column values before the change) and after-image; without this, UPDATE events in CDC only show the new values and primary key, making it impossible to detect which specific fields changed; required for audit trails, sync systems, and data pipelines that need to compute diffs

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
- [ ] 16.10 ⏳ Built-in connection pooler — Pgbouncer-equivalent implemented inside the engine; multiplexes N application connections into M database backend connections (N >> M); transaction-mode pooling (connection returned to pool after each COMMIT/ROLLBACK); session variables reset between borrows; eliminates the need for external Pgbouncer/Pgpool deployment; critical for any app with >100 concurrent users since creating one OS thread per TCP connection does not scale

### Phase 17 — Security `⏳` week 39-40
- [ ] 17.1 ⏳ CREATE USER / CREATE ROLE — user and role model
- [ ] 17.2 ⏳ GRANT / REVOKE — permissions per table and per column
- [ ] 17.3 ⏳ Row-Level Security — `CREATE POLICY empresa_isolation ON cuentas USING (empresa_id = current_setting('app.empresa_id')::INT)`; policies applied automatically on every SELECT/INSERT/UPDATE/DELETE without application code changes; multiple policies per table combined with OR; `FORCE ROW LEVEL SECURITY` for table owners; critical for multi-tenant accounting software where one DB instance serves multiple companies and data isolation is a legal requirement
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
- [ ] 17.15 ⏳ Column-level encryption — `CREATE TABLE patients (name TEXT, ssn TEXT ENCRYPTED WITH KEY 'k1')`; encryption/decryption happens inside the engine using AES-256-GCM; ciphertext stored on disk; plaintext only visible in query results to authorized roles; key rotation without full table rewrite; healthcare (HIPAA), HR, legal all require this for PII fields
- [ ] 17.16 ⏳ Dynamic data masking — `CREATE MASKING POLICY mask_ssn ON patients (ssn) USING MASKED WITH ('***-**-' || RIGHT(ssn,4))`; different roles see different representations of the same column without changing stored data; `SELECT ssn FROM patients` returns real value to admins, masked value to analysts; no application code changes required
- [ ] 17.17 ⏳ Column-level GRANT — `GRANT SELECT (name, email, created_at) ON patients TO nurse_role`; deny access to diagnosis, ssn, medication columns for that role; currently Phase 17.2 grants at table level only; column-level is required when different departments have different sensitivity levels
- [ ] 17.18 ⏳ Consent-based row access — `CREATE POLICY patient_consent ON records USING (has_consent(patient_id, CURRENT_USER))`; patient explicitly grants a specific doctor access to their records; revoking consent immediately removes access; beyond standard RLS — the USING expression calls a user-defined consent table
- [ ] 17.19 ⏳ GDPR physical purge — `DELETE PERMANENTLY FROM patients WHERE id = 42 PURGE ALL VERSIONS`; with MVCC, normal DELETE leaves historical versions visible to old snapshots; PURGE physically overwrites all pages containing that row's versions across all WAL history; required for GDPR right-to-erasure and CCPA; audit entry records the purge but not the data
- [ ] 17.20 ⏳ Digital signatures on rows — `SELECT SIGN_ROW(contract_id) FROM contracts` embeds an HMAC of the row's content + timestamp + signer_id; `VERIFY_ROW(contract_id)` returns TRUE if content matches signature; tamper detection for legal documents, audit logs, financial records; signatures stored alongside the row in the heap
- [ ] 17.21 ⏳ Storage quotas per tenant — `ALTER TENANT acme SET (max_storage = '10 GB', max_rows = 1000000)`;
- [ ] 17.22 ⏳ Transparent Data Encryption (TDE) at tablespace level — `CREATE DATABASE axiomdb ENCRYPTION = 'AES-256-GCM'`; the engine encrypts all pages before writing to disk and decrypts on read; the application sees plaintext — zero code changes required; the `.db` file is meaningless without the key even if stolen from disk; key stored separately from data (configurable: local keystore, HSM, AWS KMS, Vault); complements Phase 17.15 (column-level encryption) — TDE protects the whole database at rest, column encryption protects specific fields even from DBAs; required for PCI-DSS, HIPAA, SOC 2 compliance where full disk encryption of database files is mandatory engine tracks storage used per schema/tenant and rejects INSERTs when quota is exceeded with a clear SQLSTATE error; `SELECT * FROM axiom_tenant_usage` for monitoring; critical for SaaS billing and preventing one tenant from monopolizing disk space

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
- [ ] 18.11 ⏳ Replication slot WAL retention protection — `max_replication_slot_wal_keep = '10 GB'` (safe default); when a replica falls behind and the retention limit is reached, the slot is dropped gracefully and the replica is disconnected with a clear error instead of silently filling the primary's disk; `SELECT * FROM axiom_replication_slots` shows slot name, active, wal_retained_bytes, age; this is a known production outage cause in PostgreSQL (fixed in PG 13 but not as default) — we ship with a safe default from day one

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
- [ ] 20.10 ⏳ GENERATE_SERIES — `SELECT * FROM GENERATE_SERIES(1, 100)` and `GENERATE_SERIES('2024-01-01'::date, '2024-12-31', '1 month')`; fill calendar gaps, generate synthetic data, pivot by time period; used in reporting, IoT dashboards, financial calendars; no app-side loop needed
- [ ] 20.11 ⏳ TABLESAMPLE — `SELECT * FROM users TABLESAMPLE SYSTEM(1)` returns ~1% of rows with minimal I/O (page-level sampling); `TABLESAMPLE BERNOULLI(0.1)` for row-level random sampling; A/B testing, statistical analysis, ML train/test splits, approximate analytics on large tables without full scan
- [ ] 20.12 ⏳ ORDER BY RANDOM() — `SELECT * FROM items WHERE rarity='epic' ORDER BY RANDOM() LIMIT 5`; random ordering using Fisher-Yates shuffle on result set; gaming loot drops, quiz randomization, A/B test group assignment, recommendation diversity; simple but missing from current plan
- [ ] 20.13 ⏳ Range types — `int4range`, `int8range`, `numrange`, `daterange`, `tsrange`; operators: `@>` (contains), `&&` (overlaps), `+` (union), `*` (intersection), `-` (difference); hotel booking systems (no overlapping reservations), salary bands, price ranges, event scheduling; stored compactly as two values + bounds
- [ ] 20.14 ⏳ UNNEST — `SELECT id, UNNEST(tags) AS tag FROM posts`; expands an array column into multiple rows; joins with array elements, search by tag, pivot unnested data; complement to Phase 20.4 (ARRAY types)
- [ ] 20.15 ⏳ Regex in queries — `~` (match), `~*` (case-insensitive match), `!~` (not match), `REGEXP_MATCH(str, pattern)`, `REGEXP_REPLACE(str, pattern, replacement)`; more powerful than LIKE; legal document pattern extraction, log parsing, data validation, address/email format checking
- [ ] 20.16 ⏳ Business calendar functions — `NEXT_BUSINESS_DAY(date, country_code)` returns next non-weekend non-holiday date; `BUSINESS_DAYS_BETWEEN(date1, date2, country_code)` counts working days excluding weekends and public holidays; `IS_BUSINESS_DAY(date, country_code)→BOOL`; holidays configurable per country via `CREATE HOLIDAY CALENDAR 'CO' ...`; used in HR (vacation days), legal (filing deadlines), logistics (delivery estimates), finance (settlement dates T+2); virtually every business app needs this but most implement it incorrectly in application code
- [ ] 20.17 ⏳ MONEY type with multi-currency arithmetic — `MONEY(amount DECIMAL, currency CHAR(3))`; `100 USD + 85 EUR` converts using a configurable exchange rate table (`axiom_exchange_rates`); `CONVERT(amount, from_currency, to_currency, AS OF date)`; stored as (amount, currency_code) pair; arithmetic rejects mixing currencies without explicit conversion; apps with international pricing, multi-currency invoicing, forex trading need this to avoid embedding currency logic in application code
- [ ] 20.18 ⏳ Composite / user-defined types — `CREATE TYPE address AS (street TEXT, city TEXT, state CHAR(2), zip TEXT)`; used as column type: `ALTER TABLE users ADD COLUMN home_address address`; queried with dot notation: `SELECT home_address.city FROM users`; more type-safe than JSON, more compact than separate columns; domain modeling for complex objects (coordinates, ranges, contact info, product dimensions)
- [ ] 20.19 ⏳ ltree — hierarchical path type — `CREATE TABLE categories (path ltree)`; stores paths like `electronics.phones.smartphones`; operators: `@>` (ancestor), `<@` (descendant), `~` (pattern match), `||` (concatenate); GIN index makes subtree queries O(1) regardless of depth; for deep hierarchies (100+ levels) recursive CTEs become slow — ltree solves this without schema changes; file systems, org charts, category trees, DNS zones
- [ ] 20.20 ⏳ XMLType — `CREATE TABLE contratos (id BIGINT, contenido XML)`; `XMLType` stores XML documents natively with validation against XSD schemas; `XMLTABLE()` shreds XML into relational rows: `SELECT * FROM XMLTABLE('/pedidos/pedido' PASSING xml_col COLUMNS id INT PATH '@id', total DECIMAL PATH 'total')`; `XMLQUERY()` for XQuery expressions; `XMLELEMENT()`, `XMLFOREST()` to construct XML from relational data; critical for: SOAP web services, EDI (Electronic Data Interchange), SWIFT financial messages, HL7 healthcare, FIX protocol trading, legacy enterprise systems that speak XML; PostgreSQL has XMLType, MySQL does not; many Oracle migration projects require it

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
- [ ] 21.25 ⏳ PIVOT dynamic — `SELECT * FROM sales PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb', 'Mar', 'Apr'))` transforms rows into columns dynamically; unlike CASE WHEN (which requires knowing column names at write time), dynamic PIVOT adapts to the data; BI reports, cross-tab analysis, cohort studies, financial summaries by period
- [ ] 21.24 ⏳ ORM compatibility tier 2 — Prisma and ActiveRecord connect; migrations with RETURNING, GENERATED IDENTITY and deferred FK; document incompatibilities

---

## BLOCK 7 — Product Features (Phases 22-23)

### Phase 22 — Vector search + advanced search + GIS `⏳` week 52-54
- [ ] 22.1 ⏳ Vector similarity — `VECTOR(n)`, operators `<=>`, `<->`, `<#>`
- [ ] 22.2 ⏳ HNSW index — `CREATE INDEX USING hnsw(col vector_cosine_ops)`
- [ ] 22.3 ⏳ Fuzzy search — `SIMILARITY()`, trigrams, `LEVENSHTEIN()`
- [ ] 22.4 ⏳ ANN benchmarks — compare HNSW vs pgvector vs FAISS on recall@10 and QPS; document quality/speed tradeoff
- [ ] 22.5 ⏳ IVFFlat alternative index — lower RAM option than HNSW for collections >10M vectors
- [ ] 22.6 ⏳ GIS: Spatial data types — POINT, LINESTRING, POLYGON, MULTIPOINT, MULTIPOLYGON, GEOMETRY; stored compactly as WKB (Well-Known Binary); implements axiomdb-geo crate (currently stub); required by every delivery, store-locator, logistics, real-estate, and fleet-management application
- [ ] 22.7 ⏳ GIS: R-Tree spatial index — `CREATE INDEX ON locations USING rtree(coords)`; O(log n) bounding box queries; without this every spatial query is a full table scan; enables `WHERE ST_DWithin(location, point, 5000)` in milliseconds over millions of points
- [ ] 22.8 ⏳ GIS: Core spatial functions — `ST_Distance`, `ST_Within`, `ST_Contains`, `ST_Intersects`, `ST_Area`, `ST_Length`, `ST_Buffer`, `ST_Union`, `ST_AsText`, `ST_GeomFromText`; the minimum vocabulary for geographic queries; `SELECT * FROM stores WHERE ST_Distance(location, ST_Point(-74.0, 40.7)) < 5000`
- [ ] 22.9 ⏳ GIS: Coordinate system support — WGS84 (GPS coordinates) and local projections; `ST_Transform(geom, 4326)` converts between SRID systems; without this distances are in degrees instead of meters
- [ ] 22.10 ⏳ GIS: Spatial benchmarks — compare range query and nearest-neighbor vs PostGIS on 1M point dataset; document performance characteristics
- [ ] 22.11 ⏳ Approximate query processing — `SELECT APPROX_COUNT_DISTINCT(user_id) FROM events` uses HyperLogLog (error < 2%, 10000x faster than COUNT DISTINCT); `SELECT PERCENTILE_APPROX(response_ms, 0.95) FROM requests` uses t-digest (accurate tail estimation); `SELECT APPROX_TOP_K(product_id, 10) FROM purchases` returns approximate top-10 using Count-Min Sketch; for analytics on billions of rows where exact answers take minutes and approximate answers (99.9% accurate) take milliseconds

### Phase 22b — Platform features `⏳` week 55-57
- [ ] 22b.1 ⏳ Scheduled jobs — `cron_schedule()` with `tokio-cron-scheduler`
- [ ] 22b.2 ⏳ Foreign Data Wrappers — HTTP + PostgreSQL as external sources
- [ ] 22b.3 ⏳ Multi-database — `CREATE DATABASE`, `USE`, cross-db queries
- [ ] 22b.4 ⏳ Schema namespacing — `CREATE SCHEMA`, `schema.table`
- [ ] 22b.5 ⏳ Schema migrations CLI — `axiomdb migrate up/down/status`
- [ ] 22b.6 ⏳ FDW pushdown — push SQL predicates to remote origin when possible; avoid fetching unnecessary rows
- [ ] 22b.7 ⏳ Data lineage tracking — `SELECT * FROM axiom_lineage WHERE table_name = 'ml_features'` shows which tables fed this one and when; `CREATE TABLE ml_features AS SELECT ... FROM raw_events WITH LINEAGE`; tracks column-level derivations across transformations; ML pipelines need to know which training data produced which model; compliance systems need to trace PII through all derived tables; enables impact analysis ("if I change this source table, what downstream tables break?")
- [ ] 22b.8 ⏳ Query result cache with auto-invalidation — `SELECT /*+ RESULT_CACHE */ * FROM products WHERE featured = TRUE`; engine caches the result set and automatically invalidates it when any of the underlying tables changes (not just TTL-based); `SELECT /*+ RESULT_CACHE(ttl=60s) */ ...` for TTL fallback; `SELECT * FROM axiom_result_cache` shows cached queries, hit rate, memory used; smarter than Phase 22b.8 original (TTL only) — inspired by Oracle SQL Result Cache which invalidates on data change: no stale data, no manual INVALIDATE needed
- [ ] 22b.9 ⏳ Transactional Message Queue — `CREATE QUEUE pagos_pendientes`; `ENQUEUE(queue=>'pagos_pendientes', message=>pago_record)` inside a transaction: the message is only visible to consumers when the surrounding COMMIT succeeds; if the transaction rolls back, the message never appears; `DEQUEUE(queue=>'pagos_pendientes')` removes and returns the next message atomically; `max_retries=3` + dead letter queue `pagos_fallidos` after N failed attempts; `message_delay = INTERVAL '5 minutes'` for delayed delivery; ACID semantics throughout — fundamentally different from LISTEN/NOTIFY (which is fire-and-forget, not persistent, not transactional); enables: payment processing, order fulfillment, async email sending, workflow orchestration — all with exactly-once delivery guarantees
- [ ] 22b.10 ⏳ Job Chains with DAG scheduling — `CREATE CHAIN etl_noche` defines a directed acyclic graph of jobs: step A runs first, then B and C run in parallel when A succeeds, then D runs only when both B and C succeed, then E always runs (cleanup) regardless of success/failure; `ON_ERROR = 'continue'|'abort_chain'|'skip_to'` per step; retry with exponential backoff; timeout per step; notification on chain failure via the transactional queue (22b.9); `SELECT * FROM axiom_chain_runs` shows execution history with per-step timing; far more powerful than cron-style scheduling (22b.1) — enables complex ETL pipelines, multi-step data processing, database-native workflow orchestration

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
- [ ] 23.3 ⏳ Migrate from MySQL — `axiomdb migrate from-mysql` with `mysql_async`
- [ ] 23.4 ⏳ Migrate from PostgreSQL — `axiomdb migrate from-postgres` with `tokio-postgres`
- [ ] 23.5 ⏳ PostgreSQL wire protocol — port 5432, psql and psycopg2 connect
- [ ] 23.6 ⏳ Both protocols simultaneously — :3306 MySQL + :5432 PostgreSQL
- [ ] 23.7 ⏳ ORM compatibility tests — Django ORM, SQLAlchemy, ActiveRecord, Prisma connect without changes
- [ ] 23.8 ⏳ Dump / restore compatibility — read dumps from `mysqldump` and `pg_dump --format=plain`
- [ ] 23.9 ⏳ ORM compatibility tier 3 — Typeorm (async), psycopg3 (Python), SQLx (Rust compile-time) connect; benchmark queries/s vs native PostgreSQL

---

> **🏁 PRODUCTION-READY CHECKPOINT — week ~67**
> On completing Phase 23, AxiomDB must be able to:
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
- [ ] 25.10 ⏳ OLTP Compression (online, during DML) — `CREATE TABLE pedidos (...) COMPRESS FOR OLTP`; unlike Phase 14.3 (historical partition compression), this compresses rows during normal INSERT/UPDATE/DELETE operations using a page-level dictionary: duplicate values within the same page share a single copy; typical savings 3-5x with <5% CPU overhead; Oracle Advanced Compression (2008) achieves this; no open-source database does online OLTP compression — all require either bulk load or separate compression passes; particularly effective for tables with many repeated values (status columns, foreign keys, short strings)

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
- [ ] 31.12 ⏳ Connection string DSN — `axiomdb://user:pass@host:port/dbname?param=val`; `postgres://` and `mysql://` as aliases
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
- [ ] 34.5 ⏳ Standard DSN — `axiomdb://`, `postgres://`, `DATABASE_URL` env var
- [ ] 34.6 ⏳ Extensions system — `CREATE EXTENSION` + `pg_available_extensions`
- [ ] 34.7 ⏳ WASM extensions — `CREATE EXTENSION FROM FILE '*.wasm'`
- [ ] 34.8 ⏳ VACUUM FREEZE — prevent Transaction ID Wraparound
- [ ] 34.9 ⏳ Parallel DDL — `CREATE TABLE AS SELECT WITH PARALLEL N`
- [ ] 34.10 ⏳ pgbench equivalent — `axiomdb-bench` with standard OLTP scenarios
- [ ] 34.11 ⏳ Final benchmarks — full comparison vs MySQL, PostgreSQL, SQLite, DuckDB
- [ ] 34.12 ⏳ Consensus protocol (basic Raft) — for automatic failover in cluster; replaces manual failover from 18.10
- [ ] 34.13 ⏳ Distributed transactions — two-phase commit between shards; cross-shard consistency

### Phase 35 — Deployment and DevEx `⏳` week 111-113
- [ ] 35.1 ⏳ Multi-stage Dockerfile — Rust builder + debian-slim runtime
- [ ] 35.2 ⏳ docker-compose.yml — complete setup with volumes and env vars
- [ ] 35.3 ⏳ systemd service file — `axiomdb.service` for Linux production
- [ ] 35.4 ⏳ Complete axiomdb.toml — network, storage, logging, AI, TLS configuration
- [ ] 35.5 ⏳ Log levels and rotation — trace/debug/info/warn/error + daily/size rotation
- [ ] 35.6 ⏳ axiomdb-client crate — official Rust SDK with connection pool
- [ ] 35.7 ⏳ Python package — `pip install axiomdb-python` with psycopg2-style API
- [ ] 35.8 ⏳ Homebrew formula — `brew install axiomdb` for macOS
- [ ] 35.9 ⏳ GitHub Actions CI — test + clippy + bench + fuzz on each PR
- [ ] 35.10 ⏳ Performance tuning guide — which parameters to adjust for each workload
- [ ] 35.11 ⏳ Kubernetes operator — `AxiomDBCluster` CRD with replica management and auto-scaling
- [ ] 35.12 ⏳ Helm chart — K8s deployment with production defaults
- [ ] 35.13 ⏳ TPC-H production benchmark — run full TPC-H and publish results; public reference point
- [ ] 35.14 ⏳ Public API documentation — complete reference of SQL dialect, wire protocol extensions, C FFI, configuration; auto-generated from code + hand-written where needed
- [ ] 35.15 ⏳ External security audit — review attack surfaces before release: SQL injection, auth bypass, path traversal in COPY, buffer overflows in parser; use `cargo-audit` + manual review of unsafe

---

## BLOCK 11 — AxiomQL (Phases 36-37)

> **Design decision (2026-03-23):** AxiomDB will support two query languages sharing
> one AST and executor. SQL stays as the primary language with full wire protocol
> compatibility. AxiomQL is an optional method-chain alternative for developers who
> prefer modern readable syntax. Both compile to the same `Stmt` enum — zero executor
> overhead, every SQL feature automatically available in AxiomQL.
>
> **Prerequisite:** Phase 8 (wire protocol) must be complete so the AST is stable.

### Phase 36 — AxiomQL Core (SELECT + READ) `⏳` week 114-117

#### 36.A — Foundation
- [ ] 36.1 ⏳ AxiomQL lexer — `.`, `(`, `)`, `:` named args, operators, string/number/bool literals, identifiers, `@` decorators
- [ ] 36.2 ⏳ Core SELECT: `.filter()`, `.sort()`, `.take()`, `.pick()`, `.skip()` → compile to SQL `Stmt`
- [ ] 36.3 ⏳ `.distinct()` — removes duplicate rows; `.distinct(col)` = DISTINCT ON(col)

#### 36.B — Joins
- [ ] 36.4 ⏳ `.join(table)` — auto-infers ON from FK catalog; `.join(orders, on: user_id)` for explicit
- [ ] 36.5 ⏳ `.left_join()`, `.right_join()`, `.full_join()`, `.cross_join()` — all join types
- [ ] 36.6 ⏳ `.join(table.join(other))` — nested/chained joins for multi-table queries

#### 36.C — Aggregation
- [ ] 36.7 ⏳ `.group(col, agg: fn())` — GROUP BY with aggregates; no need to repeat group key in pick
- [ ] 36.8 ⏳ Aggregate functions: `count()`, `sum(col)`, `avg(col)`, `min(col)`, `max(col)`, `string_agg(col, sep)`
- [ ] 36.9 ⏳ Aggregate with filter: `count(where: active)`, `sum(amount, where: status = 'ok')` → compiles to AGG FILTER(WHERE)
- [ ] 36.10 ⏳ `.rollup(a, b)`, `.cube(a, b)`, `.grouping_sets([a], [b], [])` — analytical grouping
- [ ] 36.11 ⏳ Terminal aggregates: `users.count()`, `orders.sum(amount)`, `orders.avg(amount)` — no group needed

#### 36.D — Window functions
- [ ] 36.12 ⏳ `.window(col: fn().over(partition).sort(order))` — OVER clause; `row_number()`, `rank()`, `dense_rank()`
- [ ] 36.13 ⏳ Offset window functions: `lag(col)`, `lead(col)`, `first_value(col)`, `last_value(col)`, `nth_value(col, n)`
- [ ] 36.14 ⏳ Window aggregates: `sum(col).over(partition)`, `avg(col).over(partition).rows(preceding: 3)`
- [ ] 36.15 ⏳ Frame clauses: `.rows(unbounded_preceding)`, `.range(current_row)`, `.groups(n)` as chained methods

#### 36.E — Set operations + advanced subqueries
- [ ] 36.16 ⏳ `.union(other)`, `.union_all(other)`, `.intersect(other)`, `.except(other)` — set operations
- [ ] 36.17 ⏳ Subquery in `.filter()`: `users.filter(id in orders.filter(amount > 1000).pick(user_id))`
- [ ] 36.18 ⏳ `.exists(subquery)`, `.not_exists(subquery)` — EXISTS / NOT EXISTS
- [ ] 36.19 ⏳ Correlated subquery in `.pick()`: `users.pick(name, total: orders.filter(user_id = .id).sum(amount))`
- [ ] 36.20 ⏳ `let` bindings / named CTEs: `let top = orders.group(...)` → WITH clause; multiple lets compose
- [ ] 36.21 ⏳ Recursive CTE: `let tree = nodes.recursive(parent_id = .id)` → WITH RECURSIVE

#### 36.F — Expressions
- [ ] 36.22 ⏳ `match {}` — alternative to CASE WHEN: `match(status) { 'ok' → 1, _ → 0 }`
- [ ] 36.23 ⏳ Null-safe: `.filter(col.is_null())`, `.filter(col.not_null())`, `col.or(default)` → COALESCE
- [ ] 36.24 ⏳ JSON navigation: `data.name`, `data['key']`, `data.tags[0]` → JSON operators `->>` / `->` / `#>>`
- [ ] 36.25 ⏳ Full-text search: `.search(col, 'term')`, `.search(col, 'term', lang: 'english')` → tsvector/tsquery
- [ ] 36.26 ⏳ `.filter(col ~ 'regex')` — regex match operator

#### 36.G — Introspection + diagnostics
- [ ] 36.27 ⏳ `.explain()` — appends EXPLAIN; `.explain(analyze: true)` → EXPLAIN ANALYZE
- [ ] 36.28 ⏳ `show tables`, `show columns(users)`, `describe(users)` — introspection commands

#### 36.H — Advanced joins + inline data
- [ ] 36.32 ⏳ `.lateral_join(fn)` — LATERAL JOIN; fn receives outer row: `orders.lateral_join(o => items.filter(order_id = o.id).limit(3))`
- [ ] 36.33 ⏳ `values([[1,'a'],[2,'b']]).as('t', cols: [id, name])` — VALUES as inline table; useful in JOINs and CTEs
- [ ] 36.34 ⏳ `users.sample(pct: 10)` / `users.sample(rows: 1000)` — TABLESAMPLE SYSTEM; approximate random sample

#### 36.I — Statistical + ordered-set aggregates
- [ ] 36.35 ⏳ `orders.percentile(amount, 0.95)` → PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY amount)
- [ ] 36.36 ⏳ `orders.percentile_disc(amount, 0.5)`, `orders.mode(status)` → PERCENTILE_DISC / MODE()
- [ ] 36.37 ⏳ `json_agg(expr)`, `json_build_object(k, v)`, `array_agg(col)` as aggregate functions in `.group()` and `.pick()`
- [ ] 36.38 ⏳ `table.unnest(col)` — UNNEST array column into rows

#### 36.J — Date/time + ranges
- [ ] 36.39 ⏳ `col.in_tz('America/Bogota')` → AT TIME ZONE; `col.format('YYYY-MM-DD')` → TO_CHAR
- [ ] 36.40 ⏳ Interval arithmetic: `created_at + interval(days: 7)`, `now() - interval(hours: 1)`
- [ ] 36.41 ⏳ `series(from: 1, to: 100)` / `series(from: date1, to: date2, step: interval(days: 1))` → GENERATE_SERIES
- [ ] 36.42 ⏳ Range operators: `period.overlaps(other)`, `period.contains(point)`, `period.adjacent(other)` → `&&`, `@>`, `-|-`

#### 36.K — Collation
- [ ] 36.43 ⏳ `.sort(name.collate('utf8mb4_unicode_ci'))` — per-expression COLLATE; `.filter(a.collate('C') = b)` for byte-level comparison

#### 36.L — Quality
- [ ] 36.44 ⏳ Equivalence test suite — for every AxiomQL construct, assert SQL equivalent produces identical results
- [ ] 36.45 ⏳ Parser benchmarks — AxiomQL throughput vs SQL parser on same queries
- [ ] 36.46 ⏳ Error messages — when a construct isn't supported: "use the SQL equivalent: SELECT ... OVER (...)"

### Phase 37 — AxiomQL Write + DDL + Control `⏳` week 118-121

#### 37.A — DML write
- [ ] 37.1 ⏳ `.insert(col: val, ...)` — single row; `users.insert_many([...])` — batch
- [ ] 37.2 ⏳ `.insert_select(query)` — INSERT INTO ... SELECT
- [ ] 37.3 ⏳ `.update(col: val, ...)` — UPDATE with filter chain
- [ ] 37.4 ⏳ `.delete()` — DELETE with filter chain
- [ ] 37.5 ⏳ `.upsert(on: col)` — INSERT ON CONFLICT DO UPDATE
- [ ] 37.6 ⏳ `.returning(col, ...)` — RETURNING clause on insert/update/delete; returns affected rows
- [ ] 37.7 ⏳ `.for_update()`, `.for_share()`, `.skip_locked()` — pessimistic locking on SELECT

#### 37.B — DDL
- [ ] 37.8 ⏳ `create table {}` with `@` decorators: `@primary`, `@auto`, `@unique`, `@required`, `@default(val)`, `@references(other.col)`
- [ ] 37.9 ⏳ `alter table` — `.add(col: type)`, `.drop(col)`, `.rename(old, new)`, `.rename_to(name)`
- [ ] 37.10 ⏳ `drop table`, `truncate table` — destructive DDL
- [ ] 37.11 ⏳ `create table_as(query)` — CREATE TABLE AS SELECT
- [ ] 37.12 ⏳ Indexes: `index table.col`, `index table(a, b)`, `@fulltext`, `@partial(filter_expr)`
- [ ] 37.13 ⏳ `migration 'name' { }` block — versioned schema changes with up/down

#### 37.C — Transactions + control flow
- [ ] 37.14 ⏳ `transaction { }` block — BEGIN/COMMIT with auto ROLLBACK on error
- [ ] 37.15 ⏳ `transaction(isolation: serializable) { }` — SET TRANSACTION ISOLATION LEVEL
- [ ] 37.16 ⏳ `savepoint 'name'` / `rollback to 'name'` / `release 'name'` inside transaction blocks
- [ ] 37.17 ⏳ `abort(msg)` inside transaction — manual ROLLBACK with error message

#### 37.D — Reusable logic
- [ ] 37.18 ⏳ `proc name(args) { }` — stored procedures in AxiomQL syntax
- [ ] 37.19 ⏳ `fn name(args) -> type { }` — user-defined functions; callable inside `.filter()`, `.pick()`
- [ ] 37.20 ⏳ `on table.after.insert { }`, `on table.before.update { }` — triggers with `.new` / `.old` access

#### 37.E — Temporal (requires Phase 7 MVCC time-travel)
- [ ] 37.21 ⏳ `users.as_of('2026-01-01')` — historical snapshot read → AS OF TIMESTAMP
- [ ] 37.22 ⏳ `users.history()` — all versions of rows → temporal scan
- [ ] 37.23 ⏳ `users.changes(from: t1, to: t2)` — delta between two snapshots

#### 37.G — Bulk I/O (COPY)
- [ ] 37.27 ⏳ `users.export('/path/file.csv', format: csv)` — COPY TO; also `format: json`, `format: parquet`
- [ ] 37.28 ⏳ `users.import('/path/file.csv', format: csv)` — COPY FROM with schema validation and error reporting
- [ ] 37.29 ⏳ `users.filter(...).export(query)` — export result of arbitrary query, not just full table

#### 37.H — Reactive queries (LISTEN/NOTIFY)
- [ ] 37.30 ⏳ `channel('name').listen()` — LISTEN channel; returns async stream of notifications
- [ ] 37.31 ⏳ `channel('name').notify(payload)` — NOTIFY channel, 'payload'
- [ ] 37.32 ⏳ `users.subscribe(filter: active)` — reactive query stream; uses WAL CatalogChangeNotifier from Phase 3.13

#### 37.I — Cursors (server-side iteration)
- [ ] 37.33 ⏳ `users.filter(...).cursor()` — server-side cursor for large result sets; compiles to DECLARE + CURSOR
- [ ] 37.34 ⏳ `.fetch(n)` / `.fetch_all()` / `.close()` — FETCH n / FETCH ALL / CLOSE on cursor object
- [ ] 37.35 ⏳ `.each(batch: 1000, fn)` — convenience: cursor + fetch loop + auto-close

#### 37.J — Row-Level Security
- [ ] 37.36 ⏳ `policy on users { name: 'p', using: tenant_id = current_user() }` — CREATE POLICY; auto-filter per user
- [ ] 37.37 ⏳ `users.enable_rls()` / `users.disable_rls()` — ALTER TABLE ENABLE/DISABLE ROW LEVEL SECURITY
- [ ] 37.38 ⏳ `drop policy 'name' on users` — DROP POLICY

#### 37.K — Advisory locks
- [ ] 37.39 ⏳ `advisory_lock(key) { ... }` — block-based advisory lock; auto-release on exit
- [ ] 37.40 ⏳ `advisory_lock_shared(key) { ... }` — shared advisory lock for read-only critical sections
- [ ] 37.41 ⏳ `lock.try_acquire(key)` — non-blocking attempt; returns bool

#### 37.L — Maintenance
- [ ] 37.42 ⏳ `vacuum(users)`, `vacuum(users, full: true, analyze: true)` — VACUUM; reclaims dead MVCC rows
- [ ] 37.43 ⏳ `analyze(users)` — UPDATE STATISTICS for query planner
- [ ] 37.44 ⏳ `reindex(users)`, `reindex(users.email_idx)` — REINDEX table or index
- [ ] 37.45 ⏳ `checkpoint()` — manual WAL checkpoint; flush all dirty pages

#### 37.N — Prepared statements
- [ ] 37.49 ⏳ `prepare('name', users.filter(id = $1).pick(name, email))` — PREPARE; compiles query once, reuses plan
- [ ] 37.50 ⏳ `execute('name', args: [42])` — EXECUTE prepared statement with bound parameters
- [ ] 37.51 ⏳ `deallocate('name')` / `deallocate_all()` — DEALLOCATE; free one or all prepared statements

#### 37.O — Advanced write
- [ ] 37.52 ⏳ `users.filter(...).into_table('archive')` — SELECT INTO; creates new table from query result
- [ ] 37.53 ⏳ `.merge(source, on: key, matched: .update(amount: .new.amount), not_matched: .insert())` — full MERGE statement
- [ ] 37.54 ⏳ `truncate(users, cascade: true)` — TRUNCATE with CASCADE; also truncates dependent FK tables

#### 37.P — Special operations
- [ ] 37.55 ⏳ `users.flashback(before_drop: true)` — restore table from recycle bin (Phase 13.17)
- [ ] 37.56 ⏳ `fiscal_lock('2023')` / `fiscal_unlock('2023')` — lock/unlock fiscal period (Phase 13.11)
- [ ] 37.57 ⏳ `.explain(format: json)` / `.explain(format: text, buffers: true)` — extended EXPLAIN options

#### 37.Q — Real-time change watching
- [ ] 37.61 ⏳ `users.watch()` — returns a live stream of row changes (insert/update/delete); uses WAL CatalogChangeNotifier
- [ ] 37.62 ⏳ `users.watch(filter: active)` — filtered watch; only emits changes matching the condition
- [ ] 37.63 ⏳ `.on('insert', fn)`, `.on('update', fn)`, `.on('delete', fn)` — per-event handlers on watch stream
- [ ] 37.64 ⏳ `users.watch().diff()` — emits `{old, new}` pairs on update; useful for audit trails

#### 37.R — Schemas + multitenancy
- [ ] 37.65 ⏳ `schema('tenant_123').users.filter(active)` — query within a specific schema; compiles to SET search_path or schema-qualified names
- [ ] 37.66 ⏳ `create schema('tenant_123')` / `drop schema('tenant_123', cascade: true)` — CREATE/DROP SCHEMA
- [ ] 37.67 ⏳ `schema('src').users.copy_to(schema: 'dst')` — copy table structure (and optionally data) between schemas

#### 37.S — Sequences
- [ ] 37.68 ⏳ `create sequence('order_num', start: 1000, step: 5)` — CREATE SEQUENCE with options
- [ ] 37.69 ⏳ `sequence('order_num').next()` — NEXTVAL; `sequence('order_num').current()` — CURRVAL; `sequence('order_num').set(500)` — SETVAL
- [ ] 37.70 ⏳ `drop sequence('order_num')` / `alter sequence('order_num', max: 99999)` — DDL on sequences

#### 37.T — Materialized views
- [ ] 37.71 ⏳ `materialized_view('active_users', users.filter(active).pick(id, name))` — CREATE MATERIALIZED VIEW from AxiomQL query
- [ ] 37.72 ⏳ `active_users.refresh()` / `active_users.refresh(concurrent: true)` — REFRESH MATERIALIZED VIEW
- [ ] 37.73 ⏳ `drop materialized_view('active_users')` — DROP MATERIALIZED VIEW
- [ ] 37.74 ⏳ Materialized views are queryable like regular tables: `active_users.filter(name ~ 'A%').count()`

#### 37.U — Schema metadata + comments
- [ ] 37.75 ⏳ `users.comment('Registered application users')` — COMMENT ON TABLE
- [ ] 37.76 ⏳ `users.col('email').comment('Primary contact, must be verified')` — COMMENT ON COLUMN
- [ ] 37.77 ⏳ `users.labels(team: 'auth', domain: 'users')` — key/value labels on tables for tooling and autodoc

#### 37.V — Extensions + statistics
- [ ] 37.78 ⏳ `enable_extension('uuid-ossp')` / `enable_extension('pgvector')` — CREATE EXTENSION; required before using extension types/functions
- [ ] 37.79 ⏳ `disable_extension('name')` — DROP EXTENSION
- [ ] 37.80 ⏳ `list_extensions()` — show available and installed extensions
- [ ] 37.81 ⏳ `statistics('stat_name', users, [age, country])` — CREATE STATISTICS; teaches planner about column correlations for better query plans

#### 37.W — Table inheritance
- [ ] 37.82 ⏳ `create employees extends persons { salary: real, department: text }` — CREATE TABLE ... INHERITS; employees rows appear in persons queries
- [ ] 37.83 ⏳ `persons.only()` — SELECT from parent only, excluding inherited rows → ONLY keyword
- [ ] 37.84 ⏳ `drop table employees (no_inherit: true)` — DROP TABLE without affecting parent

#### 37.M — Quality
- [ ] 37.85 ⏳ Documentation — AxiomQL reference in docs-site: every method with SQL equivalent side-by-side
- [ ] 37.86 ⏳ Fuzz testing — malformed AxiomQL input; every panic = regression test
- [ ] 37.87 ⏳ `.to_sql()` pretty-printer — `users.filter(active).to_sql()` returns the generated SQL (debug + learning tool)

---

> **🏁 FEATURE-COMPLETE CHECKPOINT — week ~120**
> On completing Phase 37, AxiomDB is a complete production database engine with two query interfaces:
> - MySQL + PostgreSQL + OData + GraphQL simultaneously
> - AxiomQL method-chain language as modern alternative to SQL
> - AI-native (embeddings, hybrid search, RAG)
> - Horizontal distribution (sharding + Raft)
> - Deploy on Docker/K8s/systemd
> - Complete documentation and TPC-H published
