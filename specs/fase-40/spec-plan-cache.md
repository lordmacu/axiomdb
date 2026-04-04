# Spec: 40.2 — Statement Plan Cache with OID-based Invalidation

## What to build (not how)

Replace the current global-version plan cache with a per-OID fine-grained cache
that avoids invalidating plans for tables not touched by a DDL event.

The cache stores fully analyzed statements keyed by normalized SQL hash. Each
entry tracks the exact set of `TableId`s it depends on, together with the
per-table schema version at the time the plan was compiled. A hit is valid only
when every dependency's current version matches its cached snapshot. A DDL event
on table T invalidates only entries whose dependency set includes T — all other
entries remain alive.

Two cache surfaces must be kept consistent:

1. **COM_QUERY plan cache** — the existing `PlanCache` struct per connection,
   redesigned to use `CachedPlanSource` entries with OID dependency tracking.

2. **COM_STMT_PREPARE / COM_STMT_EXECUTE** — `PreparedStatement` in
   `session.rs` migrated from `compiled_at_version: u64` (global) to
   `deps: Vec<(TableId, u64)>` (per-table).

The catalog must expose a per-table version counter: `TableDef` gains a
`schema_version: u64` field, incremented atomically by every DDL operation that
touches that specific table (CREATE INDEX, DROP INDEX, ALTER TABLE, DROP TABLE).
The global `Database::schema_version` remains for DDL that has no specific table
target (e.g., CREATE TABLE, which creates a new OID).

---

## Inputs / Outputs

**Plan cache lookup:**
- Input: normalized SQL string, current catalog reference
- Output: `Option<CachedPlanSource>` — None on miss or any dep stale

**Plan cache store:**
- Input: normalized SQL string, analyzed `Stmt`, catalog reference
- Side effect: entry stored with deps snapshot extracted from the `Stmt`

**Per-table version bump:**
- Input: `TableId`
- Output: new `u64` version (monotonically increasing per table)
- Side effect: all cache entries whose deps include that `TableId` become
  invalid on next lookup

**COM_STMT_PREPARE:**
- Input: SQL with `?` placeholders
- Output: `PreparedStatement` with `deps: Vec<(TableId, u64)>` instead of
  single `compiled_at_version`

**COM_STMT_EXECUTE:**
- Input: `PreparedStatement`, parameter values
- Behavior: verifies each dep against current catalog version; re-analyzes
  only if any dep is stale (not on every schema change globally)

---

## Key data structures

```rust
/// A single entry in the plan cache.
/// Stored by normalized SQL hash; valid as long as every dep matches.
pub struct CachedPlanSource {
    /// Fully analyzed statement (Expr::Param nodes for literals).
    pub stmt: Arc<Stmt>,
    /// (TableId, per-table schema_version at compile time) for every table
    /// referenced by this statement.
    pub deps: Vec<(TableId, u64)>,
    /// Number of ? placeholders in the normalized form.
    pub param_count: usize,
    /// Logical clock updated on every hit — used for LRU eviction.
    pub last_used_seq: u64,
    /// Lifetime hit counter — for cache metrics.
    pub hits: u64,
}

/// Added to TableDef in axiomdb-catalog.
pub struct TableDef {
    // ... existing fields ...
    /// Monotonically increasing. Bumped by every DDL that modifies this table
    /// (CREATE/DROP INDEX, ALTER TABLE column add/drop, DROP TABLE).
    /// Initialized to 1 when the table is created.
    pub schema_version: u64,
}
```

**Dependency extraction** — `fn extract_table_deps(stmt: &Stmt, catalog) -> Vec<(TableId, u64)>`

Walks the analyzed `Stmt` and collects every `TableRef` → resolves to `TableId`
via catalog → snapshots `TableDef::schema_version`. One pass, O(tables in query).

---

## Use cases

### 1. Happy path — repeated SELECT with different literals

```
t=0: SELECT * FROM users WHERE id = 42  →  cache miss
     parse + analyze → CachedPlanSource { deps: [(users_id, 3)] }
     stored under hash("SELECT * FROM users WHERE id = ?")

t=1: SELECT * FROM users WHERE id = 99  →  cache hit
     hash matches, deps[users_id] == catalog.users.schema_version(3) ✓
     substitute 99 into Expr::Param → execute
     (parse + analyze skipped — ~5ms saved)
```

### 2. DDL on a different table — entry survives

