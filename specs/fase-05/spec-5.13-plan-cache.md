# Spec: 5.13 — Prepared Statement Plan Cache

## What to build (not how)

Two correctness and resource-management fixes for the prepared statement subsystem:

1. **Schema invalidation** — when a DDL statement changes the schema (CREATE/DROP/ALTER
   TABLE, CREATE/DROP INDEX), any cached `analyzed_stmt` that was compiled against the
   old schema must be detected as stale and re-analyzed before the next execute.
   Currently the cached plan is **never invalidated** — executing after a schema change
   produces incorrect results.

2. **LRU eviction** — `prepared_statements` is an unbounded `HashMap`. A client that
   prepares thousands of statements can exhaust server memory. Add a configurable cap
   with LRU eviction of the least-recently-used statement when the cap is reached.

The COM_STMT_EXECUTE fast path (skip parse+analyze when `analyzed_stmt` is Some) is
**already implemented** and working. This spec only covers the two missing pieces.

---

## Inputs / Outputs

### New field: `Database.schema_version: Arc<AtomicU64>`

- Starts at `0`.
- Incremented by `1` (Relaxed + Release fence) after any DDL statement completes
  successfully: `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE`, `CREATE INDEX`, `DROP INDEX`,
  `TRUNCATE TABLE`.
- Shared across all connections via `Arc`.
- Read by each connection on every `COM_STMT_EXECUTE`.

### New field: `PreparedStatement.compiled_at_version: u64`

- Set to `db.schema_version.load(Acquire)` at `COM_STMT_PREPARE` time.
- On `COM_STMT_EXECUTE`: if `stmt.compiled_at_version != db.schema_version.load(Acquire)`,
  the plan is stale → re-analyze and update both `analyzed_stmt` and
  `compiled_at_version` before executing.

### New field: `PreparedStatement.last_used_seq: u64`

- A per-connection monotonic counter incremented on every `COM_STMT_EXECUTE`.
- Used by LRU eviction: when the statement count reaches `max_prepared_stmts`,
  the statement with the lowest `last_used_seq` is evicted.

### New config field: `DbConfig.max_prepared_stmts_per_connection: usize`

- Default: `1024`.
- Enforced per-connection in `ConnectionState::prepare_statement()`.
- When the limit is reached, the LRU statement is removed before inserting the new one.

### Modified: `execute_query` / `execute_stmt` in `database.rs`

- After a successful DDL execution, increment `self.schema_version`.
- Detection: check `Stmt` type before dispatch — `CreateTable`, `DropTable`,
  `AlterTable`, `CreateIndex`, `DropIndex`, `Truncate` → increment.
- No change to the executor code itself; the increment happens in `database.rs`.

---

## Correctness invariant

```
At the moment COM_STMT_EXECUTE runs:
  compiled_at_version == schema_version  →  use cached plan
  compiled_at_version  < schema_version  →  re-analyze, update cache, then execute
```

This guarantees: a `SELECT * FROM users WHERE id = ?` plan is never executed against
a `users` schema that has had columns added, dropped, or renamed since the plan was
compiled.

**Over-invalidation is acceptable.** A DDL on `orders` invalidates plans for `users`.
This is a performance cost (one extra analyze call), never a correctness error.
DDL is rare relative to DML — over-invalidation is negligible in practice.

---

## Use cases

### 1. Happy path — no DDL between prepare and execute

```
PREPARE: analyze 'SELECT id FROM users WHERE id = ?'
         compiled_at_version = 3 (current schema_version)
EXECUTE (x1000):
  check: compiled_at_version (3) == schema_version (3)  → cache hit
  substitute_params_in_ast → execute_stmt
  last_used_seq = 1, 2, 3, ... 1000
Result: 1000 executes, zero re-analyzes
```

### 2. DDL between prepare and execute — stale plan detected

