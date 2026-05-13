# Spec: SQL Arrays — Full PostgreSQL Parity

Phase: 20 — Types + import/export
Task: 20.4 — PostgreSQL arrays (all types, operators, functions, GIN, multidimensional)
Status: approved
Effort: max

## Context

AxiomDB currently has 12 scalar types with no array support. The ENUM subphase (20.3)
established the pattern of adding catalog-backed types via trailing optional fields on
`ColumnDef` while keeping physical storage in existing column types. This spec extends
that pattern to arrays using a new `ColumnType::Array` variant.

PostgreSQL's array implementation (`pg_type` entries with `typelem` links, on-disk
format with `ndim`+`dims`+`lbound`+null bitmap) is the reference design. The
`@>`/`<@`/`&&` operators must be polymorphic — existing JSONB dispatches and new
array dispatches coexist on the same infix tokens.

## Goal

Deliver complete PostgreSQL-compatible SQL arrays: any scalar element type, 
multidimensional (up to 6D), all operators/functions, `ANY`/`ALL` constructs,
`unnest()` set-returning function, GIN indexing, and `array_agg()` aggregate.

## Non-goals

- Not implementing arrays of composite/user-defined types — only scalar elements
- Not implementing arrays of arrays (e.g., `INT[][]`) where the middle type is
  also an array — PG supports this, deferred to composite type support (20.18)
- Not implementing PL/pgSQL "expanded array" (mutable in-place) representation
- Not implementing `array_cross_product`, `array_cosine_similarity`, or
  DuckDB/vector-specific array functions — those are Phase 34 (Vector/GIS)
- Not implementing `pg_notify` for array changes
- Not changing the existing JSONB `@>` / `@@` semantics — array operators are
  polymorphic dispatch, not a replacement

## Behavior

### Type system

**DataType** (axiomdb-types/src/types.rs):
```rust
pub enum DataType {
    // ... existing 12 variants ...
    Array(Box<DataType>),  // NEW — element type for in-memory use
}
```

**Value** (axiomdb-types/src/value.rs):
```rust
pub enum Value {
    // ... existing variants ...
    Array(Vec<Value>),  // NEW — owned vector of element values
}
```

**ColumnType** (axiomdb-catalog/src/schema_database.rs):
```rust
pub enum ColumnType {
    // ... existing 1-12 ...
    Array = 13,  // NEW
}
```

**ColumnDef** (axiomdb-catalog/src/schema_table.rs):
Extend with trailing optional field: `array_element_type: Option<ColumnType>`.

Binary format extension (after `enum_type_name` field):
```
... [enum_type_name_bytes] [array_element_type_len:u8(1 = present, 0 = absent)] [array_element_type:u8 (ColumnType discriminant)]
```

### DDL syntax

```sql
CREATE TABLE t (
    tags TEXT[],              -- 1D text array
    scores INT[][],           -- 2D int array (max 6D)
    matrix FLOAT[3][3],       -- fixed-size 3×3 (size hints stored but unenforced)
    data JSONB[],             -- 1D JSONB array
    flags BOOL ARRAY          -- PG-compatible ARRAY keyword suffix
);

-- Display in SHOW CREATE TABLE:
--   tags TEXT[], scores INT[][], matrix FLOAT[3][3], data JSONB[]
```

Parser changes in `parse_data_type()` (`ddl.rs`):
- After parsing the base type, consume `[]` brackets in a loop
- `TEXT[]` → DataType::Array(Box::new(DataType::Text)), ndims=1
- `INT[][]` → DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int)))), ndims=2 (PG style)
- `FLOAT[3][3]` → same but with size hints parsed and stored

Per PG convention, `TEXT[][]` is really `_text` (a 1D array whose elements are 1D arrays).
For simplicity in MVP, `INT[][]` creates a `ColumnType::Array` with `array_element_type = Array`
and `ndims = 2` stored on ColumnDef.

Simplified catalog convention:
- `array_element_type` stores the immediate element type
- `array_ndims: Option<u8>` stores the number of dimensions (1-6)
- For `INT[]`: element_type=Int, ndims=1
- For `INT[][]`: element_type=Array(Int), ndims=2

### On-disk array format (varlena, similar to TEXT/BYTES in row codec)

Each array column in a row is stored as a length-prefixed varlena blob:

```
┌──────────────────────────────────────────────────────────────┐
│ [total_len: u32 LE]  ← total bytes of this array blob        │
│ [ndim: i32]           ← number of dimensions (1-6, 0=empty)  │
│ [dataoffset: i32]     ← 0 if no nulls, else offset to data   │
│ [elemtype_flags: u8]  ← ColumnType discriminant of elements   │
│ [dims[ndim]: i32[]]   ← dimension lengths (row-major order)  │
│ [lbound[ndim]: i32[]] ← lower bounds (default all 1)         │
│ [null_bitmap]         ← ceil(nitems/8) bytes, present if     │
│                          dataoffset != 0                      │
│ [elements: u8[]]      ← packed element values, each uses its  │
│                          type's storage size (PG row-major)   │
└──────────────────────────────────────────────────────────────┘
```

Design decisions:
- `total_len` is u32 (not u24 like TEXT) because arrays can be larger than 16MB
- `dataoffset = 0` when no null elements (null bitmap omitted)
- All integers are little-endian (matching AxiomDB convention, not PG big-endian)
- Row-major: last subscript varies fastest — `a[1][1], a[1][2], a[2][1], a[2][2]`
- Empty array: `ndim=0, dims=[], lbound=[], elements=[]` — decoded as empty `{}`
- Maximum size: TOAST threshold (8KB per tuple), overflow to TOAST pages
- Maximum elements: 2^31 - 1

**NULL handling:**
- A `Value::Null` column is SQL NULL (no array blob at all)
- A non-null array column may contain NULL *elements* — these are marked in the null bitmap
- Fixed-size elements skip their bytes when null; variable-size elements skip their u24+payload

**Fixed-size element types (no change to element encoding):**
- Bool: 1 byte (0x00/0x01)
- Int/Date: 4 bytes LE
- BigInt/Real/Timestamp: 8 bytes LE
- Decimal: 16 bytes LE i128 + 1 byte scale
- Uuid: 16 bytes

**Variable-size element types:**
- Text/Bytes/Json/Jsonb: u24 LE length + payload bytes

**Nested arrays (INT[][]):**
- The element type is itself `ColumnType::Array(13)`
- Each "element" is itself a full array blob (recursive)
- So `INT[2][3]` (2 rows, 3 cols) stores:
  - Top-level: ndim=2, dims=[2,3], lbound=[1,1]
  - Each of 6 elements is... an INT value (the leaf type)
- But since element_type=Array, each of the 6 "elements" is actually an inner array blob:
  - Top-level: ndim=1, dims=[3], lbound=[1], elements = [inner_blob_row0, inner_blob_row1]
  - Each inner_blob: ndim=1, dims=[3], lbound=[1], elements = [int1, int2, int3]

Actually, for AxiomDB, we simplify: a multidimensional array stores ALL leaf elements 
in a single blob with ndim>1, NOT as nested array blobs. This matches PG's actual storage.
`array_element_type` is always a SCALAR ColumnType, never ColumnType::Array.

So for `INT[2][3]`:
- array_element_type = Int
- ndim = 2
- dims = [2, 3]
- 6 Int values packed row-major

And for a 1D array of 1D arrays (PG: `INT[][]` is really `_int4` with int4[] elements):
- Actually in PG, `INT[][]` IS a 2D array, because PG always collapses to the
  maximum dimension with same bounds. So PG and our simplified model align.

For `FLOAT[]` (1D):
- array_element_type = Float
- ndim = 1
- dims = [n]
- n Float values packed

### Array input/output (text ↔ binary conversion)

**array_out (binary → text):** PG-compatible text format
```
{}                          ← empty array
{1,2,3}                     ← 1D int array
{{1,2},{3,4}}              ← 2D int array (2×2)
{foo,bar,baz}              ← 1D text array (no quoting needed)
{"hello, world",barelem}   ← elements with commas/spaces are double-quoted
{"say \"hi\"",normal}      ← double-quotes inside are escaped with backslash
{1,NULL,3}                 ← NULL elements written as NULL (unquoted)
{"NULL","not null"}        ← string "NULL" must be quoted to avoid ambiguity
[1:3]={1,2,3}             ← explicit lower bounds (only if any lbound != 1)
[-1:1][1:3]={{0,1,2},{3,4,5}}  ← multidimensional explicit bounds
```

