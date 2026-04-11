# Plan: 11.16 — Binary JSONB + SQL:2016 JSONPath

## Files to create/modify

### New files

- `crates/axiomdb-types/src/jsonb.rs`
  Binary JSONB encoder (`JsonbEncoder`), decoder (`JsonbDecoder`), and zero-copy
  access struct `JsonbRef<'a>`. Owns all layout constants and key binary search.

- `crates/axiomdb-sql/src/eval/jsonpath.rs`
  JSONPath compiler (`parse_jsonpath`) and executor (`execute_jsonpath`).
  Contains `PathStep`, `FilterExpr`, `FilterPath`, `CmpOp`, `FilterLiteral`,
  `PathMode` enums.

- `crates/axiomdb-sql/tests/integration_jsonb.rs`
  Integration tests for all new functions, `->` operator, TOAST, and coexistence
  with Phase 11.4 `JSON` rows.

### Modified files

- `crates/axiomdb-types/src/value.rs` — add `Value::Jsonb(Arc<Vec<u8>>)` variant
- `crates/axiomdb-types/src/types.rs` — add `DataType::Jsonb`
- `crates/axiomdb-types/src/lib.rs` — `pub mod jsonb;` and re-exports
- `crates/axiomdb-types/src/codec.rs` — encode/decode arms for `DataType::Jsonb`
- `crates/axiomdb-types/src/coerce_api.rs` — `Text↔Jsonb`, `Json↔Jsonb` coercions
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Jsonb = 10`
- `crates/axiomdb-sql/src/lexer.rs` — `Token::JsonExtractSub` (`->`) and `Token::TyJsonb`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `JSONB` data type
- `crates/axiomdb-sql/src/parser/expr.rs` — `->` operator precedence level
- `crates/axiomdb-sql/src/expr.rs` — `BinaryOp::JsonSub` variant
- `crates/axiomdb-sql/src/eval/ops.rs` — evaluate `BinaryOp::JsonSub`
- `crates/axiomdb-sql/src/eval/functions/json.rs` — all new functions + binary upgrades
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — dispatch new function names
- `crates/axiomdb-network/src/mysql/result.rs` — `Value::Jsonb` → VAR_STRING
- `crates/axiomdb-network/src/mysql/prepared.rs` — `Value::Jsonb` → text string
- `crates/axiomdb-embedded/src/lib.rs` — `Value::Jsonb` → string conversion
- `crates/axiomdb-types/tests/integration_row_codec.rs` — JSONB codec round-trip tests

---

## Algorithm / Data structure

### Binary Layout Constants

```rust
// Layout flags in the 4-byte container header
const CONTAINER_IS_ARRAY:   u32 = 0x8000_0000; // bit 31 set = array
const CONTAINER_COUNT_MASK: u32 = 0x7FFF_FFFF; // bits 30..0 = element count
const JENTRY_FSCALAR:       u32 = 0x0100_0000; // scalar wrapper flag

// JEntry (u32 per element)
const JENTRY_HAS_OFF:    u32 = 0x8000_0000; // bit 31: 1=offset stored, 0=length stored
const JENTRY_TYPE_MASK:  u32 = 0x7000_0000; // bits 30..28: type
const JENTRY_OFF_MASK:   u32 = 0x0FFF_FFFF; // bits 27..0: length or absolute offset

// JEntry types (bits 30..28)
const JENTRY_ISSTRING:    u32 = 0x0000_0000; // 0b000
const JENTRY_ISNUMERIC:   u32 = 0x1000_0000; // 0b001 (stored as text repr)
const JENTRY_ISFALSE:     u32 = 0x2000_0000; // 0b010
const JENTRY_ISTRUE:      u32 = 0x3000_0000; // 0b011
const JENTRY_ISNULL:      u32 = 0x4000_0000; // 0b100
const JENTRY_ISCONTAINER: u32 = 0x5000_0000; // 0b101 (nested obj/arr)

