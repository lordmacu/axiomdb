# Plan: Index Statistics (Phases 6.10 + 6.11 + 6.12)

## Files to create / modify

### Create
- `crates/axiomdb-sql/tests/integration_stats.rs` — all stats tests

### Modify
- `crates/axiomdb-storage/src/meta.rs` — `CATALOG_STATS_ROOT_BODY_OFFSET = 96`
- `crates/axiomdb-storage/src/lib.rs` — re-export new constant
- `crates/axiomdb-catalog/src/bootstrap.rs` — `CatalogPageIds.stats`, `ensure_stats_root()`
- `crates/axiomdb-catalog/src/schema.rs` — `StatsDef` struct + serde
- `crates/axiomdb-catalog/src/writer.rs` — `upsert_stats()`, `SYSTEM_TABLE_STATS`
- `crates/axiomdb-catalog/src/reader.rs` — `get_stats()`, `list_stats()`
- `crates/axiomdb-catalog/src/lib.rs` — re-export `StatsDef`
- `crates/axiomdb-sql/src/session.rs` — `StaleStatsTracker` in `SessionContext`
- `crates/axiomdb-sql/src/planner.rs` — cost gate with stats
- `crates/axiomdb-sql/src/executor.rs` — stats bootstrap + ANALYZE + staleness updates
- `crates/axiomdb-sql/src/ast.rs` — `AnalyzeStmt`, `Stmt::Analyze`
- `crates/axiomdb-sql/src/lexer.rs` — `Token::Analyze`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `parse_analyze`

---

## Algorithm / Data structures

### `StatsDef` binary format (22 bytes)

```
[table_id:  4 bytes LE u32]
[col_idx:   2 bytes LE u16]
[row_count: 8 bytes LE u64]
[ndv:       8 bytes LE i64]   positive=absolute count, negative=proportion (reserved)
```

### `compute_ndv_exact` (executor helper)

```rust
fn compute_ndv_exact(col_idx: u16, rows: &[(RecordId, Vec<Value>)]) -> i64 {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for (_, row) in rows {
        let val = row.get(col_idx as usize).unwrap_or(&Value::Null);
        if matches!(val, Value::Null) { continue; }
        if let Ok(key) = encode_index_key(std::slice::from_ref(val)) {
            seen.insert(key);
        }
    }
    seen.len() as i64
}
```

### `StaleStatsTracker` in `SessionContext`

```rust
#[derive(Debug, Default)]
pub struct StaleStatsTracker {
    /// Changes per table since last stats baseline was set.
    changes: HashMap<u32, u64>,
    /// Row count at last stats load (from catalog statsDef.row_count).
    baseline: HashMap<u32, u64>,
    /// Tables currently considered stale.
    stale: HashSet<u32>,
}

impl StaleStatsTracker {
    /// Called after each row INSERT or DELETE.
    pub fn on_row_changed(&mut self, table_id: u32) {
        *self.changes.entry(table_id).or_insert(0) += 1;
        self.check_stale(table_id);
    }
    /// Called by planner after loading stats from catalog.
    pub fn set_baseline(&mut self, table_id: u32, row_count: u64) {
        self.baseline.insert(table_id, row_count);
        self.check_stale(table_id);
    }
    /// Called after ANALYZE.
    pub fn mark_fresh(&mut self, table_id: u32) {
        self.stale.remove(&table_id);
        self.changes.remove(&table_id);
    }
    pub fn is_stale(&self, table_id: u32) -> bool {
        self.stale.contains(&table_id)
    }
    fn check_stale(&mut self, table_id: u32) {
        if let (Some(&changes), Some(&baseline)) =
            (self.changes.get(&table_id), self.baseline.get(&table_id))
        {
            if baseline > 0 && changes > baseline / 5 {  // > 20%
                self.stale.insert(table_id);
            }
        }
    }
}
```

### Planner cost gate

New `plan_select` signature:

