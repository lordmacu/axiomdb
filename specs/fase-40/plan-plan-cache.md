# Plan: 40.2 — Statement Plan Cache with OID-based Invalidation

## Reference: PostgreSQL's plancache.c

PostgreSQL's `CachedPlanSource` (plancache.h:105-147) uses:
- `relationOids: List*` — OIDs of all relations the query depends on
- `invalItems: List*` — non-relation dependencies (functions, types, domains)
- `is_valid: bool` — set false by invalidation callbacks, checked lazily at GetCachedPlan
- `generation: int` — increments each time a new plan is compiled
- `gplan: CachedPlan*` — shared generic plan (params not substituted)
- `generic_cost / total_custom_cost / num_custom_plans` — cost-based generic vs custom decision
- Callback registration via `CacheRegisterRelcacheCallback(PlanCacheRelCallback, 0)`

PostgreSQL's generic vs custom threshold: first 5 executions always produce custom
plans; after that, compare `generic_cost` vs `avg_custom_cost` and switch to generic
when the generic plan is cheaper.

MariaDB uses `TABLE_SHARE::tabledef_version` (binary string) per table, invalidated
by `tdc_wait_for_old_version` — less granular than PostgreSQL.

**AxiomDB adopts PostgreSQL's architecture**, improved with Rust's type system.

---

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-catalog/src/schema.rs` | modify | `TableDef` gains `schema_version: u64` |
| `crates/axiomdb-catalog/src/writer.rs` | modify | init `schema_version = 1`; add `bump_table_schema_version(table_id) -> u64` |
| `crates/axiomdb-catalog/src/reader.rs` | modify | deserialize `schema_version`; add `get_table_schema_version(id) -> Option<u64>` fast path |
| `crates/axiomdb-catalog/src/lib.rs` | modify | re-export new API |
| `crates/axiomdb-sql/src/plan_deps.rs` | **create** | `extract_table_deps`, `InvalItem`, `PlanDeps` |
| `crates/axiomdb-sql/src/lib.rs` | modify | `pub mod plan_deps;` |
| `crates/axiomdb-network/src/mysql/plan_cache.rs` | **rewrite** | `CachedPlanSource`, `PlanCache` with full PostgreSQL-inspired design |
| `crates/axiomdb-network/src/mysql/session.rs` | modify | `PreparedStatement.deps` replaces `compiled_at_version` |
| `crates/axiomdb-network/src/mysql/handler.rs` | modify | COM_STMT_EXECUTE re-analyze via dep check |
| `crates/axiomdb-network/src/mysql/database.rs` | modify | DDL calls `bump_table_schema_version` + `plan_cache.invalidate_table` |
| `crates/axiomdb-network/tests/plan_cache_oid.rs` | **create** | integration tests |

---

## Key data structures (PostgreSQL-inspired, Rust)

```rust
// ── axiomdb-sql/src/plan_deps.rs ─────────────────────────────────────────────

/// A dependency on a non-table catalog object (function, type, index — future).
/// Mirrors PostgreSQL's PlanInvalItem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalItem {
    pub kind: InvalItemKind,
    pub object_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalItemKind {
    Index,       // index OID — plan is invalid if index is dropped
    // Function, Type — deferred to Phase 6 when UDFs exist
}

/// All dependencies extracted from an analyzed Stmt.
/// Mirrors PostgreSQL's (relationOids + invalItems).
#[derive(Debug, Clone, Default)]
pub struct PlanDeps {
    /// (TableId, schema_version_at_compile_time) for every table referenced.
    pub tables: Vec<(TableId, u64)>,
    /// Non-table dependencies (index OIDs used by the chosen access method).
    pub items: Vec<InvalItem>,
}

impl PlanDeps {
    /// Returns true if any dependency is stale according to the catalog.
    pub fn is_stale(&self, reader: &mut CatalogReader) -> bool { ... }
}

// ── axiomdb-network/src/mysql/plan_cache.rs ───────────────────────────────────

/// Mirrors PostgreSQL's CachedPlanSource.
pub struct CachedPlanSource {
    /// Analyzed statement with Expr::Param nodes for normalized literals.
    /// Shared across all executions that use this source.
    /// None means the plan was invalidated and must be re-compiled.
    pub stmt: Option<Arc<Stmt>>,

    /// All catalog dependencies at compile time.
    /// Checked lazily on every lookup; set_invalid() clears stmt.
    pub deps: PlanDeps,

    /// Number of ? placeholders in the normalized form.
    pub param_count: usize,

    /// Incremented each time this source is re-compiled after invalidation.
    /// Mirrors PostgreSQL's CachedPlanSource.generation.
    pub generation: u32,

