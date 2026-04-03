# Project State

## 2026-04-02

- Phase 39 subphases `39.2`, `39.3`, `39.4`, `39.5`, `39.6`, `39.7`, `39.8`, `39.9`, `39.10`, `39.11`, `39.12`, `39.13`, and `39.14` are closed in code, targeted validation,
  and docs.
- `axiomdb-catalog` / `axiomdb-sql` now expose the first SQL-visible clustered-table boundary:
  - `TableDef` now stores `root_page_id` plus `TableStorageLayout::{Heap, Clustered}`
  - legacy catalog rows without the trailing layout byte still decode as heap tables
  - `CatalogWriter::create_table_with_layout(...)` can bootstrap either a heap `Data` root or a clustered `ClusteredLeaf` root
  - `CREATE TABLE ... PRIMARY KEY ...` now creates clustered tables and persists logical PK index metadata on that same clustered root
  - `CREATE TABLE` without an explicit `PRIMARY KEY` stays on the heap path
  - heap-only runtime paths now fail explicitly on clustered tables with phase-scoped `NotImplemented` errors
  - `INSERT` on clustered tables now routes directly through the clustered tree in both ctx and non-ctx executor paths
  - clustered PK bytes are derived from logical primary-index metadata order, not raw table-column order
  - clustered `AUTO_INCREMENT` bootstraps from clustered rows, not heap scans
  - non-primary clustered indexes are now maintained through PK bookmarks instead of heap `RecordId` payloads
  - explicit-transaction clustered inserts bypass `SessionContext::pending_inserts` and stay rollback/savepoint-safe
- `axiomdb-storage` now has the first complete clustered-tree write path:
  - `PageType::ClusteredInternal = 6`
  - storage module `crates/axiomdb-storage/src/clustered_internal.rs`
  - storage module `crates/axiomdb-storage/src/clustered_tree.rs`
  - `clustered_tree::insert(storage, root_opt, key, row_header, row_data) -> Result<u64, DbError>`
- `axiomdb-storage` now also has the first clustered-tree read path:
  - `clustered_tree::lookup(storage, root_opt, key, snapshot) -> Result<Option<ClusteredRow>, DbError>`
  - `clustered_tree::range(storage, root_opt, from, to, snapshot) -> Result<ClusteredRangeIter<'_>, DbError>`
  - `clustered_tree::update_in_place(storage, root_opt, key, new_row_data, txn_id, snapshot) -> Result<bool, DbError>`
  - `clustered_tree::update_with_relocation(storage, root_opt, key, new_row_data, txn_id, snapshot) -> Result<Option<u64>, DbError>`
  - `clustered_tree::delete_mark(storage, root_opt, key, txn_id, snapshot) -> Result<bool, DbError>`
  - exact root-to-leaf descent over clustered internal pages
  - exact leaf search returning full logical row bytes reconstructed from the clustered leaf descriptor
  - bound-aware start-leaf descent for ordered range scans
  - `next_leaf` traversal with `StorageEngine::prefetch_hint(...)`
  - MVCC filtering via `RowHeader::is_visible(&TransactionSnapshot)`
  - same-leaf clustered row rewrite with `row_version` bump
  - same-leaf clustered delete-mark with old-snapshot visibility preserved inline
  - private structural delete + rebalance path for relocate-update fallback
- Clustered tree behavior now includes:
  - empty-tree root bootstrap into `ClusteredLeaf`
  - descent over `ClusteredInternal` via `find_child_idx()`
  - sorted clustered-row descriptor inserts into clustered leaves
  - one defragmentation retry before split
  - leaf split by cumulative cell byte volume
  - internal split by cumulative separator byte volume
  - root split into a new clustered internal page
  - local-prefix + overflow-page storage for large clustered rows
  - transparent logical-row reconstruction in clustered lookup and range scan
  - overflow-aware same-leaf update transitions (`inline -> overflow -> inline`)
  - physical delete freeing obsolete clustered overflow chains
- `axiomdb-wal` now has the first clustered WAL / rollback path:
  - `crates/axiomdb-wal/src/clustered.rs`
  - `ClusteredRowImage { root_pid, row_header, row_data }`
  - `EntryType::{ClusteredInsert, ClusteredDeleteMark, ClusteredUpdate}`
  - `TxnManager` clustered root tracking per `table_id`
  - rollback / savepoint restore by primary key and exact row image
  - rollback helpers `delete_physical_by_key(...)` and `restore_exact_row_image(...)`
