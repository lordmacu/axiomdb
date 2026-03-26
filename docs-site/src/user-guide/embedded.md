# Embedded Mode

AxiomDB can run **in-process** — inside your application, with no TCP server, no daemon, no network round-trips. This is the SQLite model: the database is a library you link against, not a process you connect to.

The embedded crate ships two APIs:

| API | Language | Use case |
|-----|----------|---------|
| `Db` | Rust | Native Rust apps, desktop, CLI tools |
| `axiomdb_open` / `axiomdb_query` / … | C | C, C++, Python (`ctypes`), Swift, Kotlin JNI, Unity |
| `AsyncDb` | Rust + Tokio | Async Rust services |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Zero Network Overhead</span>
Every query is a direct function call. No TCP, no packet serialization, no thread context switch. Compared to connecting to a local MySQL or PostgreSQL server (~50–200 µs per query on localhost), an embedded AxiomDB query has no networking overhead at all.
</div>
</div>

## Build profiles

```toml
# Cargo.toml
[dependencies]
axiomdb-embedded = { path = "...", features = ["desktop"] }  # default
# axiomdb-embedded = { path = "...", features = ["async-api"] }  # + tokio
```

| Feature | Includes | Binary output |
|---------|----------|--------------|
| `desktop` (default) | Rust sync API + C FFI | `.dylib` / `.so` / `.dll` + `.a` |
| `async-api` | + tokio async wrapper | same + async |
| `wasm` | sync, in-memory (future) | `.wasm` |

The `desktop` build produces a ~1.1 MB dynamic library. The server binary (with full wire protocol) is ~2.1 MB. You get a leaner binary by only linking what you need.

---

## Rust API

### Opening a database

```rust
use axiomdb_embedded::Db;

// Creates ./myapp.db and ./myapp.wal if they don't exist.
// Runs crash recovery automatically if the WAL has uncommitted entries.
let mut db = Db::open("./myapp.db")?;
```

### DDL and DML

```rust
db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT, score REAL)")?;

let affected = db.execute("INSERT INTO users VALUES (1, 'Alice', 9.5)")?;
assert_eq!(affected, 1);

let affected = db.execute("UPDATE users SET score = 10.0 WHERE id = 1")?;
assert_eq!(affected, 1);

let affected = db.execute("DELETE FROM users WHERE score < 5.0")?;
```

### SELECT — rows only

```rust
let rows = db.query("SELECT * FROM users WHERE score > 8.0")?;
for row in &rows {
    // row is Vec<Value> — one Value per column
    println!("{:?}", row);
}
```

### SELECT — rows + column names

Use `query_with_columns` when you need the column names at runtime (building a
table display, serializing to JSON, passing headers to a UI component, etc.).

```rust
let (columns, rows) = db.query_with_columns("SELECT id, name FROM users")?;

println!("columns: {:?}", columns); // ["id", "name"]

for row in &rows {
    for (col, val) in columns.iter().zip(row.iter()) {
        println!("{col} = {val}");
    }
}
```

### Full QueryResult (metadata + last_insert_id)

```rust
use axiomdb_sql::result::QueryResult;

match db.run("INSERT INTO users VALUES (2, 'Bob', 7.2)")? {
    QueryResult::Affected { count, last_insert_id } => {
        println!("inserted {count} row, id = {:?}", last_insert_id);
    }
    QueryResult::Rows { columns, rows } => { /* SELECT */ }
    QueryResult::Empty => { /* DDL */ }
}
```

### Explicit transactions

```rust
db.begin()?;
db.execute("INSERT INTO orders VALUES (1, 100.0)")?;
db.execute("UPDATE inventory SET qty = qty - 1 WHERE id = 42")?;
db.commit()?;

// Or:
db.begin()?;
// ... something goes wrong ...
db.rollback()?;
```

### Error handling

```rust
match db.query("SELECT * FROM nonexistent") {
    Ok(rows) => { /* ... */ }
    Err(e) => {
        eprintln!("query failed: {e}");
        // Also accessible as a string for display/logging:
        if let Some(msg) = db.last_error() {
            eprintln!("last error: {msg}");
        }
    }
}
```

### Async (Tokio)

```rust
use axiomdb_embedded::async_db::AsyncDb;

#[tokio::main]
async fn main() {
    let db = AsyncDb::open("./myapp.db").await?;
    db.execute("CREATE TABLE t (id INT)").await?;

    let (columns, rows) = db.query_with_columns("SELECT * FROM t").await?;
}
```

