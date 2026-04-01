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
- [x] ✅ rotate_right key shift bug FIXED (2026-03-26) — was leaving stale bytes in key_lens[cn] causing key_at panic at scale; fixed with explicit reverse loop in tree.rs
- [x] ✅ Stale root_page_id in SessionContext cache FIXED (2026-03-26) — after B+tree root split, cached IndexDef held freed page_id; fixed with ctx.invalidate_all() after index root change in execute_insert_ctx and execute_delete
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
- [x] 3.5a ✅ Autocommit mode (`SET autocommit=0`) — implemented in `SessionContext` + executor + wire sync; covered by `spec-3.5abc-autocommit-txn-semantics.md`
- [x] 3.5b ✅ Implicit transaction start (MySQL ORM compat) — implemented in executor semantics; covered by `spec-3.5abc-autocommit-txn-semantics.md`
- [x] 3.5c ✅ Error semantics mid-transaction (statement vs txn rollback) — implemented with savepoint-based rollback in executor; covered by `spec-3.5abc-autocommit-txn-semantics.md`
- [x] ✅ 3.6b ENOSPC handling — read-only degraded mode on disk full
- [x] ✅ 3.8b Verified open — detect page checksum corruption on startup; scan all allocated pages; swap reopen to open_with_recovery in server + embedded
- [x] 3.8c ✅ Doublewrite buffer for torn page repair — `.db.dw` file alongside main file; all dirty pages + pages 0/1 written and fsynced before main fsync; startup recovery repairs torn pages from DW copy; idempotent recovery; CRC32c + sentinel validation; MySQL 8.0.20+ separate-file model
- [x] ✅ 3.15b Per-page flush_range optimization — targeted durable flush of dirty mmap ranges
- [x] 3.17 ✅ WAL batch append — `TxnManager::record_insert_batch()`: reserve_lsns(N) + serialize all N Insert entries into wal_scratch + write_batch() in one write_all; O(1) BufWriter calls instead of O(N); entries byte-for-byte identical to per-row path; crash recovery unchanged
- [x] 3.18 ✅ WAL PageWrite — EntryType::PageWrite=9; record_page_writes() emits 1 compact entry per affected page (key=page_id, new_value=[num_slots][slot_ids...]); insert_rows_batch groups phys_locs by page_id; crash recovery uses slot_ids for undo; redo of committed page images remains deferred
- [x] 3.19 ✅ WAL Group Commit — initial timer-based concurrent fsync batching with `CommitCoordinator`; later superseded in the server path by `6.19`'s always-on leader-based fsync pipeline
- [x] 3.19c ✅ WAL sync policy — `axiomdb-wal/src/sync.rs` now selects the DML durability syscall explicitly (`fsync`, `fdatasync`, `F_FULLFSYNC`, `sync_all` fallback) instead of hiding the hot path behind `File::sync_data()`; metadata-changing WAL operations remain on the metadata-sync path
- [x] 3.19d ✅ Configurable WAL durability policy — `WalDurabilityPolicy::{Strict, Normal, Off}` is now parsed from config with backward-compatible `fsync` fallback; `TxnManager` routes commit acknowledgement by policy and the server uses the fsync pipeline only in `Strict`

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
- [x] 4.25b ✅ Structured error responses — ParseError position field (byte offset) + visual snippet in ERR packet; UniqueViolation renamed {index_name, value: Option<String>}; SET error_format='json' returns JSON-structured ERR payload; 10 integration tests + 12 wire assertions
- [x] 4.25c ✅ Strict mode + warnings system — `strict_mode: bool` in SessionContext (default ON); SET strict_mode=OFF|ON|DEFAULT; SET sql_mode=''/STRICT_TRANS_TABLES/STRICT_ALL_TABLES; permissive INSERT/UPDATE fallback with warning 1265 "Data truncated for column '%s' at row %d"; wire sync in handler.rs; SET parser added to top-level parser; 14 integration tests + 10 unit tests + 14 wire assertions
- [x] 4.7 ✅ SQLSTATE codes — all DbError variants mapped; SQL-reachable errors have precise 5-char codes

<!-- ── Group E — Core SQL (needs executor) ── -->
- [x] 4.8 ✅ JOIN — INNER, LEFT, RIGHT, CROSS with nested loop; USING; multi-table; FULL → NotImplemented
- [x] 4.8b ✅ FULL OUTER JOIN — matched-right bitmap nested-loop in apply_join; compute_outer_nullable replaces is_outer_nullable for chain-aware metadata; both NotImplemented guards removed; 6 integration tests: matched+unmatched, one-to-many, ON vs WHERE, USING, SELECT * nullability, join chain; AxiomDB SQL extension over MySQL wire protocol; docs added
- [x] 4.9a ✅ GROUP BY hash-based — HashMap<key_bytes, GroupState>; value_to_key_bytes; NULL keys group correctly
- [x] 4.9b ✅ GROUP BY sort-based — sorted streaming executor; auto-selects when access method is IndexLookup/IndexRange/IndexOnlyScan with matching GROUP BY prefix; hash path remains default fallback; 9 integration tests + 11 wire assertions
  - [x] ✅ ORDER BY + GROUP BY col_idx mismatch — fixed in 4.10e: remap_order_by_for_grouped rewrites column/aggregate expressions to output positions before apply_order_by
- [x] 4.9c ✅ Aggregate functions — COUNT(*), COUNT(col), SUM, MIN, MAX, AVG (→ Real); skip NULL; finalize
- [x] 4.9e ✅ GROUP_CONCAT() — `SELECT GROUP_CONCAT(tag ORDER BY tag SEPARATOR ', ') FROM tags GROUP BY post_id`; MySQL's most-used aggregate function; DISTINCT modifier to deduplicate before concatenating; max length configurable; NULL values skipped; returns NULL for empty group; `string_agg(expr, sep)` PostgreSQL alias; 18 integration tests + 15 wire assertions
- [x] 4.9d ✅ HAVING clause — eval_with_aggs intercepts aggregate calls; representative_row for col refs
- [x] 4.10 ✅ ORDER BY + LIMIT/OFFSET — in-memory sort; stable sort_by; sort_err pattern
- [x] 4.10b ✅ Multi-column ORDER BY with mixed direction — composite comparator, left-to-right
- [x] 4.10c ✅ NULLS FIRST / NULLS LAST — ASC→NULLS LAST, DESC→NULLS FIRST (PG defaults); explicit override
- [x] 4.10d ✅ Parameterized LIMIT/OFFSET — `LIMIT ?` / `OFFSET ?` in prepared statements; accepts Int/BigInt/>0 and exact integer Text; rejects negatives, non-integral text, NULL; safe usize::try_from for BigInt
- [x] 4.11 ✅ Scalar subqueries — scalar `(SELECT ...)`, `IN (SELECT ...)`, `EXISTS/NOT EXISTS`, correlated subqueries, derived tables `FROM (SELECT ...)`, `SubqueryRunner` trait + `eval_with`; 14 integration tests
- [x] 4.12 ✅ DISTINCT — HashSet dedup on projected output rows; NULL=NULL for grouping; pre-LIMIT
- [x] 4.12b ✅ CAST + basic type coercion — explicit and implicit conversion between compatible types
- [x] 4.24 ✅ CASE WHEN — searched + simple form; NULL semantics; nested; SELECT/WHERE/ORDER BY/GROUP BY
- [x] 4.6 ✅ INSERT ... SELECT — execute_select + col_map + insert_row; MVCC prevents self-reads

<!-- ── Group F — Functions (needs executor) ── -->
- [x] 4.13 ✅ version() / current_user / session_user / current_database() — ORMs call these on connect; required for Phase 5 compatibility
- [x] 4.14 ✅ LAST_INSERT_ID() / lastval() — AUTO_INCREMENT execution + per-table thread-local sequence; ColumnDef.auto_increment flag (bit1 of existing flags byte); LAST_INSERT_ID()/lastval() in eval_function
- [x] 4.19 ✅ Basic built-in functions — `ABS`, `LENGTH`, `SUBSTR`, `UPPER`, `LOWER`, `TRIM`, `COALESCE`, `NOW()`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `ROUND`, `FLOOR`, `CEIL`
- [x] 4.19b ✅ BLOB functions — `FROM_BASE64(text)→BLOB`, `TO_BASE64(blob)→TEXT`, `OCTET_LENGTH(value)→INT`, `ENCODE(blob,'base64'/'hex')→TEXT`, `DECODE(text,'base64'/'hex')→BLOB`; b64_encode/b64_decode/hex_encode/hex_decode helpers inline (no external crate)
- [x] 4.19d ✅ MySQL scalar functions — `DATE_FORMAT(ts, fmt)` (MySQL strftime-style, all 18 specifiers + passthrough for unknowns); `STR_TO_DATE(str, fmt)` (inverse parser, NULL on failure, 2-digit year rule); `FIND_IN_SET(needle, csv)` (1-indexed, case-insensitive); fixed `year/month/day/hour/minute/second` extractors (were stub, now use chrono); `IF/IFNULL/NULLIF` were already implemented
- [x] 4.19c ✅ UUID generation functions — `gen_random_uuid()`/`uuid_generate_v4()` (UUID v4 random); `uuid_generate_v7()`/`uuid7()` (UUID v7 time-ordered, better B+Tree locality); `is_valid_uuid(text)→BOOL`; `parse_uuid_str` helper; rand crate added to axiomdb-sql

<!-- ── Group G — DevEx (parallel with E+F) ── -->
- [x] 4.15 ✅ Interactive CLI — axiomdb-cli REPL: multi-line SQL, ASCII table formatter, .tables/.schema/.quit/.open/.help dot commands, TTY detection (no prompt in pipe mode), timing per query, pipe/script mode; new crate axiomdb-cli
- [x] 4.15b ✅ CLI history + autocomplete — rustyline Editor with SqlHelper: ↑/↓ history, Ctrl-R reverse search, Tab SQL keyword completion, ~/.axiomdb_history persistence; Ctrl-C clears buffer; pipe mode reads all stdin then splits on ';'

<!-- ── Group H — Introspection + DDL modification (needs executor) ── -->
- [x] 4.20 ✅ SHOW TABLES / SHOW COLUMNS / DESCRIBE — parser + executor using CatalogReader; MySQL-compatible 6-column output; Extra shows auto_increment
- [x] 4.21 ✅ TRUNCATE TABLE — delete-all + AUTO_INCREMENT sequence reset; MySQL convention (returns count=0)
- [x] 4.22 ✅ Basic ALTER TABLE — `ADD COLUMN` (row rewrite + default), `DROP COLUMN` (row rewrite), `RENAME COLUMN`, `RENAME TO`; parser + CatalogWriter extensions; 15 integration tests; ColumnAlreadyExists (SQLSTATE 42701)
- [x] 4.22b ✅ ALTER TABLE ADD/DROP CONSTRAINT — parser handles ADD CONSTRAINT UNIQUE/CHECK, DROP CONSTRAINT [IF EXISTS]; UNIQUE→creates unique index; CHECK→persists in new axiom_constraints catalog table (4th system table, lazy-init); check_row_constraints() enforced on INSERT; expr_to_sql_string() for persistence; drop searches both axiom_indexes and axiom_constraints; FK/PK return NotImplemented

