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
image. This is still an internal storage milestone, not a SQL-visible
clustered-table feature yet.

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
- row-level locking, deadlock detection, and `SELECT ... FOR UPDATE` are planned
  for Phases 13.7, 13.8, and 13.8b

This is good for read-heavy workloads, but it is still below MySQL/InnoDB and
PostgreSQL for write concurrency because they already lock at row granularity.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Current Concurrency Cut</span>
MySQL InnoDB and PostgreSQL allow multiple writers to proceed concurrently when
they touch different rows. AxiomDB's current runtime intentionally keeps a
database-wide write guard while Phase 13.7 introduces row-level locking, then
adds deadlock detection and `FOR UPDATE` syntax in follow-up subphases.
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

- row-level deadlock detection
- `40001 serialization_failure` retries for ordinary write-write conflicts
- `SELECT ... FOR UPDATE` / `SKIP LOCKED` job-queue patterns

Those behaviors are planned, but not implemented yet.

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

## E-commerce Checkout — Current Safe Pattern

Until row-level locking lands, the supported stock-reservation pattern is a
guarded `UPDATE ... WHERE stock >= ?` plus affected-row checks.

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

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">`FOR UPDATE` Not Yet Available</span>
`SELECT ... FOR UPDATE`, `FOR SHARE`, `NOWAIT`, and `SKIP LOCKED` are planned
for the row-locking phases and are not implemented in the current runtime. Use
guarded `UPDATE ... WHERE ...` statements plus affected-row checks today.
</div>
</div>

---

## Transaction Performance Tips

- Keep transactions short. Long-running transactions hold MVCC versions in memory
  longer, increasing memory pressure.
- Avoid user interaction within a transaction. Never open a transaction and wait
  for a user to click a button.
- Use `INSERT ... SELECT` to batch multiple rows rather than thousands of individual
  INSERTs in a loop within one transaction.
- For bulk loads, consider committing every 10,000 rows to limit WAL growth.

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
