mod common;

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

fn run(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
    b: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> QueryResult {
    common::run_ctx(sql, s, t, b, ctx).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn run_err(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
    b: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> DbError {
    common::run_ctx(sql, s, t, b, ctx).expect_err(&format!("expected error for: {sql}"))
}

// ── CREATE SCHEMA + schema.table DDL/DML ─────────────────────────────────────

#[test]
fn schema_create_and_table_in_schema() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA myns", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE myns.users (id INT, name TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "INSERT INTO myns.users VALUES (1, 'alice')",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let r = run(
        "SELECT id, name FROM myns.users",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got = rows(r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Int(1));
    assert_eq!(got[0][1], Value::Text("alice".into()));
}

#[test]
fn schema_qualified_insert_update_delete() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA ns2", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE ns2.items (id INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "INSERT INTO ns2.items VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "UPDATE ns2.items SET val = 99 WHERE id = 1",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "DELETE FROM ns2.items WHERE id = 2",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let r = run(
        "SELECT id, val FROM ns2.items",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got = rows(r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Int(1));
    assert_eq!(got[0][1], Value::Int(99));
}

#[test]
fn create_table_in_nonexistent_schema_fails() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let err = run_err(
        "CREATE TABLE nosuchschema.t (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert!(
        matches!(err, DbError::SchemaNotFound { .. }),
        "expected SchemaNotFound, got {err:?}"
    );
}

// ── SET search_path ───────────────────────────────────────────────────────────

#[test]
fn set_search_path_resolves_unqualified_table() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA app", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE app.orders (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "INSERT INTO app.orders VALUES (42)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run("SET search_path = 'app'", &mut s, &mut t, &mut b, &mut ctx);
    let r = run("SELECT id FROM orders", &mut s, &mut t, &mut b, &mut ctx);
    let got = rows(r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Int(42));
}

// ── DROP SCHEMA RESTRICT ──────────────────────────────────────────────────────

#[test]
fn drop_schema_empty_succeeds() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA empty_ns", &mut s, &mut t, &mut b, &mut ctx);
    run("DROP SCHEMA empty_ns", &mut s, &mut t, &mut b, &mut ctx);
    // Re-creating it should work (not still present)
    run("CREATE SCHEMA empty_ns", &mut s, &mut t, &mut b, &mut ctx);
}

#[test]
fn drop_schema_restrict_rejects_nonempty() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA populated", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE populated.t (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let err = run_err("DROP SCHEMA populated", &mut s, &mut t, &mut b, &mut ctx);
    assert!(
        matches!(err, DbError::SchemaNotEmpty { .. }),
        "expected SchemaNotEmpty, got {err:?}"
    );
}

#[test]
fn drop_schema_restrict_keyword_rejects_nonempty() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA ns_r", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE ns_r.t (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let err = run_err(
        "DROP SCHEMA ns_r RESTRICT",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert!(
        matches!(err, DbError::SchemaNotEmpty { .. }),
        "expected SchemaNotEmpty, got {err:?}"
    );
}

// ── DROP SCHEMA CASCADE ───────────────────────────────────────────────────────

#[test]
fn drop_schema_cascade_drops_tables() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA cascade_ns", &mut s, &mut t, &mut b, &mut ctx);
    run(
        "CREATE TABLE cascade_ns.t1 (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "CREATE TABLE cascade_ns.t2 (val TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "INSERT INTO cascade_ns.t1 VALUES (1)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "DROP SCHEMA cascade_ns CASCADE",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    // Tables should be gone
    let err = run_err(
        "SELECT * FROM cascade_ns.t1",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert!(
        matches!(
            err,
            DbError::TableNotFound { .. } | DbError::SchemaNotFound { .. }
        ),
        "expected table/schema not found, got {err:?}"
    );
}

// ── DROP SCHEMA IF EXISTS ─────────────────────────────────────────────────────

#[test]
fn drop_schema_if_exists_silent_on_missing() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    // Should not error
    run(
        "DROP SCHEMA IF EXISTS nonexistent_ns",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
}

#[test]
fn drop_schema_without_if_exists_errors_on_missing() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let err = run_err(
        "DROP SCHEMA nonexistent_ns",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert!(
        matches!(err, DbError::SchemaNotFound { .. }),
        "expected SchemaNotFound, got {err:?}"
    );
}

// ── DROP SCHEMA information_schema ────────────────────────────────────────────

#[test]
fn drop_schema_information_schema_rejected() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let err = run_err(
        "DROP SCHEMA information_schema",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
}

// ── DROP SCHEMA public then recreate ─────────────────────────────────────────

#[test]
fn drop_schema_public_then_recreate() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    // `public` is always implicitly present; DROP IF EXISTS should not error.
    run(
        "DROP SCHEMA IF EXISTS public",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    // Creating tables in public still works without an explicit CREATE SCHEMA.
    run(
        "CREATE TABLE public.test (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
}

// ── SHOW SCHEMAS ─────────────────────────────────────────────────────────────

#[test]
fn show_schemas_lists_public() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let r = run("SHOW SCHEMAS", &mut s, &mut t, &mut b, &mut ctx);
    let names = col_names(&r);
    assert_eq!(names, vec!["Schema"]);
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        got.contains(&"public".to_string()),
        "expected 'public' in SHOW SCHEMAS: {got:?}"
    );
}

#[test]
fn show_schemas_after_create() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA alpha", &mut s, &mut t, &mut b, &mut ctx);
    run("CREATE SCHEMA beta", &mut s, &mut t, &mut b, &mut ctx);
    let r = run("SHOW SCHEMAS", &mut s, &mut t, &mut b, &mut ctx);
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(got.contains(&"alpha".to_string()), "missing alpha: {got:?}");
    assert!(got.contains(&"beta".to_string()), "missing beta: {got:?}");
    assert!(
        got.contains(&"public".to_string()),
        "missing public: {got:?}"
    );
}

