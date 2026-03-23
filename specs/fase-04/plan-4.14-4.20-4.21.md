# Plan: 4.14 + 4.20 + 4.21 — LAST_INSERT_ID, SHOW/DESCRIBE, TRUNCATE

## Files to create/modify

| File | Action | What changes |
|------|--------|--------------|
| `crates/axiomdb-catalog/src/schema.rs` | modify | `ColumnDef.auto_increment: bool` — stored in existing `flags` byte bit1 |
| `crates/axiomdb-catalog/src/writer.rs` | modify | `create_column` accepts `auto_increment` flag |
| `crates/axiomdb-sql/src/executor.rs` | modify | AUTO_INCREMENT generation + SHOW/DESCRIBE + TRUNCATE |
| `crates/axiomdb-sql/src/eval.rs` | modify | `last_insert_id()` / `lastval()` in `eval_function` |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Tests for all three subfases |

---

## Implementation phases (in dependency order)

### Phase A — Catalog format: `ColumnDef.auto_increment`

**`schema.rs`** — `ColumnDef` already has a `flags: u8` byte:
```
Format: [table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]
  flags bit0 = nullable  (existing)
  flags bit1 = auto_increment  (NEW — bit was previously always 0)
```

Changes:
1. Add `pub auto_increment: bool` field to `ColumnDef` struct.
2. `to_bytes()`: set `flags |= 0x02` when `auto_increment = true`.
3. `from_bytes()`: `let auto_increment = flags & 0x02 != 0;` — backward compatible
   (old rows had bit1 = 0 → defaults to `false`).
4. Add `auto_increment: false` to all existing `ColumnDef { ... }` construction sites.

**`writer.rs`** — `create_column(def: ColumnDef)` already takes the full struct;
no signature change needed. Callers must set `auto_increment` appropriately.

---

### Phase B — executor.rs: AUTO_INCREMENT generation (4.14)

**Thread-local sequence state:**
```rust
use std::collections::HashMap;
use std::cell::Cell;

thread_local! {
    /// Per-table AUTO_INCREMENT sequence counter.
    /// Key = TableId, Value = next value to assign.
    /// Initialized lazily on first AUTO_INCREMENT insert into each table.
    static AUTO_INC_SEQ: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());

    /// The last auto-generated ID in this thread (returned by LAST_INSERT_ID()).
    static LAST_INSERT_ID: Cell<u64> = Cell::new(0);
}
```

**`execute_create_table` — store auto_increment in catalog:**
```rust
let auto_increment = col_def.constraints.iter()
    .any(|c| matches!(c, ColumnConstraint::AutoIncrement));
writer.create_column(CatalogColumnDef {
    table_id,
    col_idx: i as u16,
    name: col_def.name.clone(),
    col_type,
    nullable,
    auto_increment,    // ← new
})?;
```

**`execute_insert` — generate AUTO_INCREMENT values:**

After resolving the table and schema_cols, find the AUTO_INCREMENT column (if any):
```rust
let auto_inc_col: Option<usize> = schema_cols.iter()
    .position(|c| c.auto_increment);
```

For each inserted row, if `auto_inc_col.is_some()` and the provided value is `NULL`:
```rust
fn next_auto_inc(storage: &dyn StorageEngine, table_def: &TableDef, columns: &[ColumnDef], col_idx: usize, txn: &TxnManager) -> Result<u64, DbError> {
    let table_id = table_def.id as u64;
    AUTO_INC_SEQ.with(|seq| {
        let mut map = seq.borrow_mut();
        if let Some(&next) = map.get(&table_id) {
            let val = next;
            map.insert(table_id, next + 1);
            return Ok(val);
        }
        // First use: scan to find MAX of the auto-increment column.
        drop(map); // release borrow before calling scan
        // ... scan table, find max, init sequence
    })
}
```

