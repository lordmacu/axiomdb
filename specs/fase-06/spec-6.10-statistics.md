# Spec: Index Statistics (Phases 6.10 + 6.11 + 6.12)

## What to build

A lightweight per-column statistics system that feeds the query planner with
enough data to choose between index scan and full table scan. Three tightly
coupled phases implemented together:

- **6.10** Bootstrap stats at `CREATE INDEX` and `CREATE TABLE` time.
- **6.11** In-memory staleness tracking — mark stats as stale when enough
  rows change, so the planner falls back to conservative defaults.
- **6.12** `ANALYZE [TABLE name [column]]` — manually refresh stats with an
  exact full-table scan.

Reference: PostgreSQL `pg_statistic` dual-encoding for NDV; Duj1 estimator for
large tables (deferred); `DEFAULT_NUM_DISTINCT = 200` fallback.

---

## What statistics are stored

Per-column, stored in the new `axiom_stats` catalog heap:

```rust
pub struct StatsDef {
    pub table_id:  u32,   // which table
    pub col_idx:   u16,   // which column (0-indexed)
    pub row_count: u64,   // total visible rows at last ANALYZE / CREATE INDEX
    /// Number of distinct non-NULL values.
    /// PostgreSQL dual-encoding:
    ///   > 0  →  absolute count  (e.g. 42 unique emails)
    ///   < 0  →  proportion × row_count  (reserved — Phase 6.10 always writes > 0)
    ///   = 0  →  unknown → planner uses DEFAULT_NUM_DISTINCT = 200
    pub ndv:       i64,
}
```

**Binary format (22 bytes fixed):**
```
[table_id: 4 LE][col_idx: 2 LE][row_count: 8 LE][ndv: 8 LE]
```

No sequence ID needed — rows are keyed by `(table_id, col_idx)` and replaced
on every ANALYZE. The heap scanner filters by `table_id`.

---

## Catalog: `axiom_stats` (6th system table)

New meta page offset: **96** (u64, after NEXT_FK_ID at 92+4=96).

```
CATALOG_STATS_ROOT_BODY_OFFSET = 96   // u64 LE — stats heap root page
```

Lazy-initialized (same pattern as `ensure_fk_root`): value = 0 on pre-6.10
databases. `CatalogBootstrap::ensure_stats_root()` allocates on first write.

### `CatalogWriter` new methods
- `upsert_stats(def: StatsDef) → Result<(), DbError>`
  Scans heap for existing row with `(table_id, col_idx)`; if found → MVCC-delete
  old and insert new. If not found → just insert. Atomic within the same txn.

### `CatalogReader` new methods
- `get_stats(table_id, col_idx) → Result<Option<StatsDef>, DbError>`
- `list_stats(table_id) → Result<Vec<StatsDef>, DbError>`

---

## Planner constants

```rust
/// Fraction of rows below which an index scan beats a full scan.
/// Based on PostgreSQL's default (seq_page_cost=1 / random_page_cost=4 ≈ 0.25).
/// AxiomDB uses 0.20 (slightly more conservative for embedded use).
const INDEX_SELECTIVITY_THRESHOLD: f64 = 0.20;

/// Default NDV when no statistics exist (PostgreSQL DEFAULT_NUM_DISTINCT).
const DEFAULT_NUM_DISTINCT: i64 = 200;
```

### Planner cost gate

Before returning `IndexLookup` or `IndexRange` in `plan_select`, load stats for
the first indexed column and compute selectivity:

```
ndv = stats.ndv > 0 ? stats.ndv : DEFAULT_NUM_DISTINCT
selectivity = 1.0 / max(ndv, 1)
if selectivity > INDEX_SELECTIVITY_THRESHOLD:
    return AccessMethod::Scan  // index not selective enough
```

**With no statistics:** default ndv = 200, selectivity = 0.005 → always use
index (conservative, never wrong, just suboptimal for very low-cardinality cols).