<!-- ── Group I — Validation (last, closes the phase) ── -->
- [x] 4.16 ✅ SQL full test suite — LIKE/BETWEEN/IN/IS NULL, CAST, scalar functions (ABS/LENGTH/UPPER/LOWER/TRIM/SUBSTR/ROUND/COALESCE/NOW), NULL semantics, string concat, arithmetic expressions, error cases (division by zero, InvalidCoercion); documents NOT NULL/UNIQUE/CHECK gaps; 1046 total tests
- [x] 4.16b ✅ INSERT throughput benchmark (2026-03-24, Apple M2 Pro, MmapStorage+WAL, 10K rows):
  - Baseline (no cache): 20,694/s
  - + SchemaCache in analyze_cached(): 28,702/s (+38%) — eliminates catalog heap scan per row
  - + SessionContext in execute_with_ctx(): 29,887/s (+44% total) — eliminates executor-side catalog scan
  - MariaDB 12.1 reference: 140,926/s (4.7× faster)
  - Remaining gap cause: per-row HeapChain::insert() + WalEntry serialization (~20µs/row)
  - Full scan: AxiomDB 501K/s vs MariaDB 213K/s → AxiomDB **2.4× faster** ✅
- [x] 4.16c ✅ Multi-row INSERT optimization — insert_rows_batch() uses record_insert_batch() (3.17); bench_insert_multi_row/10K: 211K rows/s (1 SQL string) vs 35K rows/s (N strings) = 6× faster; AxiomDB 211K/s vs MariaDB ~140K/s = 1.5× faster in bulk INSERT
- [x] 4.16d ✅ WAL record per page — implemented in Phase 3.18 (EntryType::PageWrite=9); insert_rows_batch() emits 1 PageWrite per affected page; 238× fewer WAL entries for 10K-row insert; 30% smaller WAL; crash recovery parses slot_ids for undo

### Phase 5 — MySQL Wire Protocol `🔄` week 26-30
- [x] 5.1 ✅ TCP listener with Tokio — accept connections on :3306; Arc<Mutex<Database>>; tokio::spawn per connection
- [x] 5.2 ✅ MySQL handshake — HandshakeV10 (greeting) + HandshakeResponse41 (client response)
- [x] 5.2a ✅ Charset/collation negotiation in handshake — `character_set_client`, `character_set_results`, `collation_connection` sent in Server Greeting; client chooses charset; `ConnectionState` built from handshake collation id; inbound text decoded with client charset; outbound rows encoded with result charset; latin1 (cp1252), utf8mb3, utf8mb4, binary supported; `SET NAMES` and individual SET charset vars update typed session fields; `SHOW VARIABLES LIKE 'character_set%'` reflects live state; 8 new wire assertions + ~25 unit tests
- [x] 5.2c ✅ ON_ERROR session behavior — typed `OnErrorMode` shared by `SessionContext` + `ConnectionState`; `SET on_error = rollback_statement|rollback_transaction|savepoint|ignore|DEFAULT`; visible via `SELECT @@on_error`, `SELECT @@session.on_error`, `SHOW VARIABLES LIKE 'on_error'`; `database.rs` applies policy to parse/analyze/execute failures; `ignore` converts ignorable SQL errors to warnings + OK while non-ignorable runtime errors eagerly roll back the txn and still return ERR; `COM_RESET_CONNECTION` restores `rollback_statement`; 6 executor integration tests + 14 unit tests + 17 wire assertions
- [x] 5.2b ✅ Session-level collation and compat mode — `CompatMode`+`SessionCollation` typed; `SET AXIOM_COMPAT=mysql|postgresql|standard`; `SET collation=es|binary|DEFAULT`; CollationGuard thread-local propagates through all eval(); Es fold: NFC+lowercase+strip-accents (unicode-normalization); text_semantics.rs: compare_text+text_eq+like_match_collated; GROUP BY/DISTINCT use value_to_session_key_bytes; GROUP_CONCAT ORDER BY+dedup session-aware; presorted GROUP BY gated on binary-safe text; plan_select_ctx rejects text indexes under Es; `@@axiom_compat`, `@@collation` via SHOW VARIABLES (real % wildcard); 9 unit tests
- [x] 5.3 ✅ Authentication — mysql_native_password (SHA1-based); permissive mode (Phase 5); root/axiomdb accepted
- [x] 5.3b ✅ caching_sha2_password — fast auth path (0x01 0x03 + ack read + OK at seq=4); pymysql default plugin works
- [x] 5.4 ✅ COM_QUERY handler — receive SQL → parse → analyze → execute_with_ctx → respond; COM_PING/QUIT/INIT_DB; ORM query interception (SET, @@version, SHOW DATABASES)
- [x] 5.4a ✅ max_allowed_packet enforcement — `MySqlCodec` stateful decoder with configurable `max_payload_len`; logical multi-packet reassembly (0xFFFFFF continuation fragments); `PacketTooLarge` error before buffer allocation; ERR 1153/08S01 + connection close on oversize; `SET max_allowed_packet` validated and syncs live decoder limit; `COM_RESET_CONNECTION` restores default; 12 codec unit tests + 6 wire assertions
- [x] 5.5 ✅ Result set serialization — column_count + column_defs + EOF + rows (lenenc text) + EOF; all AxiomDB types mapped to MySQL type codes
- [x] 5.5a ✅ Binary result encoding by type — `serialize_query_result_binary()` for COM_STMT_EXECUTE: null bitmap with offset-2, BIGINT 8-byte LE, INT 4-byte LE, REAL 8-byte IEEE-754, DECIMAL lenenc ASCII, TEXT/BYTES lenenc, DATE binary `[4][year u16][month][day]`, TIMESTAMP 7/11-byte binary; Bool→TINY (0x01), Decimal→NEWDECIMAL (0xf6) type-code alignment; COM_QUERY text path unchanged; 15 unit tests + 8 wire assertions
- [x] 5.6 ✅ Error packets — DbError → MySQL error code + SQLSTATE; full mapping for all error variants
- [x] 5.7 ✅ Test with real client — pymysql: connect, CREATE, INSERT (AUTO_INCREMENT), SELECT, error handling all pass
- [x] 5.8 ✅ Protocol unit tests — 47 tests: codec round-trip, greeting structure, OK/ERR/EOF, lenenc boundaries, result set sequence IDs, auth, session state
- [x] 5.9 ✅ Session state — ConnectionState: SET autocommit/NAMES/@@vars stored; SHOW VARIABLES result set; SELECT @@var from state; COM_INIT_DB updates current_database
- [x] 5.10 ✅ COM_STMT_PREPARE / COM_STMT_EXECUTE — binary param decoding (TINY/SHORT/LONG/LONGLONG/FLOAT/DOUBLE/DATE/DATETIME/strings); ? substitution with escape; COM_STMT_CLOSE/RESET; pymysql full test suite passes (INT/Bool/NULL/quotes/DictCursor)
- [x] 5.11 ✅ COM_PING / COM_QUIT / COM_RESET_CONNECTION / COM_INIT_DB — all handled in handler.rs command loop (0x0e, 0x01, 0x1f, 0x02)
- [x] 5.9c ✅ SHOW STATUS — server counters queryable via `SHOW STATUS` and `SHOW GLOBAL STATUS`: `Threads_connected`, `Threads_running`, `Questions`, `Uptime`, `Bytes_received`, `Bytes_sent`, `Com_select`, `Com_insert`, `Innodb_buffer_pool_read_requests`, `Innodb_buffer_pool_reads`; LIKE wildcard filtering with `%`/`_` semantics; session vs global scope; RAII guards for threads_connected/threads_running; 21 unit tests + 26 wire assertions
- [x] 5.9b ✅ `@@in_transaction` system variable — returns 1 inside an active transaction, 0 otherwise; visible via `SELECT @@in_transaction`; lets developers and ORMs verify transaction state without tracking it themselves; also add warning in OK packet (warning_count=1) when COMMIT/ROLLBACK is a no-op (no active txn), queryable via `SHOW WARNINGS`
- [x] 5.11b ✅ COM_STMT_SEND_LONG_DATA — chunked large-parameter transmission buffered per prepared statement; raw-byte accumulation until EXECUTE; `COM_STMT_RESET` clears stmt-local long-data state; long-data precedence over inline/null; `Com_stmt_send_long_data` visible in SHOW STATUS; wire smoke covers multibyte text split, binary `0x00` preservation, reset, and deferred `max_allowed_packet` overflow
- [x] 5.11c ✅ Explicit connection state machine — new `ConnectionLifecycle` transport layer separate from `ConnectionState`; explicit `CONNECTED→AUTH→IDLE→EXECUTING→CLOSING` phases in `handler.rs`; fixed 10s auth timeout; `wait_timeout` vs `interactive_timeout` on idle reads based on `CLIENT_INTERACTIVE`; `net_write_timeout` on packet writes; socket helper enables `TCP_NODELAY` + `SO_KEEPALIVE`; `COM_RESET_CONNECTION` recreates session state but preserves interactive classification; 3 live-socket integration tests + 1 protocol validation test + 11 wire assertions
- [x] 5.12 ✅ Multi-statement queries — split_sql_statements() handles `;` with quoted-string awareness; COM_QUERY loop executes each stmt; SERVER_MORE_RESULTS_EXISTS (0x0008) flag in intermediate EOF/OK; serialize_query_result_multi(); build_eof_with_status()/build_ok_with_status()
- [x] 5.13 ✅ Prepared statement plan cache — schema_version Arc<AtomicU64> in Database; compiled_at_version in PreparedStatement; lock-free version check on COM_STMT_EXECUTE; re-analyze on DDL mismatch; LRU eviction with max_prepared_stmts_per_connection (default 1024); 6 unit tests
- [x] 5.14 ✅ Throughput benchmarks + perf fix — SELECT 185 q/s (3.3× vs 56 q/s antes); INSERT 58 q/s (fsync necesario); root cause: read-only txns hacían fsync innecesario; fix: flush_no_sync para undo_ops.is_empty()
- [x] 5.15 ✅ Connection string DSN — shared typed parser in `axiomdb-core::dsn` accepts `axiomdb://`, `mysql://`, `postgres://`, `postgresql://`, `file:` and plain local paths; percent-decodes credentials/path/query, preserves query params, rejects duplicate keys, and supports bracketed IPv6; `axiomdb-server` now accepts `AXIOMDB_URL` with explicit `data_dir` validation while keeping `AXIOMDB_DATA`/`AXIOMDB_PORT` fallback behavior unchanged; `Db::open_dsn`, `AsyncDb::open_dsn`, and `axiomdb_open_dsn` accept only local-path DSNs and reject remote wire endpoints explicitly
- [x] 5.16 ✅ DELETE fast-path bulk truncate — root rotation: plan_bulk_empty_table collects old pages + alloc fresh heap/index roots; apply_bulk_empty_table rotates via CatalogWriter (update_table_data_root + update_index_root), resets Bloom filters, defers page free; execute_delete_ctx + execute_delete: removed secondary_indexes.is_empty() gate → bulk path on ALL tables (PK+UNIQUE+FK+secondary) unless has_fk_references; execute_truncate: parent-FK rejection + bulk path + AUTO_INCREMENT reset; TxnManager: deferred_free_pages in ActiveTxn, Savepoint.deferred_free_len, release_immediate_committed_frees() + release_committed_frees(), group_commit hooked; 9 integration tests
- [x] 5.17 ✅ In-place B+Tree write path expansion — 4 fast paths: (1) insert_leaf same-page already existed, now explicit; (2) delete_leaf same-page when !underfull — no alloc/free; (3) insert_subtree parent absorbs child split in-place when n<ORDER_INTERNAL — no alloc/free; (4) delete_subtree skips parent rewrite when new_child_pid==child_pid and !underfull — eliminates internal_underfull() call; helpers write_leaf_same_pid + write_internal_same_pid; fill_threshold exported pub; 7 new integration tests including CountingStorage delta proof (allocs=0/frees=0 on delete fast path); 2 stale CoW-only test assertions updated to functional correctness
- [x] 5.18 ✅ Heap insert tail-pointer cache — HeapAppendHint{root_page_id,tail_page_id} in heap_chain.rs; resolve_tail_with_hint validates hint (chain_next_page==0, PageNotFound→stale) + self-heals; insert_with_hint public API, insert stays as wrapper; heap_tail: HashMap<u32,(u64,u64)> in SessionContext; get/set/invalidate_heap_tail helpers; invalidate_all+invalidate_table also clear heap_tail; insert_row_with_ctx and update_row_with_ctx pull/push hint from ctx automatically — all callers benefit without API changes; insert_row_with_hint + update_row_with_hint for non-ctx hint reuse; 3 unit tests: valid hint skips walk, stale hint self-heals, root mismatch
- [x] 5.19a ✅ Executor decomposition — monolithic `crates/axiomdb-sql/src/executor.rs` replaced by `crates/axiomdb-sql/src/executor/` with `mod.rs` facade plus `shared.rs`, `select.rs`, `joins.rs`, `aggregate.rs`, `insert.rs`, `update.rs`, `delete.rs`, `bulk_empty.rs`, and `ddl.rs`; public API (`execute`, `execute_with_ctx`, `last_insert_id_value`) preserved; no SQL-visible behavior change; `cargo test -p axiomdb-sql`, `cargo clippy -p axiomdb-sql --tests -- -D warnings`, workspace validation, and wire smoke used as regression proof
- [x] 5.19b ✅ Eval decomposition — monolithic `crates/axiomdb-sql/src/eval.rs` replaced by `crates/axiomdb-sql/src/eval/` with stable facade in `mod.rs`; evaluator internals split into `context.rs`, `core.rs`, `ops.rs`, `functions/` families, and `tests.rs`; public exports (`eval`, `eval_with`, `eval_in_session`, `eval_with_in_session`, `is_truthy`, `like_match`, `ClosureRunner`, `CollationGuard`, `NoSubquery`, `SubqueryRunner`) preserved; `current_eval_collation()` remains available to sibling modules; no SQL-visible behavior change; targeted evaluator/date/subquery/executor regression suites used as proof
- [x] 5.19 ✅ B+tree batch delete (sorted single-pass) — new `BTree::delete_many_in` primitive accepts pre-sorted encoded keys and removes them in one left-to-right pass per index; `collect_delete_keys_by_index` / `delete_many_from_indexes` replace per-row `delete_from_indexes` calls for DELETE and UPDATE old-key removal; direct B+Tree regressions cover same-leaf no-alloc/free, cross-leaf survivors, and root collapse; executor regressions cover PK + secondary-index UPDATE correctness; root persisted once per affected index; wire smoke covers DELETE WHERE, UPDATE, full-table DELETE, and post-batch INSERT; local bench at 5K rows: `DELETE WHERE id > 2500` = 396K rows/s, `UPDATE ... WHERE active=TRUE` = 52.9K rows/s
- [x] 5.20 ✅ Stable-RID UPDATE fast path — heap same-slot rewrite path (`rewrite_tuple_same_slot` / `rewrite_batch_same_slot`) preserves `(page_id, slot_id)` when the new encoded row fits in the existing slot; dedicated WAL entry `UpdateInPlace=10` plus rollback/savepoint/crash-recovery restore by old tuple image; `TableEngine::update_rows_preserve_rid[_with_ctx]` batches stable-RID candidates and falls back row-by-row to delete+insert when the row no longer fits; selective index maintenance now uses real `(old_rid,new_rid)` + logical key/predicate membership comparison, so unchanged PK/secondary/FK indexes are skipped only when RID is stable; new regression coverage for same-page heap rewrite I/O and index-affected decisions; local bench at 50K rows: `UPDATE ... WHERE active=TRUE` = 647K rows/s vs 52.9K rows/s before `5.20`, while `DELETE WHERE id > 25000` remains 1.13M rows/s
- [x] 5.21 ✅ Transactional INSERT staging — explicit transactions now stage consecutive `INSERT ... VALUES` rows in `SessionContext::pending_inserts`; same-table INSERTs keep appending, while `SELECT`/`UPDATE`/`DELETE`/DDL/`COMMIT`/table switch/ineligible INSERT flush via `executor/staging.rs`; `ROLLBACK` discards unflushed rows; `batch_insert_into_indexes` persists each changed index root once per flush; savepoint ordering fixed so table-switch flushes happen before the next statement savepoint; SQL tests + wire smoke + `local_bench.py --scenario insert --rows 50000 --table` verified. Latest local benchmark (release server, 2026-03-27): MariaDB `28.0K r/s`, MySQL `26.7K r/s`, AxiomDB `23.9K r/s` for `single-row INSERTs in 1 txn`

