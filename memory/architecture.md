# Architecture Notes

## 2026-04-02 — Clustered internal page primitive, clustered insert, point lookup, range scan, same-leaf update, delete-mark, structural rebalance, and clustered secondary bookmarks

- `crates/axiomdb-storage/src/clustered_internal.rs` owns the clustered
  internal page format for Phase `39.2`.
- `crates/axiomdb-storage/src/clustered_tree.rs` now owns the first clustered
  tree controller for Phases `39.3`, `39.4`, `39.5`, `39.6`, `39.7`, and `39.8`.
- `crates/axiomdb-sql/src/clustered_secondary.rs` now owns the clustered-first
  secondary bookmark layout for Phase `39.9`.
- The architecture still keeps storage and tree responsibilities separate:
  - storage owns page layout, free-space accounting, binary search, and page-local mutation
  - `clustered_tree` owns descent, exact leaf search, split planning, separator propagation, and root growth
  - the executor is still not wired to clustered tables
- The child mapping rule remains explicit:
  - `leftmost_child` lives in the 16-byte page-local header
  - every separator cell stores only its `right_child`
  - logical child `i` is:
    - `0` → `leftmost_child`
    - `i > 0` → `right_child` of cell `i - 1`
- The clustered split policy is intentionally in-place for the left half:
  - old page ID stays as the left page
  - only the new right sibling is allocated
  - parent propagation inserts only `(separator_key, right_child_pid)`
- The clustered point-lookup contract is currently one-version only:
  - visible current inline row → return it
  - missing key → `None`
  - invisible current inline row → `None`
  - no undo/version-chain reconstruction until later clustered MVCC phases
- The clustered range-scan contract mirrors that same boundary:
  - descend once to the first relevant leaf for the lower bound
  - then iterate through the `next_leaf` chain in key order
  - yield only visible current inline rows
  - skip invisible current inline rows instead of reconstructing older versions
  - issue bounded prefetch hints while crossing leaf boundaries
- The clustered update contract is now also explicit:
  - descend to the owning leaf by exact primary key
  - rewrite only the current inline version in that leaf
  - keep key order, parent separators, and `next_leaf` unchanged
  - allow overwrite or same-leaf rebuild, but never structural relocation in `39.6`
  - report `HeapPageFull` when the row no longer fits in the same leaf
- The clustered delete contract is now also explicit:
  - descend to the owning leaf by exact primary key
  - stamp `txn_id_deleted` on the current inline version only when it is visible
  - preserve key bytes, row payload bytes, `txn_id_created`, `row_version`, and `next_leaf`
  - keep the physical cell inline so older snapshots can still observe it
  - defer purge, merge, undo, and WAL to later clustered phases
- The clustered structural-maintenance contract is now also explicit:
  - `update_with_relocation(...)` uses `update_in_place(...)` as the fast path
  - physical delete is private helper logic, not the public delete API
  - underfull and minimum-key-change signals propagate upward separately
  - sibling rebalance uses encoded-byte occupancy, not fixed key counts
  - leaf merge preserves `next_leaf`
  - an empty internal root collapses to its only child
  - current separator repair assumes the repaired key still fits on the parent page
- The clustered secondary-bookmark contract is now explicit:
  - physical secondary key = `secondary_logical_key ++ missing_primary_key_columns`
  - missing PK columns are derived from the primary index definition, not hard-coded in catalog
  - scanned secondary entries decode into a logical secondary key plus a full PK bookmark
  - relocate-only updates are secondary no-ops when both the logical secondary key and PK stay stable
  - the old `RecordId` payload in `axiomdb-index::BTree` is retained only for compatibility with the fixed page format
- Variable-size occupancy is measured in encoded bytes, not key count:
  - clustered leaves split by cumulative cell footprint
  - clustered internals split by cumulative separator footprint
  - clustered underflow rebalance also uses cumulative encoded bytes
- `remove_at(key_pos, child_pos)` intentionally supports only the two adjacent
  child removals (`child_pos == key_pos` or `child_pos == key_pos + 1`), which
  matches real B-tree merge / redistribution semantics and avoids an overly
  permissive page-local API.

## 2026-03-29 — Thematic integration test binaries for `axiomdb-sql`

- `crates/axiomdb-sql/tests/common/mod.rs` now owns the shared executor-test
  harness (`setup`, `run`, `run_ctx`, staging helpers, row extractors).
