# SQL Executor

The executor is the component that interprets an analyzed `Stmt` (all column
references resolved to `col_idx` by the semantic analyzer) and drives it to
completion, returning a `QueryResult`. It is the highest-level component in the
query pipeline.

Since subphase `5.19a`, the executor no longer lives in a single source file.
It is organized under `crates/axiomdb-sql/src/executor/` with `mod.rs` as the
stable facade and responsibility-based source files behind it.

## Source Layout

| File | Responsibility |
|---|---|
| `executor/mod.rs` | public facade, statement dispatch, thread-local last-insert-id |
| `executor/shared.rs` | helpers shared across multiple statement families |
| `executor/select.rs` | SELECT entrypoints, projection, ORDER BY/LIMIT wiring |
| `executor/joins.rs` | nested-loop join execution and join-specific metadata |
| `executor/aggregate.rs` | GROUP BY, aggregates, DISTINCT/group-key helpers |
| `executor/insert.rs` | INSERT and INSERT ... SELECT paths |
| `executor/update.rs` | UPDATE execution |
| `executor/delete.rs` | DELETE execution and candidate collection |
| `executor/bulk_empty.rs` | shared bulk-empty helpers for DELETE/TRUNCATE |
| `executor/ddl.rs` | DDL, SHOW, ANALYZE, TRUNCATE |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — File-Level Split First</span>
The first goal of `5.19a` was to make the executor readable and changeable without
changing SQL behavior. The split preserves the existing facade and keeps later DML
optimizations isolated from unrelated SELECT and DDL code.
</div>
</div>

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

## GROUP BY — Strategy Selection (Phase 4.9b)

The executor selects between two GROUP BY execution strategies at runtime:

| Strategy | When selected | Behavior |
|---|---|---|
| `Hash` | Default; JOINs; derived tables; plain scans | HashMap per group key; `O(k)` memory |
| `Sorted { presorted: true }` | Single-table ctx path + compatible B-Tree index | Stream adjacent equal groups; `O(1)` memory |

```rust
enum GroupByStrategy {
    Hash,
    Sorted { presorted: bool },
}
```

Strategy selection (`choose_group_by_strategy_ctx`) is only active on the
**single-table ctx path** (`execute_with_ctx`). All JOIN, derived-table, and
non-ctx paths use `Hash`.

### Prefix Match Rule

The sorted strategy is selected when all four conditions hold:

1. Access method is `IndexLookup`, `IndexRange`, or `IndexOnlyScan`.
2. Every `GROUP BY` expression is a plain `Expr::Column` (no function calls, no aliases).
3. The column references match the **leading key prefix** of the chosen index in the **same order**.
4. The prefix length ≤ number of index columns.

Examples (index `(region, dept)`):

| GROUP BY | Result |
|---|---|
| `region, dept` | ✅ Sorted |
| `region` | ✅ Sorted (prefix) |
| `dept, region` | ❌ Hash (wrong order) |
| `LOWER(region)` | ❌ Hash (computed expression) |

This is correct because `BTree::range_in` guarantees rows arrive in key order,
and equal leading prefixes are contiguous even with extra suffix columns or RID
suffixes on non-unique indexes.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Borrowed from PostgreSQL + DuckDB</span>
PostgreSQL keeps <code>GroupAggregate</code> (sorted) and <code>HashAggregate</code> as separate strategies selected at planning time (<code>pathnodes.h</code>). DuckDB selects the aggregation strategy at physical plan time based on input guarantees. AxiomDB borrows the two-strategy concept but selects at execution time using the already-chosen access method — no separate planner pass needed.
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

## GROUP BY — Sorted Streaming Executor (Phase 4.9b)

The sorted executor replaces the hash table with a single linear pass over
pre-ordered rows, accumulating state for the current group and emitting it
when the key changes.

### Algorithm

