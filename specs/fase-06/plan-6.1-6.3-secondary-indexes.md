# Plan: Secondary Indexes — Subfases 6.1, 6.2, 6.2b, 6.3

## Files to create / modify

| File | Action | What |
|------|--------|------|
| `crates/axiomdb-core/src/error.rs` | modify | Add `IndexAlreadyExists`, `IndexKeyTooLong`, `UniqueViolation` |
| `crates/axiomdb-catalog/src/schema.rs` | modify | `IndexColumnDef` type + `IndexDef.columns` + serialization |
| `crates/axiomdb-catalog/src/writer.rs` | modify | `create_index` takes `columns: Vec<IndexColumnDef>` |
| `crates/axiomdb-catalog/src/bootstrap.rs` | modify | Pass columns to PK `create_index` call |
| `crates/axiomdb-index/src/tree.rs` | modify | Add `lookup_in`, `insert_in`, `range_in` static fns |
| `crates/axiomdb-index/src/lib.rs` | modify | Re-export new functions |
| `crates/axiomdb-sql/src/key_encoding.rs` | create | `encode_index_key` + helpers |
| `crates/axiomdb-sql/src/index_maintenance.rs` | create | `indexes_for_table`, `insert_into_indexes`, `delete_from_indexes` |
| `crates/axiomdb-sql/src/planner.rs` | create | `AccessMethod`, `plan_select` |
| `crates/axiomdb-sql/src/executor.rs` | modify | `execute_create_index`, `execute_drop_index`, DML hooks, planner call |
| `crates/axiomdb-sql/src/lib.rs` | modify | Export new modules |
| `docs-site/src/user-guide/features/indexes.md` | modify | Full user guide |
| `docs-site/src/internals/btree.md` | modify | Static API + key encoding |
| `docs-site/src/internals/planner.md` | create | Access method selection |
| `docs-site/src/user-guide/errors.md` | modify | New error types |

---

## Phase-by-phase implementation steps

### Step 1 — Error variants (axiomdb-core)

Add to `DbError`:

```rust
#[error("index '{name}' already exists on table {table}")]
IndexAlreadyExists { name: String, table: String },

#[error("index key length {key_len} exceeds maximum {max}")]
IndexKeyTooLong { key_len: usize, max: usize },

#[error("unique constraint violated on index '{index_name}': duplicate key {key_repr}")]
UniqueViolation { index_name: String, key_repr: String },
```

---

### Step 2 — IndexColumnDef + IndexDef.columns (axiomdb-catalog)

In `schema.rs`:

```rust
// SortOrder: import from axiomdb-core (add to core, or define inline)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder { Asc = 0, Desc = 1 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnDef {
    pub col_idx: u16,
    pub order: SortOrder,
}
```

Extend `IndexDef`:
```rust
pub struct IndexDef {
    // ... existing fields ...
    pub columns: Vec<IndexColumnDef>,
}
```

`to_bytes` appends:
```rust
buf.push(self.columns.len() as u8);
for c in &self.columns {
    buf.extend_from_slice(&c.col_idx.to_le_bytes());
    buf.push(c.order as u8);
}
```

`from_bytes` after reading name:
```rust
let columns = if bytes.len() > consumed {
    let ncols = bytes[consumed] as usize;
    consumed += 1;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        if bytes.len() < consumed + 3 { return Err(err()); }
        let col_idx = u16::from_le_bytes([bytes[consumed], bytes[consumed+1]]);
        let order = match bytes[consumed+2] {
            0 => SortOrder::Asc,
            1 => SortOrder::Desc,
            _ => return Err(DbError::ParseError { message: "unknown SortOrder".into() }),
        };
        consumed += 3;
        cols.push(IndexColumnDef { col_idx, order });
    }
    cols
} else {
    vec![]
};
```

Update `CatalogWriter::create_index(&mut self, def: IndexDef)` — signature already takes `IndexDef`; just ensure callers populate `def.columns`.

Update `bootstrap.rs` PK create_index to pass `columns: vec![IndexColumnDef { col_idx: 0, order: SortOrder::Asc }]`.

---

### Step 3 — BTree static API (axiomdb-index)

In `tree.rs`, add to `impl BTree`:

```rust
pub fn lookup_in(
    storage: &dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
) -> Result<Option<RecordId>, DbError> {
    Self::check_key(key)?;
    let mut pid = root_pid;
    loop {
        let page = storage.read_page(pid)?;
        if page.body()[0] == 1 {
            let node = cast_leaf(page);
            return Ok(node.search(key).ok().map(|i| node.rid_at(i)));
        } else {
            let node = cast_internal(page);
            pid = node.child_at(node.find_child_idx(key));
        }
    }
}

pub fn insert_in(
    storage: &mut dyn StorageEngine,
    root_pid: &AtomicU64,
    key: &[u8],
    rid: RecordId,
) -> Result<(), DbError> {
    Self::check_key(key)?;
    let root = root_pid.load(Ordering::Acquire);
    match Self::insert_subtree(storage, root, key, rid)? {
        InsertResult::Ok(new_root) => {
            root_pid.store(new_root, Ordering::Release);
        }
        InsertResult::Split { left_pid, right_pid, sep } => {
            let new_root = Self::alloc_root(storage, &sep, left_pid, right_pid)?;
            root_pid.store(new_root, Ordering::Release);
        }
    }
    Ok(())
}

pub fn range_in(
    storage: &dyn StorageEngine,
    root_pid: u64,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Result<RangeIter<'_>, DbError> {
    // delegate to existing range() internals, using root_pid directly
    // RangeIter already takes (storage, root_pid, lo, hi) parameters in its constructor
    RangeIter::new(storage, root_pid, lo, hi)
}
```

Note: `insert_in` uses `store` instead of `compare_exchange` because the caller holds
`&mut dyn StorageEngine` — single-threaded by construction in Phase 6.

---

### Step 4 — Key encoding (axiomdb-sql/src/key_encoding.rs)

```rust
use axiomdb_core::value::Value;
use axiomdb_core::error::DbError;

pub const MAX_INDEX_KEY: usize = 768;

pub fn encode_index_key(values: &[Value]) -> Result<Vec<u8>, DbError> {
    let mut buf = Vec::with_capacity(64);
    for v in values {
        encode_value(v, &mut buf);
    }
    if buf.len() > MAX_INDEX_KEY {
        return Err(DbError::IndexKeyTooLong { key_len: buf.len(), max: MAX_INDEX_KEY });
    }
    Ok(buf)
}

fn encode_value(v: &Value, buf: &mut Vec<u8>) {
    match v {
        Value::Null       => buf.push(0x00),
        Value::Bool(b)    => { buf.push(0x01); buf.push(*b as u8); }
        Value::Int(n)     => { buf.push(0x02); buf.extend_from_slice(&((*n as i64 ^ i64::MIN) as u64).to_be_bytes()); }
        Value::BigInt(n)  => { buf.push(0x03); buf.extend_from_slice(&((*n ^ i64::MIN) as u64).to_be_bytes()); }
        Value::Float(f)   => { buf.push(0x04); buf.extend_from_slice(&encode_f64(*f)); }
        Value::Text(s)    => { buf.push(0x05); encode_bytes_nul(s.as_bytes(), buf); }
        Value::Bytes(b)   => { buf.push(0x06); encode_bytes_nul(b, buf); }
        Value::Timestamp(t) => { buf.push(0x07); buf.extend_from_slice(&((*t ^ i64::MIN) as u64).to_be_bytes()); }
        Value::Uuid(u)    => { buf.push(0x08); buf.extend_from_slice(u); }
    }
}

fn encode_f64(f: f64) -> [u8; 8] {
    if f.is_nan() { return [0u8; 8]; }
    let bits = f.to_bits();
    let result = if f >= 0.0 { bits | (1 << 63) } else { !bits };
    result.to_be_bytes()
}

fn encode_bytes_nul(b: &[u8], buf: &mut Vec<u8>) {
    for &byte in b {
        if byte == 0x00 { buf.push(0xFF); buf.push(0x00); }
        else { buf.push(byte); }
    }
    buf.push(0x00);
}

pub fn encode_index_key_desc(values: &[Value]) -> Result<Vec<u8>, DbError> {
    let mut enc = encode_index_key(values)?;
    for b in &mut enc { *b = !*b; }
    Ok(enc)
}
```

---

### Step 5 — execute_create_index rewrite (axiomdb-sql/src/executor.rs)

Replace the stub with:

```rust
fn execute_create_index(stmt, storage, txn) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // 1. Resolve table + columns
    let (table_def, col_defs) = {
        let reader = CatalogReader::new(storage, snap)?;
        let tdef = reader.get_table(schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound { name: stmt.table.name.clone() })?;
        let cols = reader.list_columns(tdef.id)?;
        (tdef, cols)
    };

    // 2. Check duplicate index name
    {
        let reader = CatalogReader::new(storage, snap)?;
        let existing = reader.list_indexes(table_def.id)?;
        if existing.iter().any(|i| i.name == stmt.name) {
            return Err(DbError::IndexAlreadyExists { name: stmt.name, table: stmt.table.name });
        }
    }

    // 3. Build IndexColumnDef list
    let index_columns: Vec<IndexColumnDef> = stmt.columns.iter().map(|ic| {
        let col = col_defs.iter().find(|c| c.name == ic.name)
            .expect("analyzer guarantees column exists");
        IndexColumnDef { col_idx: col.col_idx, order: ic.order.into() }
    }).collect();

    // 4. Allocate and initialize B-Tree root leaf
    let root_pid = AtomicU64::new({
        let pid = storage.alloc_page(PageType::Index)?;
        let mut page = Page::new(PageType::Index, pid);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(pid, &page)?;
        pid
    });

    // 5. Scan table and insert into B-Tree
    let heap_root = table_def.data_root_page_id;
    let mut heap = HeapChain::open(storage, heap_root)?;
    let mut warn_count = 0usize;
    for result in heap.scan(snap)? {
        let (rid, row_bytes) = result?;
        let row_vals = RowCodec::decode(&row_bytes, &col_defs)?;
        let key_vals: Vec<Value> = index_columns.iter()
            .map(|ic| row_vals[ic.col_idx as usize].clone())
            .collect();
        match encode_index_key(&key_vals) {
            Ok(key) => BTree::insert_in(storage, &root_pid, &key, rid)?,
            Err(DbError::IndexKeyTooLong { .. }) => { warn_count += 1; }
            Err(e) => return Err(e),
        }
    }
    if warn_count > 0 {
        eprintln!("CREATE INDEX: skipped {warn_count} rows with keys exceeding {MAX_INDEX_KEY} bytes");
    }

    // 6. Persist IndexDef with columns
    let mut writer = CatalogWriter::new(storage, txn)?;
    writer.create_index(IndexDef {
        index_id: 0,
        table_id: table_def.id,
        name: stmt.name,
        root_page_id: root_pid.load(Ordering::Acquire),
        is_unique: stmt.unique,
        is_primary: false,
        columns: index_columns,
    })?;

    Ok(QueryResult::Empty)
}
```

---

### Step 6 — execute_drop_index: free B-Tree pages

After `CatalogWriter::delete_index`, add:

```rust
fn free_btree_pages(storage: &mut dyn StorageEngine, root_pid: u64) -> Result<(), DbError> {
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let page = storage.read_page(pid)?;
        if page.body()[0] != 1 {
            // Internal node — push all children
            let node = cast_internal(page);
            for i in 0..=node.num_keys() {
                stack.push(node.child_at(i));
            }
        }
        storage.free_page(pid)?;
    }
    Ok(())
}
```

---

### Step 7 — index_maintenance module (axiomdb-sql/src/index_maintenance.rs)

```rust
pub fn indexes_for_table(
    table_id: TableId,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Vec<IndexDef>, DbError> {
    let reader = CatalogReader::new(storage, snapshot)?;
    reader.list_indexes(table_id)
}

pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
) -> Result<(), DbError> {
    for idx in indexes.filter(|i| !i.is_primary) {
        let key_vals: Vec<Value> = idx.columns.iter().map(|c| row[c.col_idx as usize].clone()).collect();
        let key = encode_index_key(&key_vals)?;
        if idx.is_unique {
            // Check for NULL — skip uniqueness check
            if !key_vals.iter().any(|v| matches!(v, Value::Null)) {
                if BTree::lookup_in(storage, idx.root_page_id, &key)?.is_some() {
                    return Err(DbError::UniqueViolation {
                        index_name: idx.name.clone(),
                        key_repr: format!("{key_vals:?}"),
                    });
                }
            }
        }
        let root_pid = AtomicU64::new(idx.root_page_id);
        BTree::insert_in(storage, &root_pid, &key, rid)?;
        // Note: root_page_id may change after split — need to persist updated root.
        // Solution: CatalogWriter::update_index_root(idx.index_id, new_root_pid)
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            // persist via catalog writer — caller must flush txn
        }
    }
    Ok(())
}
```

**Note on root_page_id persistence**: when `BTree::insert_in` causes a root split, the
root page changes.  The updated `root_page_id` must be persisted back to the catalog.
Add `CatalogWriter::update_index_root(index_id: u32, new_root: u64)` which rewrites the
IndexDef row in `axiom_indexes`.

This is the most complex part of 6.2b.

---

### Step 8 — Planner (axiomdb-sql/src/planner.rs)