// Every STRIDE JEntries, store absolute offset instead of length
// → random access to any element is O(STRIDE) = O(1)
const JENTRY_STRIDE: usize = 32;
```

### Memory layout (on-disk bytes)

```
OBJECT with N keys:
  [0..3]        : u32 container header  (bit31=0, bits30..0=N)
  [4..4+8N-1]   : [u32; 2*N] JEntry array  (N key entries, then N value entries)
  [4+8N..]      : data section  (key strings sorted bytewise, then value payloads)

ARRAY with N elements:
  [0..3]        : u32 container header  (bit31=1, bits30..0=N)
  [4..4+4N-1]   : [u32; N] JEntry array
  [4+4N..]      : data section  (element payloads in order)

Scalar (wrapper for a single scalar value):
  [0..3]        : u32 container header  (JENTRY_FSCALAR | 1)
  [4..7]        : u32 JEntry for the single element
  [8..]         : payload bytes

Key sorting invariant:
  Keys are stored in "length-first, then bytewise" order:
    "a" < "ab" < "aa" < "ba" (shorter strings come first)
  This enables a binary search with a cheap length-first comparator.
```

### Key Lookup Algorithm (with stride)

```rust
// Given: object with N keys, want to find value for key `needle`
//
// Step 1: binary search over key JEntry indices 0..N
//   comparator: (key_len, key_bytes) vs (needle.len, needle.bytes)
//
// Step 2: for each candidate `mid`, call element_offset(mid) to find the
//   start of that key's bytes in the data section.
//
// element_offset(i):
//   stride_base = (i / STRIDE) * STRIDE
//   base_offset = if stride_base == 0 { 0 }
//                 else { jentry_at(stride_base) & JENTRY_OFF_MASK }  // absolute
//   offset = base_offset
//   for j in stride_base..i {
//       offset += jentry_at(j) & JENTRY_OFF_MASK  // lengths are additive
//   }
//   return offset
//
// Step 3: once key found at index `k`, value lives at JEntry index N+k.
//   Call decode_element(N + k) to return JsonbValue.
```

### JsonbEncoder Algorithm

```rust
// Two-pass approach:
// Pass 1 — recursive DFS building output buffer:
//   For each object: sort keys bytewise-length-first, write header,
//   write 2N placeholder JEntries (all zeros), write key payloads,
//   write value payloads (recursing for nested containers).
//   Track lengths in a side-Vec<u32>.
//
// Pass 2 — stride fixup:
//   Walk the JEntry array for this container.
//   For every index i that is a multiple of STRIDE (and i > 0):
//     accumulated_offset = sum of lengths[0..i]
//     jentry[i] |= JENTRY_HAS_OFF
//     jentry[i] = (jentry[i] & !JENTRY_OFF_MASK) | accumulated_offset
//
// Use an explicit stack (Vec<Frame>) to avoid Rust stack overflow for deep docs.
// Depth limit: 256 levels. Return DbError::InvalidValue beyond that.
```

### JSONPath Enums

```rust
pub enum PathStep {
    Key(Arc<str>),                                   // .key
    Index(i64),                                      // [n] negative=from end
    AnyKey,                                          // .*
    AnyIndex,                                        // [*]
    Recursive(Arc<str>),                             // ..key
    RecursiveAny,                                    // ..*
    Filter(Box<FilterExpr>),                         // ?(...)
    Slice(Option<i64>, Option<i64>, Option<i64>),    // [from:to:step]
}

pub enum FilterExpr {
    Exists(Vec<PathStep>),
    Cmp(FilterPath, CmpOp, FilterLiteral),
    And(Box<FilterExpr>, Box<FilterExpr>),
    Or(Box<FilterExpr>, Box<FilterExpr>),
    Not(Box<FilterExpr>),
}

