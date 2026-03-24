# Getting Started

AxiomDB is a relational database engine written in Rust. It supports standard SQL, ACID
transactions, a Write-Ahead Log for crash recovery, and a Copy-on-Write B+ Tree for
lock-free concurrent reads. This guide walks you through connecting to AxiomDB, choosing
a usage mode, and running your first queries.

---

## Choosing a Usage Mode

AxiomDB operates in two distinct modes that share the exact same engine code.

### Server Mode

The engine runs as a standalone daemon that speaks the **MySQL wire protocol** on TCP
port 3306 (configurable). Any MySQL-compatible client connects without installing custom
drivers.

```
Application (PHP / Python / Node.js)
        │
        │ TCP :3306  (MySQL wire protocol)
        ▼
  axiomdb-server process
        │
        ▼
  axiomdb.db   axiomdb.wal
```

**When to use server mode:**
- Web applications with REST or GraphQL APIs
- Microservices where multiple processes share a database
- Any environment where you would normally use MySQL

### Embedded Mode

The engine is compiled into your process as a shared library (`.so` / `.dylib` / `.dll`).
There is no daemon, no network, and no port. Calls go directly to Rust code with
microsecond latency.

```
Your Application (Rust / C++ / Python / Electron)
        │
        │ direct function call (C FFI / Rust crate)
        ▼
  AxiomDB engine (in-process)
        │
        ▼
  axiomdb.db   axiomdb.wal   (local files)
```

**When to use embedded mode:**
- Desktop applications (Qt, Electron, Tauri)
- CLI tools that need a local database
- Python scripts that need fast local storage without a daemon
- Any context where SQLite would be considered

### Mode Comparison

| Feature                 | Server Mode              | Embedded Mode          |
|-------------------------|--------------------------|------------------------|
| Latency                 | ~0.1 ms (TCP loopback)   | ~1 µs (in-process)     |
| Multiple processes      | Yes                      | No (one process)       |
| Installation            | Binary + port            | Library only           |
| Compatible clients      | Any MySQL client         | Rust crate / C FFI     |
| Ideal for               | Web, APIs, microservices | Desktop, CLI, scripts  |

---

## Server Mode — Connecting

### Starting the Server

```bash
# Default: stores data in ./data, listens on port 3306
axiomdb-server

# Custom data directory and port
axiomdb-server --data-dir /var/lib/axiomdb --port 3306

# Override port via environment variable
AXIOMDB_PORT=3307 axiomdb-server
```

The server is ready when you see:

```
INFO axiomdb_server: listening on 0.0.0.0:3306
```

### Connecting with the mysql CLI

```bash
mysql -h 127.0.0.1 -P 3306 -u root
```

