# AxiomDB Node.js native binding — performance notes

Native N-API addon (napi-rs) for AxiomDB, plus the measured comparison that
decided its design. The headline: **on Node, returning a packed buffer and
parsing it in JS beats building JS objects in Rust** — the opposite of Python,
where PyO3 native construction wins.

## Benchmark (10K rows × 6 cols, macOS, median of 11)

| Approach | time | vs better-sqlite3 |
|---|---|---|
| `better-sqlite3` `.raw().all()` (C++ / raw V8) | ~3.4 ms | 1.00× |
| **napi packed** (`queryPacked` → Buffer + JS parse) | **~5.8 ms** | **~1.66×** ← best AxiomDB path |
| koffi packed (`bindings/nodejs`, FFI + JS parse) | ~6.7 ms | ~2.03× |
| napi per-value (`queryTuples`, build JS in Rust) | ~9.2 ms | ~2.7× |
| koffi per-cell (original, ~2 FFI calls/cell) | ~28 ms | ~8.9× |

## Why building JS objects in Rust (napi) is *slower* here

PyO3 builds Python objects with the CPython C API directly and **beats** sqlite3
(0.79×). N-API is different: it is a **stable C ABI**, so every
`create_int32` / `create_string` / `array.set` is a guarded ABI call. For a
10K×6 result that is ~140K N-API calls — the overhead exceeds a single buffer
return plus a JS parse loop.

So the ranking inverts vs Python:

| | interim (buffer) | native object build |
|---|---|---|
| Python | ctypes packed 3.5× | **PyO3 0.79× — native wins** |
| Node | **packed 1.66× — buffer wins** | napi per-value 2.7× |

## Why napi packed (1.66×) beats koffi packed (2.03×)

Same JS parse on both; the difference is the single transfer:
- napi `create_buffer_with_data` is one optimized call into V8.
- koffi's FFI call + `koffi.view` window carries more marshalling overhead.

## Why we still don't reach better-sqlite3 (1.0×) on Node

better-sqlite3 uses the **raw V8 API** (node-gyp, version-specific) and builds
row objects in C++ with no intermediate buffer and no JS parse loop. The packed
path always pays a JS-side parse over every cell (~60K). Matching 1.0× would
require the same raw-V8 approach (much more complex, per-Node-version builds) —
low ROI given the packed path is already ~1.66× and sub-6 ms.

## Recommendation

- **Fastest Node path:** the napi `queryPacked`/`queryTuplesPacked` (~1.66×),
  but it needs the napi build toolchain (`@napi-rs/cli`, a `.node` per platform).
- **Simplest Node path:** the koffi packed binding in `bindings/nodejs`
  (~2.03×), pure JS + the shared `libaxiomdb_embedded` dylib, no native build.
- The naive napi per-value path is kept only for reference; it is the slowest of
  the native options.

## Build

```bash
cd bindings/axiomdb-node
npm install
npx napi build --release   # produces axiomdb-node.node
node test.mjs              # correctness vs better-sqlite3
node perf.mjs              # the table above
```
