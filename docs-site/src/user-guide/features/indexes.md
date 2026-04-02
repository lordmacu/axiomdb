# Indexes

Indexes are B+ Tree data structures that allow AxiomDB to find rows matching a
condition without scanning the entire table. Every index is a Copy-on-Write B+ Tree
stored in the same `.db` file as the table data.

## Current Storage Model

Today, SQL tables still use the classic **heap + index** layout:

- the PRIMARY KEY B+ Tree finds a row location
- the engine then reads the heap row itself

Phase 39 is building clustered storage internally, but there is not yet a
user-visible table option that stores full rows inside PRIMARY KEY leaves. For
now, all user-visible tables still behave as heap-backed tables.

Internally, the storage rewrite already has clustered insert, point lookup,
range-scan, and same-leaf update primitives, but they are not wired into SQL yet.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Current Behavior</span>
If you compare AxiomDB with InnoDB or MariaDB today, remember that SQL-visible PRIMARY KEY lookups still use an index lookup followed by a heap fetch. The clustered-table rewrite is in progress internally, not exposed at the SQL surface yet.
</div>
</div>

---

## Index Statistics and Query Planner

AxiomDB maintains per-column statistics to help the query planner choose
between an index scan and a full table scan.

### How it works

When you create an index, AxiomDB automatically computes:
- **`row_count`** — total visible rows in the table
- **`ndv`** (number of distinct values) — exact count of distinct non-NULL values

The planner uses `selectivity = 1 / ndv` for equality predicates. If
`selectivity > 20%` of rows would be returned, a full table scan is cheaper
than an index scan, so the planner uses the table scan.

```
ndv = 3,   rows = 10,000  →  selectivity = 33%  > 20%  →  Scan
ndv = 100, rows = 10,000  →  selectivity = 1%   < 20%  →  Index
```

### ANALYZE command

Run `ANALYZE` to refresh statistics after bulk inserts or deletes:

```sql
-- Analyze a specific table (all indexed columns)
ANALYZE TABLE users;

-- Analyze a specific column only
ANALYZE TABLE orders (status);
```

Statistics are automatically computed at `CREATE INDEX` time. Run `ANALYZE` when:
- Significant data was added after the index was created
- Query plans seem wrong (e.g., full scan when index would be faster)

### Automatic staleness detection

After enough row changes (>20% of the analyzed row count), the planner
automatically uses conservative defaults (`ndv = 200`) until the next `ANALYZE`.
This prevents stale statistics from causing poor query plans.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Exact NDV, No Sampling</span>
AxiomDB computes exact distinct value counts (no sampling). PostgreSQL uses
Vitter's reservoir sampling algorithm for large tables. Exact counting is
simpler and correct for the typical table sizes of an embedded database.
Reservoir sampling (Duj1 estimator) is planned for a future statistics phase
when tables exceed 1M rows.
</div>
</div>

---

## Composite Indexes

A composite index covers two or more columns. The query planner uses it when
the WHERE clause contains equality conditions on the leading columns.

```sql
CREATE INDEX idx_user_status ON orders(user_id, status);

-- Uses composite index: both leading columns matched
SELECT * FROM orders WHERE user_id = 42 AND status = 'active';

-- Also uses index via prefix scan: leading column only
SELECT * FROM orders WHERE user_id = 42;

-- Does NOT use index: leading column absent from WHERE
SELECT * FROM orders WHERE status = 'active';
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Prefix Scan for Leading Column</span>
When only the leading column is in the WHERE clause, AxiomDB performs a B-Tree
range scan (prefix scan) rather than an exact lookup. This correctly returns all
rows matching the leading column, at the cost of a slightly wider range scan
vs. a point lookup. PostgreSQL uses the same strategy for index scans on the
leading column of a composite index.
</div>
</div>

---

## Fill Factor

Fill factor controls how full a B-Tree leaf page is allowed to get before it
splits. A lower fill factor leaves intentional free space on each page, reducing
split frequency for workloads that add rows after index creation.

```sql
-- Append-heavy time-series table: pages fill to 70% before splitting.
CREATE INDEX idx_ts ON events(created_at) WITH (fillfactor = 70);

-- Compact read-only index: fill pages completely.
CREATE UNIQUE INDEX uq_email ON users(email) WITH (fillfactor = 100);

