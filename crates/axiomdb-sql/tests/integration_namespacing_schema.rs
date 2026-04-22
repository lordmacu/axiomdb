//! Integration tests for schema namespacing (Phase 22b.4).
//!
//! Covers: CREATE SCHEMA, SET search_path, SHOW TABLES, and schema-aware
//! unqualified resolution.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

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

#[test]
fn test_temp_table_shadows_public_table_for_unqualified_resolution() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE accounts (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO accounts VALUES (1, 'public')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE TEMP TABLE accounts (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO accounts VALUES (2, 'temp')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT label FROM accounts",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::Text("temp".into())]]);

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT label FROM public.accounts",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::Text("public".into())]]);

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
    assert_eq!(
        names.iter().filter(|name| **name == "accounts").count(),
        1,
        "temporary shadow must not duplicate SHOW TABLES rows: {names:?}"
    );
}

#[test]
fn test_regular_create_uses_non_temp_default_schema_even_when_temp_active() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TEMP TABLE temp_anchor (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.temp_schema_name().is_some(),
        "temp schema should be allocated after CREATE TEMP TABLE"
    );

    run_ctx(
        "CREATE TABLE regular_after_temp (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO public.regular_after_temp VALUES (7)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM public.regular_after_temp",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::Int(7)]]);
}

#[test]
fn test_drop_table_prefers_temp_shadow_before_public_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE shadow_me (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO shadow_me VALUES (1, 'public')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TEMP TABLE shadow_me (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO shadow_me VALUES (2, 'temp')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "DROP TABLE shadow_me",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT label FROM shadow_me",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::Text("public".into())]]);
}

#[test]
fn test_temp_table_rejects_explicit_schema_or_database() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let err = run_ctx(
        "CREATE TEMP TABLE public.bad_temp (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = run_ctx(
        "CREATE TEMP TABLE analytics.public.bad_temp2 (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}