pub enum FilterPath {
    Current(Vec<PathStep>),  // @.something
    Root(Vec<PathStep>),     // $.something
}

pub enum CmpOp { Eq, Ne, Lt, Le, Gt, Ge, Like, StartsWith }

pub enum FilterLiteral {
    Null, Bool(bool), Int(i64), Float(f64), String(Arc<str>),
}

pub enum PathMode { Lax, Strict }
```

### JSONPath Lax Mode Auto-Unwrap Rule

```
When a PathStep::Key("x") is applied to an array,
lax mode applies the step to EACH element of the array,
collecting all matches (SQL:2016 §9.39).

When a PathStep::Index(n) is applied to a non-array scalar in lax mode,
and n == 0, return the scalar itself (singleton auto-wrap).
```

---

## Implementation phases

### Phase 1 — Core binary types (axiomdb-types)

Files: `jsonb.rs` (new), `value.rs`, `types.rs`, `lib.rs`, `codec.rs`

Deliverables:
- Layout constants and struct definitions (`JsonbBlob`, `JsonbRef`, `JsonbValue`)
- `JsonbEncoder::encode(&serde_json::Value) -> Vec<u8>` (iterative, depth-limited)
- `JsonbDecoder::decode(&[u8]) -> Result<serde_json::Value>` (for pretty-print/mutation)
- `JsonbDecoder::to_string(&[u8]) -> Result<String>` (canonical JSON text)
- `JsonbRef::get_key`, `JsonbRef::get_index`, `JsonbRef::element_offset` (stride)
- `Value::Jsonb(Arc<Vec<u8>>)` variant with `Display`, `PartialEq`, `variant_name`
- `DataType::Jsonb` with `name() → "JSONB"`
- `codec.rs` encode/decode arms for `DataType::Jsonb`

Test gate: `cargo test -p axiomdb-types` — all unit tests in `jsonb.rs` pass.

### Phase 2 — Catalog + DDL (axiomdb-catalog + axiomdb-sql parser)

Files: `schema_database.rs`, `lexer.rs`, `parser/ddl.rs`

Deliverables:
- `ColumnType::Jsonb = 10` with `TryFrom<u8>` and `From<ColumnType> for u8`
- `Token::TyJsonb` (keyword `JSONB`) — placed before any shorter matching token
- `Token::JsonExtractSub` (pattern `"->"`) — placed BEFORE `Token::Minus` in
  logos attribute list (CRITICAL: longer token must win)
- `parse_data_type` recognizes `JSONB` → `DataType::Jsonb`

Test gate: `cargo test -p axiomdb-catalog -p axiomdb-sql` — DDL round-trips.

### Phase 3 — Coercion (axiomdb-types)

Files: `coerce_api.rs`

Deliverables:
- `Text → Jsonb`: `serde_json::from_str` then `JsonbEncoder::encode`
- `Json → Jsonb`: `JsonbEncoder::encode` from parsed text
- `Jsonb → Text`: `JsonbDecoder::to_string`
- `Jsonb → Json`: decode to string, wrap as `Value::Json`
- All coercions return `DbError::InvalidValue` on malformed input

Test gate: `cargo test -p axiomdb-types` — coercion unit tests.

### Phase 4 — `->` operator (axiomdb-sql)

Files: `lexer.rs` (already updated in Phase 2), `parser/expr.rs`, `expr.rs`, `eval/ops.rs`

Deliverables:
- `BinaryOp::JsonSub` variant in `expr.rs`
- `parse_json_extract_sub` level in `expr.rs` between `parse_json_extract_text`
  and `parse_unary`; lowers `expr -> 'key'` → `Expr::BinaryOp(JsonSub)`,
  `expr -> integer` → same
- `eval/ops.rs`: `BinaryOp::JsonSub` evaluation:
  - String RHS → `JsonbRef::get_key` on Jsonb input, or parse-then-navigate on Json
  - Integer RHS → `JsonbRef::get_index`
  - Returns `Value::Jsonb` or `Value::Null`

Test gate: parser tests that `a->'b'` parses to `JsonSub` and `a->>'b'` still
lowers to `JSON_EXTRACT`. Eval test that `data->'name'` returns `Value::Jsonb`.

### Phase 5 — Basic new functions (axiomdb-sql)

Files: `eval/functions/json.rs`, `eval/functions/mod.rs`

Deliverables:
- `to_jsonb(value)` / `jsonb(text)` — coerce to `Value::Jsonb`
- `json_pretty(doc)` — `JsonbDecoder::decode` then `serde_json::to_string_pretty`
- `json_array_length(doc)` and `json_array_length(doc, path)` — reads array
  element count from container header directly (no decode)
- `json_depth(doc)` — recursive depth traversal via `JsonbRef`

Test gate: SQL integration tests for each function.

### Phase 6 — Upgrade existing functions to binary path

Files: `eval/functions/json.rs`

Deliverables:
- `json_extract`: if input is `Value::Jsonb`, navigate via `JsonbRef::get_key` /
  `get_index` for each path segment; skip `serde_json::from_str` entirely.
- `json_type`: read container header or JEntry type bits directly from binary.
- `json_keys`: iterate over key JEntries (indices 0..N) reading key strings.
- `json_valid`: `Value::Jsonb` is always valid by construction → return `Value::Int(1)`.
- `json_set`, `json_remove`: decode to `serde_json::Value`, mutate, re-encode as
  `Value::Jsonb`. (In-place mutation deferred to Phase 11.17.)

Test gate: run all Phase 11.4 integration tests (`integration_json.rs`) with
`JSONB` columns — must pass without modification.

### Phase 7 — JSON_MERGE_PATCH, JSON_CONTAINS, JSON_OVERLAPS

Files: `eval/functions/json.rs`

Deliverables:
- `json_merge_patch(doc, patch)` — RFC 7396:
  ```
  fn merge_patch(doc: &mut Value, patch: Value) {
    if patch is object {
      for (k, v) in patch {
        if v is null { doc.remove(k) }
        else { merge_patch(doc[k], v) }  // recurse if both objects, else replace
      }
    } else {
      *doc = patch  // non-object patch replaces entire document
    }
  }
  ```
  Decode both inputs to `serde_json::Value`, apply, re-encode to `Value::Jsonb`.
- `json_contains(doc, candidate [, path])` — structural subset check:
  - For objects: every key in `candidate` must exist in `doc` with a matching value.
  - For arrays: every element in `candidate` must appear in `doc`.
  - Scalar: equality check.
- `json_overlaps(doc1, doc2)` — any shared key (objects) or element (arrays).

Test gate: edge cases including null patch deletion, non-object patch replacement,
array subset containment, overlapping keys.

### Phase 8 — JSONPath compiler

Files: `eval/jsonpath.rs` (new)

Deliverables:
- `parse_jsonpath(input: &str) -> Result<Vec<PathStep>, DbError>`
- Supported syntax:
  - `$` → empty vec
  - `.key` or `."quoted key"` → `PathStep::Key`
  - `[n]` or `[-n]` → `PathStep::Index`
  - `.*` → `PathStep::AnyKey`
  - `[*]` → `PathStep::AnyIndex`
  - `..key` or `.."key"` → `PathStep::Recursive`
  - `..*` → `PathStep::RecursiveAny`
  - `[from:to]` or `[from:to:step]` → `PathStep::Slice`
  - `?(filter)` → `PathStep::Filter` with `FilterExpr`
  - Filter: `@.key op literal`, `exists(@.key)`, `&&`, `||`, `!`, `()`

Test gate: unit tests for every syntax variant, and error cases.

### Phase 9 — JSONPath executor + path-based functions

Files: `eval/jsonpath.rs`, `eval/functions/json.rs`, `eval/functions/mod.rs`

Deliverables:
- `execute_jsonpath(root_bytes: &[u8], path: &[PathStep], mode: PathMode) -> Vec<JsonbValue>`
  - Lax: auto-unwrap arrays on key access; auto-wrap scalar on `[0]`
  - Strict: type errors return empty vec (lax) or error (strict)
  - Recursive descent: DFS collecting all matching nodes
- `json_path_exists(doc, path)` → `Value::Bool`
- `json_path_query(doc, path)` → `Value::Jsonb` (wraps all matches in JSONB array)
- `json_path_query_first(doc, path)` → first match or `Value::Null`

Test gate: integration tests with filter predicates, recursive paths, lax/strict
comparison.

### Phase 10 — Wire layer + closing gates

Files: `result.rs`, `prepared.rs`, `axiomdb-embedded/src/lib.rs`

Deliverables:
- `Value::Jsonb` → `VAR_STRING` containing canonical JSON text on MySQL wire
- `integration_jsonb.rs`: full test suite covering every acceptance criterion
- `cargo test --workspace` passes
- `cargo clippy --workspace -- -D warnings` passes
- `cargo fmt --check` passes
- `tools/wire-test.py` updated with JSONB assertions

---

## Tests to write

### Unit tests in `crates/axiomdb-types/src/jsonb.rs`

- `test_encode_decode_null` — scalar null wrapper round-trips
- `test_encode_decode_bool_true` and `_false`
- `test_encode_decode_integer_positive`, `_negative`, `_large_i64`
- `test_encode_decode_float`
- `test_encode_decode_string_empty`, `_ascii`, `_utf8`
- `test_encode_decode_empty_object` — `{}`
- `test_encode_decode_flat_object` — 5 keys, verify bytewise-length-first sort order
- `test_encode_decode_nested_object` — two levels of nesting
- `test_encode_decode_empty_array` — `[]`
- `test_encode_decode_array_mixed` — null, bool, int, string elements
- `test_stride_boundary` — object with exactly 33 keys; verify that JEntry at
  index 32 has `JENTRY_HAS_OFF` set and `element_offset(32)` returns correct value
- `test_get_key_binary_search_1000_keys` — all 1000 keys found, missing key → None
- `test_get_key_length_first_ordering` — "a" and "aa" in correct sort order
- `test_get_index_positive`, `_negative`, `_out_of_bounds`

### Unit tests in `crates/axiomdb-sql/src/eval/jsonpath.rs`

- `test_parse_root` — `$` → empty
- `test_parse_key` — `$.name`
- `test_parse_chained_keys` — `$.a.b.c`
- `test_parse_index_positive`, `_negative`
- `test_parse_any_key`, `test_parse_any_index`
- `test_parse_recursive`, `test_parse_recursive_any`
- `test_parse_slice_full`, `_partial`, `_with_step`
- `test_parse_filter_exists`, `test_parse_filter_cmp`
- `test_parse_filter_and_or_not`
- `test_execute_simple_key`, `test_execute_nested_key`
- `test_execute_lax_auto_unwrap_array`
- `test_execute_strict_type_error`
- `test_execute_recursive_descent`
- `test_execute_filter_cmp`, `test_execute_filter_exists`

### Integration tests in `crates/axiomdb-sql/tests/integration_jsonb.rs`

- `test_create_table_jsonb` — catalog stores `ColumnType::Jsonb = 10`
- `test_insert_and_select_jsonb` — round-trip through row heap
- `test_arrow_operator_key` — `data->'name'` returns `Value::Jsonb`
- `test_arrow_operator_index` — `data->0` returns first array element
- `test_double_arrow_still_works` — `data->>'name'` still returns `Value::Text`
- `test_json_merge_patch_add_key`
- `test_json_merge_patch_delete_key` — null patch value removes key
- `test_json_merge_patch_replace_non_object`
- `test_json_contains_object_subset`
- `test_json_contains_array_subset`
- `test_json_contains_with_path`
- `test_json_overlaps_shared_key`
- `test_json_overlaps_no_overlap`
- `test_json_path_exists_true`, `_false`
- `test_json_path_query_multiple_results`
- `test_json_path_query_first_match`, `_no_match`
- `test_json_array_length_root`, `_with_path`
- `test_json_depth`
- `test_json_pretty`
- `test_to_jsonb_from_text`
- `test_existing_functions_on_jsonb_input` — all 6 Phase 11.4 functions work on `Value::Jsonb`
- `test_jsonb_toast` — document exceeding `TOAST_THRESHOLD` round-trips
- `test_json_columns_still_work` — Phase 11.4 `JSON` columns unchanged

---

## Anti-patterns to avoid

- **DO NOT parse back to `serde_json::Value` for read-only access.** `JSON_EXTRACT`,
  `JSON_TYPE`, `JSON_KEYS`, `->`, all `JSON_PATH_*`, `JSON_CONTAINS`,
  `JSON_OVERLAPS`, and `JSON_ARRAY_LENGTH` must navigate the binary blob via
  `JsonbRef` when the input is `Value::Jsonb`. Deserialising on every call negates
  the entire binary JSONB performance benefit.

- **DO NOT mutate JEntries in-place.** The binary layout is immutable once written.
  Mutation functions (`JSON_SET`, `JSON_REMOVE`, `JSON_MERGE_PATCH`) decode to
  `serde_json::Value`, mutate, and re-encode a fresh blob. In-place delta encoding
  is deferred to Phase 11.17.

- **DO NOT add `Token::JsonExtractSub` after `Token::Minus` in the logos attribute
  list.** Logos processes tokens in declaration order; `"->"` must appear before
  `"-"` to prevent the lexer emitting `Minus + Gt` instead of `JsonExtractSub`.

- **DO NOT use `unwrap()` in `JsonbRef` navigation.** All buffer access must go
  through checked slice indexing and propagate `DbError::ParseError`.

- **DO NOT break `DataType::Json` / `Value::Json`.** Rows from before Phase 11.16
  decode as `Value::Json(String)` unchanged. The codec `DataType::Json = 9` arm
  must remain identical to Phase 11.4. Only `DataType::Jsonb = 10` uses the new
  binary decoder.

- **DO NOT claim `ColumnType::Jsonb = 10` without updating `TryFrom<u8>`.** A
  catalog discriminant without a decode arm causes silent data corruption.

- **DO NOT store integer keys as binary.** All numeric values are stored as their
  text representation in the data section; only the JEntry type bits distinguish
  numeric from string.

---

## Risks

| Risk | Mitigation |
|---|---|
| Stride offset accumulation bug producing wrong field offsets | Property test: encode 100-key object, access every key by index, verify each offset = sum of preceding lengths |
| Key sort order diverges from what binary search expects | Unit test: verify encoded key section is in bytewise-length-first order before binary search is exercised |
| `->` lexer token shadows legitimate future `->`  syntax | Document token ordering constraint; logos longest-match handles it cleanly |
| Old `ColumnType::Json = 9` decoded as `DataType::Jsonb` | Codec discriminant 9 → text path; 10 → binary path; no cross-mapping possible |
| Large deeply-nested documents cause stack overflow in encoder | Encoder uses explicit `Vec<Frame>` stack; depth limit 256; return `DbError::InvalidValue` beyond |
| JSONPath filter expressions interact incorrectly with lax-mode array unwrapping | Integration tests with filter predicates applied to arrays in both lax and strict modes |
| TOAST path not exercised for JSONB blobs | Dedicated integration test with document large enough to exceed `TOAST_THRESHOLD` |
| `JSON_MERGE_PATCH` semantics diverge from RFC 7396 for non-object patches | Test case where patch is a string literal; must replace entire document |
