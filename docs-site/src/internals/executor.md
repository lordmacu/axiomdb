# SQL Executor

The executor is the component that interprets an analyzed `Stmt` (all column
references resolved to `col_idx` by the semantic analyzer) and drives it to
completion, returning a `QueryResult`. It is the highest-level component in the
query pipeline.

---

## Entry Point

```rust
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError>
```

When no transaction is active, `execute` wraps the statement in an implicit
`BEGIN / COMMIT` (autocommit mode). Transaction control statements (`BEGIN`,
`COMMIT`, `ROLLBACK`) bypass autocommit and operate on `TxnManager` directly.

All reads use `txn.active_snapshot()?` — a snapshot fixed at `BEGIN` — so that
writes made earlier in the same transaction are visible (read-your-own-writes).

---

## Query Pipeline

```
SQL string
  → tokenize()         logos DFA, ~85 tokens, zero-copy &str
  → parse()            recursive descent, produces Stmt with col_idx = 0
  → analyze()          BindContext resolves every col_idx
  → execute()          dispatches to per-statement handler
      ├── scan_table   HeapChain::scan_visible + decode_row
      ├── filter       eval(WHERE, &row) + is_truthy
      ├── join         nested-loop, apply_join
      ├── aggregate    hash-based GroupState
      ├── sort         apply_order_by, compare_sort_values
      ├── deduplicate  apply_distinct, value_to_key_bytes
      ├── project      project_row / project_grouped_row
      └── paginate     apply_limit_offset
  → QueryResult::Rows / Affected / Empty
```

---

## JOIN — Nested Loop

Phase 4 implements nested-loop joins. All tables are **pre-scanned once** before
any loop begins — scanning inside the inner loop would re-read the same data O(n)
times and could see partially-inserted rows.

### Algorithm

```
scanned[0] = scan(FROM table)
scanned[1] = scan(JOIN[0] table)
...

combined_rows = scanned[0]
for each JoinClause in stmt.joins:
    combined_rows = apply_join(combined_rows, scanned[i+1], join_type, ON/USING)
```

### `apply_join` per type

| Join type | Behavior |
|---|---|
| `INNER` / `CROSS` | Emit combined row for each pair where ON is truthy |
| `LEFT` | Emit all left rows; unmatched left → right side padded with `NULL` |
| `RIGHT` | Emit all right rows; unmatched right → left side padded with `NULL`; uses a `matched_right: Vec<bool>` bitset |
| `FULL` | `NotImplemented` — Phase 4.8+ |

### USING condition

`USING(col_name)` is resolved at execution time using `left_schema: Vec<(name, col_idx)>`,
accumulated across all join stages. The condition `combined[left_idx] == combined[right_idx]`
uses SQL equality — `NULL = NULL` returns UNKNOWN (false), so NULLs never match in USING.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Pre-scan Before Loop</span>
All tables are scanned once before the nested-loop begins. This is the primary anti-pattern to avoid: scanning inside the inner loop re-reads data O(n) times and, for LEFT/RIGHT joins that modify the heap, can observe partially-inserted rows. Pre-scanning also enables the RIGHT JOIN bitset pattern, which requires knowing the total right-side row count upfront.
</div>
</div>

---

## GROUP BY — Hash Aggregation

Group BY uses a single-pass hash aggregation strategy: one scan through the
filtered rows, accumulating aggregate state per group key.

### Group Key Serialization

`Value` contains `f64` which does not implement `Hash` in Rust. AxiomDB uses a
custom self-describing byte serialization instead of the row codec:

```
value_to_key_bytes(Value::Null)        → [0x00]
value_to_key_bytes(Value::Int(n))      → [0x02, n as 4 LE bytes]
value_to_key_bytes(Value::Text(s))     → [0x06, len as 4 LE bytes, UTF-8 bytes]
...
```

Two `NULL` values produce identical bytes `[0x00]` → they form **one group**.
This matches SQL GROUP BY semantics: NULLs are considered equal for grouping
(unlike `NULL = NULL` in comparisons, which is UNKNOWN).

The group key for a multi-column GROUP BY is the concatenation of all column
serializations.

### GroupState

Each unique group key maps to a `GroupState`:

```rust
struct GroupState {
    key_values: Vec<Value>,       // GROUP BY expression results
    representative_row: Row,      // first source row (for HAVING col refs)
    accumulators: Vec<AggAccumulator>,
}
```

The `representative_row` is critical for HAVING: expressions like
`HAVING salary > 50000` use `col_idx` relative to the source row, not the
output row. Without `representative_row`, HAVING column references would be
out-of-bounds on the projected output.

### Aggregate Accumulators

| Aggregate | Accumulator | NULL behavior |
|---|---|---|
| `COUNT(*)` | `u64` counter | Increments for every row |
| `COUNT(col)` | `u64` counter | Skips rows where `col` is NULL |
| `SUM(col)` | `Option<Value>` | Skips NULL; `None` if all rows are NULL |
| `MIN(col)` | `Option<Value>` | Skips NULL; tracks running minimum |
| `MAX(col)` | `Option<Value>` | Skips NULL; tracks running maximum |
| `AVG(col)` | `(sum: Value, count: u64)` | Skips NULL; final = `sum / count` as Real |

