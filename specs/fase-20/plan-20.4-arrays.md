# Plan: 20.4 — PostgreSQL Array Parity

Phase: 20 — Types + import/export
Task: 20.4 Arrays
Spec: `specs/fase-20/spec-20.4-arrays.md`
Status: ready

## Summary

Deliver full PostgreSQL-compatible SQL arrays across 11 steps, each following
TDD and producing a compilable, test-passing commit. The plan adds
`DataType::Array(Box<DataType>)` and `Value::Array(Vec<Value>)` to the type
system, a varlena on-disk array blob format, DDL support (`TEXT[]`, `INT[][]`,
`FLOAT[3][3]`, `BOOL ARRAY`), the `ARRAY[...]` constructor, 6 operators + 17
functions, `ANY`/`ALL` constructs, `unnest()` as a FROM-clause table function,
GIN indexing for `@>`/`<@`/`&&`/`=`, `array_agg()` aggregate, and MySQL wire
protocol serialization as `{...}` text.  The design reuses Phase 11.17 GIN
infrastructure and follows the same catalog trailing-field pattern used by
Phase 20.3 ENUMs.

## Dependencies

Must be done first:

- [x] Phase 20.3 ENUMs closed — trailing-field pattern established.
- [x] Phase 11.17 GIN for JSONB — `gin_key_term`, `gin_term_bounds`,
  `gin_scan_rows`, `plan_gin_scan`, `index_type == 4` infrastructure.
- [x] Spec 20.4 approved.

Blocks:

- [ ] Phase 20.14 (UNNEST) — `unnest()` implemented here.
- [ ] Phase 20.18 (Composite types) — arrays of composites deferred.
- [ ] Phase 29.11 (`array_to_string` / `string_to_array`) — implemented here.

## Affected Files

New files:

- `crates/axiomdb-types/src/array_codec.rs` — on-disk array blob encode/decode.
- `crates/axiomdb-types/src/array_io.rs` — `array_to_text` / `text_to_array` text conversion.
- `crates/axiomdb-sql/src/eval/array_ops.rs` — array operator dispatch (`@>`, `<@`, `&&`, `||`, subscript).
- `crates/axiomdb-sql/src/eval/functions/array.rs` — 17 array functions.
- `crates/axiomdb-sql/src/eval/functions/any_all.rs` — `ANY` / `ALL` evaluator.
- `crates/axiomdb-sql/src/executor/agg_arrays.rs` — `array_agg` accumulator and finalizer.
- `crates/axiomdb-sql/tests/integration_arrays.rs` — end-to-end array tests (50+ tests).
- `crates/axiomdb-sql/tests/integration_array_operators.rs` — operator-specific tests.
- `crates/axiomdb-sql/tests/integration_array_functions.rs` — function-specific tests.
- `crates/axiomdb-sql/tests/integration_array_any_all.rs` — ANY/ALL tests.
- `crates/axiomdb-sql/tests/integration_array_unnest.rs` — unnest tests.
- `crates/axiomdb-sql/tests/integration_array_gin.rs` — GIN index tests.
- `crates/axiomdb-sql/tests/integration_array_agg.rs` — array_agg tests.

Modified files (by step):

| Step | Files |
|------|-------|
| 1 | `crates/axiomdb-types/src/types.rs`, `crates/axiomdb-types/src/value.rs`, `crates/axiomdb-catalog/src/schema_database.rs`, `crates/axiomdb-catalog/src/schema_table.rs`, `crates/axiomdb-types/src/lib.rs` |
| 2 | `crates/axiomdb-types/src/array_codec.rs` (new), `crates/axiomdb-types/src/array_io.rs` (new), `crates/axiomdb-types/src/codec.rs`, `crates/axiomdb-types/src/lib.rs` |
| 3 | `crates/axiomdb-sql/src/lexer.rs`, `crates/axiomdb-sql/src/ast.rs`, `crates/axiomdb-sql/src/parser/ddl.rs`, `crates/axiomdb-sql/src/parser/mod.rs`, `crates/axiomdb-sql/src/analyzer_stmt.rs`, `crates/axiomdb-sql/src/executor/ddl_create_table.rs`, `crates/axiomdb-sql/src/executor/ddl_show.rs`, `crates/axiomdb-sql/src/information_schema.rs` / `information_schema_exec.rs` |
| 4 | `crates/axiomdb-sql/src/ast.rs`, `crates/axiomdb-sql/src/expr.rs`, `crates/axiomdb-sql/src/parser/expr.rs`, `crates/axiomdb-sql/src/eval/core.rs` (or new `array_constructor.rs`), `crates/axiomdb-sql/src/coerce_*.rs` |
| 5 | `crates/axiomdb-sql/src/eval/array_ops.rs` (new), `crates/axiomdb-sql/src/eval/ops.rs`, `crates/axiomdb-sql/src/expr.rs` |
| 6 | `crates/axiomdb-sql/src/eval/functions/mod.rs`, `crates/axiomdb-sql/src/eval/functions/array.rs` (new) |
| 7 | `crates/axiomdb-sql/src/lexer.rs`, `crates/axiomdb-sql/src/ast.rs`, `crates/axiomdb-sql/src/expr.rs`, `crates/axiomdb-sql/src/parser/expr.rs`, `crates/axiomdb-sql/src/parser/dml.rs`, `crates/axiomdb-sql/src/eval/functions/any_all.rs` (new), `crates/axiomdb-sql/src/executor/select.rs`, `crates/axiomdb-sql/src/executor/select_core.rs`, `crates/axiomdb-sql/src/executor/select_ctx.rs`, `crates/axiomdb-sql/src/analyzer_stmt.rs` |
| 8 | `crates/axiomdb-sql/src/planner_select.rs` (`plan_gin_scan`), `crates/axiomdb-sql/src/index_maintenance.rs`, `crates/axiomdb-sql/src/executor/select_helpers.rs` (`gin_scan_rows`), `crates/axiomdb-sql/src/executor/ddl_create_index.rs` |
| 9 | `crates/axiomdb-sql/src/executor/agg_descriptor.rs`, `crates/axiomdb-sql/src/executor/agg_accum.rs`, `crates/axiomdb-sql/src/executor/agg_sorted.rs`, `crates/axiomdb-sql/src/executor/agg_arrays.rs` (new) |
| 10 | `crates/axiomdb-network/src/mysql/result.rs`, `crates/axiomdb-network/src/mysql/column.rs` |
| 11 | `specs/fase-20/plan-20.4-arrays.md` (this file), `docs/fase-20.md`, `docs/progreso.md`, `docs-site/src/user-guide/sql-reference/ddl.md`, `docs-site/src/user-guide/features/indexes.md`, `docs-site/src/internals/catalog.md`, `docs-site/src/internals/storage.md`, `docs-site/src/internals/btree.md`, `docs-site/src/internals/mvcc.md`, `memory/project_state.md`, `memory/architecture.md`, `tools/wire-test.py` |

---

## Step 1 — Type System + ColumnType + Catalog Extension

**Goal:** Make `DataType::Array`, `Value::Array`, `ColumnType::Array = 13`, and
the `array_element_type` trailing field on `ColumnDef` compilable and
round-trip-safe through the catalog.

**TDD test:**

```rust
// crates/axiomdb-types/tests/array_types.rs  (or inline in types.rs test module)

#[test]
fn datatype_array_variant_exists() {
    let dt = DataType::Array(Box::new(DataType::Int));
    assert_eq!(dt.name(), "INT[]");
    // nested
    let dt2 = DataType::Array(Box::new(DataType::Array(Box::new(DataType::Text))));
    assert_eq!(dt2.name(), "TEXT[][]");
}

#[test]
fn value_array_variant_roundtrips() {
    let arr = Value::Array(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
    ]);
    assert_eq!(arr.variant_name(), "Array");
    match &arr {
        Value::Array(elems) => assert_eq!(elems.len(), 3),
        _ => panic!("expected Array"),
    }
}

#[test]
fn columndef_array_element_type_roundtrips() {
    let cd = ColumnDef {
        array_element_type: Some(ColumnType::Int),
        array_ndims: Some(1),
        ..default_coldef()
    };
    let bytes = cd.to_bytes();
    let (cd2, _n) = ColumnDef::from_bytes(&bytes).unwrap();
    assert_eq!(cd2.array_element_type, Some(ColumnType::Int));
    assert_eq!(cd2.array_ndims, Some(1));
}

#[test]
fn columndef_legacy_no_array_field() {
    // A legacy row without the array trailing field should decode as None,None.
    let mut cd = default_coldef();
    cd.array_element_type = None;
    cd.array_ndims = None;
    let bytes = cd.to_bytes();
    let (cd2, _n) = ColumnDef::from_bytes(&bytes).unwrap();
    assert_eq!(cd2.array_element_type, None);
    assert_eq!(cd2.array_ndims, None);
}
```

