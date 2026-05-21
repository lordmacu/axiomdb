# AxiomDB — Swift binding

A Swift Package for the AxiomDB embedded engine. Swift imports the C FFI
directly (no marshalling layer), and result materialization uses the
**packed-buffer** path: one FFI call returns the whole result as a single
buffer, decoded once in Swift with `loadUnaligned` (compiled, allocation-light).

Ideal for iOS/macOS apps — the same niche SQLite owns on Apple platforms.

## Usage

```swift
import AxiomDB

let db = try AxiomDB("./myapp.db") // or ":memory:"
defer { db.close() }

try db.execute("CREATE TABLE users (id INT, name TEXT)")
try db.execute("INSERT INTO users VALUES (1, 'Alice')")

let rows = try db.queryTuples("SELECT * FROM users")        // [[.int(1), .text("Alice")]]
let recs = try db.query("SELECT * FROM users")              // [["id": .int(1), ...]]
let (cols, rs) = try db.queryWithColumns("SELECT * FROM users")

try db.begin(); try db.execute("INSERT ..."); try db.commit() // or db.rollback()
```

Cells are an `AxiomValue` enum: `.null`, `.int(Int64)`, `.double(Double)`,
`.text(String)`, `.blob([UInt8])`. Swift's `Int64` is native, so integers are
exact.

## Performance

10K rows × 6 cols, materialize every cell into Swift values (macOS, median):

| Binding | vs system SQLite3 |
|---|---|
| system `SQLite3` C API (`sqlite3_column_*`) | 1.00× |
| **AxiomDB `queryTuples`** (packed) | **~1.5×** |

This is the same class as the C binding (~1.50×). Swift's SQLite baseline is the
**direct C API** — no FFI/wrapper overhead and lazy zero-copy column access — so
it is the toughest baseline (unlike Go/Python, whose SQLite wrappers add their
own per-row overhead that AxiomDB's single packed call beats). The residual
~1.5× is AxiomDB's engine materialization (`Vec<Value>`) + the packed round-trip
vs SQLite reading columns in place; closing it needs lazy zero-copy decode in
the engine (Approach B), low ROI for now.

## Build / test / bench

The shared library must exist first:

```bash
cargo build --release -p axiomdb-embedded   # target/release/libaxiomdb_embedded.dylib
cd bindings/swift
swift test                     # correctness
swift run -c release bench     # the table above (vs system SQLite3)
```

`Package.swift` computes the absolute `target/release` path from the manifest
location, so the linker and runtime rpath resolve in local development.