Implementation approach:
- `fn array_to_text(blob: &[u8]) -> String` — recurse by dimension, handle quoting
- `fn text_to_array(text: &str, element_type: ColumnType) -> Result<Vec<u8>, DbError>`
  
**array_in (text → binary):** Parse `{...}` format
- Validate bracket nesting matches ndims
- Handle quoted strings with backslash escaping
- Parse dimension prefix `[lb:ub]=` if present
- Returns the binary blob

**Error cases:**
| Input | Error |
|-------|-------|
| `{1,2` (unclosed) | `ParseError: "unterminated array literal"` |
| `{1,2,3}::int[]` valid, `{1,2,three}::int[]` | `InvalidValue: "invalid input syntax for type integer: \"three\""` |
| Too many dimensions | `InvalidValue: "number of array dimensions exceeds the maximum allowed (6)"` |
| Mismatched dimensions across sub-arrays | `InvalidValue: "multidimensional arrays must have array expressions with matching dimensions"` |

### ARRAY constructor

```sql
SELECT ARRAY[1, 2, 3];                    -- {1,2,3}  (int[])
SELECT ARRAY['a', 'b', 'c'];              -- {a,b,c}  (text[])
SELECT ARRAY[ARRAY[1,2], ARRAY[3,4]];     -- {{1,2},{3,4}}  (int[][])
SELECT ARRAY[]::int[];                     -- {} (empty int array)
```

Parser: new `parse_array_constructor(p)` called when `Token::LBracket` follows `ARRAY`.
AST: `Expr::ArrayConstructor { elements: Vec<Expr> }`.
Evaluator: `eval_array_constructor(elements, session)` — evaluates each element,
infers common type via coercion, builds binary array blob.

Type inference:
- All elements must have a common type (coercion matrix applied)
- Empty array requires explicit cast: `ARRAY[]::int[]`
- Mixed types: `ARRAY[1, 2.5]` → int coerced to real → `{1,2.5}` (real[])
- Nested: `ARRAY[ARRAY[1,2], ARRAY[3,4,5]]` → error (mismatched dimensions)

### Operators

All operators match PostgreSQL semantics:

**Subscript `arr[n]`:**
- 1-indexed (PG convention)
- `arr[1]` returns first element
- NULL if index out of bounds (PG: returns NULL, not error)
- `arr[1:3]` returns slice (sub-array from index 1 to 3 inclusive)
- Multidimensional: `arr[1][2]` or `arr[1,2]` (both accepted per PG)
- PG-style: `arr[1:2][3:4]` for multidimensional slices

**Equality `=` and inequality `<>`:**
- Element-by-element comparison
- Both arrays must have same ndim and dims
- NULL element ≠ NULL element (PG: NULL = NULL in array context is NULL/unknown, 
  but = between two arrays with all NULLs... PG returns NULL)
- Actually PG says: `ARRAY[NULL] = ARRAY[NULL]` returns NULL (unknown)
- Implementation: element-by-element comparison; if any pair is NULL, result is NULL

**Contains `@>`:**
- `arr1 @> arr2` — true if every element of arr2 appears in arr1
- Not order-sensitive (set semantics)
- Duplicates matter: `ARRAY[1,1,2] @> ARRAY[1,2]` is true
- Multidimensional: treats all leaf elements as a flat set
- NULL elements: `ARRAY[1,NULL] @> ARRAY[1]` is true (NULL in LHS is just another element)
  `ARRAY[1] @> ARRAY[NULL]` returns NULL (unknown, per PG)

**Contained by `<@`:**
- Reverse of `@>`: `arr1 <@ arr2` ≡ `arr2 @> arr1`

**Overlap `&&`:**
- `arr1 && arr2` — true if they share any element
- `ARRAY[1,2,3] && ARRAY[3,4,5]` → true
- NULL elements: `ARRAY[NULL] && ARRAY[1]` → NULL (unknown)
  But `ARRAY[NULL, 1] && ARRAY[1]` → true (found non-null match)

**Concatenation `||`:**
- `arr1 || arr2` — concatenates arrays
- Dimension must match: 1D+1D → 1D, 2D+2D → 2D
- `ARRAY[1,2] || ARRAY[3,4]` → `{1,2,3,4}`
- `ARRAY[1,2] || 3` → `{1,2,3}` (element-to-array concat, PG support)
- This conflicts with JSONB's `||` (concat) — polymorphic dispatch based on LHS type

