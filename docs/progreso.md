# Progress — AxiomDB Database Engine

> Automatically updated with `/subfase-completa`
> Legend: ✅ completed | 🔄 in progress | ⏳ pending | ⏸ blocked
>
> **Progress: 392/1028 subphases (38.1%) — Phase 11 active**

---

## BLOCK 1 — Engine Foundations (Phases 1-7)

### Phase 1 ✅ (9/9) — Page format (CRC32c, align64), MmapStorage, MemoryStorage, FreeList, StorageEngine trait, file locking, tracing
### Phase 2 ✅ (14/14) — B+ Tree: lookup O(log n), insert+leaf split, range scan (next_leaf chain fixed 7.9), delete+merge, CoW AtomicU64 root, prefix compression, benchmarks; rotate_right key-shift bug fixed 2026-03-26
### Phase 3 ✅ (28/28) — WAL (append-only, LSN, CRC), WalWriter/Reader/Rotator, RowHeader+MVCC, TxnManager (BEGIN/COMMIT/ROLLBACK), checkpoint, crash recovery (UNDO state machine), post-recovery integrity, catalog (bootstrap+reader+writer+notifier), schema resolver, page dirty tracker, config (axiomdb.toml), autocommit semantics, doublewrite buffer (torn page repair), WAL batch append, WAL per-page record, group commit, configurable durability policy
### Phase 4 ✅ (106/110) — SQL parser + executor: row codec (all SQL types), expression evaluator (3-valued NULL semantics), AST, lexer (logos DFA, ~85 tokens), DDL+DML parsers (CREATE/DROP/ALTER/SELECT/INSERT/UPDATE/DELETE), semantic analyzer + type coercion matrix, executor (heap+clustered), JOINs (INNER/LEFT/RIGHT/FULL/CROSS/hash), GROUP BY (hash+sort), aggregates (COUNT/SUM/MIN/MAX/AVG/DISTINCT/GROUP_CONCAT), ORDER BY+LIMIT/OFFSET, subqueries (scalar/IN/EXISTS/correlated/derived), DISTINCT, CASE WHEN, 150+ functions, CLI+REPL, SHOW/DESCRIBE/TRUNCATE/ALTER TABLE, INFORMATION_SCHEMA (6 views), error framework (SQLSTATE + structured errors), strict mode + warnings, prepared statement plan cache (OID-based), clustered DDL integration
- [x] 4.10f ✅ GROUP BY WITH ROLLUP — closed via GAP-C.5 (parser + executor, level-loop subtotals)
- [x] 4.11b ✅ Subquery in JOIN — `FROM t JOIN (SELECT …) alias ON …`; implemented by `e34e9f4`: analyzer persists join-side derived tables in the AST; executor materializes JOIN subqueries once per statement; integration coverage includes INNER/LEFT/RIGHT/FULL joins, USING, alias wildcard, and chained joins mixing base and derived sources
- [x] 4.4g ✅ Multi-table DELETE/UPDATE JOIN — closed via GAP-B.5 (parser + analyzer + ctx executor handle `UPDATE t JOIN s ON ... SET t.col=...` and `DELETE t FROM t JOIN s ON ...`; integration coverage in `integration_multi_table_dml_join.rs`)
- [ ] 4.22f ⏳ DROP PRIMARY KEY on clustered table — requires full rebuild back to heap layout (`ddl.rs:1031`)
- [x] ✅ 4.22e follow-up — closed: `alter_add_column` now rebuilds every secondary index from the live heap after `rewrite_rows` (which delete+re-inserts rows and thereby changes RIDs). Clustered tables are skipped because `rewrite_rows_clustered` updates in place and preserves PKs. Regression test: `tests/integration_add_column_with_index.rs` asserts index-driven point lookups return every row after `ADD COLUMN`
- [x] ✅ `INSERT ... ON DUPLICATE KEY UPDATE` (MySQL ODKU) — closed via full /brainstorm→/spec→/plan workflow (`specs/fase-gap-audit/spec-insert-on-duplicate-key-update.md`, `plan-insert-on-duplicate-key-update.md`). Research against MariaDB `sql/sql_insert.cc::write_record` DUP_UPDATE branch + PostgreSQL `ExecOnConflictUpdate` (EXCLUDED pseudo-relation). New `Expr::InsertValue { col_idx, name }` AST variant threaded through analyzer/eval/plan-deps/partial-index/executor pattern matches. Parser adds an `on_duplicate_update: Option<Vec<Assignment>>` tail to every INSERT source (VALUES, DEFAULT VALUES, SET, SELECT); `VALUES(col)` pseudo-function recognized only when `parser.in_odku_assignment == true` (mirrors MariaDB's `IN_UPDATE_ON_DUP_KEY` parsing scope) and rejects `REPLACE ... ON DUPLICATE KEY UPDATE`. Executor helper `apply_odku_heap` iterates PK/UNIQUE indexes (partial-predicate + MATCH SIMPLE NULL + bloom shortcut), picks the FIRST match (MariaDB semantics), evaluates assignments with dual-row context (existing_row for `Column`, proposed_row for `InsertValue`), runs `check_fk_child_update` + conditional `enforce_fk_on_parent_update`, and dispatches to `TableEngine::update_row` + the shared `apply_update_index_maintenance` helper; a no-op detection produces `affected_rows += 0` when the UPDATE leaves the row equal. Per-row outcome feeds the MySQL `1/2/0` counter rule. Clustered tables return `NotImplemented`. 16 integration tests in `tests/integration_insert_on_dup.rs` cover every acceptance criterion: all source forms, PK/UNIQUE/composite conflict, counter-increment pattern, VALUES(col), unchanged-update, NULL-in-key insert, batch mixed, SELECT source, FK child validation on update branch, INSERT IGNORE coexistence, clustered rejection.
- [x] ✅ `REPLACE INTO` (MySQL upsert) — closed via full /brainstorm→/spec→/plan workflow (`specs/fase-gap-audit/spec-replace-into.md`, `plan-replace-into.md`). Research against MariaDB (`sql/sql_insert.cc::replace_row`) + PostgreSQL (`ExecOnConflictUpdate`) informed the design. Heap tables covered end-to-end: parser accepts every source form (`VALUES`, `DEFAULT VALUES`, `SET`, `SELECT`, `LOW_PRIORITY`/`DELAYED` prefixes) and rejects `REPLACE IGNORE`; `REPLACE(str, from, to)` stays available as a scalar function in expression contexts. Executor helper `replace_displace_conflicts_heap` probes every PRIMARY/UNIQUE index (honoring partial predicates + MATCH SIMPLE NULL semantics + bloom shortcut), deletes each conflicting row through the FK-aware delete path, and returns a count that contributes to `affected_rows = inserted + deleted` (MariaDB formula). Clustered tables return `NotImplemented` cleanly and are tracked as a follow-up. 16 integration tests in `tests/integration_replace_into.rs` cover every acceptance criterion (no-conflict, PK/UNIQUE/composite conflicts, multi-index displacement, FK CASCADE + RESTRICT, batch VALUES, `DEFAULT VALUES`, SET syntax, SELECT self-reference, parser rejections, scalar-function coexistence).
- [x] ✅ `ON UPDATE CURRENT_TIMESTAMP` column attribute — closed: `ColumnConstraint::OnUpdate(Expr)` captured by the parser; persisted in the catalog as `ColumnDef.on_update_expr: Option<String>` (flag bit5 in the column row; old rows read as `None`); `execute_update_ctx` auto-appends the parsed expression to the assignment list for every column whose explicit assignment is absent. Niladic SQL-std keywords (`CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_USER`, `CURRENT_SCHEMA`, `LOCALTIMESTAMP`, etc.) now parse as zero-arg functions even without parentheses. Coverage: `tests/integration_on_update_expr.rs` — auto-refresh on UPDATE, explicit assignment wins, non-annotated columns untouched
### Phase 5 ✅ (49/51) — MySQL wire protocol: TCP/Tokio listener, handshake (HandshakeV10), auth (mysql_native_password + caching_sha2_password), charset/collation negotiation, COM_QUERY/PING/QUIT/RESET_CONNECTION/INIT_DB/STATISTICS/CHANGE_USER, COM_STMT_PREPARE/EXECUTE/CLOSE/RESET/SEND_LONG_DATA, binary result encoding (all types), session state (variables, collation, compat mode, on_error), multi-statement, max_allowed_packet, plan cache (schema_version), throughput benchmarks, DSN parser (axiomdb://, mysql://, postgres://), DELETE bulk truncate (root rotation), B+ Tree in-place writes (4 fast paths), heap insert tail cache, executor/eval decomposition, batch B+ Tree delete, stable-RID UPDATE fast path, transactional INSERT staging; 5.1–5.21 complete
- [x] 5.2d ✅ MySQL default collation CI — closed via GAP-C.9 (CompatMode::MySql → SessionCollation::Es; greeting advertises utf8mb4_0900_ai_ci / id 255 CI)
- [x] 5.5b ✅ Column type wire encoding subtypes — closed via GAP-B.2 (parser aliases TINYINT/SMALLINT/MEDIUMINT/YEAR/TIME/DATETIME mapped to existing column types with correct wire codes)
### Phase 6 ✅ (33/36) — Secondary indexes + FK: key encoding (order-preserving), CREATE INDEX executor, index maintenance (INSERT/UPDATE/DELETE), query planner (IndexLookup/IndexRange/IndexOnlyScan), bloom filter (1% FPR), FK checker + CASCADE/SET NULL/RESTRICT/ON UPDATE, partial indexes (WHERE predicate), fill factor, index statistics (axiom_stats) + ANALYZE, index-only scans + INCLUDE, startup integrity verifier (auto-rebuild divergent indexes), PK SELECT access path, indexed UPDATE candidates, multi-row INSERT batch path, WAL fsync pipeline (leader-based FsyncPipeline), UPDATE apply fast path (369.9K r/s), MVCC lazy index deletion (PostgreSQL-style deferred delete), bulk DELETE root rotation
- [x] 6.5b ✅ Multi-column FK constraints — closed via GAP-C.2 (catalog + CREATE/ALTER + INSERT child validation; UPDATE child + parent-side cascade deferred)
- [x] 6.6d ✅ FK references on RENAME TABLE — closed via GAP-B.6 (FK catalog rows key by table_id, so renames preserve enforcement without updates; integration coverage for parent/child rename)
- [x] 6.6c ✅ ON DELETE SET DEFAULT / ON UPDATE SET DEFAULT — closed via GAP-C.4 (helper `fk_replacement_value` evaluates child column's default_expr; merged SetNull/SetDefault paths)
### Phase 7 ✅ (22/22) — MVCC: READ COMMITTED/REPEATABLE READ snapshot isolation, TxnManager, CoW lock-free readers (Arc<RwLock>), savepoints, VACUUM (heap + index lazy deletion), epoch-based reclamation (1024-slot SnapshotRegistry), next_leaf CoW gap fixed, lock timeout, TxnID overflow prevention; 7.16/7.17/7.18/7.19/7.20 moved to later phases
## BLOCK 2 — Execution Optimizations (Phases 8-10)

### Phase 8 ✅ (13/13) — SIMD vectorized filter (BatchPredicate, 6× faster, ~20 ns/row), zone maps (per-page min/max, heap page skip), improved planner (full plan cache, PK bloom filter), wire row serialization fast path, full-scan parity with MariaDB (205K r/s), EXPLAIN output, SIMD benchmarks vs MySQL/MariaDB; `wide` crate (AVX2/NEON/scalar fallback)
### Phase 9 ✅ (12/12) — Morsel-driven parallelism (Rayon par_iter over pages), operator fusion + unified decode mask, late materialization (BatchPredicate zero-decode for non-matching), hash join O(n+m) equijoin, sort-merge join, spill to disk (radix partitioning, 64MB limit), adaptive join selection, streaming LIMIT early-exit (no full materialization); join bench: LEFT 14.7ms vs MariaDB 162ms (11×)
### Phase 10 ✅ (8/8) — Embedded: `Db` struct (open/execute/query/begin/commit/rollback), C FFI (15 `#[no_mangle]` functions), cdylib+staticlib, Python ctypes binding, Node.js koffi binding, embedded vs server benchmark (2.7× faster embedded for PK lookups), PreparedStatement Rust API (skip parse+analyze in tight loops)
## GAP CLOSURE — Auditoría 2026-04-08

> Hallazgos de la auditoría completa de 5 subsistemas.
> Spec: `specs/fase-gap-audit/audit-2026-04-08.md`
> Orden de ejecución: GAP-A → GAP-B → GAP-C

### GAP-A — Hardening: eliminar panics de producción `⏳`
<!-- PRIORIDAD: CRÍTICA — hacer antes de cualquier otra implementación -->
- [x] GAP-A.1 ✅ key_encoding.rs — closed: decode paths have no unwrap/expect; truncated index key now surfaces as `DbError::BTreeCorrupted` and unit test covers truncation
- [x] GAP-A.2 ✅ eval/batch.rs — closed: replaced 12 `try_into().unwrap()` with `try_into().unwrap_or_default()` in BatchPredicate; all sites were already bounds-checked, defensive fallback prevents panic on corrupted data
- [x] GAP-A.3 ✅ schema_constraints.rs — closed: replaced 15 try_into().unwrap() with unwrap_or_default() in ConstraintDef/FkDef deserialization
- [x] GAP-A.4 ✅ doublewrite.rs — closed: size math via checked ops (`dw_expected_size`), defensive parsing (no `expect()` on on-disk bytes), added regression test for extreme slot_count
- [x] GAP-A.5 ✅ fsync_pipeline.rs — closed: replaced 4 .expect("poisoned") with .unwrap_or_else(|e| e.into_inner()) to recover from poisoned Mutex
- [x] GAP-A.6 ✅ notifier.rs — closed: replaced 3 .expect("poisoned") with .unwrap_or_else(|e| e.into_inner()) on RwLock
- [x] GAP-A.7 ✅ agg_accum.rs — closed: replaced 8 simple_arg.unwrap() with require_arg() helper that returns DbError::Internal instead of panic
- [x] GAP-A.8 ✅ exec_with_ctx.rs — closed: improved 18 expect() messages to document the invariant guarding each call (is_some() check or preceding begin()); all sites are structurally safe

### GAP-B — MySQL wire compat crítica `⏳`
<!-- PRIORIDAD: ALTA — desbloquea ORMs y clientes -->
- [x] GAP-B.1 ✅ UNION / UNION ALL — closed: `Stmt::Union { selects, all }` AST variant; parser detects `UNION [ALL]` after SELECT and chains multiple SELECTs; executor materializes each SELECT then concatenates (ALL) or hash-deduplicates (UNION); analyzer resolves columns in all branches; supports chained triple+ UNIONs, NULLs, literals, aliases
- [x] GAP-B.2 ✅ Column type wire codes — closed: existing types already sent correct codes; added parser aliases for TINYINT→Bool, SMALLINT→Int, MEDIUMINT→Int, YEAR→Int, TIME→Timestamp in parse_data_type(); DATETIME already worked as Timestamp alias
- [x] GAP-B.3 ✅ DECIMAL column type — ColumnType::Decimal added (catalog + executor mappings). Storage: row codec already uses `Value::Decimal(i128,u8)` (mantissa+scale). MySQL wire advertises NEWDECIMAL (0xf6). Comparisons support `Decimal` vs numeric `Text` (ORM-friendly). Integration test covers CREATE/INSERT/SELECT/WHERE.
- [x] GAP-B.4 ✅ DATE column type independiente — ColumnType::Date added (catalog + executor mappings). Storage: `Value::Date(i32)` days-since-epoch. MySQL wire advertises DATE (0x0a) + binary DATE encode/decode. Column assignment supports `Text`→`Date` for ISO `YYYY-MM-DD` (time part ignored). Integration test covers CREATE/INSERT/SELECT.
- [x] GAP-B.5 ✅ Multi-table DELETE/UPDATE JOIN — closed: parser/analyzer support `UPDATE t JOIN s ON ... SET t.col=...` and `DELETE t FROM t JOIN s ON ...`; ctx executor materializes JOIN candidates, deduplicates target RIDs, and reuses UPDATE/DELETE heap/index/FK paths; integration coverage includes assignment from joined table and DELETE target alias
- [x] GAP-B.6 ✅ FK RENAME TABLE fix — closed by verification: FK catalog rows store `child_table_id` / `parent_table_id`, so parent/child table renames preserve enforcement without name rewrites; integration coverage added for `RENAME TABLE` parent rename, `ALTER TABLE ... RENAME TO` parent rename, and child rename
- [x] GAP-B.7 ✅ SHOW PROCESSLIST real — closed: added `ConnectionInfo` + shared `Arc<RwLock<HashMap<u32, ConnectionInfo>>>` registry on `SharedDatabase` (`connection_registry`); new `processlist` module provides `ProcesslistGuard` (RAII register/deregister) with `set_command` / `set_database` helpers for live state updates; handler registers every authenticated connection with user, peer host, and initial db; `SHOW [FULL] PROCESSLIST` interceptor in `handler_sql_intercept.rs` now reads a sorted snapshot with real Id/User/Host/db/Command/Time columns; unit tests cover register → snapshot ordering, drop-removes, `set_command`/`set_database` mutations
- [x] GAP-B.8 ✅ INTERSECT / EXCEPT — closed: new `Stmt::SetOp { first, rest: Vec<SetOpTail> }` AST unifies UNION/INTERSECT/EXCEPT with per-step kind + ALL flag; parser accepts chained set ops (left-assoc); executor applies left-to-right using hash-based key (per-group counts for ALL variants: INTERSECT ALL = min(L,R), EXCEPT ALL = max(0,L-R)); integration coverage in `tests/integration_set_operations.rs` (10 tests: UNION/INTERSECT/EXCEPT + ALL variants, chained mixes, arity mismatch)

### GAP-C — SQL completeness + DDL robustness `⏳`
<!-- PRIORIDAD: MEDIA — mejora significativa de compatibilidad -->
- [x] GAP-C.1 ✅ Subquery in JOIN — closed by Phase 4.11b (`e34e9f4`); `FROM t JOIN (SELECT …) alias ON …` now has analyzer/executor wiring for join-side derived tables
- [x] GAP-C.2 ✅ Composite FK (multi-column) — closed (scoped): `FkDef` extended with `child_col_idxs: Vec<u16>` / `parent_col_idxs: Vec<u16>`; serialization appends an `0xCF`-prefixed extension trailer only when `len > 1` so single-column rows remain bit-for-bit identical to the legacy format; `persist_composite_fk_constraint` validates that parent has a PK/UNIQUE covering the column list in order and that the child has a pre-declared matching index (auto-creation of composite FK indexes deferred); `check_fk_child_insert` encodes the composite tuple key, finds a parent index whose leading columns match, and uses exact lookup or a prefix range scan when the parent index has extra trailing columns; CREATE TABLE and ALTER TABLE ADD CONSTRAINT both route multi-col FKs through the new path. ⚠️ DEFERRED: composite-aware `check_fk_child_update` and parent-side enforcement (`enforce_fk_on_parent_delete` / `enforce_fk_on_parent_update`) still operate on the first column only — composite rows get UPDATE/DELETE parent handling as follow-up. Integration tests in `tests/integration_fk_composite.rs` (3): matched tuple accepted, mismatched tuple rejected, NULL passes MATCH SIMPLE
- [x] GAP-C.3 ✅ ALTER TABLE DROP/MODIFY COLUMN con index — closed by verification: `alter_drop_column` already auto-drops indexes whose key depends on the dropped column (`ddl_alter_column.rs:798-801`) and rebuilds surviving indexes with remapped col_idx (clustered + heap paths); `alter_modify_column` rebuilds every dependent index from storage using the new column type (`ddl_alter_column.rs:976-999`); test coverage: `test_alter_drop_column_auto_drops_partial_index_on_heap`, `test_alter_drop_column_heap_rebuilds_surviving_indexes_and_remaps_metadata`, `test_alter_modify_column_heap_rebuilds_unique_index_and_preserves_metadata`, `drop_indexed_column_on_clustered_table_auto_drops_secondary_index`, `drop_unrelated_column_on_clustered_table_remaps_surviving_unique_index` — all passing
- [x] GAP-C.4 ✅ ON DELETE/UPDATE SET DEFAULT — closed: added `fk_replacement_value` helper in `fk_enforcement.rs` that evaluates the child column's persisted `default_expr` (or falls back to NULL when absent — PG semantics); merged `SetNull`/`SetDefault` match arms in `enforce_fk_on_parent_delete` (clustered + heap paths) and `enforce_fk_on_parent_update`; previously NotImplemented, now fully functional; integration tests in `tests/integration_fk_set_default.rs` cover ON DELETE with default, ON UPDATE with default, and missing-default→NULL fallback
- [x] GAP-C.5 ✅ GROUP BY WITH ROLLUP — closed: added `with_rollup: bool` to `SelectStmt`; parser recognizes `GROUP BY … WITH ROLLUP` via 2-token lookahead (`WITH` + ident `ROLLUP`); executor wraps grouped path with `execute_select_grouped_rollup` that re-runs aggregation for levels N down to 0 with progressively truncated GROUP BY list, nulling out SELECT slots for rolled-up expressions; outer ORDER BY / LIMIT / DISTINCT apply to the union of all levels; integration coverage in `tests/integration_rollup.rs` (4 tests: single-col, two-col all-levels, COUNT grand total, LIMIT)
- [x] GAP-C.6 ✅ ALTER AUTO_INCREMENT=N — closed: `ddl_alter_column.rs` now honors `ALTER TABLE t AUTO_INCREMENT = N` by updating the `AUTO_INC_SEQ` thread-local after scanning for current max; MySQL semantics applied (`desired = max(N, max_existing + 1)` — N below current max is silently ignored); integration tests cover empty-table advance, below-max ignore, and above-max honor (`tests/integration_alter_auto_increment.rs`). NOTE: catalog-level cross-restart persistence still pending — would require adding `auto_increment_next: Option<u64>` to TableDef and migration; tracked as follow-up (current impl matches MySQL per-session behavior)
- [x] GAP-C.7 ✅ DROP INDEX not found → error propio — closed: added `DbError::IndexNotFound { name }` variant; MySQL errno 1091 "Can't DROP; check that it exists"; replaced NotImplemented in both ON-table and scan-all paths of ddl_drop_index.rs
- [x] GAP-C.8 ✅ Correlated subquery depth > 1 — closed: `Expr::OuterColumn` gained a `depth: u16` field carrying nesting distance (0 = immediate parent); analyzer emits depth based on outer-scope position in `resolve_expr_full`; executor `subst_expr`/`substitute_outer_at` threads a `binding_depth` parameter that only substitutes OuterColumns whose depth matches the current binding level and increments on nested subquery entry; single-equijoin materialization optimization now requires `depth: 0` to avoid misoptimizing deep refs; integration coverage: EXISTS depth-2 (positive + negative), IN subquery depth-2 (`tests/integration_correlated_depth.rs`)
- [x] GAP-C.9 ✅ MySQL default collation case-insensitive — closed by verification: `SessionContext::effective_collation()` returns `SessionCollation::Es` (CI+AI fold) when `CompatMode::MySql` is active (`session.rs:688`); server greeting advertises `utf8mb4_0900_ai_ci` (id 255) which is MySQL 8.0's default CI collation — matches the advertised server version "8.0.36" and is semantically equivalent to id 45 (`utf8mb4_general_ci`) for CI comparison purposes; regression tests added: `gap_c9_mysql_compat_defaults_to_case_insensitive_collation` (session) + `greeting_advertises_case_insensitive_utf8mb4_collation` (packets)
- [x] GAP-C.10 ✅ COM_STMT_SEND_LONG_DATA validación — closed by verification: `handler.rs` guards `body.len() < 6` before slicing stmt_id/param_idx, unknown stmt_id is silently ignored (MySQL wire contract — no response), `PreparedStatement::append_long_data` validates param_idx bounds and stores deferred error (surfaced on next `COM_STMT_EXECUTE`); added `checked_add` overflow guard on `current_len + chunk.len()` to protect against pathological usize overflow; unit tests cover all deferred-error paths

---

## BLOCK 3 — Advanced Features (Phases 11-15)

### Phase 11 — Robustness and indexes `🔄` week 61-64
- [x] 11.1 ✅ Sparse index — covered by zone maps (per-page min/max, Phase 8.3b) + BRIN (11.1b); research confirmed zone maps are equivalent to sparse indexes for time-series data
- [x] 11.1b ✅ BRIN indexes — Step 1 (AST+parser) + Step 2 (catalog) + Step 3 (`brin.rs`: BrinSummary, metapage init/read/write, qualifying_ranges, 7 unit tests) + Step 4 (CREATE INDEX USING brin: scan heap, compute per-range min/max, write summaries) + Step 5 (INSERT maintenance: update_range_summary in insert_into_indexes_with_undo for index_type=1) + Step 6 (planner: extract_brin_predicate + build_brin_page_skip_set; scanner: scan_table_filtered_brin skips pages not in qualifying ranges); PostgreSQL brin.c reference; minmax-only for numeric types (Int, BigInt, Real, Date, Timestamp); spec+plan in specs/fase-11/
- [x] 11.2 ✅ TOAST — oversized Text/Bytes values externalized to overflow chains when encoded row > 8000 bytes; LZ4 compression via `lz4_flex` (pure Rust); u24 sentinel pointers (0xFF_FFFE raw, 0xFF_FFFD LZ4) inline; `toast_row_if_needed()` in write path (largest columns first); `detoast_row()` in scan paths resolves placeholders via `read_chain()`; `free_toast_chains_in_encoded()` on DELETE frees overflow pages; reuses `clustered_overflow::write_chain/read_chain/free_chain`; PostgreSQL heaptoast.c EXTENDED strategy reference; backward compatible (existing rows never have sentinel u24 values)
- [x] 11.2b ✅ BLOB_REF storage format — implemented via TOAST sentinel u24 values in 11.2: `0x00-0xFF_FFFC`=inline (u24 length + payload), `0xFF_FFFE`=TOAST overflow uncompressed (u64 page_id + u32 raw_len), `0xFF_FFFD`=TOAST overflow LZ4 compressed; the `0x02` content-hash variant (SHA256, 32B) deferred to Phase 14.9 when content-addressed BLOB store is needed; `encode_toast_pointer()`/`decode_toast_pointer()` public API in codec.rs; `is_toast_sentinel()` check; executor and SQL layer are agnostic to which variant is used
- [x] 11.2c ✅ MIME_TYPE auto-detection — `MIME_TYPE(blob_col)` SQL function detects content type from magic bytes (first 4-12 bytes); 10 formats: PNG, JPEG, GIF, WebP, PDF, ZIP, GZIP, JSON, XML + fallback; `detect_mime_type()` zero-allocation (&str return); wired into binary function dispatcher — on BLOB insert, read first 16 magic bytes to detect PNG/JPEG/WebP/PDF/GIF/ZIP/etc.; cache as 1-byte enum alongside the BLOB_REF in the row; expose as `MIME_TYPE(col)→TEXT` SQL function; zero overhead on read (metadata is in the row)
- [x] 11.2d ✅ BLOB reference tracking — closed: TOAST/BLOB chains use versioned `ABOB` overflow headers with first-page refcount, per-page `part_len`, `write_refcounted_chain()`, `read_blob_chain()`, `incref_blob()`, and `free_blob()`; TOAST write/read/delete paths are wired and validated with workspace gates plus a dedicated `overflow/refcounted_blob` storage benchmark
  - [ ] ⚠️ COM_QUERY long text literal stack overflow — gap identified during wire smoke; storage/refcount path passes through `COM_STMT_SEND_LONG_DATA`, revisit in wire/parser hardening
- [x] 11.2e ✅ Unicode NFC normalization on store — every TEXT value is normalized to NFC (Canonical Decomposition followed by Canonical Composition) before being written to disk; `'café'` (NFD: 6 bytes) and `'café'` (NFC: 5 bytes) become identical on store, making `=` always correct for visually identical strings; zero API surface change — completely transparent to the application; this is what DuckDB does and it eliminates an entire class of invisible Unicode bugs that cause `'García' = 'García'` to return FALSE when one was typed and one was pasted from a different source
- [x] 11.3 ✅ In-memory mode — `Db::open(":memory:")` detects special path and creates tempdir-backed ephemeral database; `Db::open_memory()` explicit constructor; `_tmpdir: Option<TempDir>` kept alive in `Db` struct, cleaned up on drop; SQLite `:memory:` compatible API
- [x] 11.4 ✅ Native JSON — single SQL `JSON` type backed by validated UTF-8 JSON text; `Value::Json`, `DataType::Json`, and `ColumnType::Json`; DDL/catalog/row-codec/coercion/embedded/MySQL wire support; `JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`, `JSON_VALID`, `JSON_TYPE`; PostgreSQL-style `->>` lowered to `JSON_EXTRACT`; simple paths `$`, `$.key`, `$.key1.key2`, and array indexes for extraction; TOAST sentinel handling preserved for JSON masked decode and detoast; 35 row-codec tests, 6 JSON SQL integration tests, workspace test/clippy/fmt, wire smoke 341/341, local `json_extract` benchmark 28.7ms / 348,652 rows/s on 10K rows
  - [x] ✅ Binary JSONB layout, full SQL:2016 JSONPath, `->`, `JSON_MERGE_PATCH`, `JSON_CONTAINS`, `JSON_OVERLAPS` — implemented in Phase 11.16
  - [x] ✅ GIN indexing for JSONB `@>` containment — implemented in Phase 11.17
- [x] 11.4b ✅ JSONB_SET — implemented as `JSON_SET(json, path, value)` in Phase 11.4
- [x] 11.4c ✅ JSONB_DELETE_PATH — implemented as `JSON_REMOVE(json, path)` in Phase 11.4
- [x] 11.4b ✅ Trigram indexes for substring search — `CREATE INDEX ON productos (nombre) USING trigram`; makes `WHERE nombre LIKE '%García%'` use the index instead of full table scan; `WHERE nombre ILIKE '%garcia%'` also indexed (case-insensitive); PostgreSQL requires installing pg_trgm extension manually and it is not enabled by default — we include trigram support built-in; the query planner automatically suggests `CREATE INDEX ... USING trigram` in EXPLAIN output when it detects frequent `LIKE '%...%'` patterns causing sequential scans
- [x] 11.5 ✅ Partial indexes — `CREATE INDEX ... WHERE condition` — implemented in Phase 6.7 (partial UNIQUE index) and fully generalized: `partial_index.rs` with `compile_index_predicates()` + `resolve_predicate_columns()`; predicate stored as SQL string in `IndexDef`; enforced on INSERT/UPDATE/DELETE; planner uses index only when query WHERE implies predicate; works on both heap and clustered tables; dedicated `integration_partial_index.rs` test suite
- [x] 11.6 ✅ Basic FTS — tokenizer (whitespace+punctuation split, lowercase, 174 stop words, position tracking) + inverted index (B-Tree with `term\0docid_8LE+slot_2LE+position_4LE` keys) + `MATCH(col, 'query')` function (TF-based scoring); `CREATE INDEX ... USING fts` builds inverted index from text; INSERT maintenance adds term postings; PostgreSQL wparser_def.c + tsrank.c reference; spec: specs/fase-11/spec-11.6-basic-fts.md
- [x] 11.7 ✅ Advanced FTS — boolean query parser (`+required`, `-excluded`, `|` OR, `"phrase"`, `prefix*`); `FtsClause` enum (Required/Excluded/Optional/Or/Phrase/Prefix); `evaluate_fts()` with weighted scoring (Required=1.0, Phrase=2.0, Optional=0.5, Or=0.75, Prefix=0.5); phrase matching uses token positions for adjacency check; MATCH() function upgraded to use advanced parser; 9 unit tests; PostgreSQL tsquery `&`/`|`/`!`/`<->` concepts adapted to MySQL FULLTEXT `+`/`-`/`|`/`""`/`*` syntax
- [x] 11.8 ✅ Buffer pool manager — 16-shard partitioned LRU (`BufferPool` in `buffer_pool.rs`); `CacheShard` with `HashMap<u64, CacheEntry>` + `VecDeque<u64>` LRU order; `Arc<PageRef>` for cheap clones; pin/unpin prevents eviction of in-flight pages; `invalidate()` on write; configurable capacity (default 1024 pages = 16 MB); InnoDB buf0buf.cc (hash+LRU+young/old) and PostgreSQL bufmgr.c (clock-sweep+pin) reference; 5 unit tests (hit, miss, eviction, pin protection, invalidate); **integration**: ready to layer on top of MmapStorage::read_page(); spec: specs/fase-11/spec-11.8-buffer-pool.md
- [x] 11.9 ✅ Page prefetching — sequential scan now prefetches 8 pages ahead (128 KB) via `madvise(MADV_SEQUENTIAL)` instead of 1 page; wired into `scan_table_direct()` and `scan_table_filtered_brin()`; InnoDB reads 64 pages ahead (buf0rea.cc), PostgreSQL uses `effective_io_concurrency`; MmapStorage.prefetch_hint already existed (Phase 8.3d) — this phase increases lookahead from 1→8 pages
- [x] 11.10 ✅ Write combining — **ALREADY IMPLEMENTED** across multiple phases: `record_insert_batch()` (Phase 6.18) writes N WAL entries in single `write_all`; `FsyncPipeline` (Phase 6.19) coalesces fsyncs via leader-based group commit; `wal_scratch` buffer reuse (Phase 40.4b) eliminates per-entry allocation; `apply_insert_batch_with_ctx()` (Phase 6.18) batches heap + WAL + index in single pass; benchmark gap (insert 0.27x) is per-row INSERT overhead in single-txn mode — batch INSERT path already competitive
- [x] 11.11 ✅ Top-N heap sort — SELECT path already had `apply_order_by_top_n()` (introselect + partial sort, O(n log k)); this close extends Top-N to UPDATE/DELETE `apply_order_by_limit_to_candidates()` and clustered variants using `select_nth_unstable_by()` partitioning; PostgreSQL `tuplesort.c bounded_sort` pattern; **benchmark gap**: order_limit 0.48x
- [x] 11.12 ✅ Correlated subquery materialization — `detect_materializable_pattern()` identifies single-equijoin correlated subqueries (`inner.col = OuterColumn(idx)`); `materialize_correlated_subquery()` rewrites inner query with GROUP BY on join column, executes ONCE, builds `HashMap<outer_key, result>` for O(1) lookup per outer row; `MaterializedCache` per-subquery state in `ExecSubqueryRunner`; `strip_outer_equijoin()` removes correlation from WHERE; fallback to existing CorrelatedCache for non-matching patterns; PostgreSQL `ExecHashSubPlan` + MySQL 8.0 `left_expr_cache` research; spec+plan in `specs/fase-11/`; **benchmark gap**: subquery_scalar 0.12x — pattern detection tuning needed for specific benchmark query shape
- [x] 11.13 ✅ Hash-based DISTINCT — **ALREADY IMPLEMENTED**: `apply_distinct_with_session()` in `agg_group_table.rs` uses `HashSet<Vec<u8>>` with `value_to_session_key_bytes` serialization — already O(n) hash-based, NOT sort-based. Collation-aware via `canonical_text()`. Gap vs MariaDB (0.70x) is serialization overhead, not algorithm. Future: `HashableRow` newtype to avoid `Vec<u8>` allocation
- [x] 11.14 ✅ LIKE fast paths — 4 zero-alloc fast paths in `like_match()`: prefix `'abc%'` → `starts_with()`, suffix `'%abc'` → `ends_with()`, infix `'%abc%'` → `contains()`, exact (no wildcards) → `==`; avoids O(n·m) backtracking + `Vec<char>` allocation for common patterns; InnoDB `Field::key_cmp` prefix optimization pattern; **benchmark**: like_pattern 0.62x → 0.68x (+10%); remaining gap is per-row decode overhead (BatchPredicate doesn't support Text yet)
- [x] 11.15 ✅ Batch INSERT from SELECT — INSERT SELECT now collects all rows first, then uses `apply_insert_batch_with_ctx()` for batch heap insert + batch WAL (one entry per page, not per row) + batch index maintenance; per-row path retained for IGNORE mode (needs per-row error handling); **benchmark**: insert_select 0.66x → 0.71x (+8%); remaining gap is encode overhead + per-row FK validation
- [x] 11.16 ✅ Binary JSONB + JSONPath — `Value::Jsonb(Arc<Vec<u8>>)` / `DataType::Jsonb` / `ColumnType::Jsonb=10`; binary encoder (iterative DFS, depth limit 256, key-sort bytewise-length-first), decoder, `JsonbRef` zero-alloc accessor with O(1) stride-based element_offset; `->` operator (`BinaryOp::JsonSub`) for key/index extraction; `JSON_MERGE_PATCH`, `JSON_CONTAINS`, `JSON_OVERLAPS`, `JSON_ARRAY_LENGTH`, `JSON_DEPTH`, `JSON_PRETTY`, `TO_JSONB` functions; JSONPath compiler+executor (`jsonpath.rs`): lax/strict modes, `$`, `$.key`, `$[idx]`, `$.*`, `$[*]`, `$..key` recursive descent, `$[?(@.field op val)]` filter expressions; all existing Phase 11.4 JSON functions upgraded to JSONB binary path; `CAST(x AS JSONB)`; `integration_jsonb.rs` now covers 35 JSONB/GIN cases after Phase 11.17; wire: JSONB displays as JSON text over MySQL wire; PostgreSQL jsonb.c JEntry stride + key-sort reference
- [x] 11.17 ✅ GIN index for JSONB containment — `CREATE INDEX ... USING GIN (jsonb_col)` creates a term-posting B-Tree for JSONB documents; `WHERE col @> '<json literal>'` plans as `GinScan` when a matching GIN index exists and falls back to full scan otherwise; index build covers existing rows; INSERT/DELETE/UPDATE maintain terms; heap tables use RID postings and clustered tables use encoded PK bookmarks; term intersection always rechecks structural `@>` so rows with the same words in the wrong nested shape are not false positives; regression coverage includes nested objects, arrays, booleans, numbers, create-after-data, DML maintenance, EXPLAIN, and a super-complex realistic payload; local bench `jsonb_gin_contains` added to `benches/comparison/local_bench.py`
- [ ] 11.18 ⏳ PostgreSQL JSONB operator parity — split into sub-phases:
  - [x] ✅ 11.18a — `?`, `<@`, `||`, `-(text)`, `-(int)` + function-style aliases (`JSONB_EXISTS`, `JSONB_CONTAINED`, `JSONB_CONCAT`, `JSONB_DELETE_KEY`, `JSONB_DELETE_INDEX`) + GIN planner integration for `?`. Closed via full /brainstorm → /spec → /plan → /implement workflow (`specs/fase-11/spec-11.18a-jsonb-operators.md`, `plan-11.18a-jsonb-operators.md`) informed by cross-engine research (PG `jsonb_op.c` + `jsonb_gin.c`, MariaDB `item_jsonfunc`, DuckDB JSON functions). New `BinaryOp::JsonExists`, `BinaryOp::JsonContainedBy` AST variants; polymorphic dispatch on `||` (Concat) and `-` (Sub) at eval time when LHS is `Value::Jsonb`; `?` reused as JSONB-exists infix operator when preceded by a completed left expression, prepared-statement `?` placeholder semantics untouched. GIN uses the existing Phase 11.17 term layout via a new `gin_key_term(text)` helper; `recheck_required = true` matches PG's `gin_consistent_jsonb` strategy 9. Coverage: `tests/integration_jsonb_operators.rs` (21 tests) including PG regression parity cases (`jsonb.sql` 300-333, 1135-1197, 245-250) + GIN accelerate + DELETE/INSERT maintenance regression.
  - [ ] 11.18b ⏳ `?|`, `?&`, `-(text[])`, `#-`, `#>`, `#>>` — require a SQL `TEXT[]` type (absent today). Next session picks between introducing native `TEXT[]` or accepting JSONB-array RHS with a documented PG divergence.
- [ ] 11.19 ⏳ SQL/JSON standard query functions — split into sub-phases:
  - [x] ✅ 11.19a — `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS` as SQL:2016 special-form expressions. Closed via full /brainstorm → /spec → /plan → /implement workflow (`specs/fase-11/spec-11.19a-sql-json-query-functions.md`, `plan-11.19a-sql-json-query-functions.md`) informed by PG `src/backend/parser/gram.y:17117-17172` + `jsonpath_exec.c` + MariaDB `Item_func_json_value` + DuckDB. New `Expr::SqlJsonQuery { kind, doc, path, path_mode, returning, on_empty, on_error }` AST variant + `SqlJsonQueryKind`, `SqlJsonPathMode`, `SqlJsonOnBehavior` enums. Parser dispatches `JSON_VALUE/QUERY/EXISTS` as special forms before the variadic call parser consumes them; grammar enforces clause order `RETURNING → ON EMPTY → ON ERROR`. Path-mode prefix `strict `/`lax ` stripped at parse time (default strict, PG parity). Evaluator `eval_sql_json_query` walks the path with strict/lax semantics, dispatches outcomes (Matched/Empty/Error) through per-kind behaviors: `ON EMPTY {ERROR|NULL|DEFAULT expr}`, `ON ERROR {ERROR|NULL|DEFAULT expr}` plus `TRUE|FALSE|UNKNOWN` literals for JSON_EXISTS. RETURNING type coercion via `axiomdb_types::coerce::coerce`. Coverage: `tests/integration_sql_json_query.rs` (25 tests) — scalar/type coercion, strict/lax missing key, DEFAULT fallback, multi-array reject (scalar-only), RETURNING TEXT vs JSONB for JSON_QUERY, JSON_EXISTS TRUE/FALSE/UNKNOWN/ERROR on-error, column-level WHERE predicate, wildcard rejection, clause-ordering error.
  - [x] ✅ 11.19b — `WITH [CONDITIONAL|UNCONDITIONAL] ARRAY WRAPPER` / `WITHOUT [ARRAY] WRAPPER` + `KEEP|OMIT QUOTES [ON SCALAR STRING]` clauses on JSON_QUERY (SQL:2016 § 6.29). Added `SqlJsonWrapper { Without, Unconditional, Conditional }` and `SqlJsonQuotes { Keep, Omit }` enums, extended `Expr::SqlJsonQuery` with `wrapper` + `quotes` fields, and threaded both through parser + analyzer_expr + sql_json_query evaluator. Grammar enforces spec clause order `RETURNING → WRAPPER → QUOTES → ON EMPTY → ON ERROR`; wrapper/quotes rejected on JSON_VALUE / JSON_EXISTS with explicit "only valid on JSON_QUERY" error. Evaluator helper `apply_wrapper` implements PG-parity conditional semantics (conditional wraps unless result is a single array). `OMIT QUOTES` on scalar string renders as `Value::Text` (not `Value::Json`). 12 integration tests in `tests/integration_sql_json_query_wrapper_quotes.rs`. `PASSING` (jsonpath variable binding) deferred to 11.19c.
  - [x] ✅ 11.19c — `PASSING expr AS name [, …]` clause on `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS`. Extended `Expr::SqlJsonQuery` with `passing: Vec<(Expr, String)>` threaded through parser + `analyzer_expr` (each expr resolved in current scope) + evaluator. Grammar position: between path literal and `RETURNING` per SQL:2016. At eval time each binding's value is rendered to a jsonpath literal (string → JSON-quoted, bool → `true`/`false`, numeric → decimal form, NULL → `null`) and substituted for `$name` in the path string with word-boundary protection (so `$foo` does not match inside `$foobar`). Substitution happens BEFORE `execute_path`, so the downstream walker sees a variable-free path. MVP caveat: the path walker still rejects filter expressions with `[?(…)]` and wildcards, so PASSING bindings are useful today primarily for dynamic scalar-key paths and for forward compatibility with a future filter evaluator; existing clause ordering and RETURNING/WRAPPER/QUOTES/ON EMPTY/ON ERROR semantics unchanged. 7 integration tests in `tests/integration_sql_json_passing.rs` — parse acceptance, multiple bindings, order (PASSING→RETURNING), expression bindings (`2+3`), combined with WRAPPER, JSON_EXISTS form, missing-AS parse error. 11.19a regression suite (25 tests) still green.
- [ ] 11.20 ⏳ `JSON_TABLE` row source — parse and execute `JSON_TABLE(...)` in `FROM` with `COLUMNS`, `FOR ORDINALITY`, `EXISTS PATH`, scalar `PATH`, and `NESTED PATH`; first target is PostgreSQL/Oracle-style shredding of JSON arrays into relational rows, later integrated into `UPDATE`, `DELETE`, and `MERGE` sources
- [ ] 11.21 ⏳ JSONPath parity + indexed path operators — split into sub-phases:
  - [x] ✅ 11.21a — PG `jsonb_path_*` family: `jsonb_path_exists`, `jsonb_path_query`, `jsonb_path_query_first`, `jsonb_path_query_array`, `jsonb_path_match`. Closed via full /brainstorm → /spec → /implement workflow (`specs/fase-11/spec-11.21a-jsonb-path-functions.md`). Reuses existing `parse_jsonpath` + `execute_jsonpath`. `_query_array` wraps matches in JSONB array (empty `[]` on miss, not NULL). `_match` returns bool only when exactly one boolean result; otherwise NULL (permissive PG-lite). 13 integration tests in `tests/integration_jsonb_path_functions.rs`.
  - [x] ✅ 11.21b — `@?` JSONPath-exists binary operator (PG parity): `doc @? 'jsonpath'` ≡ `jsonb_path_exists(doc, path)`. New `Token::JsonbPathExists` in lexer (must appear before `@` in token order to beat the `@>` / `@` prefixes); new `BinaryOp::JsonbPathExists` variant threaded through `expr_to_sql` and DDL constraint printer. Evaluator `eval_jsonb_path_exists` in `eval/ops.rs` reuses the now `pub(crate)` `parse_jsonpath` / `execute_jsonpath` helpers. NULL-on-either-operand propagation. 6 integration tests in `tests/integration_jsonb_path_operator.rs` (scalar match/miss, NULL doc, NULL path, WHERE filter over JSONB column, text-doc coercion). `@@` operator deferred: lexer already binds `@@` to MySQL session-variable prefix; adding jsonpath-match semantics requires contextual disambiguation (left-hand-side is a value expression, not a session-var name) — pushed to 11.21c alongside the GIN + variables work.
  - [x] ✅ 11.21c (partial) — `@@` JSONB JSONPath-match binary operator added. `BinaryOp::JsonbPathMatch` reuses the now-`pub(crate)` `parse_jsonpath` / `execute_jsonpath` helpers from 11.21a; NULL propagation on either operand; non-boolean result / multi-match / missing path → NULL (PG parity). Infix `Token::AtAt` handler sits next to `@?` in `parse_cmp` — grammatical collision with MySQL `@@session_var` is avoided naturally because the session-var prefix parses only at atom position (`parse_set_variable`) while the operator arm runs only after a completed LHS. 7 integration tests in `tests/integration_jsonb_path_match_op.rs`. Remaining 11.21c items (JSONPath variables beyond the existing 11.19c PASSING mechanism, richer accessors `.type()`/`.size()`/arithmetic-in-filters, planner predicate extraction, `jsonb_path_ops` hash-based GIN opclass) deferred to 11.21d.
  - [x] ✅ 11.21d (partial) — JSONPath `.size()` and `.type()` terminal accessors. New `PathStep::Size` / `PathStep::TypeOf` variants; path parser recognizes trailing `.ident()` (consume_identifier stops at `(`); unknown accessor → explicit error. Walker keeps ref-returning `execute_jsonpath` for internal use and adds `execute_jsonpath_owned` which splits off a trailing accessor, walks via the ref path, then transforms each borrowed result into an owned `serde_json::Value` (array→length integer for `.size()`, non-array→1; JSON type-name string for `.type()` using PG-style labels `null`/`boolean`/`integer`/`number`/`string`/`array`/`object`). Wired into `jsonb_path_exists`, `jsonb_path_query`, `jsonb_path_query_first`, `jsonb_path_query_array`, `jsonb_path_match`, and both `@?` / `@@` operator evaluators. 11 integration tests in `tests/integration_jsonpath_accessors.rs` (array/scalar size, 6 type names, query_array packaging, unknown accessor error). All 11.21a/b/c regression tests still green. Arithmetic in filter expressions + planner predicate extraction + `jsonb_path_ops` GIN opclass remain for 11.21e.
  - [x] ✅ 11.21e (partial) — Boolean combinators in JSONPath filter expressions: `&&` (AND), `||` (OR), `!` (NOT), parenthesized grouping. New `FilterExpr::{And, Or, Not}` variants + recursive-descent `FilterParser` (skip_ws, eat literal, parse_or → parse_and → parse_unary → parse_atom → parse_primary) replacing the prior single-comparison `strip_prefix` chain. Precedence: NOT > AND > OR, parens override. Primary-atom parsing keeps the `@.key.key…` walker from 11.21a and the same `=/!=/</<=/>/>=` operator set + `parse_jsonpath_literal`. Trailing-input validation in parse_filter prevents silently accepted malformed filters. 5 new integration tests in `tests/integration_jsonpath_filter_combinators.rs` (AND, OR, NOT, parenthesized precedence, existence+comparison mix). All prior 11.21a/b/c/d regression suites still green. Arithmetic inside filters (`@.a + @.b > 5`), planner predicate extraction, and `jsonb_path_ops` GIN opclass remain for 11.21f.
  - [ ] 11.21f ⏳ Arithmetic in filter expressions (`@.a + @.b > $k`), planner predicate extraction for indexable JSONPath, `jsonb_path_ops` hash-based GIN opclass.
- [x] ✅ 11.22 JSONB mutation parity — split into sub-phases:
  - [x] ✅ 11.22a — PG `JSONB_SET`, `JSONB_INSERT`, `JSONB_DELETE_PATH` + MySQL `JSON_INSERT`, `JSON_REPLACE` (complements existing `JSON_SET` / `JSON_REMOVE` from Phase 11.4). Closed via full /brainstorm → /spec → /plan → /implement workflow (`specs/fase-11/spec-11.22a-jsonb-mutations.md`, `plan-11.22a-jsonb-mutations.md`) informed by PG `jsonfuncs.c` (jsonb_set/jsonb_insert/jsonb_delete_path at lines 4856/5005/4965) + MariaDB `item_jsonfunc.cc` (Item_func_json_insert constructor flags). New shared helpers `parse_mutation_path` (accepts MySQL string `$.a.b` and PG-lite JSON-array `["a","b"]`; rejects wildcards), `set_path_ext` (flag-driven: `create_if_missing`, `insert_after`, `raise_on_existing_key`, `allow_insert`), `remove_path_parts`, `path_exists`, `jsonb_blob_from_serde`, `is_truthy_arg`. PG functions return `Value::Jsonb`; MySQL functions return `Value::Json`. Deliberate semantic divergence preserved: `JSONB_INSERT` on existing object key **raises** (PG), `JSON_INSERT` on existing key **silently no-ops** (MySQL); tests #13 and #19 assert opposite outcomes. `JSON_INSERT`/`JSON_REPLACE` are variadic (`doc, p1, v1, p2, v2, ...`); odd arg counts raise `TypeMismatch`. 24 integration tests in `tests/integration_jsonb_mutations.rs`.
  - [x] ✅ 11.22b — `jsonb_set_lax(target, path, new_value [, create_if_missing=true [, null_value_treatment='use_json_null']])`. Closed via full /brainstorm → /spec → /plan → /implement workflow (`specs/fase-11/spec-11.22b-jsonb-set-lax.md`, `plan-11.22b-jsonb-set-lax.md`) informed by PG `src/backend/utils/adt/jsonfuncs.c:4898-4959`. Reuses 11.22a helpers (`parse_mutation_path`, `set_path_ext`, `remove_path_parts`, `jsonb_blob_from_serde`). Semantics: NULL target/path/create_if_missing → NULL; NULL treatment arg → error; non-NULL new_value → delegates to `jsonb_set` behavior; SQL-NULL new_value dispatches on treatment enum (`use_json_null` embeds JSON null, `raise_exception` errors, `delete_key` → `jsonb_delete_path` semantics, `return_target` returns target unchanged, other → error). Coverage: `tests/integration_jsonb_set_lax.rs` (12 tests) — non-null passthrough, 4 treatment branches, NULL arg handling, invalid treatment, `create_if_missing=false`.
- [ ] 11.23 ⏳ JSON Schema validation — split into sub-phases:
  - [x] ✅ 11.23a — `JSON_SCHEMA_VALID(schema, doc)` predicate with Draft-07 subset validator. Supports keywords: `type` (scalar + union-array), `enum`, `const`, `required`, `properties` (recursive), `additionalProperties` (bool), `items` (homogeneous schema or positional array), `minimum`/`maximum`/`exclusiveMinimum`/`exclusiveMaximum`, `minLength`/`maxLength` (Unicode codepoint-aware), `minItems`/`maxItems`, `multipleOf`. Boolean schemas `true`/`false` (Draft-07 accept-all / reject-all) supported. NULL propagation on either argument. JSONB + TEXT surfaces both accepted via existing `value_to_serde_json`. 18 integration tests in `tests/integration_json_schema_valid.rs`.
  - [x] ✅ 11.23b — `JSON_SCHEMA_VALIDATION_REPORT(schema, doc)` returns a JSON array of `{path, keyword, message}` error objects (empty array = valid). Shares all keyword support with 11.23a/d/e/f (`$ref` resolves identically and records the nested error at the referenced sub-schema's resulting path). Path uses JSON-pointer-style root `#` with `/key` and `/index` segments. Logical combinators: `allOf` surfaces each branch's errors inline; `anyOf`/`oneOf` report a single consolidated error (individual branch errors would be noisy when the combinator is the intent). 9 integration tests in `tests/integration_json_schema_report.rs`.
  - [ ] 11.23c ⏳ Catalog storage for reusable named schemas (`CREATE JSON SCHEMA …` DDL + metadata) and `CHECK (JSON_SCHEMA_VALID(<schema_name>, col))` resolution from catalog — closes the Phase 11.16 deferred item.
  - [x] ✅ 11.23d (partial) — Draft-07 keywords added to `JSON_SCHEMA_VALID`: `pattern` (regex via the `regex` crate; invalid pattern → validation failure, not SQL error), `allOf` / `anyOf` / `oneOf` (exactly-one) / `not` logical combinators, `uniqueItems`. 6 new integration tests (24 total in file). Remaining Draft-07 gaps (→ 11.23e): `patternProperties`, `format`, `$ref`, `dependencies`, `propertyNames`, `if/then/else`.
  - [x] ✅ 11.23e (partial) — Remaining Draft-07 keywords except `$ref`: `patternProperties` (regex→subschema, composable with `properties` + `additionalProperties` enumeration; additional is now "not in properties AND no patternProperties regex matches"), `propertyNames` (apply schema to each key as a string doc), `dependencies` (key → [required keys] array form + key → subschema form), `if`/`then`/`else` (if-schema drives which branch to validate), `format` subset: `email` / `uuid` / `date` / `date-time` / `time` / `ipv4` / `ipv6` / `uri` / `regex`. `additionalProperties` now accepts a sub-schema (not only a bool). 10 new integration tests (34 total). `$ref` → 11.23f (requires root-schema tracking + JSON-pointer resolver).
  - [x] ✅ 11.23f — `$ref` internal JSON-pointer references. Refactored `json_schema_validate` into `validate_with_root(schema, doc, root, depth)` that threads the original root schema + recursion depth through every recursive call (allOf/anyOf/oneOf/not/properties/patternProperties/additionalProperties/propertyNames/items/dependencies/if/then/else). `$ref` short-circuits per Draft-07 (sibling keywords ignored). `resolve_json_pointer` implements RFC 6901: `#` / `#/` → root, `~1` → `/`, `~0` → `~`, array-index segment by parse. Depth limit 32 blocks cyclic recursion (self-referential `{"$ref":"#"}` safe). 6 new integration tests: basic definitions ref, nested-in-properties ref, root-self-ref over nested arrays, missing pointer → false, `~1` escape decoding, Draft-07 sibling-keyword ignore. 40 total tests now across JSON Schema surface. **Phase 11.23 Draft-07 coverage now complete** except for advanced cases ($refs across multiple documents, remote $refs, `definitions`/`$defs` alias, Draft 2019+ keywords).
- [x] ✅ 11.25c — MySQL `JSON_SEARCH(doc, 'one'|'all', pattern [, escape, path…])`. Recursive string-value walk with LIKE-style pattern matching (`%` any-length, `_` single char; implemented in a small `like_match` chars-recursive matcher — no regex overhead). `'one'` mode returns first matching path as TEXT; `'all'` returns a JSON array of all matching paths. Short-circuit in `'one'` mode avoids walking past the first hit. Paths use MySQL-compatible notation (`$.key`, `$[idx]`). Non-string values skipped (integer 42 vs string "42" behave per MySQL). Escape/path-filter args accepted but ignored in MVP (full-doc walk). No match → NULL; any NULL arg → NULL; bad mode → error. 9 integration tests in `tests/integration_json_search.rs`.
- [x] ✅ 11.25b — JSON constructors + MySQL merge/contains_path bundle: `JSON_ARRAY(v…)` (variadic, empty-args allowed, `[]`); `JSON_OBJECT(k, v, …)` (even arg count; NULL key → error); `JSON_MERGE_PRESERVE(d1, d2, …)` / alias `JSON_MERGE` — array concat on arrays-arrays; object key-merge recurses (conflicting leaf values wrap into array — MySQL parity, distinct from `JSON_MERGE_PATCH` right-wins); object+array promotes to unified array; any NULL arg → NULL; `JSON_CONTAINS_PATH(doc, 'one'|'all', path, …)` (short-circuits on first hit in `'one'`, first miss in `'all'`; mode string case-insensitive; bad mode → error). Helper `merge_preserve(a, b)` handles the 4-arm type matrix. 15 integration tests in `tests/integration_json_constructors.rs`.
- [x] ✅ 11.25a — MySQL JSON completion bundle (gap close): `JSON_QUOTE(text)` serializes TEXT as JSON string literal; `JSON_UNQUOTE(json)` strips outer quotes and decodes escapes (non-string input passes through as its text form); `JSON_LENGTH(doc [, path])` returns top-level (or path-descended) element count — object=keys, array=length, scalar=1, missing path=NULL; `JSON_STORAGE_SIZE(doc)` returns byte length of the JSONB encoding; `JSON_ARRAY_APPEND(doc, path, val, ...)` variadic pairs, wrapping non-array targets before appending; `JSON_ARRAY_INSERT(doc, path, val, ...)` variadic pairs, final path segment must be `[idx]`, out-of-range indices clamp to append at end (MySQL parity). Helpers `descend_mut`, `json_array_append_at`, `json_array_insert_at` in `eval/functions/json.rs`. 17 integration tests in `tests/integration_mysql_json_bundle.rs`.
- [ ] 11.24 ⏳ Oracle JSON compatibility surface — split into sub-phases:
  - [x] ✅ 11.24a — Oracle scalar JSON functions: `JSON_EQUAL(a, b)` (deep structural equality, key-order-insensitive, NULL propagation on either side), `JSON_SCALAR(v)` (wraps a SQL scalar into a JSONB scalar; NULL → NULL), `JSON_SERIALIZE(v)` (canonical TEXT rendering of any JSON/JSONB input; also normalizes whitespace in TEXT inputs via serde round-trip). Implemented in `eval/functions/json.rs` reusing existing `value_to_serde_json` + `sql_to_serde_json` + `jsonb_blob_from_serde` helpers. 12 integration tests in `tests/integration_oracle_json.rs` (key-order equality, deep nesting, JSONB-vs-text equality, scalar wrapping of int/text, serialize roundtrip normalization, NULL propagation).
  - [x] ✅ 11.24b — `JSON_TRANSFORM(doc, op1, args…, op2, args…, …)` variadic multi-op mutator in **function-form** (not Oracle's special-form grammar; each op consumes its own arg count: SET=3, REMOVE=1, RENAME=2, APPEND=2, INSERT=2, REPLACE=2). Sequential apply reuses 11.22a helpers (`parse_mutation_path`, `set_path_ext`, `remove_path_parts`, `path_exists`) plus new `json_rename_at` helper. Semantic details: `SET` upserts (create_if_missing=true); `REPLACE` only overwrites existing paths (no-op on missing); `INSERT` silent no-op when path exists (MySQL-like); `APPEND` wraps non-array targets; `RENAME` only renames object keys; `REMOVE` tolerant no-op on missing paths or scalar roots. Unknown op → error. NULL doc → NULL. Missing op args → clear error mentioning the op name. Deliberate deviation from Oracle spec: function-form removes the need for JSON_TRANSFORM parser special-form grammar (`REMOVE path IGNORE ON MISSING` syntax) which would cost several days of parser work — can be layered on top later if user demand appears. 11 integration tests in `tests/integration_json_transform.rs`.
  - [ ] 11.24c ⏳ Dot notation for JSON columns (`t.doc.a.b` syntactic sugar for `JSON_EXTRACT(t.doc, '$.a.b')`); requires parser disambiguation against schema-qualified names.
  - [x] ✅ 11.24d (partial) — `JSON_DATAGUIDE(doc)` Oracle Data Guide analog: returns a JSON array of `{path, type}` entries describing every reachable subpath. Walker `json_dataguide_walk` recurses objects + arrays; root entry is `$`; type names: `null`, `boolean`, `integer` (i64/u64), `number` (float), `string`, `array`, `object`. Format-spec args (ORDERED, FORMAT) accepted but ignored in MVP. NULL doc → NULL. 6 integration tests in `tests/integration_json_dataguide.rs`. Remaining 11.24d items (JSON search index hooks, document-collection APIs, JSON-relational duality views) are storage-engine-level features deferred to dedicated phases.
  - Tracking spec: `specs/fase-11/spec-11.18-11.24-json-parity.md`

### Phase 12 — Testing + JIT `⏳` week 65-68
- [ ] 12.1 ⏳ Deterministic simulation testing — `FaultInjector` with seed
- [ ] 12.2 ⏳ EXPLAIN ANALYZE — real times per plan node; JSON output format compatible with PostgreSQL (`{"Plan":{"Node Type":..., "Actual Rows":..., "Actual Total Time":..., "Buffers":{}}}`) and indented text format for psql/CLI; metrics: actual rows, loops, shared/local buffers hit/read, planning time, execution time
- [ ] 12.3 ⏳ Basic JIT with LLVM — compile simple predicates to native code
- [ ] 12.4 ⏳ Final block 1 benchmarks — compare with MySQL and SQLite
- [ ] 12.5 ⏳ SQL parser fuzz testing — `cargo fuzz` on the parser with random inputs; register crashes as regression tests
- [ ] 12.6 ⏳ Storage fuzz testing — pages with random bytes, deliberate corruptions; verify that crash recovery handles corrupted data
- [ ] 12.7 ⏳ ORM compatibility tier 1 — Django ORM and SQLAlchemy connect, run simple migrations and SELECT/INSERT/UPDATE/DELETE queries without errors; document workarounds if any
- [ ] 12.8 ⏳ Unified axiom_* observability system — all system views use consistent naming, types, and join keys; `SELECT * FROM axiom_queries` shows running queries with pid, duration, state, sql_text, plan_hash; `SELECT * FROM axiom_bloat` shows table bloat (from 7.11); `SELECT * FROM axiom_slow_queries` is auto-populated when query exceeds `slow_query_threshold` (default 1s); `SELECT * FROM axiom_stats` shows database-wide metrics (cache hit rate, rows read/written, lock waits); `SELECT * FROM axiom_index_usage` shows which indexes are used/unused; unlike MySQL's inconsistent SHOW commands and PostgreSQL's complex pg_catalog joins, every axiom_* view is self-documented, joinable, and has the same timestamp/duration formats
- [x] 12.9 ✅ Date/time validation strictness — closed by verification: `coerce_helpers::parse_text_to_date_days` delegates to `ymd_to_days_checked` which rejects every out-of-range component (month ∉ 1..=12, day ∉ 1..=days_in_month including leap-year math for February). `'0000-00-00'`, `'2024-13-01'`, `'2024-00-15'`, `'2024-02-30'`, `'2023-02-29'`, `'2024-04-31'` all surface as `DbError::InvalidCoercion`. Regression coverage: `tests/integration_date_strictness.rs` (9 tests). `TIMESTAMP WITH TIME ZONE` / `SET AXIOM_COMPAT='mysql'` lenient opt-in deferred to a dedicated follow-up — current behavior matches MySQL strict mode

### Phase 13 — Advanced PostgreSQL `⏳` week 69-72
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — native pub-sub with `DashMap` of channels
- [ ] 13.5 ⏳ Covering indexes — store INCLUDE column values in B+ Tree leaf nodes; 6.13 already has catalog storage + IndexOnlyScan for key columns only; this phase adds the actual value payload to the leaf layout so index-only scans can return non-key projected columns without touching the heap
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + atomic swap
- [ ] 13.7 ⏳ Row-level locking — lock specific row during UPDATE/DELETE; reduces contention vs per-table lock from 7.5; **production-grade implementation moved to Phase 40.5** (hierarchical S/X/IS/IX intent locking, per-table lock table, wait queues); Phase 13.7 is superseded
- [ ] 13.8 ⏳ Deadlock detection — DFS on wait graph when lock_timeout expires; kill the youngest transaction; **production-grade implementation moved to Phase 40.6** (wait-for graph, cycle detection, victim selection); Phase 13.8 is superseded
- [ ] 13.8b ⏳ SELECT FOR UPDATE / SKIP LOCKED — pessimistic row locking for job queues, inventory checkout, concurrent reservations; `SELECT ... FOR UPDATE` acquires row lock until COMMIT/ROLLBACK; `SKIP LOCKED` skips already-locked rows instead of blocking; requires Phase 40.5 (Lock Manager) rather than 13.7; also covers `UPDATE t SET qty=qty-1 WHERE id=? AND qty>0` optimistic CAS pattern returning 0 rows on conflict (moved from 7.17)
- [x] 13.9 ✅ Immutable / append-only tables — closed: `TableDef.immutable: bool` (v3 on-disk row format — 10-byte trailer, v0/v1/v2 rows decode as `false`); `CatalogWriter::create_table_with_options` allocates with the flag; parser accepts `CREATE TABLE ... IMMUTABLE` after the column list; `execute_update_ctx` and `execute_delete_ctx` reject modifications with new `DbError::ImmutableTable` variant (SQLSTATE 42000, marked ignorable via `is_ignorable_on_error`); INSERTs still work. Guard also applied to multi-table `execute_update_join_ctx` / `execute_delete_join_ctx` and `execute_truncate` so `UPDATE t JOIN ...`, `DELETE t FROM t JOIN ...`, and `TRUNCATE TABLE t` all get the same protection. Coverage: `tests/integration_immutable_table.rs` (7 tests — INSERT allowed; UPDATE/DELETE/TRUNCATE/JOIN variants all rejected; non-immutable regression)
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
- [ ] 19.3 ⏳ Deadlock detection — DFS on wait graph every 100ms; ⚠️ TRIPLICATE: same feature also in 13.8 and 40.6 — implement once in Phase 40.6, then mark all three
- [ ] 19.4 ⏳ Statement fingerprinting — normalize SQL (remove literals, replace with `$1`, `$2`); hash the result to group identical queries with different parameters; prerequisite for pg_stat_statements and slow query log
- [ ] 19.4b ⏳ pg_stat_statements — fingerprint (via 19.4) + calls + total/min/max/stddev time + cache hits/misses per query
- [ ] 19.5 ⏳ Slow query log — JSON with execution plan
- [ ] 19.6 ⏳ Connection pooling — Semaphore + built-in idle pool; ⚠️ DUPLICATE of 16.10 — implement once and mark both
- [ ] 19.7 ⏳ pg_stat_activity — view and cancel running queries
- [ ] 19.7b ⏳ Cancel / kill query — `SELECT axiom_cancel_query(pid)` sends cancellation signal to a running query (like `pg_cancel_backend`); `axiom_terminate_session(pid)` forcibly closes a connection; without this, a runaway `SELECT * FROM logs` (millions of rows) cannot be stopped without restarting the server; integrates with pg_stat_activity (19.7) to expose the pid (moved from 7.18)
- [ ] 19.8 ⏳ pg_stat_progress_vacuum — real-time vacuum progress
- [x] 19.9 ✅ lock_timeout — implemented in Phase 7.10: `SET lock_timeout = N`; `DbError::LockTimeout` with MySQL error 1205 + SQLSTATE 40001; `lock_wait_timeout`/`innodb_lock_wait_timeout` aliases; per-session; default 30s
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
- [x] 21.1 ✅ Savepoints — `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name`, `RELEASE [SAVEPOINT] name` — implemented in Phase 7.12; stack-based MySQL/PG/SQLite compatible model; nested; duplicate names allowed (most-recent wins)
- [ ] 21.2 ⏳ CTEs — `WITH` queries
- [ ] 21.3 ⏳ Recursive CTEs — `WITH RECURSIVE` for trees and hierarchies
- [ ] 21.4 ⏳ RETURNING — in INSERT, UPDATE, DELETE
- [ ] 21.5 ⏳ MERGE / UPSERT — `ON CONFLICT DO UPDATE` + standard `MERGE`
- [ ] 21.5b ⏳ REPLACE INTO — `REPLACE INTO users (id, name) VALUES (1, 'Alice')`; MySQL shorthand for DELETE-then-INSERT; if the row does not exist it inserts; if it does, it deletes the old row and inserts the new one (triggers ON DELETE + ON INSERT, unlike ON DUPLICATE KEY UPDATE which triggers ON UPDATE); AUTO_INCREMENT increments on replace; very common in MySQL codebases for upsert-by-PK patterns
- [ ] 21.5c ⏳ INSERT IGNORE — `INSERT IGNORE INTO tags (post_id, tag) VALUES (1, 'rust')`; silences unique/FK/NOT NULL violations and inserts only the rows that don't conflict; returns warning count instead of error; used extensively for idempotent imports, tag systems, and bulk loads where partial success is acceptable
- [ ] 21.5d ⏳ Multi-table UPDATE/DELETE — `UPDATE orders o JOIN customers c ON o.customer_id = c.id SET o.priority = c.tier WHERE c.country = 'CO'`; and `DELETE o FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.deleted_at IS NOT NULL`; MySQL-specific syntax widely used in data migrations and cleanup scripts; different from standard SQL MERGE — simpler for the common "join + update/delete" pattern
- [ ] 21.5f ⏳ GENERATED ALWAYS AS (expr) STORED/VIRTUAL columns — computed columns not in parser or executor; `total DECIMAL(10,2) GENERATED ALWAYS AS (price * (1 + tax_rate)) STORED`; STORED: persisted on INSERT/UPDATE; VIRTUAL: computed at read time; requires parser extension in `ColumnDef` + executor materialization on INSERT/SELECT
- [x] 4.1f ✅ Version-conditional MySQL comments `/*!NNNNN SQL*/` — `expand_version_comments()` in `lexer.rs` preprocesses input before tokenization; includes SQL if NNNNN ≤ 80000; no version number → always include; fast path (no allocation) when `/*!` not present; 5 unit tests
- [ ] 21.5e ⏳ INSERT ... ON DUPLICATE KEY UPDATE — MySQL-specific upsert; attempt insert, and on `DuplicateKey` apply assignments via UPDATE instead of failing; used heavily by ORMs (Sequelize, TypeORM, GORM) for idempotent inserts; parser: extend `InsertStmt` with `on_duplicate: Option<Vec<Assignment>>`; executor: insert attempt + `DuplicateKey` catch + apply assignments; distinct from `REPLACE INTO` (which DELETEs + re-INSERTs) — `ON DUPLICATE KEY UPDATE` triggers `ON UPDATE` actions, not `ON DELETE` + `ON INSERT`
- [ ] 21.6 ⏳ CHECK constraints + DOMAIN types
- [ ] 21.6b ⏳ Exclusion constraints — `CREATE TABLE reservations (..., EXCLUDE USING btree (room_id WITH =, period WITH &&))`; prevents rows where ALL specified operators return TRUE simultaneously; B-Tree exclusion for equality (e.g., no duplicate active slugs); full range-overlap exclusion (hotel rooms, calendar slots, parking spots) requires GiST index (Phase 30.2); use case: `EXCLUDE USING gist (room WITH =, during WITH &&)` guarantees no two reservations overlap the same room in the same time period — impossible to enforce with CHECK or UNIQUE; `period` requires range type (Phase 20.13); document B-Tree subset now, GiST full power after Phase 30.2
- [ ] 21.7 ⏳ TEMP and UNLOGGED tables
- [ ] 21.8 ⏳ Expression indexes — `CREATE INDEX ON users(LOWER(email))`
- [ ] 21.9 ⏳ LATERAL joins
- [ ] 21.10 ⏳ Cursors — `DECLARE`, `FETCH`, `CLOSE`
- [ ] 21.11 ⏳ Query hints — `/*+ INDEX() HASH_JOIN() PARALLEL() */`
- [ ] 21.12 ⏳ DISTINCT ON — first row per group `SELECT DISTINCT ON (user_id) *`
- [x] 21.13 ✅ NULLS FIRST / NULLS LAST — implemented in Phase 4.10c; ASC→NULLS LAST and DESC→NULLS FIRST as PG defaults; explicit override supported
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
- [x] 24.9 ✅ UUID v4/v7 — implemented in Phase 4.19c: `gen_random_uuid()`/`uuid_generate_v4()` (v4 random); `uuid_generate_v7()`/`uuid7()` (v7 time-ordered, better B-tree locality); `is_valid_uuid(text)→BOOL`; storage as `[u8;16]` in codec
- [ ] 24.10 ⏳ INET, CIDR, MACADDR — network types with operators
- [ ] 24.11 ⏳ RANGE(T) — `int4range`, `daterange`, `tsrange` with `@>` and `&&`
- [ ] 24.12 ⏳ COMPOSITE types — `CREATE TYPE ... AS (fields)`
- [ ] 24.13 ⏳ Domain types — `CREATE DOMAIN email AS TEXT CHECK (VALUE ~ '^.+@.+$')` with constraint inheritance
- [ ] 24.14b ⏳ MySQL type aliases — `TINYTEXT` (≤255B), `MEDIUMTEXT` (≤16MB), `LONGTEXT` (≤4GB) stored as TEXT with length constraint; `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB` stored as BLOB with limit; `ZEROFILL` display attribute on integer columns (`INT(10) ZEROFILL` pads with zeros on display, stored as normal INT); `SET('a','b','c')` multi-value type (stores a bitmask, displays as comma-separated subset of declared values; different from ENUM which allows one value); these types are required to import `mysqldump` output without manual schema rewriting
- [ ] 24.3b ⏳ DECIMAL executor blocker — `DECIMAL(p,s)` is accepted by the parser but `executor/shared.rs:176` returns `NotImplemented`; blocks any schema with exact-decimal columns (financial amounts, prices, coordinates); simplest path: map to `ColumnType::Float` (lossy but unblocking); correct path: implement `ColumnType::Decimal(u8, u8)` with fixed-point arithmetic as planned in 24.3
- [ ] 24.15 ⏳ DATE column type — `DATE` columns are parsed but `executor/shared.rs:179` returns `NotImplemented`; blocks any schema with date-only columns (birthdays, deadlines, calendar events); map to `ColumnType::Timestamp` with day-level truncation, or add a dedicated `ColumnType::Date` variant; simpler and more common than TIMESTAMP for date-only fields
- [ ] 24.14 ⏳ Complete type tests — coercion, overflow, DECIMAL precision, timezone conversions

### Phase 25 — Type optimizations `⏳` week 70-72
- [ ] 25.1 ⏳ VarInt encoding — 1-9 byte integers by value + zigzag for negatives
- [ ] 25.2 ⏳ JSONB layout optimization — binary JSONB already exists in Phase 11.16; this follow-up is for a denser offset table / path cache / packed numeric representation if profiling shows the current JEntry layout is not enough
- [ ] 25.3 ⏳ VECTOR quantization — f16 (2x savings) and int8 (4x savings)
- [ ] 25.4 ⏳ PAX layout — columnar within each 8KB page
- [ ] 25.5 ⏳ Per-column statistics — histogram, correlation, most_common
- [x] 25.6 ✅ ANALYZE — implemented in Phase 6.12: `ANALYZE [TABLE name [(column)]]`; exact NDV full scan; resets staleness; `StaleStatsTracker` in SessionContext (Phase 6.11); auto-update after >20% row change
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
- [x] 28.1 ✅ Isolation levels — RC, RR, SERIALIZABLE (aliased to RR snapshot) implemented in Phase 7.1; `TxnManager::active_snapshot()` returns fresh (RC) or frozen (RR) snapshots; isolation tests in 7.13. Parser now also accepts standard SQL syntax `SET [SESSION|GLOBAL] TRANSACTION ISOLATION LEVEL READ UNCOMMITTED | READ COMMITTED | REPEATABLE READ | SERIALIZABLE` plus `SET TRANSACTION READ ONLY|WRITE` (rewritten to the existing `transaction_isolation` / `transaction_read_only` session variables). Coverage: `tests/integration_set_transaction.rs` (6 tests)
- [ ] 28.2 ⏳ SELECT FOR UPDATE / FOR SHARE / SKIP LOCKED / NOWAIT; ⚠️ DUPLICATE of 13.8b and 28.11 — implement once when Phase 40.5 (LockManager) is ready
- [ ] 28.3 ⏳ LOCK TABLE — ACCESS SHARE, ROW EXCLUSIVE, ACCESS EXCLUSIVE modes
- [ ] 28.4 ⏳ Advisory locks — `pg_advisory_lock` / `pg_try_advisory_lock`
- [ ] 28.5 ⏳ UNION / UNION ALL / INTERSECT / EXCEPT
- [x] 28.6 ✅ EXISTS / NOT EXISTS / IN subquery / correlated subqueries / derived tables — implemented in Phase 4.11; `SubqueryRunner` trait + `eval_with`; 14 integration tests
- [x] 28.7 ✅ Simple and searched CASE — implemented in Phase 4.24; NULL semantics; nested; works in SELECT/WHERE/ORDER BY/GROUP BY
- [ ] 28.8 ⏳ TABLESAMPLE SYSTEM and BERNOULLI with REPEATABLE
- [ ] 28.9 ⏳ Serializable Snapshot Isolation (SSI) — write-read dependency graph between transactions; DFS to detect cycles; automatic rollback of the youngest transaction on cycle detection; prerequisite: 7.1 (MVCC visibility)
- [ ] 28.10 ⏳ Isolation level tests — dirty read, non-repeatable read, phantom read; each test uses real concurrent transactions; verify that each level prevents exactly what it should and no more
- [ ] 28.11 ⏳ SELECT FOR UPDATE / FOR SHARE with skip locked — required by job queues; ⚠️ TRIPLICATE of 13.8b and 28.2 — implement once when Phase 40.5 is ready

### Phase 29 — Complete functions `⏳` week 82-84
- [ ] 29.1 ⏳ Advanced aggregations — `STRING_AGG`, `ARRAY_AGG`, `JSON_AGG`
- [ ] 29.2 ⏳ Statistical aggregations — `PERCENTILE_CONT`, `MODE`, `FILTER`
- [ ] 29.3 ⏳ Complete window functions — `NTILE`, `PERCENT_RANK`, `CUME_DIST`, `FIRST_VALUE`
- [ ] 29.4 ⏳ Text functions — `REGEXP_*`, `LPAD`, `RPAD`, `FORMAT`, `TRANSLATE`
- [ ] 29.5 ⏳ Date functions — `AT TIME ZONE`, `AGE`, `TO_CHAR`, `TO_DATE`
- [ ] 29.6 ⏳ Timezone database — embedded tzdata, portable without depending on the OS
- [ ] 29.7 ⏳ Math functions — trigonometry, logarithms, `GCD`, `RANDOM`
- [x] 29.8 ✅ COALESCE / NULLIF / GREATEST / LEAST — COALESCE + IFNULL + NVL + NULLIF implemented in `eval/functions/nulls.rs`; GREATEST and LEAST implemented in `eval/functions/numeric.rs` (verified at `"greatest"` / `"least"` match arms, lines 301 / 331)
- [ ] 29.9 ⏳ GENERATE_SERIES — numeric and date sequence generator; ⚠️ DUPLICATE of 20.10 — implement once and mark both
- [ ] 29.10 ⏳ UNNEST — expand array to individual rows; ⚠️ DUPLICATE of 20.14 — implement once and mark both
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
- [ ] 29.4b ⏳ HEX() / UNHEX() — not implemented in `eval/functions/`; `HEX(n)` returns the hexadecimal representation of an integer or string; `UNHEX(s)` returns the binary string represented by hex pairs; commonly used for binary id encoding, UUID display, and binary protocol parsing
- [ ] 29.5b ⏳ DATE_ADD() / DATE_SUB() — not implemented; `DATE_ADD(date, INTERVAL n unit)` / `DATE_SUB(date, INTERVAL n unit)`; needed for date arithmetic (due dates, expiry dates, rolling windows); MySQL-specific but universal in MySQL codebases; requires INTERVAL parsing in the parser
- [ ] 29.5c ⏳ TIMESTAMPDIFF() — not implemented; `TIMESTAMPDIFF(unit, ts1, ts2)` returns the difference between two timestamps in the given unit (SECOND, MINUTE, HOUR, DAY, MONTH, YEAR); common in age calculations, SLA timers, duration reports; unit is a keyword not a string
- [ ] 29.8b ⏳ GREATEST() / LEAST() — not implemented; `GREATEST(a, b, c, ...)` / `LEAST(a, b, c, ...)` return the largest/smallest non-NULL argument; common for clamping values (score caps, range enforcement, salary bands); `COALESCE` / `NULLIF` already work (Phase 4.19); these are the comparable value-selection companions; add to `eval/functions/` alongside existing comparators
- [ ] 29.5d ⏳ DATE() / TIME() scalar + EXTRACT() + ADDDATE() — `DATE(ts)` extracts date part from timestamp; `TIME(ts)` extracts time part; `EXTRACT(YEAR FROM ts)` SQL standard (requires special parser token — unit is keyword, not argument); `ADDDATE()` / `SUBDATE()` are MySQL aliases for DATE_ADD/DATE_SUB; additional date components: `WEEK()`, `WEEKDAY()`, `WEEKOFYEAR()`, `QUARTER()`, `DAYNAME()`, `MONTHNAME()`, `DAYOFWEEK()`, `DAYOFMONTH()`, `DAYOFYEAR()`, `YEARWEEK()`, `LAST_DAY()`, `MAKEDATE()`, `MAKETIME()`, `TIME_TO_SEC()`, `SEC_TO_TIME()`; all belong in `eval/functions/datetime.rs`
- [ ] 29.4c ⏳ SHA1() / SHA2() / MD5() hash functions — not implemented; `MD5(str)` / `SHA1(str)` / `SHA2(str, 256)` return hex-encoded digests; used in password migration scripts, content checksums, and audit trails; add to `eval/functions/` (sha1 + sha2 Rust crates or ring)
- [ ] 29.4d ⏳ TRUNCATE(n, d) numeric function — `TRUNCATE(3.14159, 2)` → `3.14`; different from `TRUNCATE TABLE`; used in financial rounding; add to `eval/functions/numeric.rs`
- [ ] 29.4e ⏳ BIN() / OCT() / CONV() base conversion — `BIN(255)` → `'11111111'`; `OCT(8)` → `'10'`; `CONV('ff', 16, 10)` → `'255'`; used in bit-manipulation queries and protocol parsing; add to `eval/functions/numeric.rs` or `string.rs`
- [ ] 29.4f ⏳ ELT() / FIELD() string lookup — `ELT(n, s1, s2, ...)` returns the Nth string; `FIELD(s, s1, s2, ...)` returns 1-based position of first match (0 if not found); used for enum-style lookups and reordering queries; add to `eval/functions/string.rs`
- [x] 4.6e ✅ INSERT LOW_PRIORITY / HIGH_PRIORITY / DELAYED — `parse_insert()` now consumes these idents before IGNORE/INTO via `eat_ident_ci`; no semantic change; `INSERT LOW_PRIORITY INTO t VALUES (1)` now parses correctly
- [ ] 4.5g ⏳ SELECT ... INTO @var — `SELECT name INTO @user_var FROM users LIMIT 1` not in parser or AST; common MySQL idiom for single-row variable assignment; parser/dml.rs needs an `INTO @ident` clause after the SELECT list
- [ ] 29.4g ⏳ FORMAT(n, d) — `FORMAT(1234567.89, 2)` → `'1,234,567.89'`; formats a number with thousands separators and d decimal places; common in reporting queries and display columns; `eval/functions/string.rs`
- [ ] 29.7b ⏳ SLEEP() — `SLEEP(N)` pauses N seconds and returns 0 (1 if interrupted); widely used in integration tests, simulations, and rate-limit testing; add to `eval/functions/system.rs`

### Phase 30 — Pro infrastructure `⏳` week 85-87
- [ ] 30.1 ⏳ Generalized GIN infrastructure — array GIN, posting-list compression, pending-list fast update, multi-column GIN, and remaining JSONB opclasses; basic JSONB `@>` GIN exists in Phase 11.17 and trigram indexing exists in Phase 11.4b
- [ ] 30.2 ⏳ GiST indexes — for ranges and geometry
- [ ] 30.3 ⏳ BRIN advanced — multi-column BRIN, custom `pages_per_range`, `BRIN_SUMMARIZE_NEW_VALUES()`, integration with GiST for geometric ranges (basic BRIN implemented in 11.1b)
- [ ] 30.4 ⏳ Hash indexes — O(1) for exact equality
- [ ] 30.5 ⏳ CREATE INDEX CONCURRENTLY — without blocking writes
- [ ] 30.6 ⏳ Complete information_schema — tables, columns, constraints
- [ ] 30.7 ⏳ Basic pg_catalog — pg_class, pg_attribute, pg_index
- [x] 30.8 ⚠️ DESCRIBE / SHOW TABLES / SHOW COLUMNS — implemented in Phase 4.20 (MySQL-compatible 6-column output); SHOW CREATE TABLE NOT yet implemented → remaining gap
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
- [x] 31.12 ✅ Connection string DSN — implemented in Phase 5.15: `axiomdb://`, `mysql://`, `postgres://`, `file:` and plain paths; percent-decodes credentials; `Db::open_dsn`, `axiomdb_open_dsn`; `AXIOMDB_URL` env var
- [ ] 31.13 ⏳ Read replicas routing — automatically route read-only queries to replicas from the connection pool

### Phase 32 — Final architecture `⏳` week 91-93
- [ ] 32.1 ⏳ Complete workspace refactor — 18+ specialized crates
- [ ] 32.2 ⏳ Interchangeable StorageEngine trait — Mmap, Memory, Encrypted, Fault
- [ ] 32.3 ⏳ Interchangeable Index trait — BTree, Hash, Gin, Gist, Brin, Hnsw, Fts
- [ ] 32.4 ⏳ Central engine with complete pipeline — cache→parse→rbac→plan→opt→exec→audit
- [ ] 32.5 ⏳ WAL as event bus — replication, CDC, cache, triggers, audit
- [ ] 32.6 ⏳ Release profiles — LTO fat, codegen-units=1, panic=abort
- [x] 32.7 ✅ CI/CD — GitHub Actions implemented: `.github/workflows/ci.yml` (push/PR: test + clippy) + `release.yml` (tags v*: Linux musl + macOS arm64/x86 binaries) + `deploy-docs.yml`; ⚠️ DUPLICATE of 35.9
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
- [x] 35.9 ✅ GitHub Actions CI — implemented (see Phase 32.7: ci.yml + release.yml + deploy-docs.yml); fuzz targets still pending
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

## BLOCK 12 — Browser Engine (Phase 38)

> **Design decision (2026-03-30):** AxiomDB already has a clean separation between
> engine logic (parser, executor, B+ Tree, buffer pool, MVCC) and I/O (storage backend).
> By compiling the engine crates to `wasm32-wasi` and providing an OPFS-backed
> StorageEngine implementation, the full database runs inside a Web Worker — no POSIX
> emulation layer, no SQLite port. This is not a toy: it's the same engine, same SQL,
> same MVCC, compiled to a different target.
>
> **Key differentiator vs existing solutions (sql.js, PGlite, wa-sqlite):** Those are
> ports of C/C++ engines that emulate POSIX over browser APIs, adding overhead.
> AxiomDB-Wasm is the engine itself compiled natively — no emulation layer, smaller
> binary (~200KB vs 800KB-3MB), and features designed for browser from day one
> (reactive queries, CRDT sync, tab coordination).

### Phase 38 — AxiomDB-Wasm: Browser Database Engine `⏳` week 122-130

#### 38.A — Wasm compilation target
- [ ] 38.1 ⏳ Compile axiomdb-core, axiomdb-sql, axiomdb-storage to wasm32-wasi — verify all pure-Rust crates compile clean without std::fs / std::net
- [ ] 38.2 ⏳ Feature-gate all OS-dependent code (`#[cfg(not(target_arch = "wasm32"))]`) — mmap, tokio, TCP, file I/O
- [ ] 38.3 ⏳ Wasm-compatible allocator — wee_alloc or dlmalloc for smaller binary size
- [ ] 38.4 ⏳ Binary size budget: ≤200KB gzipped for core engine (parser + executor + B+ Tree + buffer pool)
- [ ] 38.5 ⏳ `cargo build --target wasm32-unknown-unknown` passes clean for engine crates

#### 38.B — OPFS storage backend
- [ ] 38.6 ⏳ `OpfsStorageEngine` — implements StorageEngine trait over Origin Private File System
- [ ] 38.7 ⏳ Synchronous access via `FileSystemSyncAccessHandle` inside Web Worker (read/write/flush at byte offsets)
- [ ] 38.8 ⏳ Page-level I/O: 16KB pages read/written directly to OPFS — same page format as native engine
- [ ] 38.9 ⏳ WAL on OPFS — append-only log file, same format as native WAL, crash recovery on page reload
- [ ] 38.10 ⏳ Storage quota detection — `navigator.storage.estimate()` to warn before hitting browser limits
- [ ] 38.11 ⏳ Fallback to IndexedDB for browsers without OPFS sync access (Safari, older Firefox)

#### 38.C — JavaScript bindings
- [ ] 38.12 ⏳ `wasm-bindgen` API: `AxiomDB.open(name)`, `.execute(sql)`, `.query(sql)` — returns JS objects
- [ ] 38.13 ⏳ Web Worker wrapper — all DB operations run off main thread, communicate via `postMessage`
- [ ] 38.14 ⏳ Promise-based async API: `const rows = await db.query("SELECT * FROM users WHERE id = ?", [42])`
- [ ] 38.15 ⏳ Prepared statements: `const stmt = db.prepare(sql)` — reuse parsed plan, bind params per execution
- [ ] 38.16 ⏳ TypeScript type definitions — full `.d.ts` with generics for query results
- [ ] 38.17 ⏳ npm package: `@axiomdb/browser` — zero dependencies, ESM + CJS, tree-shakeable

#### 38.D — Reactive queries (browser-native)
- [ ] 38.18 ⏳ Live queries: `db.watch("SELECT * FROM todos WHERE done = false", callback)` — callback fires on every INSERT/UPDATE/DELETE that changes the result set
- [ ] 38.19 ⏳ Efficient invalidation — WAL-based change tracking per table, only re-execute watched queries on affected tables
- [ ] 38.20 ⏳ React hook: `useAxiomQuery(sql, params)` — returns reactive state, auto-subscribes/unsubscribes
- [ ] 38.21 ⏳ Vue composable: `useAxiomQuery(sql, params)` — same semantics, Vue reactivity system
- [ ] 38.22 ⏳ Svelte store: `axiomQuery(sql, params)` — Svelte writable store with auto-subscription

#### 38.E — Multi-tab coordination
- [ ] 38.23 ⏳ SharedWorker or BroadcastChannel — single writer across all tabs, prevent OPFS lock conflicts
- [ ] 38.24 ⏳ Tab-aware connection pool — tabs share one DB instance, queries routed to the shared worker
- [ ] 38.25 ⏳ Cross-tab live query notifications — change in tab A triggers reactive update in tab B

#### 38.F — Sync engine (offline-first)
- [ ] 38.26 ⏳ CRDT-based merge — last-write-wins per column with Hybrid Logical Clocks (HLC)
- [ ] 38.27 ⏳ Sync protocol: browser ↔ AxiomDB server — delta sync over WebSocket, only changed rows since last sync
- [ ] 38.28 ⏳ Conflict resolution strategies: LWW (default), server-wins, client-wins, custom merge function
- [ ] 38.29 ⏳ Offline queue — mutations while offline are queued and replayed on reconnect in order
- [ ] 38.30 ⏳ Sync status API: `db.sync.status` → `{ state: 'syncing' | 'synced' | 'offline', pending: 12 }`

#### 38.G — Performance and limits
- [ ] 38.31 ⏳ Benchmark: point lookup latency in Wasm vs native (target: ≤3× native)
- [ ] 38.32 ⏳ Benchmark: INSERT throughput in Wasm+OPFS (target: ≥50K rows/s)
- [ ] 38.33 ⏳ Benchmark: binary size vs sql.js, PGlite, wa-sqlite, DuckDB-Wasm
- [ ] 38.34 ⏳ Benchmark: cold start time (Wasm instantiation + OPFS open, target: <100ms)
- [ ] 38.35 ⏳ Memory pressure handling — graceful eviction when browser signals memory pressure (`performance.measureUserAgentSpecificMemory()`)

#### 38.H — Developer experience
- [ ] 38.36 ⏳ `axiomdb-browser` DevTools extension — inspect tables, run queries, see WAL, monitor sync status
- [ ] 38.37 ⏳ Migration support: `db.migrate(version, upSQL, downSQL)` — versioned schema migrations stored in OPFS
- [ ] 38.38 ⏳ Seed data: `db.seed(sql)` — run initial data script only on first open
- [ ] 38.39 ⏳ Export/import: `db.export()` → ArrayBuffer (full DB file), `AxiomDB.import(buffer)` — portable backups
- [ ] 38.40 ⏳ Encryption at rest: `AxiomDB.open(name, { encryption: key })` — AES-256-GCM per page, key never touches disk

#### 38.I — Quality
- [ ] 38.41 ⏳ Integration tests in Playwright — real browser (Chrome, Firefox, Safari), real OPFS
- [ ] 38.42 ⏳ Stress test: 100K rows INSERT + SELECT + live query in single tab
- [ ] 38.43 ⏳ Multi-tab stress test: 4 tabs writing concurrently, verify consistency
- [ ] 38.44 ⏳ Sync integration test: browser ↔ AxiomDB server, network interruption, reconnect, verify convergence
- [ ] 38.45 ⏳ Documentation — user guide: getting started, React/Vue/Svelte integration, sync setup, migration guide
- [ ] 38.46 ⏳ Documentation — internals: OPFS storage engine, Wasm compilation, sync protocol, CRDT implementation

---

> **🏁 FULL PLATFORM CHECKPOINT — week ~130**
> On completing Phase 38, AxiomDB runs everywhere:
> - Server: Linux/macOS/Windows via TCP (MySQL wire protocol)
> - Embedded: desktop (Tauri, Electron), mobile (Flutter, React Native)
> - Browser: Wasm + OPFS, offline-first, reactive queries, multi-tab, sync
> - Same engine, same SQL, same MVCC — every target

---

### Phase 39 🔄 (26/38) — Clustered index storage engine: variable-size slotted leaf+internal pages, clustered B-Tree insert+lookup+range+update+delete+rebalance (byte-volume splits, separator propagation, root collapse), secondary PK bookmarks (`secondary_key ++ missing_PK_cols`), overflow pages (local prefix + chain), WAL (ClusteredRowImage, EntryType::Clustered*), crash recovery (PK-based undo/redo), SQL executor integration (CREATE TABLE/INSERT/SELECT/UPDATE/DELETE for explicit-PK tables), VACUUM (clustered purge + overflow free + secondary cleanup), heap→clustered rebuild (ALTER TABLE REBUILD), aggregate hash (GroupTablePrimitive/Generic, 14.25× speedup), zero-alloc clustered scan (scan_all_callback), UPDATE in-place field patch (≥1M r/s); 39.22 ✅
- [ ] ⚠️ Clustered older-version reconstruction (MVCC version chains) — lookup/range/update return None for invisible current versions; deferred to later clustered MVCC / version-chain work (affects 39.4, 39.5, 39.6, 39.8, 39.14, 39.15)
- [ ] ⚠️ Clustered root checkpoint persistence — roots still reconstructed from WAL history after crash; checkpoint/rotation-stable persistence deferred (39.12)
- [ ] ⚠️ FK enforcement for clustered child tables — parent-side enforcement works; child-table deferred (39.17)
- [ ] ⚠️ Parent separator repair page overflow — repair assumes new separator fits in current internal page budget; revisit in 39.20 (39.8)
- [ ] ⚠️ 39.19b — ALTER TABLE ADD/DROP COLUMN on clustered tables: code complete (246 lines, `rewrite_rows_clustered`, 13 tests) but NOT YET COMMITTED
### Phase 40 ✅ (16/16) — Clustered Engine Performance: ClusteredInsertBatch (55.9K r/s, +59% vs MySQL 8), CREATE INDEX on clustered tables, statement plan cache (OID-based invalidation, LRU eviction), StorageEngine interior mutability (64-shard PageLockTable), ALTER TABLE ADD/DROP/MODIFY on clustered, INSERT DEFAULT VALUES + SHOW INDEX, concurrent WAL writer (lock-free LSN reservation, group commit leader/follower), per-connection TxnState + atomic MVCC snapshots (DuckDB GC horizon), Lock Manager (5 modes, 64-shard, InnoDB-pattern), deadlock detection (Brent's O(1) cycle-finding), HeapChain concurrent access (page X-latch, atomic chain growth), B-tree latch coupling (hybrid optimistic/pessimistic, both index+clustered trees), FreeList tier-1 batch (703× speedup 8-thread), database lock redesign (SharedDatabase, per-subsystem sync, DDL write lock / DML read lock), executor lock integration (IX table + X row per DML), integration tests (2540 workspace tests, 8 concurrent DML tests)
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