#[test]
fn show_schemas_like_filter() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA app_main", &mut s, &mut t, &mut b, &mut ctx);
    run("CREATE SCHEMA app_test", &mut s, &mut t, &mut b, &mut ctx);
    run("CREATE SCHEMA other", &mut s, &mut t, &mut b, &mut ctx);
    let r = run("SHOW SCHEMAS LIKE 'app%'", &mut s, &mut t, &mut b, &mut ctx);
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        got.contains(&"app_main".to_string()),
        "missing app_main: {got:?}"
    );
    assert!(
        got.contains(&"app_test".to_string()),
        "missing app_test: {got:?}"
    );
    assert!(
        !got.contains(&"other".to_string()),
        "unexpected 'other': {got:?}"
    );
}

// ── information_schema.SCHEMATA ───────────────────────────────────────────────

#[test]
fn information_schema_schemata_columns() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let r = run(
        "SELECT * FROM information_schema.schemata",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let names = col_names(&r);
    assert_eq!(
        names,
        vec![
            "CATALOG_NAME",
            "SCHEMA_NAME",
            "DEFAULT_CHARACTER_SET_NAME",
            "DEFAULT_COLLATION_NAME",
            "SQL_PATH",
        ]
    );
}

#[test]
fn information_schema_schemata_includes_public() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let r = run(
        "SELECT schema_name FROM information_schema.schemata",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        got.contains(&"public".to_string()),
        "missing 'public': {got:?}"
    );
}

#[test]
fn information_schema_schemata_includes_self() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let r = run(
        "SELECT schema_name FROM information_schema.schemata",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        got.contains(&"information_schema".to_string()),
        "'information_schema' must appear in schemata: {got:?}"
    );
}

#[test]
fn information_schema_schemata_catalog_name_is_def() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    let r = run(
        "SELECT catalog_name FROM information_schema.schemata WHERE schema_name = 'public'",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got = rows(r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Text("def".into()));
}

#[test]
fn information_schema_schemata_includes_created_schema() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA myapp", &mut s, &mut t, &mut b, &mut ctx);
    let r = run(
        "SELECT schema_name FROM information_schema.schemata",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    let got: Vec<String> = rows(r)
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(v) => v.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        got.contains(&"myapp".to_string()),
        "missing 'myapp': {got:?}"
    );
}

// ── CREATE SCHEMA IF NOT EXISTS ───────────────────────────────────────────────

#[test]
fn create_schema_if_not_exists_is_idempotent() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run(
        "CREATE SCHEMA IF NOT EXISTS ns_idem",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    run(
        "CREATE SCHEMA IF NOT EXISTS ns_idem",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
}

#[test]
fn create_schema_duplicate_without_if_not_exists_fails() {
    let (mut s, mut t, mut b, mut ctx) = common::setup_ctx();
    run("CREATE SCHEMA dupschema", &mut s, &mut t, &mut b, &mut ctx);
    let err = run_err("CREATE SCHEMA dupschema", &mut s, &mut t, &mut b, &mut ctx);
    assert!(
        matches!(err, DbError::SchemaAlreadyExists { .. }),
        "expected SchemaAlreadyExists, got {err:?}"
    );
}
