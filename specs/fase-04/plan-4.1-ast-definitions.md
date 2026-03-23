# Plan: 4.1 — AST Definitions

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/nexusdb-sql/src/ast.rs` | CREATE | All AST types: TableRef, ColumnDef, Stmt, … |
| `crates/nexusdb-sql/src/lib.rs` | MODIFY | Add `pub mod ast` + re-exports |

No new Cargo.toml changes needed — `nexusdb-types` is already a dependency.

---

## Implementation phases

### Phase 1 — ast.rs: base types
1. `TableRef { schema, name, alias }`
2. `SortOrder` (Asc/Desc, Default=Asc, Copy)
3. `NullsOrder` (First/Last, Copy)
4. `JoinType` (Inner/Left/Right/Cross/Full, Copy)
5. `JoinCondition` (On(Expr) / Using(Vec<String>))
6. `ForeignKeyAction` (NoAction/Restrict/Cascade/SetNull/SetDefault, Default=NoAction, Copy)
7. `Assignment { column, value: Expr }`
8. `OrderByItem { expr, order: SortOrder, nulls: Option<NullsOrder> }`
9. `IndexColumn { name, order: SortOrder }`

### Phase 2 — ast.rs: column and constraint types
1. `ColumnConstraint` enum (8 variants)
2. `TableConstraint` enum (4 variants)
3. `ColumnDef { name, data_type: DataType, constraints: Vec<ColumnConstraint> }`

### Phase 3 — ast.rs: SELECT types
1. `SelectItem` (Wildcard / QualifiedWildcard / Expr{expr, alias})
2. `FromClause` (Table(TableRef) / Subquery{query: Box<SelectStmt>, alias})
3. `JoinClause { join_type, table: FromClause, condition: JoinCondition }`
4. `SelectStmt` (all 10 fields)

### Phase 4 — ast.rs: DML types
1. `InsertSource` (Values / Select / DefaultValues)
2. `InsertStmt { table, columns: Option<Vec<String>>, source }`
3. `UpdateStmt { table, assignments, where_clause }`
4. `DeleteStmt { table, where_clause }`

### Phase 5 — ast.rs: DDL types
1. `CreateTableStmt { if_not_exists, table, columns, table_constraints }`
2. `CreateIndexStmt { if_not_exists, unique, name, table, columns }`
3. `DropTableStmt { if_exists, tables: Vec<TableRef>, cascade }`
4. `DropIndexStmt { if_exists, name, table: Option<TableRef> }`
5. `TruncateTableStmt { table }`
6. `AlterTableOp` enum (7 variants)
7. `AlterTableStmt { table, operations: Vec<AlterTableOp> }`

### Phase 6 — ast.rs: utility + Stmt
1. `ShowTablesStmt { schema: Option<String> }`
2. `ShowColumnsStmt { table: TableRef }`
3. `SetValue` (Expr / Default)
4. `SetStmt { variable, value: SetValue }`
5. `Stmt` enum (16 variants)

### Phase 7 — lib.rs + compilation tests
1. Add `pub mod ast;` to `lib.rs`
2. Re-export key types: `Stmt`, `SelectStmt`, `InsertStmt`, `UpdateStmt`,
   `DeleteStmt`, `CreateTableStmt`, `TableRef`, `ColumnDef`, `SelectItem`,
   `FromClause`, `OrderByItem`, `Assignment`, `InsertSource`
3. Inline `#[cfg(test)]` module with construction tests

---

## Construction tests (inline in ast.rs)

These tests verify the types are ergonomic to construct — the primary API
contract of pure data types.

```rust
#[test]
fn test_select_star_from_table() {
    let stmt = Stmt::Select(SelectStmt {
        distinct: false,
        columns: vec![SelectItem::Wildcard],
        from: Some(FromClause::Table(TableRef {
            schema: None,
            name: "users".into(),
            alias: None,
        })),
        joins: vec![],
        where_clause: Some(Expr::binop(
            BinaryOp::Gt,
            Expr::Column { col_idx: 0, name: "age".into() },
            Expr::int(18),
        )),
        group_by: vec![],
        having: None,
        order_by: vec![OrderByItem {
            expr: Expr::Column { col_idx: 1, name: "name".into() },
            order: SortOrder::Asc,
            nulls: None,
        }],
        limit: Some(Expr::int(10)),
        offset: None,
    });
    assert!(matches!(stmt, Stmt::Select(_)));
}

#[test]
fn test_create_table_with_constraints() {
    let stmt = Stmt::CreateTable(CreateTableStmt {
        if_not_exists: true,
        table: TableRef { schema: Some("public".into()), name: "users".into(), alias: None },
        columns: vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::BigInt,
                constraints: vec![
                    ColumnConstraint::PrimaryKey,
                    ColumnConstraint::AutoIncrement,
                ],
            },
            ColumnDef {
                name: "email".into(),
                data_type: DataType::Text,
                constraints: vec![ColumnConstraint::NotNull, ColumnConstraint::Unique],
            },
        ],
        table_constraints: vec![],
    });
    assert!(matches!(stmt, Stmt::CreateTable(_)));
}

#[test]
fn test_insert_values() {
    let stmt = Stmt::Insert(InsertStmt {
        table: TableRef { schema: None, name: "users".into(), alias: None },
        columns: Some(vec!["id".into(), "name".into()]),
        source: InsertSource::Values(vec![
            vec![Expr::int(1), Expr::text("Alice")],
            vec![Expr::int(2), Expr::text("Bob")],
        ]),
    });
    assert!(matches!(stmt, Stmt::Insert(_)));
}

#[test]
fn test_transaction_stmts() {
    assert!(matches!(Stmt::Begin, Stmt::Begin));
    assert!(matches!(Stmt::Commit, Stmt::Commit));
    assert!(matches!(Stmt::Rollback, Stmt::Rollback));
}

#[test]
fn test_join_clause() {
    let join = JoinClause {
        join_type: JoinType::Inner,
        table: FromClause::Table(TableRef {
            schema: None, name: "orders".into(), alias: Some("o".into()),
        }),
        condition: JoinCondition::On(Expr::binop(
            BinaryOp::Eq,
            Expr::Column { col_idx: 0, name: "u.id".into() },
            Expr::Column { col_idx: 1, name: "o.user_id".into() },
        )),
    };
    assert_eq!(join.join_type, JoinType::Inner);
}
```

---

## Anti-patterns to avoid

- **DO NOT** add source positions (line, col) to AST nodes — those belong in
  the parser's error reporting layer, not the clean AST.
- **DO NOT** use `String` for keywords (JOIN type, sort order) — use enums.
- **DO NOT** box every `Expr` unless there is a recursion cycle —
  `SelectStmt.where_clause: Option<Expr>` is fine (no box needed).
  `FromClause::Subquery` boxes `SelectStmt` because `SelectStmt` contains
  `FromClause` (recursion cycle).
- **DO NOT** make `ColumnDef` in the AST identical to the catalog `ColumnDef`
  — they serve different layers. The executor bridges between them.
- **DO NOT** add `Default` to `SelectStmt` — it has no sensible default;
  callers must always specify all fields.

---

## Recursion note

`SelectStmt` and `FromClause` are mutually recursive:
- `SelectStmt` has `from: Option<FromClause>`
- `FromClause::Subquery` has `query: Box<SelectStmt>`

The `Box` on `SelectStmt` inside `FromClause::Subquery` breaks the cycle.
No other boxing is needed.