```
rows_with_keys = [(row, eval(group_by exprs, row)) for row in combined_rows]

if !presorted:
    stable_sort rows_with_keys by compare_group_key_lists

current_key   = rows_with_keys[0].key_values
current_accumulators = AggAccumulator::new() for each aggregate
update accumulators with rows_with_keys[0].row

for next in rows_with_keys[1..]:
    if group_keys_equal(current_key, next.key_values):
        update accumulators with next.row
    else:
        finalize → apply HAVING → emit output row
        reset: current_key = next.key_values, new accumulators, update

finalize last group
```

### Key Comparison

```rust
fn compare_group_key_lists(a: &[Value], b: &[Value]) -> Ordering
fn group_keys_equal(a: &[Value], b: &[Value]) -> bool
```

Uses `compare_values_null_last` so `NULL == NULL` for grouping (consistent with
the hash path's serialization). Comparison is left-to-right: returns the first
non-Equal ordering.

### Shared Aggregate Machinery

Both hash and sorted executors reuse the same:

- `AggAccumulator` (state, update, finalize)
- `eval_with_aggs` (HAVING evaluation)
- `project_grouped_row` (output projection)
- `build_grouped_column_meta` (column metadata)
- GROUP_CONCAT handling
- Post-group DISTINCT / ORDER BY / LIMIT

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Memory Advantage</span>
When the index already orders rows by the GROUP BY key prefix, the sorted executor uses <code>O(1)</code> accumulator memory (one group at a time) instead of <code>O(k)</code> where <code>k = distinct groups</code>. For a high-cardinality column with many distinct values, this eliminates the entire hash table allocation.
</div>
</div>

---

## ORDER BY — Multi-Column Sort

ORDER BY is applied **after scan + filter + aggregation but before projection**
for non-GROUP BY queries. For GROUP BY queries, it is applied to the projected
output rows after `remap_order_by_for_grouped` rewrites column references.

### ORDER BY in GROUP BY Context — Expression Remapping

Grouped output rows are indexed by SELECT output position: position 0 = first
SELECT item, position 1 = second, etc. ORDER BY expressions, however, are
analyzed against the source schema where `Expr::Column { col_idx }` refers to
the original table column.

`remap_order_by_for_grouped` fixes this mismatch before calling `apply_order_by`:

```
remap_order_by_for_grouped(order_by, select_items):
  for each ORDER BY item:
    rewrite expr via remap_expr_for_grouped(expr, select_items)

remap_expr_for_grouped(expr, select_items):
  if expr == select_items[pos].expr (structural PartialEq):
    return Column { col_idx: pos }   // output position
  match expr:
    BinaryOp → recurse into left, right
    UnaryOp  → recurse into operand
    IsNull   → recurse into inner
    Between  → recurse into expr, low, high
    Function → recurse into args
    other    → return unchanged
```

This means `ORDER BY dept` (where `dept` is `Expr::Column{col_idx:1}` in the
source) becomes `Expr::Column{col_idx:0}` when the SELECT is `SELECT dept, COUNT(*)`,
correctly indexing into the projected output row.

Aggregate expressions like `ORDER BY COUNT(*)` are matched structurally:
if `Expr::Function{name:"count", args:[]}` appears in the SELECT at position 1,
it is rewritten to `Expr::Column{col_idx:1}`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Expr PartialEq matching</span>
Rather than maintaining a separate alias/position resolution table, AxiomDB uses structural <code>PartialEq</code> on <code>Expr</code> (which is derived) to identify ORDER BY expressions that match SELECT items. This is simpler than PostgreSQL's SortClause/TargetEntry reference system and correct for the common cases (column references, aggregates, compound expressions).
</div>
</div>

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

## LIMIT / OFFSET — Row-Count Coercion (Phase 4.10d)

`apply_limit_offset` runs after ORDER BY and DISTINCT. It calls
`eval_row_count_as_usize` for each row-count expression.

### Row-count coercion contract

| Evaluated value | Result |
|---|---|
| `Int(n)` where `n ≥ 0` | `n as usize` |
| `BigInt(n)` where `n ≥ 0` | `usize::try_from(n)` — errors on overflow |
| `Text(s)` where `s.trim()` parses as an exact base-10 integer `≥ 0` | parsed value as `usize` |
| negative `Int` or `BigInt` | `DbError::TypeMismatch` |
| non-integral `Text` (`"10.1"`, `"1e3"`, `"abc"`) | `DbError::TypeMismatch` |
| `NULL`, `Bool`, `Real`, `Decimal`, `Date`, `Timestamp` | `DbError::TypeMismatch` |

Text coercion is intentionally narrow: only exact base-10 integers are accepted.
Scientific notation, decimal fractions, and time-like strings are all rejected.

### Why Text is accepted

The prepared-statement SQL-string substitution path serializes a `Value::Text("2")`
parameter as `LIMIT '2'` in the generated SQL. Without Text coercion, the fallback
path would always fail for string-bound LIMIT parameters — which is the binding
type used by some MariaDB clients. Accepting exact integer Text keeps the
cached-AST prepared path and the SQL-string fallback path on identical semantics.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
AxiomDB does not call the general <code>coerce()</code> function here.
<code>coerce()</code> uses assignment-coercion semantics and would change the
error class to <code>InvalidCoercion</code>, masking the semantic error.
<code>eval_row_count_as_usize</code> implements the narrower 4.10d contract
directly in the executor, keeping the error class and message family consistent
for both prepared paths.
</div>
</div>

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

## Foreign Key Enforcement

FK constraints are validated during DML operations by `crates/axiomdb-sql/src/fk_enforcement.rs`.

### Catalog Storage

Each FK is stored as a `FkDef` row in the `axiom_foreign_keys` heap (5th system table,
root page at meta offset 84). Fields:

```
fk_id, child_table_id, child_col_idx, parent_table_id, parent_col_idx,
on_delete: FkAction, on_update: FkAction, fk_index_id: u32, name: String
```

`FkAction` encoding: 0=NoAction, 1=Restrict, 2=Cascade, 3=SetNull, 4=SetDefault.
`fk_index_id != 0` → FK auto-index exists (composite key, Phase 6.9).
`fk_index_id = 0` → no auto-index; enforcement falls back to full table scan.

### FK auto-index — composite key `(fk_val | RecordId)` (Phase 6.9)

Each FK constraint auto-creates a B-Tree index on the child FK column using a
composite key format that makes every entry globally unique:

```
key = encode_index_key(&[fk_val]) ++ encode_rid(rid)  (10 bytes RecordId suffix)
```

This follows **InnoDB's approach** of appending the primary key as a tiebreaker
(`row0row.cc`). Every entry is unique even when many rows share the same FK value.

Range scan for all children with a given parent key:
```rust
lo = encode_index_key(&[parent_key]) ++ [0x00; 10]  // smallest RecordId
hi = encode_index_key(&[parent_key]) ++ [0xFF; 10]  // largest RecordId
children = BTree::range_in(fk_index_root, lo, hi)   // O(log n + k)
```

### INSERT / UPDATE child — `check_fk_child_insert`

```
For each FK on the child table:
  1. FK column is NULL → skip (MATCH SIMPLE)
  2. Encode FK value as B-Tree key
  3. Find parent's PK or UNIQUE index covering parent_col_idx
  4. Bloom shortcut: if filter says absent → ForeignKeyViolation immediately
  5. BTree::lookup_in(parent_index_root, key) — O(log n)
  6. No match → ForeignKeyViolation (SQLSTATE 23503)
```

PK indexes are populated on every INSERT since Phase 6.9 (removed `!is_primary`
filter in `insert_into_indexes`). All index types now use B-Tree + Bloom lookup.

### DELETE parent — `enforce_fk_on_parent_delete`

Called **before** the parent rows are deleted. For each FK referencing this table:

| Action | Behavior |
|--------|---------|
| RESTRICT / NO ACTION | `BTree::range_in(fk_index)` — O(log n); error if any child found |
| CASCADE | Range scan finds all children; recursive delete (depth limit = 10) |
| SET NULL | Range scan finds all children; updates FK column to NULL |

Cascade recursion uses `depth` parameter — exceeding 10 levels returns
`ForeignKeyCascadeDepth` (SQLSTATE 23503).

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
Phase 6.9 replaced full table scans with B-Tree range scans for FK enforcement.
RESTRICT check: O(log n) vs O(n). CASCADE with 1,000 children: O(log n + 1000)
vs O(n × 1000). This follows InnoDB's composite secondary index approach
(`dict_foreign_t.foreign_index`) rather than PostgreSQL's trigger-based strategy.
</div>
</div>

---

## Query Planner Cost Gate (Phase 6.10)

Before returning `IndexLookup` or `IndexRange`, `plan_select` applies a cost gate
using per-column statistics to decide if the index scan is worth the overhead.

### Algorithm

```
ndv = stats.ndv > 0 ? stats.ndv : DEFAULT_NUM_DISTINCT (= 200)
selectivity = 1.0 / ndv        // equality predicate: 1/ndv rows match
if selectivity > 0.20:
    return Scan                 // too many rows — full scan is cheaper
if stats.row_count < 1,000:
    return Scan                 // tiny table — index overhead not worth it
return IndexLookup / IndexRange // selective enough — use index
```

Constants derived from PostgreSQL:
- `INDEX_SELECTIVITY_THRESHOLD = 0.20` (PG default: `seq/random_page_cost = 0.25`; AxiomDB is slightly more conservative for embedded storage)
- `DEFAULT_NUM_DISTINCT = 200` (PG `DEFAULT_NUM_DISTINCT` in `selfuncs.c`)

### Stats are loaded once per SELECT

In `execute_select_ctx`, before calling `plan_select`:

```rust
let table_stats = CatalogReader::new(storage, snap)?.list_stats(table_id)?;
let access_method = plan_select(where_clause, indexes, columns, table_id,
                                &table_stats, &mut ctx.stats);
```

If `table_stats` is empty (pre-6.10 database or ANALYZE never run),
`plan_select` conservatively uses the index — never wrong, just possibly suboptimal.

### Staleness (`StaleStatsTracker`)

`StaleStatsTracker` lives in `SessionContext` and tracks row changes per table:

```
INSERT / DELETE row  → on_row_changed(table_id)
changes > 20% of baseline  → mark stale
planner loads stats  → set_baseline(table_id, row_count)
ANALYZE TABLE        → mark_fresh(table_id)
```

When stale, the planner uses `ndv = DEFAULT_NUM_DISTINCT = 200` regardless of
catalog stats, preventing stale low-NDV estimates from causing full scans on
high-selectivity columns.

---

## Bloom Filter — Index Lookup Shortcut

The executor holds a `BloomRegistry` (one per database connection) that maps
`index_id → Bloom<Vec<u8>>`. Before performing any B-Tree lookup for an index
equality predicate, the executor consults the filter:

```rust
// In execute_select_ctx — IndexLookup path
if !bloom.might_exist(index_def.index_id, &encoded_key) {
    // Definite absence: skip B-Tree entirely.
    return Ok(vec![]);
}
// False positive or true positive: proceed with B-Tree.
BTree::lookup_in(storage, index_def.root_page_id, &encoded_key)?
```

### BloomRegistry API

```rust
pub struct BloomRegistry { /* per-index filters */ }

impl BloomRegistry {
    pub fn create(&mut self, index_id: u32, expected_items: usize);
    pub fn add(&mut self, index_id: u32, key: &[u8]);
    pub fn might_exist(&self, index_id: u32, key: &[u8]) -> bool;
    pub fn mark_dirty(&mut self, index_id: u32);
    pub fn remove(&mut self, index_id: u32);
}
```

`might_exist` returns `true` (conservative) for unknown `index_id`s — correct
behavior for indexes that existed before the current server session (no filter
was populated for them at startup).

### DML Integration

Every DML handler in the `execute_with_ctx` path updates the registry:

| Handler | Bloom action |
|---------|-------------|
| `execute_insert_ctx` | `bloom.add(index_id, &key)` after each B-Tree insert |
| `execute_update_ctx` | `mark_dirty()` for delete side; `add()` for insert side |
| `execute_delete_ctx` | `mark_dirty(index_id)` per deleted row |
| `execute_create_index` | `create(index_id, n)` then `add()` for every existing key |
| `execute_drop_index` | `remove(index_id)` |

### Memory Budget

Each filter is sized at `max(2 × expected_items, 1000)` with a 1% FPR target
(~9.6 bits/key, 7 hash functions). For a 1M-row table with one secondary index:
2M × 9.6 bits ≈ **2.4 MB**.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Standard Bloom, Not Counting</span>
A standard (non-counting) Bloom filter is used instead of a counting variant.
Deleted keys cannot be removed — the filter is marked <em>dirty</em> instead.
This avoids the 4× memory overhead of counting Bloom filters (used by Apache
Cassandra and some RocksDB SST configurations) while maintaining full
correctness: dirty filters produce more false positives but never false
negatives. Reconstruction is deferred to <code>ANALYZE TABLE</code> (Phase 6.12),
mirroring PostgreSQL's lazy statistics-rebuild model.
</div>
</div>

---

## IndexOnlyScan — Heap-Free Execution

When `plan_select` returns `AccessMethod::IndexOnlyScan`, the executor reads
all result values directly from the B-Tree key bytes, with only a lightweight
MVCC visibility check against the heap slot header.

### Execution Path

```
IndexOnlyScan { index_def, lo, hi, n_key_cols, needed_key_positions }:

for (rid, key_bytes) in BTree::range_in(storage, index_def.root_page_id, lo, hi):
  page_id = rid.page_id
  slot_id = rid.slot_id

  // MVCC: read only the 24-byte RowHeader — no full row decode.
  visible = HeapChain::is_slot_visible(storage, page_id, slot_id, snap)
  if !visible:
    continue

  // Extract column values from B-Tree key bytes (no heap page needed).
  (decoded_cols, _) = decode_index_key(&key_bytes, n_key_cols)

  // Project only the columns the query requested.
  row = needed_key_positions.iter().map(|&p| decoded_cols[p].clone()).collect()
  emit row
```

The 24-byte `RowHeader` contains `txn_id_created`, `txn_id_deleted`, and a
sequence number — enough for full MVCC visibility evaluation without loading
the row payload.

### `decode_index_key` — Self-Delimiting Key Decoder

`decode_index_key` lives in `key_encoding.rs` and is the exact inverse of
`encode_index_key`. It uses type tags embedded in the key bytes to self-delimit
each value without needing an external schema:

| Tag byte | Type | Encoding |
|---|---|---|
| `0x00` | NULL | tag only, 0 payload bytes |
| `0x01` | Bool | tag + 1 byte (0 = false, 1 = true) |
| `0x02` | Int (positive, 1 B) | tag + 1 LE byte |
| `0x03` | Int (positive, 2 B) | tag + 2 LE bytes |
| `0x04` | Int (positive, 4 B) | tag + 4 LE bytes |
| `0x05` | Int (negative, 4 B) | tag + 4 LE bytes (i32) |
| `0x06` | BigInt (positive, 1 B) | tag + 1 byte |
| `0x07` | BigInt (positive, 4 B) | tag + 4 LE bytes |
| `0x08` | BigInt (positive, 8 B) | tag + 8 LE bytes |
| `0x09` | BigInt (negative, 8 B) | tag + 8 LE bytes (i64) |
| `0x0A` | Real | tag + 8 LE bytes (f64 bits) |
| `0x0B` | Text | tag + NUL-terminated UTF-8 (NUL = end marker) |
| `0x0C` | Bytes | tag + NUL-escaped bytes (0x00 → [0x00, 0xFF], NUL terminator = [0x00, 0x00]) |

```rust
// Signature
pub fn decode_index_key(key: &[u8], n_cols: usize) -> (Vec<Value>, usize)
// Returns: (decoded column values, total bytes consumed)
```

The self-delimiting format means `decode_index_key` requires no column type
metadata — the tag bytes carry all necessary type information. This is the
same approach used by SQLite's record format and RocksDB's comparator-encoded
keys.

### Full-Width Row Layout in IndexOnlyScan Output

IndexOnlyScan emits **full-width rows** — the same width as a heap row — with
index key column values placed at their table `col_idx` positions and `NULL`
everywhere else. This is required because downstream operators (WHERE
re-evaluation, projection, expression evaluator) all address columns by their
original table column index, not by SELECT output position.

```
table: (id INT [0], name TEXT [1], age INT [2], dept TEXT [3])
index: ON (age, dept)  ← covers col_idx 2 and 3

IndexOnlyScan emits: [NULL, NULL, <age_val>, <dept_val>]
                      col0  col1    col2         col3
```

If the executor placed decoded values at positions `0, 1, ...` instead, a
`WHERE age > 25` re-evaluation would read `col_idx=2` from a 2-element row and
panic with `ColumnIndexOutOfBounds`. The full-width layout eliminates this class
of error entirely.

### `execute_with_ctx` — Required for IndexOnlyScan Selection

The planner selects `IndexOnlyScan` only when `select_col_idxs` (the set of
columns touched by the query) is a subset of the index's key columns. The
`select_col_idxs` argument is supplied by `execute_with_ctx`; the simpler
`execute` entry-point passes an empty slice, so IndexOnlyScan is never selected
through it.

Test coverage for this path lives in
`crates/axiomdb-sql/tests/integration_index_only.rs` — functions prefixed
`test_ctx_` use `execute_with_ctx` with real `select_col_idxs` and are the
only tests that exercise the `IndexOnlyScan` access method end-to-end.

---

## Non-Unique Secondary Index Key Format

Non-unique secondary indexes append a 10-byte `RecordId` suffix to every
B-Tree key to guarantee uniqueness across all entries:

```
key = encode_index_key(col_vals) || encode_rid(rid)
                                    ^^^^^^^^^^^^^^
                                    page_id (4 B) + slot_id (2 B) + seq (4 B) = 10 bytes
```

This prevents `DuplicateKey` errors when two rows share the same indexed value,
because the `RecordId` suffix always makes the full key distinct.

### Lookup Bounds for Non-Unique Indexes

To find all rows matching a specific indexed value, the executor performs a
range scan using synthetic `[lo, hi]` bounds that span all possible `RecordId`
suffixes:

```rust
lo = encode_index_key(&[val]) + [0x00; 10]   // smallest RecordId
hi = encode_index_key(&[val]) + [0xFF; 10]   // largest RecordId
BTree::range_in(root, lo, hi)                // returns all entries for val
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — InnoDB Secondary Index Approach</span>
MySQL InnoDB secondary indexes append the primary key as a tiebreaker in every
non-unique B-Tree entry (<code>row0row.cc</code>). AxiomDB uses
<code>RecordId</code> (page_id + slot_id + sequence) instead of a separate
primary key column, keeping the suffix at a fixed 10 bytes regardless of the
table's key type — simpler to encode and guaranteed to be globally unique within
the storage engine's address space.
</div>
</div>

---

## Performance Characteristics

| Operation | Time complexity | Notes |
|---|---|---|
| Table scan | O(n) | HeapChain linear traversal |
| Nested loop JOIN | O(n × m) | Both sides materialized before loop |
| Hash GROUP BY | O(n) | One pass; O(k) memory where k = distinct groups |
| Sorted GROUP BY | O(n) | One pass; O(1) accumulator memory per group |
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

#### Index root invalidation on B+tree split

The `SchemaCache` also stores `IndexDef.root_page_id` for each index. When an
INSERT causes the B+tree root to split, `insert_in` allocates a new root page
and frees the old one. After this, the cached `root_page_id` points to a freed
page. If the cache is not invalidated, the next `execute_insert_ctx` call reads
`IndexDef.root_page_id` from the cache and passes it to `BTree::lookup_in`
(uniqueness check), causing a stale-pointer read on a freed page.

The fix: call `ctx.invalidate_all()` whenever any index root changes during
INSERT or DELETE index maintenance. This forces re-resolution from the catalog
(which always has the current `root_page_id`) on the next DML statement.

The same fix applies to `execute_delete_ctx` (DELETE WHERE path): the
`secondary_indexes` slice must be kept in sync within the per-row loop so
that each subsequent row's deletion starts from the current root, not the
stale freed one. Without this, a root collapse on row N causes row N+1 to
start from a freed page, triggering a double-free in the B+tree freelist.

```
// After each updated root in execute_insert_ctx / execute_delete:
catalog.update_index_root(index_id, new_root)
secondary_indexes[i].root_page_id = new_root   // sync in-memory slice
ctx.invalidate_all()                            // drop cached IndexDefs
```

This only fires O(log N) times per batch (once per root split), so the
performance impact is limited to a single catalog re-read per split event.

---

## Strict Mode and Warning 1265

`SessionContext.strict_mode` is a `bool` flag (default `true`) that controls
how `INSERT` and `UPDATE` column coercion failures are handled.

### Coercion paths

```
INSERT / UPDATE column value assignment:
  if ctx.strict_mode:
    coerce(value, target_type, CoercionMode::Strict)
      → Ok(v)    : use v
      → Err(e)   : return Err immediately (SQLSTATE 22018)
  else:
    coerce(value, target_type, CoercionMode::Strict)
      → Ok(v)    : use v  (no warning — strict succeeded)
      → Err(_)   : try CoercionMode::Permissive
          → Ok(v) : use v, emit ctx.warn(1265, "Data truncated for column '<col>' at row <n>")
          → Err(e): return Err (both paths failed)
```

`CoercionMode::Permissive` performs best-effort conversion: `'42abc'` → `42`,
`'abc'` → `0`, overflowing integers clamped to the type bounds.

### Row numbering

`insert_row_with_ctx` and `insert_rows_batch_with_ctx` accept an explicit
`row_num: usize` (1-based). The VALUES loop in `execute_insert_ctx` passes
`row_idx + 1` from `enumerate()`:

```rust
for (row_idx, value_exprs) in rows.into_iter().enumerate() {
    let values = eval_value_exprs(value_exprs, ...)?;
    engine.insert_row_with_ctx(&mut ctx, values, row_idx + 1)?;
}
```

This makes warning 1265 messages meaningful for multi-row inserts:
`"Data truncated for column 'stock' at row 2"`.

### SET strict_mode / SET sql_mode

The executor intercepts `SET strict_mode` and `SET sql_mode` in `execute_set_ctx`
(called from `dispatch_ctx`). It delegates to helpers from `session.rs`:

```rust
"strict_mode" => {
    let b = parse_boolish_setting(&raw)?;
    ctx.strict_mode = b;
}
"sql_mode" => {
    let normalized = normalize_sql_mode(&raw);
    ctx.strict_mode = sql_mode_is_strict(&normalized);
}
```

The wire layer (`handler.rs`) syncs the wire-visible `@@sql_mode` and
`@@strict_mode` variables with the session `bool` after every intercepted SET.
Both variables are surfaced in `SHOW VARIABLES`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Try Strict First, Then Permissive</span>
In permissive mode, AxiomDB always tries strict coercion first. A warning is
only emitted when the strict path fails and the permissive path succeeds.
This means values that coerce cleanly in strict mode (e.g. <code>'42'</code> →
<code>42</code>) never generate a warning in either mode — matching MySQL 8's
behavior where warning 1265 is reserved for actual data loss, not clean widening.
</div>
</div>