`AVG` always returns `Real` (SQL standard), even for integer columns. This
avoids integer truncation (MySQL-style `AVG(INT)` returns DECIMAL but truncates
in many contexts).

### Ungrouped Aggregates

`SELECT COUNT(*) FROM t` (no GROUP BY) is handled as a single-group query with
an empty key. Even on an empty table, the executor emits exactly **one output
row** — `(0)` for `COUNT(*)`, `NULL` for `SUM/MIN/MAX/AVG`. This matches the
SQL standard and every major database.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — representative_row</span>
HAVING expressions reference source columns via `col_idx`, not output positions. The `representative_row` preserves one source row per group so that `HAVING salary > 50000` (where `salary` has `col_idx = 2` in the source) can be evaluated correctly, even after the output row has been projected down to just `(dept, COUNT(*))`.
</div>
</div>

---

## ORDER BY — Multi-Column Sort

ORDER BY is applied **after scan + filter + aggregation but before projection**
for non-GROUP BY queries. For GROUP BY queries, it is applied to the projected
output rows.

### NULL Ordering Defaults (PostgreSQL-compatible)

| Direction | Default | Override |
|---|---|---|
| `ASC` | NULLs LAST | `NULLS FIRST` |
| `DESC` | NULLs FIRST | `NULLS LAST` |

```
compare_sort_values(a, b, direction, nulls_override):
  nulls_first = explicit_nulls_order OR (DESC && no explicit)
  if a = NULL and b = NULL → Equal
  if a = NULL → Less if nulls_first, else Greater
  if b = NULL → Greater if nulls_first, else Less
  otherwise → compare a and b, reverse if DESC
```

Non-NULL comparison delegates to `eval(BinaryOp{Lt}, Literal(a), Literal(b))`
via the expression evaluator, reusing all type coercion and promotion logic.

### Error Propagation from sort_by

Rust's `sort_by` closure cannot return `Result`. AxiomDB uses the `sort_err`
pattern: errors are captured in `Option<DbError>` during the sort and returned
after it completes.

```rust
let mut sort_err: Option<DbError> = None;
rows.sort_by(|a, b| {
    match compare_rows_for_sort(a, b, order_items) {
        Ok(ord) => ord,
        Err(e)  => { sort_err = Some(e); Equal }
    }
});
if let Some(e) = sort_err { return Err(e); }
```

---

## DISTINCT — Deduplication

`SELECT DISTINCT` is applied **after projection and before LIMIT/OFFSET**, using
a `HashSet<Vec<u8>>` keyed by `value_to_key_bytes`.

```
fn apply_distinct(rows: Vec<Row>) -> Vec<Row>:
    seen = HashSet::new()
    for row in rows:
        key = concat(value_to_key_bytes(v) for v in row)
        if seen.insert(key):   // first occurrence
            keep row
```

Two rows are identical if every column value serializes to the same bytes.
Critically, `NULL` → `[0x00]` means **two NULLs are considered equal** for
deduplication — only one row with a NULL in that position is kept. This is the
SQL standard behavior for DISTINCT, and is different from equality comparison
where `NULL = NULL` returns UNKNOWN.

---

## INSERT ... SELECT — MVCC Isolation

`INSERT INTO target SELECT ... FROM source` executes the SELECT phase under
the **same snapshot** as any other read in the transaction — fixed at `BEGIN`.

This prevents the "Halloween problem": rows inserted by this `INSERT` have
`txn_id_created = current_txn_id`. The snapshot was taken before any insert
occurred, so `snapshot_id ≤ current_txn_id`. The MVCC visibility rule
(`txn_id_created < snapshot_id`) causes newly inserted rows to be invisible to
the SELECT scan. The result:

- If `source = target` (inserting from a table into itself): the SELECT sees
  exactly the rows that existed at `BEGIN`. The inserted copies are not
  re-scanned. No infinite loop.
- If another transaction inserts rows into `source` after this transaction's
  `BEGIN`: those rows are also invisible (consistent snapshot).

```
Before BEGIN:  source = {row1, row2}
After BEGIN:   snapshot_id = 3  (max_committed = 2)

INSERT INTO source SELECT * FROM source:
  SELECT sees:  {row1 (xmin=1), row2 (xmin=2)} — both have xmin < snapshot_id ✅
  Inserts:      row3 (xmin=3), row4 (xmin=3) — xmin = current_txn_id = 3
  SELECT does NOT see row3 or row4 (xmin ≮ snapshot_id) ✅

After COMMIT:  source = {row1, row2, row3, row4}  ← exactly 2 new rows, not infinite
```

---

## Performance Characteristics

| Operation | Time complexity | Notes |
|---|---|---|
| Table scan | O(n) | HeapChain linear traversal |
| Nested loop JOIN | O(n × m) | Both sides materialized before loop |
| Hash GROUP BY | O(n) | One pass; O(k) memory where k = distinct groups |
| Sort ORDER BY | O(n log n) | `sort_by` (stable, in-memory) |
| DISTINCT | O(n) | One HashSet pass |
| LIMIT/OFFSET | O(1) after sort | `skip(offset).take(limit)` |

All operations are **in-memory** for Phase 4. External sort and hash spill for
large datasets are planned for Phase 14 (vectorized execution).