### Phase 6 — Secondary indexes + FK `🔄` week 31-39
- [x] 6.1 ✅ `IndexColumnDef` + `IndexDef.columns` — catalog stores which columns each index covers; backward-compatible serialization
- [x] 6.1b ✅ Key encoding — order-preserving `Value` → `[u8]` for all SQL types (NULL sorts first, sign-flip for ints, NUL-escaped text)
- [x] 6.2 ✅ CREATE INDEX executor — scans table, builds B-Tree from existing data; `columns` persisted in catalog
- [x] 6.2b ✅ Index maintenance on INSERT/UPDATE/DELETE — secondary indexes kept in sync with heap; UNIQUE violation detection
- [x] 6.3 ✅ Basic query planner — detects `WHERE col = literal` and `WHERE col > lo AND col < hi` on indexed columns; replaces full scan with B-Tree lookup/range
- [x] 6.3b ✅ Indexed DELETE WHERE fast path — `plan_delete_candidates` / `plan_delete_candidates_ctx` skip stats_cost_gate (always use index for DELETE); `collect_delete_candidates` helper: IndexLookup/IndexRange → materialize RIDs → heap read → full WHERE recheck before deletion; wired into execute_delete_ctx and execute_delete; FK enforcement, index maintenance, partial-index predicates, collation guard all preserved; 5 integration tests
- [x] ⚠️ Composite index planner (> 1 column) — implemented in 6.9 (Rule 0 composite planner)
- [x] 6.4 ✅ Bloom filter per index — `BloomRegistry` per-DB; CREATE INDEX populates filter; INSERT adds keys; DELETE/UPDATE marks dirty; SELECT IndexLookup skips B-Tree on definite absence (1% FPR)
- [x] 6.5 ✅ Foreign key checker — `axiom_foreign_keys` catalog; DDL (CREATE TABLE REFERENCES, ALTER TABLE ADD/DROP CONSTRAINT FK); INSERT/UPDATE child validates parent; DELETE/UPDATE parent enforces RESTRICT
- [x] 6.6 ✅ ON DELETE CASCADE / SET NULL — recursive cascade (depth ≤ 10); SET NULL with nullable check; ON UPDATE RESTRICT; ON UPDATE CASCADE/SET NULL deferred to 6.9
- [x] ⚠️ FK auto-index (non-unique B-Tree duplicate keys) — implemented in 6.9 (fk_val|RecordId composite key)
- [x] ⚠️ PK B-Tree index population on INSERT — implemented in 6.9
- [x] 6.7 ✅ Partial UNIQUE index — `CREATE [UNIQUE] INDEX ... WHERE predicate`; predicate stored as SQL string in IndexDef; build/INSERT/UPDATE/DELETE filter by predicate; planner uses index only when query WHERE implies predicate; session cache invalidated after CoW B-Tree root change
- [x] 6.8 ✅ Fill factor — `CREATE INDEX ... WITH (fillfactor=N)`; persisted in IndexDef (u8, default 90); BTree::insert_in threads fillfactor → split threshold = ceil(FF×ORDER_LEAF/100); backward-compat
- [x] 6.9 ✅ PK B-Tree population on INSERT; FK composite key index (fk_val|RecordId); composite index planner Rule 0; B-Tree range scan for FK enforcement
- [x] 6.10 ✅ Index statistics bootstrap — axiom_stats heap; StatsDef (row_count, ndv); bootstrapped at CREATE INDEX
- [x] 6.11 ✅ Auto-update statistics — StaleStatsTracker in SessionContext; marks stale after >20% row change
- [x] 6.12 ✅ ANALYZE [TABLE name [(column)]] — exact NDV full scan; resets staleness
- [x] 6.13 ✅ Index-only scans — `IndexOnlyScan` planner variant; `decode_index_key` inverse codec; `is_slot_visible` MVCC header-only check; `INCLUDE (cols)` DDL syntax + catalog storage; non-unique secondary indexes fixed to use `key||RecordId` format (InnoDB approach) — DuplicateKey on duplicate non-unique values fixed
- [x] ⚠️ 6.14 — MVCC on secondary indexes implemented in 7.3b (lazy delete + heap visibility)
- [x] 6.15 ✅ Index corruption detection — startup verifier compares every catalog index against heap-visible rows after WAL recovery; readable-but-divergent indexes are rebuilt from heap and their catalog roots rotated before traffic starts; unreadable / structurally broken trees fail open with `IndexIntegrityFailure`; shared by server and embedded open paths; SQL `REINDEX` remains deferred to 19.15 / 37.44
- [x] 6.16 ✅ Primary-key SELECT access path — `plan_select` / `plan_select_ctx` now allow PRIMARY KEY indexes as first-class single-table SELECT candidates; `WHERE pk = literal` bypasses the small-table / NDV cost gate and emits `IndexLookup` directly, while PK ranges reuse the existing `IndexRange` machinery; collation guard still rejects text PK index access under non-binary session collation; planner unit tests, SQL integration coverage, and wire smoke added
- [x] 6.17 ✅ Indexed UPDATE candidate fast path — `plan_update_candidates` / `plan_update_candidates_ctx` now choose `IndexLookup` or `IndexRange` for UPDATE discovery using PK, UNIQUE, secondary, and eligible partial indexes without a stats gate; `execute_update[_ctx]` materializes candidate RIDs before any mutation, fetches rows, rechecks the full `WHERE`, and then hands the survivors to the existing `5.20` stable-RID / fallback write path unchanged; planner unit tests, integration regressions, wire smoke, and `update_range` benchmark added
- [x] 6.18 ✅ Indexed multi-row INSERT batch path — immediate `INSERT ... VALUES (...), (... )` now reuses shared batch heap/index apply helpers even when the target table has PRIMARY KEY or secondary indexes; the path keeps strict same-statement UNIQUE semantics by avoiding the staged `committed_empty` shortcut and grouping index root persistence per flush instead; integration regressions, wire smoke, and the `insert_multi_values` benchmark added
- [x] 6.19 ✅ WAL fsync pipeline — leader-based FsyncPipeline, TxnManager deferred commit, WalSyncMethod, WalDurabilityPolicy, CommitCoordinator removed; 9 unit + 6 pipeline integration + wire smoke; all 1797 workspace tests pass
- [ ] ⚠️ 6.19 gap — single-conn ~224 ops/s: MySQL wire request-response, macOS APFS fsync limit; Linux fdatasync would yield ~5-10K; multi-conn batching works
- [x] 6.20 ✅ UPDATE apply fast path — `IndexLookup` / `IndexRange` candidate materialization now batches heap reads by page, UPDATE skips physical work for byte-identical rows while preserving current matched-row semantics, stable-RID rewrites batch `UpdateInPlace` WAL append through `reserve_lsns + write_batch`, and affected indexes now do grouped delete+insert with one root persistence write per index; targeted tests and wire smoke pass, and the release `update_range` benchmark improved from the `6.17` baseline `85.2K rows/s` to `369.9K rows/s` vs MariaDB `618K` / MySQL `291K`
- [x] ⚠️ PERF: DELETE bulk path — full-table DELETE (no WHERE) already uses root rotation fast path for ALL indexes (PK, UNIQUE, non-unique) since Phase 5.16; `write_leaf_same_pid` and `write_internal_same_pid` now use `into_page()` (avoids 16KB Page::new + struct copy); Phase 7.3b lazy delete eliminates per-row index delete for non-unique indexes on WHERE DELETE; batch_delete_leaf uses merge-scan O(N) per leaf
- [x] ⚠️ PERF: insert_leaf in-place optimization — non-split leaf inserts now modify the page directly via `PageRef::into_page()` + `cast_leaf_mut` + `write_page`; eliminates 16KB zeroed `Page::new` + 16KB struct copy; fast path in `insert_subtree` checks threshold before falling through to split path; `NodeCopy::read` (16KB copy) skipped entirely for non-split leaves
- [x] ⚠️ PERF: update_range optimization — eliminated per-row `current_values.clone()` in UPDATE executor (build new_values incrementally, only clone unchanged columns); `rewrite_batch_same_slot` uses `into_page()` instead of 16KB raw copy + `Page::from_bytes` (skip redundant CRC check); research: InnoDB logs only changed fields (not full tuple), PG uses HOT bitmask, SQLite uses ONEPASS; remaining gap vs MariaDB: WAL still logs full old+new tuple images (field-level WAL deferred)
- [x] ⚠️ PERF: point lookup wire optimization — row packet pre-allocation (`Vec::with_capacity(cols × 32)`); Int/BigInt/Bool fast path skips charset encoding (ASCII-only); UPDATE executor eliminates per-row `clone()` via incremental new_values build; research: InnoDB uses AHI hash bypass, PG uses plan cache after 5x, SQLite uses ONEPASS rowid seek; remaining: literal-normalized plan cache (27.8b) would eliminate 52% parse+analyze overhead
- [x] PERF: DELETE deferred index deletion (PostgreSQL model) — DELETE no longer touches ANY secondary index B-Tree (PK, unique, FK, non-unique); all entries left in place, filtered by heap visibility on read; dead entries cleaned by VACUUM; FK enforcement made heap-visibility-aware (RESTRICT, CASCADE, SET NULL all filter dead FK entries); uniqueness on INSERT handles dead PK entries via lookup+delete+reinsert; UPDATE still deletes old unique/PK keys (needed for correct index-based lookups after key change); eliminates ~120 B-Tree page ops + 10,000 allocs for 5000-row DELETE with 2 indexes