**Implementation outline:**

1. **`crates/axiomdb-types/src/types.rs`**
   - Add `Array(Box<DataType>)` variant to `DataType`.
   - Update `name()` to display `"TYPE[]"` for 1D arrays, `"TYPE[][]"` etc.
   - Update `Display` impl.
   - Update every `match` arm in the crate and dependents that exhaustively
     matches `DataType`.

2. **`crates/axiomdb-types/src/value.rs`**
   - Add `Array(Vec<Value>)` variant to `Value`.
   - Update `variant_name()`.
   - Update `Display` to show `{elem1,elem2,...}` (PG text format).
   - Update every exhaustive `match` in the crate.

3. **`crates/axiomdb-catalog/src/schema_database.rs`**
   - Add `ColumnType::Array = 13`.
   - Update `TryFrom<u8>` with `13 => Ok(Self::Array)`.
   - Update `From<ColumnType> for u8` (automatic with `as u8`).

4. **`crates/axiomdb-catalog/src/schema_table.rs`**
   - Add two fields to `ColumnDef`:
     - `pub array_element_type: Option<ColumnType>` — the leaf element type.
     - `pub array_ndims: Option<u8>` — number of dimensions (1-6).
   - Extend `to_bytes()`: after `enum_type_name` bytes, write:
     - `[array_ndims: u8]` (0 if None, else ndims).
     - If `array_ndims > 0`: `[array_element_type: u8]` (ColumnType discriminant).
   - Extend `from_bytes()`: after `enum_type_name` parsing, consume these
     trailing bytes. If no bytes remain, default to `None, None`.
   - Update the constructor sites in `bootstrap.rs` and `schema.rs` (all
     existing `ColumnDef` literals gain `..` default or
     `array_element_type: None, array_ndims: None`).

5. **`crates/axiomdb-types/src/lib.rs`**
   - Re-export `DataType::Array`, `Value::Array`.

**Verification:**

```bash
cargo test -p axiomdb-types --test array_types       # or inline tests
cargo test -p axiomdb-catalog columndef_array
cargo test -p axiomdb-types                      # ensure no match exhaustion
cargo test -p axiomdb-catalog                    # full catalog tests
```

**Commit message:**

```
feat(fase-20): add DataType::Array, Value::Array, ColumnType::Array and catalog extension

- DataType::Array(Box<DataType>) with recursive name display (INT[], TEXT[][])
- Value::Array(Vec<Value>) with PG-compatible text Display
- ColumnType::Array = 13 in schema_database.rs
- ColumnDef.array_element_type and array_ndims trailing fields (backward-compatible)
- Legacy catalog rows decode as None/None

Phase 20/34. Step 1/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 2 — Array Binary Codec + Text I/O

**Goal:** Encode/decode the varlena array blob format and convert between
binary and PG-compatible `{...}` text.

**TDD test:**

```rust
// crates/axiomdb-types/tests/array_codec.rs

#[test]
fn encode_decode_1d_int_array_empty() {
    let arr = Value::Array(vec![]);
    let blob = encode_array(&arr, ColumnType::Int).unwrap(); // ndim=0
    let (decoded, _, _) = decode_array(&blob).unwrap();
    assert_eq!(decoded, arr);
}

#[test]
fn encode_decode_1d_int_array_three_elems() {
    let arr = Value::Array(vec![Value::Int(10), Value::Int(20), Value::Int(30)]);
    let blob = encode_array(&arr, ColumnType::Int).unwrap();
    let (decoded, elem_type, ndim) = decode_array(&blob).unwrap();
    assert_eq!(ndim, 1);
    assert_eq!(elem_type, ColumnType::Int);
    assert_eq!(decoded, arr);
}

#[test]
fn encode_decode_1d_text_array() {
    let arr = Value::Array(vec![
        Value::Text("hello".into()),
        Value::Text("world".into()),
    ]);
    let blob = encode_array(&arr, ColumnType::Text).unwrap();
    let (decoded, _, _) = decode_array(&blob).unwrap();
    assert_eq!(decoded, arr);
}

#[test]
fn encode_decode_2d_int_array() {
    // 2×3 = {{1,2,3},{4,5,6}}
    let arr = Value::Array(vec![
        Value::Int(1), Value::Int(2), Value::Int(3),
        Value::Int(4), Value::Int(5), Value::Int(6),
    ]);
    // We need a way to encode with ndim=2,dims=[2,3].
    // Provide encode_array_with_dims(&arr, elem_type, ndim, dims).
    let blob = encode_array_nd(&arr, ColumnType::Int, 2, &[2, 3]).unwrap();
    let (decoded, elem_type, ndim) = decode_array(&blob).unwrap();
    assert_eq!(ndim, 2);
    assert_eq!(elem_type, ColumnType::Int);
    assert_eq!(decoded, Value::Array(vec![
        Value::Int(1), Value::Int(2), Value::Int(3),
        Value::Int(4), Value::Int(5), Value::Int(6),
    ]));
}

#[test]
fn encode_decode_null_elements() {
    let arr = Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]);
    let blob = encode_array(&arr, ColumnType::Int).unwrap();
    let (decoded, _, _) = decode_array(&blob).unwrap();
    assert_eq!(decoded, arr); // Value::Null preserved in element position
}

#[test]
fn array_to_text_roundtrip_int() {
    let blob = encode_array_nd(
        &Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        ColumnType::Int, 1, &[3],
    ).unwrap();
    let text = array_to_text(&blob).unwrap();
    assert_eq!(text, "{1,2,3}");
    let decoded_blob = text_to_array(&text, ColumnType::Int).unwrap();
    let (decoded, _, _) = decode_array(&decoded_blob).unwrap();
    assert_eq!(decoded, Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
}

#[test]
fn array_to_text_2d() {
    let blob = encode_array_nd(
        &Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3),
                           Value::Int(4), Value::Int(5), Value::Int(6)]),
        ColumnType::Int, 2, &[2, 3],
    ).unwrap();
    let text = array_to_text(&blob).unwrap();
    assert_eq!(text, "{{1,2,3},{4,5,6}}");
}

#[test]
fn array_to_text_null_elements() {
    let blob = encode_array(
        &Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]),
        ColumnType::Int,
    ).unwrap();
    let text = array_to_text(&blob).unwrap();
    assert_eq!(text, "{1,NULL,3}");
}

#[test]
fn array_to_text_empty() {
    let blob = encode_array(&Value::Array(vec![]), ColumnType::Int).unwrap();
    let text = array_to_text(&blob).unwrap();
    assert_eq!(text, "{}");
}

#[test]
fn text_to_array_invalid_unclosed() {
    let result = text_to_array("{1,2", ColumnType::Int);
    assert!(result.is_err());
}

