# Spec: 20.18 Composite / user-defined types

Phase: 20 — Types + import/export
Task: 20.18 — Composite user-defined types with binary packed storage and dot notation
Status: approved

## Context

AxiomDB already supports enum types (20.3), arrays (20.4), range types (20.13), and the
MONEY type (20.17). Composite types are the next user-defined type, allowing multiple
named fields of different scalar types to be stored as a single column value. They touch
every layer of the stack: `axiomdb-types` (value + codec), `axiomdb-catalog` (catalog heap
+ ColumnDef extension), `axiomdb-sql` (parser, analyzer, executor, eval). The feature
closes the gap between AxiomDB and PostgreSQL's `CREATE TYPE … AS (…)` DDL, which is
required for domain-modeling workloads (addresses, coordinates, contact info).

## Goal

Add `CREATE TYPE name AS (field1 type1, field2 type2, …)` with column-type usage,
`ROW(…)` literal constructor, and dot-notation field access in queries.

## Non-goals

- Nested composite types (composite field of composite type) — deferred; raise a clear error
- `ALTER TYPE … ADD ATTRIBUTE / DROP ATTRIBUTE` — deferred to a later subphase
- Composite types as function parameters or return types
- Composite types in array columns (`composite_type[]`)
- Composite type comparison with `=` / `<` / `>` — field equality via `=` is supported,
  tuple ordering is not
- `ROW` constructor in SELECT list (returning a composite value) — only used in INSERT VALUES

## Behavior

### DDL

```sql
-- Create a composite type
CREATE TYPE address AS (
    street TEXT,
    city   TEXT,
    state  CHAR(2),
    zip    TEXT
);

-- Drop a composite type
DROP TYPE address;
DROP TYPE IF EXISTS address;
```

### Column type in CREATE TABLE

```sql
CREATE TABLE customers (
    id      INT PRIMARY KEY,
    name    TEXT NOT NULL,
    home    address
);
```

`home` stores a full `address` composite value on each row.

### INSERT with ROW constructor

```sql
INSERT INTO customers VALUES (1, 'Alice', ROW('123 Main', 'NYC', 'NY', '10001'));
```

`ROW(e1, e2, …)` evaluates each expression and packs it into a `Value::Composite`.
The arity must match the composite type's field count; each value must be coercible
to the corresponding field type.

### SELECT with dot notation

```sql
SELECT home.city  FROM customers;
SELECT home.state FROM customers WHERE home.state = 'NY';
```

`home.city` is parsed as `Expr::Column { name: "home.city" }`. The analyzer detects
that the qualifier `home` resolves to a composite column, looks up the field `city`
in the composite type definition, and rewrites the node to
`Expr::FieldAccess { col_idx: <home_col_idx>, field_idx: <city_field_idx> }`.

### Display

`Value::Composite(fields)` displays as `(v1, v2, v3, …)` — a parenthesized
comma-separated list of the field values' Display representations.

### Public API — key types

```rust
// axiomdb-catalog/src/schema_composite.rs
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeTypeDef {
    pub schema_name: String,
    pub name:        String,
    pub fields:      Vec<CompositeField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompositeField {
    pub name:      String,
    pub data_type: DataType,
}

impl CompositeTypeDef {
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError>;
}

// axiomdb-catalog: CatalogWriter
pub fn create_composite_type(&mut self, def: CompositeTypeDef) -> Result<(), DbError>;
pub fn delete_composite_type(&mut self, schema: &str, name: &str) -> Result<bool, DbError>;

// axiomdb-catalog: CatalogReader
pub fn get_composite_type(&mut self, schema: &str, name: &str)
    -> Result<Option<CompositeTypeDef>, DbError>;
pub fn list_composite_types(&mut self) -> Result<Vec<CompositeTypeDef>, DbError>;

// axiomdb-types/src/value.rs — new variant (after Money)
Value::Composite(Vec<Value>)   // ordered field values

// axiomdb-types/src/types.rs — new variant (after Money)
DataType::Composite(Vec<(String, DataType)>)  // (field_name, field_type)

// axiomdb-sql/src/expr.rs — new Expr variant
Expr::FieldAccess { col_idx: usize, field_idx: usize }

// axiomdb-sql/src/ast.rs — new Expr constructor
Expr::Row(Vec<Expr>)  // ROW(e1, e2, …) constructor — resolved to Composite at INSERT
```