**Polymorphic dispatch for @>/<@/&&/||:**
Operators `@>`, `<@`, `&&`, and `||` already exist in the lexer and AST for JSONB.
The evaluator now dispatches based on LHS type:
- If LHS is `Value::Array` → array semantics
- If LHS is `Value::Jsonb`/`Value::Json` → JSONB semantics
- Otherwise → `TypeMismatch` error

**Other comparisons (<, <=, >, >=):**
- Lexicographic element-by-element comparison (PG semantics)
- Deferred to later subphase if needed (rarely used in practice)

### Functions

All functions accept any array type (polymorphic on element type):

| Function | Signature | Semantics |
|----------|-----------|-----------|
| `array_length(arr, dim)` | `(anyarray, int) → int` | Length of dimension `dim` (1-indexed). NULL if dim > ndim |
| `array_lower(arr, dim)` | `(anyarray, int) → int` | Lower bound of dimension `dim` |
| `array_upper(arr, dim)` | `(anyarray, int) → int` | Upper bound = lbound[dim] + dims[dim] - 1 |
| `array_ndims(arr)` | `(anyarray) → int` | Number of dimensions. Empty array → 0 |
| `array_dims(arr)` | `(anyarray) → text` | Text like `[1:3][1:2]` |
| `cardinality(arr)` | `(anyarray) → int` | Total number of elements (product of dims) |
| `unnest(arr)` | `(anyarray) → setof anyelement` | Expand array to rows. Multidimensional: flattens? PG unnests one level |
| `array_append(arr, elem)` | `(anyarray, anyelement) → anyarray` | Append element to end of 1D array. Element type must match |
| `array_prepend(elem, arr)` | `(anyelement, anyarray) → anyarray` | Prepend element to start of 1D array |
| `array_cat(arr1, arr2)` | `(anyarray, anyarray) → anyarray` | Concatenate two arrays. Same as `\|\|` |
| `array_remove(arr, elem)` | `(anyarray, anyelement) → anyarray` | Remove all occurrences of elem. Preserves order |
| `array_replace(arr, old, new)` | `(anyarray, anyelement, anyelement) → anyarray` | Replace all old with new |
| `array_position(arr, elem)` | `(anyarray, anyelement) → int` | 1-indexed position of first occurrence. 0 if not found. NULL-safe |
| `array_position(arr, elem, start)` | `(anyarray, anyelement, int) → int` | Search starting from position `start` |
| `array_to_string(arr, delim)` | `(anyarray, text) → text` | Join non-null elements with delimiter. NULL elements skipped |
| `array_to_string(arr, delim, null_str)` | `(anyarray, text, text) → text` | Replace NULL elements with `null_str` |
| `string_to_array(str, delim)` | `(text, text) → text[]` | Split string by delimiter → text array |
| `string_to_array(str, delim, null_str)` | `(text, text, text) → text[]` | Replace `null_str` with NULL elements |

### ANY and ALL constructs

SQL-standard constructs for comparing a scalar against array elements:

```sql
-- ANY: true if any element satisfies
SELECT 100 = ANY(ARRAY[50, 100, 200]);       -- true
SELECT 100 > ANY(ARRAY[50, 150]);            -- true (150 > 100)
SELECT 'foo' = ANY(ARRAY['bar', 'baz']);     -- false

-- ALL: true if all elements satisfy
SELECT 100 < ALL(ARRAY[200, 300]);           -- true
SELECT 100 > ALL(ARRAY[50, 100]);            -- false (100 is NOT > 100)
SELECT 'foo' = ALL(ARRAY['foo', 'foo']);     -- true

-- With subqueries (existing support) — parser disambiguates:
SELECT 100 = ANY(SELECT id FROM t);           -- subquery (existing)
SELECT 100 = ANY(ARRAY[1,2,3]);              -- array (new)
```

**Parser:**
- `ANY(arr_expr)` and `ALL(arr_expr)` where `arr_expr` evaluates to an array
- AST: `Expr::AnyOf { expr: Box<Expr>, array: Box<Expr> }`, 
  `Expr::AllOf { expr: Box<Expr>, array: Box<Expr> }`
- Disambiguation: if `ANY`/`ALL` is followed by `(` and the next token is `SELECT`,
  it's a subquery (existing path). Otherwise, it's an array expression.

