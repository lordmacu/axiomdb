# Phase 5 — MySQL Wire Protocol + executor/runtime cleanup

## Subfases completed in this session: 5.11c, 5.19, 5.19a

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
- Existing callers in `lib.rs`, `eval.rs`, and integration tests keep the same import paths.
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

## Validation

- `cargo test -p axiomdb-network`
- `cargo clippy -p axiomdb-network -p axiomdb-server --tests -- -D warnings`
- `cargo test -p axiomdb-sql`
- `cargo clippy -p axiomdb-sql --lib -- -D warnings`
- `cargo clippy -p axiomdb-sql --tests -- -D warnings`
- `cargo test -p axiomdb-index --test integration_btree`
- `cargo test -p axiomdb-sql --test integration_executor`
- `cargo fmt --check`
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- `python3 tools/wire-test.py` → `212/212 passed`
- `python3 benches/comparison/local_bench.py --scenario delete_where --rows 5000 --table`
- `python3 benches/comparison/local_bench.py --scenario update --rows 5000 --table`

## Follow-up subfases still open in Phase 5

- `5.15` — DSN parsing
