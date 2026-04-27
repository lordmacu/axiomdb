# Plan: 20.1 — Regular Views

Phase: 20 — Types + import/export
Task: 20.1 Regular views
Spec: specs/fase-20/spec-20.1-regular-views.md
Status: in-progress

## Summary

Views are implemented in 5 ordered steps that each compile and pass tests
before moving to the next. The catalog gets `RelationKind::View` first so all
downstream code can branch on it. Then AST+parser add the DDL syntax. Then
the executor wires CREATE/DROP. Then the analyzer adds `expand_views()` — the
core feature — which mirrors `expand_ctes()` exactly: for every
`FromClause::Table` that resolves to a view in the catalog, re-parse
`defining_query` and substitute `FromClause::Subquery`. Finally, introspection
(`SHOW CREATE VIEW`, `information_schema.VIEWS`, `SHOW FULL TABLES`) and the
integration test suite close the subphase.

## Dependencies

Must be done first:
- [x] spec-20.1-regular-views.md approved
- [x] 13.1 closed (materialized view catalog pattern)
- [x] 21.2 closed (CTE expansion pattern)

Blocks:
- nothing immediate

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_views.rs` — integration tests
- `crates/axiomdb-sql/src/executor/ddl_view.rs` — execute_create_view / execute_drop_view

Modified files:
- `crates/axiomdb-catalog/src/schema_table.rs` — RelationKind::View variant + is_view()
- `crates/axiomdb-sql/src/ast.rs` — CreateViewStmt, DropViewStmt
- `crates/axiomdb-sql/src/lexer.rs` — Token::Replace if not present
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse CREATE [OR REPLACE] VIEW, DROP VIEW
- `crates/axiomdb-sql/src/parser/mod.rs` — dispatch CreateView / DropView
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — expand_views(), call site before expand_ctes
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — dispatch CreateView / DropView
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` — read-only routing for CreateView / DropView
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — arm for CreateView / DropView
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — SHOW CREATE VIEW, show_table_type_name
- `crates/axiomdb-sql/src/information_schema.rs` — IS_VIEWS_COLS, "views" routing
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — build_information_schema_views
- `crates/axiomdb-sql/src/plan_deps.rs` — arm for CreateView / DropView
- `crates/axiomdb-sql/src/lib.rs` — module pub use if needed
- `tools/wire-test.py` — [20.1 regular views] block

---

## Step 1 — RelationKind::View in catalog

**Goal:** add `View` variant (tag=2) and `is_view()` helper to `TableDef`
**Files:** `crates/axiomdb-catalog/src/schema_table.rs`

### Changes

```rust
// schema_table.rs — RelationKind enum
pub enum RelationKind {
    #[default]
    Table,
    MaterializedView,
    View,          // new — tag = 2
}

// From<RelationKind> for u8
RelationKind::View => 2,

// TryFrom<u8> for RelationKind
2 => Ok(Self::View),

// TableDef impl
pub fn is_view(&self) -> bool {
    self.relation_kind == RelationKind::View
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-catalog 2>&1 | tail -5"
```

---

## Step 2 — AST + Lexer + Parser

**Goal:** parse `CREATE [OR REPLACE] VIEW name [(cols)] AS SELECT ...` and `DROP VIEW [IF EXISTS] name [, ...]`
**Files:** `ast.rs`, `lexer.rs`, `parser/ddl.rs`, `parser/mod.rs`

### AST nodes

```rust
// ast.rs
pub struct CreateViewStmt {
    pub or_replace: bool,
    pub view: TableRef,                    // name + optional schema/db
    pub column_names: Option<Vec<String>>, // optional (col1, col2, ...)
    pub select: SelectStmt,
    pub query_sql: String,                 // raw SQL text for storing
}

pub struct DropViewStmt {
    pub if_exists: bool,
    pub views: Vec<TableRef>,              // supports DROP VIEW v1, v2
}

// Stmt enum — new arms
Stmt::CreateView(CreateViewStmt),
Stmt::DropView(DropViewStmt),
```

### Lexer

Check if `Token::Replace` already exists; add if missing.

### Parser outline

