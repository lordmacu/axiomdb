# Plan: Bloom Filter por índice (Subfase 6.4)

## Files to create / modify

| File | Action | What |
|------|--------|------|
| `Cargo.toml` (workspace) | modify | Add `bloomfilter = "3"` to `[workspace.dependencies]` |
| `crates/axiomdb-sql/Cargo.toml` | modify | Add `bloomfilter = { workspace = true }` |
| `crates/axiomdb-sql/src/bloom.rs` | create | `BloomRegistry` + `IndexBloom` |
| `crates/axiomdb-sql/src/lib.rs` | modify | `pub mod bloom;` + re-export `BloomRegistry` |
| `crates/axiomdb-sql/src/executor.rs` | modify | New param, populate/check/mark_dirty/remove integration |
| `crates/axiomdb-sql/src/index_maintenance.rs` | modify | `insert_into_indexes` → add; `delete_from_indexes` → mark_dirty |
| `crates/axiomdb-network/src/mysql/database.rs` | modify | `bloom: BloomRegistry` in struct + pass to executor |
| `docs-site/src/internals/btree.md` | modify | Add Bloom filter section |
| `docs-site/src/user-guide/features/indexes.md` | modify | Mention automatic Bloom filter |

---

## Step-by-step implementation

### Step 1 — Add `bloomfilter` crate

In root `Cargo.toml` `[workspace.dependencies]`:
```toml
bloomfilter = "3"
```

In `crates/axiomdb-sql/Cargo.toml` `[dependencies]`:
```toml
bloomfilter = { workspace = true }
```

### Step 2 — `BloomRegistry` struct

```rust
// axiomdb-sql/src/bloom.rs
use bloomfilter::Bloom;
use std::collections::HashMap;

pub struct BloomRegistry {
    filters: HashMap<u32, IndexBloom>,
}

struct IndexBloom {
    filter: Bloom<Vec<u8>>,
    dirty: bool,
}

impl BloomRegistry {
    pub fn new() -> Self {
        Self { filters: HashMap::new() }
    }

    pub fn create(&mut self, index_id: u32, expected_items: usize) {
        let n = expected_items.max(1000).saturating_mul(2);
        let filter = Bloom::new_for_fp_rate(n, 0.01);
        self.filters.insert(index_id, IndexBloom { filter, dirty: false });
    }

    pub fn add(&mut self, index_id: u32, key: &[u8]) {
        if let Some(ib) = self.filters.get_mut(&index_id) {
            ib.filter.set(&key.to_vec());
        }
    }

    pub fn might_exist(&self, index_id: u32, key: &[u8]) -> bool {
        match self.filters.get(&index_id) {
            None => true,  // no filter → conservative: assume might exist
            Some(ib) => ib.filter.check(&key.to_vec()),
        }
    }

    pub fn mark_dirty(&mut self, index_id: u32) {
        if let Some(ib) = self.filters.get_mut(&index_id) {
            ib.dirty = true;
        }
    }

    pub fn remove(&mut self, index_id: u32) {
        self.filters.remove(&index_id);
    }

    pub fn len(&self) -> usize { self.filters.len() }
    pub fn is_empty(&self) -> bool { self.filters.is_empty() }
}

impl Default for BloomRegistry {
    fn default() -> Self { Self::new() }
}
```

### Step 3 — Update `execute_with_ctx` signature

```rust
// axiomdb-sql/src/executor.rs (and lib.rs re-export)

pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,     // ← new
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

Internal recursive calls (subqueries use `execute_select_ctx` / `dispatch` — these call `execute_with_ctx` too; pass `bloom` through):
```rust
// Inside executor.rs wherever execute_with_ctx is called internally:
execute_with_ctx(stmt, storage, txn, bloom, ctx)
```

### Step 4 — Update `Database` struct and call sites

```rust
// axiomdb-network/src/mysql/database.rs
use axiomdb_sql::bloom::BloomRegistry;

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    pub bloom: BloomRegistry,   // ← new
}

impl Database {
    pub fn open(data_dir: &Path) -> Result<Self, DbError> {
        // ... existing code ...
        Ok(Self { storage, txn, bloom: BloomRegistry::new() })
    }

    pub fn execute_query(&mut self, sql, session, schema_cache) -> Result<QueryResult, DbError> {
        // ...
        execute_with_ctx(analyzed, &mut self.storage, &mut self.txn, &mut self.bloom, session)
    }

    pub fn execute_stmt(&mut self, stmt, session) -> Result<QueryResult, DbError> {
        execute_with_ctx(stmt, &mut self.storage, &mut self.txn, &mut self.bloom, session)
    }
}
```

### Step 5 — Populate bloom in `execute_create_index`

After the B-Tree is built (at the end of the table scan loop):

```rust
// In execute_create_index, after Step 3 (build index_columns):
let row_count = rows.len();

// ... (existing table scan + BTree::insert_in loop) ...

// After loop: register bloom filter for this index.
// Use 2× row_count as the expected_items for growth headroom.
bloom.create(new_index_id, row_count);