- `axiomdb-wal` now also has the first clustered crash-recovery path:
  - `CrashRecovery` undoes in-progress clustered rows by PK + exact row image
  - recovery tracks current clustered roots per `table_id`
  - `TxnManager::open_with_recovery(...)` seeds clustered roots from recovery
  - `TxnManager::open(...)` rebuilds committed clustered roots from surviving WAL history
- The clustered rewrite remains storage-first:
  - current `axiomdb-index::BTree` still uses fixed-slot `InternalNodePage` / `LeafNodePage`
  - SQL DDL can now create clustered tables for explicit primary keys
  - SQL clustered `INSERT` now writes clustered rows
  - no SQL clustered `SELECT` path reads clustered rows yet
  - clustered range scan now exists internally, but no SQL path uses it yet
  - clustered same-leaf update now exists internally, but no SQL path uses it yet
  - clustered delete-mark now exists internally, but no SQL path uses it yet
- Current clustered lookup limitation:
  - if the current inline version is invisible to the snapshot, `lookup()` returns `None`
  - older-version reconstruction remains deferred beyond the new rollback-only clustered WAL path
- Current clustered update limitation:
  - `update_in_place()` remains same-leaf only in `39.6`, but same-leaf growth can now use overflow pages in `39.10`
  - `update_with_relocation()` now handles same-page failure by physical delete + reinsert + rebalance
  - old-version visibility after relocate-update still depends on future clustered version-chain work
  - clustered-first secondary bookmark maintenance now exists in `crates/axiomdb-sql/src/clustered_secondary.rs`
  - the SQL-visible heap executor, FK enforcement, and index-integrity rebuild path still use `RecordId`-based secondaries
  - parent separator repair still assumes the repaired separator fits on the current internal page
- Current clustered delete limitation:
  - delete-mark keeps the physical cell inline in `39.7`
  - `39.8` adds private structural rebalance helpers, but public clustered delete still does not purge dead rows
  - clustered purge after delete still depends on later phases
  - snapshot-safe purge still depends on later phases
- `39.1` is now effectively closed by `39.10`:
  - clustered leaves support both inline rows and overflow-backed rows
  - primary key bytes and `RowHeader` remain inline in the leaf
  - generic TOAST/compression/WAL/recovery still belong to later phases
- Current clustered WAL / recovery limitation:
  - rollback, `ROLLBACK TO SAVEPOINT`, and crash recovery are implemented for clustered rows
  - clustered roots are still reconstructed from surviving WAL history
  - checkpoint/rotation-safe clustered root persistence remains deferred
  - standalone clustered `CREATE INDEX` / `ANALYZE` / `VACUUM` remain explicitly deferred
  - reusing a delete-marked clustered PK during SQL `INSERT` rewrites only the current physical version; older-snapshot reconstruction of the superseded tombstone remains future MVCC work

## 2026-03-29

- `crates/axiomdb-sql/tests/integration_executor.rs` was split into targeted
  binaries so executor work can run in narrower loops:
  - `integration_executor`
  - `integration_executor_joins`
  - `integration_executor_query`
  - `integration_executor_ddl`
  - `integration_executor_ctx`
  - `integration_executor_ctx_group`
  - `integration_executor_ctx_limit`
  - `integration_executor_ctx_on_error`
  - `integration_executor_sql`
  - `integration_delete_apply`
  - `integration_insert_staging`
  - `integration_namespacing`
  - `integration_namespacing_cross_db`
  - `integration_namespacing_schema`
- Shared integration-test harness code now lives in
  `crates/axiomdb-sql/tests/common/mod.rs`.
- Current testing policy for local development:
  - run the smallest relevant test binary first
  - add only directly related binaries when shared helpers or adjacent paths changed
  - keep `cargo test -p axiomdb-sql --tests` as the crate-level gate
  - keep `cargo test --workspace` for phase/subphase close only