```
parse_create_view:
  consume OR REPLACE → or_replace flag
  consume VIEW
  parse table ref (schema.name or name)
  optional: ( ident, ident, ... ) → column_names
  consume AS
  parse_select → select + capture query_sql via source span or re-serialize
  return CreateViewStmt

parse_drop_view:
  optional IF EXISTS
  parse comma-separated table refs
  return DropViewStmt
```

`query_sql` is captured as the raw remainder of the SQL after `AS ` — same
pattern as `CreateMaterializedViewStmt.query_sql`.

### Verification

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_ddl_parser -E 'test(view)' 2>&1 | tail -10"
```

Add parser-only tests to `integration_ddl_parser.rs`:
- `test_create_view_parses`
- `test_create_or_replace_view_parses`
- `test_create_view_with_column_names_parses`
- `test_drop_view_parses`
- `test_drop_view_if_exists_parses`
- `test_drop_view_multi_name_parses`

---

## Step 3 — Executor DDL (create / drop)

**Goal:** `CREATE VIEW` catalogs metadata with no physical pages; `DROP VIEW` removes it.
**Files:** `executor/ddl_view.rs` (new), `executor/exec_dispatch.rs`, `executor/exec_with_ctx.rs`, `executor/exec_explain.rs`, `plan_deps.rs`

### execute_create_view

```rust
fn execute_create_view(
    stmt: CreateViewStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    schema: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let existing = reader.get_table(schema, &stmt.view.name)?;

    if let Some(ref t) = existing {
        if stmt.or_replace && t.is_view() {
            // drop old, re-create below
            let mut w = CatalogWriter::new(storage, txn, conn_txn)?;
            w.drop_table(t.id)?;
        } else if stmt.or_replace && !t.is_view() {
            return Err(DbError::InvalidValue {
                reason: format!("'{}' is a table, not a view", stmt.view.name),
            });
        } else {
            return Err(DbError::InvalidValue {
                reason: format!("view '{}' already exists", stmt.view.name),
            });
        }
    }

    // Create table row with no columns, no physical pages
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_table_with_options(
        schema,
        &stmt.view.name,
        &[],          // no columns stored
        RelationKind::View,
        Some(stmt.query_sql),  // defining_query
        None,         // collation
        false,        // immutable
        TablePersistence::Permanent,
    )?;
    Ok(QueryResult::Empty)
}
```

### execute_drop_view

```rust
fn execute_drop_view(
    stmt: DropViewStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    schema: &str,
) -> Result<QueryResult, DbError> {
    let mut total_dropped = 0u64;
    for vref in stmt.views {
        let snap = txn.active_snapshot(conn_txn);
        let reader = CatalogReader::new(storage, snap)?;
        match reader.get_table(schema, &vref.name)? {
            None if stmt.if_exists => continue,
            None => return Err(DbError::InvalidValue {
                reason: format!("view '{}' does not exist", vref.name),
            }),
            Some(t) if !t.is_view() => return Err(DbError::InvalidValue {
                reason: format!("'{}' is not a view", vref.name),
            }),
            Some(t) => {
                let mut w = CatalogWriter::new(storage, txn, conn_txn)?;
                w.drop_table(t.id)?;
                total_dropped += 1;
            }
        }
    }
    Ok(QueryResult::Affected(total_dropped))
}
```

### exec_dispatch.rs wiring

```rust
Stmt::CreateView(s) => execute_create_view(s, storage, txn, conn_txn, schema)?,
Stmt::DropView(s)   => execute_drop_view(s, storage, txn, conn_txn, schema)?,
```

`exec_with_ctx.rs`: add both arms to the DDL branch (not read-only).
`exec_explain.rs`: add `NotImplemented` arms.
`plan_deps.rs`: add no-op arms.

### Verification

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_views -E 'test(create|drop)' 2>&1 | tail -10"
```

Tests:
- `create_view_stores_catalog_entry`
- `create_or_replace_view_replaces_definition`
- `create_view_errors_on_duplicate`
- `create_or_replace_errors_on_base_table`
- `drop_view_removes_catalog_entry`
- `drop_view_if_exists_on_missing_is_ok`
- `drop_view_errors_on_base_table`
- `drop_view_multi_name`

---

## Step 4 — Analyzer expansion (expand_views)

**Goal:** `SELECT * FROM v` transparently expands view to its defining SELECT.
**Files:** `analyzer_stmt.rs`