**Evaluator:**
- Evaluate `array` expression → must be `Value::Array`
- For each element:
  - ANY: compare `expr` with element using the operator. If comparison is true, return true. If NULL, defer.
  - ALL: compare `expr` with element using the operator. If comparison is false, return false. If NULL, return NULL.
- ANY: if no element returned true and at least one NULL comparison, return NULL. Otherwise false.
- ALL: if no element returned false and at least one NULL comparison, return NULL. Otherwise true.

**Operators allowed with ANY/ALL:** =, <>, <, <=, >, >=, LIKE, ILIKE, and (for PG compat) any of the existing comparison operators.

### unnest() set-returning function

```sql
SELECT unnest(ARRAY[1,2,3]);                -- returns 3 rows: 1, 2, 3
SELECT unnest(tags) FROM t;                 -- expands each row's tags
SELECT t.id, u.tag FROM t, unnest(t.tags) AS u(tag);  -- lateral join
SELECT t.id, u.tag FROM t CROSS JOIN unnest(t.tags) AS u(tag);
SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b']) AS u(x,y);  -- multiple arrays (PG)
```

**Implementation:**
- New `FromClause::Unnest(Box<UnnestClause>)` AST variant
- `UnnestClause { exprs: Vec<Expr>, alias: Option<String>, column_aliases: Vec<String> }`
- Parser: `parse_unnest(p)` called when `Token::TyUnnest` (new token) followed by `(`
- Evaluator: `materialize_unnest(spec, session, runner)` — evaluates each array, produces rows
- Multiple arrays: zip them (must have same length per PG)
- For single-array: one column of `anyelement` type
- For multi-array: one column per array, types match respective element types
- NULL array → zero rows
- May or may not be correlated (LATERAL-like behavior)

### GIN indexing for arrays

```sql
CREATE INDEX idx_tags ON productos USING GIN (tags);
CREATE INDEX idx_scores ON t USING GIN (scores);

-- These use the index:
SELECT * FROM productos WHERE tags @> ARRAY['importante'];
SELECT * FROM productos WHERE tags && ARRAY['urgente', 'critico'];
SELECT * FROM t WHERE scores @> ARRAY[100];  -- contains
```

**Implementation:**
- Reuse Phase 11.17 / 11.21h GIN infrastructure (`GinScan`, `gin_key_term`)
- Index build: scan all rows, extract each array element as a separate GIN key
- For multidimensional arrays: flatten all leaf elements
- GIN key encoding: element value encoded as `[ColumnType tag][encoded element bytes]`
- Index strategies:
  - Strategy 1: `&&` (overlap) — GIN_SEARCH_MODE_DEFAULT
  - Strategy 2: `@>` (contains) — GIN_SEARCH_MODE_DEFAULT (must have ALL keys)
  - Strategy 3: `<@` (contained by) — GIN_SEARCH_MODE_INCLUDE_EMPTY
  - Strategy 4: `=` (equality) — GIN_SEARCH_MODE_DEFAULT
- Planner: `plan_gin_scan` extended to recognize `arr_col @> ARRAY[...]` and `arr_col && ARRAY[...]`
- Recheck always required (element counts not stored in GIN postings, structural semantics)
- This is identical to how PostgreSQL's `ginarrayproc.c` works

**Null elements in GIN:**
- NULL array elements are NOT indexed as GIN keys
- A query like `ARRAY[NULL] @> ARRAY[1]` cannot use GIN (NULL not in index)
- But `ARRAY[1,2] @> ARRAY[1]` uses GIN normally

### array_agg() aggregate function

```sql
SELECT array_agg(name) FROM users;           -- {Alice,Bob,Charlie}
SELECT array_agg(score ORDER BY score DESC) FROM scores;
SELECT dept, array_agg(name) FROM users GROUP BY dept;
```

**Implementation:**
- New `AggExpr::ArrayAgg` variant
- Accumulator: `ArrayAggAccum { values: Vec<Value>, element_type: DataType }`
- At finalize: build binary array blob (ndim=1) from accumulated values
- ORDER BY: sort accumulated values before building blob
- NULL values: included in array (PG: array_agg includes NULLs)
- Empty group: returns NULL (not empty array), matching PG behavior
- Return type: `DataType::Array(Box::new(element_type))`

### Wire protocol