```
t=0: Cache has entry for SELECT * FROM orders WHERE id = ?
     deps: [(orders_id=2, version=1)]

t=1: CREATE INDEX idx ON users(email)
     → users.schema_version bumped to 2
     → orders entry NOT invalidated (orders not in event)

t=2: SELECT * FROM orders WHERE id = 5  →  still a cache hit ✓
```

### 3. DDL on the exact table — entry invalidated

```
t=0: Cache has entry for SELECT * FROM users WHERE id = ?
     deps: [(users_id=1, version=3)]

t=1: CREATE INDEX idx ON users(email)
     → users.schema_version bumped to 4

t=2: SELECT * FROM users WHERE id = 7
     → lookup: dep (users_id=1, cached=3) != current(4)  →  miss
     → re-parse + re-analyze → new entry with version=4 stored
```

### 4. JOIN — both deps tracked

```
SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id
→ deps: [(users_id=1, v=3), (orders_id=2, v=1)]
→ invalidated if EITHER table's schema changes
```

### 5. COM_STMT_PREPARE / EXECUTE — stale detection

```
PREPARE:  SELECT * FROM users WHERE id = ?
          PreparedStatement { deps: [(users_id=1, v=3)], ... }

DDL:      CREATE INDEX idx ON users(name)
          → users.schema_version = 4

EXECUTE:  dep (users_id=1, v=3) != current(4)  →  re-analyze once
          PreparedStatement.deps updated to [(users_id=1, v=4)]
          execute proceeds — no error surfaced to client
```

### 6. LRU eviction — bounded memory

```
Cache at max_entries (default 512):
→ evict entry with lowest last_used_seq
→ new entry stored in its place
```

### 7. CREATE TABLE — global version path

```
CREATE TABLE new_table (...)
→ global schema_version bumped (new OID, no prior deps)
→ all existing cache entries remain valid (they don't reference new_table_id)
→ only new queries touching new_table will build fresh entries
```

---

## Acceptance criteria

- [ ] `TableDef::schema_version` exists and is initialized to `1` on creation
- [ ] Every DDL operation that modifies a table bumps its `schema_version` atomically: CREATE INDEX, DROP INDEX, DROP TABLE, ALTER TABLE (future)
- [ ] `CachedPlanSource` stores `deps: Vec<(TableId, u64)>` extracted from the analyzed `Stmt`
- [ ] `PlanCache::lookup` returns `None` if any dep's current version != cached version; returns `Some` only when all deps match
- [ ] DDL on table A does NOT invalidate cache entries for table B (verified by test)
- [ ] DDL on table A DOES invalidate all cache entries that reference table A (verified by test)
- [ ] `PreparedStatement` uses `deps: Vec<(TableId, u64)>` instead of `compiled_at_version: u64`
- [ ] COM_STMT_EXECUTE re-analyzes transparently when any dep is stale (no ERR sent to client)
- [ ] Cache is bounded at `max_entries` (default 512); LRU eviction when full
- [ ] `extract_table_deps` correctly resolves all `TableRef` nodes in SELECT, INSERT, UPDATE, DELETE, JOIN, subqueries
- [ ] `cargo test --workspace` passes clean
- [ ] Wire test: execute 200 SELECT queries with different literals — exactly 1 parse+analyze (verified via metrics or log count)
- [ ] Wire test: CREATE INDEX mid-session → only the affected table's cached plans are re-planned on next query; unrelated table plans survive
- [ ] Benchmark `select_pk` improves vs previous run (direction check, not a fixed threshold)

---

## Out of scope

- Cross-session shared plan cache (plans are per-connection; sharing requires Arc + locking strategy, deferred to Phase 6)
- Prepared statement protocol extensions beyond what already exists
- Cost-based re-planning (e.g., stale statistics trigger re-plan) — Phase 6.12
- View invalidation — no views yet
- Per-database isolation of cache entries (single database per connection for now)
- `ANALYZE TABLE` triggered re-plan — Phase 6.12

---

## Dependencies

- `TableDef` in `axiomdb-catalog` must gain `schema_version: u64` before any other change
- `extract_table_deps` requires a catalog reference at plan-store time (already available via `Database`)
- `CatalogChangeNotifier` is already implemented and can be used to push DDL events; however the primary invalidation path is lazy (checked at lookup time), not push-based — no strict dependency on the notifier for correctness
- Existing `substitute_params_in_ast` in `prepared.rs` is reused unchanged

---

## ⚠️ DEFERRED

- Cross-session plan sharing (Arc<CachedPlanSource> pool) → Phase 6
- Re-plan on statistics change (ANALYZE TABLE) → Phase 6.12
- ALTER TABLE column mutations → Phase 5 (ALTER not yet implemented)
