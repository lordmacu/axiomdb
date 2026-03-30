# Plan: 7.11 — Basic MVCC Vacuum

## Files to create/modify

| File | Action | Purpose |
|------|--------|---------|
| `crates/axiomdb-sql/src/ast.rs` | Modify | Add `Stmt::Vacuum(VacuumStmt)` |
| `crates/axiomdb-sql/src/parser/mod.rs` | Modify | Parse `VACUUM [table_name]` |
| `crates/axiomdb-sql/src/vacuum.rs` | **Create** | `vacuum_table()`, `vacuum_heap_page()` |
| `crates/axiomdb-sql/src/index_maintenance.rs` | Modify | Add `vacuum_index()` |
| `crates/axiomdb-sql/src/executor/mod.rs` | Modify | Dispatch `Stmt::Vacuum` |
| `crates/axiomdb-sql/src/lib.rs` | Modify | Add `pub mod vacuum;` |
| `docs-site/src/internals/mvcc.md` | Modify | Add vacuum section |
| `docs/progreso.md` | Modify | Mark 7.11 |

---

## Data Structures

### AST

```rust
// In ast.rs
pub struct VacuumStmt {
    /// None = vacuum all tables in current database.
    pub table: Option<TableRef>,
}

// Add to enum Stmt:
Vacuum(VacuumStmt),
```

### Vacuum result

```rust
// In vacuum.rs
pub struct VacuumResult {
    pub table_name: String,
    pub dead_rows_removed: u64,
    pub dead_index_entries_removed: u64,
}
```

---

## Algorithm

### 1. `vacuum_table` — main entry point

```rust
pub fn vacuum_table(
    table_name: &str,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: TransactionSnapshot,
) -> Result<VacuumResult, DbError> {
    // 1. Resolve table (get TableDef, columns, indexes)
    // 2. Determine oldest_safe_txn = txn.max_committed() + 1
    // 3. Heap vacuum: walk chain, mark dead slots
    // 4. Index vacuum: for each non-unique non-FK secondary index, clean dead entries
    // 5. Return statistics
}
```

### 2. `vacuum_heap_chain` — heap cleanup

```rust
fn vacuum_heap_chain(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
    oldest_safe_txn: u64,
) -> Result<u64, DbError> {
    let mut dead_count = 0u64;
    let mut page_id = root_page_id;

    while page_id != 0 {
        let raw = *storage.read_page(page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        let n = num_slots(&page);
        let mut page_modified = false;

        for slot_id in 0..n {
            let entry = read_slot(&page, slot_id);
            if entry.is_dead() {
                continue; // already vacuumed
            }
            // Read only the RowHeader (24 bytes) — skip the full tuple
            match read_tuple_header(&page, slot_id)? {
                None => continue,
                Some(txn_id_deleted) => {
                    if txn_id_deleted != 0 && txn_id_deleted < oldest_safe_txn {
                        mark_slot_dead(&mut page, slot_id)?;
                        dead_count += 1;
                        page_modified = true;
                    }
                }
            }
        }

        if page_modified {
            page.update_checksum();
            storage.write_page(page_id, &page)?;
        }

        page_id = chain_next_page(&page);
    }

    Ok(dead_count)
}
```

**Key optimization:** Use `read_tuple_header()` (reads only `txn_id_deleted`, 8 bytes)
instead of `read_tuple()` (reads full header + data). Avoids decoding row payload.

### 3. `vacuum_index` — index cleanup

```rust
pub fn vacuum_index(
    storage: &mut dyn StorageEngine,
    index: &IndexDef,
    snap: TransactionSnapshot,
) -> Result<u64, DbError> {
    // Collect all entries from the index
    let all_entries = BTree::range_in(storage, index.root_page_id, None, None)?;

    // Find dead entries (heap row not visible)
    let mut dead_keys: Vec<Vec<u8>> = Vec::new();
    for (rid, key_bytes) in &all_entries {
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
            dead_keys.push(key_bytes.clone());
        }
    }

    if dead_keys.is_empty() {
        return Ok(0);
    }

    // Batch delete dead entries
    dead_keys.sort_unstable();
    let root_pid = AtomicU64::new(index.root_page_id);
    BTree::delete_many_in(storage, &root_pid, &dead_keys)?;

    Ok(dead_keys.len() as u64)
}
```

**Only vacuum non-unique, non-FK secondary indexes.** Unique/PK/FK indexes
already have their entries deleted immediately in DELETE/UPDATE (Phase 7.3b).

### 4. Parser — `VACUUM [table_name]`

```rust
// In parser, after matching keyword "VACUUM":
fn parse_vacuum(&mut self) -> Result<Stmt, DbError> {
    // VACUUM
    // VACUUM table_name
    // VACUUM schema.table_name
    let table = if self.peek_is_end_or_semicolon() {
        None
    } else {
        Some(self.parse_table_ref()?)
    };
    Ok(Stmt::Vacuum(VacuumStmt { table }))
}
```

### 5. Executor dispatch

```rust
// In executor mod.rs, dispatch_ctx or equivalent:
Stmt::Vacuum(stmt) => {
    let results = crate::vacuum::execute_vacuum(stmt, storage, txn, bloom, ctx)?;
    // Format as QueryResult::Rows with columns: table, dead_rows, dead_index_entries
    format_vacuum_results(results)
}
```