### Phase 7 — Concurrency + MVCC `🔄` week 40-48
- [x] ✅ Non-unique secondary indexes composite key — Fixed in 6.13: all non-unique non-FK secondary indexes now use `key||RecordId` format (same as FK auto-indexes). DuplicateKey blocker for MVCC removed.
- [x] 7.1 ✅ MVCC visibility rules — `IsolationLevel` + `SessionContext` now expose `READ COMMITTED`, `REPEATABLE READ`, and `SERIALIZABLE` (aliased to RR snapshot policy); `TxnManager::active_snapshot()` returns fresh snapshots for RC and frozen snapshots for RR/SERIALIZABLE; covered by `integration_isolation`
- [x] 7.2 ✅ Transaction manager — `TxnManager` with monotonic `next_txn_id` counter, `max_committed` tracking, `begin()/commit()` lifecycle, WAL-integrated; atomicity guaranteed by `&mut self` under `Arc<RwLock<Database>>`; implemented across 7.1-7.4
- [x] 7.3 ✅ Snapshot isolation — `TransactionSnapshot { snapshot_id, current_txn_id }` + `RowHeader::is_visible()` already enforce committed-before-snapshot visibility, read-your-own-writes, and committed-vs-uncommitted delete semantics; covered by heap visibility tests and `integration_table`
- [x] 7.3b ✅ MVCC on secondary indexes — InnoDB-style lazy index deletion: non-unique secondary index entries NOT removed on DELETE (heap visibility filters dead entries); unique/FK/PK indexes still deleted immediately (B-Tree enforces uniqueness); UPDATE of indexed col keeps old entry + inserts new (lazy delete for non-unique, immediate for unique/FK); heap-aware uniqueness check (`has_visible_duplicate`); `UndoOp::UndoIndexInsert` for ROLLBACK; HOT optimization in UPDATE skips index when no indexed col changed; visibility filtering added to all IndexLookup/IndexRange/IndexOnlyScan paths
- [x] 7.4 ✅ Lock-free readers with CoW — `Arc<RwLock<Database>>`; SELECT uses `db.read()` (concurrent, no blocking), DML uses `db.write()` (exclusive); `execute_read_only_with_ctx` takes `&dyn StorageEngine` shared ref; 7.4a production-safe storage: pwrite for writes, read-only Mmap, owned PageRef, deferred free queue
- [x] 7.5 ✅ Writer serialization — `Arc<RwLock<Database>>` guarantees exclusive writer access via `db.write()`; only 1 writer at a time (whole-database granularity); per-table locking deferred to Phase 13.7 (row-level locking)
- [x] 7.6 ✅ ROLLBACK — `TxnManager::rollback()` applies UndoOps in reverse: UndoInsert (mark_slot_dead), UndoDelete (clear_deletion), UndoUpdateInPlace (restore_tuple_image), UndoTruncate (clear_deletions_by_txn), UndoIndexInsert (BTree::delete_in); SQL `ROLLBACK` dispatched in executor; `discard_pending_inserts` for staged rows; implemented across Phase 3+7.3b
- [x] 7.7 ✅ Concurrency tests — 9 tokio integration tests: concurrent readers (8 parallel SELECTs), exclusive writer, sequential write consistency, interleaved read/write, delete visibility, index scan with dead entries (7.3b), vacuum cleanup, savepoint within transaction, concurrent insert+select monotonicity
- [x] 7.8 ✅ Epoch-based reclamation — `SnapshotRegistry` with atomic slot array (1024 slots); `register(conn_id, snapshot_id)` / `unregister(conn_id)` / `oldest_active()` for safe deferred page release; integrated in handler (read queries register/unregister around execution); stored in `Database` via `Arc`; 7 unit tests; forward-compatible with concurrent reader+writer model
- [x] 7.9 ✅ Resolve next_leaf CoW gap — predecessor leaf's `next_leaf` updated on every split via `update_predecessor_next_leaf()` (finds rightmost leaf of left sibling via parent descent); range iterator now follows `next_leaf` O(1) per leaf boundary instead of tree traversal O(log n); `descend_rightmost_leaf()` helper; `InsertResult::leaf_split` flag; removed tree-descent fallback from iter.rs
- [x] 7.10 ✅ Lock timeout — `tokio::time::timeout(lock_timeout, db.write())` in handler; `DbError::LockTimeout` with MySQL error 1205 + SQLSTATE 40001; `SET lock_timeout = N` (seconds, default 30, MySQL-compatible aliases: `lock_wait_timeout`, `innodb_lock_wait_timeout`); `session.lock_timeout_secs` in SessionContext; non-ignorable error in `is_ignorable_on_error`
- [x] 7.11 ✅ Basic MVCC vacuum — SQL `VACUUM [table]` command; heap vacuum: walk chain, `mark_slot_dead` for tuples where `txn_id_deleted < oldest_safe_txn`; index vacuum: full B-Tree scan of non-unique non-FK indexes, batch-delete dead entries; `oldest_safe_txn = max_committed + 1` under RwLock; returns statistics (dead_rows_removed, dead_index_entries_removed); parser + executor dispatch; 3 unit tests; autovacuum/compaction/axiom_bloat deferred
- [x] 7.12 ✅ SQL savepoints — `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name`, `RELEASE [SAVEPOINT] name`; stack-based model (MySQL/PostgreSQL/SQLite-compatible); ROLLBACK TO destroys later savepoints; RELEASE destroys target + later; COMMIT/ROLLBACK clears all; duplicate names allowed (most recent wins); uses existing TxnManager::savepoint/rollback_to_savepoint infrastructure
- [x] 7.13 ✅ Isolation tests — 9 Database-level tests: RR frozen snapshot, RC fresh snapshot, rollback hides insert/update/delete, savepoint partial rollback, nested savepoints, release savepoint, autocommit isolation, delete visibility, index query isolation; validates MVCC semantics at production Database layer
- [x] 7.14 ✅ Cascading rollback prevention — structurally prevented by MVCC visibility: `is_visible()` only returns rows where `txn_id_created` is committed (< snapshot_id); uncommitted rows from aborted txn A are never visible to txn B; READ COMMITTED and REPEATABLE READ both enforce this via `TransactionSnapshot::is_committed()`; no dirty reads possible by design
- [x] 7.15 ✅ Transaction ID overflow prevention — `begin_with_isolation()` checks `next_txn_id` against 50% and 90% of u64 capacity; logs `warn!` at 50%, `error!` at 90% with "VACUUM FREEZE required" message; u64 gives ~584,942 years at 1M txn/s; full VACUUM FREEZE deferred to Phase 34
- [ ] ⚠️ 7.16 MOVED to Phase 13 (13.18b) — Historical reads; requires MVCC (done here) but the SQL syntax and use cases belong with the rest of the temporal features in Phase 13
- [ ] ⚠️ 7.17 MOVED to Phase 13 (13.8b) — SELECT FOR UPDATE / SKIP LOCKED; belongs alongside row-level locking (13.7) which is its natural prerequisite
- [ ] ⚠️ 7.18 MOVED to Phase 19 (19.7b) — Cancel/kill query; operational feature, belongs with pg_stat_activity (19.7) and query management observability
- [ ] ⚠️ 7.19 MOVED to Phase 19 (19.21b) — Lock contention visibility; observability feature, belongs with the axiom_* monitoring views in Phase 19
- [ ] ⚠️ 7.20 DEFERRED to Phase 16 — Autonomous transactions require stored procedures (16.7) to exist first; moved to 16.10b

