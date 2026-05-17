# Plan: statement-fingerprinting — auto-prepared statements

Phase: perf-sqlite-gap
Task: Attack 2 — automatic plan reuse by query shape
Spec: specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md
Status: done (2.1-2.3 infrastructure landed; 2.4 wire-up reverted, follow-up needed)

## Summary

Six TDD-ordered steps. **2.1** adds the AST literal walker
(`extract_literals` + reuses existing `substitute_params` promoted to
`axiomdb-sql`). **2.2** adds the `shape_hash` function. **2.3** adds the
`CachedPlan` + `SessionContext` API with LRU eviction. **2.4** wires
everything into `Db::run_inner` and validates the perf budget on the
bench. **2.5** layers a SQL-text fast path on top (cheap pre-check
before fingerprinting). **2.6** is the closing protocol — workspace
gates, wire smoke, docs, memory.

Pre-existing infra reused (no rewrite):
- `PlanDeps` + `extract_table_deps` (plan_deps.rs:141) for cache
  invalidation
- `substitute_params` (embedded/lib.rs:440) — promoted to `axiomdb-sql`
  in Step 2.1 so both the manual `PreparedStatement` and the auto-cache
  use the same implementation
- `schema_version` stamping pattern (already wired by Attack 3.A/B)

## Dependencies

Must be done first:
- [x] spec-statement-fingerprinting approved (commit `04df496d`)
- [x] Attack 3.A — schema_version infra (commit `50930d99`)
- [x] Attack 3.B — version-stamped cache pattern (commit `accd6827`)
- [x] `PlanDeps` infra exists (`plan_deps.rs`)
- [x] `extract_table_deps` exists (`plan_deps.rs:141`)

Blocks (until done):
- Cross-session plan cache (Attack 2.7) — deferred follow-up
- Attack 4 (per-row engine work) — only worth attempting after Attack 2
  closes the per-statement cost

## Affected files

New files:
- `crates/axiomdb-sql/src/statement_cache.rs` — `CachedPlan`,
  `extract_literals`, `shape_hash`, `substitute_params` (promoted)
- `crates/axiomdb-sql/tests/integration_statement_cache.rs` — all spec
  edge-case tests

Modified files:
- `crates/axiomdb-sql/src/lib.rs` — `pub mod statement_cache;` + re-exports
- `crates/axiomdb-sql/src/session.rs` — add `statement_cache:
  HashMap<u64, (CachedPlan, lru_seq: u64)>` + API (get/insert/count/clear)
- `crates/axiomdb-embedded/src/lib.rs` — switch `Db::run_inner` to the
  cache flow; replace duplicated `substitute_params` / `count_params`
  with re-exports from `axiomdb-sql`
- `docs/perf-sqlite-gap.md` — Step 6 update
- `memory/project_sqlite_baseline.md` — Step 6 update

---

## Step 2.1 — Literal walker (`extract_literals` + `substitute_params`)

**Goal:** Round-trip property: `substitute_params(extract_literals(stmt))
== stmt` for all Expr variants covered by the existing
`substitute_params`.

**Files:**
- `crates/axiomdb-sql/src/statement_cache.rs` (new)
- `crates/axiomdb-sql/src/lib.rs` (add `pub mod statement_cache`)
- `crates/axiomdb-sql/tests/integration_statement_cache.rs` (new)

**Approach:** TDD. Write a roundtrip test using the bench INSERT shape;
then implement the walker.

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_statement_cache.rs
use axiomdb_sql::{parse, statement_cache::{extract_literals, substitute_params}};

#[test]
fn extract_then_substitute_roundtrips_simple_insert() {
    let original = parse(
        "INSERT INTO t VALUES (1, 'hello', 3.14, TRUE, NULL)", None
    ).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 5,
        "5 literals: 1 INT, 1 TEXT, 1 REAL, 1 BOOL, 1 NULL");
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original, "round-trip must match original AST");
}

