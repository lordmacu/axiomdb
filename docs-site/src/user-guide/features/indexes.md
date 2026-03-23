# Indexes

Indexes are B+ Tree data structures that allow NexusDB to find rows matching a
condition without scanning the entire table. Every index is a Copy-on-Write B+ Tree
stored in the same `.db` file as the table data.

---

## Automatic Indexes

NexusDB automatically creates a unique B+ Tree index for:

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

The B+ Tree stores keys up to **64 bytes**. If the indexed column value exceeds 64
bytes, NexusDB truncates the key and stores the full value in the heap row for
tie-breaking. This means:

- All `BIGINT`, `INT`, `UUID`, and other fixed-size types fit comfortably.
- Short `TEXT` and `VARCHAR` values fit (ASCII names, emails up to 64 chars).
- Very long text values are truncated in the index but remain correct for lookup
  (the engine verifies the heap value after using the index).

---

## Dropping an Index

```sql
DROP INDEX index_name;
DROP INDEX IF EXISTS idx_old;
```

Dropping an index that backs a PRIMARY KEY or UNIQUE constraint requires dropping the
constraint first (via `ALTER TABLE DROP CONSTRAINT`).

---

## Index Introspection

```sql
-- All indexes on a table
SELECT index_name, is_unique, is_primary, columns
FROM nexus_indexes
WHERE table_name = 'orders'
ORDER BY is_primary DESC, index_name;

-- Root page of each index (useful for storage analysis)
SELECT index_name, root_page_id
FROM nexus_indexes;
```

---

## B+ Tree Implementation Details

NexusDB's B+ Tree is a Copy-on-Write structure backed by the `StorageEngine` trait.
Key properties:

- **ORDER_INTERNAL = 223**: up to 223 separator keys and 224 child pointers per internal node
- **ORDER_LEAF = 217**: up to 217 (key, RecordId) pairs per leaf node
- **16 KB pages**: both internal and leaf nodes fit exactly in one page
- **AtomicU64 root**: root page swapped atomically — readers are lock-free
- **CoW semantics**: writes copy the path from root to the modified leaf; old versions
  are visible to concurrent readers until they finish

See [B+ Tree Internals](../../internals/btree.md) for the on-disk format and the
derivation of the ORDER constants.
