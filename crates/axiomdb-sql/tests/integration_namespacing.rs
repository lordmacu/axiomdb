//! Integration tests for database and schema namespacing (Phases 22b.3a, 22b.3b, 22b.4).
//!
//! Covers: CREATE/DROP DATABASE, USE, SHOW DATABASES, cross-database 3-part
//! names, CREATE SCHEMA, SET search_path, and search-path-aware resolution.

use axiomdb_catalog::{CatalogBootstrap, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_with_defaults, bloom::BloomRegistry, execute_with_ctx, parse, QueryResult,
    SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Test helpers ──────────────────────────────────────────────────────────────

fn run_ctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze_with_defaults(
        stmt,
        storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn setup_ctx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

fn affected_count(result: QueryResult) -> u64 {
    match result {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

// ── Phase 22b.3a: database catalog ───────────────────────────────────────────

#[test]
fn test_show_databases_lists_default_and_created_database() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SHOW DATABASES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };

    let names: Vec<String> = rows
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(name) => name.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["analytics".to_string(), "axiomdb".to_string()]);
}

#[test]
fn test_use_database_switches_unqualified_resolution_namespace() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (10), (20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn test_drop_database_cascades_owned_tables_without_touching_axiomdb() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (10), (20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    run_ctx(
        "DROP DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(!reader.database_exists("analytics").unwrap());
    assert!(reader
        .get_table_in_database("analytics", "public", "events")
        .unwrap()
        .is_none());
    assert!(reader
        .get_table_in_database("axiomdb", "public", "events")
        .unwrap()
        .is_some());

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn test_drop_active_database_is_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "DROP DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::ActiveDatabaseDrop { ref name } if name == "analytics"));
}

// ── Phase 22b.3b: cross-database name resolution ─────────────────────────────

#[test]
fn test_cross_db_three_part_select() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice'), (2, 'bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id, name FROM axiomdb.public.users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("alice".into()));
}

#[test]
fn test_cross_db_three_part_insert() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE target (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE source (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO source VALUES (10, 'x'), (20, 'y')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO axiomdb.public.target SELECT * FROM source",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM target",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(2));
}

#[test]
fn test_cross_db_three_part_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE items (id INT, score INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (1, 10), (2, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE other",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE other", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let count = affected_count(
        run_ctx(
            "UPDATE axiomdb.public.items SET score = 99",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(count, 2);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT score FROM items",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert!(rows.iter().all(|r| r[0] == Value::Int(99)));
}

#[test]
fn test_cross_db_three_part_delete() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE logs (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO logs VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE other",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE other", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let count = affected_count(
        run_ctx(
            "DELETE FROM axiomdb.public.logs WHERE id > 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(count, 2);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM logs",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(1));
}

#[test]
fn test_cross_db_database_not_found_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let err = run_ctx(
        "SELECT * FROM ghost.public.events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::DatabaseNotFound { .. }),
        "expected DatabaseNotFound, got {err:?}"
    );
}

#[test]
fn test_cross_db_explicit_overrides_use() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (100), (200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM axiomdb.public.t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(1));

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(2));
}

#[test]
fn test_cross_db_unqualified_still_works() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE x (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO x VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM x",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Int(42));

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM public.x",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn test_cross_db_create_table_three_part() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE TABLE analytics.public.events (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1, 'test')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT name FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Text("test".into()));
}

// ── Phase 22b.4: schema namespacing ──────────────────────────────────────────

#[test]
fn test_create_schema_basic() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE SCHEMA inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "CREATE SCHEMA inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::SchemaAlreadyExists { .. }),
        "got {err:?}"
    );

    run_ctx(
        "CREATE SCHEMA IF NOT EXISTS inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn test_set_search_path() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    assert_eq!(ctx.current_schema(), "public");

    run_ctx(
        "CREATE SCHEMA inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'inventory, public'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.search_path, vec!["inventory", "public"]);
    assert_eq!(ctx.current_schema(), "inventory");
}

#[test]
fn test_search_path_reset_on_use_db() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE SCHEMA custom",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'custom'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.current_schema(), "custom");

    run_ctx(
        "CREATE DATABASE other",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE other", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    assert_eq!(ctx.current_schema(), "public");
}

#[test]
fn test_create_schema_per_database() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE SCHEMA myschema",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "CREATE SCHEMA IF NOT EXISTS myschema",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_ok());
}

#[test]
fn test_search_path_resolves_unqualified_in_non_public_schema() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE SCHEMA inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'inventory'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE TABLE products (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO products VALUES (1, 'widget')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT name FROM products",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Text("widget".into()));

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT name FROM inventory.products",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Text("widget".into()));
}

#[test]
fn test_search_path_fallback_to_second_schema() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE shared (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO shared VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE SCHEMA custom",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'custom, public'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM shared",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn test_show_tables_uses_current_schema() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE pub_table (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE SCHEMA inventory",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'inventory'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE inv_table (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } =
        run_ctx("SHOW TABLES", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap()
    else {
        panic!("expected rows")
    };
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str(),
            _ => panic!("expected text"),
        })
        .collect();
    assert!(names.contains(&"inv_table"), "got {names:?}");
    assert!(!names.contains(&"pub_table"), "got {names:?}");

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SHOW TABLES FROM public",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str(),
            _ => panic!("expected text"),
        })
        .collect();
    assert!(names.contains(&"pub_table"), "got {names:?}");
}

#[test]
fn test_set_search_path_empty_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let err = run_ctx(
        "SET search_path = ''",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}

#[test]
fn test_reset_search_path_via_default() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE SCHEMA custom",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET search_path = 'custom'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.current_schema(), "custom");

    run_ctx(
        "SET search_path = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.current_schema(), "public");
}