### 6. `execute_vacuum` — orchestrator

```rust
pub fn execute_vacuum(
    stmt: VacuumStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<Vec<VacuumResult>, DbError> {
    let snap = txn.active_snapshot()?;

    let tables = if let Some(ref table_ref) = stmt.table {
        // Single table
        vec![resolve_table(storage, txn, ctx, table_ref)?]
    } else {
        // All tables in current database
        list_all_tables(storage, txn, ctx)?
    };

    let mut results = Vec::new();
    for table in &tables {
        let result = vacuum_table(&table.def, &table.columns, &table.indexes,
                                   storage, txn, snap, bloom)?;
        results.push(result);
    }

    Ok(results)
}
```

---

## Implementation Phases

### Phase 1: AST + Parser

1. Add `VacuumStmt { table: Option<TableRef> }` to `ast.rs`
2. Add `Stmt::Vacuum(VacuumStmt)` variant
3. Parse `VACUUM` keyword in parser (with optional table name)
4. Test: parse `VACUUM` → `Stmt::Vacuum { table: None }`
5. Test: parse `VACUUM orders` → `Stmt::Vacuum { table: Some(...) }`

### Phase 2: Heap vacuum

6. Create `crates/axiomdb-sql/src/vacuum.rs`
7. Implement `vacuum_heap_chain()` — walk pages, mark dead slots
8. Add `pub mod vacuum;` to `lib.rs`
9. Test: insert rows, delete some, vacuum → dead slots zeroed
10. Test: vacuum with no dead rows → 0 removed
11. Test: recently deleted rows (txn_id_deleted >= oldest_safe) NOT vacuumed

### Phase 3: Index vacuum

12. Add `vacuum_index()` to `index_maintenance.rs`
13. Test: insert rows with non-unique index, delete, vacuum → dead entries removed
14. Test: unique index entries NOT touched by vacuum (already deleted in 7.3b)

### Phase 4: Executor + orchestration

15. Add `execute_vacuum()` in `vacuum.rs`
16. Dispatch `Stmt::Vacuum` in executor mod.rs (both ctx and non-ctx paths)
17. Format results as `QueryResult::Rows`
18. Test: `VACUUM orders` via executor → returns statistics
19. Test: `VACUUM` (all tables) via executor
20. Test: `VACUUM nonexistent` → TableNotFound

### Phase 5: Documentation + close

21. Update `docs-site/src/internals/mvcc.md` — add vacuum section
22. Update `docs/progreso.md` — mark 7.11

---

## Tests to Write

### Unit tests (vacuum.rs)

```
test_vacuum_heap_marks_dead_slots
  — insert 5 rows, delete 3, vacuum → 3 slots dead, 2 alive

test_vacuum_heap_preserves_live_rows
  — insert 5 rows, vacuum → 0 removed

test_vacuum_heap_preserves_recent_deletes
  — delete rows but txn_id_deleted >= oldest_safe → not vacuumed

test_vacuum_index_removes_dead_entries
  — insert with non-unique index, delete, vacuum → entries removed from B-Tree

test_vacuum_index_preserves_live_entries
  — all rows alive → 0 entries removed

test_vacuum_skips_unique_indexes
  — unique index entries already deleted in 7.3b → vacuum doesn't touch them
```

### Integration tests (executor)

```
test_vacuum_single_table
  — CREATE TABLE, INSERT, DELETE, VACUUM table → statistics returned

test_vacuum_all_tables
  — CREATE 2 tables, INSERT+DELETE in both, VACUUM → both vacuumed

test_vacuum_nonexistent_table
  — VACUUM nonexistent → TableNotFound

test_vacuum_empty_table
  — VACUUM on empty table → 0 removed

test_vacuum_after_update_cleans_old_index_entries
  — UPDATE indexed col, VACUUM → old lazy-deleted index entry removed
```

---

## Anti-patterns to Avoid

- **DO NOT** vacuum rows where `txn_id_deleted >= oldest_safe_txn`. These rows
  might still be visible to an older snapshot (future concurrent readers).

- **DO NOT** touch unique/PK/FK index entries during vacuum. They were already
  deleted immediately in Phase 7.3b. Attempting to vacuum them would be wasted
  work and could interfere with the B-Tree.

- **DO NOT** run vacuum inside a transaction (`BEGIN ... VACUUM ... COMMIT`).
  Vacuum should be a standalone operation. If called inside an explicit txn,
  raise an error or warn.

- **DO NOT** rewrite pages that have no dead slots. Only read+write pages that
  actually have dead rows to minimize I/O.

- **DO NOT** hold all dead keys in memory for large tables. Process one index at
  a time, and within each index, batch the deletes. For very large indexes,
  consider chunking (future optimization).

---

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Vacuum of a large table blocks all queries | Long pause | Acceptable for manual VACUUM; autovacuum (7.11c) will be incremental |
| Dead key collection OOMs for huge indexes | Process crash | Bounded by index size; future: streaming scan with chunked deletes |
| Vacuum during active transaction | Incorrect oldest_safe_txn | Under RwLock: impossible (exclusive access). Document for future. |
| mark_slot_dead races with concurrent read | Torn slot | Under RwLock: impossible. Future concurrent: needs page-level lock. |