    /// Total number of times this source was executed (hits).
    pub exec_count: u64,

    /// Sum of execution durations in µs (for generic vs custom decision).
    pub total_exec_us: u64,

    /// Logical clock for LRU eviction. Updated on every hit.
    pub last_used_seq: u64,
}

/// Generic vs custom plan decision — mirrors PostgreSQL's choose_custom_plan().
///
/// - Executions 1..=GENERIC_THRESHOLD: always use custom plan (substitute params).
/// - After GENERIC_THRESHOLD: use generic plan (params left as Expr::Param,
///   substituted at execution time), unless custom plans have been measurably
///   faster on average.
///
/// Generic plan avoids repeated param substitution for queries that are always
/// executed with the same structural pattern (e.g. SELECT * WHERE id = ?).
const GENERIC_THRESHOLD: u64 = 5;

pub struct PlanCache {
    /// key: fnv1a(normalized_sql)
    entries: HashMap<u64, CachedPlanSource>,
    max_entries: usize,
    /// Monotonic clock for LRU — incremented on every access.
    seq: u64,
    /// Cache-wide hit/miss counters (exposed via SHOW STATUS).
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
}
```

---

## Algorithm

### Step 1 — `TableDef::schema_version` in catalog

`schema_version: u64` appended to the on-disk `TableDef` row (8 bytes LE at end).
Backward compatibility: rows shorter than the new size default to `schema_version = 1`.

```rust
// writer.rs
pub fn bump_table_schema_version(&mut self, table_id: TableId) -> Result<u64, DbError> {
    let mut def = self.read_table_def(table_id)?;
    def.schema_version += 1;
    self.write_table_def(&def)?;
    Ok(def.schema_version)
}
```

Called after every successful DDL that modifies a specific table:
`CREATE INDEX`, `DROP INDEX`, `DROP TABLE`, `TRUNCATE TABLE`.

NOT called for `CREATE TABLE` / `CREATE DATABASE` / `DROP DATABASE` — those bump
the global `schema_version` only (new OIDs, no prior plan can reference them).

### Step 2 — `extract_table_deps` — full AST traversal

```rust
// plan_deps.rs
pub fn extract_table_deps(
    stmt: &Stmt,
    reader: &mut CatalogReader,
    db: &str,
) -> Result<PlanDeps, DbError>
```

Exhaustive traversal — every Stmt variant explicitly handled:

```
Stmt::Select(s)        → collect_from(&s.from) + collect_joins(&s.joins)
                          + recurse into subqueries in Expr::Subquery
