mod common;

use axiomdb_catalog::{CatalogReader, RelationKind};
use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn col_names(result: &QueryResult) -> Vec<String> {
    match result {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

// ── CREATE VIEW ───────────────────────────────────────────────────────────────

#[test]
fn create_view_persists_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id, name FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_table_in_database("axiomdb", "public", "v")
        .unwrap()
        .expect("view must be in catalog");
    assert_eq!(def.relation_kind, RelationKind::View);
    assert_eq!(def.root_page_id, 0, "views have no physical pages");
    assert!(
        def.defining_query.as_deref().unwrap().contains("SELECT"),
        "defining_query must contain SELECT"
    );
}

#[test]
fn create_view_duplicate_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "CREATE VIEW v AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
}

#[test]
fn create_or_replace_view_updates_definition() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE OR REPLACE VIEW v AS SELECT id, val FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_table_in_database("axiomdb", "public", "v")
        .unwrap()
        .expect("view must still exist");
    assert!(
        def.defining_query.as_deref().unwrap().contains("val"),
        "defining query must be updated to include 'val'"
    );
}

#[test]
fn create_view_on_existing_table_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "CREATE VIEW t AS SELECT 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
}

// ── DROP VIEW ─────────────────────────────────────────────────────────────────

#[test]
fn drop_view_removes_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx("DROP VIEW v", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(
        reader
            .get_table_in_database("axiomdb", "public", "v")
            .unwrap()
            .is_none(),
        "view must be gone after DROP VIEW"
    );
}

#[test]
fn drop_view_if_exists_on_missing_view_succeeds() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "DROP VIEW IF EXISTS no_such_view",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn drop_view_on_missing_view_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "DROP VIEW no_such_view",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
}

#[test]
fn drop_view_on_base_table_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err =
        common::run_ctx("DROP VIEW t", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue for DROP VIEW on base table, got {err:?}"
    );
}

#[test]
fn drop_view_multi_name() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v1 AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v2 AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "DROP VIEW v1, v2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader
        .get_table_in_database("axiomdb", "public", "v1")
        .unwrap()
        .is_none());
    assert!(reader
        .get_table_in_database("axiomdb", "public", "v2")
        .unwrap()
        .is_none());
}

// ── Transparent expansion ─────────────────────────────────────────────────────

#[test]
fn select_from_view_expands_transparently() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id, val FROM t WHERE val > 10",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT * FROM v",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let data = rows(result);
    assert_eq!(data, vec![vec![Value::Int(2), Value::Int(20)]]);
}

#[test]
fn view_in_join() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE orders (id INT, user_id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO orders VALUES (1, 10), (2, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO users VALUES (10, 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW active_orders AS SELECT id, user_id FROM orders",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT ao.id, u.name FROM active_orders ao JOIN users u ON ao.user_id = u.id ORDER BY ao.id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let data = rows(result);
    assert_eq!(
        data,
        vec![
            vec![Value::Int(1), Value::Text("Alice".into())],
            vec![Value::Int(2), Value::Text("Alice".into())],
        ]
    );
}

#[test]
fn nested_view_expansion() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v1 AS SELECT x FROM t WHERE x > 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v2 AS SELECT x FROM v1 WHERE x > 2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT * FROM v2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let data = rows(result);
    assert_eq!(data, vec![vec![Value::Int(3)]]);
}

#[test]
fn circular_view_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // We create v1 referencing v2. At CREATE time, v2 doesn't exist yet, so
    // the view body parses without error. But v2 then references v1, making a circle.
    // We create them individually to simulate the circular setup.
    common::run_ctx(
        "CREATE TABLE t (x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Force a circular view by direct catalog manipulation — not possible cleanly
    // through SQL alone since v2 must exist when v1 references it and vice versa.
    // Instead, test that querying a self-referential view returns an error.
    // We insert the circular view definition directly by running CREATE VIEW on
    // a view that happens to reference itself via the same name after CREATE:
    // Use v1 -> v2 -> v1 pattern: create v1 on t, then v2 on v1, then replace v1 to ref v2.
    common::run_ctx(
        "CREATE VIEW v1 AS SELECT x FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v2 AS SELECT x FROM v1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Replace v1 to reference v2, creating a cycle.
    common::run_ctx(
        "CREATE OR REPLACE VIEW v1 AS SELECT x FROM v2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "SELECT * FROM v1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected circular view error, got {err:?}"
    );
}

// ── SHOW CREATE VIEW ──────────────────────────────────────────────────────────

#[test]
fn show_create_view_returns_ddl() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW v AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SHOW CREATE VIEW v",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let names = col_names(&result);
    assert_eq!(names, vec!["View", "Create View"]);
    let data = rows(result);
    assert_eq!(data.len(), 1);
    let create_ddl = match &data[0][1] {
        Value::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert!(
        create_ddl.contains("CREATE VIEW"),
        "DDL must start with CREATE VIEW: {create_ddl}"
    );
    assert!(
        create_ddl.contains("SELECT"),
        "DDL must include SELECT: {create_ddl}"
    );
}

#[test]
fn show_create_view_on_table_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "SHOW CREATE VIEW t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue for SHOW CREATE VIEW on base table, got {err:?}"
    );
}

// ── information_schema.VIEWS ──────────────────────────────────────────────────

#[test]
fn information_schema_views_returns_view_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE VIEW my_view AS SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT TABLE_NAME, CHECK_OPTION, IS_UPDATABLE FROM information_schema.VIEWS",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let data = rows(result);
    assert!(!data.is_empty(), "VIEWS must have at least one row");
    let found = data.iter().any(|row| {
        row[0] == Value::Text("my_view".into())
            && row[1] == Value::Text("NONE".into())
            && row[2] == Value::Text("NO".into())
    });
    assert!(
        found,
        "my_view must appear in information_schema.VIEWS: {data:?}"
    );
}
