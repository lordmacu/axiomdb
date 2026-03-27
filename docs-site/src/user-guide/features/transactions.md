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

A savepoint marks a point within a transaction to which you can roll back without
aborting the entire transaction.

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

AxiomDB uses MVCC to allow readers and writers to proceed concurrently without
blocking each other. The key insight is that **readers never block writers, and
writers never block readers**.

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

### No Read Locks

Because readers access immutable snapshots, they require no locks. A long-running
`SELECT` never blocks `INSERT`, `UPDATE`, or `DELETE` operations.

### Write Conflicts

Two concurrent writers can conflict if they both attempt to modify the same row.
AxiomDB uses first-writer-wins: the second writer's transaction is aborted with
error `40001 serialization_failure`. The application should retry the transaction.

```python
import time, random

def transfer_with_retry(db, from_id, to_id, amount, max_retries=5):
    for attempt in range(max_retries):
        try:
            db.execute("BEGIN")
            db.execute(f"UPDATE accounts SET balance = balance - {amount} WHERE id = {from_id}")
            db.execute(f"UPDATE accounts SET balance = balance + {amount} WHERE id = {to_id}")
            db.execute("COMMIT")
            return  # success
        except Exception as e:
            db.execute("ROLLBACK")
            if '40001' in str(e) and attempt < max_retries - 1:
                time.sleep(0.01 * (2 ** attempt) + random.random() * 0.01)
            else:
                raise
```

---

## Isolation Levels

AxiomDB supports two isolation levels. The level is set per-transaction.

### READ COMMITTED (default)

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

### REPEATABLE READ

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
| Phantom reads       | Possible       | Never (MVCC)    |
| Write conflicts     | Detected       | Detected        |

---

## E-commerce Checkout — Full Example

This example shows a complete checkout flow with stock validation, order creation,
and stock decrement, all atomically.

```sql
BEGIN;

-- 1. Lock the product rows for update (prevent concurrent stock changes)
SELECT id, stock, price FROM products
WHERE id IN (1, 3)
FOR UPDATE;

-- 2. Validate stock
-- Application checks result: if product 1 stock < 2, ROLLBACK
-- Application checks result: if product 3 stock < 1, ROLLBACK

-- 3. Create the order header
INSERT INTO orders (user_id, total, status)
VALUES (99, 149.97, 'paid');

-- 4. Create order items
INSERT INTO order_items (order_id, product_id, quantity, unit_price) VALUES
    (LAST_INSERT_ID(), 1, 2, 49.99),
    (LAST_INSERT_ID(), 3, 1, 49.99);

-- 5. Decrement stock
UPDATE products SET stock = stock - 2 WHERE id = 1;
UPDATE products SET stock = stock - 1 WHERE id = 3;

COMMIT;
```

If any step fails (constraint violation, connection drop, server crash), the WAL
ensures the entire transaction is rolled back on recovery.

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