#[test]
fn text_to_array_type_mismatch() {
    let result = text_to_array("{1,two,3}", ColumnType::Int);
    assert!(result.is_err());
}
```

**Implementation outline:**

1. **`crates/axiomdb-types/src/array_codec.rs`** (new file)
   - `pub fn encode_array(value: &Value, elem_type: ColumnType) -> Result<Vec<u8>, DbError>`
     — infers ndim from flat value (1D, dims=[len]).
   - `pub fn encode_array_nd(value: &Value, elem_type: ColumnType, ndim: i32, dims: &[i32]) -> Result<Vec<u8>, DbError>`
     — full control for multidimensional.
   - `pub fn decode_array(blob: &[u8]) -> Result<(Value, ColumnType, i32), DbError>`
     — returns decoded value, element ColumnType, ndim.
   - Internal helpers for fixed-size element encode/decode, variable-size
     element encode/decode, null bitmap read/write.
   - Follow the format from spec 20.4 lines 107-131:
     `[total_len:u32][ndim:i32][dataoffset:i32][elemtype:u8][flags:u8][pad:u16][dims[]][lbound[]][null_bitmap?][elements]`
   - Validation: ndim 0-6, total_len matches, dims product ≤ 2^31-1.

2. **`crates/axiomdb-types/src/array_io.rs`** (new file)
   - `pub fn array_to_text(blob: &[u8]) -> Result<String, DbError>` — produces PG-compatible `{...}` text.
     - Empty array (ndim=0) → `{}`.
     - 1D → `{elem1,elem2,...}` with quoting rules.
     - nD → `{inner1,inner2,...}` recurse per outer dim.
     - NULL elements → literal `NULL` (unquoted).
     - Text elements with `{`, `}`, `,`, `"`, `\` → double-quoted + backslash escaped.
     - String `"NULL"` → double-quoted to avoid ambiguity.
   - `pub fn text_to_array(text: &str, elem_type: ColumnType) -> Result<Vec<u8>, DbError>` — parse `{...}` → blob.
     - State machine parser handling quoted strings, NULL literals, nested `{`.
     - Validate bracket nesting, element count compatible with ndim inference.

3. **`crates/axiomdb-types/src/codec.rs`**
   - Handle `Value::Array` in `encode_row` / `decode_row`: store/skip the
     raw varlena blob alongside existing TEXT/BYTES/JSONB logic.
   - Call `encode_array` before storing, `decode_array` when reading.

4. **`crates/axiomdb-types/src/lib.rs`**
   - Add `pub mod array_codec;` and `pub mod array_io;`.
   - Re-export `encode_array`, `decode_array`, `array_to_text`, `text_to_array`.

**Verification:**

```bash
cargo test -p axiomdb-types --test array_codec
cargo test -p axiomdb-types codec array         # row-codec array path
cargo test -p axiomdb-types                     # full types crate
```

**Commit message:**

```
feat(fase-20): implement array binary codec and PG text I/O

- New encode_array / encode_array_nd / decode_array in array_codec.rs
- Varlena blob format: u32 total_len, i32 ndim, null bitmap, element packing
- Fixed-size (Bool/Int/BigInt/Real/Decimal/Date/Timestamp/Uuid) and
  variable-size (Text/Bytes/Json/Jsonb) element encoding
- New array_to_text / text_to_array in array_io.rs
- PG-compatible {…} text format with quoting, escaping, NULL literals
- Row codec integration for Value::Array

Phase 20/34. Step 2/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 3 — DDL Parser + Catalog Write Path

**Goal:** Parse `TEXT[]`, `INT[][]`, `FLOAT[3][3]`, `BOOL ARRAY` in
`CREATE TABLE` / `ALTER TABLE` and persist array metadata through the catalog.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_ddl_parser.rs  (add array section)

#[test]
fn parse_create_table_with_1d_int_array() {
    // CREATE TABLE t (vals INT[])
}

#[test]
fn parse_create_table_with_2d_int_array() {
    // CREATE TABLE t (matrix INT[][])
}

#[test]
fn parse_create_table_with_text_array() {
    // CREATE TABLE t (tags TEXT[])
}

#[test]
fn parse_create_table_with_bool_array_keyword() {
    // CREATE TABLE t (flags BOOL ARRAY)
}

#[test]
fn parse_create_table_with_fixed_size_hints() {
    // CREATE TABLE t (m FLOAT[3][3]) — size hints parsed but unenforced (PG compat)
}

#[test]
fn create_table_array_column_roundtrips() {
    // CREATE TABLE t (tags TEXT[], scores INT[][]);
    // Verify in SHOW CREATE TABLE and information_schema.COLUMNS
}

#[test]
fn show_create_table_displays_array_notation() {
    // CREATE TABLE t (a TEXT[]);
    // SHOW CREATE TABLE t → contains "a TEXT[]"
}
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/lexer.rs`**
   - Add `#[token("ARRAY", ignore(ascii_case))]` → `Token::Array`.

2. **`crates/axiomdb-sql/src/ast.rs`**
   - Add `Array` to `ColumnConstraint` or create an `ArrayType` descriptor
     carried by `ColumnDef`: `pub array_dims: Option<Vec<Option<u16>>>` —
     `None` = not array; `Some(vec![None])` = 1D unspecified; 
     `Some(vec![Some(3), Some(3)])` = `[3][3]`.
   - Or follow the simpler path: extend `ColumnDef` with
     `pub is_array: bool`, `pub array_ndims: u8`, `pub array_size_hints: Vec<Option<u16>>`.
     Keep `data_type` as the leaf scalar type.

3. **`crates/axiomdb-sql/src/parser/ddl.rs`** — `parse_data_type()`
   - After parsing base type identity, loop: while `Token::LBracket` → consume
     `[size?]` → increment ndims counter, store optional size hint.
   - After the loop, also accept `Token::Array` keyword (PG-compatible suffix).
   - Return: data type of the leaf scalar, ndims, size hints.
   - Update the return type from `(DataType, u16, bool)` to a richer struct
     or add ndims/size_hints fields.

4. **`crates/axiomdb-sql/src/parser/mod.rs`**
   - Update `parse_column_def` to propagate array metadata into `ColumnDef`.

5. **`crates/axiomdb-sql/src/executor/ddl_create_table.rs`**
   - When creating a table with an array column:
     - Compute `col_type = ColumnType::Array`.
     - Set `array_element_type = Some(leaf_scalar_column_type)`.
     - Set `array_ndims = Some(ndims)` (1-6).
     - Store size hints in a new optional field or discard (PG ignores them).
   - Validate ndims ≤ 6 at DDL time.

6. **`crates/axiomdb-sql/src/executor/ddl_show.rs`**
   - `SHOW CREATE TABLE`: for array columns, append `[size?]` brackets.
     - `TEXT[3]` for 1D with size hint, `TEXT[]` for 1D unbounded.
     - `INT[][][]` for 3D unbounded.
     - `INT[][]` for 2D unbounded.

7. **`crates/axiomdb-sql/src/information_schema.rs`** / **`information_schema_exec.rs`**
   - Report array columns: `DATA_TYPE = 'ARRAY'`, `ELEMENT_TYPE` column shows the
     leaf scalar type name.

8. **`crates/axiomdb-sql/src/analyzer_stmt.rs`**
   - When resolving array column types for the executor, convert
     `ColumnType::Array` + `array_element_type` → `DataType::Array(Box::new(leaf_type))`.
   - Update `column_data_types()` or equivalent to produce `DataType::Array(...)`.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser array
cargo test -p axiomdb-sql --test integration_arrays ddl
cargo test -p axiomdb-catalog columndef_array
```

**Commit message:**

```
feat(fase-20): DDL parser and catalog write path for array columns

- parse_data_type() handles [] brackets in a loop (TEXT[], INT[][], FLOAT[3][3])
- BOOL ARRAY keyword suffix accepted (PG-compatible)
- ColumnDef.array_element_type and array_ndims persisted through catalog
- SHOW CREATE TABLE reconstructs array notation (TEXT[], INT[][])
- information_schema.COLUMNS reports DATA_TYPE=ARRAY with element info
- ndims validation (1-6) at DDL time

Phase 20/34. Step 3/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 4 — ARRAY Constructor + Coercion

**Goal:** Parse and evaluate `ARRAY[e1, e2, ...]` with type inference and
coercion, including nested `ARRAY[ARRAY[1,2], ARRAY[3,4]]` and
`ARRAY[]::int[]`.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_arrays.rs

#[test]
fn array_constructor_int() {
    // SELECT ARRAY[1, 2, 3] → {1,2,3}
}

#[test]
fn array_constructor_text() {
    // SELECT ARRAY['a', 'b', 'c'] → {a,b,c}
}

#[test]
fn array_constructor_nested() {
    // SELECT ARRAY[ARRAY[1,2], ARRAY[3,4]] → {{1,2},{3,4}}
}

