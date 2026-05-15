//! Integration tests for HTTP Foreign Data Wrapper (Phase 22b.2).
//!
//! Covers: CREATE/DROP SERVER, CREATE/DROP FOREIGN TABLE, IS virtual tables,
//! catalog lifecycle, error cases, and live HTTP scan via a local mock server.

mod common;

use axiomdb_types::Value;
use common::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

// ── helpers ──────────────────────────────────────────────────────────────────

fn ok(sql: &str, storage: &mut axiomdb_storage::MemoryStorage, txn: &mut axiomdb_wal::TxnManager) {
    run(sql, storage, txn);
}

fn err_contains(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    needle: &str,
) {
    let e = run_result(sql, storage, txn).expect_err(&format!("expected error for: {sql}"));
    let msg = format!("{e:?}");
    assert!(
        msg.contains(needle),
        "error message '{msg}' should contain '{needle}'"
    );
}

/// Spin up a minimal HTTP/1.1 server on 127.0.0.1:0 that replies with the
/// given JSON body for every request. Returns the bound port.
fn spawn_mock_http_server(json_body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        // Serve exactly one request then exit.
        if let Ok((mut stream, _)) = listener.accept() {
            // Drain the HTTP request.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            // Send response.
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                json_body.len(),
                json_body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

// ── CREATE SERVER ─────────────────────────────────────────────────────────────

#[test]
fn test_create_server_basic() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_create_server_with_timeout() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999', timeout_ms '5000')",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_create_server_if_not_exists() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    // Should not fail with IF NOT EXISTS.
    ok(
        "CREATE SERVER IF NOT EXISTS myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_create_server_duplicate_fails() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    err_contains(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
        "already exists",
    );
}

// ── DROP SERVER ───────────────────────────────────────────────────────────────

#[test]
fn test_drop_server_basic() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok("DROP SERVER myapi", &mut storage, &mut txn);
}

#[test]
fn test_drop_server_if_exists() {
    let (mut storage, mut txn) = setup();
    // Should not fail even if server does not exist.
    ok("DROP SERVER IF EXISTS ghost", &mut storage, &mut txn);
}

#[test]
fn test_drop_server_not_found_fails() {
    let (mut storage, mut txn) = setup();
    err_contains(
        "DROP SERVER ghost",
        &mut storage,
        &mut txn,
        "does not exist",
    );
}

#[test]
fn test_drop_server_then_recreate() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok("DROP SERVER myapi", &mut storage, &mut txn);
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
}

// ── CREATE FOREIGN TABLE ──────────────────────────────────────────────────────

#[test]
fn test_create_foreign_table_basic() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT, active BOOLEAN) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_create_foreign_table_if_not_exists() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE IF NOT EXISTS ft_users (id INT, name TEXT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_create_foreign_table_duplicate_fails() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );
    err_contains(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
        "already exists",
    );
}

// ── DROP FOREIGN TABLE ────────────────────────────────────────────────────────

#[test]
fn test_drop_foreign_table_basic() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );
    ok("DROP FOREIGN TABLE ft_users", &mut storage, &mut txn);
}

#[test]
fn test_drop_foreign_table_if_exists() {
    let (mut storage, mut txn) = setup();
    ok(
        "DROP FOREIGN TABLE IF EXISTS ft_users",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_drop_foreign_table_not_found_fails() {
    let (mut storage, mut txn) = setup();
    err_contains(
        "DROP FOREIGN TABLE ft_users",
        &mut storage,
        &mut txn,
        "does not exist",
    );
}

// ── information_schema ────────────────────────────────────────────────────────

#[test]
fn test_is_foreign_servers_empty_initially() {
    let (mut storage, mut txn) = setup();
    let result = run_result(
        "SELECT SERVER_NAME FROM information_schema.foreign_servers",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert!(r.is_empty(), "no servers initially");
}

#[test]
fn test_is_foreign_servers_after_create() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER svc1 FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE SERVER svc2 FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:8888')",
        &mut storage,
        &mut txn,
    );

    let result = run_result(
        "SELECT SERVER_NAME FROM information_schema.foreign_servers ORDER BY SERVER_NAME",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("svc1".into()));
    assert_eq!(r[1][0], Value::Text("svc2".into()));
}

#[test]
fn test_is_foreign_servers_after_drop() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER svc1 FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok("DROP SERVER svc1", &mut storage, &mut txn);

    let result = run_result(
        "SELECT SERVER_NAME FROM information_schema.foreign_servers",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert!(r.is_empty(), "server should be gone after drop");
}

#[test]
fn test_is_foreign_tables_after_create() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT) SERVER myapi OPTIONS (endpoint '/users')",
        &mut storage,
        &mut txn,
    );

    let result = run_result(
        "SELECT TABLE_NAME, SERVER_NAME FROM information_schema.foreign_tables",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("ft_users".into()));
    assert_eq!(r[0][1], Value::Text("myapi".into()));
}