### Semantics

**CREATE TYPE:**
- Validates: no duplicate field names (case-insensitive), no field of `DataType::Composite`
  (no nesting), no field of `DataType::Array` of composite (no nesting), ≥ 1 field,
  ≤ 255 fields, field names ≤ 255 bytes, type name ≤ 255 bytes, schema name ≤ 255 bytes.
- Rejects creation if a type with the same `(schema, name)` already exists.
- Default schema is `"public"` when none is specified.

**DROP TYPE:**
- Removes the composite type definition from the catalog.
- If `IF EXISTS` is omitted and the type does not exist, returns `DbError::InvalidValue`.
- Does NOT check whether any table column still uses this type (deferred enforcement).

**Column type:**
- When the analyzer sees a column declared as `address` in `CREATE TABLE`, it resolves
  the type name against the catalog (using the current database's default schema first,
  then `public`). Stores `ColumnType::Composite` + `composite_type_name` in `ColumnDef`.
- When loading column definitions, the executor reconstructs
  `DataType::Composite(fields)` from the `CompositeTypeDef`.

**INSERT:**
- `ROW(e1, …)` is parsed as `Expr::Row(Vec<Expr>)`. During INSERT value evaluation,
  the executor coerces `Expr::Row` against the target column's `DataType::Composite`:
  arity check → field-wise coercion → `Value::Composite(evaluated_fields)`.
- NULL propagation: `ROW(NULL, 'NYC', …)` stores `Value::Null` in the first field slot.
  The composite value itself is never NULL unless the entire column is set to NULL.

**SELECT field access:**
- `Expr::FieldAccess { col_idx, field_idx }` evaluates by decoding the composite
  value from the row at `col_idx`, then returning `fields[field_idx]` (or `Value::Null`
  if the composite value itself is NULL).

### Error cases

| Situation | Expected error | Message pattern |
|---|---|---|
| CREATE TYPE with duplicate field name | `DbError::InvalidValue` | `"duplicate field name 'city' in composite type"` |
| CREATE TYPE with nested composite field | `DbError::InvalidValue` | `"composite type fields cannot themselves be composite"` |
| CREATE TYPE with zero fields | `DbError::InvalidValue` | `"composite type must have at least one field"` |
| CREATE TYPE with name that already exists | `DbError::InvalidValue` | `"composite type 'public.address' already exists"` |
| DROP TYPE that does not exist (no IF EXISTS) | `DbError::InvalidValue` | `"composite type 'public.address' does not exist"` |
| CREATE TABLE column using unknown type | `DbError::TypeMismatch` | `"unknown type 'address'"` |
| INSERT ROW(…) arity mismatch | `DbError::TypeMismatch` | `"ROW() has N values but type expects M"` |
| INSERT ROW(…) field type mismatch | `DbError::TypeMismatch` | existing coercion error |
| Dot access on non-composite column | `DbError::ColumnNotFound` | existing column-not-found |
| Dot access with unknown field name | `DbError::ColumnNotFound` | `"field 'zzz' not found in composite type 'address'"` |

## Edge cases

- [ ] Composite column set to NULL (the whole value is null; field access returns NULL)
- [ ] Field value is NULL inside a non-null composite
- [ ] Field name collision: `home.city` where `home` is both a table alias and a composite column → table alias wins (standard SQL precedence)
- [ ] Type name = SQL keyword (e.g., `CREATE TYPE date AS (...)`) — should work if quoted
- [ ] DROP TYPE used by an existing column — no error at DROP time (deferred enforcement); reads of those columns will fail gracefully
- [ ] CREATE TYPE with the same name in a different schema — both coexist; lookup uses current search path
- [ ] Composite in WHERE: `WHERE home.state = 'NY'` works via FieldAccess
- [ ] Composite in ORDER BY: `ORDER BY home.zip` works via FieldAccess
- [ ] Composite SELECT * expansion: `SELECT * FROM customers` returns the whole composite value for `home`, not its sub-fields