#[test]
fn array_constructor_empty_with_cast() {
    // SELECT ARRAY[]::int[] → {}
}

#[test]
fn array_constructor_empty_no_cast_error() {
    // SELECT ARRAY[] → error (cannot determine type)
}

#[test]
fn array_constructor_coerces_types() {
    // SELECT ARRAY[1, 2.5] → {1,2.5} (real[])
}

#[test]
fn array_constructor_mixed_types_error() {
    // SELECT ARRAY[1, 'text'] → type mismatch error
}

#[test]
fn array_constructor_mismatched_dimensions_error() {
    // SELECT ARRAY[ARRAY[1,2], ARRAY[3,4,5]] → error
}

#[test]
fn insert_array_value() {
    // CREATE TABLE t (vals INT[]); INSERT INTO t VALUES (ARRAY[1,2,3]);
    // SELECT * FROM t → row with {1,2,3}
}
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/ast.rs`**
   - In `ColumnDef`, ensure the array metadata fields from Step 3 are present.

2. **`crates/axiomdb-sql/src/expr.rs`**
   - Add `Expr::ArrayConstructor { elements: Vec<Expr> }`.

3. **`crates/axiomdb-sql/src/parser/expr.rs`** (or appropriate parser file)
   - Add `parse_array_constructor(p)` dispatch: when `Token::KwArray` (existing
     `ARRAY` keyword) is followed by `Token::LBracket`.
   - Parse comma-separated elements until `Token::RBracket`.
   - Support nested `ARRAY[ARRAY[...], ...]`.

4. **`crates/axiomdb-sql/src/eval/core.rs`** (or new `eval/array_constructor.rs`)
   - `fn eval_array_constructor(elements: &[Expr], row: &[Value], session_ctx: ...) -> Result<Value, DbError>`
   - Evaluate each element.
   - Infer common type using existing coercion matrix:
     - All same type → use that type.
     - Mixed numeric → widen to common (int→real, int→decimal, etc.).
     - Mixed text+json → text.
   - Empty array: require explicit cast (check for `Expr::Cast` wrapper or
     propagate an error).
   - Build `Value::Array(vec![...])`.
   - Nested: each inner `ARRAY[...]` produces its own `Value::Array`, then
     validate all inner arrays have same ndim and compatible dims.
   - For row-codec storage, call `encode_array` / `encode_array_nd`.

5. **`crates/axiomdb-sql/src/eval/ops.rs`** (or eval dispatch)
   - Wire `Expr::ArrayConstructor` into `eval()`.

6. **`crates/axiomdb-types/src/coerce.rs`** (or `coerce_helpers.rs`)
   - Ensure `coerce_for_op` / `coerce_to_type` handles `DataType::Array(_)`:
     two arrays with same element type are compatible. Mixed-element arrays use
     the element coercion matrix.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_arrays constructor
cargo test -p axiomdb-sql --test integration_arrays insert
```

**Commit message:**

```
feat(fase-20): ARRAY[...] constructor with type inference and coercion

- Expr::ArrayConstructor parsed from ARRAY[expr, ...] syntax
- Nested ARRAY[ARRAY[...], ...] supported
- Type inference via existing coercion matrix (int→real widening, etc.)
- Empty ARRAY[] requires explicit ::type[] cast
- Value::Array stored via encode_array through row codec on INSERT
- Mismatched dimensions and type errors produce clear DbError messages

Phase 20/34. Step 4/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 5 — Operators

**Goal:** Implement subscript `arr[n]` / `arr[n:m]`, equality `=` / inequality
`<>`, contains `@>`, contained by `<@`, overlap `&&`, concatenation `||`, and
polymorphic dispatch for the four operators shared with JSONB.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_array_operators.rs

#[test]
fn subscript_1d_first_element() {
    // SELECT (ARRAY[10,20,30])[1] → 10
}

#[test]
fn subscript_1d_out_of_bounds_null() {
    // SELECT (ARRAY[10,20,30])[5] → NULL
}

#[test]
fn subscript_1d_slice() {
    // SELECT (ARRAY[10,20,30,40])[2:3] → {20,30}
}

#[test]
fn subscript_2d() {
    // SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])[2][1] → 3
}

#[test]
fn subscript_negative_index_null() {
    // SELECT (ARRAY[1,2,3])[-1] → NULL
}

#[test]
fn equality_same_arrays() {
    // SELECT ARRAY[1,2,3] = ARRAY[1,2,3] → TRUE
}

#[test]
fn equality_different_arrays() {
    // SELECT ARRAY[1,2,3] = ARRAY[1,2,4] → FALSE
}

#[test]
fn equality_null_elements_unknown() {
    // SELECT ARRAY[1,NULL] = ARRAY[1,NULL] → NULL (UNKNOWN)
}

#[test]
fn contains_atat() {
    // SELECT ARRAY[1,2,3] @> ARRAY[1,2] → TRUE
}

#[test]
fn contains_not_subset() {
    // SELECT ARRAY[1,2] @> ARRAY[1,3] → FALSE
}

#[test]
fn contains_null_in_query_unknown() {
    // SELECT ARRAY[1,2] @> ARRAY[NULL] → NULL
}

#[test]
fn contained_by_ltat() {
    // SELECT ARRAY[1,2] <@ ARRAY[1,2,3] → TRUE
}

#[test]
fn overlap_andand() {
    // SELECT ARRAY[1,2,3] && ARRAY[3,4,5] → TRUE
}

#[test]
fn overlap_disjoint() {
    // SELECT ARRAY[1,2] && ARRAY[3,4] → FALSE
}

#[test]
fn concatenation_pipepipe() {
    // SELECT ARRAY[1,2] || ARRAY[3,4] → {1,2,3,4}
}

#[test]
fn concat_element_to_array() {
    // SELECT ARRAY[1,2] || 3 → {1,2,3}
}

#[test]
fn jsonb_atat_still_works() {
    // SELECT '{"a":1}'::jsonb @> '{"a":1}'::jsonb → TRUE
    // (polymorphic dispatch: JSONB path unchanged)
}

#[test]
fn array_dimensional_equality() {
    // SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] = ARRAY[ARRAY[1,2],ARRAY[3,4]]
}
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/eval/array_ops.rs`** (new file)
   - `fn array_subscript(arr: &Value, index: i64) -> Result<Value, DbError>`
     — 1-indexed, NULL on out of bounds / negative.
   - `fn array_slice(arr: &Value, lo: i64, hi: i64) -> Result<Value, DbError>`
     — returns sub-array.
   - `fn array_subscript_2d(arr: &Value, i: i64, j: i64) -> Result<Value, DbError>`
     — multidimensional.
   - `fn array_equals(a: &Value, b: &Value) -> Result<Value, DbError>`
     — element-by-element; NULL element pair → Value::Null.
   - `fn array_contains(subject: &Value, query: &Value) -> Result<Value, DbError>`
     — flat set membership. NULL in query → Value::Null. NULL in subject
       treated as element.
   - `fn array_overlap(a: &Value, b: &Value) -> Result<Value, DbError>`
     — any shared element.
   - `fn array_concat(a: &Value, b: &Value) -> Result<Value, DbError>`
     — 1D+1D concatenation. Element+array promoted to 1-element array.
   - `fn array_concat_element_to_array(arr: &Value, elem: &Value) -> Result<Value, DbError>`.

2. **`crates/axiomdb-sql/src/eval/ops.rs`** — Polymorphic dispatch
   - In `eval_binary()` or equivalent, for `BinaryOp::JsonContains` (`@>`),
     `BinaryOp::JsonContainedBy` (`<@`), `BinaryOp::JsonOverlap` (`&&`),
     `BinaryOp::Concat` (`||`):
     - If LHS is `Value::Array` → dispatch to `array_ops::array_*`.
     - If LHS is `Value::Jsonb` / `Value::Json` → existing JSONB path.
     - Otherwise → `TypeMismatch` error.
   - Add `BinaryOp::ArraySubscript` or reuse `BinaryOp::Subscript` for `arr[n]`.
   - Add `BinaryOp::Equals` + `BinaryOp::NotEquals` dispatch for arrays.
   - Add `BinaryOp::Concat` dispatch for array+element and array+array.

