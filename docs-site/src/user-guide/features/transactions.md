# Transactions

A transaction is a sequence of SQL operations that execute as a single atomic unit:
either all succeed (COMMIT) or none of them take effect (ROLLBACK). AxiomDB implements
full ACID transactions backed by a Write-Ahead Log and Multi-Version Concurrency Control.

---

## Basic Transaction Control

```sql
BEGIN;
-- ... SQL statements ...
COMMIT;   -- make all changes permanent
```

```sql
BEGIN;
-- ... SQL statements ...
ROLLBACK; -- undo all changes since BEGIN
```

### Simple Example — Money Transfer

```sql
BEGIN;

-- Debit the sender
UPDATE accounts SET balance = balance - 250.00 WHERE id = 1;

-- Credit the receiver
UPDATE accounts SET balance = balance + 250.00 WHERE id = 2;

-- Both succeed together, or neither succeeds
COMMIT;
```

If the connection drops after the first UPDATE but before COMMIT, the WAL records
both the transaction start and the mutation. During crash recovery, AxiomDB sees no
COMMIT record for this transaction and discards the partial change. Account 1 keeps
its original balance.

Phases `39.11` and `39.12` extend that internal durability model to the
clustered-index storage rewrite: clustered rows now have WAL-backed
rollback/savepoint support and crash recovery by primary key plus exact row
image. Phase `39.13` makes the first SQL-visible clustered cut: `CREATE TABLE`
with an explicit `PRIMARY KEY` now creates clustered metadata and a clustered
table root. Phase `39.14` extends that cut to clustered `INSERT`: SQL writes
now record clustered WAL/undo directly against the clustered PK tree, and
Phase `39.15` opens clustered `SELECT` over that same storage, Phase
`39.16` extends the same transaction contract to clustered `UPDATE`, and
`39.17` now extends it to clustered `DELETE` as delete-mark plus exact
row-image undo. `39.18` adds clustered `VACUUM`: once a clustered delete-mark
is old enough to be physically safe, `VACUUM table_name` purges the dead row,
frees any overflow chain it owned, and cleans dead bookmark entries from
clustered secondary indexes. `39.22` adds zero-allocation in-place UPDATE: when
all SET columns are fixed-size (INT, BIGINT, REAL, BOOL, DATE, TIMESTAMP),
field bytes are patched directly in the page buffer without decoding the row,
and ROLLBACK reverses only the changed bytes via `UndoClusteredFieldPatch` —
no full row image stored in the undo log.

```sql
CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE);

BEGIN;
INSERT INTO users VALUES (1, 'alice@example.com');
ROLLBACK;
```

That rollback now restores clustered INSERT, clustered UPDATE, and clustered
DELETE state: the clustered base row goes back to its exact previous row image,
and any bookmark-bearing secondary entries are deleted or reinserted to match
when the statement rewrote them.

---

## Autocommit

When no explicit `BEGIN` is issued, each statement executes in its own implicit
transaction and is committed automatically on success. This is the default mode.

```sql
-- Each of these is its own transaction
INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
INSERT INTO users (name, email) VALUES ('Bob',   'bob@example.com');
```

To group multiple statements atomically, always use explicit `BEGIN ... COMMIT`.

---

## Durability — `SET synchronous`

Per-session control over how aggressively AxiomDB pushes WAL data to disk on
`COMMIT`. Mirrors SQLite's `PRAGMA synchronous` so workloads can opt into the
same throughput / durability tradeoffs.

```sql
SET synchronous = 'STRICT';   -- default: fsync per commit. Durable on ACK.
SET synchronous = 'NORMAL';   -- flush to OS only. Recent commits may be lost
                              -- on OS crash, DB stays consistent.
SET synchronous = 'OFF';      -- no flush, no fsync. Test/ephemeral only.
SET synchronous = DEFAULT;    -- reset to STRICT.
```