```
PREPARE: 'SELECT id, name FROM users WHERE id = ?'
         compiled_at_version = 3
ALTER TABLE users DROP COLUMN name
  → schema_version becomes 4
EXECUTE:
  check: compiled_at_version (3) != schema_version (4)  → stale
  re-analyze 'SELECT id, name FROM users WHERE id = ?'
  → ColumnNotFound { name: "name", table: "users" }  → error sent to client
  compiled_at_version updated to 4 (even on error, to avoid re-analyzing on next attempt)
```

### 3. LRU eviction — client prepares beyond cap

```
max_prepared_stmts_per_connection = 1024
Client prepares stmt_id 1 ... 1024  (fills the cache)
Client prepares stmt_id 1025:
  evict the stmt with lowest last_used_seq (the oldest unused)
  insert stmt_id 1025 with compiled_at_version = current, last_used_seq = 0
If client later executes the evicted stmt_id → COM_STMT_EXECUTE returns
  error 1243 "Unknown prepared statement handler" (existing behavior, unchanged)
```

### 4. Re-analyze succeeds — new column added

```
PREPARE: 'SELECT id FROM users WHERE id = ?'
         compiled_at_version = 3
ALTER TABLE users ADD COLUMN phone TEXT
  → schema_version = 4
EXECUTE WITH (42):
  stale → re-analyze
  plan updated: SELECT id FROM users WHERE id = ?  (phone not in projection, still valid)
  compiled_at_version = 4
  execute normally → returns id=42
```

### 5. Multiple connections — DDL on one invalidates others

```
Connection A: PREPARE stmt1 = 'SELECT * FROM t WHERE id=?'
              compiled_at_version = 5
Connection B: DROP TABLE t  → schema_version = 6
Connection A: EXECUTE stmt1
  stale (5 != 6) → re-analyze 'SELECT * FROM t WHERE id=?'
  → TableNotFound { name: "t" }  → error to client
```

---

## Acceptance criteria

- [ ] `Database.schema_version` starts at 0 and increments after every successful DDL
- [ ] After `ALTER TABLE` / `DROP TABLE` / `CREATE INDEX` (etc.), executing a previously
  prepared statement that references the changed table re-analyzes the plan
- [ ] If re-analysis fails (e.g., column no longer exists), the client receives the
  correct SQL error — not a panic, not a stale incorrect result
- [ ] If re-analysis succeeds (schema change unrelated to this query), execution
  continues with the updated plan
- [ ] `max_prepared_stmts_per_connection` default 1024: preparing the 1025th statement
  evicts the LRU entry
- [ ] The evicted statement's `stmt_id` returns error 1243 on subsequent EXECUTE (existing)
- [ ] `COM_STMT_EXECUTE` with a valid, unmodified schema still uses the cached plan
  (zero parse+analyze overhead on the hot path)
- [ ] `cargo test --workspace` passes

---

## Out of scope

- Cross-connection sharing of compiled plans (each connection keeps its own cache)
- Fine-grained invalidation by table_id (over-invalidation on any DDL is acceptable)
- Persistent plan cache across reconnects (in-memory only, per-connection lifecycle)
- `max_prepared_stmts_per_connection = 0` meaning "unlimited" (not needed for Phase 5)
- `DEALLOCATE`/`DROP PREPARED STATEMENT` SQL syntax (COM_STMT_CLOSE is sufficient)

---

## Dependencies

- `PreparedStatement` @ `axiomdb-network/src/mysql/session.rs` ✅
- `COM_STMT_EXECUTE` fast path @ `handler.rs:319` ✅ (already uses `analyzed_stmt`)
- `DbConfig` @ `axiomdb-storage/src/config.rs` ✅ (add field)
- `Database` @ `axiomdb-network/src/mysql/database.rs` ✅ (add `schema_version`)
- `axiomdb_sql::analyze()` ✅ (re-analysis path already exists)
- `SchemaCache` invalidation on re-analyze ✅ (already handled by `analyze_cached`)

✅ Spec written. You can now switch to `/effort medium` for the Plan phase.
