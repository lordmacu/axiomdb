# Architecture Notes

## 2026-03-26 — Prepared long data over MySQL wire

- `COM_STMT_SEND_LONG_DATA` is handled entirely in `axiomdb-network/src/mysql/handler.rs`.
- The command does not take the `Database` mutex and never touches the SQL engine directly.
- Pending bytes are owned by `PreparedStatement` in `axiomdb-network/src/mysql/session.rs`.
- Chunks are stored as raw bytes and decoded only in `parse_execute_packet(...)`.
- Long data has precedence over both the inline execute payload and the null bitmap.
- `COM_STMT_RESET` clears stmt-local long-data state but keeps the prepared statement, cached analyzed plan, and type metadata.
- `SHOW STATUS` exposes `Com_stmt_send_long_data` in both session and global scope.
