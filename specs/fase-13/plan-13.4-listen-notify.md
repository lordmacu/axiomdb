# Plan: 13.4 — LISTEN / NOTIFY

Phase: 13 — Advanced PostgreSQL
Task: 13.4 LISTEN / NOTIFY
Spec: specs/fase-13/spec-13.4-listen-notify.md
Status: completed

## Summary

This plan delivers `13.4` as a transaction-safe, pull-based pub-sub MVP over
the current MySQL wire architecture. The order is: first add parser/AST
support for the new statements, then introduce session-local subscription /
queue state and a shared broker in the MySQL server, then wire commit/rollback
aware delivery, and finally expose queued events through `SHOW NOTIFICATIONS`
plus dedicated SQL/network/wire coverage.

## Dependencies

Must be done first:
- [x] `spec-13.4-listen-notify.md` approved
- [x] `13.3` closeout committed

Blocks (until this plan is done):
- [ ] `13.15` filtered LISTEN / NOTIFY follow-up

## Affected files

New files:
- `specs/fase-13/spec-13.4-listen-notify.md` — task contract
- `specs/fase-13/plan-13.4-listen-notify.md` — execution plan
- likely `crates/axiomdb-network/tests/integration_listen_notify.rs` — network-level acceptance

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — tokens for `LISTEN`, `UNLISTEN`, `NOTIFY`
- `crates/axiomdb-sql/src/ast.rs` — new statement variants
- parser files — new statement parsing
- `crates/axiomdb-sql/src/session.rs` — session queue/subscription state and pending transactional notifications
- `crates/axiomdb-network/src/mysql/shared_db.rs` — shared broker and `SHOW NOTIFICATIONS`
- `crates/axiomdb-network/src/mysql/connection.rs` / `handler.rs` — reset/cleanup hooks
- `tools/wire-test.py` — bounded `13.4` smoke
- closeout docs/memory files

## Step 1 — Parse the SQL surface

**Goal:** represent `LISTEN`, `UNLISTEN`, and `NOTIFY` explicitly in the AST.
**Files:** SQL lexer/parser/AST files.
**Approach:** TDD — parser tests for valid and malformed forms before execution.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Session and broker state

**Goal:** add the minimal runtime state needed for subscriptions and queued notifications.
**Files:** `session.rs`, MySQL shared DB/runtime files.
**Approach:** keep subscription membership and notification queue structures explicit
and resettable; avoid persistence and avoid coupling to catalog notifier internals.

### Verification

```bash
cargo test -p axiomdb-network --test integration_connection_lifecycle
```

## Step 3 — Commit-safe delivery semantics

**Goal:** ensure `NOTIFY` fans out only on successful commit and is discarded on rollback.
**Files:** execution path for the new statements, transaction boundary hooks.
**Approach:** queue pending notifications in the session while a transaction is open;
publish immediately in autocommit / no-txn mode; flush on commit and drop on rollback.

### Verification

```bash
cargo test -p axiomdb-network --test integration_listen_notify
```

## Step 4 — Pull surface and wire acceptance

**Goal:** expose queued notifications through `SHOW NOTIFICATIONS` and lock the behavior down over the wire.
**Files:** `shared_db.rs`, `tools/wire-test.py`, network integration tests.
**Approach:** reuse the existing “special read query” pattern for `SHOW WARNINGS`
instead of inventing a new executor-visible catalog object.

### Verification

```bash
python3 tools/wire-test.py
```

## Step 5 — Close docs and memory

**Goal:** document the bounded MVP honestly.
**Files:** `docs/progreso.md`, `docs/fase-13.md`, `memory/project_state.md`,
`memory/architecture.md`, `memory/lessons.md`

### Verification against spec

- [x] `LISTEN`, `UNLISTEN`, `UNLISTEN *`, and `NOTIFY` parse and execute
- [x] shared broker delivers across connections
- [x] commit / rollback semantics are correct
- [x] `SHOW NOTIFICATIONS` drains queued rows
- [x] reset/cleanup clears subscriptions and queue
- [x] dedicated SQL and network coverage exists
- [x] wire smoke is green
- [x] docs/memory reflect the bounded pull-based scope
- [x] `cargo test -p axiomdb-sql` for touched tests passes
- [x] `cargo test -p axiomdb-network` for touched tests passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| trying to emulate true server-push over MySQL wire blows up scope | high | keep the MVP strictly pull-based via `SHOW NOTIFICATIONS` |
| commit/rollback hooks miss one transaction exit path | medium | cover explicit commit, rollback, autocommit, reset, and disconnect in tests |
| session-scoped subscriptions accidentally become transaction-scoped | medium | store them separately from pending transactional notification events |

## Rollback plan

If commit-safe delivery becomes too invasive:

1. keep spec/plan in draft
2. do not mark `13.4` closed
3. revise the MVP around a simpler scope before implementation continues

## Estimated effort

Total: high
Per step:
- Step 1: 45-60 min
- Step 2: 1-2 h
- Step 3: 1-2 h
- Step 4: 45-90 min
- Step 5: 20-30 min
