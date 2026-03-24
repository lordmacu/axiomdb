# Plan: 4.22b — ALTER TABLE ADD/DROP CONSTRAINT

## Files to create/modify

| Acción | Archivo | Qué cambia |
|---|---|---|
| Modify | `crates/axiomdb-storage/src/meta.rs` | Añade `CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET` (body[72..80]) y `NEXT_CONSTRAINT_ID_BODY_OFFSET` (body[80..84]) |
| Modify | `crates/axiomdb-storage/src/lib.rs` | Re-exporta las nuevas constantes |
| Modify | `crates/axiomdb-catalog/src/bootstrap.rs` | `CatalogPageIds.constraints: u64`; lazy-init del root page en `page_ids()` |
| Modify | `crates/axiomdb-catalog/src/schema.rs` | Añade `ConstraintDef { constraint_id, table_id, name, check_expr }` con `to_bytes()`/`from_bytes()` |
| Modify | `crates/axiomdb-catalog/src/writer.rs` | `create_constraint()`, `drop_constraint(id)` |
| Modify | `crates/axiomdb-catalog/src/reader.rs` | `list_constraints(table_id)`, `get_constraint_by_name(table_id, name)` |
| Modify | `crates/axiomdb-catalog/src/resolver.rs` | `ResolvedTable.constraints: Vec<ConstraintDef>` |
| Modify | `crates/axiomdb-sql/src/parser/ddl.rs` | Parsea `ADD CONSTRAINT` y `DROP CONSTRAINT` en `parse_alter_table` |
| Modify | `crates/axiomdb-sql/src/executor.rs` | Implementa `AddConstraint(Unique)`, `AddConstraint(Check)`, `DropConstraint` |
| Modify | `crates/axiomdb-sql/src/executor.rs` | CHECK enforcement en INSERT / UPDATE: evalúa constraints de la tabla |

---

## Algoritmo — meta page (lazy-init pattern)

```
meta.rs nuevas constantes:
  CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET = 72  ← body[72..80], u64 LE
  NEXT_CONSTRAINT_ID_BODY_OFFSET       = 80  ← body[80..84], u32 LE
```

`CatalogBootstrap::page_ids()` — lazy init para constraints:

```rust
pub fn page_ids(storage: &mut dyn StorageEngine) -> Result<CatalogPageIds, DbError> {
    let tables  = read_meta_u64(storage, CATALOG_TABLES_ROOT_BODY_OFFSET)?;
    let columns = read_meta_u64(storage, CATALOG_COLUMNS_ROOT_BODY_OFFSET)?;
    let indexes = read_meta_u64(storage, CATALOG_INDEXES_ROOT_BODY_OFFSET)?;
    let mut constraints = read_meta_u64(storage, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET)?;

    // Lazy init: existing databases (schema_ver=1) have constraints=0.
    // Allocate and persist the page on first access — idempotent.
    if constraints == 0 {
        constraints = storage.alloc_page(PageType::Data)?;
        let p = Page::new(PageType::Data, constraints);
        storage.write_page(constraints, &p)?;
        write_meta_u64(storage, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET, constraints)?;
        storage.flush()?;
    }

    Ok(CatalogPageIds { tables, columns, indexes, constraints })
}
```

---

## Algoritmo — ConstraintDef serialization

```rust
// schema.rs
pub struct ConstraintDef {
    pub constraint_id: u32,
    pub table_id: u32,
    pub name: String,          // constraint name (required)
    pub check_expr: String,    // SQL expression as string (empty for future non-CHECK types)
}

impl ConstraintDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.constraint_id.to_le_bytes()); // 4 bytes
        buf.extend_from_slice(&self.table_id.to_le_bytes());       // 4 bytes
        let name_bytes = self.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes()); // 4 bytes
        buf.extend_from_slice(name_bytes);
        let expr_bytes = self.check_expr.as_bytes();
        buf.extend_from_slice(&(expr_bytes.len() as u32).to_le_bytes()); // 4 bytes
        buf.extend_from_slice(expr_bytes);
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, DbError> {
        let constraint_id = u32::from_le_bytes(data[0..4].try_into()?);
        let table_id      = u32::from_le_bytes(data[4..8].try_into()?);
        let name_len      = u32::from_le_bytes(data[8..12].try_into()?) as usize;
        let name          = String::from_utf8(data[12..12+name_len].to_vec())?;
        let pos = 12 + name_len;
        let expr_len      = u32::from_le_bytes(data[pos..pos+4].try_into()?) as usize;
        let check_expr    = String::from_utf8(data[pos+4..pos+4+expr_len].to_vec())?;
        Ok(Self { constraint_id, table_id, name, check_expr })
    }
}
```