-- Default (90%) — equivalent to omitting WITH:
CREATE INDEX idx_x ON t(x);
```

### Range and default

Valid range: **10–100**. Default: **90** (matches PostgreSQL's
`BTREE_DEFAULT_FILLFACTOR`). `fillfactor = 100` reproduces the current behavior
exactly — pages fill completely before splitting.

### Effect on splits

With `fillfactor = F`:
- Leaf page splits when it reaches `⌈F × ORDER_LEAF / 100⌉` entries
  (instead of at full capacity).
- After a split, both new pages hold roughly `F/2 %` of capacity —
  leaving room for future inserts without triggering another split.
- Internal pages always fill to capacity (not user-configurable).

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
For append-heavy tables (time-series, log tables, auto-increment keys), a fill
factor of 70–80 reduces split frequency during inserts because each page has
20–30% free space instead of splitting immediately on the next insert. This
lowers write amplification for sequential insert workloads — an optimization
also used by PostgreSQL and MariaDB InnoDB for INSERT-heavy indexes.
</div>
</div>

---

## Automatic Indexes

AxiomDB automatically creates a unique B+ Tree index for:

- Every `PRIMARY KEY` declaration
- Every `UNIQUE` column constraint or `UNIQUE` table constraint

These indexes are created at `CREATE TABLE` time and cannot be dropped without
dropping the corresponding constraint.

## Multi-row INSERT on Indexed Tables

Multi-row `INSERT ... VALUES (...), (... )` statements now stay on a grouped
heap/index path even when the target table already has a `PRIMARY KEY` or
secondary indexes.

```sql
INSERT INTO users VALUES
  (1, 'a@example.com'),
  (2, 'b@example.com'),
  (3, 'c@example.com');
```

This matters because indexed tables used to fall back to per-row maintenance on
this workload. The grouped path keeps the same SQL-visible behavior:

- duplicate `PRIMARY KEY` / `UNIQUE` values inside the same statement still fail
- a failed multi-row statement does not leak partial committed rows
- partial indexes still include only rows whose predicate matches

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
On the local PK-only benchmark, AxiomDB multi-row INSERT reaches <strong>321,002 rows/s</strong> vs MariaDB 12.1 at <strong>160,581 rows/s</strong>. The gain comes from grouped heap/index apply instead of per-row indexed maintenance for the statement.
</div>
</div>

## Startup Integrity Verification

When a database opens, AxiomDB verifies every catalog-visible index against the
heap-visible rows reconstructed after WAL recovery.

- If the tree is readable but its contents diverge from the heap, AxiomDB
  rebuilds the index automatically from table contents before serving traffic.
- If the tree cannot be traversed safely, open fails with
  `IndexIntegrityFailure` instead of guessing.

This check applies to both embedded mode and server mode because both call the
same startup verifier.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Auto-Rebuild Only When Safe</span>
AxiomDB combines PostgreSQL <code>amcheck</code>'s “never trust an unreadable B-Tree” rule
with SQLite's <code>REINDEX</code>-style rebuild-from-table approach. Readable divergence is
healed automatically from heap data; unreadable trees still block open.
</div>
</div>

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

This includes the PRIMARY KEY. A query like `WHERE id = 42` does not need a
redundant secondary index on `id`.

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
automatically when a matching index exists, including the PRIMARY KEY, with zero
configuration required.
</div>
</div>

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">No Redundant PK Index</span>
If `id` is already your PRIMARY KEY, do not also create `CREATE INDEX ... ON t(id)` just for point lookups. The planner already uses the primary-key B+Tree for `WHERE id = ...`.
</div>
</div>

---

## Partial Indexes

A partial index covers only the rows matching a WHERE predicate. This reduces
index size, speeds up maintenance, and — for UNIQUE indexes — restricts
uniqueness enforcement to the matching subset.

```sql
-- Only active users need unique emails.
CREATE UNIQUE INDEX uq_active_email ON users(email) WHERE deleted_at IS NULL;

-- Index only pending orders for fast user lookups.
CREATE INDEX idx_pending ON orders(user_id) WHERE status = 'pending';
```

### Partial UNIQUE indexes

The uniqueness constraint applies only among rows satisfying the predicate.
Rows that do not satisfy the predicate are never inserted into the index.

```sql
-- alice deleted, then re-created: no conflict.
INSERT INTO users VALUES (1, 'alice@x.com', '2025-01-01'); -- deleted
INSERT INTO users VALUES (2, 'alice@x.com', NULL);          -- active ✅
INSERT INTO users VALUES (3, 'alice@x.com', NULL);          -- ❌ UniqueViolation
INSERT INTO users VALUES (4, 'alice@x.com', '2025-06-01');  -- deleted ✅
```

### Planner support

The planner uses a partial index only when the query's WHERE clause implies the
index predicate. If the implication cannot be verified, the planner falls back
to a full scan or a full index — always correct.

```sql
-- Uses partial index (WHERE contains `deleted_at IS NULL`):
SELECT * FROM users WHERE email = 'alice@x.com' AND deleted_at IS NULL;

-- Falls back to full scan (predicate not in WHERE):
SELECT * FROM users WHERE email = 'alice@x.com';
```

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
A partial unique index on a soft-delete table (e.g., <code>WHERE deleted_at IS
NULL</code>) is typically 10–100× smaller than a full unique index, since most
rows in high-churn tables are in the deleted state. This reduces build time,
per-INSERT maintenance cost, and bloom filter memory. MySQL InnoDB does not
support partial indexes, so this optimization is not available there.
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

### Enforcement Examples

```sql
CREATE TABLE users  (id INT PRIMARY KEY, email TEXT);
CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id) ON DELETE CASCADE);