### expand_views function

```rust
fn expand_views(
    s: &mut SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
    seen: &mut std::collections::HashSet<String>,  // circular detection
) -> Result<(), DbError> {
    // 1. Expand FROM
    if let Some(from) = s.from.take() {
        s.from = Some(substitute_view_ref(
            from, storage, snapshot.clone(), default_database,
            default_schema, outer_scopes, seen,
        )?);
    }
    // 2. Expand JOIN tables
    for join in &mut s.joins {
        let taken = join.table.take_from();
        join.set_from(substitute_view_ref(
            taken, storage, snapshot.clone(), default_database,
            default_schema, outer_scopes, seen,
        )?);
    }
    Ok(())
}

fn substitute_view_ref(
    from: FromClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
    seen: &mut std::collections::HashSet<String>,
) -> Result<FromClause, DbError> {
    match from {
        FromClause::Table(ref tref)
            if tref.database.is_none()
               && tref.schema.as_deref().map(|s| s.eq_ignore_ascii_case("public")).unwrap_or(true) =>
        {
            let schema = tref.schema.as_deref().unwrap_or(default_schema);
            let reader = CatalogReader::new(storage, snapshot.clone())?;
            if let Some(view_def) = reader.get_table(schema, &tref.name)? {
                if view_def.is_view() {
                    let key = format!("{}.{}", schema, tref.name.to_ascii_lowercase());
                    if seen.contains(&key) {
                        return Err(DbError::InvalidValue {
                            reason: format!("circular view reference: {}", tref.name),
                        });
                    }
                    seen.insert(key.clone());

                    let sql = view_def.defining_query.as_deref().ok_or_else(|| DbError::Internal {
                        message: format!("view '{}' has no defining_query", tref.name),
                    })?;
                    let alias = tref.alias.clone().unwrap_or_else(|| tref.name.clone());

                    // Parse and recursively expand
                    let parsed = crate::parser::parse(sql)?;
                    if let crate::ast::Stmt::Select(mut inner) = parsed {
                        expand_views(&mut inner, storage, snapshot, default_database,
                                     default_schema, outer_scopes, seen)?;
                        seen.remove(&key);

                        // Apply column-name rename if view had column list
                        // (stored as alias overrides on the outer Subquery)
                        return Ok(FromClause::Subquery {
                            query: Box::new(inner),
                            alias,
                            lateral: false,
                        });
                    }
                }
            }
            Ok(from)
        }
        other => Ok(other),
    }
}
```

**Call site** in `analyze_stmt` — before `expand_ctes`:

```rust
// In analyze_select_with_outer, before expand_ctes:
let mut seen_views = std::collections::HashSet::new();
expand_views(&mut s, storage, snapshot.clone(), default_database, default_schema,
             outer_scopes, &mut seen_views)?;
if !s.with_ctes.is_empty() {
    expand_ctes(&mut s, storage, snapshot.clone(), ...)?;
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_views 2>&1 | tail -10"
```

Tests:
- `select_from_view_expands_transparently`
- `view_in_join_expands`
- `view_in_subquery_expands`
- `nested_views_expand_recursively`
- `circular_view_returns_error`
- `view_with_column_names_rename`
- `view_cte_same_name_cte_wins`
- `view_used_in_insert_select`

---

## Step 5 — Introspection

**Goal:** `SHOW CREATE VIEW`, `SHOW FULL TABLES`, `information_schema.TABLES/VIEWS`.
**Files:** `executor/ddl_show.rs`, `information_schema.rs`, `executor/information_schema_exec.rs`

### SHOW CREATE VIEW (ddl_show.rs)

In `execute_show_create_table`, add branch before the materialized view branch:

```rust
if table_def.is_view() {
    let defining_query = table_def.defining_query.as_deref()
        .ok_or_else(|| DbError::Internal { ... })?;
    return Ok(QueryResult::Rows {
        columns: vec![
            ColumnMeta::computed("View", DataType::Text),
            ColumnMeta::computed("Create View", DataType::Text),
        ],
        rows: vec![vec![
            Value::Text(table_def.table_name.clone()),
            Value::Text(format!("CREATE VIEW `{}` AS {}", table_def.table_name, defining_query)),
        ]],
    });
}
```

