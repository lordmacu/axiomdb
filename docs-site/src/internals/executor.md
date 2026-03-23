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

## Subquery Execution

Subquery execution is integrated into the expression evaluator via the
`SubqueryRunner` trait. This design allows the compiler to eliminate all subquery
dispatch overhead in the non-subquery path at zero runtime cost.

### SubqueryRunner Trait

```rust
pub trait SubqueryRunner {
    fn eval_scalar(&mut self, subquery: &SelectStmt) -> Result<Value, DbError>;
    fn eval_in(&mut self, subquery: &SelectStmt, needle: &Value) -> Result<Value, DbError>;
    fn eval_exists(&mut self, subquery: &SelectStmt) -> Result<bool, DbError>;
}
```

All expression evaluation is dispatched through `eval_with<R: SubqueryRunner>`:

```rust
pub fn eval_with<R: SubqueryRunner>(
    expr: &Expr,
    row: &Row,
    runner: &mut R,
) -> Result<Value, DbError>
```

Two concrete implementations exist:

| Implementation | Purpose |
|---|---|
| `NoSubquery` | Used for simple expressions with no subqueries. All three `SubqueryRunner` methods are `unreachable!()`. Monomorphization guarantees they are dead code. |
| `ExecSubqueryRunner<'a>` | Used when the query contains at least one subquery. Holds mutable references to storage, the transaction manager, and the outer row for correlated access. |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Generic Trait Monomorphization</span>
Using <code>SubqueryRunner</code> as a generic trait parameter — rather than a runtime <code>Option&lt;&amp;mut dyn FnMut&gt;</code> or a boolean flag — allows the compiler to generate two separate code paths: <code>eval_with::&lt;NoSubquery&gt;</code> and <code>eval_with::&lt;ExecSubqueryRunner&gt;</code>. In the <code>NoSubquery</code> path, every subquery branch is dead code and is eliminated by LLVM. A runtime option would add a pointer-width check plus a potential indirect call on every expression node evaluation, even for the 99% of expressions that have no subqueries.
</div>
</div>

### Scalar Subquery Evaluation

`ExecSubqueryRunner::eval_scalar` executes the inner `SelectStmt` fully using
the existing `execute_select` path, then inspects the result:

```
eval_scalar(subquery):
  result = execute_select(subquery, storage, txn)
  match result.rows.len():
    0     → Value::Null
    1     → result.rows[0][0]   // single column, single row
    n > 1 → Err(CardinalityViolation { returned: n })
```

The inner SELECT is always run with a fresh output context. It inherits the outer
transaction snapshot so it sees the same consistent view as the outer query.

### IN Subquery Evaluation

`eval_in` materializes the subquery result into a `HashSet<Value>`, then applies
three-valued logic:

```
eval_in(subquery, needle):
  rows = execute_select(subquery)
  values: HashSet<Value> = rows.map(|r| r[0]).collect()

  if values.contains(needle):
    return Value::Bool(true)
  if values.contains(Value::Null):
    return Value::Null       // unknown — could match
  return Value::Bool(false)
```

For `NOT IN`, the calling code wraps the result: `TRUE → FALSE`, `FALSE → TRUE`,
`NULL → NULL` (NULL propagates unchanged).

### EXISTS Evaluation

`eval_exists` executes the subquery and checks whether the result set is non-empty.
No rows are materialized beyond the first:

```
eval_exists(subquery):
  rows = execute_select(subquery)
  return !rows.is_empty()   // always bool, never null
```

### Correlated Subqueries — `substitute_outer`

Before executing a correlated subquery, `ExecSubqueryRunner` walks the subquery
AST and replaces every `Expr::OuterColumn { col_idx, depth: 1 }` with a concrete
`Expr::Literal(value)` from the current outer row. This operation is called
`substitute_outer`:

```
substitute_outer(expr_tree, outer_row):
  for each node in expr_tree:
    if node = OuterColumn { col_idx, depth: 1 }:
      replace with Literal(outer_row[col_idx])
    if node = OuterColumn { col_idx, depth: d > 1 }:
      decrement depth by 1  // pass through for deeper nesting
```

After substitution, the subquery is a fully self-contained statement with no
outer references, and it is executed by the standard `execute_select` path.

Re-execution happens once per outer row: for a correlated `EXISTS` in a query
that produces 10,000 outer rows, the inner query is executed 10,000 times.
For large datasets, rewriting as a JOIN is recommended.

### Derived Table Execution

A derived table (`FROM (SELECT ...) AS alias`) is materialized once at the
start of query execution, before any scan or filter of the outer query begins:

```
execute_select(stmt):
  for each TableRef::Derived { subquery, alias } in stmt.from:
    materialized[alias] = execute_select(subquery)   // fully materialized in memory
  // outer query scans materialized[alias] as if it were a base table
```

The materialized result is an in-memory `Vec<Row>` wrapped in a
`MaterializedTable`. The outer query uses the derived table's output schema
(column names from the inner SELECT list) for column resolution.

Derived tables are not correlated — they cannot reference columns from the outer
query. Lateral joins (which allow correlation in `FROM`) are not yet supported.

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

---

## AUTO_INCREMENT Execution

### Per-Table Sequence State

Each table that has an `AUTO_INCREMENT` column maintains a sequence counter.
The counter is stored as a thread-local `HashMap<String, i64>` keyed by table
name, lazily initialized on the first INSERT:

```
auto_increment_next(table_name):
  if table_name not in thread_local_map:
    max_existing = MAX(id) from HeapChain scan, or 0 if table is empty
    thread_local_map[table_name] = max_existing + 1
  value = thread_local_map[table_name]
  thread_local_map[table_name] += 1
  return value
```

The `MAX+1` lazy-init strategy means the sequence is always consistent with
existing data, even after rows are inserted by a previous session or after
a crash recovery.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Thread-Local vs Per-Session State</span>
The sequence counter is stored in thread-local storage rather than attached to a
session object. Phase 4 uses a single-threaded executor, so thread-local and
session-local are equivalent. This avoids the complexity of a session handle
threading through every call site. When Phase 7 introduces concurrent sessions,
the counter will migrate to per-session state. The lazy-init from <code>MAX+1</code>
is compatible with either approach.
</div>
</div>

### Explicit Value Bypass

When the INSERT column list includes the AUTO_INCREMENT column with a non-NULL
value, the explicit value is used directly and the sequence counter is not
advanced:

```
for each row to insert:
  if auto_increment_col in provided_columns:
    value = provided value   // bypass — no counter update
  else:
    value = auto_increment_next(table_name)
    session.last_insert_id = value   // update only for generated IDs
```

`LAST_INSERT_ID()` is updated only when a value is auto-generated. Inserting
an explicit ID does not change the session's `last_insert_id`.

### Multi-Row INSERT

For `INSERT INTO t VALUES (...), (...), ...`, the executor calls
`auto_increment_next` once per row. `last_insert_id` is set to the value
generated for the **first** row before iterating through the rest:

```
ids = [auto_increment_next(t) for _ in rows]
session.last_insert_id = ids[0]   // MySQL semantics
insert all rows with their respective ids
```

### TRUNCATE — Sequence Reset

`TRUNCATE TABLE t` deletes all rows by scanning the `HeapChain` and marking
every visible row as deleted (same algorithm as `DELETE FROM t` without a
WHERE clause). After clearing the rows, it resets the sequence:

```
execute_truncate(table_name):
  for row in HeapChain::scan_visible(table_name, snapshot):
    storage.delete_row(row.record_id, txn_id)
  thread_local_map.remove(table_name)   // next insert re-initializes from MAX+1 = 1
  return QueryResult::Affected { count: 0 }
```

Removing the entry from the map forces a `MAX+1` re-initialization on the next
INSERT. Because the table is now empty, `MAX = 0`, so `next = 1`.

---

## SHOW TABLES / SHOW COLUMNS

### SHOW TABLES

`SHOW TABLES [FROM schema]` reads the catalog's table registry and returns one
row per table. The output column is named `Tables_in_<schema>`:

```
execute_show_tables(schema):
  tables = catalog.list_tables(schema)
  column_name = "Tables_in_" + schema
  return QueryResult::Rows { columns: [column_name], rows: [[t] for t in tables] }
```

### SHOW COLUMNS / DESCRIBE

`SHOW COLUMNS FROM t`, `DESCRIBE t`, and `DESC t` are all dispatched to the
same handler. The executor reads the column definitions from the catalog and
constructs a fixed six-column result set:

```
execute_show_columns(table_name):
  cols = catalog.get_table(table_name).columns
  for col in cols:
    Field   = col.name
    Type    = col.data_type.to_sql_string()
    Null    = if col.nullable { "YES" } else { "NO" }
    Key     = if col.is_primary_key { "PRI" } else { "" }
    Default = "NULL"   // stub
    Extra   = if col.auto_increment { "auto_increment" } else { "" }
  return six-column result set
```

The `Key` and `Default` fields are stubs: `Key` only reflects primary key
membership; composite keys, unique constraints, and foreign keys are not yet
surfaced. `Default` always shows `"NULL"` regardless of the declared default
expression. Full metadata exposure is planned for a later catalog enhancement.

---

## ALTER TABLE Execution

