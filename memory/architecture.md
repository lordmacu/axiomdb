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