Also update `show_table_type_name`:

```rust
fn show_table_type_name(table: &TableDef) -> &'static str {
    if table.is_view()              { "VIEW" }
    else if table.is_materialized_view() { "MATERIALIZED VIEW" }
    else                            { "BASE TABLE" }
}
```

`SHOW CREATE VIEW` command: currently `SHOW CREATE TABLE` handles the dispatch;
parse `SHOW CREATE VIEW name` → reuse same executor path. Add `VIEW` keyword
arm in the parser for `SHOW CREATE`.

### information_schema.VIEWS (information_schema.rs + exec)

```rust
// information_schema.rs
pub static IS_VIEWS_COLS: &[(&str, ColumnType)] = &[
    ("TABLE_CATALOG",  ColumnType::Text),
    ("TABLE_SCHEMA",   ColumnType::Text),
    ("TABLE_NAME",     ColumnType::Text),
    ("VIEW_DEFINITION",ColumnType::Text),
    ("CHECK_OPTION",   ColumnType::Text),
    ("IS_UPDATABLE",   ColumnType::Text),
];

// is_table_cols routing
"views" => Some(IS_VIEWS_COLS),
```

```rust
// information_schema_exec.rs
fn build_information_schema_views(
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<QueryResult, DbError> {
    // enumerate all tables in all schemas, filter is_view()
    // emit one row per view
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_views -E 'test(show|info)' 2>&1 | tail -10"
```

Tests:
- `show_create_view_roundtrips`
- `show_full_tables_shows_view_type`
- `information_schema_tables_shows_view_type`
- `information_schema_views_returns_definition`
- `show_create_view_errors_on_base_table`

---

## Step 6 — Wire smoke + full closeout

**Goal:** wire-test passes, workspace clean.
**Files:** `tools/wire-test.py`

### Wire smoke block

```python
# [20.1 regular views]
cur.execute("CREATE TABLE v20_base (id INT, name TEXT)")
cur.execute("INSERT INTO v20_base VALUES (1, 'alice'), (2, 'bob')")
cur.execute("CREATE VIEW v20_active AS SELECT * FROM v20_base WHERE id > 0")
conn.commit()

cur.execute("SELECT id, name FROM v20_active ORDER BY id")
rows = cur.fetchall()
ok("[20.1] view SELECT expands transparently", rows == ((1, 'alice'), (2, 'bob')), rows)

cur.execute("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_NAME = 'v20_active'")
rows = cur.fetchall()
ok("[20.1] information_schema.TABLES shows VIEW", rows == (('v20_active', 'VIEW'),), rows)

cur.execute("DROP VIEW v20_active")
conn.commit()
cur.execute("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_NAME = 'v20_active'")
rows = cur.fetchall()
ok("[20.1] DROP VIEW removes from catalog", rows == ((0,),), rows)
```

### Final verification against spec done criteria

```bash
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run --workspace 2>&1 | tail -5"
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo clippy --workspace -- -D warnings 2>&1 | tail -5"
limactl shell axiomdb -- bash -c "CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo fmt --check 2>&1"
limactl shell axiomdb -- bash -c "AXIOMDB_SERVER_BIN=\$HOME/axiomdb-target/debug/axiomdb-server python3 tools/wire-test.py 2>&1 | tail -5"
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `get_table` returns view when lookup expects only base tables | high | guard `is_view()` checks in all places that call `get_table` for DML targets |
| `expand_views` runs after `expand_ctes` misses view-in-CTE | low | call `expand_views` before `expand_ctes`; views inside CTE bodies expand when that CTE is analyzed |
| `create_table_with_options` doesn't handle `root_page_id=0` for views | medium | check that writer doesn't allocate pages when column list is empty |
| `SHOW FULL TABLES` loops all tables including views — existing clients filter by type | low | `TABLE_TYPE='VIEW'` is standard; clients that want only tables filter on their side |

## Estimated effort

Total: ~3 hours
- Step 1 (catalog): 15 min
- Step 2 (AST+parser): 45 min
- Step 3 (executor DDL): 30 min
- Step 4 (analyzer expansion): 60 min  ← core, most complex
- Step 5 (introspection): 30 min
- Step 6 (wire + closeout): 20 min