- Integration tests are now grouped by execution path rather than by one giant
  catch-all file:
  - `integration_executor` for base CRUD
  - `integration_executor_joins` for joins and aggregates
  - `integration_executor_query` for query-shaping features
  - `integration_executor_ddl` for `SHOW` / `DESCRIBE` / `TRUNCATE` / `ALTER`
  - `integration_executor_ctx` for base `SessionContext` and `strict_mode`
  - `integration_executor_ctx_group` for ctx-path sorted group-by
  - `integration_executor_ctx_limit` for ctx-path `LIMIT` / `OFFSET` coercion
  - `integration_executor_ctx_on_error` for ctx-path `on_error`
  - `integration_executor_sql` for broader SQL semantics
  - `integration_delete_apply` for DELETE fast paths
  - `integration_insert_staging` for transactional INSERT staging
  - `integration_namespacing` for database catalog behavior
  - `integration_namespacing_cross_db` for explicit cross-database resolution
  - `integration_namespacing_schema` for schema namespacing and `search_path`
- The structural rule is:
  - keep one binary per cohesive execution path
  - prefer adding related tests to an existing themed binary
  - split only when a binary starts mixing unrelated paths or grows beyond a practical review/run size
- Current watch list for future split is empty inside `axiomdb-sql/tests/`; the larger remaining bins are still responsibility-cohesive.

## 2026-03-26 — Prepared long data over MySQL wire

- `COM_STMT_SEND_LONG_DATA` is handled entirely in `axiomdb-network/src/mysql/handler.rs`.
- The command does not take the `Database` mutex and never touches the SQL engine directly.
- Pending bytes are owned by `PreparedStatement` in `axiomdb-network/src/mysql/session.rs`.
- Chunks are stored as raw bytes and decoded only in `parse_execute_packet(...)`.
- Long data has precedence over both the inline execute payload and the null bitmap.
- `COM_STMT_RESET` clears stmt-local long-data state but keeps the prepared statement, cached analyzed plan, and type metadata.
- `SHOW STATUS` exposes `Com_stmt_send_long_data` in both session and global scope.

## 2026-03-26 — Executor decomposition

- `crates/axiomdb-sql/src/executor.rs` was replaced by `crates/axiomdb-sql/src/executor/`.
- `executor/mod.rs` remains the stable facade for `execute`, `execute_with_ctx`, and `last_insert_id_value()`.
- Responsibility is now split across:
  - `shared.rs`
  - `select.rs`
  - `joins.rs`
  - `aggregate.rs`
  - `insert.rs`
  - `update.rs`
  - `delete.rs`
  - `bulk_empty.rs`
  - `ddl.rs`
- The current implementation keeps a single logical module via `include!` inside `mod.rs`, which preserves private helper visibility while eliminating the 7K-line monolith.
- This is an internal refactor only. No SQL-visible behavior or public crate API changed.

## 2026-03-26 — Batched B+Tree delete for DELETE / UPDATE

- `axiomdb-index/src/tree.rs` now exposes `BTree::delete_many_in(...)` for exact
  encoded keys that are already sorted ascending.
- The batch primitive partitions the delete slice by child range, recurses once
  per affected child, then normalizes the parent once instead of calling the
  point-delete path in a loop.
- `axiomdb-sql/src/index_maintenance.rs` stages delete keys per index with
  `collect_delete_keys_by_index(...)` and executes one batch delete per index
  through `delete_many_from_indexes(...)`.
- `DELETE` now batch-deletes index keys after heap deletion.
- `UPDATE` now batch-deletes old keys before reinserting new keys, which keeps
  PRIMARY KEY and secondary indexes correct even though heap `RecordId`s change.

## 2026-03-26 — Eval decomposition

- `crates/axiomdb-sql/src/eval.rs` was replaced by `crates/axiomdb-sql/src/eval/`.
- `eval/mod.rs` keeps the old public facade:
  - `eval`
  - `eval_with`
  - `eval_in_session`
  - `eval_with_in_session`
  - `is_truthy`
  - `like_match`
  - `CollationGuard`
  - `ClosureRunner`
  - `NoSubquery`
  - `SubqueryRunner`
- Internal responsibilities are now split into:
  - `context.rs`
  - `core.rs`
  - `ops.rs`
  - `functions/` by family
