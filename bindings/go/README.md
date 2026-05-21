# AxiomDB — Go binding

A cgo binding for the AxiomDB embedded engine. Result materialization uses the
**packed-buffer** path: one cgo call returns the whole result set as a single
buffer, parsed once in compiled Go. cgo has notable per-call overhead, so this
beats a per-row/per-column approach — and beats `mattn/go-sqlite3`, which crosses
cgo on every `rows.Next()`/`Scan`.

## Usage

```go
import "axiomdb"

db, err := axiomdb.Open("./myapp.db") // or ":memory:"
defer db.Close()

db.Execute("CREATE TABLE users (id INT, name TEXT)")
db.Execute("INSERT INTO users VALUES (1, 'Alice')")

rows, _ := db.QueryTuples("SELECT * FROM users") // [][]any{{int64(1), "Alice"}}
recs, _ := db.Query("SELECT * FROM users")       // []map[string]any
cols, rs, _ := db.QueryWithColumns("SELECT * FROM users")

db.Begin(); db.Execute("INSERT ..."); db.Commit() // or db.Rollback()
```

Value types: `int64`, `float64`, `string`, `[]byte`, or `nil` (NULL). Go's native
`int64` means integers are exact (no BigInt workaround like JS/Python need).

## Parameter binding

Pass variadic values to bind `?` placeholders. This is **real prepared-statement
binding** (the engine parses, analyzes, and substitutes the values) — no string
interpolation, so untrusted input cannot inject SQL:

```go
db.Execute("INSERT INTO users VALUES (?, ?)", 1, "Alice")
rows, _ := db.QueryTuples("SELECT * FROM users WHERE id = ?", 1)
recs, _ := db.Query("SELECT * FROM users WHERE name = ?", "Alice")
```

Param types: `nil`, `bool`, `int`/`int32`/`int64`, `float32`/`float64`,
`string`, and `[]byte` (BLOB). `Execute`, `QueryTuples`, `Query`, and
`QueryWithColumns` all take variadic params.

## Performance

10K rows × 6 cols, materialize every cell (macOS, median):

| Binding | vs go-sqlite3 |
|---|---|
| `mattn/go-sqlite3` (cgo, `rows.Scan`) | 1.00× |
| **AxiomDB `QueryTuples`** | **~0.70× (FASTER)** |

AxiomDB is ~1.4× faster: one cgo call + a compiled Go parse vs go-sqlite3's
per-row cgo crossings. Go is the second binding (after Python/PyO3) where AxiomDB
beats its SQLite baseline.

## Build / test / bench

The shared library must exist first:

```bash
cargo build --release -p axiomdb-embedded   # produces target/release/libaxiomdb_embedded.*
cd bindings/go
go test ./...        # correctness
go run ./bench       # the table above (needs mattn/go-sqlite3, pulled by go.mod)
```

The cgo `LDFLAGS` link against `../../target/release` with an rpath, so the
binary finds the dylib at runtime in local development.
