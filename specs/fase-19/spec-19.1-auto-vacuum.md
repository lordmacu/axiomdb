# Spec: 19.1 — Auto-vacuum (inline at commit)

Phase: 19 — Maintenance + observability
Task: Auto-vacuum
Status: approved

## Context

AxiomDB's MVCC leaves dead tuples behind on DELETE/UPDATE. Manual `VACUUM`
reclaims their space and removes their index entries (Phase 7.11 / 39.18).
For the embedded library to be production-ready, dead tuples must be
collected without operator intervention — otherwise long-lived embedded
DBs (a Python process, a desktop app) grow forever.

Phase 19.1 introduces auto-vacuum **inline at commit**: at the end of a
successful autocommit query, the embedded `Db::run_inner` checks each
table that received writes during this session, and runs `vacuum_one_table`
on those whose accumulated change count crosses a threshold.

## Goal

After every successful autocommit query in the embedded API, vacuum any
table whose per-session change count is ≥ `auto_vacuum_threshold`. No
background task, no API change, no new tokio runtime requirement.

## Non-goals

- Background tokio task — deferred to Phase 19.1b (needs Arc<Mutex<Db>>
  refactor; not required for embedded-first ship).
- Per-table threshold configuration via SQL (`ALTER TABLE ... SET
  (autovacuum_vacuum_threshold = N)`) — deferred. v1 uses a single
  global threshold per session.
- Auto-analyze (refreshing column stats) — Phase 19.x.
- VACUUM FULL semantics (rewriting heap pages compactly) — deferred to
  Phase 19.2 (VACUUM CONCURRENTLY).
- Wire-server auto-vacuum — the wire-server path has different
  concurrency requirements (multiple sessions writing); v1 of 19.1 is
  embedded-only.

## Behavior

### Trigger

After `Db::run_inner` returns a successful `QueryResult` for an
autocommit query (i.e. `ctx.autocommit && !ctx.in_explicit_txn`),
iterate `ctx.stats.changes` and identify tables where
`changes_for(table_id) >= ctx.auto_vacuum_threshold`. For each such
table, open a new short-lived transaction, run `vacuum_one_table`,
commit, and reset `stats.changes_for(table_id)` to 0.

Skip when:
- The session is in an explicit transaction (`BEGIN ... ROLLBACK/COMMIT`
  in progress) — would cross transaction boundaries.
- The session is degraded (`Db.degraded == true`) — read-only mode.
- `ctx.auto_vacuum_enabled == false` — user disabled it.

### Configuration

- `SessionContext.auto_vacuum_enabled: bool` — default `true`.
- `SessionContext.auto_vacuum_threshold: u64` — default `1000` changes.

Configurable via:
- `SET autovacuum = ON|OFF` (default `ON`)
- `SET autovacuum_vacuum_threshold = N` (default `1000`)

### Error handling

A vacuum failure does **not** propagate to the user. The original
query result has already succeeded; the user sees their result.
Vacuum errors are logged via `eprintln!` (acceptable for v1 — proper
logging is Phase 19.5+) and the change counter is left alone so the
next commit tries again.

### Reset semantics

After a successful per-table vacuum, the change counter for that
table is reset to 0. The next 1000 changes will trigger the next
vacuum.

## Edge cases

- [ ] Empty change counter — no vacuum runs, no overhead beyond the
      HashMap iteration.
- [ ] Threshold = 0 — vacuum after every commit (stress mode for tests).
      Should still work (each commit triggers vacuum on every dirty table).
- [ ] In explicit txn — no auto-vacuum (skipped by the
      `ctx.in_explicit_txn` guard).
- [ ] Vacuum-induced txn fails — error logged, counter untouched, user's
      original query result is still returned.
- [ ] Concurrent DDL during vacuum — handled by the existing vacuum
      logic (snapshot consistency).
- [ ] Auto-vacuum on a table where `axiom_*` (system catalogs) — exclude
      system tables (table_id < `USER_TABLE_ID_BASE`).

## On-disk format

No new on-disk format. Vacuum already uses the existing WAL + storage
protocol.

## Performance budget

- Per-commit overhead when nothing is due: `~1µs` (HashMap iteration
  over `stats.changes`, no allocation).
- When vacuum is due: same cost as a manual `VACUUM <table>` (varies
  by table size). The threshold (1000 changes default) bounds how
  often this fires.

## Dependencies

- Depends on: Phase 7.11 (VACUUM implementation) — done.
- Depends on: `StaleStatsTracker.changes_for` (added in Attack 17b) —
  done.
- Blocks: nothing.

## Open questions

None.

## Done criteria

- [ ] `SessionContext` gains `auto_vacuum_enabled: bool` (default true)
      and `auto_vacuum_threshold: u64` (default 1000).
- [ ] New `pub fn auto_vacuum_if_needed(...)` in `axiomdb_sql::vacuum`
      iterates `stats.changes`, vacuums table by table, resets counters.
- [ ] `Db::run_inner` calls `auto_vacuum_if_needed` after a successful
      autocommit query.
- [ ] `SET autovacuum = ON|OFF` parses and updates the session field.
- [ ] `SET autovacuum_vacuum_threshold = N` parses and updates.
- [ ] 5+ integration tests:
      - large DELETE triggers auto-vacuum
      - threshold=0 stress mode
      - autovacuum=OFF disables it
      - explicit BEGIN..COMMIT does NOT auto-vacuum inside the txn
      - vacuum failure doesn't break the user's query
- [ ] cargo test on Lima passes workspace.
- [ ] cargo clippy clean.

## References

- Phase 7.11 spec: `axiomdb-sql/src/vacuum.rs` source comments.
- PostgreSQL autovacuum: `src/backend/postmaster/autovacuum.c`.
- SQLite incremental vacuum: `PRAGMA auto_vacuum`.
