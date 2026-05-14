# Plan: Schema Namespacing

Phase: 22b — Platform features
Task: 22b.4 — Schema Namespacing
Spec: specs/fase-22b/spec-22b.4-schema-namespacing.md
Status: done

## Summary

Three small deliverables built in sequence: (1) catalog writer `delete_schema()`
plus the `SchemaNotEmpty` error variant, (2) parser/AST for `DROP SCHEMA` and
`SHOW SCHEMAS` plus the executor functions, (3) `information_schema.SCHEMATA`
virtual table plus full integration tests and wire smoke. Each step produces a
clean, compilable commit.

## Dependencies

Must be done first:
- [x] spec-22b.4-schema-namespacing.md approved

Blocks:
- nothing

## Affected files

New files:
- `tests/integration_schema_namespacing.rs` — integration test suite

Modified files:
- `crates/axiomdb-core/src/error.rs` — add `SchemaNotEmpty`
- `crates/axiomdb-network/src/mysql/error.rs` — wire SchemaNotEmpty to MySQL 1010
- `crates/axiomdb-catalog/src/writer.rs` — add `delete_schema()`
- `crates/axiomdb-sql/src/ast.rs` — add `DropSchemaStmt`, `ShowSchemasStmt`, `Stmt` arms
- `crates/axiomdb-sql/src/parser/ddl.rs` — add `parse_drop_schema()`
- `crates/axiomdb-sql/src/parser/mod.rs` — `parse_drop()` + `parse_show()` arms
- `crates/axiomdb-sql/src/plan_deps.rs` — new Stmt arms
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — `execute_drop_schema()`, `execute_show_schemas()`
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — dispatch new stmts
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — new Stmt arms
- `crates/axiomdb-sql/src/lib.rs` — re-export new AST nodes
- `crates/axiomdb-sql/src/information_schema.rs` — `IS_SCHEMATA_COLS`, register in `is_table_cols()`
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — `"schemata"` arm + row generator
- `tools/wire-test.py` — `[22b.4 schema namespacing]` section

---

## Step 1 — SchemaNotEmpty error + catalog delete_schema

**Goal:** New error variant + catalog writer method; everything the executor needs
to delete a schema row.

**Files:**
- `crates/axiomdb-core/src/error.rs`
- `crates/axiomdb-network/src/mysql/error.rs`
- `crates/axiomdb-catalog/src/writer.rs`

### Test to add

```rust
// crates/axiomdb-catalog/src/writer.rs (unit test at bottom of file)
#[test]
fn test_delete_schema_roundtrip() {
    use crate::test_helpers::open_temp;
    let (storage, txn, mut conn) = open_temp();
    let mut w = CatalogWriter::new(&*storage, &txn, &mut conn).unwrap();
    w.create_schema("axiomdb", "myschema").unwrap();
    // schema exists now
    let snap = txn.snapshot();
    let mut r = CatalogReader::new(&*storage, snap).unwrap();
    assert!(r.schema_exists("axiomdb", "myschema").unwrap());
    // delete it
    let mut conn2 = txn.begin().unwrap();
    let mut w2 = CatalogWriter::new(&*storage, &txn, &mut conn2).unwrap();
    w2.delete_schema("axiomdb", "myschema").unwrap();
    txn.commit(conn2).unwrap();
    let snap2 = txn.snapshot();
    let mut r2 = CatalogReader::new(&*storage, snap2).unwrap();
    assert!(!r2.schema_exists("axiomdb", "myschema").unwrap());
}
```

### Implementation

**`crates/axiomdb-core/src/error.rs`** — add after `SchemaNotFound`:
```rust
#[error("schema '{name}' is not empty")]
SchemaNotEmpty { name: String },
```

**`crates/axiomdb-network/src/mysql/error.rs`** — add to the match:
```rust
DbError::SchemaNotEmpty { name } => (
    1010,
    b"HY000",
    format!("Can't drop schema '{name}'; schema is not empty"),
),
```