## On-disk format

### CompositeTypeDef binary codec

```
[schema_len : u8][schema_utf8 : schema_len bytes]
[name_len   : u8][name_utf8   : name_len bytes  ]
[field_count: u8]
for each field:
    [field_name_len: u8][field_name_utf8: field_name_len bytes]
    [dtype_tag     : u8]                  // DataType discriminant (see below)
    [dtype_payload : variable]            // only for parameterized types
```

`dtype_tag` values (must match `DataType`):
- 0 = Bool, 1 = Int, 2 = BigInt, 3 = Real, 4 = Decimal, 5 = Text, 6 = Bytes,
  7 = Date, 8 = Timestamp, 9 = Uuid, 10 = Json, 11 = Jsonb, 12 = Money
- No Array, Range, or Composite fields (rejected at CREATE TYPE)

### Value::Composite on-disk codec (in row pages)

```
[data_len : u32 LE]                     // byte count of the packed field data below
[field_data: data_len bytes]            // encode_row(field_values, field_types)
```

`field_data` uses the existing `encode_row` / `decode_row` codec with the
`DataType` slice from `DataType::Composite(fields)`. Variable-length fields
(Text, Bytes, Json) carry their own length prefixes inside `field_data`.

### ColumnDef extension

`ColumnType::Composite = 16`. A new trailing extension is appended after
`array_element_type` in `ColumnDef::to_bytes()`:

```
[composite_type_len : u16 LE][composite_type_utf8 : composite_type_len bytes]
```

Presence is signaled by a new `flags bit8` (requires flags to be u16) or by
extending the trailing-extension chain with a length-prefixed sentinel.
**Implementation note:** Because the flags byte is only 1 byte today, use the
existing extension-chain pattern: the composite_type_name is present iff
`col_type == ColumnType::Composite`. This avoids a flags byte width change.

### Catalog heap

- Meta page offset: **200** (after exchange rates at 192)
- WAL table_id: **`u32::MAX - 17`**
- Heap named `axiom_composite_types`

## Performance budget

No specific budget — composite column reads add one `decode_row` call per field access.
Target: field access latency ≤ 2× a plain column read for a 4-field composite.

## Dependencies

- Depends on: 20.17 (MONEY) complete — codec pattern established
- Blocks: 20.19 (ltree) — no direct dependency

## Open questions

All resolved during brainstorm:
- Storage: binary packed (not JSON) — matches "more compact than separate columns" spec requirement
- Dot notation: analyzer-level disambiguation — parser stays unchanged
- Nested composites: deferred (clear error at CREATE TYPE)

## Done criteria

- [ ] `CREATE TYPE address AS (street TEXT, city TEXT, state CHAR(2), zip TEXT)` persists to catalog
- [ ] `DROP TYPE address` / `DROP TYPE IF EXISTS address` work correctly
- [ ] `CREATE TABLE t (id INT, addr address)` creates composite column
- [ ] `INSERT INTO t VALUES (1, ROW('Main St', 'NYC', 'NY', '10001'))` stores correctly
- [ ] `SELECT addr.city FROM t` returns correct field value
- [ ] `SELECT addr.city FROM t WHERE addr.state = 'NY'` filters correctly
- [ ] Wrong-arity `ROW(...)` returns `DbError::TypeMismatch`
- [ ] NULL composite value: field access returns NULL
- [ ] `cargo nextest run --workspace` passes (all ~4166 existing tests + new ones)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] ≥ 20 integration tests in `crates/axiomdb-sql/tests/integration_composite.rs`
- [ ] 4 wire assertions in `tools/wire-test.py`

## References

- PostgreSQL `typecmds.c:DefineCompositeType` — `research/postgres/src/backend/commands/typecmds.c:2568`
- AxiomDB enum type pattern: `crates/axiomdb-catalog/src/schema_enum.rs`
- AxiomDB analyzer column resolution: `crates/axiomdb-sql/src/analyzer_bind.rs:122`
- AxiomDB ColumnDef extension pattern: `crates/axiomdb-catalog/src/schema_table.rs:611`
- Phase doc: `docs/fase-20.md`