WAL table_id: `SYSTEM_TABLE_CONSTRAINTS = u32::MAX - 3`

---

## Algoritmo — parser/ddl.rs

Modificar `Token::Add` branch en `parse_alter_table()`:

```rust
Token::Add => {
    p.advance();
    // ADD CONSTRAINT name ... | ADD UNIQUE (...) | ADD COLUMN col_def
    if p.eat(&Token::Constraint) {
        // ADD CONSTRAINT name <constraint_type>
        let constraint = parse_table_constraint(p)?;  // already handles all types
        AlterTableOp::AddConstraint(constraint)
    } else if matches!(p.peek(), Token::Unique) {
        // ADD UNIQUE (cols) — shorthand without CONSTRAINT keyword
        let constraint = parse_table_constraint(p)?;  // eats Unique
        AlterTableOp::AddConstraint(constraint)
    } else {
        // ADD [COLUMN] col_def — existing behavior
        p.eat(&Token::Column);
        let col_def = parse_column_def(p)?;
        AlterTableOp::AddColumn(col_def)
    }
}
```

Modificar `Token::Drop` branch:

```rust
Token::Drop => {
    p.advance();
    if p.eat(&Token::Constraint) {
        // DROP CONSTRAINT [IF EXISTS] name
        let if_exists = p.eat(&Token::If) && p.eat(&Token::Exists);
        // ⚠️ if_exists: eat both If+Exists atomically — need lookahead
        let name = p.parse_identifier()?;
        AlterTableOp::DropConstraint { name, if_exists }
    } else {
        // DROP [COLUMN] [IF EXISTS] col — existing behavior
        p.eat(&Token::Column);
        // ... existing logic ...
    }
}
```

---

## Algoritmo — executor.rs: execute_alter_table

Replace `_ => NotImplemented` with:

```rust
AlterTableOp::AddConstraint(TableConstraint::Unique { name, columns }) => {
    let idx_name = name.unwrap_or_else(|| {
        // Auto-generate: axiom_uq_{table}_{col1}_{col2}
        format!("axiom_uq_{}_{}", table_def.def.table_name,
                columns.join("_"))
    });
    // Reuse execute_create_index logic via helper
    alter_add_unique(storage, txn, &table_def, &columns, &idx_name, schema)?;
}

AlterTableOp::AddConstraint(TableConstraint::Check { name, expr }) => {
    let cname = name.ok_or_else(|| DbError::ParseError {
        message: "ADD CONSTRAINT CHECK requires an explicit constraint name".into()
    })?;
    alter_add_check(storage, txn, &table_def, &columns, &cname, expr, snap)?;
}

AlterTableOp::DropConstraint { name, if_exists } => {
    alter_drop_constraint(storage, txn, &table_def, &name, if_exists, schema)?;
}

AlterTableOp::AddConstraint(TableConstraint::ForeignKey { .. }) => {
    return Err(DbError::NotImplemented {
        feature: "ADD CONSTRAINT FOREIGN KEY — Phase 6.5".into()
    });
}

AlterTableOp::AddConstraint(TableConstraint::PrimaryKey { .. }) => {
    return Err(DbError::NotImplemented {
        feature: "ADD CONSTRAINT PRIMARY KEY — requires full table rewrite".into()
    });
}
```

### `alter_add_unique` helper

```rust
fn alter_add_unique(storage, txn, table_def, col_names, idx_name, schema) {
    // Build CreateIndexStmt and delegate to execute_create_index logic
    let stmt = CreateIndexStmt {
        name: idx_name.clone(),
        table: TableRef { schema: Some(schema.to_string()), name: table_def.def.table_name.clone() },
        columns: col_names.iter().map(|c| IndexColumn { name: c.clone(), order: SortOrder::Asc }).collect(),
        unique: true,
        if_not_exists: false,
    };
    execute_create_index(stmt, storage, txn)?;
}
```

### `alter_add_check` helper