ALTER TABLE dispatches to one of four handlers depending on the operation.
Two of them (ADD COLUMN and DROP COLUMN) require rewriting every row in the
table. The other two (RENAME COLUMN and RENAME TO) touch only the catalog.

### Why Row Rewriting Is Needed

AxiomDB rows are stored as positional binary blobs. The null bitmap at the
start of each row has exactly `ceil(column_count / 8)` bytes — one bit per
column, in column-index order. Packed values follow immediately, with offsets
derived from the column types declared at write time.

```
Row layout (schema: id BIGINT, name TEXT, age INT):

  null_bitmap (1 byte)   [b0=id_null, b1=name_null, b2=age_null, ...]
  id   (8 bytes, LE i64) [only present if b0=0]
  name (4-byte len + UTF-8 bytes) [only present if b1=0]
  age  (4 bytes, LE i32) [only present if b2=0]
```

When the column count changes, the null bitmap size changes and all subsequent
offsets shift. A row written under the old schema cannot be decoded against the
new schema — the null bitmap has the wrong number of bits, and value positions
no longer align. Every row must therefore be rewritten to match the new layout.

`RENAME COLUMN` does not change column positions or types — only the name entry
in the catalog changes. `RENAME TO` changes only the table name in the catalog.
Neither operation touches row data.

### `rewrite_rows` Helper

Both ADD COLUMN and DROP COLUMN use a shared `rewrite_rows` path:

```
rewrite_rows(table_name, old_schema, new_schema, transform_fn):
  snapshot = txn.active_snapshot()
  old_rows = HeapChain::scan_visible(table_name, snapshot)

  for (record_id, old_row) in old_rows:
    new_row = transform_fn(old_row)   // apply per-operation transformation
    storage.delete_row(record_id, txn_id)
    storage.insert_row(table_name, encode_row(new_row, new_schema), txn_id)
```

The `transform_fn` is operation-specific:

| Operation | transform_fn |
|---|---|
| ADD COLUMN | Append `DEFAULT` value (or `NULL` if no default) to the end of the row |
| DROP COLUMN | Remove the value at `col_idx` from the row vector |

### Ordering Constraint — Catalog Before vs. After Rewrite

The ordering of the catalog update relative to the row rewrite is not arbitrary.
It is chosen so that a failure mid-rewrite leaves the database in a recoverable
state:

**ADD COLUMN — catalog update FIRST, then rewrite rows:**

```
1. catalog.add_column(table_name, new_column_def)
2. rewrite_rows(old_schema → new_schema, append DEFAULT)
```

If the process crashes after step 1 but before step 2 completes, the catalog
already reflects the new schema. The partially-rewritten rows are discarded by
crash recovery (their transactions are uncommitted). On restart, the table is
consistent: the new column exists in the catalog, and all rows either have been
fully rewritten (if the transaction committed) or none have been (if it was
rolled back).

**DROP COLUMN — rewrite rows FIRST, then update catalog:**

```
1. rewrite_rows(old_schema → new_schema, remove col at col_idx)
2. catalog.remove_column(table_name, col_idx)
```

If the process crashes after step 1 but before step 2, the rows have already
been written in the new (narrower) layout but the catalog still shows the old
schema. Recovery rolls back the uncommitted row rewrites and the catalog is
never touched — the table is fully consistent under the old schema.

The invariant is: **the catalog always describes rows that can be decoded.**
Swapping the order for either operation would create a window where the catalog
describes a schema that does not match the on-disk rows.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Asymmetric Catalog Ordering</span>
ADD COLUMN updates the catalog before rewriting rows; DROP COLUMN rewrites rows
before updating the catalog. The direction is chosen so that a mid-operation
crash always leaves the catalog consistent with whatever rows are on disk. For
ADD, a rolled-back partial rewrite leaves rows under the old (narrower) schema —
but the catalog already shows the new column, which is a problem. The solution
is that partial rewrites are uncommitted transactions and are invisible to crash
recovery, which only replays committed WAL entries. For DROP, partial rewrites
under the new (narrower) layout are also rolled back, and the catalog still
describes the old (wider) schema — fully decodable. This mirrors the ordering
used in PostgreSQL's heap rewrite path for ALTER TABLE operations.
</div>
</div>

### Session Cache Invalidation

The session holds a `SchemaCache` that maps table names to their column
definitions at the time the last query was prepared. After any ALTER TABLE
operation completes, the cache entry for the affected table is invalidated:

```
execute_alter_table(stmt):
  // ... perform operation (catalog update + optional row rewrite) ...
  session.schema_cache.invalidate(table_name)
```

This ensures that the next query against the altered table re-reads the catalog
and sees the updated column list, rather than operating on a stale schema that
may reference columns that no longer exist or omit newly added ones.