**MySQL text protocol:**
- Array columns rendered as PG-compatible `{...}` text format
- `datatype_to_mysql_type(DataType::Array(_))` → `0xfd` (VAR_STRING, same as TEXT/JSON)
- Column metadata shows the array type name: `TEXT[]` or `INT[]`

**MySQL binary protocol:**
- Array columns encoded as length-prefixed string (same as TEXT)
- The string is the `{...}` text representation
- Prepared statements: `encode_binary_cell` handles `Value::Array` by calling `array_to_text`

## Edge cases

- [ ] Empty array `{}` — ndim=0, dims=[], elements=[]. Valid for any element type.
- [ ] NULL array column — `Value::Null`, no blob. Different from empty array.
- [ ] Array with NULL elements — null bitmap present, elements skipped. `{1,NULL,3}`.
- [ ] Maximum dimensions (6D) — validated at array construction and parsing.
- [ ] Maximum elements — 2^31-1 total. Enforced at construction.
- [ ] Zero-size dimension: `[1:0]` — ndim>0 but 0 elements. PG supports this.
- [ ] Degenerate bounds: `[-5:-3]` — 3 elements, negative indices. PG standard.
- [ ] Text elements with special chars: `{`, `}`, `,`, `"`, `\`, whitespace — properly quoted.
- [ ] Text element `"NULL"` — must be quoted in text format to avoid ambiguity with SQL NULL.
- [ ] `ARRAY[]` empty constructor — requires explicit cast, error if none.
- [ ] `unnest(ARRAY[NULL, 1, NULL])` — produces 3 rows (NULL, 1, NULL).
- [ ] `array_agg()` with no rows — returns NULL (PG compat).
- [ ] `@>` between arrays with NULL elements — PG semantics (NULL in query → NULL result).
- [ ] `array_position(ARRAY[NULL, 1], 1)` — returns 2 (NULL is a value, not a match).
- [ ] `array_position(ARRAY[1,2,3], 5)` — returns 0 (not found).
- [ ] Array TOAST: array > 8KB → TOAST overflow chain via `toast_row_if_needed`.
- [ ] Multidimensional subscript: `arr[1][2]` — 2 subscript operations.
- [ ] Negative subscript index: `arr[-1]` — PG returns NULL for negative indices (1-indexed).
- [ ] `ANY`/`ALL` with subquery vs array — parser disambiguates by checking for SELECT keyword.
- [ ] `ANY`/`ALL` with `LIKE`/`ILIKE` — `'foo' LIKE ANY(tags)` works.
- [ ] GIN index on empty array — no keys extracted, row not in index for containment queries.
- [ ] GIN index maintenance on UPDATE — old elements removed, new elements added.
- [ ] Array display via `information_schema.COLUMNS` — `DATA_TYPE = 'ARRAY'`, element info in separate column.
- [ ] `SHOW CREATE TABLE` shows `TEXT[]`, `INT[][]` as originally declared.

## On-disk format

```
Array blob stored as varlena in row codec (like TEXT/BYTES):

  Offset  Size    Field              Description
  0       4       total_len          u32 LE — total bytes including this header
  4       4       ndim               i32 — number of dimensions (0-6)
  8       4       dataoffset         i32 — 0 = no null bitmap; else offset to first element
  12      1       elemtype           u8 — ColumnType discriminant of leaf elements
  13      1       flags              u8 — reserved (bit0: 1=has lower bounds explicitly set)
  14      2       _pad               u16 — alignment padding
  16      ndim*4  dims               i32[ndim] LE — dimension lengths
  16+ndim*4  ndim*4  lbound           i32[ndim] LE — lower bounds (default all 1)
  ...     ...     null_bitmap        ceil(nitems/8) bytes (present if dataoffset != 0)
  ...     ...     elements           Packed element values, row-major order

Total header size: 16 + ndim*8 bytes
```

**Catalog ColumnDef extension:**
```
After existing trailing fields (collation, enum_type_name):
  [array_element_type_len: u8]   = 1 if array, 0 if not
  [array_element_type: u8]       = ColumnType discriminant of leaf elements (present if len=1)