- Current next split candidates are empty for `axiomdb-sql/tests/`; the remaining large bins are still cohesive enough to keep as one binary for now:
  - `crates/axiomdb-sql/tests/integration_executor_query.rs`
  - `crates/axiomdb-sql/tests/integration_index_only.rs`

## 2026-03-26

- Phase 5 subphase `5.11b` is closed in code, tests, and docs.
- Phase 5 subphase `5.11c` is closed in code, tests, and docs.
- Phase 5 subphase `5.19` is closed in code, tests, and docs.
- Phase 5 subphase `5.19a` is closed in code, tests, and docs.
- Phase 5 subphase `5.19b` is closed in code, tests, and docs.
- Phase 5 subphase `5.20` is closed in code, tests, and docs.
- `COM_STMT_SEND_LONG_DATA` was already largely implemented in the network layer; the remaining work was closure:
  - wire smoke coverage
  - protocol-facing tests
  - tracker reconciliation
  - documentation alignment
- The SQL executor is now split under `crates/axiomdb-sql/src/executor/` with a stable `mod.rs` facade.
- The refactor was structural only: `execute`, `execute_with_ctx`, and `last_insert_id_value()` kept the same paths and behavior.
- The expression evaluator now lives under `crates/axiomdb-sql/src/eval/` with `mod.rs`
  preserving the old public API while internals are split across `context.rs`,
  `core.rs`, `ops.rs`, and `functions/`.
- `DELETE ... WHERE` and the old-key half of `UPDATE` now batch-delete exact encoded keys per index through `BTree::delete_many_in(...)`.
- The hot path moved from `N` root descents per statement to one ordered delete batch per affected index.
- UPDATE now has a stable-RID fast path:
  - same-slot heap rewrites preserve `(page_id, slot_id)` when the new row still fits
  - WAL records that branch as `EntryType::UpdateInPlace`
  - unchanged indexes are skipped only when RID stability makes that safe
- Latest 50K-row local benchmark snapshot:
  - `UPDATE ... WHERE active = TRUE`: `648K rows/s`
  - `DELETE WHERE id > 25000`: `1.13M rows/s`
- Validation workflow was tightened:
  - iterative development uses targeted crate tests plus related dependents only when the blast radius justifies it
  - `cargo test --workspace` remains mandatory, but only as the subphase/phase closing gate
  - `tools/wire-test.py` is part of the loop only for MySQL wire-visible changes
- Remaining notable Phase 5 items after this close:
  - `5.15` DSN parsing

## 2026-03-27

- Phase 22 subphase `22b.3a` is closed in code, tests, wire smoke, and docs.
- AxiomDB now has a persisted logical database catalog:
  - `axiom_databases` stores database definitions
  - `axiom_table_databases` maps tables to owning databases
  - legacy tables with no explicit mapping resolve to the default database `axiomdb`
- SQL surface added in this subphase:
  - `CREATE DATABASE`
  - `DROP DATABASE [IF EXISTS]`
  - `USE`
  - catalog-backed `SHOW DATABASES`
- Session resolution is now database-aware:
  - unqualified table names resolve inside the selected database when one is active
  - otherwise they resolve against the effective default database `axiomdb`
- `DROP DATABASE` now performs catalog-driven cascade deletion of owned tables and
  rejects dropping the currently selected database for the same connection.
- MySQL wire behavior now matches the catalog:
  - handshake `database=...` validates before auth success is finalized
  - `COM_INIT_DB` rejects unknown databases with `1049`
  - `SHOW DATABASES` is no longer hardcoded
- Phase 6 subphase `6.16` is closed in code, targeted tests, and docs.
- Phase 6 subphase `6.17` is closed in code, targeted tests, and docs.
- Phase 6 subphase `6.18` is closed in code, targeted tests, and docs.
- Phase 6 subphase `6.19` is closed in code, tests, and docs, with a documented
  remaining single-connection performance gap.
- The leader-based WAL fsync pipeline is live in the server path, but the closure benchmark still fails:
  - `local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb`
  - result: `224 ops/s`
  - spec target: `>= 5K ops/s`
- The blocker is architectural, not a missing test:
  - the MySQL wire handler still waits for durability before sending the OK packet
  - with a single request/response client, the next statement cannot arrive while the fsync is in flight
  - the MariaDB-style leader/follower piggyback therefore helps overlapping commits, but not the sequential single-connection benchmark the spec targeted
