# Phase 5 — MySQL Wire Protocol + executor/runtime cleanup

## Subfases completed in this session: 5.11c, 5.19, 5.19a, 5.19b, 5.20, 5.21

## What was built

### 5.11c — Explicit connection state machine

AxiomDB's MySQL wire server now has a transport lifecycle that is explicit in
code instead of being implicit inside the command loop. The new lifecycle lives
outside `ConnectionState`, so socket phase, keepalive, and timeout policy are no
longer mixed with SQL session variables or prepared-statement state.

What changed:

- `crates/axiomdb-network/src/mysql/lifecycle.rs` now owns:
  - `ConnectionPhase`
  - `ConnectionLifecycle`
  - timeout selection per phase
  - timeout-wrapped packet reads and writes
  - socket configuration (`TCP_NODELAY` + `SO_KEEPALIVE`)
- `handler.rs` now transitions explicitly through:
  - `CONNECTED`
  - `AUTH`
  - `IDLE`
  - `EXECUTING`
  - `CLOSING`
- `ConnectionState` validates and exposes typed getters for:
  - `wait_timeout`
  - `interactive_timeout`
  - `net_read_timeout`
  - `net_write_timeout`
- `CLIENT_INTERACTIVE` from the handshake now matters operationally:
  - idle timeout uses `interactive_timeout` for interactive clients
  - idle timeout uses `wait_timeout` otherwise
- `COM_RESET_CONNECTION` still recreates `ConnectionState`, but preserves the
  connection's transport classification and returns the lifecycle to `IDLE`.

Why this matters:

- auth timeout, idle timeout, and write timeout are now deterministic and tied
  to explicit connection phases
- dead peers are detected more reliably because accepted sockets enable
  keepalive, instead of depending only on the next application-level read
- transport state is now isolated from SQL session state, which makes later
  wire/runtime work safer

### 5.19a — Executor decomposition

AxiomDB's SQL executor is no longer a single giant source file. The old
`crates/axiomdb-sql/src/executor.rs` monolith was replaced by a directory module
under `crates/axiomdb-sql/src/executor/` with a stable facade in `mod.rs`.

What changed:

- `mod.rs` keeps the public entrypoints stable:
  - `execute(...)`
  - `execute_with_ctx(...)`
  - `last_insert_id_value()`
- Statement logic is now split by responsibility:
  - `shared.rs`
  - `select.rs`
  - `joins.rs`
  - `aggregate.rs`
  - `insert.rs`
  - `update.rs`
  - `delete.rs`
  - `bulk_empty.rs`
  - `ddl.rs`
- Existing callers in `lib.rs`, `crate::eval`, and integration tests keep the same import paths.
- This subphase is structural only: it changes source layout, not SQL semantics.

Why this matters:

- later DML work such as `5.19` no longer has to edit a 7K-line file
- SELECT, GROUP BY, JOIN, DDL, and DML paths can evolve independently
- executor-local helpers are now easier to find and reason about without widening public API

### 5.19 — B+Tree batch delete

AxiomDB no longer removes index entries one row at a time during `DELETE ... WHERE`
and the old-key half of `UPDATE`. The executor now stages exact encoded keys per
index, sorts them once, and deletes them with a dedicated `delete_many_in(...)`
pass per affected tree.

What changed:

- `crates/axiomdb-index/src/tree.rs` now exposes `BTree::delete_many_in(...)`
  plus page-local batch-delete recursion for leaves and internal nodes
- `crates/axiomdb-sql/src/index_maintenance.rs` now owns
  `collect_delete_keys_by_index(...)` and `delete_many_from_indexes(...)`
- `delete.rs` batch-deletes old index keys once per index after heap deletion
- `update.rs` batch-deletes old PRIMARY KEY and secondary-index keys before
  reinserting new ones, which keeps index state correct when heap `RecordId`s
  change

Why this matters:

- `DELETE WHERE` no longer pays one root descent per deleted row and per index
- index roots are persisted once per affected index instead of once per row
- the direct tree primitive is now explicit and testable outside executor loops

Measured with the local four-engine benchmark (`5K` rows, wire protocol):

- `DELETE WHERE id > 2500` → **396K rows/s**
- `UPDATE ... WHERE active = TRUE` → **52.9K rows/s**

Compared to the `4.6K rows/s` pre-`5.19` DELETE-WHERE baseline that opened the
subphase, the batched delete path removes the old O(N log N) index-delete
behavior as the dominant bottleneck.

### 5.19b — Eval decomposition

AxiomDB's expression evaluator is no longer a single monolithic source file.
The old `crates/axiomdb-sql/src/eval.rs` implementation now lives under
`crates/axiomdb-sql/src/eval/` with a stable facade in `mod.rs`.

What changed:

- `mod.rs` preserves the existing `crate::eval` surface:
  - `eval(...)`
  - `eval_with(...)`
  - `eval_in_session(...)`
  - `eval_with_in_session(...)`
  - `is_truthy(...)`
  - `like_match(...)`
  - `ClosureRunner`, `CollationGuard`, `NoSubquery`, `SubqueryRunner`
- evaluator internals are now split by responsibility:
  - `context.rs`
  - `core.rs`
  - `ops.rs`
  - `functions/` (`system`, `nulls`, `numeric`, `string`, `datetime`, `binary`, `uuid`)
  - `tests.rs`
- `current_eval_collation()` remains exported as `pub(crate)` so executor modules
  that depend on evaluator collation state kept the same import path.

Why this matters:

- future function work no longer has to edit one multi-thousand-line file
- scalar built-ins, value operations, collation context, and recursive traversal
  are now separable review units
- the split stayed structural only: evaluator semantics did not change

