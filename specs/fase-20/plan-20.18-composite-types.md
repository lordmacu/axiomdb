# Plan: 20.18 Composite / user-defined types

Phase: 20 — Types + import/export
Task: 20.18 — Composite user-defined types with binary packed storage and dot notation
Spec: specs/fase-20/spec-20.18-composite-types.md
Status: in-progress

## Summary

Six ordered steps, each producing a working commit. Steps 1–2 build the
foundation in `axiomdb-types` and `axiomdb-catalog`. Steps 3–4 add the DDL
surface (AST, parser, executor). Step 5 wires the analyzer (dot-notation →
`Expr::FieldAccess`) and the evaluator. Step 6 closes with integration tests
and wire smoke assertions. All changes follow the patterns established by
`Money` (20.17) and `Array` (20.4).

## Dependencies

Must be done first:
- [x] spec-20.18 approved
- [x] 20.17 MONEY merged to main (ColumnType::Money = 15 present)

Blocks (until this plan is done):
- [ ] 20.19 ltree

## Affected files

New files:
- `crates/axiomdb-catalog/src/schema_composite.rs` — CompositeTypeDef codec
- `crates/axiomdb-sql/tests/integration_composite.rs` — ≥ 20 integration tests

Modified files:
- `crates/axiomdb-types/src/value.rs` — `Value::Composite`
- `crates/axiomdb-types/src/types.rs` — `DataType::Composite`
- `crates/axiomdb-types/src/codec.rs` — encode/decode for Composite
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Composite = 16`
- `crates/axiomdb-catalog/src/schema_table.rs` — `ColumnDef::composite_type_name` + codec extension
- `crates/axiomdb-catalog/src/schema.rs` — re-export `CompositeTypeDef`
- `crates/axiomdb-catalog/src/bootstrap.rs` — `composite_types: u64` in `CatalogPageIds`
- `crates/axiomdb-catalog/src/reader.rs` — `get_composite_type`, `list_composite_types`
- `crates/axiomdb-catalog/src/writer.rs` — `create_composite_type`, `delete_composite_type`, `SYSTEM_TABLE_COMPOSITE_TYPES`
- `crates/axiomdb-catalog/src/lib.rs` — re-exports
- `crates/axiomdb-storage/src/meta.rs` — `CATALOG_COMPOSITE_TYPES_ROOT_BODY_OFFSET = 200`
- `crates/axiomdb-storage/src/lib.rs` — re-export new constant
- `crates/axiomdb-sql/src/expr.rs` — `Expr::Row`, `Expr::FieldAccess`
- `crates/axiomdb-sql/src/ast.rs` — `CreateCompositeTypeStmt`, `DropTypeStmt`, `Stmt` variants
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `CREATE TYPE … AS (…)`, `DROP TYPE`
- `crates/axiomdb-sql/src/parser/expr.rs` — parse `ROW(…)` expression
- `crates/axiomdb-sql/src/table.rs` — composite arm in `column_data_types`
- `crates/axiomdb-sql/src/executor/*.rs` — CREATE/DROP TYPE executor, INSERT ROW, SHOW COLUMNS
- `crates/axiomdb-sql/src/analyzer_bind.rs` — composite-column dot access → `Expr::FieldAccess`
- `crates/axiomdb-sql/src/eval/core.rs` — eval `Expr::FieldAccess` + `Expr::Row`
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — no change needed (ROW handled in executor)
- `tools/wire-test.py` — 4 wire assertions

---

## Step 1 — Foundation: Value::Composite + DataType::Composite + codec

**Goal:** Add the new type variants and their binary codec so all downstream layers can compile.
**Files:** `crates/axiomdb-types/src/{value,types,codec}.rs`, `crates/axiomdb-catalog/src/schema_database.rs`

### Test to add

```rust
// crates/axiomdb-types/src/codec.rs — in the existing roundtrip tests
#[test]
fn encode_decode_composite_two_fields() {
    use crate::types::DataType;
    use crate::value::Value;

    let schema = vec![
        DataType::Composite(vec![
            ("street".into(), DataType::Text),
            ("zip".into(), DataType::Text),
        ]),
    ];
    let row = vec![Value::Composite(vec![
        Value::Text("123 Main".into()),
        Value::Text("10001".into()),
    ])];
    let bytes = encode_row(&row, &schema).unwrap();
    let decoded = decode_row(&bytes, &schema).unwrap();
    assert_eq!(decoded, row);
}

#[test]
fn encode_decode_composite_null_column() {
    // A NULL composite: the outer null-bitmap bit is set; FieldAccess returns NULL.
    let schema = vec![DataType::Composite(vec![("x".into(), DataType::Int)])];
    let row = vec![Value::Null];
    let bytes = encode_row(&row, &schema).unwrap();
    let decoded = decode_row(&bytes, &schema).unwrap();
    assert_eq!(decoded, vec![Value::Null]);
}

#[test]
fn encode_decode_composite_null_inner_field() {
    // A non-null composite containing a NULL field.
    let schema = vec![DataType::Composite(vec![("x".into(), DataType::Int)])];
    let row = vec![Value::Composite(vec![Value::Null])];
    let bytes = encode_row(&row, &schema).unwrap();
    let decoded = decode_row(&bytes, &schema).unwrap();
    assert_eq!(decoded, row);
}

#[test]
fn value_composite_display() {
    let v = Value::Composite(vec![
        Value::Text("NYC".into()),
        Value::Int(10001),
    ]);
    assert_eq!(v.to_string(), "(NYC,10001)");
}

#[test]
fn column_type_composite_roundtrip() {
    // axiomdb-catalog: ColumnType::Composite = 16
    use axiomdb_catalog::schema::ColumnType;
    assert_eq!(u8::from(ColumnType::Composite), 16u8);
    assert_eq!(ColumnType::try_from(16u8).unwrap(), ColumnType::Composite);
}
```

### Implementation outline

```rust
// crates/axiomdb-types/src/value.rs
// Add after Value::Money:
/// SQL composite (user-defined) type value (Phase 20.18).
/// Fields are in declaration order; a NULL field slot is Value::Null.
Composite(Vec<Value>),

// Display: (v1,v2,…)  — no spaces, parenthesized
Self::Composite(fields) => {
    write!(f, "(")?;
    for (i, v) in fields.iter().enumerate() {
        if i > 0 { write!(f, ",")?; }
        write!(f, "{}", v)?;
    }
    write!(f, ")")
}

// crates/axiomdb-types/src/types.rs
// Add after DataType::Range:
/// SQL composite type (Phase 20.18).
/// Inner vec holds (field_name, field_type) in declaration order.
/// Name is used by the analyzer for dot-notation disambiguation.
Composite(Vec<(String, DataType)>),

// DataType::name():
Self::Composite(_) => "COMPOSITE".into(),

// crates/axiomdb-types/src/codec.rs — encode_row:
// After Money arm, before Null unreachable:
(Value::Composite(fields), DataType::Composite(field_defs)) => {
    // encode the fields as a nested row
    let inner_schema: Vec<DataType> = field_defs.iter().map(|(_, dt)| dt.clone()).collect();
    let inner_bytes = encode_row(fields, &inner_schema)?;
    // write [u32 LE data_len][inner_bytes]
    let len = inner_bytes.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&inner_bytes);
}
// Mismatch arm for wrong pairing handled by existing outer mismatch check.

// decode_row — after Money arm:
DataType::Composite(field_defs) => {
    // read [u32 LE data_len][inner_bytes]
    ensure_bytes(bytes, pos, 4)?;
    let data_len = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap()) as usize;
    pos += 4;
    ensure_bytes(bytes, pos, data_len)?;
    let inner_bytes = &bytes[pos..pos + data_len];
    pos += data_len;
    let inner_schema: Vec<DataType> = field_defs.iter().map(|(_, dt)| dt.clone()).collect();
    let inner_values = decode_row(inner_bytes, &inner_schema)?;
    Value::Composite(inner_values)
}

// fixed_encoded_size (for skip-mask path):
DataType::Composite(_) => None,  // variable length

// crates/axiomdb-catalog/src/schema_database.rs — ColumnType enum:
// Add after Money = 15:
Composite = 16,
```

### Verification

```bash
./tools/vm.sh test axiomdb-types
./tools/vm.sh clippy axiomdb-types
```

### Commit

```
feat(fase-20): step 1 — Value::Composite + DataType::Composite + codec
```

---

## Step 2 — Catalog layer: CompositeTypeDef, ColumnDef extension, reader/writer

**Goal:** Persist composite type definitions in a new catalog heap; extend ColumnDef to
store the composite type name; expose reader/writer API.
**Files:** `schema_composite.rs` (new), `schema_table.rs`, `schema_database.rs` (try_from test),
`bootstrap.rs`, `reader.rs`, `writer.rs`, `lib.rs`, `meta.rs` (storage), `schema.rs`

### Test to add

```rust
// crates/axiomdb-catalog/tests/composite_type_test.rs  (new file)
use axiomdb_catalog::{CompositeTypeDef, CompositeField};
use axiomdb_types::types::DataType;

#[test]
fn composite_type_def_roundtrip() {
    let def = CompositeTypeDef {
        schema_name: "public".into(),
        name: "address".into(),
        fields: vec![
            CompositeField { name: "street".into(), data_type: DataType::Text },
            CompositeField { name: "zip".into(),    data_type: DataType::Text },
            CompositeField { name: "code".into(),   data_type: DataType::Int  },
        ],
    };
    let bytes = def.to_bytes();
    let (decoded, consumed) = CompositeTypeDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn column_def_composite_type_name_roundtrip() {
    use axiomdb_catalog::schema::{ColumnDef, ColumnType};
    let col = ColumnDef {
        table_id: 1,
        col_idx: 0,
        name: "addr".into(),
        col_type: ColumnType::Composite,
        nullable: true,
        auto_increment: false,
        type_len: 0,
        is_fixed_len: false,
        default_expr: None,
        on_update_expr: None,
        generated_expr: None,
        collation: None,
        generated_stored: false,
        enum_type_name: None,
        array_element_type: None,
        array_ndims: None,
        composite_type_name: Some("address".into()),
    };
    let bytes = col.to_bytes();
    let (decoded, _) = ColumnDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.composite_type_name, Some("address".into()));
}
```

### Integration test (catalog CRUD)

```rust
// Added to existing integration tests in axiomdb-catalog/tests/ or a new file:
#[test]
fn create_get_delete_composite_type() {
    let dir = tempfile::tempdir().unwrap();
    // ... setup storage, bootstrap catalog ...
    // create
    writer.create_composite_type(def.clone()).unwrap();
    // get
    let got = reader.get_composite_type("public", "address").unwrap();
    assert_eq!(got, Some(def.clone()));
    // duplicate create → InvalidValue
    assert!(writer.create_composite_type(def.clone()).is_err());
    // delete
    writer.delete_composite_type("public", "address").unwrap();
    assert!(reader.get_composite_type("public", "address").unwrap().is_none());
    // delete non-existent without IF EXISTS → InvalidValue
    assert!(writer.delete_composite_type("public", "address").is_err());
}
```

### Implementation outline

```rust
// crates/axiomdb-catalog/src/schema_composite.rs  (new)
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeField {
    pub name:      String,
    pub data_type: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompositeTypeDef {
    pub schema_name: String,
    pub name:        String,
    pub fields:      Vec<CompositeField>,
}

// dtype_tag mapping (matches spec):
// 0=Bool,1=Int,2=BigInt,3=Real,4=Decimal,5=Text,6=Bytes,
// 7=Date,8=Timestamp,9=Uuid,10=Json,11=Jsonb,12=Money
// No Array/Range/Composite (rejected at CREATE TYPE time)

impl CompositeTypeDef {
    pub fn to_bytes(&self) -> Vec<u8> { ... }
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> { ... }
}

// crates/axiomdb-catalog/src/schema_table.rs — ColumnDef:
// Add field after array_ndims:
pub composite_type_name: Option<String>,

// to_bytes: append after array trailing fields:
if let Some(ctn) = &self.composite_type_name {
    if self.col_type == ColumnType::Composite {
        let b = ctn.as_bytes();
        buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
        buf.extend_from_slice(b);
    }
}

// from_bytes: after array trailing fields:
let composite_type_name = if col_type == ColumnType::Composite && bytes.len() > consumed + 1 {
    let len = u16::from_le_bytes(bytes[consumed..consumed+2].try_into().unwrap()) as usize;
    consumed += 2;
    if bytes.len() < consumed + len { return Err(err()); }
    let s = std::str::from_utf8(&bytes[consumed..consumed+len])
        .map_err(|_| err())?.to_string();
    consumed += len;
    Some(s)
} else { None };

// crates/axiomdb-storage/src/meta.rs:
pub const CATALOG_COMPOSITE_TYPES_ROOT_BODY_OFFSET: usize = 200;

// crates/axiomdb-catalog/src/bootstrap.rs — CatalogPageIds:
pub composite_types: u64,
// read/write at offset 200

// reader.rs — public methods:
pub fn get_composite_type(&mut self, schema: &str, name: &str)
    -> Result<Option<CompositeTypeDef>, DbError>
pub fn list_composite_types(&mut self) -> Result<Vec<CompositeTypeDef>, DbError>

// writer.rs — public methods:
pub fn create_composite_type(&mut self, def: CompositeTypeDef) -> Result<(), DbError>
// validates: ≥1 field, ≤255 fields, no duplicate names, no nested composite/array-of-composite
// returns InvalidValue if already exists

pub fn delete_composite_type(&mut self, schema: &str, name: &str, if_exists: bool)
    -> Result<(), DbError>
// returns InvalidValue if not found and if_exists = false
```

### Verification

```bash
./tools/vm.sh test axiomdb-catalog
./tools/vm.sh test axiomdb-storage
./tools/vm.sh clippy axiomdb-catalog
```

### Commit

```
feat(fase-20): step 2 — CompositeTypeDef catalog heap, ColumnDef extension, reader/writer
```

---

## Step 3 — DDL parser + AST

**Goal:** Add AST nodes for `CREATE TYPE … AS (…)` / `DROP TYPE [IF EXISTS]` and `ROW(…)`;
wire the parser to produce them. No executor yet — the new `Stmt` variants return
`NotImplemented` at execution time (so existing tests are unaffected).
**Files:** `crates/axiomdb-sql/src/ast.rs`, `crates/axiomdb-sql/src/expr.rs`,
`crates/axiomdb-sql/src/parser/ddl.rs`, `crates/axiomdb-sql/src/parser/expr.rs`

### Test to add

```rust
// crates/axiomdb-sql/tests/parser_composite.rs  (new, or inline)
use axiomdb_sql::parser::parse;
use axiomdb_sql::ast::{Stmt, CreateCompositeTypeStmt, DropTypeStmt};
use axiomdb_sql::expr::Expr;
use axiomdb_types::types::DataType;

#[test]
fn parse_create_composite_type() {
    let sql = "CREATE TYPE address AS (street TEXT, city TEXT, state CHAR(2), zip TEXT)";
    let stmt = parse(sql).unwrap();
    let Stmt::CreateCompositeType(s) = stmt else { panic!("wrong variant") };
    assert_eq!(s.name, "address");
    assert_eq!(s.schema, "public");
    assert_eq!(s.fields.len(), 4);
    assert_eq!(s.fields[0].name, "street");
}

#[test]
fn parse_drop_type_if_exists() {
    let sql = "DROP TYPE IF EXISTS address";
    let stmt = parse(sql).unwrap();
    let Stmt::DropType(s) = stmt else { panic!("wrong variant") };
    assert!(s.if_exists);
    assert_eq!(s.name, "address");
}

#[test]
fn parse_row_constructor() {
    let sql = "SELECT ROW('Main', 'NYC')";
    // After parse, the ROW(…) is in the SELECT list as Expr::Row
    // (analysis/execution deferred)
    let stmt = parse(sql).unwrap();
    // Just check it parses without error.
    drop(stmt);
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/ast.rs — new structs:

pub struct CompositeTypeField {
    pub name:      String,
    pub data_type: DataType,
    pub type_len:  u16,  // for CHAR(N)/VARCHAR(N)
}

pub struct CreateCompositeTypeStmt {
    pub schema: String,   // default "public"
    pub name:   String,
    pub fields: Vec<CompositeTypeField>,
}

pub struct DropTypeStmt {
    pub schema:    String,
    pub name:      String,
    pub if_exists: bool,
}

// Add to Stmt enum:
CreateCompositeType(CreateCompositeTypeStmt),
DropType(DropTypeStmt),

// crates/axiomdb-sql/src/expr.rs — add after existing variants:

/// `ROW(e1, e2, …)` constructor (Phase 20.18). Used in INSERT VALUES.
/// Resolved to Value::Composite at INSERT evaluation time.
Row(Vec<Expr>),

/// Composite field access (Phase 20.18). Emitted by the analyzer when
/// `table.column` resolves to a composite column + field offset.
/// col_idx  = position of the composite column in the combined row
/// field_idx = position of the field within the composite value's fields vec
FieldAccess { col_idx: usize, field_idx: usize },

// crates/axiomdb-sql/src/parser/ddl.rs — parse_create_type:
// Triggered by: CREATE TYPE <name> AS (<field> <type>, ...)
// Parses field list like CREATE TABLE column list (reuse parse_data_type helper)

// crates/axiomdb-sql/src/parser/expr.rs — parse_row:
// Triggered by: ROW keyword followed by '('
// Parse comma-separated expressions, return Expr::Row(exprs)
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test parser_composite
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-20): step 3 — AST + parser for CREATE TYPE, DROP TYPE, ROW(…)
```

---

## Step 4 — DDL executor + CREATE TABLE composite column + INSERT ROW

**Goal:** Execute `CREATE TYPE` / `DROP TYPE`; resolve composite column type in `CREATE TABLE`;
evaluate `ROW(…)` during INSERT to produce `Value::Composite`.
**Files:** executor DDL files (include! pattern), `table.rs`, `parser/ddl.rs` (CREATE TABLE parsing)

### Test to add

```rust
// Part of integration_composite.rs (written fully in Step 6; these tests drive Step 4)

// Test: CREATE TYPE + CREATE TABLE + INSERT ROW + SELECT * (returns composite value)
fn ddl_create_and_insert(ctx: &mut SessionContext) {
    ok!(ctx, "CREATE TYPE address AS (street TEXT, city TEXT, state CHAR(2), zip TEXT)");
    ok!(ctx, "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, home address)");
    ok!(ctx, "INSERT INTO customers VALUES (1, 'Alice', ROW('123 Main', 'NYC', 'NY', '10001'))");
    let rows = rows!(ctx, "SELECT * FROM customers");
    assert_eq!(rows.len(), 1);
    // home column displays as (123 Main,NYC,NY,10001)
    assert_eq!(rows[0][2], "(123 Main,NYC,NY,10001)");
}

// Test: error cases
fn ddl_duplicate_type(ctx) → InvalidValue
fn ddl_drop_nonexistent(ctx) → InvalidValue
fn ddl_row_arity_mismatch(ctx) → TypeMismatch
fn ddl_create_table_unknown_type(ctx) → TypeMismatch
fn ddl_create_type_duplicate_field(ctx) → InvalidValue
fn ddl_create_type_zero_fields(ctx) → InvalidValue (rejected by parser or executor)
fn ddl_drop_if_exists_noop(ctx) → Ok (no error)
```

### Implementation outline

```rust
// Executor DDL (new arms in the include! exec dispatch, following Money/Holiday patterns):

Stmt::CreateCompositeType(s) => {
    // validate fields (≥1, ≤255, no dup names, no composite/array-of-composite)
    // build CompositeTypeDef
    // writer.create_composite_type(def)?
    // ctx.invalidate_all()
    Ok(ResultSet::empty())
}

Stmt::DropType(s) => {
    // writer.delete_composite_type(schema, name, if_exists)?
    // ctx.invalidate_all()
    Ok(ResultSet::empty())
}

// parser/ddl.rs — CREATE TABLE column type resolution:
// When parse_data_type returns DataType::UserDefined(name), resolve against catalog.
// Actually: parse_column_def already handles text type names for enum.
// Follow the enum pattern: if the type name is not a built-in keyword,
// try catalog.get_composite_type("public", name):
//   Ok(Some(_)) → (DataType::Composite(fields), ColumnType::Composite, composite_type_name=name)
//   Ok(None)    → check enum → if still not found, DbError::TypeMismatch { "unknown type 'name'" }
// Store composite_type_name in the ColumnDef for persistence.

// table.rs — column_data_types():
// Add Composite arm:
ColumnType::Composite => {
    // Composite DataType is resolved at table-open time by the executor
    // using the catalog. Return a placeholder; the real DataType is built
    // by resolve_composite_column_types() below.
    DataType::Composite(vec![])
}

// Add new function:
pub fn resolve_composite_column_types(
    columns: &[ColumnDef],
    catalog: &mut CatalogReader,
    snap: TransactionSnapshot,
) -> Result<Vec<DataType>, DbError> {
    columns.iter().map(|c| {
        if c.col_type == ColumnType::Composite {
            let type_name = c.composite_type_name.as_deref().unwrap_or("");
            let def = catalog.get_composite_type("public", type_name)?
                .ok_or_else(|| DbError::TypeMismatch {
                    expected: format!("known composite type"),
                    got: type_name.to_string(),
                })?;
            Ok(DataType::Composite(
                def.fields.iter()
                    .map(|f| (f.name.clone(), f.data_type.clone()))
                    .collect()
            ))
        } else {
            Ok(column_type_to_data_type(c.col_type))
        }
    }).collect()
}

// Executor scan_table path: replace column_data_types() with resolve_composite_column_types()
// for tables that contain at least one composite column. Use the existing catalog reader
// already threaded through the executor.

// INSERT ROW evaluation (in executor/dml.rs or eval/core.rs):
// When coercing a value against DataType::Composite for an INSERT column:
//   if the value is Expr::Row(exprs), evaluate each expr, check arity,
//   coerce each field value, build Value::Composite(fields)
// This is done in the INSERT executor, not in pure eval, because it needs
// the target DataType to know expected field count + types.
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-20): step 4 — DDL executor CREATE/DROP TYPE + CREATE TABLE + INSERT ROW
```

---

## Step 5 — Analyzer dot-notation + evaluator FieldAccess

**Goal:** Teach the analyzer to rewrite `home.city` (when `home` is a composite column,
not a table alias) to `Expr::FieldAccess { col_idx, field_idx }`. Evaluate
`Expr::FieldAccess` and `Expr::Row` at runtime.
**Files:** `crates/axiomdb-sql/src/analyzer_bind.rs`, `crates/axiomdb-sql/src/eval/core.rs`

### Test to add

```rust
// Driven by integration_composite.rs tests (Step 6 writes them fully; these drive Step 5):

// SELECT home.city FROM customers
// → col 2 (home) is composite, field "city" is index 1
// → FieldAccess { col_idx: 2, field_idx: 1 } → Value::Text("NYC")

// WHERE home.state = 'NY'
// → FieldAccess used in filter predicate

// ORDER BY home.zip
// → FieldAccess in sort key

// NULL composite: SELECT home.city WHERE home IS NULL → NULL
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/analyzer_bind.rs

// In resolve_column_with_def, after find_table(q) fails (TableNotFound branch):
// Check if any table has a composite column named `q`.
// If found, look up field `field` in the composite type definition.
// The BindContext needs composite type info: add a field
//   composite_type_map: HashMap<usize, Vec<(String, DataType)>>
// keyed by global col_idx, populated when building BoundTable entries.
//
// When building BoundTable from catalog columns, for each ColumnType::Composite column:
//   fetch the CompositeTypeDef (via catalog reader threaded through analyze_stmt)
//   store fields in composite_type_map[col_idx]
//
// Modified resolve_column_with_def signature variant for composite:
//   fn try_resolve_composite_field(&self, qualifier: &str, field: &str)
//       -> Option<(usize, usize)>  // (col_idx, field_idx)

// Then in the expression binder, when Column resolution returns TableNotFound
// with a dotted name:
//   if let Some((col_idx, field_idx)) = ctx.try_resolve_composite_field(q, field) {
//       return Ok(Expr::FieldAccess { col_idx, field_idx });
//   }
//   // otherwise propagate TableNotFound (for real table-not-found)

// crates/axiomdb-sql/src/eval/core.rs — eval():

Expr::FieldAccess { col_idx, field_idx } => {
    let composite_val = row.get(*col_idx).cloned().unwrap_or(Value::Null);
    match composite_val {
        Value::Null => Ok(Value::Null),
        Value::Composite(fields) => {
            Ok(fields.get(*field_idx).cloned().unwrap_or(Value::Null))
        }
        _ => Err(DbError::TypeMismatch {
            expected: "Composite".into(),
            got: composite_val.variant_name().into(),
        }),
    }
}

Expr::Row(exprs) => {
    // Pure eval path (not inside INSERT coercion): evaluate all sub-exprs
    // and return Value::Composite. Type checking deferred to executor.
    let mut values = Vec::with_capacity(exprs.len());
    for e in exprs {
        values.push(eval(e, row)?);
    }
    Ok(Value::Composite(values))
}

// SHOW COLUMNS / information_schema extensions:
// ColumnType::Composite → display as the composite type name (e.g. "address")
// Add arm to ddl_show.rs and information_schema_exec.rs
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-20): step 5 — analyzer dot-notation → FieldAccess, eval FieldAccess + Row
```

---

## Step 6 — Integration tests + wire smoke + close

**Goal:** ≥ 20 integration tests in `integration_composite.rs`; 4 wire assertions; full
workspace test suite and clippy clean; phase docs updated.
**Files:** `crates/axiomdb-sql/tests/integration_composite.rs` (new), `tools/wire-test.py`,
`docs/fase-20.md`, `docs/progreso.md`, `docs-site/…`

### Tests to write

1. `create_and_drop_composite_type` — basic DDL roundtrip
2. `create_type_duplicate_field_error` — InvalidValue
3. `create_type_zero_fields_error` — InvalidValue
4. `create_type_already_exists_error` — InvalidValue
5. `drop_type_not_exists_error` — InvalidValue
6. `drop_type_if_exists_noop` — Ok (no error)
7. `create_table_composite_column` — column accepted
8. `create_table_unknown_type_error` — TypeMismatch
9. `insert_row_literal` — ROW(…) stores correctly
10. `insert_row_arity_mismatch_error` — TypeMismatch
11. `insert_null_composite` — NULL composite value
12. `select_field_access_city` — `addr.city` returns correct value
13. `select_field_access_state` — `addr.state`
14. `where_field_access_filter` — `WHERE addr.state = 'NY'`
15. `order_by_field_access` — `ORDER BY addr.zip`
16. `select_star_returns_whole_composite` — SELECT * shows whole composite value
17. `field_access_on_null_composite` — returns NULL
18. `composite_with_null_inner_field` — inner NULL propagates
19. `multiple_composite_columns` — two composite columns in same table
20. `create_type_same_name_different_schema` — two schemas coexist
21. `display_format` — `(v1,v2,v3)` form matches spec
22. `row_constructor_in_where_clause` — rejected gracefully (or supported)
23. Bonus: field in ORDER BY returns correct sort order

### Wire assertions (4)

```python
# [20.18a] CREATE TYPE + CREATE TABLE + INSERT + SELECT * (composite value display)
# [20.18b] dot-notation field access: SELECT addr.city FROM …
# [20.18c] WHERE addr.state filter
# [20.18d] DROP TYPE removes from catalog; subsequent CREATE TABLE with that type → error
```

### Closing protocol

```bash
./tools/vm.sh test --workspace   # all ~4200 tests pass
./tools/vm.sh clippy             # -D warnings clean
./tools/vm.sh fmt-check          # cargo fmt --check clean
pkill axiomdb-server; cargo build -p axiomdb-server; python3 tools/wire-test.py
```

Update:
- `docs/fase-20.md` — Subphase 20.18 section
- `docs/progreso.md` — mark 20.18 ✅
- `docs-site/src/user-guide/sql-reference/ddl.md` — CREATE TYPE syntax
- `docs-site/src/internals/catalog.md` — composite heap + binary format
- `memory/project_state.md` — 20.18 closed, next 20.19
- `memory/architecture.md` — CompositeTypeDef, meta offset 200

### Final commit

```
feat(fase-20): complete 20.18 — composite user-defined types

Implements specs/fase-20/spec-20.18-composite-types.md
Plan: specs/fase-20/plan-20.18-composite-types.md
Tests: 23 integration tests + 4 wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `column_data_types()` called from many paths without catalog access | high | `resolve_composite_column_types()` replaces it only in scan paths; composite tables fall back to resolve at open time |
| Analyzer threading catalog through to BindContext | medium | Follow same pattern as enum type resolution (check existing code in analyze_stmt) |
| ROW(…) in WHERE / ORDER BY (not just INSERT) | low | Eval path already handles Expr::Row; type check at coercion point |
| Nested composite (rejected at CREATE TYPE) — test coverage | low | Add explicit test for the rejection error |

## Rollback plan

If abandoned mid-way:
1. `git reset --hard 225e6ba4` (last stable commit before this plan)
2. Leave partial work on branch `abandoned/plan-composite-20.18-<date>`
3. Update spec status back to `approved` (not `implemented`)

## Estimated effort

Total: ~5–6 hours
- Step 1 (types + codec): 45 min
- Step 2 (catalog): 90 min
- Step 3 (parser + AST): 60 min
- Step 4 (executor): 90 min
- Step 5 (analyzer + eval): 60 min
- Step 6 (tests + wire + docs): 60 min