- This is structural only. No SQL-visible evaluator behavior changed.

## 2026-03-26 — Stable-RID UPDATE fast path

- `axiomdb-storage/src/heap.rs` now exposes same-slot tuple rewrite/restore helpers.
- `axiomdb-storage/src/heap_chain.rs` batches same-page stable-RID rewrites so a
  page is read once and written once for the eligible rows in that batch.
- `axiomdb-wal` adds `EntryType::UpdateInPlace` plus matching undo/recovery handling.
- `axiomdb-sql/src/table.rs` now tries a preserve-RID branch before falling back to
  heap delete+insert.
- Index skipping in UPDATE is now keyed on two facts together:
  - the RID stayed stable
  - the logical key / partial-index membership for that index did not change
- This fixed the large UPDATE throughput gap without weakening rollback or recovery.

## 2026-03-27 — Transactional INSERT staging

- `axiomdb-sql/src/session.rs` now owns `PendingInsertBatch` in `SessionContext`.
- The current staging design is session-scoped but transaction-gated:
  - only explicit transactions (`BEGIN ... COMMIT`) can append to the batch
  - autocommit-wrapped single statements do not use it
- `axiomdb-sql/src/executor/staging.rs` flushes staged rows through:
  - `TableEngine::insert_rows_batch_with_ctx(...)`
  - `batch_insert_into_indexes(...)`
  - one catalog root update per changed index
- The critical ordering invariant is:
  - if the next statement cannot continue the current INSERT batch, flush it
    before taking that statement's savepoint
- This invariant matters for `rollback_statement` / `savepoint` / `ignore`
  modes: a later failing statement must not roll back staged rows that logically
  belonged to earlier successful INSERT statements.

## 2026-03-26 — Explicit MySQL connection lifecycle

- `axiomdb-network/src/mysql/lifecycle.rs` now owns transport-phase tracking for
  MySQL connections.
- The lifecycle is explicit: `CONNECTED -> AUTH -> IDLE -> EXECUTING -> CLOSING`.
- `ConnectionLifecycle` is intentionally separate from `ConnectionState`.
- `ConnectionState` still owns SQL session variables, prepared statements,
  warnings, and session counters.
- `ConnectionLifecycle` owns timeout policy, client interactivity classification,
  and socket configuration helpers.
- Auth uses a fixed 10-second timeout.
- Idle uses `interactive_timeout` or `wait_timeout` depending on the original
  handshake capability flags.
- Packet writes during command execution use `net_write_timeout`.
- `COM_RESET_CONNECTION` recreates `ConnectionState` and resets timeout vars to
  defaults, but preserves lifecycle metadata such as interactive classification.

## 2026-03-27 — Shared DSN parsing for server and embedded

- `axiomdb-core/src/dsn.rs` now owns DSN normalization and classification.
- The parser returns one of two typed shapes:
  - `ParsedDsn::Wire(WireEndpointDsn)`
  - `ParsedDsn::Local(LocalPathDsn)`
- `mysql://`, `postgres://`, and `postgresql://` are parsing aliases only.
- `axiomdb-server` consumes only the wire shape and currently uses only:
  - bind host
  - bind port
  - `data_dir` query param
- `axiomdb-embedded` consumes only the local shape and rejects query params in
  `5.15`.
- The architectural rule is:
  - parse once in core
  - validate per consumer
  - do not silently invent semantics for unsupported DSN fields

## 2026-03-27 — Startup index integrity verification

- `axiomdb-sql/src/index_integrity.rs` owns the startup verifier.
- The verifier treats heap-visible rows as source of truth and compares them
  against every catalog-visible index.
- Readable divergence is repaired by:
  - rebuilding a fresh index root from heap rows
  - flushing the rebuilt pages
  - rotating the catalog root in a WAL-protected txn
  - deferring free of old tree pages until commit durability is confirmed
- Unreadable or non-enumerable index trees are not auto-healed; open fails with
  `DbError::IndexIntegrityFailure`.

## 2026-03-27 — PRIMARY KEY SELECT planner parity

- `axiomdb-sql/src/planner.rs` now treats PRIMARY KEY indexes as first-class
  candidates for single-table `SELECT`.
- The local architectural rule is now:
  - PK equality uses `IndexLookup` unconditionally
  - PK range can use `IndexRange` through the normal extraction path
  - session collation can still veto text-key index access, even on PK