### 5.20 — Stable-RID UPDATE fast path

AxiomDB no longer forces every `UPDATE` row through heap delete+insert. When the
new encoded row fits in the same heap slot, the engine now rewrites the tuple in
place, keeps the `RecordId` stable, and skips index maintenance for indexes whose
logical key membership is unchanged.

What changed:

- `crates/axiomdb-storage/src/heap.rs` now exposes a same-slot rewrite primitive
  that updates the tuple in place and returns the old tuple image for undo.
- `crates/axiomdb-storage/src/heap_chain.rs` now batches same-page stable-RID
  rewrites so multiple rows on one page cost one read + one write for that page.
- `crates/axiomdb-wal/src/entry.rs`, `txn.rs`, and `recovery.rs` now handle
  `EntryType::UpdateInPlace`, with rollback/savepoint/crash recovery restoring the
  old tuple bytes into the same slot.
- `TableEngine::update_rows_preserve_rid[_with_ctx](...)` in
  `crates/axiomdb-sql/src/table.rs` chooses stable-RID rewrite when the row fits,
  and falls back to the old delete+insert path when it does not.
- `crates/axiomdb-sql/src/index_maintenance.rs` now decides index impact from the
  actual `(old_rid, new_rid)` pair plus logical key/predicate membership:
  unchanged indexes are skipped only when the RID stayed stable.
- `executor/update.rs` now batches stable-RID rows and only maintains the indexes
  truly affected by each row.

Why this matters:

- UPDATE on PK-only tables no longer pays unnecessary PK delete+insert work when
  only non-indexed columns change
- the old tracker idea "skip indexes if the SET list doesn't touch them" became
  correct only after row identity stopped changing for the fast path
- rollback and crash recovery remain correct because the same-slot path has its
  own WAL/undo contract instead of pretending to be delete+insert

Measured with the local four-engine benchmark (`50K` rows, wire protocol):

- `UPDATE ... WHERE active = TRUE` → **647K rows/s**
- `DELETE WHERE id > 25000` → **1.13M rows/s**

This replaces the old Phase 5 picture where `UPDATE` was still stuck at
**52.9K rows/s** after `5.19`. The stable-RID path turns UPDATE from the
remaining DML hot spot into a MariaDB-class path on the benchmark schema.

### 5.21 — Transactional INSERT staging

AxiomDB now stages consecutive `INSERT ... VALUES` statements inside an explicit
transaction and flushes them in one grouped heap/WAL/index pass only when it
hits a barrier.

What changed:

- `crates/axiomdb-sql/src/session.rs` now stores `PendingInsertBatch` in
  `SessionContext`, with:
  - target table metadata
  - staged row values
  - compiled partial-index predicates
  - in-batch UNIQUE tracking via `unique_seen`
- `executor/insert.rs` now evaluates expressions, fills defaults, assigns
  AUTO_INCREMENT values, runs CHECK/FK validation, and rejects duplicate
  UNIQUE / PRIMARY KEY keys at enqueue time instead of mutating heap/WAL
  immediately
- `executor/staging.rs` flushes through:
  - `TableEngine::insert_rows_batch_with_ctx(...)`
  - `batch_insert_into_indexes(...)`
  - one `CatalogWriter::update_index_root(...)` per changed index root
- `executor/mod.rs` now flushes staged rows before:
  - non-INSERT barrier statements
  - table switches
  - ineligible INSERT shapes
  - the next statement savepoint when the batch cannot continue
- `ROLLBACK` discards unflushed rows without touching heap/WAL, while `COMMIT`
  flushes any remaining staged rows first

Why this matters:

- many single-row INSERT statements inside `BEGIN ... COMMIT` no longer pay
  heap insert + WAL append + per-row index root persistence at statement time
- `SELECT` inside the same transaction still sees prior INSERTs because it acts
  as a flush barrier
- savepoint semantics remain correct across table switches: if a later INSERT
  fails, previously flushed staged rows survive the statement rollback

Measured with the local four-engine benchmark (`50K` rows, release server,
`python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`):

- MariaDB 12.1: **28.0K rows/s**
- MySQL 8.0: **26.7K rows/s**
- AxiomDB: **23.9K rows/s**

## Validation

- `cargo test -p axiomdb-network`
- `cargo clippy -p axiomdb-network -p axiomdb-server --tests -- -D warnings`
- `cargo test -p axiomdb-storage --lib`
- `cargo test -p axiomdb-wal --lib`
- `cargo test -p axiomdb-sql`
- `cargo clippy -p axiomdb-sql --lib -- -D warnings`
- `cargo clippy -p axiomdb-sql --tests -- -D warnings`
- `cargo test -p axiomdb-sql --test integration_eval`
- `cargo test -p axiomdb-sql --test integration_date_functions`
- `cargo test -p axiomdb-sql --test integration_subqueries`
- `cargo test -p axiomdb-index --test integration_btree`
- `cargo test -p axiomdb-sql --test integration_executor`
- `cargo test -p axiomdb-sql --test integration_executor test_5_21_ -- --nocapture`
- `cargo test -p axiomdb-sql --test integration_indexes test_5_21_ -- --nocapture`
- `cargo test -p axiomdb-sql --test integration_autocommit savepoint_ -- --nocapture`
- `cargo fmt --check`
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- `python3 tools/wire-test.py` → `226/226 passed`
- `python3 benches/comparison/local_bench.py --scenario delete_where --rows 5000 --table`
- `python3 benches/comparison/local_bench.py --scenario update --rows 5000 --table`
- `python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table`
- `python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`

## Follow-up subfases still open in Phase 5

- `5.15` — DSN parsing
