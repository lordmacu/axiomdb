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

/// Spin up a mock HTTP/1.1 server that:
/// - Accepts exactly one connection
/// - Sends the first request line ("GET /path?... HTTP/1.1") via `captured_tx`
/// - Responds with `json_body`
/// Returns the bound port.
fn spawn_capturing_mock_server(
    json_body: &'static str,
    captured_tx: std::sync::mpsc::SyncSender<String>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("").to_string();
            let _ = captured_tx.send(first_line);
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

// ── Phase 22b.6: pushdown integration tests ───────────────────────────────────

#[test]
fn test_pushdown_path_placeholder_int() {
    // endpoint '/users/{id}', WHERE id = 5 → GET /users/5
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(r#"[{"id":5,"name":"alice"}]"#, tx);
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_users (id INT, name TEXT) SERVER api OPTIONS (endpoint '/users/{id}')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT id, name FROM ft_users WHERE id = 5",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    assert!(
        captured.contains("/users/5"),
        "expected path /users/5 in request, got: {captured}"
    );
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], axiomdb_types::Value::Int(5));
}

#[test]
fn test_pushdown_query_param_single() {
    // pushdown_cols 'status', WHERE status = 'active' → GET /orders?status=active
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(
        r#"[{"id":1,"status":"active"},{"id":2,"status":"active"}]"#,
        tx,
    );
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_orders (id INT, status TEXT) SERVER api OPTIONS (endpoint '/orders', pushdown_cols 'status')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT id FROM ft_orders WHERE status = 'active'",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    assert!(
        captured.contains("status=active"),
        "expected ?status=active in URL, got: {captured}"
    );
    let r = rows(result);
    assert_eq!(r.len(), 2);
}

#[test]
fn test_pushdown_limit_param() {
    // limit_param 'per_page', LIMIT 2 → GET /items?per_page=2
    // Remote returns 5 rows; local LIMIT 2 is still applied → 2 returned
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(r#"[{"id":1},{"id":2},{"id":3},{"id":4},{"id":5}]"#, tx);
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_items (id INT) SERVER api OPTIONS (endpoint '/items', limit_param 'per_page')",
        &mut storage,
        &mut txn,
    );
    let result = run_result("SELECT id FROM ft_items LIMIT 2", &mut storage, &mut txn).unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    assert!(
        captured.contains("per_page=2"),
        "expected ?per_page=2 in URL, got: {captured}"
    );
    // Local LIMIT still applied — at most 2 rows even though remote returned 5.
    assert!(rows(result).len() <= 2);
}

#[test]
fn test_pushdown_mixed_pushed_and_residual() {
    // pushdown_cols 'status', WHERE status = 'active' AND id > 1
    // → status pushed to URL; id > 1 applied locally
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // Remote returns 3 rows all with status=active, ids 1,2,3
    let port = spawn_capturing_mock_server(
        r#"[{"id":1,"status":"active"},{"id":2,"status":"active"},{"id":3,"status":"active"}]"#,
        tx,
    );
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_mixed (id INT, status TEXT) SERVER api OPTIONS (endpoint '/rows', pushdown_cols 'status')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT id FROM ft_mixed WHERE status = 'active' AND id > 1",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    // status pushed to URL
    assert!(
        captured.contains("status=active"),
        "expected status=active in URL, got: {captured}"
    );
    let r = rows(result);
    // Local residual id > 1 filtered out id=1
    assert_eq!(r.len(), 2);
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            axiomdb_types::Value::Int(n) => n,
            _ => -1,
        })
        .collect();
    assert!(
        !ids.contains(&1),
        "id=1 should have been filtered out locally"
    );
}

#[test]
fn test_pushdown_or_not_pushed() {
    // WHERE status = 'active' OR id = 1 — OR is never pushed; URL unchanged
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(
        r#"[{"id":1,"status":"inactive"},{"id":2,"status":"active"}]"#,
        tx,
    );
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_or (id INT, status TEXT) SERVER api OPTIONS (endpoint '/rows', pushdown_cols 'status')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT id FROM ft_or WHERE status = 'active' OR id = 1",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    // OR not pushed — URL should NOT have status= query param
    assert!(
        !captured.contains("status="),
        "OR predicate should not be pushed to URL, got: {captured}"
    );
    // Both rows match (id=1 OR status=active)
    assert_eq!(rows(result).len(), 2);
}

#[test]
fn test_pushdown_no_config_url_unchanged() {
    // No pushdown_cols, no placeholder, no limit_param → URL is exactly base+endpoint
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(r#"[{"id":42,"name":"test"}]"#, tx);
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_plain (id INT, name TEXT) SERVER api OPTIONS (endpoint '/plain')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT id FROM ft_plain WHERE id = 42",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    // URL must be just GET /plain HTTP/1.1 — no query params appended
    assert!(
        captured.starts_with("GET /plain HTTP"),
        "expected clean /plain URL, got: {captured}"
    );
    assert_eq!(rows(result).len(), 1);
}

#[test]
fn test_pushdown_placeholder_not_duplicated_as_query_param() {
    // {id} in path AND in pushdown_cols → only in path, NOT as ?id=...
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port = spawn_capturing_mock_server(r#"[{"id":7,"name":"bob"}]"#, tx);
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_dedup (id INT, name TEXT) SERVER api OPTIONS (endpoint '/users/{id}', pushdown_cols 'id')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT name FROM ft_dedup WHERE id = 7",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    // Path has /users/7
    assert!(
        captured.contains("/users/7"),
        "expected /users/7 in URL, got: {captured}"
    );
    // id should NOT appear as a query param
    assert!(
        !captured.contains("id="),
        "id should not be duplicated as query param, got: {captured}"
    );
    assert_eq!(rows(result).len(), 1);
}

#[test]
fn test_pushdown_unbound_placeholder_left_as_literal() {
    // endpoint '/users/{id}', WHERE name = 'alice' (no id predicate)
    // Placeholder {id} stays as-is; local WHERE applied
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let port =
        spawn_capturing_mock_server(r#"[{"id":1,"name":"alice"},{"id":2,"name":"bob"}]"#, tx);
    let (mut storage, mut txn) = setup();
    ok(
        &format!(
            "CREATE SERVER api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{port}')"
        ),
        &mut storage,
        &mut txn,
    );
    ok(
        "CREATE FOREIGN TABLE ft_unbound (id INT, name TEXT) SERVER api OPTIONS (endpoint '/users/{id}')",
        &mut storage,
        &mut txn,
    );
    let result = run_result(
        "SELECT name FROM ft_unbound WHERE name = 'alice'",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("no request captured");
    // Placeholder was not substituted — {id} should appear literally OR percent-encoded
    assert!(
        captured.contains("/users/%7Bid%7D") || captured.contains("/users/{id}"),
        "expected unresolved placeholder in URL, got: {captured}"
    );
    let r = rows(result);
    // Local filter keeps only alice
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], axiomdb_types::Value::Text("alice".into()));
}
