# Spec: python-binding-pyo3 — native PyO3 extension (sqlite3 parity)

Phase: perf-sqlite-gap — Python binding parity
Task: Task 2 of sprint python-binding-perf (PyO3 native extension)
Status: implemented

## Context

Task 1 (packed buffer, ctypes) cut Python result materialization from ~11× to
~3.5× slower than the stdlib `sqlite3` module by collapsing ~120K per-cell FFI
calls into one. The structural cap of the ctypes approach is the Python-level
parse loop over every cell.

`sqlite3` is fast because it is a CPython C extension that builds Python objects
(`PyLong`/`PyUnicode`/`PyFloat`/`PyBytes`) in one C loop with zero per-cell
Python/C boundary crossings. PyO3 reproduces that exactly from Rust. This task
ships a native PyO3 extension to reach (and exceed) sqlite3 parity.

## Goal

A native Python extension module (`axiomdb_native`) that materializes query
results by constructing Python objects directly in Rust, matching or beating
`sqlite3.Cursor.fetchall()` speed.

## Non-goals

- Not a full DB-API 2.0 cursor (fetchone/fetchmany/description, paramstyle) —
  could follow; v1 exposes `connect`/`execute`/`query`/`query_dict`.
- Not replacing the ctypes binding (kept for zero-build/pure-Python use).
- Not exposing the Appender or concurrent `SharedDb`/`Connection` yet (v2).
- Not GIL-release during the engine call (single-thread parity is the target).

## Behavior

### Module `axiomdb_native`

```python
import axiomdb_native as adb

conn = adb.connect("app.db")            # or adb.Connection("app.db")
conn.execute("CREATE TABLE t (id INT, name TEXT)")   # -> rows affected (int)
rows  = conn.query("SELECT * FROM t")                # -> list[tuple]   (fast)
recs  = conn.query_dict("SELECT * FROM t")           # -> list[dict]
cols, rows = conn.query_with_columns("SELECT * FROM t")  # -> (list[str], list[tuple])
conn.begin(); conn.commit(); conn.rollback()
conn.close()
with adb.connect(":memory:") as c: ...               # context manager
adb.AxiomDBError                                     # exception type
```

### Object construction (the parity-critical part)

- `query()` calls `Db::query` (engine → `Vec<Vec<Value>>`), then builds
  `PyList::new_bound(py, rows.map(|r| PyTuple::new_bound(py, r.map(value_to_py))))`
  in one Rust loop — no per-cell FFI, no serialization buffer.
- `value_to_py` maps each `Value` to a Python object with the SAME rules as the
  C-FFI `value_to_cell` (Bool/Int/BigInt/Date/Timestamp→int, Real/Decimal→float,
  Text/Json/Jsonb/Uuid→str, Bytes→bytes, Null→None, else display string).

### Distribution

- `crate-type = ["cdylib"]`, `pyo3` with `abi3-py38` → ONE wheel works on all
  CPython ≥ 3.8 (stable ABI). Built with maturin.
- The crate is excluded from the Cargo workspace (links libpython; would break
  `cargo build --workspace` on the Lima VM). Built only via maturin.

## Edge cases

- [x] NULL cells → None
- [x] Unicode text round-trips (héllo, 日本語)
- [x] Empty result → []
- [x] Closed connection → AxiomDBError
- [x] Bad SQL → AxiomDBError
- [x] Explicit txn commit/rollback
- [x] Context manager closes on exit

## Performance budget

| Operation | Target | Measured (10K×6, macOS) |
|---|---|---|
| `query()` vs sqlite3 fetchall | ≤ 1.2× | **0.77× (faster than sqlite3)** |

Baselines on the same host: sqlite3 ~7.0 ms; ctypes per-cell ~78 ms (11×);
ctypes packed ~24 ms (3.5×); **PyO3 ~5.3 ms (0.77×)**.

## Dependencies

- Depends on: `axiomdb-embedded::Db`, `axiomdb-types::Value`, `pyo3 0.22 (abi3)`.
- Build tool: maturin ≥ 1.5.

## Done criteria

- [x] `axiomdb_native` builds as an abi3 wheel via maturin
- [x] `query`/`query_dict`/`query_with_columns`/`execute`/txn/close/ctx-mgr work
- [x] 8 correctness tests cross-checked against sqlite3 pass
- [x] `query()` ≤ 1.2× sqlite3 (achieved 0.77×)
- [x] Crate excluded from workspace; `cargo build --workspace` unaffected

## References

- Research findings (this session): CPython `_sqlite/cursor.c`, PyO3 perf guide,
  abi3, orjson object-construction model.
- Task 1 spec: `specs/fase-perf-sqlite-gap/spec-python-binding-packed.md`
- Crate: `bindings/axiomdb-py/`