`AsyncDb` wraps `Db` in an `Arc<Mutex<Db>>` and runs each operation in
`tokio::task::spawn_blocking`, keeping the async executor unblocked.

### Persist and reopen

The database persists on disk. Close it (drop the `Db`) and reopen it from
another process or session:

```rust
{
    let mut db = Db::open("./data.db")?;
    db.execute("CREATE TABLE log (ts BIGINT, msg TEXT)")?;
    db.execute("INSERT INTO log VALUES (1700000000, 'started')")?;
} // db is dropped here — WAL is flushed, file lock released

// Later — in the same process or a different one:
let mut db = Db::open("./data.db")?;
let rows = db.query("SELECT * FROM log")?;
assert_eq!(rows.len(), 1);
```

---

## C API

Link against `libaxiomdb_embedded.{so,dylib,dll}` or the static `libaxiomdb_embedded.a`.

### Header

```c
#include "axiomdb.h"
```

A minimal `axiomdb.h` to copy into your project:

```c
#pragma once
#include <stdint.h>
#include <stddef.h>

typedef struct AxiomDb    AxiomDb;
typedef struct AxiomRows  AxiomRows;

/* Type codes — same as SQLite for easy porting */
#define AXIOMDB_NULL     0
#define AXIOMDB_INTEGER  1   /* Bool, Int, BigInt, Date (days), Timestamp (µs) */
#define AXIOMDB_REAL     2   /* Real, Decimal */
#define AXIOMDB_TEXT     3   /* Text, UUID */
#define AXIOMDB_BLOB     4   /* Bytes */

/* Open / close */
AxiomDb*    axiomdb_open        (const char* path);
void        axiomdb_close       (AxiomDb* db);

/* Execute DML/DDL — returns rows affected, or -1 on error */
int64_t     axiomdb_execute     (AxiomDb* db, const char* sql);

/* Query — returns result set, or NULL on error */
AxiomRows*  axiomdb_query       (AxiomDb* db, const char* sql);

/* Result set accessors */
int64_t     axiomdb_rows_count        (const AxiomRows* rows);
int32_t     axiomdb_rows_columns      (const AxiomRows* rows);
const char* axiomdb_rows_column_name  (const AxiomRows* rows, int32_t col);
int32_t     axiomdb_rows_type         (const AxiomRows* rows, int64_t row, int32_t col);
int64_t     axiomdb_rows_get_int      (const AxiomRows* rows, int64_t row, int32_t col);
double      axiomdb_rows_get_double   (const AxiomRows* rows, int64_t row, int32_t col);
const char* axiomdb_rows_get_text     (const AxiomRows* rows, int64_t row, int32_t col);
const uint8_t* axiomdb_rows_get_blob  (const AxiomRows* rows, int64_t row, int32_t col, size_t* len);
void        axiomdb_rows_free         (AxiomRows* rows);

/* Last error message for this db handle — NULL if last operation succeeded */
const char* axiomdb_last_error  (const AxiomDb* db);
```

### Complete example

```c
#include <stdio.h>
#include <stdint.h>
#include "axiomdb.h"

int main(void) {
    AxiomDb* db = axiomdb_open("./app.db");
    if (!db) { fprintf(stderr, "failed to open db\n"); return 1; }

    axiomdb_execute(db,
        "CREATE TABLE IF NOT EXISTS products ("
        "  id INT NOT NULL, name TEXT, price REAL, active INTEGER"
        ")");

    axiomdb_execute(db, "INSERT INTO products VALUES (1, 'Widget', 9.99, 1)");
    axiomdb_execute(db, "INSERT INTO products VALUES (2, 'Gadget', 24.50, 1)");
    axiomdb_execute(db, "INSERT INTO products VALUES (3, 'Donut', 1.25, 0)");

    AxiomRows* rows = axiomdb_query(db,
        "SELECT id, name, price FROM products WHERE active = 1");

    if (!rows) {
        fprintf(stderr, "query error: %s\n", axiomdb_last_error(db));
        axiomdb_close(db);
        return 1;
    }

    /* Print header */
    int32_t ncols = axiomdb_rows_columns(rows);
    for (int32_t c = 0; c < ncols; c++) {
        printf("%-12s", axiomdb_rows_column_name(rows, c));
    }
    printf("\n");

    /* Print rows */
    int64_t nrows = axiomdb_rows_count(rows);
    for (int64_t r = 0; r < nrows; r++) {
        for (int32_t c = 0; c < ncols; c++) {
            switch (axiomdb_rows_type(rows, r, c)) {
                case AXIOMDB_INTEGER:
                    printf("%-12lld", (long long)axiomdb_rows_get_int(rows, r, c));
                    break;
                case AXIOMDB_REAL:
                    printf("%-12.2f", axiomdb_rows_get_double(rows, r, c));
                    break;
                case AXIOMDB_TEXT:
                    printf("%-12s", axiomdb_rows_get_text(rows, r, c));
                    break;
                case AXIOMDB_NULL:
                    printf("%-12s", "NULL");
                    break;
                default:
                    printf("%-12s", "?");
            }
        }
        printf("\n");
    }

    axiomdb_rows_free(rows);
    axiomdb_close(db);
    return 0;
}
```

