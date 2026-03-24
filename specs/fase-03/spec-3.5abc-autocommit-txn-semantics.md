# Spec: 3.5a + 3.5b + 3.5c — Autocommit mode + Implicit transactions + Statement-level rollback

## What to build (not how)

Three related behaviors that together make AxiomDB fully compatible with MySQL ORMs
(SQLAlchemy, Django ORM, Hibernate, ActiveRecord) which all use `SET autocommit=0` by default.

### 3.5a — SET autocommit=0 must be respected

When a client sends `SET autocommit=0`, subsequent DML statements must NOT be
auto-committed. The flag is already stored in `ConnectionState.autocommit` but is
never read by the executor. This spec requires that it is read and honored.

### 3.5b — Implicit transaction start in autocommit=0 mode

When `autocommit=false` and no explicit transaction is active, the first DML statement
must implicitly start a transaction (implicit `BEGIN`). The transaction stays open until
the client sends an explicit `COMMIT` or `ROLLBACK`. DDL statements also trigger an
implicit commit of any open transaction (MySQL semantics: DDL causes implicit COMMIT).

### 3.5c — Statement-level rollback on error inside explicit transaction

When a statement fails inside an explicit transaction (or implicit autocommit=0
transaction), only that statement's changes are rolled back — not the whole transaction.
The transaction remains active. The client can continue sending statements or issue
`ROLLBACK` to abort everything.

---

## Inputs / Outputs

### 3.5a

- Input: `SET autocommit=0` (or `SET autocommit=1`) via COM_QUERY
- Output: `ConnectionState.autocommit` is set; all subsequent statements respect it
- Error: none (SET autocommit is always valid)

### 3.5b

- Input: any DML statement (INSERT, UPDATE, DELETE, SELECT FOR UPDATE) while
  `autocommit=false` and no active transaction
- Output: transaction implicitly started before the statement executes; remains open
  after the statement succeeds
- Input: DDL statement (CREATE, DROP, ALTER, TRUNCATE) while `autocommit=false`
  with an open transaction
- Output: open transaction is implicitly committed, DDL executes in its own autocommit
  transaction, no transaction remains open after
- Input: SELECT (read-only) while `autocommit=false` and no active transaction
- Output: statement executes with a snapshot but does NOT start a transaction

### 3.5c

- Input: any statement that errors while inside an explicit transaction
- Output: statement's own changes are rolled back; transaction remains active;
  ERR packet sent to client; subsequent statements can execute
- Error triggers: constraint violation, type mismatch, row not found, any `DbError`
  returned by the executor's dispatch

---

## Use cases

### Happy path — ORM flow (3.5a + 3.5b)

```sql
SET autocommit=0;           -- flag set, no txn yet
INSERT INTO users ...;      -- implicit BEGIN + INSERT, txn open
INSERT INTO orders ...;     -- same txn, no BEGIN
COMMIT;                     -- explicit COMMIT, txn closed
INSERT INTO users ...;      -- new implicit BEGIN
ROLLBACK;                   -- rollback that txn
```

### DDL inside autocommit=0 (3.5b)

```sql
SET autocommit=0;
INSERT INTO t VALUES (1);   -- implicit BEGIN
CREATE INDEX ...;           -- implicit COMMIT of open txn, then DDL in its own txn
INSERT INTO t VALUES (2);   -- new implicit BEGIN
```

### Statement error, transaction survives (3.5c)

```sql
BEGIN;
INSERT INTO users VALUES (1, 'alice');  -- ok
INSERT INTO users VALUES (1, 'bob');    -- ERROR: duplicate PK
                                        -- only this INSERT rolled back
INSERT INTO users VALUES (2, 'bob');    -- ok, txn still active
COMMIT;                                 -- commits rows 1 and 2 (not the failed one)
```

### Error in autocommit=0 implicit txn (3.5b + 3.5c)

```sql
SET autocommit=0;
INSERT INTO t VALUES (1);   -- implicit BEGIN + INSERT, ok
INSERT INTO t VALUES (1);   -- ERROR: duplicate PK — INSERT rolled back, txn still open
INSERT INTO t VALUES (2);   -- ok, same txn
COMMIT;                     -- commits rows 1 and 2
```

### Reconnect resets autocommit (3.5a)

New connection always starts with `autocommit=true`. The client must re-send
`SET autocommit=0` after reconnect.

---

## Acceptance criteria

- [ ] `SET autocommit=0` followed by DML does not auto-commit the statement
- [ ] `SET autocommit=1` restores auto-commit behavior
- [ ] First DML in `autocommit=0` mode starts an implicit transaction
- [ ] SELECT (read-only) in `autocommit=0` does NOT start a transaction
- [ ] DDL in `autocommit=0` with open txn: commits the txn, then executes DDL
- [ ] After error in explicit txn: transaction remains active (not rolled back)
- [ ] After error in explicit txn: failed statement's partial writes are undone
- [ ] After error in explicit txn: subsequent statements execute normally
- [ ] `@@autocommit` variable returns correct value via `SELECT @@autocommit`
- [ ] `SHOW VARIABLES LIKE 'autocommit'` returns correct value
- [ ] New connection always starts with `autocommit=1`
- [ ] pymysql with `autocommit=False` works correctly end-to-end
- [ ] SQLAlchemy default session (uses `SET autocommit=0`) works correctly
- [ ] Integration test: ORM-style flow (SET + implicit txn + COMMIT/ROLLBACK)
- [ ] Integration test: error in txn → statement rollback → txn continues → final COMMIT

---

## Out of scope

- `SAVEPOINT` / `RELEASE SAVEPOINT` / `ROLLBACK TO SAVEPOINT` (explicit savepoints — Phase 7)
- `SET TRANSACTION ISOLATION LEVEL` (Phase 7)
- `autocommit` per-schema or per-user (never planned)
- Sub-statement errors in stored procedures (Phase 18)
- `ROLLBACK TO SAVEPOINT` used by ORMs for nested transactions (future)

---

## Dependencies

- `ConnectionState.autocommit` already set by `apply_set()` in `session.rs` ✅
- `TxnManager.begin/commit/rollback` exist in `txn.rs` ✅
- `execute_with_ctx()` in `executor.rs` is the integration point ✅
- `SessionContext` flows from wire handler → database → executor ✅
- Undo log (`undo_ops: Vec<UndoOp>`) in `TxnManager` is the savepoint mechanism for 3.5c ✅

---

## ⚠️ DEFERRED

- Per-statement savepoint for statements with large partial writes (e.g., batch INSERT
  of 10K rows that fails at row 5000) requires profiling to confirm the undo replay cost.
  For now, savepoint captures undo log position at statement start and replays on error.
  If undo replay proves costly for large batches, optimize in a follow-up.