```

Old rows without this field: `array_element_type_len` reads as 0 (default byte), meaning not an array.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| Array encode (1D, 100 ints) | < 500ns | < 2µs |
| Array decode (1D, 100 ints) | < 500ns | < 2µs |
| array_length() | < 50ns | < 200ns |
| arr[n] subscript | < 100ns | < 500ns |
| @> contains (100 el vs 10 el query) | < 50µs | < 200µs |
| GIN index probe for @> | < 1ms | < 5ms |
| array_to_text (1000 el) | < 200µs | < 1ms |
| unnest() per row | < 1µs | < 5µs |

## Dependencies

- Depends on: Phase 20.3 (ENUMs) — closed
- Depends on: Phase 11.17 (GIN for JSONB) — reuse GinScan + gin_key_term infrastructure
- Depends on: Phase 11.25 (JSONB SRF) — unnest follows same FromClause pattern
- Blocks: Phase 20.14 (UNNEST) — this spec implements unnest()
- Blocks: Phase 20.18 (Composite types) — arrays of composites deferred there
- Blocks: Phase 29.11 (ARRAY_TO_STRING / STRING_TO_ARRAY) — implemented here

## Open questions

- [ ] Should `FLOAT[3][3]` size hints be enforced (error on insert with wrong size)
  or stored as documentation only? → **Store as documentation** (PG behavior: size hints
  are ignored; arrays are always variable-length)
- [ ] Should `array_agg()` accept DISTINCT? → **Yes** — PG supports it
- [ ] Should `ANY`/`ALL` work with the `IN` operator? → **No** — `IN` already handles
  lists and subqueries; `= ANY(array)` is the array equivalent
- [ ] Should `unnest` with multiple arrays produce an error on mismatched lengths
  or pad with NULLs? → **Error** (PG behavior: "argument lists must have equal lengths")

## Done criteria

- [ ] `DataType::Array(Box<DataType>)` and `Value::Array(Vec<Value>)` added
- [ ] `ColumnType::Array = 13` added with `array_element_type` trailing field on `ColumnDef`
- [ ] On-disk array blob format implemented in `codec.rs` (encode/decode/masked)
- [ ] `TEXT[]`, `INT[]`, `FLOAT[]`, etc. parseable in CREATE TABLE
- [ ] DDL roundtrip: SHOW CREATE TABLE preserves array type notation
- [ ] `ARRAY[e1, e2, ...]` constructor evaluable (flat and nested)
- [ ] All 12 scalar element types supported in arrays
- [ ] Multidimensional arrays (up to 6D) work end-to-end
- [ ] `array_to_text` / `text_to_array` roundtrip for all types
- [ ] Operators: `@>`, `<@`, `&&`, `||`, subscript `[n]`, `=`, `<>` all work
- [ ] Polymorphic dispatch: `@>` works for both arrays and JSONB
- [ ] All 17 functions implemented and tested
- [ ] `ANY(array)` and `ALL(array)` constructs parseable and evaluable
- [ ] `unnest(arr)` works as FROM-clause table function
- [ ] `unnest(arr1, arr2, ...)` multi-array zip works
- [ ] GIN index on array columns: CREATE, build, DML maintenance, planner probe
- [ ] GIN supports `@>`, `&&`, `<@`, `=` strategies
- [ ] `array_agg(expr)` aggregate with ORDER BY and DISTINCT
- [ ] MySQL wire protocol: arrays serialized as `{...}` text over both protocols
- [ ] `information_schema.COLUMNS` reports array type info
- [ ] Integration tests cover all edge cases (aim for 50+ tests)
- [ ] `tools/wire-test.py` updated with array DDL + DML + SELECT assertions
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` / `expect()` in production `src/`
- [ ] All `unsafe` blocks have `// SAFETY:` comments
- [ ] `docs/progreso.md` updated marking 20.4 as ✅
- [ ] `memory/project_state.md` updated

## References

- PostgreSQL array.h: `postgres/postgres/src/include/utils/array.h`
- PostgreSQL arrayfuncs.c: `postgres/postgres/src/backend/utils/adt/arrayfuncs.c`
- PostgreSQL ginarrayproc.c: `postgres/postgres/src/backend/access/gin/ginarrayproc.c`
- PostgreSQL docs: https://www.postgresql.org/docs/current/arrays.html
- PostgreSQL functions: https://postgresql.org/docs/18/functions-array.html
- DuckDB list_column_data.cpp: `duckdb/src/storage/table/list_column_data.cpp`
- Previous spec: `specs/fase-20/spec-20.3-enums.md`
- JSONB GIN precedent: `specs/fase-11/spec-11.17-jsonb-gin.md`