---

## BLOCK 2 — Execution Optimizations (Phases 8-10)

### Phase 8 — SIMD Optimizations `🔄` week 49-52
- [x] 8.1 ✅ Vectorized filter — BatchPredicate: zero-alloc raw-byte WHERE evaluation on encoded row data; compiles `col op literal` + AND-conjunctions into pre-compiled checks; ~6× faster predicate eval (~20 ns/row vs ~130 ns/row); select_where 85K→210K rows/s (paridad con MariaDB 211K)
- [x] 8.1b ✅ Low-cardinality filter specialization — cerrada por BatchPredicate (8.1); `WHERE active = TRUE` ahora usa raw-byte Bool comparison sin decode; benchmark target alcanzado: 210K rows/s vs objetivo 204K
- [x] 8.2 ✅ SIMD with `wide` crate — cross-platform (AVX2 on x86, NEON on ARM, scalar fallback); batch_cmp_i32 processes 8×i32 per AVX2 op; gather-scatter pattern for row-oriented storage; eval_batch() replaces per-row eval_on_raw(); MSRV 1.80→1.89
- [x] 8.3 ✅ Improved query planner — full-analyzed plan cache (skip parse+analyze on cache hit), PK bloom filter skip, SMALL_TABLE_THRESHOLD 1000→100; select_pk 9.0K→9.4K (remaining gap to MariaDB 13.3K is wire protocol latency)
- [x] 8.3b ✅ Zone maps (per-page min/max) — stored in PageHeader._reserved[8..26]; heap scanner skips pages where zone map predicate doesn't match; update_zone_map() uses i64::MIN/MAX bounds on first init for pages with existing rows (correctness fix for rollback scenarios)
- [x] 8.3c ✅ Full-scan throughput parity on wire — select full scan 205K rows/s vs MariaDB 207K (99.1% parity); achieved via two-phase decode + selection mask + BatchPredicate + wire serialization fast path
- [x] 8.3d ✅ Wire row serialization fast path — build_row_into() with reusable buffer (MySQL net->buff model); ASCII fast path for Text; stack-based integer/date/timestamp formatting; select 205K rows/s (was 173K)
- [x] 8.4 ✅ Basic EXPLAIN — MySQL-compatible output (id, select_type, table, type, key, rows, Extra); shows access method (ALL/ref/range), index used, estimated row count
- [x] 8.5 ✅ SIMD vs MySQL benchmarks (2026-04-01, 5000 rows, Apple Silicon ARM NEON):
  - 🚀 select_where: **216K r/s** vs MariaDB 199K / MySQL 208K — **supera ambos**
  - 🚀 count: **9.1K q/s** vs MariaDB 1.8K / MySQL 1.5K — **5× MariaDB, 6× MySQL**
  - ✅ select: 226K r/s vs MariaDB 228K / MySQL 224K — paridad
  - ✅ insert: 31K r/s vs MariaDB 41K / MySQL 31K — paridad MySQL
  - ✅ aggregate: 611 q/s vs MariaDB 571 / MySQL 911 — supera MariaDB
  - ✅ update: 377K r/s vs MySQL 341K — **supera MySQL 1.1×**
  - ⚠️ select_pk: 9.2K vs MariaDB 13K (wire latency dominant)
  - ⚠️ select_range: 154K vs MariaDB 205K (per-query overhead)
  - ⚠️ delete: 293K vs MariaDB 1.2M (InnoDB purge-thread model)
  - Internal: parser 566ns, eval eq 19ns, scan 1K rows 91µs, codec encode 39ns
- [x] 8.5b ✅ OLTP benchmark matrix — `oltp_matrix.py` runs all scenarios × {no-index, idx(active,age)}; generates Markdown table + attribution analysis; findings: select_where 227K no-idx (✅) but drops to 101K with idx (planner prefers index over BatchPredicate scan — regression to investigate in Phase 9)
- [x] 8.6 ✅ SIMD correctness tests — 12 tests in simd.rs (eq/gt/lt/noteq/lteq/gteq, remainder, pre-filtered, min/max, AND chain, negative values) + 10 batch.rs tests
- [x] 8.7 ✅ Runtime CPU feature detection — `wide` crate handles AVX2/SSE/NEON dispatch internally; scalar fallback on unsupported CPUs; single binary works on x86_64 + aarch64
- [x] 8.8 ✅ SIMD vs scalar vs MySQL benchmark — SIMD batch (NEON 4×i32) vs scalar BatchPredicate: marginal gain on 5K rows (gather-scatter overhead); real benefit on x86_64 AVX2 (8×i32) and larger datasets; documented in progreso.md 8.5 results

### Phase 9 — DuckDB-inspired + Join Algorithms `🔄` week 53-56
- [x] 9.1 ✅ Morsel-driven parallelism — Rayon par_iter over heap pages; StorageEngine: Send+Sync; two-phase (serial page-ID collection + parallel decode+filter); BatchPredicate SIMD works across threads; falls back to serial for <4 pages; select_where 228K r/s maintained
- [x] 9.2 ✅ Operator fusion — unified decode mask (SELECT∪WHERE∪ORDER BY∪GROUP BY); scan+filter+project fused: decode_row_masked skips non-referenced columns, saving String/Text allocation for wide tables; wildcard SELECT * decodes all (correct fallback)
- [x] 9.3 ✅ Late materialization — achieved via BatchPredicate (8.1: raw-byte filter = zero decode for non-matching rows) + unified decode mask (9.2: skip non-referenced columns); full DuckDB-style RecordId-only pipeline not needed for row store (pages already cached)
- [x] 9.4 ✅ Benchmarks with parallelism — select_where 228K r/s with Rayon parallel scan; marginal gain at 5K rows (~25 pages) due to Rayon spawn overhead; infrastructure ready for larger datasets
- [x] 9.5 ✅ Vectorized correctness tests — 12 SIMD tests (simd.rs) + 10 batch tests (batch.rs) + full workspace test suite passes with parallel+fusion+late-mat enabled; serial fallback for <4 pages verified
- [x] 9.5b ✅ Aggregate execution parity — fast-path column extraction for GROUP BY + accumulators (skip eval() for column refs); aggregate 611→631 q/s (+3%); remaining gap to MySQL (822) is wire protocol overhead, not aggregation logic
<!-- Join algorithms: nested loop (4.8) is O(n*m); hash and sort-merge are essential for real queries -->
- [x] 9.6 ✅ Hash join — build phase (hash right table by join key) + probe phase (lookup left rows); O(n+m) for INNER/LEFT equijoin; detect_equijoin() extracts col indices from ON; HashableValue wrapper for HashMap key; falls back to nested loop for non-equijoin/USING/RIGHT/FULL/small tables; 25 existing join tests pass
- [x] 9.7 ✅ Sort-merge join — sort both sides by join key + merge with mark/restore for duplicates (PostgreSQL pattern); INNER + LEFT variants; cmp_values_for_join with NULL-last ordering; available for Phase 9.9 adaptive selection
- [ ] 9.8 ⏳ Spill to disk — when hash table or sort buffer exceeds `work_mem`, spill to temp files; no OOM on large joins
- [x] 9.9 ✅ Adaptive join selection — equijoin+large→hash (INNER/LEFT/RIGHT/FULL), non-equijoin/CROSS/small→nested loop; hash_join_full with matched-right bitmap; research-verified against PostgreSQL nodeHashjoin.c + DuckDB physical_hash_join.cpp + PostgreSQL nodeMergejoin.c; fixes: strictly-less tie-breaking, explicit NULL handling, merge-left restore optimization
- [x] 9.10 ✅ Join algorithms benchmarks — join_bench.py: INNER 2K×4K AxiomDB 12.4ms vs MySQL 12.5ms (parity); LEFT 2K×4K AxiomDB 14.7ms vs MariaDB 162ms (11× faster — MariaDB uses nested loop); fixed col_idx combined→local space bug + autocommit snapshot for JOIN path
- [ ] 9.11 ⏳ Streaming result iterator — `execute_streaming()` returns `impl Iterator<Item=Row>` instead of materializing `Vec<Row>` before returning; wire protocol sends rows as they are produced (no full buffer required); SELECT without ORDER BY can terminate early when LIMIT is reached without scanning the rest of the table; eliminates per-query allocation proportional to result size; MySQL C API has `USE_RESULT` mode, PostgreSQL has server-side cursors — AxiomDB needs an equivalent for competing on large result sets without OOM; prerequisite for cursor-based pagination (Phase 13+); depends on operator fusion (9.2) being lazy internally

### Phase 10 — Embedded mode + FFI `⏳` week 57-60
- [ ] 10.1 ⏳ Refactor engine as reusable `lib.rs`
- [ ] 10.2 ⏳ C FFI — `axiomdb_open`, `axiomdb_execute`, `axiomdb_close` with `#[no_mangle]`
- [ ] 10.3 ⏳ Compile as `cdylib` — `.so` / `.dll` / `.dylib`
- [ ] 10.4 ⏳ Python binding — working `ctypes` demo
- [ ] 10.5 ⏳ Embedded test — same DB used from server and from library
- [ ] 10.6 ⏳ Node.js binding (Neon) — native `.node` module for Electron and Node apps; async/await API
- [ ] 10.7 ⏳ Embedded vs server benchmark — compare in-process vs TCP loopback latency to demonstrate embedded advantage
- [ ] 10.8 ⏳ PreparedStatement Rust API — `db.prepare(sql) -> PreparedStatement`; `stmt.execute(params: &[Value])` runs N times reusing the parsed + analyzed plan without calling parse/analyze again; separate from COM_STMT_PREPARE (5.10, wire-only) — this is for the embedded Rust API where there is no MySQL wire protocol; eliminates parse+analyze overhead in tight loops (primary cause of the 15-24x INSERT gap vs SQLite in embedded benchmarks); MySQL C API has `mysql_stmt_prepare()`, libpq has `PQprepare()`, SQLite has `sqlite3_prepare_v2()` — all three competitors require this for serious embedded performance; must invalidate the cached plan on DDL changes (reuse schema_version mechanism from 5.13); implement after 10.1 (lib.rs refactor) since the public API shape depends on it

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

### Phase 11 — Robustness and indexes `⏳` week 61-64
- [ ] 11.1 ⏳ Sparse index — one entry every N rows for timestamps
- [ ] 11.1b ⏳ BRIN indexes — Block Range INdex; stores only min/max per block range (128 pages default); `CREATE INDEX ON events USING brin(created_at)`; occupies ~100x less space than B-Tree; only useful for columns that are physically ordered on disk (timestamps, auto-increment IDs, IOT sensor readings in arrival order); O(1) build, near-zero maintenance; planner uses it for range scans when column correlation is high; benchmark vs B-Tree on 10M-row time-series table
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