#[test]
fn test_is_foreign_tables_column_count() {
    let (mut storage, mut txn) = setup();
    ok(
        "CREATE SERVER myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://localhost:9999')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_items (id INT, sku TEXT, price FLOAT, in_stock BOOLEAN) SERVER myapi OPTIONS (endpoint '/items')",
        &mut storage,
        &mut txn,
    );

    let result = run_result(
        "SELECT COLUMN_COUNT FROM information_schema.foreign_tables WHERE TABLE_NAME = 'ft_items'",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(4));
}

// ── HTTP scan (live mock server) ─────────────────────────────────────────────

#[test]
fn test_select_from_foreign_table_returns_rows() {
    let json = r#"[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]"#;
    let port = spawn_mock_http_server(json);

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_people (id INT, name TEXT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result(
        "SELECT id, name FROM ft_people ORDER BY id",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 2, "expected 2 rows from mock server");
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("Alice".into()));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[1][1], Value::Text("Bob".into()));
}

#[test]
fn test_select_from_foreign_table_empty_array() {
    let port = spawn_mock_http_server("[]");

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_empty (id INT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result("SELECT id FROM ft_empty", &mut storage, &mut txn).unwrap();
    let r = rows(result);
    assert!(r.is_empty(), "expected 0 rows for empty JSON array");
}

#[test]
fn test_select_from_foreign_table_null_fields() {
    let json = r#"[{"id":1,"name":null}]"#;
    let port = spawn_mock_http_server(json);

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_nulls (id INT, name TEXT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result("SELECT id, name FROM ft_nulls", &mut storage, &mut txn).unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Null);
}

#[test]
fn test_select_from_foreign_table_missing_column_is_null() {
    // JSON objects don't have the "name" field.
    let json = r#"[{"id":10}]"#;
    let port = spawn_mock_http_server(json);

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_partial (id INT, name TEXT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result("SELECT id, name FROM ft_partial", &mut storage, &mut txn).unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(10));
    assert_eq!(r[0][1], Value::Null);
}

#[test]
fn test_select_count_from_foreign_table() {
    let json = r#"[{"id":1},{"id":2},{"id":3}]"#;
    let port = spawn_mock_http_server(json);

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_cnt (id INT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result("SELECT COUNT(*) FROM ft_cnt", &mut storage, &mut txn).unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(3));
}

#[test]
fn test_select_from_foreign_table_with_where_filter() {
    let json = r#"[{"id":1,"score":10},{"id":2,"score":99},{"id":3,"score":5}]"#;
    let port = spawn_mock_http_server(json);

    let (mut storage, mut txn) = setup();
    let create_server = format!(
        "CREATE SERVER localapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{}')",
        port
    );
    ok(&create_server, &mut storage, &mut txn);
    ok(
        "CREATE FOREIGN TABLE ft_scores (id INT, score INT) SERVER localapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );

    let result = run_result(
        "SELECT id FROM ft_scores WHERE score > 50",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

// ── error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_select_from_foreign_table_connection_refused_errors() {
    let (mut storage, mut txn) = setup();
    // Port 1 is reserved/unreachable on all platforms.
    ok(
        "CREATE SERVER deadapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:1')",
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_dead (id INT) SERVER deadapi OPTIONS (endpoint '/')",
        &mut storage,
        &mut txn,
    );
    let e = run_result("SELECT id FROM ft_dead", &mut storage, &mut txn)
        .expect_err("expected error when connecting to closed port");
    let msg = format!("{e:?}");
    assert!(
        msg.contains("connect") || msg.contains("Connection") || msg.contains("refused"),
        "expected connection error, got: {msg}"
    );
}
