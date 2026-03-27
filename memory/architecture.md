# Architecture Notes

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