Output:
```
id          name        price
1           Widget      9.99
2           Gadget      24.50
```

### Type mapping

| SQL type | C accessor | C type |
|----------|-----------|--------|
| `BOOL` | `axiomdb_rows_get_int` | `0` or `1` |
| `INT` | `axiomdb_rows_get_int` | `int64_t` |
| `BIGINT` | `axiomdb_rows_get_int` | `int64_t` |
| `REAL` / `DOUBLE` | `axiomdb_rows_get_double` | `double` |
| `DECIMAL` | `axiomdb_rows_get_double` | `double` (may lose precision for >15 digits) |
| `TEXT` / `VARCHAR` | `axiomdb_rows_get_text` | `const char*` (UTF-8) |
| `UUID` | `axiomdb_rows_get_text` | `const char*` (`xxxxxxxx-xxxx-…`) |
| `DATE` | `axiomdb_rows_get_int` | days since 1970-01-01 |
| `TIMESTAMP` | `axiomdb_rows_get_int` | microseconds since 1970-01-01 UTC |
| `BLOB` / `BYTEA` | `axiomdb_rows_get_blob` | `const uint8_t*` + `size_t len` |
| `NULL` | type code = `AXIOMDB_NULL` | — |

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Pointer lifetimes</span>
All pointers returned by <code>axiomdb_rows_get_text</code>, <code>axiomdb_rows_get_blob</code>, and <code>axiomdb_rows_column_name</code> are valid until <code>axiomdb_rows_free</code> is called. Copy the data if you need it to outlive the result set.
</div>
</div>

### Python (ctypes)

```python
import ctypes, os

lib = ctypes.CDLL("./libaxiomdb_embedded.dylib")  # or .so on Linux

lib.axiomdb_open.restype = ctypes.c_void_p
lib.axiomdb_open.argtypes = [ctypes.c_char_p]

lib.axiomdb_execute.restype = ctypes.c_int64
lib.axiomdb_execute.argtypes = [ctypes.c_void_p, ctypes.c_char_p]

lib.axiomdb_query.restype = ctypes.c_void_p
lib.axiomdb_query.argtypes = [ctypes.c_void_p, ctypes.c_char_p]

lib.axiomdb_rows_count.restype = ctypes.c_int64
lib.axiomdb_rows_count.argtypes = [ctypes.c_void_p]

lib.axiomdb_rows_get_text.restype = ctypes.c_char_p
lib.axiomdb_rows_get_text.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32]

lib.axiomdb_rows_free.argtypes = [ctypes.c_void_p]
lib.axiomdb_close.argtypes = [ctypes.c_void_p]

db = lib.axiomdb_open(b"./app.db")
lib.axiomdb_execute(db, b"CREATE TABLE IF NOT EXISTS t (id INT, name TEXT)")
lib.axiomdb_execute(db, b"INSERT INTO t VALUES (1, 'hello')")

rows = lib.axiomdb_query(db, b"SELECT id, name FROM t")
for r in range(lib.axiomdb_rows_count(rows)):
    id_  = lib.axiomdb_rows_get_text(rows, r, 0)
    name = lib.axiomdb_rows_get_text(rows, r, 1)
    print(f"id={id_.decode()}, name={name.decode()}")

lib.axiomdb_rows_free(rows)
lib.axiomdb_close(db)
```

---

## Build the shared library

```bash
# Dynamic library (.dylib / .so / .dll)
cargo build --release -p axiomdb-embedded

# Static library (.a) — for iOS, embedded systems, Unity AOT
cargo build --release -p axiomdb-embedded
# → target/release/libaxiomdb_embedded.a

# With async support
cargo build --release -p axiomdb-embedded --features async-api
```

Output files are in `target/release/`:
- macOS: `libaxiomdb_embedded.dylib`
- Linux: `libaxiomdb_embedded.so`
- Windows: `axiomdb_embedded.dll`
- All platforms: `libaxiomdb_embedded.a` (static)