**`crates/axiomdb-catalog/src/writer.rs`** — add after `create_schema`:
```rust
/// Deletes the schema row from `axiom_schemas`.
/// Does nothing if the schema row does not exist (e.g. `public` never
/// explicitly created). Returns `true` if a row was deleted.
pub fn delete_schema(
    &mut self,
    database: &str,
    schema: &str,
) -> Result<bool, DbError> {
    let root = self.page_ids.schemas;
    if root == 0 {
        return Ok(false);
    }
    let txn_id = self.conn.txn_id;
    let snap = self.txn.active_snapshot(self.conn);
    let rows = HeapChain::scan_visible(self.storage, root, snap)?;
    for (page_id, slot_id, data) in rows {
        let (def, _) = crate::schema::SchemaDef::from_bytes(&data)?;
        if def.database_name == database && def.name == schema {
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
            let key = format!("{}\0{}", database, schema);
            self.txn.record_delete(
                self.conn,
                SYSTEM_TABLE_SCHEMAS,
                key.as_bytes(),
                &data,
                page_id,
                slot_id,
            )?;
            return Ok(true);
        }
    }
    Ok(false)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-catalog
./tools/vm.sh clippy -p axiomdb-catalog -- -D warnings
```

### Commit

```
feat(fase-22b): add SchemaNotEmpty error and catalog delete_schema

Step 1 of specs/fase-22b/plan-22b.4-schema-namespacing.md
```

---

## Step 2 — DROP SCHEMA + SHOW SCHEMAS parser, AST, executor

**Goal:** Full SQL surface for `DROP SCHEMA` and `SHOW SCHEMAS`.

**Files:**
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/ddl.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/plan_deps.rs`
- `crates/axiomdb-sql/src/executor/ddl_show.rs`
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs`
- `crates/axiomdb-sql/src/executor/exec_explain.rs`
- `crates/axiomdb-sql/src/lib.rs`

### Tests to add

```rust
// crates/axiomdb-sql/src/parser/tests (inline)
#[test]
fn parse_drop_schema_basic() {
    let stmt = parse_one("DROP SCHEMA myschema").unwrap();
    assert_eq!(stmt, Stmt::DropSchema(DropSchemaStmt {
        name: "myschema".into(), if_exists: false, cascade: false
    }));
}

#[test]
fn parse_drop_schema_if_exists_cascade() {
    let stmt = parse_one("DROP SCHEMA IF EXISTS s1 CASCADE").unwrap();
    assert_eq!(stmt, Stmt::DropSchema(DropSchemaStmt {
        name: "s1".into(), if_exists: true, cascade: true
    }));
}

#[test]
fn parse_show_schemas() {
    let stmt = parse_one("SHOW SCHEMAS").unwrap();
    assert_eq!(stmt, Stmt::ShowSchemas(ShowSchemasStmt { like_pattern: None }));
}

#[test]
fn parse_show_schemas_like() {
    let stmt = parse_one("SHOW SCHEMAS LIKE 'p%'").unwrap();
    assert_eq!(stmt, Stmt::ShowSchemas(ShowSchemasStmt {
        like_pattern: Some("p%".into())
    }));
}
```

### Implementation

**`crates/axiomdb-sql/src/ast.rs`** — add after `DropDatabaseStmt`:

```rust
/// `DROP SCHEMA [IF EXISTS] name [CASCADE | RESTRICT]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropSchemaStmt {
    pub name: String,
    pub if_exists: bool,
    pub cascade: bool,
}