// Re-scan the existing rows to add all keys to the bloom filter.
// Note: we already have the rows Vec from the scan above — iterate again.
for (_, row_vals) in &rows {
    let key_vals: Vec<Value> = index_columns.iter()
        .map(|ic| row_vals[ic.col_idx as usize].clone())
        .collect();
    if key_vals.iter().any(|v| matches!(v, Value::Null)) { continue; }
    if let Ok(key) = encode_index_key(&key_vals) {
        bloom.add(new_index_id, &key);
    }
}
```

BUT: `create_index` doesn't know the `index_id` until AFTER `CatalogWriter::create_index` runs. We need to capture the returned `index_id`:

```rust
let new_index_id = writer.create_index(IndexDef { ... })?;
bloom.create(new_index_id, row_count.max(1));
for key in all_keys_iter {
    bloom.add(new_index_id, &key);
}
```

To avoid re-scanning, collect keys during the initial scan:
```rust
let mut bloom_keys: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
for (rid, row_vals) in rows {
    // ... existing encode + BTree::insert_in ...
    if let Ok(key) = encode_index_key(&key_vals) {
        bloom_keys.push(key.clone());
        BTree::insert_in(storage, &root_pid, &key, rid)?;
    }
}
// After catalog write:
let new_index_id = writer.create_index(...)?;
bloom.create(new_index_id, bloom_keys.len());
for key in &bloom_keys {
    bloom.add(new_index_id, key);
}
```

### Step 6 — Update `execute_drop_index`

After `CatalogWriter::delete_index(id)`:
```rust
bloom.remove(id);
```

### Step 7 — Update `insert_into_indexes` and `delete_from_indexes`

Change signatures to accept `bloom`:
```rust
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
    bloom: &mut BloomRegistry,    // ← new
) -> Result<Vec<(u32, u64)>, DbError>

pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    storage: &mut dyn StorageEngine,
    bloom: &mut BloomRegistry,    // ← new
) -> Result<Vec<(u32, u64)>, DbError>
```

In `insert_into_indexes`, after `BTree::insert_in`:
```rust
bloom.add(idx.index_id, &key);
```

In `delete_from_indexes`, after `BTree::delete_in`:
```rust
bloom.mark_dirty(idx.index_id);
```

Update all callers in `executor.rs` (execute_insert, execute_update, execute_delete) to pass `bloom`.

### Step 8 — IndexLookup check in `execute_select`

In the `AccessMethod::IndexLookup { index_def, key }` branch:
```rust
// Bloom filter check: skip B-Tree lookup if key definitely absent.
if !bloom.might_exist(index_def.index_id, key) {
    // key definitely not in index → no rows
} else {
    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
        None => {}
        Some(rid) => {
            if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
                // ... existing row processing
            }
        }
    }
}
```

---

## Tests to write

### Unit tests (axiomdb-sql/src/bloom.rs)
- `test_new_registry_is_empty`
- `test_might_exist_unknown_index_returns_true` (conservative)
- `test_add_then_check_returns_true`
- `test_check_nonexistent_key_returns_false` (no false negatives for absent keys)
- `test_mark_dirty_does_not_break_might_exist`
- `test_remove_makes_conservative`
- `test_fp_rate_approximately_one_percent` (insert 10K random keys, check 10K different keys, count FPs)

### Integration tests (crates/axiomdb-sql/tests/integration_indexes.rs, new tests)
- `test_bloom_skip_on_miss` — verify the optimization fires (run SELECT WHERE col=missing; count should be 0 from the index lookup path)
- `test_bloom_hit_returns_correct_row` — SELECT WHERE col=existing still works
- `test_bloom_updated_on_insert` — INSERT then SELECT by that key → found
- `test_bloom_stale_on_delete_still_correct` — DELETE then SELECT → empty (stale but correct)
- `test_bloom_removed_on_drop_index` — DROP INDEX then SELECT (full scan) → still correct

---

## Anti-patterns to avoid

- **DO NOT** call `bloom.might_exist` before the planner has decided to use IndexLookup — the check is inside the IndexLookup branch only, not for Scan or IndexRange
- **DO NOT** rebuild the filter on every DELETE — mark dirty only; rebuild deferred to ANALYZE TABLE
- **DO NOT** pass `&BloomRegistry` to functions that need to update it — always `&mut BloomRegistry`
- **DO NOT** panic if `index_id` not in registry — conservative fallback (`true`)

## Risks

| Risk | Mitigation |
|------|------------|
| `bloomfilter` 3.x API differs from 1.x in db.md | Check API before using; `Bloom::new_for_fp_rate` exists in 3.x |
| `execute_with_ctx` signature change breaks all tests | Update all tests — mechanical search+replace |
| `bloom_keys` Vec allocates O(n) extra memory during CREATE INDEX | Accept: it's a one-time build cost; after return, Vec is dropped |
| Hash collision via `Vec<u8>` default hasher (SipHash) | SipHash is fine for non-adversarial keys; no risk |
