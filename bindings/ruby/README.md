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

## Parameter binding

Pass an array to bind `?` placeholders. This is **real prepared-statement
binding** (the engine parses, analyzes, and substitutes the values) — no string
interpolation, so untrusted input cannot inject SQL:

```ruby
db.execute('INSERT INTO users VALUES (?, ?)', [1, 'Alice'])
db.query_tuples('SELECT * FROM users WHERE id = ?', [1])
db.query('SELECT * FROM users WHERE name = ?', ['Alice'])
```

Param types: `nil`, `Integer`, `Float`, `true`/`false`, `String` (UTF-8 → TEXT,
binary-encoded → BLOB). `execute`, `query_tuples`, `query`, and
`query_with_columns` all take an optional `params` array.

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

### Ruby version

The binding is version-agnostic (Fiddle is stdlib on every Ruby; no
version-specific code). Newer Ruby + YJIT helps a little:

| Ruby | columnar ratio |
|---|---|
| 2.6 (system) | ~1.37× |
| 3.4 + `--yjit` | ~1.28× |

The difference is small because the columnar parser is already mostly bulk
C-level work (`unpack`/`transpose`), leaving little interpreted code for YJIT to
accelerate. (Results that contain NULLs use the per-cell 'M' path, where YJIT
helps more — so the win is larger on null-heavy data than this null-free bench.)

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
