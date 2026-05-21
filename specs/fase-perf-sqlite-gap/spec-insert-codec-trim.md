# Spec: INSERT codec trim (NFC ASCII fast-path + precomputed schema)

Phase: perf-sqlite-gap — write parity with SQLite (inserts)
Task: cut `prepare_row` / `encode_row` per-row cost without changing the on-disk format
Status: implemented (NFC ASCII fast-path; `column_data_types` precompute + buffer reuse deferred — marginal ROI)

## Context

After the parser fast-path subphase, the fair `insert_batch` per-row cost is
execute-dominated (~3.6µs, 60%), and within execute the biggest item is
`prepare_row (codec+PK)` ~1.1µs (`--diagnose-insert-deep`). `prepare_row`
(`crates/axiomdb-sql/src/clustered_table.rs:34`) calls `encode_row`
(`crates/axiomdb-types/src/codec.rs:369`) which, for every `Text`/`Json` value,
**NFC-normalizes into a fresh `String` per row** (`s.nfc().collect()`, codec.rs:424
& :448) — wasted work when the text is ASCII (ASCII is invariant under NFC). It
also rebuilds a `Vec<DataType>` (`column_data_types(columns)`, clustered_table.rs:106)
**per row**. SQLite's `OP_MakeRecord` is cheap partly via minimal-width integer
serial types and buffer reuse (studied in `research/sqlite/src/vdbe.c`); we adopt
the parts that do **not** change our byte format.

## Goal

Reduce `prepare_row`/`encode_row` per-row work — chiefly by skipping NFC
normalization for ASCII text and precomputing the column schema once per statement
— with **byte-identical** encoded output (no on-disk format change).

## Non-goals

- **Minimal-width integer serial types** (SQLite's 1/2/3/4/6/8-byte ints, 0/1 in
  zero bytes) — would shrink rows but **changes the on-disk row format** (breaks
  `decode_row`, indexes, TOAST, MVCC headers, existing files). DEFERRED to a future
  format-modernization phase; tracked as a gap.
- **`encode_row_into` single-buffer reuse** — NOT viable here: the clustered insert
  batch accumulates `N` `PreparedClusteredInsertRow`, each holding its own
  `encoded_row: Vec<u8>` that must persist for the Attack-15 batched WAL write, so
  one buffer cannot be reused across rows (SQLite can because it inserts one row at
  a time). The per-row `Vec` is already sized exactly via `encoded_len`. Out of scope.
- The embedded `Db::run` wrapper overhead (~2µs/row) — separate lever.
- DELETE / UPDATE paths.

## Behavior

### Public API

No public signature change. `encode_row(values, schema) -> Result<Vec<u8>, DbError>`
keeps its signature and output bytes. Internal change only:

```rust
// codec.rs — Text / Json arms gain an ASCII fast-path:
//   if s.is_ascii() { use s.as_bytes() directly (no String alloc, no nfc()) }
//   else            { existing s.nfc().collect() path, unchanged }
```

The insert batch path precomputes the schema once and threads it to `prepare_row`
(internal `pub(crate)` signature may change to take `&[DataType]`; the public
`encode_row` is unaffected).

### Semantics

- **NFC ASCII fast-path (Text & Json):** when `s.is_ascii()`, the bytes written are
  `s.as_bytes()` directly. For Json, serde validation still runs (on `s`); only the
  normalization+alloc is skipped. The length check (`> MAX_INLINE_LEN`) runs on the
  same bytes.
- **Precompute schema:** `execute_clustered_insert` computes
  `column_data_types(&resolved.columns)` once and passes the slice into the per-row
  `prepare_row`, instead of each row deriving it.
- **Invariant (critical):** for any `s`, the encoded bytes are **identical** before
  and after. Justification: for `s.is_ascii()`, `s.nfc().collect::<String>()` equals
  `s` byte-for-byte (ASCII is NFC-stable: no composition, no reordering). Non-ASCII
  always takes the unchanged NFC path. Therefore `decode_row` and all on-disk/index
  consumers are unaffected.
- Precondition: `values.len() == schema.len()` (unchanged).
- Postcondition: returned bytes round-trip through `decode_row` to the input values.

### Error cases

No new error paths; identical to today:

| Input | Expected error |
|-------|----------------|
| `values.len() != schema.len()` | `DbError::TypeMismatch` |
| Text/Bytes/Json `len > MAX_INLINE_LEN` | `DbError::ValueTooLarge` |
| invalid JSON (ASCII or not) | `DbError::InvalidValue` (serde) |
| `Real` is NaN | `DbError::InvalidValue` |

## Edge cases

Each becomes a test asserting **fast-path bytes == NFC-path bytes** (and round-trip):

- [ ] Pure ASCII Text (`'user_000001'`, `'u1@b.local'`) → fast-path, identical bytes
- [ ] Empty string `''` → `is_ascii()` true, zero-length payload, identical
- [ ] Non-ASCII Text (`'café'`, CJK, emoji) → NFC path, unchanged
- [ ] Combining sequence (`e` + U+0301) → non-ASCII → NFC composes to `é` (unchanged)
- [ ] Pre-composed `é` (U+00E9) → non-ASCII → NFC path (unchanged)
- [ ] ASCII Json (`'{"a":1}'`) → fast-path, serde still validates, identical
- [ ] Non-ASCII Json (`'{"k":"café"}'`) → NFC path, unchanged
- [ ] Invalid Json (ASCII) → `DbError::InvalidValue` (same as before)
- [ ] Text exactly at / over `MAX_INLINE_LEN` (ASCII) → `ValueTooLarge` (same)
- [ ] NULL Text (bitmap) → skipped, unaffected
- [ ] Multi-row INSERT → schema computed once, every row identical to per-row derive
- [ ] Composite/Array/Range arms (recurse into `encode_row`) still correct

## On-disk format

**Unchanged.** This is the central invariant — the row codec output is byte-for-byte
identical. No version bump, no migration. (Minimal-width ints, which *would* change
it, are explicitly deferred.)

## Performance budget

`axiomdb_bench --scenario insert_batch --rows 10000` (macOS, medians; the low-noise
`--diagnose-insert-deep` is the primary metric).

| Metric | Before | Target | Max acceptable |
|--------|--------|--------|----------------|
| `prepare_row (codec+PK)` (`--diagnose-insert-deep`) | ~1.1µs | ≤ ~0.8µs | ≤ 1.0µs |
| insert_batch ratio vs SQLite | ~4.6× | ≤ ~4.4× | < 4.6× |
| any other `encode_row` consumer | unchanged | unchanged | no regression |

Honest note: ROI is modest (~0.3µs) — the larger structural levers (minimal-width
format, `Db::run` wrapper) are out of scope. The NFC fast-path also helps **every**
write path that encodes ASCII text, not just this bench.

## Dependencies

- Depends on: faster-insert-parser subphase (done) for a clean before/after baseline.
- Blocks: nothing.

## Open questions

- [x] Does the ASCII fast-path change bytes? **No** — ASCII is NFC-stable (resolved).
- [x] Apply to Json too? **Yes**, keeping serde validation (resolved).
- [x] Buffer reuse? **No** — incompatible with the accumulated batch (resolved, non-goal).

## Done criteria

- [ ] `encode_row` ASCII Text/Json take the fast-path (no `nfc()`/`String` alloc)
- [ ] Property test: for a mixed ASCII+non-ASCII corpus, encoded bytes are identical
      to the pre-change implementation, and `decode_row(encode_row(x)) == x`
- [ ] `column_data_types` computed once per statement on the clustered insert path
- [ ] `cargo nextest run -p axiomdb-types -p axiomdb-sql` passes — Lima
- [ ] `cargo nextest run --workspace` passes — Lima (subphase close)
- [ ] `cargo clippy --workspace -- -D warnings` clean; `cargo fmt --check` clean
- [ ] `--diagnose-insert-deep` shows `prepare_row` within budget; insert_batch ratio
      improved-or-equal; no regression on other `encode_row` consumers
- [ ] rustdoc note on the fast-path; gap (minimal-width ints) recorded in progreso/checkpoint

## References

- `crates/axiomdb-types/src/codec.rs:369` `encode_row` (Text :418, Json :445)
- `crates/axiomdb-sql/src/clustered_table.rs:34` `prepare_row`, `:98` `encode_prepared_row`
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` batch loop (precompute site)
- SQLite study: `research/sqlite/src/vdbe.c` `OP_MakeRecord`, `src/vdbeaux.c` serial types
- Prior subphase: `specs/fase-perf-sqlite-gap/spec-faster-insert-parser.md`