No password is required in Phase 5. Any username from the allowlist (`root`, `axiomdb`,
`admin`) is accepted. See the [Authentication](#authentication) section below for details.

### Connecting with Python (PyMySQL)

```python
import pymysql

conn = pymysql.connect(
    host='127.0.0.1',
    port=3306,
    user='root',
    db='axiomdb',
    charset='utf8mb4',
)

with conn.cursor() as cursor:
    # CREATE TABLE with AUTO_INCREMENT
    cursor.execute("""
        CREATE TABLE users (
            id    BIGINT PRIMARY KEY AUTO_INCREMENT,
            name  TEXT   NOT NULL,
            email TEXT   NOT NULL
        )
    """)

    # INSERT — last_insert_id is returned in the OK packet
    cursor.execute("INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')")
    print("inserted id:", cursor.lastrowid)

    # SELECT
    cursor.execute("SELECT id, name FROM users")
    for row in cursor.fetchall():
        print(row)

conn.close()
```

### Parameterized Queries and ORMs (Prepared Statements)

When you pass parameters to `cursor.execute()`, PyMySQL (and any MySQL-compatible
driver) automatically uses `COM_STMT_PREPARE` / `COM_STMT_EXECUTE` — the MySQL
binary prepared statement protocol. AxiomDB supports this natively from Phase 5.10.

```python
import pymysql

conn = pymysql.connect(host='127.0.0.1', port=3306, user='root', db='axiomdb')

with conn.cursor() as cursor:
    cursor.execute("""
        CREATE TABLE products (
            id    BIGINT PRIMARY KEY AUTO_INCREMENT,
            name  TEXT   NOT NULL,
            price DOUBLE NOT NULL,
            active BOOL  NOT NULL DEFAULT TRUE
        )
    """)
    conn.commit()

    # Parameterized INSERT — uses COM_STMT_PREPARE/EXECUTE automatically
    cursor.execute(
        "INSERT INTO products (name, price, active) VALUES (%s, %s, %s)",
        ('Wireless Keyboard', 49.99, True),
    )

    # NULL parameters work transparently
    cursor.execute(
        "INSERT INTO products (name, price, active) VALUES (%s, %s, %s)",
        ('USB-C Hub', 29.99, None),
    )

    # Parameterized SELECT
    cursor.execute("SELECT id, name, price FROM products WHERE price < %s", (50.0,))
    for row in cursor.fetchall():
        print(row)

    # Boolean column comparison works with integer literals (MySQL-compatible)
    cursor.execute("SELECT name FROM products WHERE active = %s", (1,))
    for row in cursor.fetchall():
        print(row)

conn.close()
```

ORMs such as SQLAlchemy use parameterized queries for all data-bearing operations.
Connecting through the MySQL dialect works without any additional configuration:

```python
from sqlalchemy import create_engine, text

engine = create_engine("mysql+pymysql://root@127.0.0.1:3306/axiomdb")

with engine.connect() as conn:
    result = conn.execute(
        text("SELECT id, name FROM products WHERE price < :max_price"),
        {"max_price": 40.0},
    )
    for row in result:
        print(row)
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Prepared Statement Lifecycle</span>
Each call to <code>cursor.execute(sql, params)</code> sends a <code>COM_STMT_PREPARE</code>
to parse the SQL and register a statement ID, followed by <code>COM_STMT_EXECUTE</code>
with the binary-encoded parameters. The statement is cached per connection in AxiomDB
and released with <code>COM_STMT_CLOSE</code> when the cursor closes. This matches the
behavior expected by PyMySQL, mysqlclient, and SQLAlchemy's MySQL dialect.
</div>
</div>

### Connecting with PHP (PDO)

```php
<?php
$pdo = new PDO(
    'mysql:host=127.0.0.1;port=3306;dbname=axiomdb',
    'root',
    '',
    [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
);

$stmt = $pdo->query('SELECT id, name FROM users LIMIT 5');
foreach ($stmt as $row) {
    echo $row['id'] . ': ' . $row['name'] . "\n";
}
```

### Connecting with any MySQL GUI

Point MySQL Workbench, DBeaver, or TablePlus to `127.0.0.1:3306`. No driver
installation is required — the MySQL wire protocol is fully compatible.

### Authentication

AxiomDB Phase 5 uses **permissive authentication**: the server accepts any password
for usernames in the allowlist (`root`, `axiomdb`, `admin`, and the empty string).
Both of the most common MySQL authentication plugins are supported with no client-side
configuration required:

| Plugin | Clients | Notes |
|--------|---------|-------|
| `mysql_native_password` | MySQL 5.x clients, older PyMySQL, mysql2 < 0.5 | 3-packet handshake (greeting → response → OK) |
| `caching_sha2_password` | MySQL 8.0+ default, PyMySQL >= 1.0, MySQL Connector/Python | 5-packet handshake (greeting → response → fast_auth_success → ack → OK) |

If your client connects with MySQL 8.0+ defaults and you see silent connection drops,
your client is using `caching_sha2_password` — AxiomDB handles this automatically.
No `--default-auth` flag or `authPlugin` option is needed.

Full password enforcement with stored credentials is planned for Phase 13 (Security).

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Connecting from ORMs</span>
SQLAlchemy, ActiveRecord, and similar ORMs send several setup queries on connect
(<code>SET NAMES</code>, <code>SELECT @@version</code>, <code>SHOW DATABASES</code>, etc.).
AxiomDB intercepts and stubs these automatically — no configuration needed.
</div>
</div>

---

## Embedded Mode — Rust API

Add AxiomDB to your `Cargo.toml`:

```toml
[dependencies]
axiomdb-embedded = { path = "../axiomdb/crates/axiomdb-embedded" }
```

### Open a Database

```rust
use axiomdb_embedded::{Database, QueryResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open (or create) a database file on disk.
    let db = Database::open("./axiomdb.db")?;

    // Or open an in-memory database for tests / temporary use.
    // let db = Database::open_in_memory()?;

    // Execute DDL
    db.execute("
        CREATE TABLE users (
            id    BIGINT PRIMARY KEY AUTO_INCREMENT,
            name  TEXT    NOT NULL,
            email TEXT    NOT NULL UNIQUE,
            age   INT
        )
    ")?;

    // Insert rows
    db.execute("INSERT INTO users (name, email, age) VALUES ('Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (name, email, age) VALUES ('Bob',   'bob@example.com',   25)")?;

    // Query
    let result = db.execute("SELECT id, name, age FROM users WHERE age > 20 ORDER BY name")?;
    for row in result.rows() {
        println!("{:?}", row);
    }

    Ok(())
}
```

### Explicit Transactions

```rust
let db = Database::open("./axiomdb.db")?;

db.transaction(|txn| {
    txn.execute("INSERT INTO accounts (owner, balance) VALUES ('Alice', 1000.00)")?;
    txn.execute("INSERT INTO accounts (owner, balance) VALUES ('Bob',     500.00)")?;
    // Both rows are committed atomically, or neither is if an error occurs.
    Ok(())
})?;
```

---

## Embedded Mode — C FFI

For C, C++, Qt, or Java (JNI):

```c
#include "axiomdb.h"

int main(void) {
    AxiomDb* db = axiomdb_open("./axiomdb.db");
    if (!db) { fprintf(stderr, "failed to open\n"); return 1; }

    char* result = NULL;
    int rc = axiomdb_execute(db, "SELECT id, name FROM users", &result);
    if (rc == 0) {
        printf("%s\n", result);   // result is JSON
        axiomdb_free_string(result);
    }

    axiomdb_close(db);
    return 0;
}
```

### Python via ctypes

```python
import ctypes, json

lib = ctypes.CDLL("./libaxiomdb.dylib")
lib.axiomdb_open.restype  = ctypes.c_void_p
lib.axiomdb_close.argtypes = [ctypes.c_void_p]
lib.axiomdb_execute.restype = ctypes.c_int

db = lib.axiomdb_open(b"./axiomdb.db")
result_ptr = ctypes.c_char_p()
lib.axiomdb_execute(db, b"SELECT * FROM users", ctypes.byref(result_ptr))
rows = json.loads(result_ptr.value)
lib.axiomdb_close(db)
```

---

## Your First Schema — End to End

The following example creates a minimal e-commerce schema, inserts sample data,
and runs a join query — all within embedded mode.

```sql
-- Create tables
CREATE TABLE products (
    id          BIGINT      PRIMARY KEY AUTO_INCREMENT,
    name        TEXT        NOT NULL,
    price       DECIMAL     NOT NULL,
    stock       INT         NOT NULL DEFAULT 0
);

CREATE TABLE orders (
    id          BIGINT      PRIMARY KEY AUTO_INCREMENT,
    product_id  BIGINT      NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    quantity    INT         NOT NULL,
    placed_at   TIMESTAMP   NOT NULL
);

CREATE INDEX idx_orders_product ON orders (product_id);

-- Insert data
INSERT INTO products (name, price, stock) VALUES
    ('Wireless Keyboard', 49.99, 200),
    ('USB-C Hub',         29.99, 500),
    ('Mechanical Mouse',  39.99, 150);

INSERT INTO orders (product_id, quantity, placed_at) VALUES
    (1, 2, '2026-03-01 10:00:00'),
    (2, 1, '2026-03-02 14:30:00'),
    (1, 1, '2026-03-03 09:15:00');

-- Query with JOIN
SELECT
    p.name,
    o.quantity,
    p.price * o.quantity AS line_total,
    o.placed_at
FROM orders o
JOIN products p ON p.id = o.product_id
ORDER BY o.placed_at;
```

Expected output:

| name               | quantity | line_total | placed_at           |
|--------------------|----------|------------|---------------------|
| Wireless Keyboard  | 2        | 99.98      | 2026-03-01 10:00:00 |
| USB-C Hub          | 1        | 29.99      | 2026-03-02 14:30:00 |
| Wireless Keyboard  | 1        | 49.99      | 2026-03-03 09:15:00 |

---

## Bulk Insert — Best Practices

The way you issue INSERT statements has a large impact on throughput. AxiomDB is
optimized for the **multi-row VALUES** form — one SQL string with all N rows:

```sql
-- Fast: one SQL string, all rows in one VALUES clause (~211K rows/s for 10K rows)
INSERT INTO products (name, price, stock) VALUES
  ('Widget A', 9.99, 100),
  ('Widget B', 14.99, 50),
  ('Widget C', 4.99, 200);
```

```python
# Python — build one multi-row string, one execute() call
rows = [(f"product_{i}", i * 1.5, i * 10) for i in range(10_000)]
placeholders = ", ".join("(%s, %s, %s)" for _ in rows)
flat_values   = [v for row in rows for v in row]
cursor.execute(f"INSERT INTO products (name, price, stock) VALUES {placeholders}",
               flat_values)
conn.commit()
```

**Why this matters:** issuing N separate INSERT statements each pays its own
parse + analyze overhead (~20 µs per string). A single multi-row string pays that
cost once for all rows.

| Approach | Throughput |
|---|---|
| **Multi-row VALUES** (1 string, N rows) | **211K rows/s** — recommended |
| N separate INSERT strings (1 txn) | ~35K rows/s — 6× slower |
| N separate autocommit INSERTs | ~58 q/s — 1 fsync per row |

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip — Batching large datasets</span>
For millions of rows, wrap every 5,000–10,000 rows in an explicit <code>BEGIN … COMMIT</code>
block. This limits WAL growth per transaction while keeping throughput high. See
<a href="features/transactions.md">Transactions</a> for Group Commit configuration,
which further improves concurrent write throughput.
</div>
</div>

---

## Next Steps

- [SQL Reference — Data Types](sql-reference/data-types.md) — full type system
- [SQL Reference — DDL](sql-reference/ddl.md) — CREATE TABLE, indexes, constraints
- [SQL Reference — DML](sql-reference/dml.md) — SELECT, INSERT, UPDATE, DELETE
- [Transactions](features/transactions.md) — BEGIN, COMMIT, ROLLBACK, MVCC
- [Performance](../performance.md) — benchmark numbers and tuning tips