Stmt::Insert(s)        → collect_tableref(&s.table)
Stmt::Update(s)        → collect_tableref(&s.table)
Stmt::Delete(s)        → collect_tableref(&s.table)
Stmt::CreateIndex(s)   → collect_tableref(&s.table)  [for bump, not cache]
DDL (everything else)  → PlanDeps::default()          [DDL never cached]
```

`collect_from(from: &Option<FromClause>)`:
```
None                    → skip
Some(Table(tref))       → resolve tref → push (id, schema_version)
Some(Subquery { sel })  → recurse into sel.from + sel.joins
```

`collect_joins(joins: &[JoinClause])`:
```
for join → collect_from(Some(&join.table))
```

Deduplication: `HashMap<TableId, u64>` during traversal → `Vec` at end.
If the same table appears multiple times (self-join), only one dep entry.

Resolution: `CatalogReader::get_table_in_database(db, schema, name)`
→ returns `TableDef` with `id` and `schema_version`.
If table not found → `DbError::TableNotFound` (plan is uncacheable).

### Step 3 — `PlanDeps::is_stale`

```rust
pub fn is_stale(&self, reader: &mut CatalogReader) -> Result<bool, DbError> {
    for (table_id, cached_ver) in &self.tables {
        match reader.get_table_schema_version(*table_id)? {
            None => return Ok(true),  // table was dropped
            Some(cur) if cur != *cached_ver => return Ok(true),
            _ => {}
        }
    }
    for item in &self.items {
        if InvalItemKind::Index == item.kind {
            // verify index still exists — deferred; for now always valid
        }
    }
    Ok(false)
}
```

### Step 4 — `PlanCache::lookup` (PostgreSQL-inspired lazy validation)

```rust
pub fn lookup(
    &mut self,
    sql: &str,
    reader: &mut CatalogReader,
    db: &str,
) -> Result<Option<Stmt>, DbError> {
    let (normalized, params) = normalize_sql(sql);
    let key = fnv1a(normalized.as_bytes());

    let entry = match self.entries.get_mut(&key) {
        None => { self.misses += 1; return Ok(None); }
        Some(e) => e,
    };

    // Lazy validity check — mirrors PostgreSQL GetCachedPlan()
    if entry.stmt.is_none() || entry.deps.is_stale(reader)? {
        self.entries.remove(&key);
        self.misses += 1;
        self.invalidations += 1;
        return Ok(None);
    }

    // Generic vs custom plan decision — mirrors PostgreSQL choose_custom_plan()
    let stmt = if entry.exec_count < GENERIC_THRESHOLD {
        // Custom plan: substitute params into a clone
        let base = Arc::clone(entry.stmt.as_ref().unwrap());
        substitute_params((*base).clone(), &params)?
    } else {
        // Generic plan: substitute at execution, reusing the shared Arc<Stmt>
        let base = Arc::clone(entry.stmt.as_ref().unwrap());
        substitute_params((*base).clone(), &params)?
        // NOTE: when we have a cost model, compare generic_cost vs avg_custom_cost
        // and skip substitution entirely for queries with no Expr::Param nodes.
        // Deferred to Phase 6.12 when cost model exists.
    };

    entry.exec_count += 1;
    entry.last_used_seq = self.seq;
    self.seq += 1;
    self.hits += 1;

    Ok(Some(stmt))
}
```

### Step 5 — `PlanCache::store`

```rust
pub fn store(
    &mut self,
    sql: &str,
    stmt: Stmt,
    deps: PlanDeps,
) {
    let (normalized, params) = normalize_sql(sql);
    let key = fnv1a(normalized.as_bytes());

    if self.entries.len() >= self.max_entries {
        self.evict_lru();
    }

    // Preserve generation count if replacing an invalidated entry
    let generation = self.entries.get(&key)
        .map(|e| e.generation + 1)
        .unwrap_or(0);

    self.entries.insert(key, CachedPlanSource {
        stmt: Some(Arc::new(stmt)),
        deps,
        param_count: params.len(),
        generation,
        exec_count: 0,
        total_exec_us: 0,
        last_used_seq: self.seq,
    });
    self.seq += 1;
}
```

### Step 6 — `PlanCache::invalidate_table` (eager push, belt-and-suspenders)

```rust
/// Called by DDL handler after bump_table_schema_version.
/// Mirrors PostgreSQL's PlanCacheRelCallback.
pub fn invalidate_table(&mut self, table_id: TableId) {
    let mut count = 0u64;
    self.entries.retain(|_, e| {
        let affected = e.deps.tables.iter().any(|(id, _)| *id == table_id);
        if affected { count += 1; }
        !affected
    });
    self.invalidations += count;
}
```

### Step 7 — `PreparedStatement` migration

```rust
pub struct PreparedStatement {
    // ... existing fields ...

    // REPLACES: compiled_at_version: u64
    /// Per-table OID dependencies at compile time.
    /// Mirrors PostgreSQL's CachedPlanSource.relationOids.
    pub deps: PlanDeps,