### Phase 12 — Testing + JIT `⏳` week 65-68
- [ ] 12.1 ⏳ Deterministic simulation testing — `FaultInjector` with seed
- [ ] 12.2 ⏳ EXPLAIN ANALYZE — real times per plan node; JSON output format compatible with PostgreSQL (`{"Plan":{"Node Type":..., "Actual Rows":..., "Actual Total Time":..., "Buffers":{}}}`) and indented text format for psql/CLI; metrics: actual rows, loops, shared/local buffers hit/read, planning time, execution time
- [ ] 12.3 ⏳ Basic JIT with LLVM — compile simple predicates to native code
- [ ] 12.4 ⏳ Final block 1 benchmarks — compare with MySQL and SQLite
- [ ] 12.5 ⏳ SQL parser fuzz testing — `cargo fuzz` on the parser with random inputs; register crashes as regression tests
- [ ] 12.6 ⏳ Storage fuzz testing — pages with random bytes, deliberate corruptions; verify that crash recovery handles corrupted data
- [ ] 12.7 ⏳ ORM compatibility tier 1 — Django ORM and SQLAlchemy connect, run simple migrations and SELECT/INSERT/UPDATE/DELETE queries without errors; document workarounds if any
- [ ] 12.8 ⏳ Unified axiom_* observability system — all system views use consistent naming, types, and join keys; `SELECT * FROM axiom_queries` shows running queries with pid, duration, state, sql_text, plan_hash; `SELECT * FROM axiom_bloat` shows table bloat (from 7.11); `SELECT * FROM axiom_slow_queries` is auto-populated when query exceeds `slow_query_threshold` (default 1s); `SELECT * FROM axiom_stats` shows database-wide metrics (cache hit rate, rows read/written, lock waits); `SELECT * FROM axiom_index_usage` shows which indexes are used/unused; unlike MySQL's inconsistent SHOW commands and PostgreSQL's complex pg_catalog joins, every axiom_* view is self-documented, joinable, and has the same timestamp/duration formats
- [ ] 12.9 ⏳ Date/time validation strictness — `'0000-00-00'` is always rejected with a clear error (MySQL allows this invalid date); `TIMESTAMP WITH TIME ZONE` is the single timestamp type with explicit timezone; no silent timezone conversion based on column type; `'2024-02-30'` is always an error; `'2024-13-01'` is always an error; retrocompatible: `SET AXIOM_COMPAT='mysql'` re-enables MySQL's lenient date behavior for migration