```rust
pub fn plan_select(
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_stats: &[StatsDef],     // NEW — empty = no stats available
    table_id: u32,                // NEW — for staleness check
    stale_tracker: &mut StaleStatsTracker,  // NEW
) -> AccessMethod
```

Inside `plan_select`, after finding an index candidate (before returning IndexLookup/IndexRange):

```rust
fn stats_cost_gate(
    index_def: &IndexDef,
    columns: &[ColumnDef],
    table_stats: &[StatsDef],
    table_id: u32,
    stale_tracker: &mut StaleStatsTracker,
) -> bool {
    // Returns true if index should be used; false → caller returns Scan instead.

    // Get stats for the first indexed column.
    let col_idx = match index_def.columns.first() {
        Some(c) => c.col_idx,
        None => return true, // no column info → use index (conservative)
    };

    let (row_count, ndv) = match table_stats.iter().find(|s| s.col_idx == col_idx) {
        Some(s) => {
            // Set baseline for staleness tracking.
            stale_tracker.set_baseline(table_id, s.row_count);
            let ndv = if s.ndv > 0 { s.ndv } else { DEFAULT_NUM_DISTINCT };
            (s.row_count, ndv)
        }
        None => {
            // No stats → assume ndv=200 (conservative; always use index).
            return true;
        }
    };

    // Small table heuristic: < 1000 rows → full scan cheaper.
    if row_count < 1000 {
        return false;
    }

    // Selectivity = 1/NDV for equality predicate.
    let selectivity = 1.0 / (ndv.max(1) as f64);
    selectivity <= INDEX_SELECTIVITY_THRESHOLD  // true → use index
}
```

---

## Implementation phases

### Phase 1 — Catalog foundation

**Step 1.1** — `meta.rs`: Add `CATALOG_STATS_ROOT_BODY_OFFSET: usize = 96`.
Add const assert. Export from `lib.rs`.

**Step 1.2** — `schema.rs`: Add `StatsDef` struct with `to_bytes()` / `from_bytes()`.
Add unit tests in `schema.rs`.

**Step 1.3** — `bootstrap.rs`:
- Add `pub stats: u64` to `CatalogPageIds`
- Update `init()` to allocate stats root
- Update `page_ids()` to read offset 96
- Add `ensure_stats_root(storage) -> Result<u64, DbError>`

**Step 1.4** — `writer.rs`:
- Add `SYSTEM_TABLE_STATS: u32 = u32::MAX - 5`
- Add `upsert_stats(&mut self, def: StatsDef) -> Result<(), DbError>`:
  Scan stats heap for existing row with `(table_id, col_idx)`. If found → MVCC-delete.
  Then insert new row. Both in same txn.

**Step 1.5** — `reader.rs`:
- Add `get_stats(table_id, col_idx) -> Result<Option<StatsDef>, DbError>`
- Add `list_stats(table_id) -> Result<Vec<StatsDef>, DbError>`

**Step 1.6** — `lib.rs`: Re-export `StatsDef`.

**Verify:** `cargo build -p axiomdb-catalog` clean. Catalog tests pass.

---

### Phase 2 — SessionContext staleness tracker

**Step 2.1** — `session.rs`:
Add `StaleStatsTracker` struct with `changes`, `baseline`, `stale` fields.
Add `stats: StaleStatsTracker` field to `SessionContext`.
Expose methods: `on_row_changed`, `set_baseline`, `mark_fresh`, `is_stale`.

**Verify:** `cargo build -p axiomdb-sql` clean. No regressions.

---

### Phase 3 — Planner cost gate

**Step 3.1** — `planner.rs`:
Add constants `INDEX_SELECTIVITY_THRESHOLD: f64 = 0.20` and `DEFAULT_NUM_DISTINCT: i64 = 200`.
Add `stats_cost_gate()` private function.
Update `plan_select` signature to accept `table_stats: &[StatsDef]` and
`stale_tracker: &mut StaleStatsTracker`.