/// `SHOW SCHEMAS [LIKE 'pattern']`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowSchemasStmt {
    pub like_pattern: Option<String>,
}
```

Add to the `Stmt` enum:
```rust
DropSchema(DropSchemaStmt),
ShowSchemas(ShowSchemasStmt),
```

**`crates/axiomdb-sql/src/parser/ddl.rs`** — add:

```rust
pub(crate) fn parse_drop_schema(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let name = p.parse_identifier()?;
    let cascade = if p.eat_ident_ci("CASCADE") {
        true
    } else {
        p.eat_ident_ci("RESTRICT"); // consume RESTRICT if present; it's the default
        false
    };
    Ok(Stmt::DropSchema(crate::ast::DropSchemaStmt { name, if_exists, cascade }))
}
```

**`crates/axiomdb-sql/src/parser/mod.rs`** — in `parse_drop()` add before the `other` arm:
```rust
Token::Schema => {
    self.advance();
    ddl::parse_drop_schema(self)
}
```

In the `SHOW` dispatch, add after `Token::Databases`:
```rust
Token::Ident(s) if s.eq_ignore_ascii_case("SCHEMAS") => {
    self.advance();
    let like_pattern = if self.eat(&Token::Like) {
        Some(self.parse_string_literal()?)
    } else {
        None
    };
    Ok(Stmt::ShowSchemas(crate::ast::ShowSchemasStmt { like_pattern }))
}
```

**`crates/axiomdb-sql/src/executor/ddl_show.rs`** — add two functions:

```rust
pub(crate) fn execute_drop_schema(
    stmt: DropSchemaStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    // Reject dropping information_schema.
    if stmt.name.eq_ignore_ascii_case("information_schema") {
        return Err(DbError::InvalidValue {
            reason: "cannot drop schema information_schema".into(),
        });
    }
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if !reader.schema_exists(database, &stmt.name)? {
        if stmt.if_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::SchemaNotFound { name: stmt.name });
    }
    if stmt.cascade {
        // Drop all tables in the schema first.
        let tables = reader.list_tables_in_database(database, &stmt.name)?;
        drop(reader); // release borrow before mutable write
        for table in tables {
            crate::executor::ddl_table::drop_table_fully(storage, txn, conn_txn, table.id)?;
        }
    } else {
        // RESTRICT: error if any tables exist.
        let tables = reader.list_tables_in_database(database, &stmt.name)?;
        if !tables.is_empty() {
            return Err(DbError::SchemaNotEmpty { name: stmt.name });
        }
        drop(reader);
    }
    CatalogWriter::new(storage, txn, conn_txn)?.delete_schema(database, &stmt.name)?;
    Ok(QueryResult::Affected { count: 0, last_insert_id: None })
}

pub(crate) fn execute_show_schemas(
    stmt: ShowSchemasStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &SessionContext,
) -> Result<QueryResult, DbError> {
    let database = ctx.effective_database();
    let snap = ctx.conn_txn.as_ref()
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());
    let mut reader = CatalogReader::new(storage, snap)?;
    let schemas = reader.list_schemas(database)?;
    let col = ColumnMeta::computed("Schema", DataType::Text);
    let rows: Vec<Row> = schemas
        .into_iter()
        .filter(|s| {
            if let Some(pat) = &stmt.like_pattern {
                crate::executor::eval::functions::like_matches(&s.name, pat)
            } else {
                true
            }
        })
        .map(|s| vec![Value::Text(s.name)])
        .collect();
    Ok(QueryResult::Rows { cols: vec![col], rows })
}
```

**`crates/axiomdb-sql/src/executor/exec_dispatch.rs`** — add dispatch arms:
```rust
Stmt::DropSchema(s) => execute_drop_schema(
    s, storage, txn, ctx.conn_txn.as_mut().expect("conn_txn"), ctx.effective_database()
),
Stmt::ShowSchemas(s) => execute_show_schemas(s, storage, txn, ctx),
```

**`crates/axiomdb-sql/src/executor/exec_explain.rs`** — add arms:
```rust
Stmt::DropSchema(s) =>
    execute_drop_schema(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME),
Stmt::ShowSchemas(s) => {
    // For EXPLAIN, just return empty — no interesting plan.
    let _ = s;
    Ok(QueryResult::Empty)
}
```

**`crates/axiomdb-sql/src/plan_deps.rs`** — add arms:
```rust
| Stmt::DropSchema(_)
| Stmt::ShowSchemas(_) => PlanDeps::default(),
```

**`crates/axiomdb-sql/src/lib.rs`** — add to re-exports:
```rust
DropSchemaStmt, ShowSchemasStmt,
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-22b): add DROP SCHEMA and SHOW SCHEMAS

Step 2 of specs/fase-22b/plan-22b.4-schema-namespacing.md
```

---

## Step 3 — information_schema.SCHEMATA + integration tests + wire smoke

**Goal:** Complete `information_schema.schemata` virtual table, integration tests
covering all spec edge cases, and wire smoke for the new feature.

**Files:**
- `crates/axiomdb-sql/src/information_schema.rs`
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs`
- `tests/integration_schema_namespacing.rs`
- `tools/wire-test.py`

### information_schema.SCHEMATA implementation

**`crates/axiomdb-sql/src/information_schema.rs`** — add constant and register:

```rust
/// Column names for `information_schema.SCHEMATA`.
pub static IS_SCHEMATA_COLS: &[(&str, ColumnType)] = &[
    ("CATALOG_NAME",               ColumnType::Text),
    ("SCHEMA_NAME",                ColumnType::Text),
    ("DEFAULT_CHARACTER_SET_NAME", ColumnType::Text),
    ("DEFAULT_COLLATION_NAME",     ColumnType::Text),
    ("SQL_PATH",                   ColumnType::Text),
];
```