3. **`crates/axiomdb-sql/src/parser/expr.rs`**
   - Parse `arr[index]` with `Token::LBracket` after an expression: this is the
     subscript operator. Distinguish from `ARRAY[...]` constructor by checking
     whether `ARRAY` keyword preceded the bracket.
   - Parse slice `arr[lo:hi]` with colon token.

4. **`crates/axiomdb-sql/src/expr.rs`**
   - Ensure existing `BinaryOp` variants for `@>`, `<@`, `&&`, `||`, `=`, `<>`
     are routed correctly.
   - Add `Expr::Subscript { array: Box<Expr>, index: Box<Expr> }` or reuse
     existing patterns.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_array_operators
cargo test -p axiomdb-sql --test integration_jsonb       # polymorphic dispatch
```

**Commit message:**

```
feat(fase-20): array operators — subscript, =, <>, @>, <@, &&, ||

- Subscript arr[n] (1-indexed) and arr[lo:hi] slice with NULL-on-OOB
- Equality (=) and inequality (<>) element-by-element with NULL propagation
- Contains (@>), contained-by (<@), overlap (&&) with PG NULL semantics
- Concatenation (||) for array+array and element+array
- Polymorphic dispatch: @>/<@/&&/|| route to array ops for Value::Array,
  JSONB ops for Value::Jsonb — existing JSONB behavior unchanged
- All operators in new eval/array_ops.rs module

Phase 20/34. Step 5/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 6 — Functions

**Goal:** Implement all 17 array functions with element-type polymorphism.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_array_functions.rs

#[test]
fn array_length_1d()         { /* array_length(ARRAY[1,2,3], 1) → 3 */ }
#[test]
fn array_length_nonexistent_dim() { /* array_length(ARRAY[1,2,3], 2) → NULL */ }
#[test]
fn array_lower()             { /* array_lower(ARRAY[1,2,3], 1) → 1 */ }
#[test]
fn array_upper()             { /* array_upper(ARRAY[1,2,3], 1) → 3 */ }
#[test]
fn array_ndims_1d()          { /* array_ndims(ARRAY[1,2,3]) → 1 */ }
#[test]
fn array_ndims_empty()       { /* array_ndims(ARRAY[]::int[]) → 0 */ }
#[test]
fn array_dims()              { /* array_dims(ARRAY[1,2,3]) → '[1:3]' */ }
#[test]
fn array_dims_2d()           { /* array_dims(ARRAY[ARRAY[1,2],ARRAY[3,4]]) → '[1:2][1:2]' */ }
#[test]
fn cardinality_1d()          { /* cardinality(ARRAY[1,2,3]) → 3 */ }
#[test]
fn cardinality_2d()          { /* cardinality(ARRAY[ARRAY[1,2],ARRAY[3,4]]) → 4 */ }
#[test]
fn array_append()            { /* array_append(ARRAY[1,2], 3) → {1,2,3} */ }
#[test]
fn array_prepend()           { /* array_prepend(0, ARRAY[1,2]) → {0,1,2} */ }
#[test]
fn array_cat()               { /* array_cat(ARRAY[1,2], ARRAY[3,4]) → {1,2,3,4} */ }
#[test]
fn array_remove()            { /* array_remove(ARRAY[1,2,3,2], 2) → {1,3} */ }
#[test]
fn array_remove_nulls()      { /* array_remove(ARRAY[1,NULL,3,NULL], NULL) → {1,3} */ }
#[test]
fn array_replace()           { /* array_replace(ARRAY[1,2,3], 2, 5) → {1,5,3} */ }
#[test]
fn array_position_found()    { /* array_position(ARRAY[10,20,30], 20) → 2 */ }
#[test]
fn array_position_not_found() { /* array_position(ARRAY[1,2,3], 5) → 0 */ }
#[test]
fn array_position_start()    { /* array_position(ARRAY[1,2,3,2], 2, 3) → 4 */ }
#[test]
fn array_to_string_simple()  { /* array_to_string(ARRAY[1,2,3], ',') → '1,2,3' */ }
#[test]
fn array_to_string_null_skip() { /* array_to_string(ARRAY[1,NULL,3], ',') → '1,3' */ }
#[test]
fn array_to_string_null_replace() {
    // array_to_string(ARRAY[1,NULL,3], ',', 'X') → '1,X,3'
}
#[test]
fn string_to_array_simple()  { /* string_to_array('a,b,c', ',') → {a,b,c} */ }
#[test]
fn string_to_array_null_str() {
    // string_to_array('a,X,c', ',', 'X') → {a,NULL,c}
}
#[test]
fn unnest_1d()               { /* (covered properly in Step 7, smoke here) */ }
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/eval/functions/array.rs`** (new file)
   - `pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError>`
   - Function-by-function implementation (polymorphic on element type):
     - **array_length(arr, dim)**: decode array, read `dims[dim-1]`, NULL if dim > ndim.
     - **array_lower(arr, dim)**: decode array, read `lbound[dim-1]`.
     - **array_upper(arr, dim)**: `lbound[dim-1] + dims[dim-1] - 1`.
     - **array_ndims(arr)**: decode array, return `ndim`.
     - **array_dims(arr)**: decode array, format `[lb:ub]` per dimension.
     - **cardinality(arr)**: decode array, return product of `dims`.
     - **array_append(arr, elem)**: decode 1D array, push element, re-encode.
     - **array_prepend(elem, arr)**: decode 1D array, insert at front, re-encode.
     - **array_cat(arr1, arr2)**: decode both 1D, concatenate, re-encode.
     - **array_remove(arr, elem)**: decode 1D, filter out matching elements.
     - **array_replace(arr, old, new)**: decode 1D, replace matching elements.
     - **array_position(arr, elem)** / **array_position(arr, elem, start)**:
       linear scan for match.
     - **array_to_string(arr, delim)** / **array_to_string(arr, delim, null_str)**:
       decode, join non-null elements with delimiter, replace nulls if 3-arg.
     - **string_to_array(str, delim)** / **string_to_array(str, delim, null_str)**:
       split string by delimiter, build `text[]` array blob.
   - For functions that modify arrays: after mutation, re-encode via `encode_array`.

2. **`crates/axiomdb-sql/src/eval/functions/mod.rs`**
   - Add dispatch entries:
     ```rust
     "array_length" | "array_lower" | "array_upper" | "array_ndims"
     | "array_dims" | "cardinality" | "unnest" | "array_append"
     | "array_prepend" | "array_cat" | "array_remove" | "array_replace"
     | "array_position" | "array_to_string" | "string_to_array"
     => array::eval(lower.as_str(), args, row),
     ```
   - `unnest` is forwarded here for now but will be fully handled in Step 7 via
     the FROM-clause SRF path. A simple scalar-eval path returns the array value
     for compatibility with early tests.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_array_functions
cargo test -p axiomdb-sql eval functions array
```

**Commit message:**