```rust
fn alter_add_check(storage, txn, table_def, columns, name, expr, snap) {
    // 1. Check for duplicate constraint name
    let reader = CatalogReader::new(storage, snap)?;
    if reader.get_constraint_by_name(table_def.def.id, name)?.is_some() {
        return Err(DbError::Other(format!("constraint '{name}' already exists on '{}'",
                   table_def.def.table_name)));
    }
    drop(reader);

    // 2. Validate all existing rows against the CHECK expression
    let rows = TableEngine::scan_table(storage, &table_def.def, columns, snap)?;
    for (_rid, row_values) in &rows {
        let result = eval(&expr, row_values)?;
        if !is_truthy(&result) {
            return Err(DbError::CheckViolation {
                table: table_def.def.table_name.clone(),
                constraint: name.to_string(),
            });
        }
    }

    // 3. Serialize the expression back to string for storage
    let check_expr_str = expr_to_sql_string(&expr)?; // or use the original SQL string if available

    // 4. Insert into axiom_constraints
    CatalogWriter::new(storage, txn)?.create_constraint(ConstraintDef {
        constraint_id: 0, // allocated by writer
        table_id: table_def.def.id,
        name: name.to_string(),
        check_expr: check_expr_str,
    })?;
}
```

⚠️ **Nota sobre `expr_to_sql_string`**: el AST `Expr` no tiene una función de serialización a SQL. Para 4.22b, el parser puede pasar el texto original del expr como String adicional, o podemos almacenar una representación textual del Expr. La solución más simple: en el spec ya tenemos `TableConstraint::Check { name, expr: Expr }`. Podemos añadir un campo `expr_str: String` al AST para preservar la cadena original durante el parse.

Alternativa más simple: en lugar de serializar el Expr de vuelta a SQL, pasar la substring del SQL original al momento del parse. El parser conoce las posiciones.

**Decisión de implementación**: Añadir `raw_sql: String` a `TableConstraint::Check` en el AST, populado por el parser con el texto entre paréntesis del CHECK. Esto es la solución más robusta y simple.

### `alter_drop_constraint` helper

```rust
fn alter_drop_constraint(storage, txn, table_def, name, if_exists, schema) {
    let snap = txn.active_snapshot()?;
    let table_id = table_def.def.id;
    let table_name = &table_def.def.table_name;

    // 1. Search in axiom_indexes (for UNIQUE constraints stored as indexes)
    let reader = CatalogReader::new(storage, snap)?;
    let indexes = reader.list_indexes(table_id)?;
    let idx = indexes.iter().find(|i| i.name == name);

    if let Some(idx_def) = idx {
        let index_id = idx_def.index_id;
        let root_page_id = idx_def.root_page_id;
        drop(reader);
        CatalogWriter::new(storage, txn)?.delete_index(index_id)?;
        free_btree_pages(storage, root_page_id)?;
        return Ok(());
    }

    // 2. Search in axiom_constraints (for CHECK constraints)
    let constraint = reader.get_constraint_by_name(table_id, name)?;
    drop(reader);

    match constraint {
        Some(c) => {
            CatalogWriter::new(storage, txn)?.drop_constraint(c.constraint_id)?;
            Ok(())
        }
        None if if_exists => Ok(()),
        None => Err(DbError::Other(format!(
            "constraint '{name}' not found on table '{table_name}'"
        ))),
    }
}
```

---

## Algoritmo — CHECK enforcement en INSERT

En `execute_insert_ctx` (executor.rs), después de que una fila pasa validaciones NOT NULL y UNIQUE, añadir:

```rust
// Check active CHECK constraints from axiom_constraints
if !resolved.constraints.is_empty() {
    for constraint in &resolved.constraints {
        if constraint.check_expr.is_empty() { continue; }
        let expr = parse_expr_str(&constraint.check_expr)?;
        let result = eval(&expr, &full_values)?;
        if !is_truthy(&result) {
            return Err(DbError::CheckViolation {
                table: table_name.to_string(),
                constraint: constraint.name.clone(),
            });
        }
    }
}
```

`resolved.constraints` viene de `ResolvedTable` (añadir campo `constraints: Vec<ConstraintDef>`).
`parse_expr_str` es una función helper que parsea una SQL expression string.

⚠️ **Nota**: `parse_expr_str` requiere pasar el SQL string por el lexer/parser. Esto añade un overhead de parsing por constraint por INSERT. Para Phase 4.22b esto es aceptable (constraints son raros). En fases futuras se puede cachear el Expr compilado en SessionContext.

---

## Fases de implementación

### Fase 1 — meta.rs + bootstrap.rs (20 min)
1. Añadir `CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET = 72` y `NEXT_CONSTRAINT_ID_BODY_OFFSET = 80`
2. Re-exportar desde `axiomdb-storage/src/lib.rs`
3. Añadir `constraints: u64` a `CatalogPageIds`
4. Implementar lazy-init en `CatalogBootstrap::page_ids()`
5. `cargo test -p axiomdb-catalog` debe pasar (existing tests use existing schema)

