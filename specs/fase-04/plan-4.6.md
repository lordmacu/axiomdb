# Plan: 4.6 — INSERT ... SELECT

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | modify | Replace `NotImplemented` for `InsertSource::Select` |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add INSERT SELECT tests |

---

## Algorithm

### Step 1 — Replace the `NotImplemented` guard

In `execute_insert`, the current code:
```rust
InsertSource::Select(_) => {
    return Err(DbError::NotImplemented {
        feature: "INSERT SELECT — Phase 4.6".into(),
    })
}
```

Replace with the full implementation (see Step 2).

### Step 2 — Extract the `col_positions` build before the `source` match

Currently `execute_insert` builds `col_positions` before the `source` match —
it is already computed and can be reused for the SELECT path identically.
The `schema_cols` and `resolved.def` are also already available at that point.

The INSERT SELECT implementation inside the `InsertSource::Select` arm:

```rust
InsertSource::Select(select_stmt) => {
    // Execute the SELECT sub-statement using the current transaction.
    // The analyzed SelectStmt already has col_idx values resolved.
    let select_result = execute_select(*select_stmt, storage, txn)?;

    let select_rows = match select_result {
        QueryResult::Rows { rows, .. } => rows,
        // execute_select always returns Rows for a SELECT statement.
        other => {
            return Err(DbError::Other(format!(
                "INSERT SELECT: expected Rows from SELECT, got {other:?}"
            )))
        }
    };

    for row_values in select_rows {
        // Apply column mapping (same as VALUES path).
        let full_values: Vec<Value> = col_positions
            .iter()
            .map(|&idx| {
                if idx == usize::MAX {
                    Value::Null
                } else {
                    row_values.get(idx).cloned().unwrap_or(Value::Null)
                }
            })
            .collect();

        TableEngine::insert_row(storage, txn, &resolved.def, schema_cols, full_values)?;
        count += 1;
    }
}
```

---

## Implementation order

1. Remove the `NotImplemented` return in `execute_insert` for `InsertSource::Select`.
2. Add the SELECT execution + row iteration + insert loop.
   `cargo check -p axiomdb-sql`.
3. Write integration tests.
4. `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

```
test_insert_select_copy_all
  — CREATE TABLE src (id INT, name TEXT); INSERT 3 rows
  — CREATE TABLE dst (id INT, name TEXT); INSERT INTO dst SELECT * FROM src
  — SELECT * FROM dst → same 3 rows

test_insert_select_with_where
  — INSERT INTO dst SELECT * FROM src WHERE id > 1 → only rows with id > 1

test_insert_select_named_columns
  — INSERT INTO dst (name) SELECT name FROM src
  — dst has (id INT, name TEXT); id should be NULL for all rows

test_insert_select_with_limit
  — INSERT INTO dst SELECT * FROM src ORDER BY id ASC LIMIT 2
  — dst has exactly 2 rows

test_insert_select_with_aggregation
  — INSERT INTO summary (dept, cnt) SELECT dept, COUNT(*) FROM employees GROUP BY dept
  — summary has correct counts per department

test_insert_select_mvcc_no_self_read
  — src and dst are the SAME table
  — INSERT INTO t SELECT * FROM t (where t has 2 rows)
  — after commit: t has 4 rows (original 2 + 2 copies; not infinite loop)
```

---

## Anti-patterns to avoid

- **DO NOT** re-analyze the SELECT statement. It arrives already analyzed
  (`col_idx` resolved). Pass it directly to `execute_select`.
- **DO NOT** worry about the Halloween problem — MVCC snapshot isolation
  already prevents self-reads within the same transaction.
- **DO NOT** `unwrap()` anywhere in `src/` code.