Scan to find MAX:
```rust
let snap = txn.active_snapshot()?;
let rows = TableEngine::scan_table(storage, table_def, columns, snap)?;
let max_id: u64 = rows.iter()
    .filter_map(|(_, vals)| vals.get(col_idx))
    .filter_map(|v| match v {
        Value::Int(n) => Some(*n as u64),
        Value::BigInt(n) => Some(*n as u64),
        _ => None,
    })
    .max()
    .unwrap_or(0);
AUTO_INC_SEQ.with(|seq| {
    seq.borrow_mut().insert(table_id, max_id + 2);  // +2: +1 for current use, +1 for next
});
Ok(max_id + 1)
```

After all rows inserted, if any AUTO_INCREMENT ID was generated:
```rust
if let Some(first_id) = first_generated {
    LAST_INSERT_ID.with(|v| v.set(first_id));
    return Ok(QueryResult::affected_with_id(count, first_id));
}
```

**TRUNCATE resets sequence:**
```rust
fn execute_truncate(table_id: u64) {
    AUTO_INC_SEQ.with(|seq| { seq.borrow_mut().remove(&table_id); });
    // On next insert, sequence re-initializes from MAX (will be 1 since table is empty)
}
```

---

### Phase C — eval.rs: `last_insert_id()` / `lastval()`

In `eval_function`, add:
```rust
"last_insert_id" | "lastval" => {
    let val = LAST_INSERT_ID.with(|v| v.get());
    Ok(Value::BigInt(val as i64))
}
```

`LAST_INSERT_ID` must be `pub(crate)` or re-exported so `eval.rs` can access it.
Best approach: define both thread-locals in `executor.rs` and add a public
`pub fn last_insert_id_value() -> u64` function that `eval.rs` can call.

---

### Phase D — executor.rs: SHOW TABLES / SHOW COLUMNS (4.20)

Replace the `NotImplemented` stub for `Stmt::ShowTables(_) | Stmt::ShowColumns(_)`:

**`execute_show_tables(stmt, storage, txn)`:**
```rust
let schema = stmt.schema.as_deref().unwrap_or("public");
let snap = txn.active_snapshot()?;
let reader = CatalogReader::new(storage, snap)?;
let tables = reader.list_tables(schema)?;

let col_name = format!("Tables_in_{schema}");
let out_cols = vec![ColumnMeta::computed(col_name, DataType::Text)];
let rows: Vec<Row> = tables.iter()
    .map(|t| vec![Value::Text(t.table_name.clone())])
    .collect();
Ok(QueryResult::Rows { columns: out_cols, rows })
```

**`execute_show_columns(stmt, storage, txn)`:**
```rust
let schema = stmt.table.schema.as_deref().unwrap_or("public");
let snap = txn.active_snapshot()?;
let reader = CatalogReader::new(storage, snap)?;
let table_def = reader.get_table(schema, &stmt.table.name)?
    .ok_or_else(|| DbError::TableNotFound { name: stmt.table.name.clone() })?;
let columns = reader.list_columns(table_def.id)?;

let out_cols = vec![
    ColumnMeta::computed("Field",   DataType::Text),
    ColumnMeta::computed("Type",    DataType::Text),
    ColumnMeta::computed("Null",    DataType::Text),
    ColumnMeta::computed("Key",     DataType::Text),
    ColumnMeta::computed("Default", DataType::Text),
    ColumnMeta::computed("Extra",   DataType::Text),
];

let rows: Vec<Row> = columns.iter().map(|c| {
    let type_str = column_type_name(c.col_type);
    let null_str = if c.nullable { "YES" } else { "NO" };
    let extra    = if c.auto_increment { "auto_increment" } else { "" };
    vec![
        Value::Text(c.name.clone()),
        Value::Text(type_str.into()),
        Value::Text(null_str.into()),
        Value::Text("".into()),    // Key — stub
        Value::Null,               // Default — stub
        Value::Text(extra.into()),
    ]
}).collect();

Ok(QueryResult::Rows { columns: out_cols, rows })
```

