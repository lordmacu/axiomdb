# Spec: 4.6 — INSERT ... SELECT

## What to build (not how)

`INSERT INTO target [(col_list)] SELECT ...` inserts the rows returned by a
`SELECT` statement into the target table. Every SQL feature supported by the
executor's `SELECT` path is automatically available: `WHERE`, `JOIN`, `GROUP BY`,
`ORDER BY`, `LIMIT`, `DISTINCT`, `CASE WHEN`, etc.

---

## Inputs / Outputs

```
Input:  InsertStmt {
          table: target table reference,
          columns: Optional<Vec<String>>,  -- named column list (same as VALUES)
          source: InsertSource::Select(Box<SelectStmt>),
        }

Output: QueryResult::Affected { count: rows_inserted, last_insert_id: None }
```

---

## Column mapping

Identical to `INSERT ... VALUES`:

- **No column list** (`stmt.columns = None`): SELECT columns are mapped to
  target columns positionally (position 0 → column 0, etc.).
- **Named column list** (`stmt.columns = Some(names)`): SELECT columns are
  mapped to the named target columns in the order listed. Non-mentioned target
  columns receive `Value::Null`.

If the SELECT produces more columns than expected by the mapping, the extra
columns are silently ignored. If it produces fewer, the missing values are
`Value::Null`.

---

## MVCC isolation

The SELECT executes under `txn.active_snapshot()`, which is fixed at the moment
the transaction began (`snapshot_id = max_committed + 1`). Rows inserted by
earlier `INSERT` statements within the same transaction have
`txn_id_created = current_txn_id > snapshot_id`, so they are **not** visible to
the SELECT scan. This prevents the "Halloween problem" (a row inserted by this
very INSERT being immediately re-scanned) without any additional mechanism.

---

## Execution order

```
1. Resolve target table → schema_cols, table_def
2. Build col_positions mapping (from stmt.columns or positional)
3. Execute SELECT → Vec<Row>  (all rows materialized in memory)
4. For each row in SELECT result:
   a. Apply col_positions mapping → full_values (same as VALUES path)
   b. TableEngine::insert_row(storage, txn, &table_def, &schema_cols, full_values)?
   c. count += 1
5. Return QueryResult::Affected { count, last_insert_id: None }
```

---

## Use cases

### 1. Copy all rows

```sql
INSERT INTO archive SELECT * FROM orders WHERE status = 'completed';
```

### 2. Named columns with transformation

```sql
INSERT INTO summary (dept, headcount)
SELECT dept, COUNT(*) FROM employees GROUP BY dept;
```

### 3. Filtered copy

```sql
INSERT INTO top_earners (id, name, salary)
SELECT id, name, salary FROM employees
WHERE salary > 100000
ORDER BY salary DESC
LIMIT 10;
```

### 4. Cross-table aggregation

```sql
INSERT INTO monthly_totals (month, total)
SELECT EXTRACT(MONTH FROM created_at), SUM(amount)
FROM orders
GROUP BY EXTRACT(MONTH FROM created_at);
-- Note: EXTRACT requires Phase 4.19 functions.
```

---

## Acceptance criteria

- [ ] `INSERT INTO t SELECT * FROM s` copies all rows from `s` to `t`
- [ ] `INSERT INTO t (a, b) SELECT x, y FROM s` maps SELECT columns to named target columns
- [ ] Column count mismatch: extra SELECT cols ignored; missing target cols → `NULL`
- [ ] `INSERT INTO t SELECT ... WHERE ...` respects WHERE filter
- [ ] `INSERT INTO t SELECT ... ORDER BY ... LIMIT n` inserts only the limited rows
- [ ] MVCC: rows inserted by this statement are NOT visible to its own SELECT
- [ ] `count` in `Affected` matches actual number of rows inserted
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- `INSERT ... SELECT ... RETURNING` (future)
- `INSERT ... ON CONFLICT` / `ON DUPLICATE KEY UPDATE` (future)
- `last_insert_id` for AUTO_INCREMENT tables (Phase 4.3c)

---

## Dependencies

- `axiomdb-sql/src/executor.rs` — only file modified
- No new crates, no new dependencies
- Reuses `execute_select`, `TableEngine::insert_row`, existing col_map logic