In `is_table_cols()` add:
```rust
"schemata" => Some(IS_SCHEMATA_COLS),
```

In `make_is_catalog_columns()` (or wherever other IS tables are registered) add
the same `"schemata"` arm pointing to `IS_SCHEMATA_COLS`.

**`crates/axiomdb-sql/src/executor/information_schema_exec.rs`** — add arm in the
`match table_name` block and new generator function:

```rust
"schemata" => generate_is_schemata_rows(&mut reader, default_database)?,
```

```rust
/// `information_schema.SCHEMATA` — one row per schema in the current database.
/// Always includes `information_schema` itself as a virtual schema row.
fn generate_is_schemata_rows(
    reader: &mut CatalogReader,
    database: &str,
) -> Result<Vec<Row>, DbError> {
    let mut schemas = reader.list_schemas(database)?;
    // Add information_schema as a virtual row.
    schemas.push(axiomdb_catalog::schema::SchemaDef {
        database_name: database.to_string(),
        name: "information_schema".to_string(),
    });
    schemas.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(schemas
        .into_iter()
        .map(|s| {
            vec![
                Value::Text("def".into()),              // CATALOG_NAME
                Value::Text(s.name),                    // SCHEMA_NAME
                Value::Text("utf8mb4".into()),           // DEFAULT_CHARACTER_SET_NAME
                Value::Text("utf8mb4_general_ci".into()),// DEFAULT_COLLATION_NAME
                Value::Null,                             // SQL_PATH
            ]
        })
        .collect())
}
```

### Integration tests

```rust
// tests/integration_schema_namespacing.rs
// Tests: CREATE SCHEMA, schema.table DDL/DML, SET search_path,
//        DROP SCHEMA (RESTRICT/CASCADE/IF EXISTS), SHOW SCHEMAS,
//        information_schema.SCHEMATA

#[test]
fn schema_create_and_table_in_schema() { ... }

#[test]
fn schema_qualified_select() { ... }

#[test]
fn schema_qualified_insert_update_delete() { ... }

#[test]
fn set_search_path() { ... }

#[test]
fn drop_schema_restrict_rejects_nonempty() { ... }

#[test]
fn drop_schema_cascade_drops_tables() { ... }

#[test]
fn drop_schema_if_exists_silent() { ... }

#[test]
fn drop_schema_information_schema_rejected() { ... }

#[test]
fn drop_schema_public_then_recreate() { ... }

#[test]
fn show_schemas_lists_public() { ... }

#[test]
fn show_schemas_like_filter() { ... }

#[test]
fn information_schema_schemata() { ... }

#[test]
fn information_schema_schemata_includes_self() { ... }
```

### Wire smoke additions

Add to `tools/wire-test.py` a new section `[22b.4 schema namespacing]`:
```python
# CREATE SCHEMA + schema.table + DROP SCHEMA CASCADE
# SHOW SCHEMAS
# information_schema.SCHEMATA
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh test --test integration_schema_namespacing
./tools/vm.sh test --workspace
./tools/vm.sh clippy --workspace -- -D warnings
cargo fmt --check
# macOS: cargo build -p axiomdb-server && python3 tools/wire-test.py
```

### Commit

```
feat(fase-22b): complete 22b.4 schema namespacing

Implements specs/fase-22b/spec-22b.4-schema-namespacing.md
- DROP SCHEMA [IF EXISTS] [CASCADE|RESTRICT]
- SHOW SCHEMAS [LIKE pattern]
- information_schema.SCHEMATA virtual table
Tests: 13 integration tests, wire smoke [22b.4]
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `drop_table_fully` signature mismatch in CASCADE path | low | check signature before coding Step 2 |
| `like_matches` not accessible from ddl_show.rs | low | use inline LIKE check or move to shared util |
| `exec_explain.rs` has exhaustive match that requires all Stmt variants | medium | clippy will catch it immediately |

## Rollback plan

If abandoned mid-way: `git reset --hard <commit before Step 1>`.
All changes are additive (no format changes, no existing behavior modified).

## Estimated effort

Total: ~3 hours
- Step 1: 30 min
- Step 2: 1.5 h
- Step 3: 1 h (tests + wire smoke)
