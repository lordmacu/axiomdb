//! Integration tests for array DDL parsing (Step 3 of plan-20.4-arrays).
//!
//! Tests that the parser correctly handles:
//! - `TEXT[]`, `INT[][]`, `FLOAT[3][3]`, `BOOL ARRAY` in CREATE TABLE
//! - Array metadata (ndims, size_hints) is stored in ColumnDef

use axiomdb_sql::ast::Stmt;
use axiomdb_sql::parse;
use axiomdb_types::DataType;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn create_table(sql: &str) -> axiomdb_sql::ast::CreateTableStmt {
    match parse(sql, None).unwrap() {
        Stmt::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

// ── 1D Array Parsing ──────────────────────────────────────────────────────────

#[test]
fn parse_1d_int_array() {
    let ct = create_table("CREATE TABLE t (vals INT[])");
    assert_eq!(ct.columns.len(), 1);
    let col = &ct.columns[0];
    assert_eq!(col.name, "vals");
    // Element type is Int
    assert_eq!(col.data_type, DataType::Int);
    // array_ndims = 1
    assert_eq!(col.array_ndims, Some(1));
    // size_hints has one entry per dimension (None = unbounded)
    assert_eq!(col.array_size_hints.len(), 1);
    assert_eq!(col.array_size_hints[0], None);
}

#[test]
fn parse_1d_text_array() {
    let ct = create_table("CREATE TABLE t (tags TEXT[])");
    assert_eq!(ct.columns.len(), 1);
    let col = &ct.columns[0];
    assert_eq!(col.name, "tags");
    // Element type is Text
    assert_eq!(col.data_type, DataType::Text);
    // array_ndims = 1
    assert_eq!(col.array_ndims, Some(1));
    assert_eq!(col.array_size_hints.len(), 1);
    assert_eq!(col.array_size_hints[0], None);
}

#[test]
fn parse_1d_bool_array() {
    let ct = create_table("CREATE TABLE t (flags BOOL[])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Bool);
    assert_eq!(col.array_ndims, Some(1));
}

#[test]
fn parse_1d_real_array() {
    let ct = create_table("CREATE TABLE t (scores REAL[])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Float);
    assert_eq!(col.array_ndims, Some(1));
}

// ── Multi-Dimensional Array Parsing ───────────────────────────────────────────

#[test]
fn parse_2d_int_array() {
    let ct = create_table("CREATE TABLE t (matrix INT[][])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Int);
    assert_eq!(col.array_ndims, Some(2));
    // size_hints has one entry per dimension
    assert_eq!(col.array_size_hints.len(), 2);
    assert_eq!(col.array_size_hints[0], None);
    assert_eq!(col.array_size_hints[1], None);
}

#[test]
fn parse_3d_int_array() {
    let ct = create_table("CREATE TABLE t (tensor INT[][][])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Int);
    assert_eq!(col.array_ndims, Some(3));
}

#[test]
fn parse_6d_int_array() {
    // Maximum supported dimensions (6 pairs of [])
    let ct = create_table("CREATE TABLE t (hyper INT[][][][][][])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Int);
    assert_eq!(col.array_ndims, Some(6));
    assert_eq!(col.array_size_hints.len(), 6);
}

#[test]
fn parse_2d_text_array() {
    let ct = create_table("CREATE TABLE t (grid TEXT[][])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Text);
    assert_eq!(col.array_ndims, Some(2));
}

// ── ARRAY Keyword Suffix ──────────────────────────────────────────────────────

#[test]
fn parse_bool_array_keyword() {
    let ct = create_table("CREATE TABLE t (flags BOOL ARRAY)");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Bool);
    assert_eq!(col.array_ndims, Some(1));
}

#[test]
fn parse_int_array_keyword() {
    let ct = create_table("CREATE TABLE t (vals INT ARRAY)");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Int);
    assert_eq!(col.array_ndims, Some(1));
}

#[test]
fn parse_text_array_keyword() {
    let ct = create_table("CREATE TABLE t (tags TEXT ARRAY)");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Text);
    assert_eq!(col.array_ndims, Some(1));
}

// ── Size Hints ────────────────────────────────────────────────────────────────

#[test]
fn parse_1d_array_with_size_hint() {
    let ct = create_table("CREATE TABLE t (vals INT[10])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Int);
    assert_eq!(col.array_ndims, Some(1));
    assert_eq!(col.array_size_hints.len(), 1);
    assert_eq!(col.array_size_hints[0], Some(10));
}

#[test]
fn parse_2d_array_with_size_hints() {
    let ct = create_table("CREATE TABLE t (m FLOAT[3][3])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Float);
    assert_eq!(col.array_ndims, Some(2));
    assert_eq!(col.array_size_hints.len(), 2);
    assert_eq!(col.array_size_hints[0], Some(3));
    assert_eq!(col.array_size_hints[1], Some(3));
}

#[test]
fn parse_2d_array_with_mixed_size_hints() {
    // One bounded, one unbounded
    let ct = create_table("CREATE TABLE t (m FLOAT[3][])");
    let col = &ct.columns[0];
    assert_eq!(col.array_ndims, Some(2));
    assert_eq!(col.array_size_hints.len(), 2);
    assert_eq!(col.array_size_hints[0], Some(3));
    assert_eq!(col.array_size_hints[1], None);
}

#[test]
fn parse_1d_text_array_with_size_hint() {
    let ct = create_table("CREATE TABLE t (names TEXT[100])");
    let col = &ct.columns[0];
    assert_eq!(col.data_type, DataType::Text);
    assert_eq!(col.array_ndims, Some(1));
    assert_eq!(col.array_size_hints.len(), 1);
    assert_eq!(col.array_size_hints[0], Some(100));
}

// ── Multiple Array Columns ─────────────────────────────────────────────────────

#[test]
fn parse_multiple_array_columns() {
    let ct = create_table("CREATE TABLE t (tags TEXT[], scores INT[][], data FLOAT[3][3])");
    assert_eq!(ct.columns.len(), 3);

    let tags = &ct.columns[0];
    assert_eq!(tags.name, "tags");
    assert_eq!(tags.data_type, DataType::Text);
    assert_eq!(tags.array_ndims, Some(1));

    let scores = &ct.columns[1];
    assert_eq!(scores.name, "scores");
    assert_eq!(scores.data_type, DataType::Int);
    assert_eq!(scores.array_ndims, Some(2));

    let data = &ct.columns[2];
    assert_eq!(data.name, "data");
    assert_eq!(data.data_type, DataType::Float);
    assert_eq!(data.array_ndims, Some(2));
    assert_eq!(data.array_size_hints, vec![Some(3), Some(3)]);
}

// ── Array and Scalar Mixed ────────────────────────────────────────────────────

#[test]
fn parse_mixed_array_and_scalar_columns() {
    let ct = create_table("CREATE TABLE t (id INT, vals INT[], name TEXT, matrix INT[][])");
    assert_eq!(ct.columns.len(), 4);

    // Scalar columns have no array metadata
    assert_eq!(ct.columns[0].array_ndims, None);
    assert!(ct.columns[0].array_size_hints.is_empty());

    assert_eq!(ct.columns[2].array_ndims, None);

    // Array columns have array metadata
    assert_eq!(ct.columns[1].array_ndims, Some(1));
    assert_eq!(ct.columns[3].array_ndims, Some(2));
}

// ── Non-Array Columns Unaffected ──────────────────────────────────────────────

#[test]
fn parse_non_array_columns_have_no_array_metadata() {
    let ct = create_table("CREATE TABLE t (id INT, name TEXT, flag BOOL, amount REAL)");
    for col in &ct.columns {
        assert_eq!(
            col.array_ndims, None,
            "column {} should not be array",
            col.name
        );
        assert!(
            col.array_size_hints.is_empty(),
            "column {} should have no size hints",
            col.name
        );
    }
}

// ── ALTER TABLE ADD COLUMN ───────────────────────────────────────────────────

#[test]
fn parse_alter_table_add_array_column() {
    let stmt = axiomdb_sql::parse("ALTER TABLE t ADD COLUMN vals INT[]", None).unwrap();
    match stmt {
        Stmt::AlterTable(at) => {
            assert!(matches!(
                at.operations.as_slice(),
                [axiomdb_sql::ast::AlterTableOp::AddColumn(_)]
            ));
            if let axiomdb_sql::ast::AlterTableOp::AddColumn(col) = &at.operations[0] {
                assert_eq!(col.data_type, DataType::Int);
                assert_eq!(col.array_ndims, Some(1));
            }
        }
        other => panic!("expected AlterTable, got {other:?}"),
    }
}

#[test]
fn parse_alter_table_add_2d_array_column() {
    let stmt = axiomdb_sql::parse("ALTER TABLE t ADD COLUMN matrix TEXT[][]", None).unwrap();
    match stmt {
        Stmt::AlterTable(at) => {
            if let axiomdb_sql::ast::AlterTableOp::AddColumn(col) = &at.operations[0] {
                assert_eq!(col.data_type, DataType::Text);
                assert_eq!(col.array_ndims, Some(2));
            }
        }
        other => panic!("expected AlterTable, got {other:?}"),
    }
}

// ── Constraints with Array Columns ────────────────────────────────────────────

#[test]
fn parse_array_column_with_not_null() {
    let ct = create_table("CREATE TABLE t (vals INT[] NOT NULL)");
    let col = &ct.columns[0];
    assert_eq!(col.array_ndims, Some(1));
    let has_not_null = col
        .constraints
        .iter()
        .any(|c| matches!(c, axiomdb_sql::ast::ColumnConstraint::NotNull));
    assert!(has_not_null);
}

#[test]
fn parse_array_column_with_default() {
    let ct = create_table("CREATE TABLE t (vals INT[] DEFAULT NULL)");
    let col = &ct.columns[0];
    assert_eq!(col.array_ndims, Some(1));
    let has_default = col
        .constraints
        .iter()
        .any(|c| matches!(c, axiomdb_sql::ast::ColumnConstraint::Default(_)));
    assert!(has_default);
}

// ── Primary Key / Foreign Key on Array Columns (not supported, just parsing) ─

#[test]
fn parse_table_with_array_and_primary_key() {
    let ct = create_table("CREATE TABLE t (id INT PRIMARY KEY, vals INT[])");
    assert_eq!(ct.columns[0].array_ndims, None);
    assert_eq!(ct.columns[1].array_ndims, Some(1));
}

// ── Validation: Too Many Dimensions ────────────────────────────────────────────

#[test]
fn parse_7d_array_rejected() {
    let result = axiomdb_sql::parse("CREATE TABLE t (x INT[][][][][][][])", None);
    assert!(
        result.is_err(),
        "7D array should be rejected (max 6 dimensions)"
    );
}

// ── ARRAY Constructor Tests (Step 4) ───────────────────────────────────────────

#[test]
fn parse_array_constructor_int() {
    // SELECT ARRAY[1, 2, 3] → ArrayConstructor with [1, 2, 3]
    let sql = "SELECT ARRAY[1, 2, 3]";
    let stmt = axiomdb_sql::parse(sql, None).unwrap();
    match stmt {
        Stmt::Select(select) => {
            assert_eq!(select.columns.len(), 1);
            match &select.columns[0] {
                axiomdb_sql::ast::SelectItem::Expr { expr, alias } => {
                    assert!(alias.is_none());
                    match expr {
                        axiomdb_sql::expr::Expr::ArrayConstructor { elements } => {
                            assert_eq!(elements.len(), 3);
                        }
                        other => panic!("expected ArrayConstructor, got {:?}", other),
                    }
                }
                other => panic!("expected Expr SelectItem, got {:?}", other),
            }
        }
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn parse_array_constructor_text() {
    // SELECT ARRAY['a', 'b', 'c']
    let sql = "SELECT ARRAY['a', 'b', 'c']";
    let stmt = axiomdb_sql::parse(sql, None).unwrap();
    match stmt {
        Stmt::Select(select) => match &select.columns[0] {
            axiomdb_sql::ast::SelectItem::Expr { expr, .. } => match expr {
                axiomdb_sql::expr::Expr::ArrayConstructor { elements } => {
                    assert_eq!(elements.len(), 3);
                }
                other => panic!("expected ArrayConstructor, got {:?}", other),
            },
            other => panic!("expected Expr SelectItem, got {:?}", other),
        },
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn parse_array_constructor_nested() {
    // SELECT ARRAY[ARRAY[1, 2], ARRAY[3, 4]]
    let sql = "SELECT ARRAY[ARRAY[1, 2], ARRAY[3, 4]]";
    let stmt = axiomdb_sql::parse(sql, None).unwrap();
    match stmt {
        Stmt::Select(select) => {
            match &select.columns[0] {
                axiomdb_sql::ast::SelectItem::Expr { expr, .. } => {
                    match expr {
                        axiomdb_sql::expr::Expr::ArrayConstructor { elements } => {
                            assert_eq!(elements.len(), 2);
                            // Both should be ArrayConstructors
                            for elem in elements {
                                assert!(matches!(
                                    elem,
                                    axiomdb_sql::expr::Expr::ArrayConstructor { .. }
                                ));
                            }
                        }
                        other => panic!("expected ArrayConstructor, got {:?}", other),
                    }
                }
                other => panic!("expected Expr SelectItem, got {:?}", other),
            }
        }
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn parse_array_constructor_empty() {
    // SELECT ARRAY[] — empty array is valid at parse time
    let sql = "SELECT ARRAY[]";
    let stmt = axiomdb_sql::parse(sql, None).unwrap();
    match stmt {
        Stmt::Select(select) => match &select.columns[0] {
            axiomdb_sql::ast::SelectItem::Expr { expr, .. } => match expr {
                axiomdb_sql::expr::Expr::ArrayConstructor { elements } => {
                    assert_eq!(elements.len(), 0);
                }
                other => panic!("expected ArrayConstructor, got {:?}", other),
            },
            other => panic!("expected Expr SelectItem, got {:?}", other),
        },
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn parse_array_constructor_with_cast() {
    // SELECT CAST(ARRAY[] AS INT[]) — empty array with explicit cast.
    // Note: The DDL parser returns ParsedDataType with separate ndims, but
    // the CAST parser currently only uses data_type field. So for INT[],
    // we get DataType::Int (not DataType::Array(Int)) — this is a known
    // gap in the DDL parser (Step 3). The important thing for Step 4 is
    // that the ArrayConstructor is parsed correctly inside the cast.
    let sql = "SELECT CAST(ARRAY[] AS INT[])";
    let stmt = axiomdb_sql::parse(sql, None).unwrap();
    match stmt {
        Stmt::Select(select) => {
            match &select.columns[0] {
                axiomdb_sql::ast::SelectItem::Expr { expr, .. } => {
                    // Should be Cast(ArrayConstructor [], ...)
                    match expr {
                        axiomdb_sql::expr::Expr::Cast { expr, .. } => {
                            assert!(matches!(
                                expr.as_ref(),
                                axiomdb_sql::expr::Expr::ArrayConstructor { .. }
                            ));
                        }
                        other => panic!("expected Cast, got {:?}", other),
                    }
                }
                other => panic!("expected Expr SelectItem, got {:?}", other),
            }
        }
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn parse_array_constructor_in_insert() {
    // CREATE TABLE t (vals INT[]); INSERT INTO t VALUES (ARRAY[1, 2, 3]);
    let create = axiomdb_sql::parse("CREATE TABLE t (vals INT[])", None).unwrap();
    assert!(matches!(create, Stmt::CreateTable(_)));

    let insert = axiomdb_sql::parse("INSERT INTO t VALUES (ARRAY[1, 2, 3])", None).unwrap();
    match insert {
        Stmt::Insert(insert_stmt) => match &insert_stmt.source {
            axiomdb_sql::ast::InsertSource::Values(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 1);
                assert!(matches!(
                    &rows[0][0],
                    axiomdb_sql::expr::Expr::ArrayConstructor { .. }
                ));
            }
            other => panic!("expected Values, got {:?}", other),
        },
        other => panic!("expected Insert, got {:?}", other),
    }
}