For SQLite migration compatibility the parser also accepts `FULL` / `EXTRA`
(both map to `STRICT`), `ON` (maps to `NORMAL`), and numeric forms
`0..4` (same numbering as SQLite's `getSafetyLevel`).

::: callout-design
**Why per-session, not a startup flag**
AxiomDB defaults to `STRICT` — no durability regression for users who don't
opt in. A connection pool can keep `STRICT` for transactional traffic and
issue `SET synchronous = 'NORMAL'` only on the connection running a bulk
load, instead of restarting the server with a global flag.
:::

`SET synchronous` is rejected inside an open transaction (`InvalidValue`).
The override is captured by `BEGIN` and frozen on the connection's
transaction record, so a mid-txn change would silently no-op until the next
transaction — matching SQLite's `PRAGMA synchronous` semantics
(`research/sqlite/src/pragma.c:1136`).

---

## SAVEPOINT — Partial Rollback

Savepoints mark a point within a transaction to which you can roll back without
aborting the entire transaction. ORMs (Django, Rails, Sequelize) use savepoints
internally for partial error recovery.

```sql
BEGIN;

INSERT INTO orders (user_id, total) VALUES (1, 99.99);
SAVEPOINT after_order;

INSERT INTO order_items (order_id, product_id, quantity) VALUES (1, 42, 1);
-- Suppose this fails a CHECK constraint

ROLLBACK TO SAVEPOINT after_order;
-- The order row still exists; only the order_item is rolled back

-- Try again with corrected data
INSERT INTO order_items (order_id, product_id, quantity) VALUES (1, 42, 0);
-- Still fails — give up entirely
ROLLBACK;
```

You can have multiple savepoints with different names:

```sql
BEGIN;
SAVEPOINT sp1;
-- ... work ...
SAVEPOINT sp2;
-- ... more work ...
ROLLBACK TO SAVEPOINT sp1;   -- undo everything since sp1
RELEASE SAVEPOINT sp1;       -- destroy the savepoint (optional cleanup)
COMMIT;
```

---

## MVCC — Multi-Version Concurrency Control

AxiomDB uses MVCC plus a server-side `Arc<RwLock<Database>>`.

Today that means:

- read-only statements (`SELECT`, `SHOW`, metadata queries) run concurrently
- mutating statements (`INSERT`, `UPDATE`, `DELETE`, DDL, `BEGIN`/`COMMIT`/`ROLLBACK`)
  are serialized at whole-database granularity
- a read that is already running keeps its snapshot while another session commits
- `SELECT ... FOR UPDATE / FOR SHARE [NOWAIT | SKIP LOCKED]` row-level
  pessimistic locking is available as of Phase 13.7–13.8b
  (see [Row-Level Locking](#row-level-locking) below)
- deadlock detection is built into the lock manager

This is good for read-heavy workloads, but it is still below MySQL/InnoDB and
PostgreSQL for write concurrency because they already lock at row granularity.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Current Concurrency Model</span>
MySQL InnoDB and PostgreSQL allow multiple writers to proceed concurrently when
they touch different rows. AxiomDB's current runtime keeps a database-wide write
guard for mutations, but row-level pessimistic locks (`FOR UPDATE / FOR SHARE`)
are now fully tracked in the `axiomdb-lock` crate and released on COMMIT/ROLLBACK.
</div>
</div>

### How It Works

When a transaction starts, it receives a **snapshot** — a consistent view of the
database as it existed at that moment. Other transactions may commit new changes
while your transaction runs, but your snapshot does not change.

```
Time →

Txn A (snapshot at T=100):    BEGIN → reads → reads → COMMIT
                               |          |          |
Txn B:                         |  INSERT  |  COMMIT  |
                               |          |          |
Txn A sees the world as it was at T=100.
Txn B's inserts are not visible to Txn A.
```

This is implemented via the Copy-on-Write B+ Tree: when Txn B writes a page, it
creates a new copy rather than overwriting the original. Txn A holds a pointer to
the old root and continues reading the old version. When Txn A commits, the old
pages become eligible for reclamation.

### No Per-Page Read Latches

Readers access immutable snapshots and owned page copies, so they do not take
per-page latches in the storage layer. The current server runtime still uses a
database-wide `RwLock`, so the real guarantee today is:

- many reads can run together
- writes do not run in parallel with other writes

### Current Write Behavior

Two sessions do **not** currently mutate different rows in parallel. Instead,
the server queues mutating statements behind the database-wide write guard.
`lock_timeout` applies to that wait today.

This means you should not yet build on assumptions such as:

- `40001 serialization_failure` retries for ordinary write-write conflicts
Row-level pessimistic locking (`FOR UPDATE / FOR SHARE`) with `NOWAIT` and
`SKIP LOCKED` is implemented as of Phases 13.7–13.8b, including deadlock detection.

---

## Row-Level Locking

`SELECT ... FOR UPDATE / FOR SHARE` acquires **pessimistic row-level locks**
on the rows returned by the query. This prevents other transactions from
modifying (or, for shared locks, exclusively locking) those rows until your
transaction commits or rolls back.

### Syntax

```sql
SELECT ... FROM table
[WHERE ...]
[ORDER BY ...] [LIMIT n]
FOR UPDATE          [NOWAIT | SKIP LOCKED]
| FOR NO KEY UPDATE [NOWAIT | SKIP LOCKED]
| FOR SHARE         [NOWAIT | SKIP LOCKED]
| FOR KEY SHARE     [NOWAIT | SKIP LOCKED]
| LOCK IN SHARE MODE   -- MySQL alias for FOR SHARE
```

### Lock Modes

| Mode | Strength | Blocks |
|---|---|---|
| `FOR UPDATE` | Exclusive row + IX table intent | All other locking reads and writes |
| `FOR NO KEY UPDATE` | Exclusive row + IX table intent | Shared reads; allows FK existence checks |
| `FOR SHARE` / `LOCK IN SHARE MODE` | Shared row + IS table intent | Exclusive locks only |
| `FOR KEY SHARE` | Shared row + IS table intent | `FOR UPDATE` only; allows FK references |

### NOWAIT

Appending `NOWAIT` causes the statement to fail immediately with
`DbError::LockTimeout` if any of the target rows is already locked by
another transaction. Without `NOWAIT`, the statement waits until the
conflicting lock is released (or until `lock_timeout` fires).

### SKIP LOCKED

Appending `SKIP LOCKED` silently omits rows that cannot be locked — no error,
no waiting. Rows are evaluated *before* `LIMIT` is applied, so
`LIMIT 1 FOR UPDATE SKIP LOCKED` always returns one *available* row even when
other rows are locked ahead of it.

```sql
-- Multiple workers can run concurrently without blocking each other.
BEGIN;

SELECT id, payload
FROM jobs
WHERE status = 'pending'
ORDER BY created_at
LIMIT 1
FOR UPDATE SKIP LOCKED;  -- skip rows already claimed by other workers

UPDATE jobs SET status = 'running' WHERE id = ?;

COMMIT;
```

### When Locks Are Acquired

- **`NOWAIT` / `Block`**: locks are acquired after `WHERE`, `ORDER BY`, and
  `LIMIT` — only rows in the final result set are locked.
- **`SKIP LOCKED`**: locking happens after `WHERE` and `ORDER BY` but *before*
  `LIMIT` — `LIMIT` is applied to the already-filtered (lockable) set.

### Deadlock Detection

The lock manager tracks the wait-for graph across all active transactions. If
a cycle is detected (transaction A waits for B, B waits for A), the later
transaction is aborted with `DbError::DeadlockDetected` and its locks are
released so the surviving transaction can proceed.

### Lock Release

All row-level locks acquired by a transaction are released atomically on
`COMMIT` or `ROLLBACK`. There is no way to release individual locks early
(use `SAVEPOINT` / `ROLLBACK TO` for partial rollback, but note that this
does not release locks acquired after the savepoint).

### Example — Job Queue

```sql
BEGIN;

-- Claim one pending job; if none is available or if another worker
-- has already claimed it, fail immediately.
SELECT id, payload
FROM jobs
WHERE status = 'pending'
ORDER BY created_at
LIMIT 1
FOR UPDATE NOWAIT;

UPDATE jobs SET status = 'running' WHERE id = ?;

COMMIT;
```

### Limitations

- `FOR UPDATE` is not supported on foreign tables (HTTP FDW).
- `FOR UPDATE` on clustered tables is not yet supported; use heap tables.

---

## Isolation Levels

AxiomDB currently accepts three wire-visible isolation names:

- `READ COMMITTED`
- `REPEATABLE READ` (session default)
- `SERIALIZABLE`

`READ COMMITTED` and `REPEATABLE READ` have distinct snapshot behavior today.
`SERIALIZABLE` is accepted and stored, but currently uses the same frozen-snapshot
policy as `REPEATABLE READ`; true SSI is still planned.

### READ COMMITTED

Each statement within the transaction sees data committed before that statement
began. A second SELECT within the same transaction may see different data if
another transaction committed between the two SELECTs.

```sql
SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
BEGIN;

SELECT balance FROM accounts WHERE id = 1;  -- sees T=100: balance = 1000
-- Txn B commits: UPDATE accounts SET balance = 900 WHERE id = 1
SELECT balance FROM accounts WHERE id = 1;  -- sees T=110: balance = 900 (changed!)

COMMIT;
```

**Use READ COMMITTED when:**
- You need maximum concurrency
- Each statement needing the freshest possible data is acceptable
- You are running analytics that can tolerate non-repeatable reads

### REPEATABLE READ (default)

The entire transaction sees the snapshot from the moment `BEGIN` was executed.
No matter how many other transactions commit, your reads return the same data.

```sql
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ;
BEGIN;

SELECT balance FROM accounts WHERE id = 1;  -- snapshot at T=100: balance = 1000
-- Txn B commits: UPDATE accounts SET balance = 900 WHERE id = 1
SELECT balance FROM accounts WHERE id = 1;  -- still sees T=100: balance = 1000

COMMIT;
```

**Use REPEATABLE READ when:**
- You need consistent data across multiple reads in one transaction
- Running reports or multi-step calculations where consistency matters
- Implementing optimistic locking patterns

### Isolation Level Comparison

| Phenomenon          | READ COMMITTED | REPEATABLE READ |
|---------------------|----------------|-----------------|
| Dirty reads         | Never          | Never           |
| Non-repeatable reads| Possible       | Never           |
| Phantom reads       | Possible       | Prevented by current single-writer runtime |
| Concurrent writes   | Serialized globally | Serialized globally |

### SERIALIZABLE

`SERIALIZABLE` is accepted for MySQL/PostgreSQL compatibility, but today it
uses the same frozen snapshot as `REPEATABLE READ`. The engine does **not** yet
run Serializable Snapshot Isolation conflict tracking.

---

## E-commerce Checkout — Stock Reservation Pattern

A safe stock-reservation pattern uses a guarded `UPDATE ... WHERE stock >= ?`
plus affected-row checks. For stricter serialization, combine with
`SELECT ... FOR UPDATE` to hold the row before updating.

```sql
BEGIN;

-- Reserve stock atomically; application checks that each UPDATE affects 1 row.
UPDATE products SET stock = stock - 2 WHERE id = 1 AND stock >= 2;
UPDATE products SET stock = stock - 1 WHERE id = 3 AND stock >= 1;

-- Create the order header
INSERT INTO orders (user_id, total, status)
VALUES (99, 149.97, 'paid');

-- Create order items
INSERT INTO order_items (order_id, product_id, quantity, unit_price) VALUES
    (LAST_INSERT_ID(), 1, 2, 49.99),
    (LAST_INSERT_ID(), 3, 1, 49.99);

COMMIT;
```

If any step fails (constraint violation, connection drop, server crash), the WAL
ensures the entire transaction is rolled back on recovery.

The same safety rule now applies to connection cleanup paths. If a client has an
open implicit or explicit transaction and then sends `COM_RESET_CONNECTION`,
`COM_CHANGE_USER`, or disconnects without committing, AxiomDB rolls that
transaction back before releasing the session.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Combining UPDATE-guard with FOR UPDATE</span>
For the highest consistency, open the reservation with
`SELECT id, stock FROM products WHERE id = ? FOR UPDATE` to prevent concurrent
readers from seeing a stale value, then apply the `UPDATE ... WHERE stock >= ?`
check. Both approaches are safe; `FOR UPDATE` adds strictness at the cost of
holding a lock for the transaction duration.
</div>
</div>

---

## Transaction Performance Tips

- Keep transactions short. Long-running transactions hold MVCC versions in memory
  longer, increasing memory pressure.
- Avoid user interaction within a transaction. Never open a transaction and wait
  for a user to click a button.
- **For bulk inserts into clustered tables, wrap all rows in a single
  `BEGIN ... COMMIT` block.** Phase 40.1 introduces `ClusteredInsertBatch`: rows
  are staged in memory, sorted by primary key, and flushed at COMMIT using the
  rightmost-leaf batch append path. This reduces O(N) CoW page-clone operations to
  O(N / leaf_capacity) page writes — delivering **55.9K rows/s** for 50K sequential
  PK rows vs MySQL 8.0 InnoDB's ~35K rows/s (+59%).
- For bulk loads, consider committing every 50,000–100,000 rows to limit WAL growth
  while keeping the batch-insert speedup.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Batch INSERT Advantage</span>
AxiomDB 40.1 achieves 55.9K rows/s for 50K sequential PK clustered inserts in
one explicit transaction — +59% faster than MySQL 8.0 InnoDB (~35K rows/s) on the
same sequential PK workload. MySQL pays one buffer-pool page write per row in the
common path; AxiomDB accumulates a full leaf worth of rows and writes the page once.
</div>
</div>

### WAL Fsync Pipeline — Current Server Commit Path

Every durable DML commit still needs WAL fsync, but AxiomDB no longer relies on
the old timer-based group-commit window for batching. The server now uses an
always-on **leader-based fsync pipeline**:

- one connection becomes the fsync leader
- later commits queue behind that leader if their WAL entry is already buffered
- if the leader's fsync covers a later commit's LSN, that later commit returns
  without paying another fsync

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip</span>
You no longer need to tune a batch window for this path. The pipeline mainly
helps when commits overlap in time. For a strictly sequential request/response
client, autocommit throughput is still limited by one durable fsync per visible
statement response.
</div>
</div>