```
feat(fase-20): 17 array functions — length, dims, cardinality, append, prepend, cat, remove, replace, position, to_string, string_to_array

- array_length, array_lower, array_upper, array_ndims, array_dims (metadata)
- cardinality (total elements)
- array_append, array_prepend, array_cat (mutation)
- array_remove, array_replace (filtering)
- array_position with optional start index
- array_to_string with optional null string replacement
- string_to_array with optional null string marker
- All functions polymorphic (any scalar element type)
- 3-arg overloads registered via function dispatch in eval/functions/mod.rs

Phase 20/34. Step 6/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 7 — ANY/ALL Constructs + unnest() Set-Returning Function

**Goal:** Parse and evaluate `expr = ANY(array_expr)` / `expr > ALL(array_expr)`,
and implement `unnest(arr)` as a FROM-clause table function with multi-array
zip support.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_array_any_all.rs

#[test]
fn any_equals_true()       { /* SELECT 100 = ANY(ARRAY[50,100,200]) → TRUE */ }
#[test]
fn any_equals_false()      { /* SELECT 100 = ANY(ARRAY[50,150]) → FALSE */ }
#[test]
fn any_greater_than()      { /* SELECT 100 > ANY(ARRAY[50,150]) → TRUE */ }
#[test]
fn any_with_null()         { /* SELECT 100 = ANY(ARRAY[NULL,200]) → TRUE */ }
#[test]
fn any_all_nulls()         { /* SELECT 100 = ANY(ARRAY[NULL,NULL]) → NULL */ }
#[test]
fn all_less_than_true()    { /* SELECT 100 < ALL(ARRAY[200,300]) → TRUE */ }
#[test]
fn all_less_than_false()   { /* SELECT 100 < ALL(ARRAY[50,150]) → FALSE */ }
#[test]
fn all_with_null_false()   { /* SELECT 100 < ALL(ARRAY[200,NULL]) → NULL */ }
#[test]
fn any_like()              { /* SELECT 'foo' LIKE ANY(ARRAY['%oo','bar']) → TRUE */ }
#[test]
fn any_subquery_still_works() { /* SELECT id = ANY(SELECT id FROM t) — existing */ }

// crates/axiomdb-sql/tests/integration_array_unnest.rs

#[test]
fn unnest_single_array() {
    // SELECT * FROM unnest(ARRAY[1,2,3]) AS u → 3 rows: 1, 2, 3
}
#[test]
fn unnest_from_table_column() {
    // CREATE TABLE t (tags TEXT[]); INSERT ...; SELECT u.tag FROM t, unnest(t.tags) AS u(tag)
}
#[test]
fn unnest_multiple_arrays() {
    // SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b']) AS u(x,y)
    // → (1,'a'), (2,'b')
}
#[test]
fn unnest_mismatched_lengths_error() {
    // SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b','c']) → error
}
#[test]
fn unnest_null_array_zero_rows() {
    // SELECT * FROM unnest(NULL::int[]) → 0 rows
}
#[test]
fn unnest_empty_array_zero_rows() {
    // SELECT * FROM unnest(ARRAY[]::int[]) → 0 rows
}
#[test]
fn unnest_null_elements() {
    // SELECT * FROM unnest(ARRAY[1, NULL, 3]) → 3 rows: 1, NULL, 3
}
#[test]
fn unnest_lateral_correlation() {
    // SELECT t.id, u.val FROM t, LATERAL unnest(t.arr) AS u(val)
}
```

**Implementation outline:**

**Part A — ANY/ALL:**

1. **`crates/axiomdb-sql/src/expr.rs`**
   - Add:
     ```rust
     AnyOf { expr: Box<Expr>, array: Box<Expr> },
     AllOf { expr: Box<Expr>, array: Box<Expr> },
     ```

2. **`crates/axiomdb-sql/src/parser/expr.rs`**
   - In expression parsing, when `ANY` or `ALL` keyword is encountered:
     - Peek ahead: if followed by `(` and next token is `SELECT` → subquery
       (existing path, do not change).
     - Otherwise → parse `(array_expression)` and wrap as `Expr::AnyOf` / `Expr::AllOf`.
   - Support operators: `=`, `<>`, `<`, `<=`, `>`, `>=`, `LIKE`, `ILIKE`.

3. **`crates/axiomdb-sql/src/eval/functions/any_all.rs`** (new file)
   - `fn eval_any_of(expr: &Expr, array: &Expr, row: &[Value], runner: &impl SubqueryRunner) -> Result<Value, DbError>`
     - Evaluate `array` expression → must be `Value::Array`.
     - For each element, evaluate the comparison via the outer binary operator.
     - ANY: first TRUE → TRUE; all FALSE + no NULL → FALSE; else NULL.
     - ALL: first FALSE → FALSE; all TRUE + no NULL → TRUE; else NULL.
   - `fn eval_all_of(...)` — same pattern for ALL.

4. **`crates/axiomdb-sql/src/eval/core.rs`**
   - Wire `Expr::AnyOf` / `Expr::AllOf` into the evaluator.

**Part B — unnest():**

5. **`crates/axiomdb-sql/src/lexer.rs`**
   - Add `#[token("UNNEST", ignore(ascii_case))]` → `Token::Unnest`.

6. **`crates/axiomdb-sql/src/ast.rs`**
   - Add `FromClause::Unnest(Box<UnnestClause>)`.
   - Add struct:
     ```rust
     UnnestClause {
         exprs: Vec<Expr>,
         alias: Option<String>,
         column_aliases: Vec<String>,
     }
     ```

7. **`crates/axiomdb-sql/src/parser/dml.rs`** (or where `parse_from_item` lives)
   - Add `parse_unnest(p)`: consume `unnest(...)` and parse comma-separated
     array expressions and optional `AS alias(col1, col2, ...)`.
   - Wire into `parse_from_item`: when `Token::Unnest` seen.

8. **`crates/axiomdb-sql/src/executor/select_core.rs`** (or new helper)
   - Add `materialize_unnest(spec: &UnnestClause, session, runner)`:
     - Evaluate each array expression → must be `Value::Array`.
     - Validate all arrays have same length (error if not).
     - Produce rows: one row per index, zipping elements from each array.
     - NULL array → zero rows.
     - Single-array: one column per row.
     - Multi-array: one column per array.
   - Integrate into the FROM processing loop alongside existing `JsonTable`,
     `JsonbSrf`, `Subquery`, `Values` materialization.

9. **`crates/axiomdb-sql/src/analyzer_stmt.rs`**
   - Resolve `FromClause::Unnest` column types: element type of each array
     becomes the column type.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_array_any_all
cargo test -p axiomdb-sql --test integration_array_unnest
cargo test -p axiomdb-sql --test integration_subquery any  # existing subquery still works
```

**Commit message:**

```
feat(fase-20): ANY/ALL constructs and unnest() set-returning function

- Expr::AnyOf / Expr::AllOf parsed and evaluated with =, <>, <, <=, >, >=, LIKE, ILIKE
- Parser disambiguates ANY/ALL(array) from ANY/ALL(subquery) by peeking for SELECT
- NULL handling: ANY returns NULL when all comparisons are NULL; ALL returns NULL on first NULL
- FromClause::Unnest(UnnestClause) with multi-array zip support
- unnest() as FROM-clause table function: single-array, multi-array, and LATERAL
- NULL array → 0 rows; mismatched lengths → error; NULL elements preserved
- Existing ANY/ALL subquery path unchanged

Phase 20/34. Step 7/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 8 — GIN Indexing for Arrays

**Goal:** Create, build, maintain, and probe GIN indexes on array columns,
reusing the Phase 11.17 `GinScan` / `gin_key_term` infrastructure with 4
strategies (`@>`, `&&`, `<@`, `=`).

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_array_gin.rs

#[test]
fn create_gin_index_on_array_column() {
    // CREATE TABLE t (tags TEXT[]);
    // CREATE INDEX idx_tags ON t USING GIN (tags);
}

#[test]
fn gin_build_extracts_all_array_elements() {
    // INSERT INTO t VALUES (ARRAY['a','b','c']), (ARRAY['b','d']);
    // CREATE INDEX ... ; verify via index scan: tags @> ARRAY['a'] returns row 1
}

#[test]
fn gin_probe_contains() {
    // CREATE INDEX ... ; SELECT * FROM t WHERE tags @> ARRAY['urgent'];
    // Only rows containing 'urgent' returned.
}

#[test]
fn gin_probe_overlap() {
    // SELECT * FROM t WHERE tags && ARRAY['a','z'];
}

#[test]
fn gin_probe_contained_by() {
    // SELECT * FROM t WHERE tags <@ ARRAY['a','b','c','d'];
}

#[test]
fn gin_probe_equality() {
    // SELECT * FROM t WHERE tags = ARRAY['a','b','c'];
}

#[test]
fn gin_null_elements_not_indexed() {
    // INSERT INTO t VALUES (ARRAY[NULL, 'a']);
    // Query tags @> ARRAY['a'] still works via GIN.
}

#[test]
fn gin_maintenance_on_insert() {
    // INSERT into indexed table → index updated with new elements.
}

#[test]
fn gin_maintenance_on_update() {
    // UPDATE tags = ARRAY['x','y'] → old elements removed, new elements added.
}

#[test]
fn gin_maintenance_on_delete() {
    // DELETE → all elements removed from GIN postings.
}

#[test]
fn gin_recheck_required() {
    // Verify EXPLAIN shows GIN scan with recheck for @> queries.
}