### Fase 2 — schema.rs + CatalogWriter/Reader (30 min)
1. `ConstraintDef { constraint_id, table_id, name, check_expr }` con `to_bytes()`/`from_bytes()`
2. `SYSTEM_TABLE_CONSTRAINTS = u32::MAX - 3` en writer.rs
3. `CatalogWriter::create_constraint(def)` y `drop_constraint(id)`
4. `CatalogReader::list_constraints(table_id)` y `get_constraint_by_name(table_id, name)`
5. `ResolvedTable.constraints: Vec<ConstraintDef>` — `SchemaResolver::resolve_table()` popula este campo
6. Unit tests: roundtrip to_bytes/from_bytes, create+list, drop

### Fase 3 — AST + Parser (20 min)
1. Añadir `raw_sql: String` a `TableConstraint::Check` en ast.rs
2. En `parse_table_constraint`: capturar el texto de la expr entre `(` y `)` y guardarlo en `raw_sql`
3. En `parse_alter_table`:
   - `Token::Add` branch: detección de `CONSTRAINT` o `UNIQUE`
   - `Token::Drop` branch: detección de `CONSTRAINT` keyword
4. Parser tests: parse ADD CONSTRAINT UNIQUE, ADD CONSTRAINT CHECK, DROP CONSTRAINT

### Fase 4 — Executor: ADD/DROP (30 min)
1. `alter_add_unique()` helper
2. `alter_add_check()` helper (usa `check_expr = constraint.raw_sql`)
3. `alter_drop_constraint()` helper
4. Reemplazar `_ => NotImplemented` con los nuevos handlers
5. Integration tests: ADD UNIQUE → index creado; DROP CONSTRAINT → index borrado; ADD CHECK → rows validadas + persiste; DROP CHECK → constraint eliminada

### Fase 5 — CHECK enforcement en INSERT/UPDATE (25 min)
1. `parse_expr_str(expr_sql: &str) -> Result<Expr, DbError>` helper (reutiliza el parser)
2. En `execute_insert_ctx`: evaluar `resolved.constraints` antes de commit
3. En `execute_update_ctx`: misma evaluación en el nuevo row
4. Integration test: INSERT con CHECK activo → success y failure

---

## Tests a escribir

### Integration tests (executor)
```
test_add_unique_constraint_creates_index
test_add_unique_constraint_duplicate_data_fails
test_drop_constraint_removes_unique_index
test_drop_constraint_if_exists_noop
test_drop_constraint_not_found_returns_error
test_add_check_constraint_validates_existing_rows
test_add_check_constraint_invalid_rows_fails
test_check_constraint_enforced_on_insert
test_check_constraint_not_enforced_after_drop
test_add_fk_returns_not_implemented
test_add_pk_returns_not_implemented
```

### Parser tests
```
test_parse_alter_add_constraint_unique
test_parse_alter_add_unique_anonymous
test_parse_alter_drop_constraint
test_parse_alter_drop_constraint_if_exists
test_parse_alter_add_constraint_check
```

---

## Anti-patterns a evitar

- **NO re-implementar execute_create_index desde cero** — usar `alter_add_unique` que construye un `CreateIndexStmt` y delega
- **NO cambiar el catalog schema_ver** — usar lazy-init para backwards compat
- **NO olvidar el campo `raw_sql` en TableConstraint::Check** — sin él, no podemos persistir la expresión en `axiom_constraints`
- **NO buscar solo en indexes en DROP CONSTRAINT** — también buscar en axiom_constraints para CHECK

---

## Risks

| Riesgo | Mitigación |
|---|---|
| `parse_expr_str` en CHECK enforcement duplica parseo | Cacheable en SessionContext en fases futuras; para 4.22b aceptable |
| Lazy-init de constraints_root corre durante una txn activa | `page_ids()` se llama dentro del `CatalogWriter::new()` — añadir flush después de la lazy-init para durabilidad |
| Expresión CHECK en `raw_sql` puede diferir del texto parseado (whitespace, case) | La stored `raw_sql` es el texto capturado directamente del input SQL — fiel al original |
| DROP CONSTRAINT que afecta tanto index como constraint con el mismo nombre | Priorizar index (lo más común) — si hay ambigüedad, índice gana |