- No executor redesign was needed:
  - `executor/select.rs` already handled `IndexLookup` / `IndexRange`
  - the gap was planner eligibility only
- Both `axiomdb-network` and `axiomdb-embedded` now call the same verifier
  immediately after `open_with_recovery(...)` and before serving traffic.

## 2026-03-27 — Indexed UPDATE candidate planning

- `axiomdb-sql/src/planner.rs` now owns a second DML discovery entrypoint:
  - `plan_update_candidates(...)`
  - `plan_update_candidates_ctx(...)`
- UPDATE candidate planning intentionally mirrors DELETE candidate planning:
  - no `stats_cost_gate`
  - no `IndexOnlyScan`
  - PK, UNIQUE, secondary, and eligible partial indexes are allowed
  - non-binary session collation can still veto text-key index access
- `executor/update.rs` now separates candidate discovery from physical rewrite:
  - planner selects an access path
  - candidate `RecordId`s are materialized first
  - fetched rows recheck the full `WHERE`
  - surviving rows flow into the existing `5.20` stable-RID / fallback write path
- The architectural boundary is now explicit:
  - `6.17` owns indexed UPDATE discovery
  - `5.20` remains the source of truth for heap/index rewrite semantics

## 2026-03-27 — Indexed multi-row INSERT batch apply

- `axiomdb-sql/src/executor/staging.rs` now owns shared physical apply helpers:
  - `apply_insert_batch(...)`
  - `apply_insert_batch_with_ctx(...)`
- Those helpers are reused by two logically different producers:
  - `5.21` transactional staging flushes
  - `6.18` immediate `INSERT ... VALUES (...), (... )`
- The architectural rule is:
  - share grouped heap/index physical apply
  - do not share staged bulk-load semantics blindly
- In particular, the immediate multi-row path must not reuse the staged
  `committed_empty` optimization because it lacks the staging path's
  `unique_seen` prevalidation and still must reject duplicate PRIMARY KEY /
  UNIQUE values inside a single SQL statement atomically.

## 2026-03-27 — WAL fsync pipeline

- `axiomdb-wal/src/fsync_pipeline.rs` now owns server-side fsync coalescing.
- The server path no longer depends on the old timer-based `CommitCoordinator`
  modules from `axiomdb-network/src/mysql/`.
- The runtime contract is:
  - `TxnManager::commit()` still writes the Commit record first
  - the server then calls `pipeline.acquire(commit_lsn, txn_id)`
  - `Expired` means a previous leader already covered this LSN
  - `Acquired` means this connection must `flush+fsync`
  - `Queued(rx)` means release the DB lock and await the leader
- The old `deferred_commit_mode` hook still exists inside `TxnManager`, but it
  is now just the internal handoff point from commit serialization to the
  leader-based fsync pipeline.

## 2026-03-27 — Database catalog and session-scoped default database

- `axiomdb-catalog` now persists logical databases explicitly instead of treating
  `SHOW DATABASES` and `USE` as wire-only session sugar.
- Two catalog relations own that state:
  - `axiom_databases` stores database definitions
  - `axiom_table_databases` stores database ownership per table id
- The architectural compatibility rule is:
  - tables created before `22b.3a` remain readable without migration
  - missing ownership rows resolve to the implicit default database `axiomdb`
- `axiomdb-sql/src/analyzer.rs` now applies database defaults in addition to
  schema defaults:
  - selected database from `SessionContext` wins
  - otherwise `axiomdb` is the effective default
- The current namespace model is intentionally two-level:
  - database ownership decides which table set is visible
  - `schema_name` inside `TableDef` remains available for future schema work
- Cross-database qualification is intentionally still deferred:
  - the parser/analyzer continue to accept only the current one-part or
    `schema.table` forms
  - `database.schema.table` belongs to the next subphase, not this one
- `DROP DATABASE` is executed as catalog-driven cascade DDL:
  - enumerate owned tables in the target database
  - drop each table and its dependent catalog rows
  - finally delete the database row itself
- `axiomdb-network/src/mysql/handler.rs` now validates the requested database in
  both places where MySQL clients can select it:
  - handshake connect-with-db
  - `COM_INIT_DB`
- The wire invariant is now explicit:
  - unknown databases must fail before the server sends the final auth OK
  - otherwise clients can observe a spurious successful connect followed by a
    late catalog error