### Phase 13 — Advanced PostgreSQL `⏳` week 69-72
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — native pub-sub with `DashMap` of channels
- [ ] 13.5 ⏳ Covering indexes — store INCLUDE column values in B+ Tree leaf nodes; 6.13 already has catalog storage + IndexOnlyScan for key columns only; this phase adds the actual value payload to the leaf layout so index-only scans can return non-key projected columns without touching the heap
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + atomic swap
- [ ] 13.7 ⏳ Row-level locking — lock specific row during UPDATE/DELETE; reduces contention vs per-table lock from 7.5
- [ ] 13.8 ⏳ Deadlock detection — DFS on wait graph when lock_timeout expires; kill the youngest transaction
- [ ] 13.8b ⏳ SELECT FOR UPDATE / SKIP LOCKED — pessimistic row locking for job queues, inventory checkout, concurrent reservations; `SELECT ... FOR UPDATE` acquires row lock until COMMIT/ROLLBACK; `SKIP LOCKED` skips already-locked rows instead of blocking; requires row-level locking (13.7); also covers `UPDATE t SET qty=qty-1 WHERE id=? AND qty>0` optimistic CAS pattern returning 0 rows on conflict (moved from 7.17)
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
- [ ] 13.18b ⏳ Historical reads — `BEGIN READ ONLY AS OF TIMESTAMP '2023-12-31 23:59:59'` anchors the snapshot to a past point in time; MVCC already stores the data (Phase 7), this adds the SQL syntax and executor support; critical for auditing financial data at a specific date without exporting; precursor to the full bi-temporal model in 13.18 (moved from 7.16)
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
- [ ] 16.10 ⏳ Built-in connection pooler
- [ ] 16.10b ⏳ Autonomous transactions — `PRAGMA AUTONOMOUS_TRANSACTION` on a stored procedure makes it run in an independent transaction; `COMMIT` inside commits only that procedure's changes; if outer transaction does `ROLLBACK`, the autonomous transaction's changes are preserved; critical for audit logging that persists even when the main operation fails; requires 16.7 (stored procedures) first (moved from 7.20) — Pgbouncer-equivalent implemented inside the engine; multiplexes N application connections into M database backend connections (N >> M); transaction-mode pooling (connection returned to pool after each COMMIT/ROLLBACK); session variables reset between borrows; eliminates the need for external Pgbouncer/Pgpool deployment; critical for any app with >100 concurrent users since creating one OS thread per TCP connection does not scale

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
- [ ] 19.7b ⏳ Cancel / kill query — `SELECT axiom_cancel_query(pid)` sends cancellation signal to a running query (like `pg_cancel_backend`); `axiom_terminate_session(pid)` forcibly closes a connection; without this, a runaway `SELECT * FROM logs` (millions of rows) cannot be stopped without restarting the server; integrates with pg_stat_activity (19.7) to expose the pid (moved from 7.18)
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
- [ ] 19.21 ⏳ performance_schema equivalent — `axiom_performance_schema` namespace with: `events_statements_current` (running queries with digest, timer, rows_examined), `events_statements_history` (last 10 per connection), `events_waits_current` (lock waits, I/O waits), `table_io_waits_summary_by_table` (read/write latency per table), `file_io_summary` (bytes read/written per file); activated via `SET axiom_performance_schema = ON`; zero overhead when off (unlike MySQL where it's always on); MySQL monitoring tools (PMM, Datadog, New Relic MySQL integration) query these tables — this makes those tools work with AxiomDB without a custom plugin
- [ ] 19.21b ⏳ Lock contention visibility — `SELECT * FROM axiom_lock_waits` shows: waiting_pid, blocking_pid, waiting_query, lock_type, wait_duration; `SELECT * FROM axiom_locks` shows all currently held locks; essential for diagnosing deadlocks in production without guessing; sits alongside the rest of the axiom_* monitoring views (moved from 7.19)
- [ ] 19.20 ⏳ Audit trail infrastructure — write audit logs async (circular buffer, without blocking writer); JSON format with: user, IP, SQL, bind params, rows_affected, duration, result; daily rotation; prerequisite for 17.7 (CREATE AUDIT POLICY)

---

## BLOCK 6 — Complete Types and SQL (Phases 20-21)

### Phase 20 — Types + import/export `⏳` week 47-48
- [ ] 20.1 ⏳ Regular views — `CREATE VIEW` and updatable views
- [ ] 20.2 ⏳ Sequences — `CREATE SEQUENCE`, `NEXTVAL`, `CURRVAL`
- [ ] 20.3 ⏳ ENUMs — `CREATE TYPE ... AS ENUM` with validation and semantic order
- [ ] 20.4 ⏳ Arrays — `TEXT[]`, `FLOAT[]`, `ANY()`, `@>`
- [ ] 20.5 ⏳ COPY FROM/TO — import/export CSV, JSON, JSONL
- [ ] 20.5b ⏳ SELECT … INTO OUTFILE — `SELECT id, name FROM users INTO OUTFILE '/tmp/users.csv' FIELDS TERMINATED BY ',' ENCLOSED BY '"' LINES TERMINATED BY '\n'`; MySQL syntax for exporting query results directly to a file on the server; complement of `LOAD DATA INFILE`; used in ETL pipelines and scheduled data exports; server-side write (unlike COPY TO CLIENT which sends data over wire)
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
- [ ] 21.5b ⏳ REPLACE INTO — `REPLACE INTO users (id, name) VALUES (1, 'Alice')`; MySQL shorthand for DELETE-then-INSERT; if the row does not exist it inserts; if it does, it deletes the old row and inserts the new one (triggers ON DELETE + ON INSERT, unlike ON DUPLICATE KEY UPDATE which triggers ON UPDATE); AUTO_INCREMENT increments on replace; very common in MySQL codebases for upsert-by-PK patterns
- [ ] 21.5c ⏳ INSERT IGNORE — `INSERT IGNORE INTO tags (post_id, tag) VALUES (1, 'rust')`; silences unique/FK/NOT NULL violations and inserts only the rows that don't conflict; returns warning count instead of error; used extensively for idempotent imports, tag systems, and bulk loads where partial success is acceptable
- [ ] 21.5d ⏳ Multi-table UPDATE/DELETE — `UPDATE orders o JOIN customers c ON o.customer_id = c.id SET o.priority = c.tier WHERE c.country = 'CO'`; and `DELETE o FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.deleted_at IS NOT NULL`; MySQL-specific syntax widely used in data migrations and cleanup scripts; different from standard SQL MERGE — simpler for the common "join + update/delete" pattern
- [ ] 21.6 ⏳ CHECK constraints + DOMAIN types
- [ ] 21.6b ⏳ Exclusion constraints — `CREATE TABLE reservations (..., EXCLUDE USING btree (room_id WITH =, period WITH &&))`; prevents rows where ALL specified operators return TRUE simultaneously; B-Tree exclusion for equality (e.g., no duplicate active slugs); full range-overlap exclusion (hotel rooms, calendar slots, parking spots) requires GiST index (Phase 30.2); use case: `EXCLUDE USING gist (room WITH =, during WITH &&)` guarantees no two reservations overlap the same room in the same time period — impossible to enforce with CHECK or UNIQUE; `period` requires range type (Phase 20.13); document B-Tree subset now, GiST full power after Phase 30.2
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

### Phase 22b — Platform features `🔄` week 55-57
- [ ] 22b.1 ⏳ Scheduled jobs — `cron_schedule()` with `tokio-cron-scheduler`
- [ ] 22b.2 ⏳ Foreign Data Wrappers — HTTP + PostgreSQL as external sources
- [x] 22b.3a ✅ Database catalog + `CREATE/DROP DATABASE` — persisted `axiom_databases`, catalog-backed `SHOW DATABASES`, validated `USE` / `COM_INIT_DB`, legacy tables default to `axiomdb`
- [ ] 22b.3b ⏳ Cross-database queries — `database.schema.table`, cross-db SELECT / JOIN / DML
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

### Phase 22e — Native Toolkit System `⏳` week 64-67

> **Design:** `db.md` § "Native Toolkit System" — the complete spec.
> Toolkits are built-in domain packs (blog, ecommerce, iot, saas, analytics) that activate
> types, functions, schema templates, optimizer hints, and monitoring views with one SQL command.
> Zero external dependencies — everything compiled into the binary.

#### 22e.A — Core infrastructure
- [ ] 22e.1 ⏳ `INSTALL TOOLKIT` / `UNINSTALL TOOLKIT` / `LIST TOOLKITS` — DDL parser + executor; persists activation in `axiom_toolkits` catalog table; one row per installed toolkit with name, version, installed_at
- [ ] 22e.2 ⏳ `DESCRIBE TOOLKIT name` — shows types, functions, templates, and monitoring views provided by the toolkit
- [ ] 22e.3 ⏳ `axiom_toolkits` system view — name, version, installed_at, objects_count
- [ ] 22e.4 ⏳ `axiom_toolkit_objects` system view — object_type, object_name, schema, toolkit
- [ ] 22e.5 ⏳ `axiom_toolkit_functions` system view — function_name, signature, toolkit, description
- [ ] 22e.6 ⏳ Schema templates — `CREATE TABLE t LIKE TOOLKIT blog.posts`; generates DDL with best-practice column definitions, constraints, indexes, and RLS policies for the template; does NOT auto-create tables
- [ ] 22e.7 ⏳ Toolkit optimizer hints — planner reads `axiom_toolkits` at session start; adjusts prefetch strategy, join preference, and index suggestion thresholds based on declared workload (read-heavy/write-heavy/analytical)
- [ ] 22e.8 ⏳ Toolkit combinability — multiple toolkits can be installed simultaneously; their namespaces are orthogonal (`toolkit_blog.*`, `toolkit_saas.*`); conflict detection for overlapping type names

#### 22e.B — Toolkit: blog
- [ ] 22e.10 ⏳ Domain types — `SLUG TEXT CHECK (value ~ '^[a-z0-9][a-z0-9-]*[a-z0-9]$')`, `POST_STATUS ENUM('draft','published','scheduled','archived')`, `READING_LEVEL ENUM('easy','moderate','advanced')`
- [ ] 22e.11 ⏳ Domain functions — `SLUG(text)→TEXT` (normalizes to URL-safe slug), `EXCERPT(text, max_words INT)→TEXT`, `READING_TIME(text)→INT` (minutes at 200 wpm), `WORD_COUNT(text)→INT`, `EXTRACT_HEADINGS(text)→TEXT[]`, `RANK_POSTS(query TEXT, col TEXT)→REAL` (BM25 + recency score)
- [ ] 22e.12 ⏳ Schema templates — `blog.posts` (id, title, slug SLUG, content, excerpt, author_id, status POST_STATUS, published_at, fts_vector; + partial index on published_at WHERE status='published', FTS index), `blog.comments` (with parent_id for nesting), `blog.tags`, `blog.post_tags`, `blog.categories` (with ltree path)
- [ ] 22e.13 ⏳ Monitoring — `axiom_blog_stats` (post_count by status, draft_count, avg_reading_time, comment_count_today, top_tags TEXT[])

#### 22e.C — Toolkit: ecommerce
- [ ] 22e.20 ⏳ Domain types — `MONEY` composite `(amount DECIMAL(12,4), currency CHAR(3))` with `+`, `-`, `*` operators, `SKU TEXT CHECK (value ~ '^[A-Z0-9][A-Z0-9\-_]{1,63}$')`, `ORDER_STATUS ENUM('pending','confirmed','processing','shipped','delivered','cancelled','refunded')`
- [ ] 22e.21 ⏳ Domain functions — `APPLY_TAX(amount, country CHAR(2), category TEXT)→MONEY`, `CONVERT_CURRENCY(amount DECIMAL, from CHAR(3), to CHAR(3))→DECIMAL` (uses `axiom_exchange_rates`), `NEXT_INVOICE_NUM(series TEXT)→TEXT` (gapless sequence, same guarantee as 13.10)
- [ ] 22e.22 ⏳ Inventory functions — `RESERVE_INVENTORY(sku, qty INT, session_id TEXT)→BIGINT` (returns reservation_id), `COMMIT_RESERVATION(reservation_id BIGINT)→BOOL`, `RELEASE_RESERVATION(reservation_id BIGINT)→BOOL`; reservations stored in `toolkit_ecommerce.reservations` with TTL
- [ ] 22e.23 ⏳ Schema templates — `ecommerce.products`, `ecommerce.inventory` (sku, stock, reserved, available as generated column), `ecommerce.orders`, `ecommerce.order_items`, `ecommerce.invoices` (gapless seq, fiscal period aware)
- [ ] 22e.24 ⏳ Monitoring — `axiom_inventory_status` (sku, stock, reserved, available), `axiom_order_pipeline` (orders by status + age bucket), `axiom_revenue_today` (total by currency)

#### 22e.D — Toolkit: iot
- [ ] 22e.30 ⏳ Domain types — `DEVICE_STATUS ENUM('active','inactive','error','maintenance')`, `READING_QUALITY ENUM('good','uncertain','bad')`
- [ ] 22e.31 ⏳ Domain functions — `TIME_BUCKET(bucket INTERVAL, ts TIMESTAMP)→TIMESTAMP` (like TimescaleDB), `DEAD_BAND(new_val REAL, prev_val REAL, threshold REAL)→BOOL`, `INTERPOLATE_LOCF(ts TIMESTAMP, val REAL)→REAL`, `INTERPOLATE_LINEAR(ts1 TIMESTAMP, v1 REAL, ts2 TIMESTAMP, v2 REAL, target TIMESTAMP)→REAL`, `SENSOR_DRIFT(readings REAL[], expected REAL)→REAL`
- [ ] 22e.32 ⏳ Schema templates — `iot.devices` (id, name, type, location POINT, status), `iot.readings` (device_id, ts, value, quality; auto-partitioned by month, BRIN on ts, TTL configurable), `iot.alerts` (device_id, ts, severity, message, resolved_at)
- [ ] 22e.33 ⏳ Monitoring — `axiom_device_status` (last_seen, reading_count_24h, alert_count_open per device), `axiom_data_freshness` (table, last_insert, expected_interval, status), `axiom_sensor_health` (devices silent for > expected interval)

#### 22e.E — Toolkit: saas
- [ ] 22e.40 ⏳ Domain types — `TENANT_ID BIGINT NOT NULL`, `SUBSCRIPTION_TIER ENUM('free','starter','pro','enterprise')`
- [ ] 22e.41 ⏳ Domain functions — `CURRENT_TENANT()→BIGINT` (reads from session variable `app.tenant_id`), `TENANT_QUOTA_CHECK(resource TEXT, amount BIGINT)→BOOL` (consults `axiom_quota_limits`), `ANONYMIZE(text TEXT)→TEXT` (SHA-256 prefix, GDPR-safe), `MASK_PII(text TEXT, policy TEXT)→TEXT`
- [ ] 22e.42 ⏳ Auto-RLS — when saas toolkit is active, `CREATE TABLE` with a `tenant_id` column automatically gets a RLS policy `USING (tenant_id = CURRENT_TENANT())`; opt-out via `WITH (no_toolkit_rls = true)`
- [ ] 22e.43 ⏳ Schema templates — `saas.tenants`, `saas.subscriptions`, `saas.audit_log` (immutable, append-only via 13.9), `saas.quota_usage`
- [ ] 22e.44 ⏳ Monitoring — `axiom_tenant_usage` (tenant_id, storage_bytes, row_count, queries_today), `axiom_quota_alerts` (tenants at >80% of any quota), `axiom_compliance_log` (accesses to PII columns with user + timestamp)

#### 22e.F — Toolkit: analytics
- [ ] 22e.50 ⏳ Domain functions — `PERCENTILE_RANK(value REAL, dataset REAL[])→REAL`, `Z_SCORE(value REAL, mean REAL, stddev REAL)→REAL`, `MOVING_AVG(col, window_size INT)→REAL` (sugar for window function), `COHORT_DATE(ts TIMESTAMP, granularity TEXT)→DATE` ('week'/'month'/'quarter'), `RETENTION_RATE(cohort_date DATE, event_date DATE)→REAL`, `FUNNEL_STEP(user_id BIGINT, step INT, ts TIMESTAMP)→BOOL`
- [ ] 22e.51 ⏳ Schema templates — `analytics.events` (user_id, event TEXT, ts, properties JSON; GIN on properties), `analytics.sessions` (session_id, user_id, started_at, ended_at, event_count), `analytics.funnels` (funnel_id, step_order, event_name, description)
- [ ] 22e.52 ⏳ Monitoring — `axiom_query_stats` (top queries by cost + frequency), `axiom_slow_analytical` (analytical queries > threshold), `axiom_cache_efficiency` (buffer pool hit rate per table)

#### 22e.G — Quality
- [ ] 22e.60 ⏳ Toolkit combination tests — install blog+saas, ecommerce+saas, iot+analytics; verify no namespace conflicts, RLS applies correctly, optimizer hints don't conflict
- [ ] 22e.61 ⏳ Schema template tests — `CREATE TABLE LIKE TOOLKIT x.y`; verify generated DDL compiles, indexes are created, RLS policies are attached
- [ ] 22e.62 ⏳ Domain function tests — unit tests for every toolkit function; edge cases (empty string, NULL, overflow, invalid currency code)
- [ ] 22e.63 ⏳ Monitoring view tests — insert test data, verify all `axiom_*` views return correct aggregates
- [ ] 22e.64 ⏳ Documentation — user guide page per toolkit: SQL examples, schema template output, monitoring queries, combination guide

---

### Phase 23 — Backwards compatibility `⏳` week 68-71
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
- [ ] 24.14b ⏳ MySQL type aliases — `TINYTEXT` (≤255B), `MEDIUMTEXT` (≤16MB), `LONGTEXT` (≤4GB) stored as TEXT with length constraint; `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB` stored as BLOB with limit; `ZEROFILL` display attribute on integer columns (`INT(10) ZEROFILL` pads with zeros on display, stored as normal INT); `SET('a','b','c')` multi-value type (stores a bitmask, displays as comma-separated subset of declared values; different from ENUM which allows one value); these types are required to import `mysqldump` output without manual schema rewriting
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
- [ ] 27.8b ⏳ Literal-normalized COM_QUERY plan cache — normalize simple repeated SQL strings that differ only in literals (`id = 42` vs `id = 43`) so parse+analyze/plan can be reused even outside `COM_STMT_PREPARE`; target benchmark: repeated point lookups over the MySQL wire
- [ ] 27.8c ⏳ Repeated DML COM_QUERY reuse — extend literal-normalized plan reuse to repeated `INSERT` / `UPDATE` statements sent as plain COM_QUERY so loops of single-row statements in one transaction do not pay full parse+analyze each time; target benchmark: `local_bench.py --scenario insert --rows 5000` where AxiomDB is still below MariaDB/MySQL despite one explicit transaction
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
- [ ] 29.19 ⏳ CONVERT_TZ() — `CONVERT_TZ(ts, 'UTC', 'America/Bogota')` converts a TIMESTAMP between timezone identifiers; uses embedded tzdata (29.6); prerequisite for apps that store UTC internally and display in local time per user; `@@global.time_zone` and `@@session.time_zone` variables affect implicit conversion
- [ ] 29.20 ⏳ BIT aggregates — `BIT_AND(flags)`, `BIT_OR(flags)`, `BIT_XOR(flags)` aggregate functions; `BIT_OR` used for permission bitmask accumulation (`SELECT BIT_OR(permission_mask) FROM roles WHERE user_id = ?`); `BIT_XOR` used for row checksums (change detection without hashing); all skip NULL values per SQL standard

### Phase 30 — Pro infrastructure `⏳` week 85-87
- [ ] 30.1 ⏳ GIN indexes — for arrays, JSONB and trigrams
- [ ] 30.2 ⏳ GiST indexes — for ranges and geometry
- [ ] 30.3 ⏳ BRIN advanced — multi-column BRIN, custom `pages_per_range`, `BRIN_SUMMARIZE_NEW_VALUES()`, integration with GiST for geometric ranges (basic BRIN implemented in 11.1b)
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
- [ ] ⚠️ 31.1 duplicate of 17.22 — Encryption at rest (TDE AES-256-GCM) is tracked there; remove this item when 17.22 is implemented
- [ ] ⚠️ 31.2 duplicate of 17.16 — Dynamic data masking + helper functions (`MASK_EMAIL`, `MASK_PHONE`) tracked there; remove this item when 17.16 is implemented
- [ ] 31.3 ⏳ SQL-level PREPARE / EXECUTE — `PREPARE name AS SELECT ...` / `EXECUTE name(params)` syntax (PostgreSQL-style named prepared statements in SQL); distinct from 5.13 (wire plan cache) and 10.8 (Rust embedded API); targets interactive sessions and stored procedures
- [ ] 31.4 ⏳ Extended statistics — column correlations (`CREATE STATISTICS`) for multi-column dependency awareness in the planner
- [x] 31.5 ✅ FULL OUTER JOIN — implemented in Phase 4.8b (moved earlier as planned)
- [ ] ⚠️ 31.6 duplicate of 13.14 — Custom aggregate functions tracked there; remove this item when 13.14 is implemented
- [ ] 31.7 ⏳ Geospatial — `POINT`, `ST_DISTANCE_KM`, R-Tree index (`rstar`)
- [ ] 31.8 ⏳ Query result cache — automatic invalidation by table
- [x] 31.9 ✅ Strict mode — already implemented in 4.25c; no action needed
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

#### 35.0 — AxiomStudio (UI built, needs wire-up post Phase 8)
> **Status:** UI complete with mock data (2026-03-24). Connection layer pending Phase 8 (wire protocol).
> All features are implemented and documented in `studio/CONNECT.md`.

- [x] 35.0.1 ✅ Core layout — sidebar, dark theme, Geist fonts, routing
- [x] 35.0.2 ✅ Dashboard — metrics cards, sparklines, recent queries, slow queries, live auto-refresh
- [x] 35.0.3 ✅ Query Editor — Monaco SQL/AxiomQL, tabs, ⌘↵, history, saved queries, export CSV
- [x] 35.0.4 ✅ Query Editor extras — split view, format SQL, variables ($name), chart (SVG bar)
- [x] 35.0.5 ✅ Monaco intelligence — AxiomQL syntax highlighting, SQL autocompletion (tables/columns)
- [x] 35.0.6 ✅ SQL ↔ AxiomQL translator (heuristic, replaces with real parser in Phase 36)
- [x] 35.0.7 ✅ Tables browser — grid of tables/views with row count, size, last updated
- [x] 35.0.8 ✅ Table detail — Data tab (inline edit, boolean toggle, add/delete row, filter, column visibility, copy DDL, right-click context menu)
- [x] 35.0.9 ✅ Table detail — Schema tab (type dropdown, nullable toggle, FK editor)
- [x] 35.0.10 ✅ Table detail — Indexes tab (add, edit inline, delete)
- [x] 35.0.11 ✅ SQL + AxiomQL preview after every edit (shows generated statement)
- [x] 35.0.12 ✅ Settings page — connections manager, engine config, studio prefs, security, about
- [x] 35.0.13 ✅ Command palette ⌘K — tables, actions, recent queries
- [ ] 35.0.14 ⏳ Wire up to real AxiomDB API — replace mock data with `/_api/` calls (Phase 8+)
- [ ] 35.0.15 ⏳ AxiomDB serves Studio — `axiomdb-server` serves `studio/out/` at `/studio` route
- [ ] 35.0.16 ⏳ Real-time features — `.watch()` reactive queries via WebSocket (Phase 8+)
- [ ] 35.0.17 ⏳ EXPLAIN plan visualization — tree/graph rendering of real explain output (Phase 5+)
- [ ] 35.0.18 ⏳ ER diagram — visual relationships between tables (Phase 8+)

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

---

## USE CASE PROFILES

Each profile maps a target workload to the minimum set of subfases needed to make
AxiomDB production-ready for that use case. Use these as prioritization guides when
deciding which phases to tackle next.

---

### 📝 Blog / CMS

**Pattern:** 95% reads, content with rich text, tags, comments, authors, drafts, SEO.

#### Minimum viable
| Feature | Subfase |
|---|---|
| Full-text search (title + body) | 11.6, 11.7 |
| Partial index (`WHERE published = true`) | 11.5 |
| `RETURNING` (get post id after insert) | 21.4 |
| `INSERT ... ON CONFLICT DO UPDATE` (view counters) | 21.5 |
| CTEs (related posts, tag cloud) | 21.2 |
| `WITH RECURSIVE` (nested comments, category trees) | 21.3 |

#### Production-grade
| Feature | Subfase |
|---|---|
| JSON native (metadata, SEO, custom fields) | 11.4 |
| Materialized views (comment counts, post stats) | 13.1 |
| LISTEN/NOTIFY (real-time comments) | 13.4 |
| Filtered LISTEN/NOTIFY (high-value events only) | 13.15 |
| Window functions (trending, rank by period) | 13.2 |
| Generated columns (auto-slug from title) | 13.3 |
| Covering indexes (listing queries without heap) | 13.5 |
| Trigram indexes (`ILIKE '%query%'`, slug search) | 11.4b |
| `DISTINCT ON` (latest post per category) | 21.12 |
| Row-Level Security (multi-author, subscriber tiers) | 17.3 |
| Immutable tables (content audit log) | 13.9 |
| Expression index (`LOWER(title)` case-insensitive) | 21.8 |

#### At scale
| Feature | Subfase |
|---|---|
| Hybrid search BM25 + HNSW (semantic + keyword) | 33.3 |
| AI embeddings (similar posts, auto-tagging) | 33.1, 33.2 |
| CDC (search index sync, cache invalidation) | 15.1 |
| GDPR physical purge (right to deletion) | 17.19 |
| Table partitioning (archive old posts) | 14.1 |
| Adaptive indexing (auto-suggest missing indexes) | 33d.1 |

---

### 🛒 E-commerce

**Pattern:** Mixed reads/writes, inventory, orders, pricing, payments, ACID-critical.

#### Minimum viable
| Feature | Subfase |
|---|---|
| `RETURNING` (order ID after insert) | 21.4 |
| `ON CONFLICT DO UPDATE` (inventory upserts) | 21.5 |
| Serializable isolation (prevent overselling) | 28.1, 28.9 |
| `SELECT FOR UPDATE SKIP LOCKED` (job queues) | 28.2, 28.11 |
| `DEFERRABLE` constraints (FK without insert order) | 21.16 |
| Range types (price ranges, date ranges) | 24.11 |

#### Production-grade
| Feature | Subfase |
|---|---|
| Transactional reservations + auto-release | 13.16 |
| Gapless sequences (invoice numbering) | 13.10 |
| Fiscal period locking (close accounting month) | 13.11 |
| Statement-level triggers (double-entry validation) | 13.12 |
| Row-Level Security (multi-tenant isolation) | 17.3 |
| Partitioning (orders by month, auto-prune) | 14.1, 14.2 |
| Column encryption (card data, PII) | 17.15 |

#### At scale
| Feature | Subfase |
|---|---|
| Bi-temporal tables (price history, corrections) | 13.18 |
| Transactional message queue (payment pipeline) | 22b.9 |
| Job chains DAG (order fulfillment workflow) | 22b.10 |
| Continuous aggregates (revenue dashboards) | 14.4 |
| OLTP compression (orders table, FK-heavy rows) | 25.10 |

---

### 📡 IoT / Time-series

**Pattern:** High insert throughput, time-ordered, rare reads, aggregations, downsampling.

#### Minimum viable
| Feature | Subfase |
|---|---|
| Partitioning by time range | 14.1, 14.2 |
| Partition pruning (planner skips old data) | 14.2 |
| `GENERATE_SERIES` (fill time gaps in reports) | 20.10 |
| `LAST(value ORDER BY ts)` aggregate | 14.12 |
| TTL per row (auto-expire stale readings) | 14.5 |

#### Production-grade
| Feature | Subfase |
|---|---|
| Dead-band recording (skip redundant readings) | 14.13 |
| Gap filling + interpolation (LOCF / linear) | 14.14 |
| `EVERY interval` downsampling syntax | 14.15 |
| Continuous aggregates (incremental refresh) | 14.4 |
| BRIN indexes (huge tables, ordered by time) | 30.3 |
| Compression of historical partitions (LZ4) | 14.3 |

#### At scale
| Feature | Subfase |
|---|---|
| Approximate aggregates (HLL, t-digest) | 22.11 |
| Arrow output (Python/pandas pipelines) | 15.4 |
| PAX layout (columnar reads within pages) | 25.4 |
| Anomaly detection (`ANOMALY_SCORE()`) | 33d.3 |

---

### 🏢 Multi-tenant SaaS

**Pattern:** Many customers sharing one DB instance, strict isolation, quotas, compliance.

#### Minimum viable
| Feature | Subfase |
|---|---|
| Row-Level Security (`tenant_id = current_user()`) | 17.3 |
| Schema namespacing (one schema per tenant) | 22b.4 |
| Storage quotas per tenant | 17.21 |

#### Production-grade
| Feature | Subfase |
|---|---|
| Column-level encryption (PII fields) | 17.15 |
| Dynamic data masking (analysts see `***-**-1234`) | 17.16 |
| Column-level `GRANT` (per-role field access) | 17.17 |
| Consent-based row access (HIPAA, GDPR) | 17.18 |
| Audit trail (who changed what, when) | 17.7, 19.20 |
| GDPR physical purge (right to erasure) | 17.19 |
| Non-blocking `ALTER TABLE` (zero downtime migrations) | 13.6 |

#### At scale
| Feature | Subfase |
|---|---|
| Transparent Data Encryption at rest (TDE) | 17.22 |
| Logical replication (per-tenant read replica) | 31.10 |
| Sharding by tenant hash | 34.1 |
| Data lineage tracking (PII audit) | 22b.7, 33d.5 |

---

### 📊 Analytics / BI

**Pattern:** Complex queries, aggregations, few writes, dashboards, reporting.

#### Minimum viable
| Feature | Subfase |
|---|---|
| Window functions (RANK, LAG, LEAD, running totals) | 13.2 |
| `GROUPING SETS / ROLLUP / CUBE` | 21.21 |
| Materialized views (pre-computed summaries) | 13.1 |
| `DISTINCT ON` (first row per dimension) | 21.12 |
| `STRING_AGG`, `ARRAY_AGG`, `JSON_AGG` | 29.1 |

#### Production-grade
| Feature | Subfase |
|---|---|
| OData v4 (PowerBI / Excel / Tableau connector) | 22d |
| Arrow output (pandas, Polars, DuckDB handoff) | 15.4 |
| `TABLESAMPLE` (approximate analysis, A/B tests) | 20.11 |
| Approximate aggregates (HLL, P95 t-digest) | 22.11 |
| Vectorized execution + SIMD | 8 |
| PAX layout (columnar within pages) | 25.4 |

#### At scale
| Feature | Subfase |
|---|---|
| Parallel query planning (split plan across cores) | 27.7 |
| Adaptive cardinality estimation | 27.10 |
| Flight SQL (high-speed columnar to Python/Java) | 15.5 |
| Hybrid search (semantic + keyword, AI re-rank) | 33.3, 33.4 |
| Text-to-SQL (`NL_QUERY()` natural language) | 33d.2 |
