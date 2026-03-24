# Indexes

Indexes are B+ Tree data structures that allow AxiomDB to find rows matching a
condition without scanning the entire table. Every index is a Copy-on-Write B+ Tree
stored in the same `.db` file as the table data.

---

## Automatic Indexes

AxiomDB automatically creates a unique B+ Tree index for:

- Every `PRIMARY KEY` declaration
- Every `UNIQUE` column constraint or `UNIQUE` table constraint

These indexes are created at `CREATE TABLE` time and cannot be dropped without
dropping the corresponding constraint.

---

## Creating Indexes Manually

```sql
CREATE [UNIQUE] INDEX index_name ON table_name (col1 [ASC|DESC], col2 ...);
CREATE INDEX idx_users_name   ON users   (name);
CREATE INDEX idx_orders_user  ON orders  (user_id, placed_at DESC);
CREATE UNIQUE INDEX uq_sku    ON products (sku);
```

See [DDL — CREATE INDEX](../sql-reference/ddl.md#create-index) for the full syntax.

---

## When Indexes Help

The query planner considers an index when:

1. The leading column(s) of the index appear in a `WHERE` equality or range condition.
2. The index columns match the `ORDER BY` direction and order (avoids a sort step).
3. The index is selective enough that scanning it is cheaper than a full table scan.

```sql
-- Good: leading column (user_id) used in WHERE
CREATE INDEX idx_orders_user ON orders (user_id, placed_at DESC);
SELECT * FROM orders WHERE user_id = 42 ORDER BY placed_at DESC;

-- Bad: leading column not in WHERE — index not used
SELECT * FROM orders WHERE placed_at > '2026-01-01';
-- Solution: create a separate index on placed_at
CREATE INDEX idx_orders_date ON orders (placed_at);
```

---

## Composite Index Column Order

The order of columns in a composite index determines which query patterns it
accelerates. The B+ Tree is sorted by the concatenated key `(col1, col2, ...)`.

```sql
CREATE INDEX idx_orders_user_status ON orders (user_id, status);
```

This index accelerates:
- `WHERE user_id = 42`
- `WHERE user_id = 42 AND status = 'paid'`

This index does NOT accelerate:
- `WHERE status = 'paid'` (leading column not constrained)

Rule of thumb: put the highest-selectivity, most frequently filtered column first.

---

## Partial Indexes

A partial index covers only the rows matching a `WHERE` predicate. This reduces index
size and maintenance cost.

```sql
-- Index only pending orders (the common access pattern)
CREATE INDEX idx_pending_orders ON orders (user_id)
WHERE status = 'pending';

-- Index only non-deleted users
CREATE INDEX idx_active_users ON users (email)
WHERE deleted_at IS NULL;
```

The query planner uses a partial index only when the query's WHERE clause implies
the index's predicate.

---

## Index Key Size Limit

The B+ Tree stores encoded keys up to **768 bytes**. For most column types this is
never an issue:

- `INT`, `BIGINT`, `UUID`, `TIMESTAMP` — fixed-size, always well under the limit.
- `TEXT`, `VARCHAR` — a 760-character value will just fit. If you index a column
  with very long strings (> 750 characters), rows exceeding the limit are silently
  skipped at `CREATE INDEX` time and return `IndexKeyTooLong` on INSERT.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
768 bytes is chosen to match the page-layout constant `MAX_KEY_LEN` (derived so that
`ORDER_LEAF × 768 + overhead ≤ 16 384 bytes`). Unlike MySQL InnoDB which silently
truncates long keys (leading to false positives on lookup), AxiomDB rejects
oversized keys at write time — correctness is never compromised.
</div>
</div>

---

## Query Planner — Phase 6.3

The planner rewrites the execution plan before running the scan. Currently recognized patterns:

**Equality lookup** — exact match on the leading indexed column:
```sql
-- Uses B-Tree point lookup (O(log n) instead of O(n))
SELECT * FROM users WHERE email = 'alice@example.com';
SELECT * FROM orders WHERE id = 42;
```

**Range scan** — upper and lower bound on the leading indexed column:
```sql
-- Uses B-Tree range scan
SELECT * FROM orders WHERE created_at > '2024-01-01' AND created_at < '2025-01-01';
SELECT * FROM products WHERE price >= 10.0 AND price <= 50.0;
```

**Full scan fallback** — any pattern not recognized above:
```sql
-- Falls back to full table scan (no index for OR, function, or non-leading column)
SELECT * FROM users WHERE email LIKE '%gmail.com';
SELECT * FROM orders WHERE status = 'paid' OR total > 1000;
```

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
A point lookup on a 1M-row table takes O(log n) ≈ 20 page reads vs O(n) = 1M reads
for a full scan — roughly a 50,000× reduction in I/O. AxiomDB's planner applies this
automatically when a matching secondary index exists, with zero configuration required.
</div>
</div>

---

## Foreign Key Constraints

Foreign key constraints ensure referential integrity between tables. Every non-NULL
value in the FK column of the child table must reference an existing row in the
parent table.

```sql
-- Inline REFERENCES syntax
CREATE TABLE orders (
  id      INT PRIMARY KEY,
  user_id INT REFERENCES users(id) ON DELETE CASCADE
);

-- Table-level FOREIGN KEY syntax
CREATE TABLE order_items (
  id         INT PRIMARY KEY,
  order_id   INT,
  product_id INT,
  CONSTRAINT fk_order   FOREIGN KEY (order_id)   REFERENCES orders(id)   ON DELETE CASCADE,
  CONSTRAINT fk_product FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);

-- Add FK after the fact
ALTER TABLE orders
  ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id);

-- Remove a FK constraint
ALTER TABLE orders DROP CONSTRAINT fk_user;
```

### ON DELETE Actions

| Action | Behavior |
|--------|----------|
| `RESTRICT` / `NO ACTION` (default) | Error if child rows reference the deleted parent row |
| `CASCADE` | Automatically delete all child rows (recursive, max depth 10) |
| `SET NULL` | Set child FK column to NULL (column must be nullable) |

### NULL FK Values

A NULL value in a FK column is always allowed — it does not reference any parent row.
This follows SQL standard MATCH SIMPLE semantics.

### Phase 6.5 Limitations

- Only single-column FKs are supported. Composite FKs are planned for Phase 6.9.
- `ON UPDATE CASCADE` / `ON UPDATE SET NULL` are planned for Phase 6.9.
- FK enforcement uses a full table scan of the parent for validation. An optimized
  index-based check is planned for Phase 6.9.

---

## Bloom Filter Optimization

AxiomDB maintains an in-memory **Bloom filter** for each secondary index. The
filter allows the query executor to skip B-Tree page reads entirely when a
lookup key is **definitively absent** from the index.

### How It Works

When the planner chooses an index lookup for a `WHERE col = value` condition,
the executor checks the Bloom filter before touching the B-Tree:

- Filter says **no** → key is 100% absent. Zero B-Tree pages read. Empty result
  returned immediately.
- Filter says **maybe** → normal B-Tree lookup proceeds.

The filter is a probabilistic data structure: it never produces false negatives
(a key that exists will always get a "maybe"), but can produce false positives
(a key that does not exist may occasionally get a "maybe" instead of "no"). The
false positive rate is tuned to **1%** — at most 1 in 100 absent-key lookups
will still read the B-Tree.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
For workloads where many queries look up keys that do not exist (authentication
checks, cache-miss patterns, soft-delete queries), the Bloom filter eliminates
all B-Tree I/O for ~99% of misses. A B-Tree point lookup on a 1M-row table
reads ~20 pages; with a Bloom filter hit, it reads zero.
</div>
</div>

### Lifecycle

| Event | Effect on Bloom filter |
|-------|----------------------|
| `CREATE INDEX` | Filter created and populated with all existing keys |
| `INSERT` | New key added to filter |
| `UPDATE` | Old key marks filter dirty; new key added |
| `DELETE` | Filter marked dirty (deleted keys cannot be removed from a standard Bloom filter) |
| `DROP INDEX` | Filter removed from memory |
| Server restart | Filters start empty; `might_exist` returns `true` (conservative) until `CREATE INDEX` is run again |

### Dirty Filters

After a `DELETE` or `UPDATE`, the filter is marked **dirty**: it may still
return "maybe" for keys that were deleted. This does not affect correctness —
the B-Tree lookup simply finds no matching row. It only means that some absent
keys may not benefit from the zero-I/O shortcut until the filter is rebuilt via
`ANALYZE TABLE` (planned for Phase 6.12).

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip</span>
The Bloom filter is most effective on tables where reads vastly outnumber
deletes. For high-churn tables (frequent INSERT + DELETE cycles), run
<code>ANALYZE TABLE t</code> periodically to rebuild the filter and restore
optimal miss performance.
</div>
</div>

---

## Dropping an Index

```sql
-- MySQL syntax (required when the server is in MySQL wire protocol mode)
DROP INDEX index_name ON table_name;
DROP INDEX IF EXISTS idx_old ON table_name;
```

Dropping an index frees all B-Tree pages, reclaiming disk space immediately.

Dropping an index that backs a PRIMARY KEY or UNIQUE constraint requires dropping the
constraint first (via `ALTER TABLE DROP CONSTRAINT`).

---

## Index Introspection

```sql
-- All indexes on a table
SELECT index_name, is_unique, is_primary, columns
FROM axiom_indexes
WHERE table_name = 'orders'
ORDER BY is_primary DESC, index_name;

-- Root page of each index (useful for storage analysis)
SELECT index_name, root_page_id
FROM axiom_indexes;
```

---

## B+ Tree Implementation Details

AxiomDB's B+ Tree is a Copy-on-Write structure backed by the `StorageEngine` trait.
Key properties:

- **ORDER_INTERNAL = 223**: up to 223 separator keys and 224 child pointers per internal node
- **ORDER_LEAF = 217**: up to 217 (key, RecordId) pairs per leaf node
- **16 KB pages**: both internal and leaf nodes fit exactly in one page
- **AtomicU64 root**: root page swapped atomically — readers are lock-free
- **CoW semantics**: writes copy the path from root to the modified leaf; old versions
  are visible to concurrent readers until they finish

See [B+ Tree Internals](../../internals/btree.md) for the on-disk format and the
derivation of the ORDER constants.