INSERT INTO users  VALUES (1, 'alice@x.com');
INSERT INTO orders VALUES (10, 1);            -- ✅ user 1 exists

-- INSERT with missing parent → error
INSERT INTO orders VALUES (20, 999);
-- ERROR 23503: Foreign key constraint fails: 'orders.user_id' = '999'

-- DELETE parent with CASCADE → child rows automatically deleted
DELETE FROM users WHERE id = 1;
SELECT COUNT(*) FROM orders;  -- → 0 (orders were cascaded)

-- DELETE parent with RESTRICT (default) → blocked if children exist
CREATE TABLE invoices (id INT PRIMARY KEY, order_id INT REFERENCES orders(id));
INSERT INTO users   VALUES (2, 'bob@x.com');
INSERT INTO orders  VALUES (30, 2);
INSERT INTO invoices VALUES (1, 30);
DELETE FROM orders WHERE id = 30;
-- ERROR 23503: foreign key constraint "fk_invoices_order_id": invoices.order_id references this row
```

### NULL FK Values

A NULL value in a FK column is always allowed — it does not reference any parent row.
This follows SQL standard MATCH SIMPLE semantics.

```sql
INSERT INTO orders VALUES (99, NULL);  -- ✅ NULL user_id is always allowed
```

### ON UPDATE

Only `ON UPDATE RESTRICT` (the default) is enforced. Updating a parent key while
child rows reference it is rejected. `ON UPDATE CASCADE` and `ON UPDATE SET NULL`
are planned for Phase 6.10.

### Current Limitations

- Only single-column FKs are supported. Composite FKs — `FOREIGN KEY (a, b) REFERENCES t(x, y)` — are planned for Phase 6.10.
- `ON UPDATE CASCADE` / `ON UPDATE SET NULL` are planned for Phase 6.10.
- FK validation uses B-Tree range scans via the FK auto-index (Phase 6.9). Falls back to full table scan for pre-6.9 FKs.

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
`ANALYZE TABLE` (available since Phase 6.12).

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

## Index-Only Scans (Covering Indexes)

When every column referenced by a `SELECT` is already stored as a key column of
the chosen index, AxiomDB can satisfy the query entirely from the B-Tree — no
heap page read is needed. This is called an **index-only scan**.

### Example

```sql
CREATE INDEX idx_age ON users (age);

-- Index-only scan: only column needed (age) is the index key.
SELECT age FROM users WHERE age = 25;
```

The executor reads the matching B-Tree leaf entries, extracts the `age` value
from the encoded key bytes, and returns the rows without ever touching the heap.

### INCLUDE syntax — declaring covering intent

You can declare additional columns as part of a covering index using the
`INCLUDE` clause:

```sql
CREATE INDEX idx_name_dept ON employees (name) INCLUDE (department, salary);
```

`INCLUDE` columns are recorded in the catalog metadata so the planner knows
the index covers those columns. **Note:** physical storage of INCLUDE column
values in B-Tree leaf nodes is deferred to a future covering-index phase. Until then, the planner
uses `INCLUDE` to correctly identify `IndexOnlyScan` opportunities, but the
values are read from the key portion of the B-Tree entry.

### MVCC and the 24-byte header read

Index-only scans still perform a lightweight visibility check per row. For each
B-Tree entry, the executor reads only the 24-byte `RowHeader` (the slot header
containing `txn_id_created`, `txn_id_deleted`, and sequence number) to determine
whether the row is visible to the current transaction snapshot. The full row
payload is never decoded.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
PostgreSQL requires an all-visible map (a per-page bitmap written by VACUUM) to
perform true index-only scans — without it, PostgreSQL falls back to a full
heap fetch. AxiomDB performs a 24-byte RowHeader read for MVCC instead, which
is simpler, requires no VACUUM pass, and still eliminates the expensive full row
decode and heap page traversal.
</div>
</div>

---

## Non-Unique Secondary Index Key Format

Non-unique secondary indexes store the indexed column values **together with the
row's `RecordId`** as the B-Tree key:

```
key = encode_index_key(col_vals) || encode_rid(rid)   // 10-byte RecordId suffix
```

This ensures every B-Tree entry is globally unique even when multiple rows share
the same indexed value — making `INSERT` safe without a `DuplicateKey` error.

When looking up all rows with a given indexed value, the executor performs a
range scan with synthetic bounds:

```
lo = encode_index_key(val) || [0x00; 10]   // smallest possible RecordId
hi = encode_index_key(val) || [0xFF; 10]   // largest possible RecordId
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — InnoDB Composite Key Approach</span>
This is the same strategy used by MySQL InnoDB secondary indexes, where the
primary key is appended as a tiebreaker in the B-Tree entry. AxiomDB uses
<code>RecordId</code> (page_id + slot_id + sequence number) instead of a
separate primary key column, keeping the suffix at a fixed 10 bytes regardless
of the table's key type.
</div>
</div>

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