Note: `plan_select` is currently a pure function taking only `where_clause`,
`indexes`, `columns`. Adding mutable state (`stale_tracker`) is a signature
change. All call sites must be updated.

**Step 3.2** — `executor.rs` in `execute_select_ctx`:
Before calling `plan_select`, load stats from catalog:
```rust
let table_stats: Vec<StatsDef> = {
    let mut reader = CatalogReader::new(storage, snap)?;
    reader.list_stats(resolved.def.id).unwrap_or_default()
};
let access_method = crate::planner::plan_select(
    stmt.where_clause.as_ref(),
    &resolved.indexes,
    &resolved.columns,
    &table_stats,
    resolved.def.id,
    &mut ctx.stats,
);
```

**Verify:** `cargo test -p axiomdb-sql` — all existing tests pass (no regressions).

---

### Phase 4 — Bootstrap stats (6.10)

**Step 4.1** — `executor.rs` in `execute_create_index`:
After the existing row scan + B-Tree build (step 5 in the function), before
persisting the IndexDef:
```rust
// Bootstrap stats for each indexed column.
for idx_col in &index_columns {
    let ndv = compute_ndv_exact(idx_col.col_idx, &rows);
    CatalogWriter::new(storage, txn)?.upsert_stats(StatsDef {
        table_id: table_def.id,
        col_idx: idx_col.col_idx,
        row_count: rows.len() as u64,
        ndv,
    })?;
}
```

**Step 4.2** — `executor.rs` in `execute_create_table` (PK/UNIQUE index creation):
After the `create_empty_index` call for each PK/UNIQUE column, write empty stats:
```rust
CatalogWriter::new(storage, txn)?.upsert_stats(StatsDef {
    table_id,
    col_idx: pk_col as u16,
    row_count: 0, // empty table at creation
    ndv: 0,
})?;
```

**Step 4.3** — `executor.rs` in `persist_fk_constraint`:
After FK auto-index creation:
```rust
CatalogWriter::new(storage, txn)?.upsert_stats(StatsDef {
    table_id: child_table_id,
    col_idx: child_col_idx,
    row_count: rows.len() as u64,
    ndv: compute_ndv_exact(child_col_idx, &rows),
})?;
```

**Verify:** `cargo test --workspace` — all tests pass including existing FK/index tests.

---

### Phase 5 — ANALYZE command (6.12)

**Step 5.1** — `lexer.rs`: Add `Analyze,` to the `Token` enum (keyword).