#[test]
fn gin_without_index_falls_back_to_seq_scan() {
    // No GIN index → seq scan with filter.
}
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/index_maintenance.rs`**
   - `fn gin_extract_array_keys(values: &[Value]) -> Vec<Vec<u8>>`:
     - Given a row's column values, for each array column with a GIN index,
       `decode_array` → iterate all leaf elements → encode each element as
       `[ColumnType tag:u8][encoded element bytes]` and call `gin_key_term`-style
       encoding (reuse `gin_key_term` from `axiomdb-types/src/jsonb.rs` or
       create `gin_array_key_term`).
     - Skip NULL elements (not indexed).
     - For multidimensional arrays: flatten all leaf elements.
   - Integrate into existing `if idx.index_type == 4` branches:
     - INSERT: extract keys from new row, add to GIN.
     - DELETE: extract keys from old row, remove from GIN.
     - UPDATE: diff old keys vs new keys, add/remove accordingly.
   - The existing `index_type == 4` code path in `insert_helpers.rs`,
     `update_entry.rs`, `update_clustered_helpers.rs`, and `delete.rs`
     should call the unified `gin_extract_keys_for_index(idx, old_row, new_row)`
     helper.

2. **`crates/axiomdb-sql/src/planner_select.rs`** — `plan_gin_scan`
   - Extend the existing `plan_gin_scan` function with a new probe type:
     ```rust
     GinProbe::ArrayContains,     // col @> ARRAY[...]
     GinProbe::ArrayOverlap,      // col && ARRAY[...]
     GinProbe::ArrayContainedBy,  // col <@ ARRAY[...]
     GinProbe::ArrayEquals,       // col =  ARRAY[...]
     ```
   - Pattern match: `Expr::BinaryOp { op: JsonContains, left: Column{name,..}, right: ArrayConstructor{..} }`
     where the column's type is `DataType::Array(...)` → `GinProbe::ArrayContains`.
   - Extract query terms: evaluate the `ARRAY[...]` expression at plan time
     (must be a literal or const-foldable), decode, extract each leaf element
     encoded as a GIN key.
   - Set `recheck_required = true` always (element counts not in postings).

3. **`crates/axiomdb-sql/src/executor/select_helpers.rs`** — `gin_scan_rows`
   - No changes needed — the existing `gin_scan_rows` already handles
     intersection of query terms and rechecks via the original predicate.

4. **`crates/axiomdb-sql/src/executor/ddl_create_index.rs`**
   - When building a GIN index on an array column (`idx.index_type == 4` and
     column type is `ColumnType::Array`), call the unified `gin_extract_array_keys`
     helper for each row during the scan/build loop.

5. **`crates/axiomdb-sql/src/executor/delete.rs`**, **`update_entry.rs`**, **`insert_helpers.rs`**
   - Ensure GIN maintenance for array columns works through the unified path.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_array_gin
cargo test -p axiomdb-sql --test integration_jsonb_gin    # existing GIN paths
```

**Commit message:**

```
feat(fase-20): GIN indexing for array columns — @>, &&, <@, = strategies

- GIN index build extracts all leaf array elements as tagged keys
- Reuses Phase 11.17 GinScan + gin_term_bounds infrastructure
- Planner detects col @> ARRAY[...], col && ARRAY[...], col <@ ARRAY[...],
  col = ARRAY[...] and routes to GinScan with recheck
- Index maintenance on INSERT/UPDATE/DELETE via unified gin_extract_array_keys
- NULL elements excluded from GIN keys
- Multidimensional arrays flattened to leaf elements for GIN extraction
- Planner fallback to seq scan when no GIN index exists

Phase 20/34. Step 8/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 9 — array_agg() Aggregate

**Goal:** Implement `array_agg(expr [ORDER BY ...] [DISTINCT])` with PG
semantics: NULLs included, empty group returns NULL.

**TDD test:**

```rust
// crates/axiomdb-sql/tests/integration_array_agg.rs

#[test]
fn array_agg_simple() {
    // SELECT array_agg(x) FROM (VALUES (1),(2),(3)) AS t(x) → {1,2,3}
}

#[test]
fn array_agg_empty_group() {
    // SELECT array_agg(x) FROM t WHERE FALSE → NULL
}

#[test]
fn array_agg_with_nulls() {
    // SELECT array_agg(x) FROM (VALUES (1),(NULL),(3)) AS t(x) → {1,NULL,3}
}

#[test]
fn array_agg_with_order_by() {
    // SELECT array_agg(x ORDER BY x DESC) FROM (VALUES (1),(3),(2)) AS t(x)
    // → {3,2,1}
}

#[test]
fn array_agg_distinct() {
    // SELECT array_agg(DISTINCT x) FROM (VALUES (1),(2),(2),(3)) AS t(x)
    // → {1,2,3}
}

#[test]
fn array_agg_grouped() {
    // SELECT grp, array_agg(val) FROM t GROUP BY grp
}

#[test]
fn array_agg_with_where() {
    // SELECT array_agg(x) FROM t WHERE x > 0
}
```

**Implementation outline:**

1. **`crates/axiomdb-sql/src/executor/agg_descriptor.rs`**
   - Add to `AggExpr`:
     ```rust
     ArrayAgg {
         arg: Expr,
         distinct: bool,
         order_by: Vec<(Expr, crate::ast::SortOrder)>,
         agg_idx: usize,
     },
     ```
   - In `collect_agg_exprs_from()`: detect `array_agg(expr [ORDER BY ...])`
     function call and register as `AggExpr::ArrayAgg`.

2. **`crates/axiomdb-sql/src/executor/agg_arrays.rs`** (new file)
   - `pub struct ArrayAggAccum { values: Vec<Value>, element_type: Option<DataType> }`
   - `fn new() -> Self` — empty accumulator.
   - `fn update(&mut self, val: &Value) -> Result<(), DbError>`
     — push value (including NULLs for PG compat).
   - `fn finalize(&self) -> Result<Value, DbError>`
     — sort values if ORDER BY, dedup if DISTINCT, build `Value::Array`, encode blob.
   - `fn finalize_sorted(&self, order_by: &[(Expr, SortOrder)]) -> Result<Value, DbError>`
     — for ORDER BY variant.

3. **`crates/axiomdb-sql/src/executor/agg_accum.rs`**
   - Add `Self::ArrayAgg { values, order_by, distinct, element_type }` variant
     to the accumulator union.
   - In `new()`: match `AggExpr::ArrayAgg { .. }` → create accumulator.
   - In `update()`: match `AggExpr::ArrayAgg { arg, .. }` → evaluate `arg`,
     push to `values`.
   - In `finalize()`: match `AggExpr::ArrayAgg { distinct, order_by, .. }` →
     dedup (hash-set of values) if `distinct`, sort if `order_by` non-empty,
     then build array blob.

4. **`crates/axiomdb-sql/src/executor/agg_sorted.rs`**
   - If `ORDER BY` is present in `AggExpr::ArrayAgg`, sort accumulated values
     before calling finalize.

**Verification:**

```bash
cargo test -p axiomdb-sql --test integration_array_agg
cargo test -p axiomdb-sql --test integration_aggregates agg    # existing aggs still pass
```

**Commit message:**

```
feat(fase-20): array_agg() aggregate with ORDER BY and DISTINCT

- AggExpr::ArrayAgg variant collected from SELECT and HAVING
- Accumulator gathers all values (including NULLs, PG compat)
- ORDER BY sorts accumulated values before building array
- DISTINCT deduplicates values via hash set
- Empty group returns NULL (PG compat), not empty array
- Return type: DataType::Array(element_type)

Phase 20/34. Step 9/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 10 — Wire Protocol

**Goal:** Serialize array columns as PG-compatible `{...}` text over both
MySQL text and binary protocols.

**TDD test:**

