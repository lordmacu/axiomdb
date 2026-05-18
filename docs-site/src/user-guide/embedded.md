# Embedded Mode

AxiomDB can run **in-process** — inside your application, with no TCP server, no daemon, no network round-trips. This is the SQLite model: the database is a library you link against, not a process you connect to.

The embedded crate ships two APIs:

| API | Language | Use case |
|-----|----------|---------|
| `Db` | Rust | Native Rust apps, desktop, CLI tools |
| `axiomdb_open` / `axiomdb_query` / … | C | C, C++, Python (`ctypes`), Swift, Kotlin JNI, Unity |
| `AsyncDb` | Rust + Tokio | Async Rust services |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Local DSN Only</span>
Like SQLite's split between URI parsing and VFS-specific validation, AxiomDB now parses DSNs centrally but keeps embedded mode local-only. In Phase <code>5.15</code>, <code>Db::open_dsn</code> and <code>axiomdb_open_dsn</code> accept filesystem DSNs and reject remote wire endpoints explicitly.
</div>
</div>

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
// Also verifies every catalog-visible index before returning the handle.
let mut db = Db::open("./myapp.db")?;
let mut db2 = Db::open_dsn("file:/tmp/myapp.db")?;
let mut db3 = Db::open_dsn("axiomdb:///tmp/myapp")?;
```

Remote DSNs such as `postgres://user@127.0.0.1:5432/app` are not supported by
embedded mode in Phase `5.15` and return `DbError::InvalidDsn`.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Open Can Repair Or Refuse</span>
Embedded open now performs startup index verification. A readable-but-divergent
index is rebuilt automatically; an unreadable tree returns
<code>DbError::IndexIntegrityFailure</code> and the handle is never created.
</div>
</div>

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

### Fast-path INSERT — `Appender`

For bulk loads, `Db::appender(table)` opens an
[`Appender`](https://docs.rs/axiomdb-embedded) that skips the SQL parser,
analyzer, and dispatcher and writes typed [`Value`]s directly to the
heap. Analog of DuckDB's Appender and SQLite's `sqlite3_bind_*` +
`sqlite3_step`.

```rust
use axiomdb_types::Value;

let mut app = db.appender("users")?;
app.append_row(&[Value::Int(1), Value::Text("Alice".into())])?;
app.append_row(&[Value::Int(2), Value::Text("Bob".into())])?;
let n_inserted = app.finish()?; // flush + commit
```

#### Typed builder (Attack 8)

For typed callers (e.g. ORM generators, code-gen pipelines) the
Appender also exposes per-column setters analog to DuckDB's
`Append<T>` and SQLite's `sqlite3_bind_<type>`:

```rust
let mut app = db.appender("users")?;
app.append_int(1)?;
app.append_text("Alice")?;
app.end_row()?;
app.append_int(2)?;
app.append_text("Bob")?;
app.end_row()?;
app.finish()?;
```

Available setters: `append_int(i32)`, `append_bigint(i64)`,
`append_bool(bool)`, `append_real(f64)`, `append_text(&str)`,
`append_bytes(&[u8])`, `append_null()`. Each row needs exactly
N values (= table column count) before `end_row()`, else
`TypeMismatch`. On any error inside `end_row()`, the in-progress
row is cleared and the appender remains usable.

#### C FFI (Attack 8)

The Appender is exposed through the C FFI for use from C / C++ /
Python (PyO3) / Node.js (napi-rs) / Swift / Kotlin:

```c
AxiomDbAppender* app = axiomdb_appender_open(db, "users");
if (!app) { fprintf(stderr, "%s\n", axiomdb_last_error(db)); return 1; }
axiomdb_appender_append_int(app, 1);
axiomdb_appender_append_text(app, "Alice");
axiomdb_appender_end_row(app);
int64_t n = axiomdb_appender_finish(app);   // commits + frees
```

Functions: `axiomdb_appender_open`, `axiomdb_appender_append_int`/
`bigint`/`bool`/`real`/`text`/`bytes`/`null`,
`axiomdb_appender_end_row`, `axiomdb_appender_flush`,
`axiomdb_appender_finish`, `axiomdb_appender_free` (rollback).
Errors return -1 (or NULL); the message is in
`axiomdb_last_error(db)`.

The Appender holds a single transaction; `finish()` commits, `drop`
without `finish` rolls back. **v1.1 supports every table SQL `INSERT`
supports except those with triggers**: clustered (PRIMARY KEY) tables,
`CHECK` constraints, `FOREIGN KEY` constraints, `AUTO_INCREMENT` /
`SERIAL` columns, and `GENERATED ALWAYS` columns all work. The
Appender returns `DbError::NotImplemented` at `appender()` open time
only when the table has a trigger.

For tables with constraints the per-row pipeline is:

1. Arity check (n columns = n values)
2. AUTO_INCREMENT assignment (if column is `AUTO_INCREMENT` and value
   is `Value::Null`)
3. STORED `GENERATED ALWAYS` column materialization
4. Text constraints (CHAR padding, VARCHAR length)
5. Type coercion (respects session `strict_mode`)
6. NOT NULL check
7. CHECK constraints
8. FOREIGN KEY validation (immediate FKs per row, deferred FKs
   queued and resolved at `finish()`/commit)

The Appender honors `SET synchronous` ([transactions
docs](features/transactions.md#durability--set-synchronous)) — the
session's durability setting at open time is stamped on the Appender's
transaction.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Throughput</span>
On Lima virtio (5000 rows, 5 iters): <strong>82K ops/s</strong> for
the v1.1 Appender on clustered tables (PRIMARY KEY) and
<strong>191K ops/s</strong> for heap-only tables, vs ~4.6K for the
same row inserted via <code>db.run("INSERT ...")</code> — a
<strong>~18-42× speedup</strong>. The remaining gap vs SQLite's
prepared-bind+step (1.7M) is dominated by B-Tree split overhead on
clustered tables, the next optimization target. See
<code>docs/perf-sqlite-gap.md</code> "Attack 7 v1.1" for the full
breakdown.
</div>
</div>

See `specs/fase-perf-sqlite-gap/spec-embedded-appender.md` for the full
design.

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
    let db2 = AsyncDb::open_dsn("file:/tmp/myapp.db").await?;
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
AxiomDb*    axiomdb_open_dsn    (const char* dsn);
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
    AxiomDb* db2 = axiomdb_open_dsn("file:/tmp/app.db");
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
    axiomdb_close(db2);
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
