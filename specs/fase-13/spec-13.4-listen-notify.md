# Spec: 13.4 — LISTEN / NOTIFY

Phase: 13 — Advanced PostgreSQL
Task: 13.4 LISTEN / NOTIFY
Status: closed

## Context

Phase `13.4` is the first SQL pub-sub slice in AxiomDB. The repo currently has
no parser or executor support for `LISTEN`, `NOTIFY`, or `UNLISTEN`; the only
existing pub-sub-like component is the internal catalog notifier used for DDL
cache invalidation, and that subsystem deliberately fires before commit with a
spurious-notification contract. That behavior is not suitable as-is for user
SQL notifications.

Because AxiomDB speaks the MySQL wire protocol today, the current request /
response handler loop is not a natural fit for PostgreSQL-style asynchronous
server-push packets to idle clients. The first delivery therefore needs a
bounded pull-based model that still gives real cross-session notifications
without changing the protocol model.

## Goal

Deliver a real SQL `LISTEN` / `NOTIFY` / `UNLISTEN` MVP using an in-process
broker with transaction-safe notification enqueueing and pull-based session
consumption.

## Non-goals

- Not implementing spontaneous server-push packets to idle clients.
- Not implementing PostgreSQL wire-compatible async notification frames.
- Not implementing `pg_notify(...)` function form in this subphase.
- Not implementing notification filtering / predicates; that belongs to `13.15`.
- Not persisting subscriptions or queued notifications across server restart.
- Not exposing notifications to other transports beyond the current MySQL-wire
  server process.

## Behavior

### Public SQL surface

Supported statements:

```sql
LISTEN channel_name;
UNLISTEN channel_name;
UNLISTEN *;
NOTIFY channel_name;
NOTIFY channel_name, 'payload';
SHOW NOTIFICATIONS;
```

`SHOW NOTIFICATIONS` is the pull surface for the MySQL-wire MVP. It returns the
notifications currently queued for the session and clears them after reading.

### Semantics

- `LISTEN channel` registers the current session as a subscriber to `channel`.
- `UNLISTEN channel` removes the current session's subscription for `channel`.
- `UNLISTEN *` removes all current session subscriptions.
- `NOTIFY channel[, payload]` enqueues one notification event for every
  currently subscribed session except the emitting session.
- `payload` is optional; omitted payload is the empty string.
- Channel names are case-insensitive for subscription matching and are stored in
  normalized lowercase form.
- `SHOW NOTIFICATIONS` returns all queued notifications for the current session
  in FIFO order, then drains the queue.
- A queued notification row has exactly these columns:
  - `channel` (`TEXT`, not null)
  - `payload` (`TEXT`, not null)
- Notifications emitted inside a transaction are delivered only if the
  transaction commits successfully.
- Notifications emitted inside a transaction that rolls back are discarded.
- `LISTEN` / `UNLISTEN` affect connection-local session state immediately and
  are not rolled back with the current transaction.
- `COM_RESET_CONNECTION`, `COM_CHANGE_USER`, and disconnect drop all
  subscriptions and queued notifications for that session.

### Error cases

| Input | Expected error | Message shape |
|-------|----------------|---------------|
| `LISTEN` with missing channel | `DbError::ParseError` | mentions channel |
| `NOTIFY` with non-string payload expr | `DbError::InvalidValue` | mentions payload |
| `NOTIFY` with channel longer than identifier limit | `DbError::InvalidValue` | mentions channel |
| `UNLISTEN` missing target | `DbError::ParseError` | mentions target |

## Edge cases

- [x] Duplicate `LISTEN channel` is idempotent.
- [x] `UNLISTEN channel` on a channel not currently subscribed is a no-op.
- [x] `UNLISTEN *` on a session with no subscriptions is a no-op.
- [x] `SHOW NOTIFICATIONS` on an empty queue returns zero rows.
- [x] Multiple notifications on the same channel preserve FIFO order.
- [x] Two listening sessions both receive one `NOTIFY`.
- [x] Emitting session does not receive its own notification in this MVP.
- [x] `NOTIFY` inside a rolled-back transaction does not reach listeners.
- [x] `COM_RESET_CONNECTION` / connection reset clears subscriptions and queue.

## Runtime format

No on-disk format is introduced in `13.4`.

In-memory broker contract:

- shared process-local broker keyed by normalized channel name
- each connection/session owns:
  - a stable session id
  - a set of subscribed channels
  - a FIFO queue of pending notifications

Compatibility rule: notifications are ephemeral and process-local only.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `LISTEN` / `UNLISTEN` single channel | O(1) average | no visible slowdown in tests |
| `NOTIFY` to N listeners | O(N) fan-out | acceptable for MVP |
| `SHOW NOTIFICATIONS` drain | O(k) queued rows | acceptable for MVP |

## Dependencies

- Depends on: parser/AST support in `axiomdb-sql`
- Depends on: per-session state in `SessionContext`
- Depends on: shared process state in `crates/axiomdb-network/src/mysql/shared_db.rs`
- Blocks: truthful closeout of `13.4` in `docs/fase-13.md`

## Open questions

- [x] Use `SHOW NOTIFICATIONS` as the pull surface for the MySQL-wire MVP.
      Delivered as specified.
- [x] Keep `LISTEN` / `UNLISTEN` outside transaction rollback semantics.
      Delivered as specified because they are session-scoped control statements.
- [x] Skip delivery back to the emitting session in the MVP.
      Delivered as specified.

## Done criteria

- [x] `LISTEN`, `UNLISTEN`, `UNLISTEN *`, and `NOTIFY` parse and execute
- [x] shared in-process broker delivers notifications across connections
- [x] `NOTIFY` honors commit / rollback boundaries
- [x] `SHOW NOTIFICATIONS` drains queued notifications for the session
- [x] connection reset/cleanup clears subscriptions and pending notifications
- [x] dedicated SQL and network integration coverage exists
- [x] wire smoke includes a bounded `13.4` scenario
- [x] `docs/progreso.md`, `docs/fase-13.md`, and `memory/project_state.md`
      reflect the delivered scope
- [x] `cargo test -p axiomdb-sql` for touched tests passes
- [x] `cargo test -p axiomdb-network` for touched tests passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## References

- Design overview: `db.md`
- Progress tracker: `docs/progreso.md`
- Existing per-session state: `crates/axiomdb-sql/src/session.rs`
- Existing shared server state: `crates/axiomdb-network/src/mysql/shared_db.rs`
- Existing connection reset lifecycle: `crates/axiomdb-network/src/mysql/connection.rs`,
  `crates/axiomdb-network/src/mysql/handler.rs`
- Internal notifier contrast: `crates/axiomdb-catalog/src/notifier.rs`