```python
# tools/wire-test.py  (add section after existing tests)

# ── Arrays ────────────────────────────────────────────────────────────
conn.execute("CREATE TABLE t_arr (id INT PRIMARY KEY, tags TEXT[])")
conn.execute("INSERT INTO t_arr VALUES (1, '{\"hello\",\"world\"}')")
conn.execute("INSERT INTO t_arr VALUES (2, '{}')")
conn.execute("INSERT INTO t_arr VALUES (3, NULL)")

rows = conn.query("SELECT * FROM t_arr ORDER BY id")
assert rows == [(1, "{hello,world}"), (2, "{}"), (3, None)]

# Prepared statement binary
stmt = conn.prepare("SELECT tags FROM t_arr WHERE id = ?")
assert stmt.execute([1]) == [("{hello,world}",)]
assert stmt.execute([3]) == [(None,)]
```

**Implementation outline:**

1. **`crates/axiomdb-network/src/mysql/result.rs`**
   - `datatype_to_mysql_type(DataType::Array(_))` → return `0xfd` (VAR_STRING,
     same as TEXT/JSON).
   - `encode_binary_cell`: match `Value::Array(_)` → call `array_to_text(blob)`,
     then encode the resulting text string as length-prefixed string (same as
     `Value::Text` path).

2. **`crates/axiomdb-network/src/mysql/column.rs`** (or wherever column metadata is built)
   - For `DataType::Array(inner)`, set column name/type to `"<INNER>[]"` in the
     column definition packet.
   - Wire charset: UTF-8 (same as text columns).

3. **`crates/axiomdb-network/src/mysql/`** text protocol result writing
   - For text protocol (non-prepared), `Value::Array` → call `array_to_text(blob)`
     and write the resulting `{...}` string inline.

**Verification:**

```bash
python3 tools/wire-test.py                     # full regression including arrays
cargo test -p axiomdb-network                  # network crate tests
```

**Commit message:**

```
feat(fase-20): MySQL wire protocol array serialization

- Array columns use VAR_STRING type code (0xfd) over wire
- Text protocol: arrays rendered as PG-compatible {…} text format
- Binary protocol (prepared stmts): array blob → array_to_text → length-prefixed string
- Column metadata reports TYPE[] name for array columns
- NULL array column → SQL NULL over wire; empty array → {}
- wire-test.py extended with array DDL + DML + SELECT assertions

Phase 20/34. Step 10/11 of plan-20.4-arrays.
Spec: specs/fase-20/spec-20.4-arrays.md
```

---

## Step 11 — Integration Tests + Close

**Goal:** Write 50+ integration tests covering all edge cases from the spec,
run full workspace gates, update documentation, and close the subphase.

**Tasks:**

1. **Integration test files** (all new):
   - `crates/axiomdb-sql/tests/integration_arrays.rs` — DDL, INSERT, SELECT,
     ARRAY constructor, nested arrays, coercion, edge cases.
   - `crates/axiomdb-sql/tests/integration_array_operators.rs` — all operators.
   - `crates/axiomdb-sql/tests/integration_array_functions.rs` — all 17 functions.
   - `crates/axiomdb-sql/tests/integration_array_any_all.rs` — ANY/ALL.
   - `crates/axiomdb-sql/tests/integration_array_unnest.rs` — unnest.
   - `crates/axiomdb-sql/tests/integration_array_gin.rs` — GIN indexes.
   - `crates/axiomdb-sql/tests/integration_array_agg.rs` — array_agg.

2. **Cover all spec edge cases** (spec lines 431-455):
   - Empty array `{}`, NULL array column, NULL elements, 6D max, 0-size dim,
     negative bounds, text quoting, `"NULL"` element, `ARRAY[]` empty constructor,
     `unnest` with NULLs, `array_agg` empty group, `@>` with NULLs,
     `array_position` with NULLs and not-found, TOAST overflow, negative
     subscript, `ANY`/`ALL` with `LIKE`, GIN on empty array, GIN maintenance,
     `information_schema.COLUMNS` display, `SHOW CREATE TABLE` display.

3. **Wire test** — `tools/wire-test.py`:
   - Extend with array DDL, DML, SELECT, INSERT, UPDATE, DELETE, operators,
     functions, unnest, array_agg, GIN queries, ANY/ALL.
   - Ensure full backward compatibility with existing tests.

4. **Documentation:**
   - `docs/fase-20.md` — update with array subphase summary.
   - `docs/progreso.md` — mark 20.4 as ✅.
   - `docs-site/src/user-guide/sql-reference/ddl.md` — add array column syntax.
   - `docs-site/src/user-guide/features/indexes.md` — add GIN array strategy.
   - `docs-site/src/internals/catalog.md` — document array_element_type field.
   - `docs-site/src/internals/storage.md` — document array blob format.
   - `docs-site/src/internals/btree.md` — "Internal B+Tree" → update if needed.
   - `docs-site/src/internals/mvcc.md` — mention array TOAST if relevant.
   - `memory/project_state.md` — update active phase.
   - `memory/architecture.md` — add array_codec.rs, array_io.rs, array modules.

5. **Gates:**
   ```bash
   cargo test --workspace
   cargo clippy --workspace -- -D warnings
   cargo fmt --check
   python3 tools/wire-test.py
   ```
   - No `unwrap()` / `expect()` in production `src/`.
   - All `unsafe` blocks have `// SAFETY:` comments.

**Verification:**

```bash
cargo test --workspace --quiet
cargo clippy --workspace -- -D warnings
cargo fmt --check
python3 tools/wire-test.py
```

**Commit message:**

```
feat(fase-20): close 20.4 — PostgreSQL array parity (83+ tests, docs, wire)

- 7 integration test files: 50+ tests covering all edge cases
- wire-test.py extended with array DDL, DML, operators, functions, GIN
- Documentation updated: user guide (DDL, indexes), internals (catalog, storage)
- docs/fase-20.md, docs/progreso.md, memory/* updated
- All gates passing: cargo test --workspace, clippy, fmt, wire-test.py

Phase 20/34 completed. See docs/fase-20.md
Spec: specs/fase-20/spec-20.4-arrays.md | Tests: crates/axiomdb-sql/tests/integration_arrays*.rs
```

---

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Exhaustive `DataType`/`Value` matches across many crates break | high | Run `cargo test --workspace` after Step 1; fix match arms incrementally. Add `_` wildcard where appropriate. |
| On-disk array format bug (endianness, null bitmap, dims) isn't caught by simple tests | medium | Test with known-answer vectors (pre-computed blobs from PG reference: `{1,NULL,3}::int[]` hex dump). |
| Polymorphic operator dispatch breaks JSONB queries | medium | Run full `integration_jsonb` test suite after Step 5; keep JSONB dispatch path identical. |
| GIN planner ambiguity between array and JSONB `@>` | medium | Check column DataType in `plan_gin_scan`: `DataType::Array(_)` vs `DataType::Jsonb`. |
| `unnest` FROM integration conflicts with existing FROM processing | high | Follow same pattern as `FromClause::JsonTable` / `JsonbSrf` (Phase 11.20/11.25). Add dedicated `materialize_unnest` helper. |
| `array_agg` ORDER BY requires sort infrastructure not yet wired | medium | Reuse existing `agg_sorted.rs` sort infrastructure from `GROUP_CONCAT` ORDER BY. |
| Memory blowup with large arrays (2^31 elements) | low | Enforce per-row max 8KB before TOAST, max 10M elements (`i32::MAX` is too large for practical use). |
| ColumnDef backward-compat: old rows without array fields | low | Extensive legacy-roundtrip tests in Step 1; `array_element_type` reads as `None` if bytes exhausted. |

## Estimated Effort

Total: max

- Step 1: 2-3 h — type system changes cascade across many match arms
- Step 2: 3-5 h — codec + I/O with PG text format edge cases
- Step 3: 2-3 h — parser brackets loop + catalog persistence
- Step 4: 2-4 h — constructor, coercion, nested arrays
- Step 5: 3-5 h — operators + polymorphic dispatch + subscript/slice
- Step 6: 3-5 h — 17 functions with polymorphic dispatch
- Step 7: 3-5 h — ANY/ALL + unnest SRF + FROM integration
- Step 8: 3-5 h — GIN integration with planner + index maintenance
- Step 9: 2-3 h — array_agg aggregate with ORDER BY/DISTINCT
- Step 10: 1-2 h — wire protocol (mostly mechanical)
- Step 11: 3-5 h — tests, docs, gates, closeout