    /// How many times this prepared statement has been re-analyzed after
    /// schema changes. Mirrors PostgreSQL's CachedPlanSource.generation.
    pub generation: u32,
}
```

COM_STMT_EXECUTE path:
```rust
if ps.deps.is_stale(&mut catalog_reader)? {
    // Transparent re-analysis — mirrors PostgreSQL RevalidateCachedQuery()
    let new_stmt = axiomdb_sql::parser::parse(&ps.sql_template, None)?;
    let analyzed = axiomdb_sql::analyze(&new_stmt, &catalog, db)?;
    ps.deps = extract_table_deps(&analyzed, &mut catalog_reader, db)?;
    ps.analyzed_stmt = Some(analyzed);
    ps.generation += 1;
    // No ERR sent to client — fully transparent
}
```

### Step 8 — DDL integration in `database.rs`

```rust
// After successful execution of DDL:
match &stmt {
    Stmt::CreateIndex(s) => {
        let id = resolve_table_id(&s.table, &catalog, db)?;
        catalog_writer.bump_table_schema_version(id)?;
        conn.plan_cache.invalidate_table(id);
    }
    Stmt::DropIndex(s) => {
        let id = resolve_table_id(&s.table, &catalog, db)?;
        catalog_writer.bump_table_schema_version(id)?;
        conn.plan_cache.invalidate_table(id);
    }
    Stmt::DropTable(s) => {
        let id = resolve_table_id(&s.table, &catalog, db)?;
        catalog_writer.bump_table_schema_version(id)?;
        conn.plan_cache.invalidate_table(id);
        // drop proceeds normally
    }
    Stmt::TruncateTable(s) => {
        let id = resolve_table_id(&s.table, &catalog, db)?;
        catalog_writer.bump_table_schema_version(id)?;
        conn.plan_cache.invalidate_table(id);
    }
    Stmt::CreateTable(_) | Stmt::CreateDatabase(_) | Stmt::DropDatabase(_) => {
        // Global bump only — no prior OID in any cached plan
        global_schema_version.fetch_add(1, Ordering::Release);
    }
    _ => {}
}
```

---

## Implementation phases (in order — each must compile and test before next)

1. **Catalog** — `TableDef::schema_version` on-disk with backward-compat deserialization; `bump_table_schema_version`; `get_table_schema_version` fast path
2. **`plan_deps.rs`** — `PlanDeps`, `InvalItem`, `extract_table_deps` full AST traversal; unit tests for all Stmt variants
3. **`PlanCache` rewrite** — `CachedPlanSource` with all fields; lookup/store/invalidate_table/evict_lru; metrics counters; unit tests
4. **`PreparedStatement` migration** — replace `compiled_at_version` with `deps: PlanDeps`; `is_stale()` delegate; COM_STMT_EXECUTE re-analyze path
5. **DDL integration** — `database.rs` bump + invalidate calls; integration tests
6. **Wire tests** — append to `tools/wire-test.py`

---

## Tests to write

**Unit — `plan_deps.rs`:**
- SELECT single table → one dep
- SELECT with JOIN → two deps, no duplicates
- SELECT with subquery in FROM → subquery table included
- INSERT / UPDATE / DELETE → one dep each
- DDL statements → empty PlanDeps
- `is_stale`: all current → false; one bumped → true; table dropped → true

**Unit — `plan_cache.rs`:**
- Hit: normalized SQL matches, all deps current → returns substituted stmt
- Miss: unknown SQL → None
- Stale miss: dep version bumped → None, entry removed, `invalidations` incremented
- Survivor: DDL on table A → lookup for table B entry succeeds (NOT invalidated)
- `invalidate_table`: removes entries with matching dep, leaves others untouched
- LRU eviction: fill to max_entries + 1 → lowest `last_used_seq` evicted
- Generation: store → generation=0; stale+re-store → generation=1
- Metrics: hits/misses/invalidations increment correctly

**Unit — `session.rs`:**
- `PreparedStatement::is_stale` delegates to `deps.is_stale`

**Integration — `tests/plan_cache_oid.rs`:**
- 200 SELECTs with different PK literals → first is a miss, 199 are hits (verified via `plan_cache.hits`)
- CREATE INDEX on `users` → users entries evicted; `orders` entries survive
- DROP TABLE → subsequent SELECT on dropped table returns ERR(1146), not stale cached result
- COM_STMT_PREPARE + DDL + COM_STMT_EXECUTE → re-analysis is transparent, result is correct
- Self-join (same table twice) → single dep entry in PlanDeps

**Wire tests — appended to `tools/wire-test.py`:**
- 50 SELECTs with different PK values → all return correct rows
- CREATE INDEX mid-session → next SELECT still returns correct rows
- DROP + recreate table with different schema → SELECT old column → `Unknown column` error

---

## Anti-patterns to avoid

- **DO NOT** use a global shared `RwLock<PlanCache>` — cache is per-connection, zero contention
- **DO NOT** skip `invalidate_table` eager push — lazy check alone is correct but wastes one extra catalog read per stale entry; belt-and-suspenders is better
- **DO NOT** cache DDL stmts — `extract_table_deps` returns `PlanDeps::default()` for them; caller must not call `store` for DDL
- **DO NOT** clone `Arc<Stmt>` inner value on lookup until param substitution — clone the Arc pointer (cheap), clone the Stmt only when substituting (unavoidable because tree mutation)
- **DO NOT** panic on missing `schema_version` in legacy catalog rows — default to `1`
- **DO NOT** remove the global `schema_version` bump path — `CREATE TABLE` still needs it to signal new table availability

---

## Risks

| Risk | Mitigation |
|---|---|
| On-disk format regression for TableDef | `from_bytes` checks slice length; default `schema_version = 1` if field absent |
| `extract_table_deps` misses a FromClause path | Exhaustive `match` with compiler warning on unhandled variants; test covers every Stmt variant |
| Stale plan served after DROP TABLE | `get_table_schema_version` returns None for dropped table → `is_stale` returns true → re-plans; integration test verifies |
| LRU O(n) too slow at max_entries | max_entries = 512; O(512) ≈ few µs — acceptable; IndexMap upgrade path available |
| DDL bump fails mid-transaction | bump is best-effort; if it fails, global `schema_version` still incremented → all plans invalidated (safe fallback) |
| PreparedStatement re-analysis fails after schema change | On error → `ps.analyzed_stmt = None`; next EXECUTE returns ERR(1146) to client — correct behavior |