- Phase 6 subphase `6.15` is closed in code, targeted tests, and docs.
- Phase 5 subphase `5.15` is closed in code, tests, and docs.
- Phase 5 subphase `5.21` is closed in code, tests, and docs.
- Explicit transactions now stage consecutive `INSERT ... VALUES` rows in
  `SessionContext::pending_inserts` and flush them together at `COMMIT` or the
  next barrier statement.
- The staging path performs enqueue-time logical validation:
  - AUTO_INCREMENT assignment
  - CHECK constraints
  - FK child validation
  - duplicate UNIQUE / PRIMARY KEY rejection against committed state and
    in-batch `unique_seen`
- A real bug surfaced during wire closure: table-switch flushes originally
  happened after the next statement savepoint, which let a later duplicate-key
  error roll back earlier staged rows. The final fix moved that flush decision
  to the statement boundary.
- Latest local benchmark snapshot for the targeted workload:
  - `insert` (`50K` one-row INSERTs in `1` explicit txn, release server): `23.9K rows/s`
  - MariaDB 12.1 on the same run: `28.0K rows/s`
  - MySQL 8.0 on the same run: `26.7K rows/s`
- A shared DSN parser now lives in `axiomdb-core/src/dsn.rs`.
- `axiomdb-server` consumes it through `AXIOMDB_URL`, while preserving
  `AXIOMDB_DATA` / `AXIOMDB_PORT` fallback behavior.
- `axiomdb-embedded` now exposes:
  - `Db::open_dsn`
  - `AsyncDb::open_dsn`
  - `axiomdb_open_dsn`
- Embedded mode accepts only local-path DSNs in `5.15`; remote wire-endpoint
  DSNs are rejected explicitly after parse.
- Phase 5 is now complete.
- `6.15` adds startup index integrity verification:
  - compare each catalog-visible index against heap-visible rows after WAL recovery
  - rebuild readable divergence automatically from heap contents
  - fail open with `DbError::IndexIntegrityFailure` if the tree cannot be read safely
- The verifier runs in both:
  - `axiomdb-network/src/mysql/database.rs`
  - `axiomdb-embedded/src/lib.rs`
- `6.16` removes the last planner-side blind spot for primary-key SELECT:
  - PRIMARY KEY indexes are now eligible in `plan_select` / `plan_select_ctx`
  - `WHERE pk = literal` bypasses the small-table / NDV scan bias and emits `IndexLookup`
  - PK range predicates reuse the existing `IndexRange` path
  - non-binary session collation still rejects text-key PK access
- Targeted `select_pk` local bench after `6.16`:
  - MariaDB `12.7K lookups/s`
  - MySQL `13.4K lookups/s`
  - AxiomDB `11.1K lookups/s`
- The old debt was planner-side full scan; the remaining gap is now smaller and
  sits after planning in row/materialization/wire overhead.
- `6.17` removes planner-side full-scan discovery for indexed UPDATE:
  - `plan_update_candidates` / `_ctx` now select PK or secondary index access
  - `execute_update[_ctx]` materializes candidate RIDs before mutation and
    rechecks the full `WHERE`
  - the existing `5.20` stable-RID / fallback write path remains unchanged
- Targeted `update_range` local bench after `6.17`:
  - MariaDB `618K rows/s`
  - MySQL `291K rows/s`
  - AxiomDB `85.2K rows/s`
- The remaining indexed UPDATE gap is no longer candidate discovery; it sits in
  heap rewrite / index maintenance after candidates are found.
- `6.18` removes the remaining immediate multi-row INSERT fallback on indexed
  tables:
  - shared apply helpers now serve both staged transactional INSERT flushes and
    immediate `INSERT ... VALUES (...), (... )`
  - PRIMARY KEY / secondary indexes no longer force per-row maintenance for the
    statement
  - immediate multi-row INSERT keeps strict same-statement UNIQUE checking and
    therefore does not reuse the staged `committed_empty` shortcut
- Targeted `insert_multi_values` local bench after `6.18` on the PK benchmark schema:
  - MariaDB `160,581 rows/s`
  - MySQL `259,854 rows/s`
  - AxiomDB `321,002 rows/s`