**Step 5.2** — `ast.rs`: Add:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStmt {
    pub table: Option<String>,   // None = all tables
    pub column: Option<String>,  // None = all indexed columns
}
// Add Stmt::Analyze(AnalyzeStmt) variant
```

**Step 5.3** — `parser/ddl.rs`: Add `parse_analyze` function:
```
ANALYZE [ TABLE table_name [ ( column_name ) ] ]
```

**Step 5.4** — `executor.rs`:
Add `execute_analyze(stmt: AnalyzeStmt, storage, txn, ctx)` function.

Algorithm:
1. Get list of target tables (all in schema OR single named table).
2. For each table:
   a. Resolve table def + columns.
   b. Determine target col_idx list (all indexed columns OR single named col).
   c. Scan table once → collect all rows.
   d. For each target col_idx: compute `row_count` and `ndv`.
   e. `writer.upsert_stats(StatsDef { ... })?`
   f. `ctx.stats.mark_fresh(table_id)` — clear staleness.
3. Return `QueryResult::Empty`.

Add `Stmt::Analyze(s) => execute_analyze(s, storage, txn, ctx)` to `dispatch_ctx`.

---

### Phase 6 — Staleness updates (6.11)

**Step 6.1** — `executor.rs` in `execute_insert_ctx`:
After each successful `TableEngine::insert_row` (and after INSERT SELECT rows),
call `ctx.stats.on_row_changed(resolved.def.id)`.

**Step 6.2** — `executor.rs` in `execute_delete_ctx`:
After `TableEngine::delete_rows_batch`, call `ctx.stats.on_row_changed(resolved.def.id)`
once per deleted row:
```rust
for _ in 0..count {
    ctx.stats.on_row_changed(resolved.def.id);
}
```

**Step 6.3** — `executor.rs` in `execute_update_ctx`:
UPDATE is conceptually DELETE + INSERT (same row count change is zero). No
staleness update needed for UPDATE (row count doesn't change).

**Verify:** `cargo test --workspace` passes. Staleness tests pass.

---

## Tests to write

### Catalog unit tests (in `schema.rs`)
```rust
fn test_stats_def_roundtrip()
fn test_stats_def_ndv_positive()
fn test_stats_def_ndv_zero()
```

### Integration tests (`tests/integration_stats.rs`)

**Phase 6.10 — bootstrap:**
```rust
fn test_create_index_bootstraps_stats()        // ndv and row_count stored
fn test_create_table_bootstraps_empty_stats()  // row_count=0, ndv=0
fn test_stats_ndv_exact_count()               // correct distinct count
fn test_stats_null_values_excluded_from_ndv() // NULLs not counted
fn test_stats_single_distinct_value()         // ndv=1 when all same
```

**Planner cost gate:**
```rust
fn test_planner_uses_scan_for_low_cardinality()  // ndv=3 on 10K rows → sel=0.33 > 0.20 → Scan
fn test_planner_uses_index_for_high_cardinality()// ndv=9000 on 10K rows → sel=0.0001 → Index
fn test_planner_uses_scan_for_small_table()      // row_count < 1000 → always Scan
fn test_planner_uses_index_without_stats()       // no stats → uses index (conservative)
fn test_planner_uses_scan_for_stale_stats_low_cardinality() // stale → default ndv=200
```

**Phase 6.12 — ANALYZE:**
```rust
fn test_analyze_table_updates_stats()
fn test_analyze_table_column_updates_one_column()
fn test_analyze_clears_staleness()
fn test_analyze_empty_table()
```

**Phase 6.11 — staleness:**
```rust
fn test_staleness_triggered_after_20_percent_inserts()
fn test_staleness_cleared_after_analyze()
fn test_staleness_not_triggered_below_threshold()
```

**Backward compatibility:**
```rust
fn test_pre_610_database_opens_without_stats_root()  // stats root = 0 → lazy init
fn test_planner_works_without_stats()                // no axiom_stats → conservative
```

---

## Anti-patterns to avoid

- **DO NOT** compute NDV using `HashSet<Value>` — use `HashSet<Vec<u8>>` (encoded
  key bytes). Values are not `Hash`-able directly due to `f64`.
- **DO NOT** call `upsert_stats` inside hot INSERT/DELETE loops — only at
  CREATE INDEX and ANALYZE time. Stats are batch-computed, not per-row.
- **DO NOT** make `plan_select` read from storage directly — stats are loaded
  once per query in `execute_select_ctx` and passed as `&[StatsDef]`. This
  keeps the planner pure and testable.
- **DO NOT** fail a DDL operation if stats write fails — stats are advisory.
  Use `let _ = writer.upsert_stats(...)` or log a warning.
- **DO NOT** mark stats stale on UPDATE — row count is unchanged. Only INSERT
  and DELETE affect row count.

## Risks

| Risk | Mitigation |
|------|-----------|
| `upsert_stats` double-delete if called twice in same txn | MVCC-delete is idempotent; second delete finds row already deleted |
| Planner rejects valid index due to wrong NDV | Default to index when stats absent; ndv=0 → use DEFAULT 200 → sel=0.005 → always index |
| `StaleStatsTracker` baseline never set if ANALYZE never run | `set_baseline` only called when stats exist in catalog; if absent → is_stale returns false |
| Stats not updated when `CREATE INDEX` errors mid-way | `upsert_stats` inside same txn as index create → rollback cleans both |