```rust
pub enum AccessMethod { Scan, IndexLookup { ... }, IndexRange { ... } }

pub fn plan_select(
    where_expr: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> AccessMethod {
    let expr = match where_expr { Some(e) => e, None => return AccessMethod::Scan };

    // Rule 1: col = literal
    if let Expr::BinOp { op: BinOp::Eq, left, right } = expr {
        if let Some((col_name, value)) = extract_col_eq(left, right).or_else(|| extract_col_eq(right, left)) {
            if let Some(idx) = find_index_on_col(col_name, indexes, columns) {
                return AccessMethod::IndexLookup { index_def: idx.clone(), key_values: vec![value] };
            }
        }
    }

    // Rule 2: col > lo AND col < hi (or >=, <=)
    if let Some((idx, lo, hi, inc_lo, inc_hi)) = extract_range(expr, indexes, columns) {
        return AccessMethod::IndexRange { index_def: idx, lo: Some(lo), hi: Some(hi), inclusive_lo: inc_lo, inclusive_hi: inc_hi };
    }

    AccessMethod::Scan
}
```

---

### Step 9 — integrate planner into execute_select

In `execute_select`, before the scan loop:

```rust
let access_method = {
    let indexes = indexes_for_table(table_id, storage, snap)?;
    let cols = col_defs; // already loaded
    plan_select(select.where_expr.as_ref(), &indexes, &cols)
};

match access_method {
    AccessMethod::Scan => { /* existing full scan */ }
    AccessMethod::IndexLookup { index_def, key_values } => {
        let key = encode_index_key(&key_values)?;
        if let Some(rid) = BTree::lookup_in(storage, index_def.root_page_id, &key)? {
            let row_bytes = heap.read(rid)?;
            let row = RowCodec::decode(&row_bytes, &col_defs)?;
            // apply residual WHERE (any conditions not consumed by index)
            // emit row if passes
        }
    }
    AccessMethod::IndexRange { index_def, lo, hi, inclusive_lo, inclusive_hi } => {
        let lo_key = lo.map(|v| encode_index_key(&v)).transpose()?;
        let hi_key = hi.map(|v| encode_index_key(&v)).transpose()?;
        for (rid, _) in BTree::range_in(storage, index_def.root_page_id, lo_key.as_deref(), hi_key.as_deref())? {
            let row_bytes = heap.read(rid)?;
            let row = RowCodec::decode(&row_bytes, &col_defs)?;
            // residual WHERE + emit
        }
    }
}
```

---

## Tests to write

### Unit tests (axiomdb-catalog)
- `test_index_column_def_roundtrip_asc_desc`
- `test_index_def_roundtrip_with_columns`
- `test_index_def_from_old_format_no_columns` — truncated bytes → `columns: []`

### Unit tests (axiomdb-sql/key_encoding)
- `test_null_sorts_first`
- `test_int_sort_order_negative_positive`
- `test_float_sort_order`
- `test_text_nul_escape`
- `test_composite_key_order`
- `test_key_too_long`

### Integration tests (tests/)
- `test_create_index_populates_btree` — INSERT rows, CREATE INDEX, lookup each row
- `test_create_index_on_existing_data` — pre-populated table, CREATE INDEX, SELECT WHERE
- `test_drop_index_no_pages_leaked` — CREATE INDEX, DROP INDEX, page count unchanged
- `test_unique_index_violation` — INSERT duplicate in UNIQUE index → UniqueViolation
- `test_null_in_unique_index_allowed` — two NULLs in UNIQUE column → OK
- `test_update_maintains_index` — UPDATE indexed column, query with new value works
- `test_delete_removes_from_index` — DELETE, verify lookup returns None
- `test_planner_uses_index_equality` — query with WHERE on indexed col
- `test_planner_uses_index_range` — query with range on indexed col
- `test_planner_fallback_no_index` — query on non-indexed col → Scan (no crash)

### Benchmark
- `bench_point_lookup_indexed` — SELECT WHERE id = ? with 1M rows; target < 50 µs

---

## Anti-patterns to avoid

- **DO NOT** copy `BTree` code into executor — use static `insert_in` / `lookup_in`
- **DO NOT** store `AccessMethod` in the AST — keep it transient, computed at execute time
- **DO NOT** re-scan the catalog for indexes inside the row loop — load once before scan
- **DO NOT** forget to persist updated `root_page_id` after B-Tree splits in maintenance

---

## Risks

| Risk | Mitigation |
|------|------------|
| B-Tree root splits during index build change root_page_id | Use `AtomicU64` + persist final value to catalog at end of build |
| Concurrent writers (Phase 7) race on index root | Deferred — Phase 6 is single-writer |
| RangeIter borrows storage — lifetime issues with `range_in` | Use `unsafe` lifetime extension if needed, with `SAFETY:` comment; or copy rids into Vec |
| Heap scan API may not exist yet | Check `HeapChain::scan` in Phase 4; if missing, use existing iterator from executor |
| `list_indexes` may not exist in CatalogReader | Add it if not present |