**Example decisions:**
- `WHERE status = 'active'` on table with ndv(status) = 3 → sel = 0.33 > 0.20 → Scan
- `WHERE email = 'x@y.com'` on table with ndv(email) = 10000 → sel = 0.0001 → Index
- `WHERE id = 42` on PK with ndv = row_count → sel ≈ 0 → Index

The planner also passes `row_count` from stats:
- If `row_count < 1000`: always use Scan (small table, index overhead not worth it)
- This avoids index lookups on tiny tables that change frequently.

---

## Phase 6.10 — Bootstrap stats at CREATE INDEX

### When stats are collected
1. **`execute_create_index`**: After building the B-Tree from existing rows,
   compute stats for each indexed column. The table has already been scanned
   (for B-Tree build) — reuse those rows.

2. **`execute_create_table`**: After creating PK/UNIQUE indexes on empty tables,
   write `StatsDef { row_count: 0, ndv: 0 }` for those columns (accurate: empty).

3. **`persist_fk_constraint`** (FK auto-index): After building the FK index,
   write stats for `child_col_idx`.

### NDV computation (Phase 6.10: exact count)

```rust
fn compute_ndv_exact(col_idx: u16, rows: &[(RecordId, Vec<Value>)]) -> i64 {
    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (_, row) in rows {
        let val = &row[col_idx as usize];
        if matches!(val, Value::Null) { continue; }
        if let Ok(key) = encode_index_key(&[val.clone()]) {
            seen.insert(key);
        }
    }
    seen.len() as i64
}
```

This is exact (not sampled). For Phase 6.10, tables at CREATE INDEX time are
rarely >100K rows. Sampling (Duj1 + reservoir) is deferred to Phase 6.15.

---

## Phase 6.11 — Staleness tracking (in-memory)

No persistent catalog changes. Pure in-memory bookkeeping.

### `StaleStats` in `SessionContext`

```rust
pub struct StaleStats {
    /// For each table: (inserts_since_analyze, deletes_since_analyze, last_analyzed_rows)
    counters: HashMap<u32, (u64, u64, u64)>,
    /// Tables marked as stale (planner uses defaults for these).
    stale: HashSet<u32>,
}
```

### Update logic (called in executor after every INSERT/DELETE row)

```
on_row_change(table_id, delta_rows):
  (ins, del, last) = counters.get_or_default(table_id)
  total_change = ins + del + delta_rows
  if last > 0 && total_change > 0.20 * last:
    stale.insert(table_id)
  counters[table_id] = update(...)
```

### Planner uses staleness

Before computing selectivity in `plan_select`:
```
if ctx.stats.is_stale(table_id):
    // Use conservative default — do not consult catalog stats
    ndv = DEFAULT_NUM_DISTINCT
else:
    // Load from catalog
    ndv = reader.get_stats(table_id, col_idx)?.map(|s| s.ndv).unwrap_or(0)
```

After a successful `ANALYZE TABLE t`, the staleness counter is reset for `t`.

---

## Phase 6.12 — ANALYZE command

### SQL syntax

```sql
ANALYZE;                          -- all tables in current schema
ANALYZE TABLE users;              -- specific table, all indexed columns
ANALYZE TABLE users (email);      -- specific column only
```

### AST

```rust
pub struct AnalyzeStmt {
    /// `None` = all tables; `Some(name)` = specific table.
    pub table: Option<String>,
    /// `None` = all indexed columns; `Some(name)` = specific column.
    pub column: Option<String>,
}
```

### Execution

1. Load table def + columns + indexes from catalog.
2. Scan the entire table (MVCC-visible rows).
3. For each target column (all or specific):
   - Count rows → `row_count`
   - Compute NDV exactly → `ndv`
   - `writer.upsert_stats(StatsDef { table_id, col_idx, row_count, ndv })`
4. Reset staleness counter for this table in `SessionContext`.
5. Return `QueryResult::Empty`.

