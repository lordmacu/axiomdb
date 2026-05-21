# AxiomDB — Ruby binding

A Ruby binding for the AxiomDB embedded engine using **Fiddle** (Ruby's stdlib
FFI — no gem to install). Result materialization uses the **packed-buffer** path:
one FFI call returns the whole result as a single buffer, parsed once in Ruby.

## Usage

```ruby
require_relative 'axiomdb'

db = AxiomDB.new('./myapp.db') # or ":memory:"
db.execute("CREATE TABLE users (id INT, name TEXT)")
db.execute("INSERT INTO users VALUES (1, 'Alice')")

db.query_tuples("SELECT * FROM users")        # [[1, "Alice"]]
db.query("SELECT * FROM users")               # [{"id"=>1, "name"=>"Alice"}]
db.query_with_columns("SELECT * FROM users")  # [["id","name"], [[1,"Alice"]]]

db.begin_txn; db.execute("INSERT ..."); db.commit  # or db.rollback
db.close
```

Value types: `Integer`, `Float`, `String` (UTF-8), binary `String` (BLOB), or
`nil` (NULL). Ruby's `Integer` is arbitrary-precision, so i64 is exact (no BigInt
workaround like JS/Python need).

## Performance

10K rows × 6 cols, materialize every cell (macOS, median of 11):

| Binding | vs sqlite3 gem |
|---|---|
| `sqlite3` gem (native C extension) | 1.00× |
| **AxiomDB `query_tuples`** (columnar) | **~1.4×** |
| AxiomDB row-major packed (previous) | ~2.9× |

The big lever was a **columnar** packed format (`axiomdb_query_packed_columnar`,
AXM2). Instead of a row-major tag+payload stream parsed cell-by-cell, values are
grouped by column so Ruby decodes each homogeneous column with **one** bulk
`unpack('q<*')` / `unpack('E*')` and assembles rows with **`transpose`** — both
C-level — instead of an interpreted per-cell loop. That roughly halved the parse
time (~2.9× → ~1.4×). Columns containing NULLs or mixed types fall back to a
per-cell ('M') encoding, still correct.

The remaining gap vs the `sqlite3` gem (a native C extension that builds Ruby
objects in C) is the text-column slicing + the FFI round-trip, both largely
irreducible without a native extension.

## Parity path (deferred)

To match the `sqlite3` gem (~1.0×) would require a **native Ruby C extension**
that builds Ruby objects in C — the Ruby equivalent of the PyO3 work. Higher
effort; deferred until the ROI justifies it.

## Test / bench

```bash
cargo build --release -p axiomdb-embedded   # build the shared library first
ruby bindings/ruby/test.rb    # correctness, cross-checked vs the sqlite3 gem
ruby bindings/ruby/bench.rb   # the table above
```