#[test]
fn extract_handles_select_where_binary_op() {
    let original = parse(
        "SELECT id FROM t WHERE id = 42 AND name = 'alice'", None
    ).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 2);
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_multi_row_values() {
    let original = parse(
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')", None
    ).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 6);
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_no_literals() {
    // SELECT * has no literals; extracted should be empty.
    let original = parse("SELECT * FROM t", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert!(extracted.is_empty());
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/statement_cache.rs

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::ast::{InsertSource, SelectItem, Stmt};
use crate::expr::Expr;

/// Walks the statement, replacing every `Expr::Literal(v)` with
/// `Expr::Param { idx }` and returning the extracted values in walk order.
///
/// The walker matches the same Expr variants that `substitute_params`
/// rewrites (binary op, unary op, IsNull, Between, In, Like, Function,
/// Cast). Other variants are left alone — their literals stay in-place
/// and contribute to the shape hash, so the cache still keys correctly
/// (just doesn't compress those positions).
pub fn extract_literals(stmt: &mut Stmt) -> Vec<Value> {
    let mut out = Vec::new();
    walk_stmt(stmt, &mut out);
    out
}

fn walk_stmt(stmt: &mut Stmt, out: &mut Vec<Value>) {
    match stmt {
        Stmt::Select(s) => {
            if let Some(ref mut wc) = s.where_clause { walk_expr(wc, out); }
            for item in &mut s.columns {
                if let SelectItem::Expr { expr, .. } = item { walk_expr(expr, out); }
            }
        }
        Stmt::Insert(s) => {
            if let InsertSource::Values(rows) = &mut s.source {
                for row in rows { for expr in row { walk_expr(expr, out); } }
            }
        }
        Stmt::Update(s) => {
            for assign in &mut s.assignments { walk_expr(&mut assign.value, out); }
            if let Some(ref mut wc) = s.where_clause { walk_expr(wc, out); }
        }
        Stmt::Delete(s) => {
            if let Some(ref mut wc) = s.where_clause { walk_expr(wc, out); }
        }
        _ => {} // DDL, others — leave alone
    }
}

fn walk_expr(expr: &mut Expr, out: &mut Vec<Value>) {
    match expr {
        Expr::Literal(_) => {
            // Replace this node with Param.
            let v = std::mem::replace(expr, Expr::Param { idx: out.len() });
            if let Expr::Literal(inner) = v {
                out.push(inner);
            } else {
                unreachable!()
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            walk_expr(left, out); walk_expr(right, out);
        }
        Expr::UnaryOp { operand, .. } => walk_expr(operand, out),
        Expr::IsNull { expr: e, .. } => walk_expr(e, out),
        Expr::Between { expr, low, high, .. } => {
            walk_expr(expr, out); walk_expr(low, out); walk_expr(high, out);
        }
        Expr::In { expr, list, .. } => {
            walk_expr(expr, out);
            for it in list { walk_expr(it, out); }
        }
        Expr::Like { expr, pattern, .. } => {
            walk_expr(expr, out); walk_expr(pattern, out);
        }
        Expr::Function { args, .. } => {
            for a in args { walk_expr(a, out); }
        }
        Expr::Cast { expr: e, .. } => walk_expr(e, out),
        _ => {} // Column, Param, etc. — no literals
    }
}

/// Moved from `axiomdb-embedded/src/lib.rs:440`. Substitutes
/// `Expr::Param { idx }` back to `Expr::Literal(values[idx])`.
pub fn substitute_params(mut stmt: Stmt, values: &[Value]) -> Result<Stmt, DbError> {
    // ... same body as today, moved verbatim ...
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_statement_cache
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 2.1 — AST literal walker

Adds extract_literals(stmt) → Vec<Value>: walks the AST replacing
every Expr::Literal with Expr::Param { idx } in walk order.

Promotes substitute_params from axiomdb-embedded to axiomdb-sql so
the manual PreparedStatement (Phase 10.8) and the upcoming auto-cache
share the same implementation.

4 round-trip tests: simple INSERT, SELECT WHERE binary op, multi-row
VALUES, no-literals SELECT *.
```

---

## Step 2.2 — `shape_hash` function

**Goal:** Two statements with identical structure (modulo extracted
literal positions = `Expr::Param { idx: N }`) hash to the same u64;
structurally distinct statements hash to different u64s with high
probability.

**Files:**
- `crates/axiomdb-sql/src/statement_cache.rs` (extend)
- `crates/axiomdb-sql/tests/integration_statement_cache.rs` (extend)

**Approach:** TDD — write the equality / inequality tests, then add
the hash function. Use Rust's `std::hash::Hash` derive on `Stmt` if
already derived, else use a custom traversal.

### Tests to add

```rust
#[test]
fn shape_hash_equal_for_same_shape_different_literals() {
    let mut s1 = parse("INSERT INTO t VALUES (1, 'a')", None).unwrap();
    let mut s2 = parse("INSERT INTO t VALUES (99, 'z')", None).unwrap();
    let _ = extract_literals(&mut s1);
    let _ = extract_literals(&mut s2);
    assert_eq!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_table() {
    let mut s1 = parse("INSERT INTO t1 VALUES (1)", None).unwrap();
    let mut s2 = parse("INSERT INTO t2 VALUES (1)", None).unwrap();
    let _ = extract_literals(&mut s1);
    let _ = extract_literals(&mut s2);
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_column_list() {
    let mut s1 = parse("INSERT INTO t(a, b) VALUES (1, 2)", None).unwrap();
    let mut s2 = parse("INSERT INTO t(a, c) VALUES (1, 2)", None).unwrap();
    let _ = extract_literals(&mut s1);
    let _ = extract_literals(&mut s2);
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_values_count() {
    let mut s1 = parse("INSERT INTO t VALUES (1, 2)", None).unwrap();
    let mut s2 = parse("INSERT INTO t VALUES (1, 2, 3)", None).unwrap();
    let _ = extract_literals(&mut s1);
    let _ = extract_literals(&mut s2);
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_in_list_length() {
    let mut s1 = parse("SELECT * FROM t WHERE id IN (1, 2)", None).unwrap();
    let mut s2 = parse("SELECT * FROM t WHERE id IN (1, 2, 3)", None).unwrap();
    let _ = extract_literals(&mut s1);
    let _ = extract_literals(&mut s2);
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}
```

### Implementation outline

```rust
/// Computes a 64-bit hash from the statement's shape (literals already
/// extracted into Params). Uses Debug-format hashing for simplicity —
/// Stmt and Expr both derive Debug; the format is stable across calls
/// within the same compile and includes every structural element.
///
/// Alternative considered: implement `std::hash::Hash` on `Stmt` and
/// recursively hash the AST. Future optimization if Debug-format
/// hashing becomes the bottleneck (current measurement: parse + extract
/// dominates; hash is negligible).
pub fn shape_hash(stmt: &Stmt) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    format!("{stmt:?}").hash(&mut h);
    h.finish()
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_statement_cache
```

### Commit

```
feat(perf-sqlite-gap): step 2.2 — shape_hash for AST shapes

shape_hash(stmt) → u64. Two ASTs with identical structure (modulo
Param indices) hash equal; structurally distinct ASTs hash distinct
with overwhelming probability.

Implementation uses Debug-format hashing for simplicity. Profiling
later if it becomes a bottleneck.

5 tests: same shape different literals, different table, different
column list, different VALUES count, different IN list length.
```

---

## Step 2.3 — `CachedPlan` + `SessionContext` API + LRU

**Goal:** A bounded LRU cache on `SessionContext` keyed by shape_hash.
Reuses `PlanDeps::is_stale` for invalidation. Default capacity 256.

**Files:**
- `crates/axiomdb-sql/src/statement_cache.rs` (add `CachedPlan`)
- `crates/axiomdb-sql/src/session.rs` (add cache + API)
- `crates/axiomdb-sql/tests/integration_statement_cache.rs` (extend)

**Approach:** TDD — write tests for hit, miss, stale (via deps),
LRU eviction; then implement.

### Tests to add

```rust
// All tests build a SessionContext + Storage + TxnManager directly
// (no SQL — exercising the cache API in isolation).
//
// Reuses the `harness` module from integration_resolve_table_cache.rs;
// or duplicate the small setup.

#[test]
fn cache_hit_returns_same_plan() {
    let mut ctx = SessionContext::default();
    let plan = CachedPlan { /* ... fake ... */ };
    ctx.cache_plan(0x1234, plan.clone());
    let got = ctx.get_cached_plan_for_test(0x1234);
    assert!(got.is_some());
}

#[test]
fn cache_miss_returns_none() {
    let ctx = SessionContext::default();
    let got = ctx.get_cached_plan_for_test(0x9999);
    assert!(got.is_none());
}

#[test]
fn cache_lru_evicts_oldest_when_full() {
    let mut ctx = SessionContext::default();
    for i in 0..STATEMENT_CACHE_MAX_ENTRIES as u64 + 1 {
        ctx.cache_plan(i, fake_plan());
    }
    assert_eq!(ctx.statement_cache_count(), STATEMENT_CACHE_MAX_ENTRIES);
    // Oldest (hash 0) was evicted; newest (hash N) is present.
    assert!(ctx.get_cached_plan_for_test(0).is_none());
    assert!(ctx.get_cached_plan_for_test(STATEMENT_CACHE_MAX_ENTRIES as u64).is_some());
}

#[test]
fn cache_stale_via_plan_deps_evicts() {
    // INSERT a plan with PlanDeps pointing to a real table.
    // Bump that table's schema_version via the catalog.
    // Look up the plan with deps validation — should be None (evicted).
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

    // Compile a plan + cache it
    let mut stmt = parse("INSERT INTO t VALUES (1)", None).unwrap();
    extract_literals(&mut stmt);
    let deps = build_deps_for(&stmt, &h);  // small helper
    h.session.cache_plan(0x42, CachedPlan { analyzed: stmt, param_count: 1, deps });

    // Bump schema_version
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();

    // Lookup with validation must return None and evict the entry.
    let snap = h.txn.snapshot();
    let mut reader = CatalogReader::new(&h.storage, snap).unwrap();
    let got = h.session.get_cached_plan(0x42, &mut reader).unwrap();
    assert!(got.is_none());
    assert_eq!(h.session.statement_cache_count(), 0, "stale entry evicted");
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/statement_cache.rs

use crate::plan_deps::PlanDeps;
use crate::ast::Stmt;

pub const STATEMENT_CACHE_MAX_ENTRIES: usize = 256;

#[derive(Debug, Clone)]
pub struct CachedPlan {
    /// Analyzed AST with literals already extracted to `Expr::Param`.
    pub analyzed: Stmt,
    /// Number of extracted literals = number of `Expr::Param` nodes.
    pub param_count: usize,
    /// Catalog deps captured at compile time; staleness checked on every hit.
    pub deps: PlanDeps,
}

// crates/axiomdb-sql/src/session.rs additions
impl SessionContext {
    pub fn get_cached_plan(
        &mut self,
        shape_hash: u64,
        reader: &mut CatalogReader<'_>,
    ) -> Result<Option<&CachedPlan>, DbError> {
        let entry = self.statement_cache.get(&shape_hash);
        match entry {
            None => Ok(None),
            Some((plan, _)) => {
                if plan.deps.is_stale(reader)? {
                    self.statement_cache.remove(&shape_hash);
                    Ok(None)
                } else {
                    // Bump LRU seq.
                    self.statement_lru_seq += 1;
                    if let Some((_, seq)) = self.statement_cache.get_mut(&shape_hash) {
                        *seq = self.statement_lru_seq;
                    }
                    Ok(self.statement_cache.get(&shape_hash).map(|(p, _)| p))
                }
            }
        }
    }

    pub fn cache_plan(&mut self, shape_hash: u64, plan: CachedPlan) {
        if self.statement_cache.len() >= STATEMENT_CACHE_MAX_ENTRIES {
            // Evict oldest by seq.
            if let Some(&oldest_key) = self.statement_cache
                .iter()
                .min_by_key(|(_, (_, seq))| *seq)
                .map(|(k, _)| k)
            {
                self.statement_cache.remove(&oldest_key);
            }
        }
        self.statement_lru_seq += 1;
        self.statement_cache.insert(shape_hash, (plan, self.statement_lru_seq));
    }

    pub fn statement_cache_count(&self) -> usize {
        self.statement_cache.len()
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_statement_cache
./tools/vm.sh test -p axiomdb-sql   # broader safety net
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 2.3 — CachedPlan + SessionContext LRU

CachedPlan { analyzed, param_count, deps }. SessionContext gains:
  get_cached_plan(hash, reader) → Option<&CachedPlan>
  cache_plan(hash, plan)
  statement_cache_count()

Bounded LRU at STATEMENT_CACHE_MAX_ENTRIES = 256. Eviction uses a
sequence counter (not a Vec or LinkedList) — keeps the API simple
and matches the ~µs lookup budget.

Stale entries are lazy-evicted via PlanDeps::is_stale on lookup,
reusing the existing infrastructure.

4 tests: hit, miss, LRU eviction at cap, stale via plan_deps.
```

---

## Step 2.4 — Wire into `Db::run_inner` + measure budget

**Goal:** `db.run(sql)` automatically uses the cache. Bench shows the
spec target: INSERT batched ≥ 100K rows/s, `execute_with_ctx` ≤ 10 µs.

**Files:**
- `crates/axiomdb-embedded/src/lib.rs` — rewrite `run_inner`
- `crates/axiomdb-sql/tests/integration_statement_cache.rs` (extend
  with end-to-end tests)

**Approach:** TDD — write the end-to-end tests first (idempotent INSERT
loop, cache count after N calls), then change the path.

### Tests to add

```rust
#[test]
fn run_inner_caches_after_first_call() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    assert_eq!(h.session.statement_cache_count(), 0);
    h.run("INSERT INTO t VALUES (1, 'a')").unwrap();
    let after_one = h.session.statement_cache_count();
    h.run("INSERT INTO t VALUES (2, 'b')").unwrap();
    let after_two = h.session.statement_cache_count();
    assert_eq!(after_one, 1);
    assert_eq!(after_two, 1, "second INSERT same shape hits cache");
}

#[test]
fn run_inner_caches_distinct_shapes() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (a INT PRIMARY KEY, b INT, c INT)").unwrap();
    h.run("INSERT INTO t(a, b) VALUES (1, 2)").unwrap();
    h.run("INSERT INTO t(a, c) VALUES (3, 4)").unwrap();
    assert_eq!(h.session.statement_cache_count(), 2);
}

#[test]
fn run_inner_evicts_on_alter() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    h.run("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(h.session.statement_cache_count(), 1);
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();
    // Next INSERT with NEW shape (2 columns) — old entry is stale,
    // gets evicted lazily; new entry inserted. Count = 1.
    h.run("INSERT INTO t VALUES (2, 99)").unwrap();
    assert_eq!(h.session.statement_cache_count(), 1);
    // Verify post-ALTER schema actually used:
    let rows = h.query("SELECT x FROM t WHERE id=2");
    assert_eq!(rows[0][0], Value::Int(99));
}

#[test]
fn run_inner_does_not_cache_ddl() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    assert_eq!(h.session.statement_cache_count(), 0,
        "CREATE TABLE must not enter the cache");
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();
    assert_eq!(h.session.statement_cache_count(), 0,
        "ALTER TABLE must not enter the cache");
}
```

### Implementation outline

```rust
// crates/axiomdb-embedded/src/lib.rs — rewrite of run_inner
fn run_inner(&mut self, sql: &str) -> Result<QueryResult, DbError> {
    if self.degraded && sql_may_mutate(sql) {
        return Err(DbError::DiskFull { /* ... */ });
    }
    // Parse always — we need the AST for shape extraction.
    let mut stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;

    // DDL bypass — no caching, no shape extraction.
    if is_ddl_or_unsuitable_for_cache(&stmt) {
        let snap = self.current_snapshot();
        let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
        return execute_with_ctx(analyzed, &self.storage, &self.txn, &self.bloom, &mut self.session);
    }

    // Extract literals (rewrites stmt in place).
    let extracted = extract_literals(&mut stmt);
    let hash = shape_hash(&stmt);

    // Look up cache.
    let snap = self.current_snapshot();
    let mut reader = CatalogReader::new(&self.storage, snap.clone())?;

    let cached_analyzed = match self.session.get_cached_plan(hash, &mut reader)? {
        Some(plan) => {
            // Hit — clone the analyzed Stmt with Param nodes for substitution.
            if extracted.len() != plan.param_count {
                return Err(DbError::Internal {
                    message: format!(
                        "statement cache: literal count mismatch (expected {}, got {})",
                        plan.param_count, extracted.len()
                    ),
                });
            }
            plan.analyzed.clone()
        }
        None => {
            // Miss — analyze the shape AST, compute deps, cache.
            let analyzed = analyze_cached(stmt, &self.storage, snap.clone(), &mut self.schema_cache)?;
            let deps = extract_table_deps(&analyzed, &mut reader, DEFAULT_DATABASE_NAME)?;
            self.session.cache_plan(hash, CachedPlan {
                analyzed: analyzed.clone(),
                param_count: extracted.len(),
                deps,
            });
            analyzed
        }
    };

    // Substitute extracted literals back as Literals.
    let final_stmt = substitute_params(cached_analyzed, &extracted)?;
    execute_with_ctx(final_stmt, &self.storage, &self.txn, &self.bloom, &mut self.session)
}

fn is_ddl_or_unsuitable_for_cache(stmt: &Stmt) -> bool {
    matches!(stmt,
        Stmt::CreateTable(_) | Stmt::DropTable(_) | Stmt::AlterTable(_) |
        Stmt::CreateIndex(_) | Stmt::DropIndex(_) |
        Stmt::CreateSchema(_) | Stmt::DropSchema(_) |
        Stmt::CreateDatabase(_) | Stmt::DropDatabase(_) |
        Stmt::Begin | Stmt::Commit | Stmt::Rollback |
        Stmt::Savepoint(_) | Stmt::RollbackToSavepoint(_) | Stmt::ReleaseSavepoint(_)
        // ...etc; full list of non-cached statements
    )
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_statement_cache
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh test -p axiomdb-embedded   # PreparedStatement still works
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
./tools/vm.sh clippy axiomdb-embedded 2>&1 | tail -5

cargo build --release -p axiomdb-bench-comparison
for i in 1 2 3; do
    cargo run -p axiomdb-bench-comparison --release -- \
        --scenario insert_batch --rows 10000 --diagnose-insert | tail -5
done
# Expect execute_with_ctx per row ≤ 10 µs, throughput ≥ 100K rows/s.

cargo run -p axiomdb-bench-comparison --release -- --compare --rows 10000 | tail -15
# Expect insert_batch ratio ≤ 10× (was 49×).
```

### Decision tree (mirrors Attack 3.A's Step 4 pattern)

After running the bench:
- **≤ 10 µs/row AND ≥ 100K rows/s** → success. Skip Step 2.5 (text-cache
  fast path) and go to Step 2.6.
- **≤ 15 µs/row AND ≥ 70K rows/s** → close. Step 2.5 layers a text-cache
  on top to skip parse + walker entirely on identical SQL re-runs;
  expected to close most of the remaining gap.
- **> 15 µs/row** → STOP and revise. Profile with `--diagnose-insert-deep`
  to find the unexpected cost.

### Commit

```
feat(perf-sqlite-gap): step 2.4 — wire auto-cache into Db::run_inner

db.run(sql) now: parse → extract_literals → shape_hash → cache
lookup (with PlanDeps validation) → substitute_params → execute.

DDL statements bypass the cache entirely.

4 end-to-end integration tests: cache after first call, distinct
shapes, ALTER eviction, DDL non-caching.

Performance: execute_with_ctx per row dropped from 44 µs to ? µs;
INSERT throughput 21K → ?K rows/s.
```

---

## Step 2.5 — SQL-text fast path (CONDITIONAL — only if Step 2.4 didn't hit the budget)

**Goal:** When the same SQL text is run repeatedly (ORM with `?`
placeholders, or any literal-free repeat), skip even the parse + walker
cost.

**Files:**
- `crates/axiomdb-embedded/src/lib.rs` — add the text-cache check
- `crates/axiomdb-sql/src/session.rs` — small `text_to_shape_hash:
  HashMap<u64 (text_hash), u64 (shape_hash)>` to skip parse+walker

### Implementation outline

```rust
// In run_inner, BEFORE parse:
let text_hash = hash_of(sql.as_bytes());
if let Some(shape_hash) = self.session.text_to_shape_hash.get(&text_hash) {
    // We've seen this SQL text before — go straight to shape lookup.
    // Still need the extracted literals; but parsing is cheap (1.8 µs)
    // and we'd need to re-extract anyway. Hmm — is there even a point?
    // ...
}
```

Decision: defer Step 2.5 to a follow-up after measuring Step 2.4. If
parse is already <2 µs per call, the text-cache fast path saves only
that and the walker (~0.5 µs), total ~2.5 µs. Worth it only if Step 2.4
landed near the 10-µs ceiling.

### Verification

Same as Step 2.4 but specifically measure repeated-SQL scenarios.

### Commit

```
perf(perf-sqlite-gap): step 2.5 — text→shape fast path

Skip parse + literal walker on identical SQL text re-runs by caching
text_hash → shape_hash in SessionContext. Saves ~2.5 µs/call on
ORM-style workloads where the text is constant.
```

---

## Step 2.6 — Close: workspace gates + wire smoke + docs + final commit

**Goal:** Every spec done criterion verified.

### Verification against spec

- [ ] `axiomdb_bench --compare --rows 10000` shows
      `insert_batch ≥ 100K rows/s`.
- [ ] `--diagnose-insert` shows `execute_with_ctx per row ≤ 10 µs`.
- [ ] All edge-case tests from the spec exist and pass.
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (per memory pre-flight rule).
- [ ] Existing Phase 10.8 `PreparedStatement` tests pass — no
      regression on the manual prepared API.
- [ ] Rustdoc on every public item in `statement_cache.rs`.

### Docs to update

- `docs/perf-sqlite-gap.md` — new "Attack 2" section with before/after
  numbers.
- `memory/project_sqlite_baseline.md` — append post-Attack-2 row.

### Final commit

```
feat(perf-sqlite-gap): close Attack 2 — automatic prepared statements

Implements specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md
Plan: specs/fase-perf-sqlite-gap/plan-statement-fingerprinting.md

Results (axiomdb_bench --compare --rows 10000):
  insert_batch:        21K → ?K rows/s
  Gap vs SQLite:       49× → ?×
  execute_with_ctx/r:  44 µs → ? µs

Tests: ? new integration tests covering all spec edge cases.
All 4351+ workspace tests pass, clippy + fmt clean, wire smoke green.
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Literal walker misses an Expr variant with embedded literals | Medium | Walker mirrors `substitute_params` coverage — exact same variants; tests roundtrip on representative shapes. Missing variants degrade to cache miss (correct, just slower). |
| `Debug`-format hashing has cross-process instability or is slow | Low | Stable within a process — that's all we need. If slow, replace with a recursive Hash impl in a follow-up. |
| Hash collisions cause wrong plan reuse | Very low | 64-bit hash space, max 256 entries → birthday bound ~1.3e9; we'd need ~3 billion shapes for 50% collision. Plus PlanDeps validation acts as a secondary check. |
| `PlanDeps::is_stale` is more expensive than expected | Low | Already used by manual PreparedStatement; if it's slow there it's slow here. Profile shows it's ~1 µs. |
| LRU eviction by min-seq scan is O(n) — slows down with cache full | Medium | At 256 entries, O(256) iter is sub-µs. If profiling shows it, swap to BTreeMap or LinkedList. |
| `substitute_params` panics on extracted-count vs param-count mismatch | Low | Step 2.4 explicit check returns `DbError::Internal` instead of panicking. |
| Wire test (server path) breaks because run() flow changed | Medium | Wire path goes through Db::run_inner too; tests will catch. Step 2.6 explicit wire smoke. |
| Manual `PreparedStatement` (Phase 10.8) breaks | Low | Auto-cache lives in Db::run_inner; prepare() uses different code path. Step 2.4 explicit test on prepare(). |

## Rollback plan

1. Each step is its own commit. Single-step revert: `git revert <hash>`.
2. If the integration in Step 2.4 turns out wrong:
   `git revert <step 2.4 hash>` — Steps 2.1-2.3 remain as
   library-only additions, no behavior change.
3. If the whole attack is abandoned:
   `git branch abandoned/plan-statement-fingerprinting-2026-05-17`
   from the last clean commit; revert the spec to `draft` with a note.

## Estimated effort

Total: ~4-5 days.
- Step 2.1 (walker + 4 tests): 4 h
- Step 2.2 (shape_hash + 5 tests): 2 h
- Step 2.3 (CachedPlan + LRU + 4 tests): 4 h
- Step 2.4 (wire + 4 end-to-end tests + measure): 1 day
- Step 2.5 (text fast path, if needed): 0.5 day
- Step 2.6 (closing): 0.5 day