Add helper `fn column_type_name(ct: ColumnType) -> &'static str` mapping ColumnType → SQL type string.

---

### Phase E — executor.rs: TRUNCATE TABLE (4.21)

Replace `TruncateTable` stub:
```rust
fn execute_truncate(stmt: TruncateTableStmt, storage: &mut dyn StorageEngine, txn: &mut TxnManager) -> Result<QueryResult, DbError> {
    let resolved = {
        let resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap)?;
    let rids: Vec<RecordId> = rows.into_iter().map(|(rid, _)| rid).collect();
    for rid in rids {
        TableEngine::delete_row(storage, txn, &resolved.def, rid)?;
    }

    // Reset AUTO_INCREMENT sequence so next insert starts from 1.
    AUTO_INC_SEQ.with(|seq| {
        seq.borrow_mut().remove(&(resolved.def.id as u64));
    });

    // MySQL returns count=0 for TRUNCATE (not the actual deleted count).
    Ok(QueryResult::Affected { count: 0, last_insert_id: None })
}
```

---

### Phase F — dispatch wiring

In `dispatch()`:
```rust
Stmt::TruncateTable(s) => execute_truncate(s, storage, txn),
Stmt::ShowTables(s)    => execute_show_tables(s, storage, txn),
Stmt::ShowColumns(s)   => execute_show_columns(s, storage, txn),
```

Also add ctx-aware variants in `execute_with_ctx` dispatch.

---

## Tests to write (in `integration_executor.rs`)

### 4.14
- Create table with AUTO_INCREMENT, insert without id → id=1
- Insert again → id=2, LAST_INSERT_ID()=2
- Insert with explicit id=99 → LAST_INSERT_ID() unchanged (still 2)
- Multi-row INSERT → LAST_INSERT_ID() returns FIRST generated id
- After restart simulation (drop thread-local via new sequence init) → continues from MAX+1
- `SELECT LAST_INSERT_ID()` returns 0 before any insert
- `SELECT lastval()` is alias for LAST_INSERT_ID

### 4.20
- SHOW TABLES returns all created tables
- SHOW TABLES after CREATE + DROP returns updated list
- DESCRIBE users shows correct Field/Type/Null
- SHOW COLUMNS FROM users identical to DESCRIBE
- Null column shows YES, NOT NULL shows NO
- AUTO_INCREMENT column shows `auto_increment` in Extra
- DESCRIBE nonexistent → TableNotFound

### 4.21
- TRUNCATE TABLE deletes all rows
- COUNT(*) after TRUNCATE = 0
- Next AUTO_INCREMENT insert after TRUNCATE starts from 1
- TRUNCATE on empty table → no error, Affected{count:0}
- TRUNCATE nonexistent → TableNotFound

---

## Anti-patterns to avoid

- **DO NOT** call `scan_table` inside the AUTO_INCREMENT sequence init while also
  holding the `seq` `RefCell` borrow — `scan_table` may call into storage which
  could trigger re-entrant borrows. Release the borrow before scanning.

- **DO NOT** advance `LAST_INSERT_ID` when the user provides an explicit non-NULL
  value for an AUTO_INCREMENT column. Only update it when the executor generated
  the value.

- **DO NOT** use `u32` for the AUTO_INCREMENT sequence map key — use `u64` to
  match `table_id as u64` and avoid truncation for large table IDs.

## Risks

- **Thread-local sequence lost on restart** — documented as deferred to Phase 5+.
  The lazy initialization from `MAX(col)+1` mitigates this: correct behavior
  is restored on the first auto-insert after restart.

- **`LAST_INSERT_ID` thread-local shared across tests** — parallel tests running
  in different threads will each have independent thread-locals (correct).
  Sequential tests in the same thread may observe stale values. Mitigate:
  always test `LAST_INSERT_ID` immediately after the INSERT that generated it.
