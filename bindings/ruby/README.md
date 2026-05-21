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
| **AxiomDB `query_tuples`** (Fiddle packed) | **~2.9×** |

The `sqlite3` gem is a native C extension that builds Ruby objects in C — the
Ruby analogue of Python's PyO3 or Node's better-sqlite3. Our Fiddle path pays
FFI overhead plus an interpreted parse loop, so ~2.9× is the practical floor
here (comparable to Python's ctypes packed at ~3.5×).

Notes:
- The parser uses `byteslice(off, n).unpack1(fmt)`. The `unpack` `@`-directive
  (absolute position, no slice copy) was measured *slower* on Ruby 2.6 because
  building the format string per field costs more than the slice.
- Ruby ≥ 3.1 has `unpack1(fmt, offset:)` which avoids the slice copy and would
  likely help; this binding targets the system Ruby 2.6 for portability.

## Parity path (deferred)

To match the `sqlite3` gem (~1.0×) would require a **native Ruby C extension**
that builds Ruby objects in C (no buffer, no interpreted parse) — the Ruby
equivalent of the PyO3 work. Higher effort; deferred until the ROI justifies it.

## Test / bench

```bash
cargo build --release -p axiomdb-embedded   # build the shared library first
ruby bindings/ruby/test.rb    # correctness, cross-checked vs the sqlite3 gem
ruby bindings/ruby/bench.rb   # the table above
```