### Token and parser

Add `Token::Analyze` to the lexer.
Parse after `ANALYZE` keyword: optional `TABLE name` then optional `(col_name)`.

---

## Use cases

### 1. CREATE INDEX bootstraps stats
```sql
CREATE TABLE orders (id INT PRIMARY KEY, status TEXT, user_id INT);
INSERT INTO orders SELECT ... (10000 rows, status has 3 distinct values)
CREATE INDEX idx_status ON orders(status);
-- Stats: row_count=10000, ndv(status)=3

SELECT * FROM orders WHERE status = 'active';
-- selectivity = 1/3 = 0.33 > 0.20 → planner chooses full scan ✅
-- (correct: 33% of rows match, index not beneficial)

SELECT * FROM orders WHERE user_id = 42;
-- no stats for user_id (no index) → falls to rule-based → Scan
```

### 2. Selective column uses index
```sql
CREATE INDEX idx_email ON users(email);
-- Stats: row_count=1000000, ndv(email)=999000

SELECT * FROM users WHERE email = 'alice@example.com';
-- selectivity = 1/999000 = 0.000001 < 0.20 → IndexLookup ✅
```

### 3. ANALYZE refreshes stale stats
```sql
-- Initial: table has 100 rows, ndv(status) = 3
INSERT INTO orders VALUES ... (+ 25 rows = 25% change → stale!)
-- Planner now uses DEFAULT_NUM_DISTINCT = 200 for status (conservative)
ANALYZE TABLE orders;
-- Stats refreshed: row_count=125, ndv(status)=3
-- Staleness cleared: planner uses real stats again
```

### 4. Small table always uses scan
```sql
CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT);
-- 5 rows (row_count < 1000)
SELECT * FROM config WHERE key = 'max_connections';
-- row_count < 1000 → always Scan regardless of selectivity ✅
```

---

## Acceptance criteria

- [ ] `axiom_stats` heap root at meta offset 96 (lazy-initialized, backward-compat)
- [ ] `StatsDef` serializes/deserializes correctly (22-byte format)
- [ ] `CREATE INDEX` bootstraps stats: correct `row_count` and `ndv` per indexed col
- [ ] `CREATE TABLE` with PK/UNIQUE bootstraps stats with row_count=0, ndv=0
- [ ] Planner: `WHERE status = 'active'` on 3-distinct column → Scan (sel > 0.20)
- [ ] Planner: `WHERE email = 'x@y.com'` on 10K-distinct column → IndexLookup
- [ ] Planner: table with <1000 rows → always Scan
- [ ] Without stats: planner still works (uses DEFAULT_NUM_DISTINCT = 200)
- [ ] `ANALYZE TABLE t` updates stats in catalog
- [ ] `ANALYZE TABLE t (col)` updates only that column's stats
- [ ] `ANALYZE` alone updates all tables in schema
- [ ] Staleness counter: after 20% row change → planner uses defaults
- [ ] Staleness resets after ANALYZE
- [ ] Pre-6.10 databases open without error (stats root = 0 → lazy init)
- [ ] Integration tests covering all acceptance criteria

---

## Out of scope

- Histogram bins (range query selectivity) → Phase 6.15
- MCV (most common values) tracking → Phase 6.15
- Reservoir sampling for large tables (Duj1 estimator) → Phase 6.15
- Join selectivity estimation → Phase 6.15
- Per-index statistics (correlation) → Phase 6.15

## ⚠️ DEFERRED
- Histogram/MCV/sampling → Phase 6.15
- Statistics for JOIN optimization → Phase 6.15

---

## Dependencies

- Phase 6.1–6.3: `IndexDef`, `execute_create_index` (stats bootstrapped here)
- Phase 6.9: `CatalogBootstrap` has 5 system tables; stats adds the 6th
- `encode_index_key` — used for exact NDV counting
- `plan_select` — planner cost gate added here
