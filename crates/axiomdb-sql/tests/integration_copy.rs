mod common;

use axiomdb_types::Value;
use common::*;

// ── helpers ───────────────────────────────────────────────────────────────────

fn tmpfile(suffix: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("axtest_{}_{}", std::process::id(), suffix));
    p
}

// ── COPY FROM CSV ─────────────────────────────────────────────────────────────

#[test]
fn copy_from_csv_basic() {
    let path = tmpfile("basic.csv");
    std::fs::write(&path, "id,name\n1,alice\n2,bob\n").unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY users FROM '{path_str}' WITH (FORMAT CSV, HEADER TRUE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT id, name FROM users ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0], vec![Value::Int(1), Value::Text("alice".into())]);
    assert_eq!(got[1], vec![Value::Int(2), Value::Text("bob".into())]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_csv_no_header() {
    let path = tmpfile("noheader.csv");
    std::fs::write(&path, "10,foo\n20,bar\n").unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (x INT, y TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY t FROM '{path_str}' WITH (FORMAT CSV, HEADER FALSE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT x, y FROM t ORDER BY x",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0][0], Value::Int(10));
    assert_eq!(got[1][0], Value::Int(20));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_csv_auto_detect_format() {
    let path = tmpfile("auto.csv");
    std::fs::write(&path, "id,val\n7,hello\n").unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t2 (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // No WITH clause — extension .csv should auto-detect
    let r = run_ctx(
        &format!("COPY t2 FROM '{path_str}'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 1);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_csv_null_value() {
    let path = tmpfile("nulls.csv");
    std::fs::write(&path, "id,val\n1,\\N\n2,real\n").unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t3 (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        &format!("COPY t3 FROM '{path_str}' WITH (FORMAT CSV)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT val FROM t3 ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0][0], Value::Null);
    assert_eq!(got[1][0], Value::Text("real".into()));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_csv_custom_delimiter() {
    let path = tmpfile("pipe.csv");
    std::fs::write(&path, "id|name\n5|eve\n6|frank\n").unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t4 (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY t4 FROM '{path_str}' WITH (FORMAT CSV, HEADER TRUE, DELIMITER '|')"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT name FROM t4 ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0][0], Value::Text("eve".into()));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_csv_empty_file() {
    let path = tmpfile("empty.csv");
    std::fs::write(&path, "id,name\n").unwrap(); // header only
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t5 (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY t5 FROM '{path_str}' WITH (FORMAT CSV, HEADER TRUE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 0);
    let _ = std::fs::remove_file(&path);
}

// ── COPY FROM JSON ────────────────────────────────────────────────────────────

#[test]
fn copy_from_json_array() {
    let path = tmpfile("data.json");
    std::fs::write(&path, r#"[{"id":1,"name":"alice"},{"id":2,"name":"bob"}]"#).unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE jt (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY jt FROM '{path_str}' WITH (FORMAT JSON)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT id, name FROM jt ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0], vec![Value::Int(1), Value::Text("alice".into())]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_jsonl() {
    let path = tmpfile("data.jsonl");
    std::fs::write(
        &path,
        "{\"id\":10,\"score\":3.14}\n{\"id\":20,\"score\":2.71}\n",
    )
    .unwrap();
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE jl (id INT, score REAL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY jl FROM '{path_str}' WITH (FORMAT JSONL)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT id FROM jl ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0][0], Value::Int(10));
    assert_eq!(got[1][0], Value::Int(20));
    let _ = std::fs::remove_file(&path);
}

// ── COPY TO ───────────────────────────────────────────────────────────────────

#[test]
fn copy_to_csv_produces_file() {
    let out = tmpfile("out.csv");
    let out_str = out.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE exp (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO exp VALUES (1, 'a'), (2, 'b')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        &format!("COPY exp TO '{out_str}' WITH (FORMAT CSV, HEADER TRUE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let content = std::fs::read_to_string(&out).unwrap();
    assert!(content.contains("id"), "CSV should have header");
    assert!(content.contains("1"), "CSV should have row data");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn copy_to_json_produces_array() {
    let out = tmpfile("out.json");
    let out_str = out.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE ej (k INT, v TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO ej VALUES (42, 'hello')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        &format!("COPY ej TO '{out_str}' WITH (FORMAT JSON)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let content = std::fs::read_to_string(&out).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["k"], serde_json::json!(42));
    assert_eq!(arr[0]["v"], serde_json::json!("hello"));
    let _ = std::fs::remove_file(&out);
}

#[test]
fn copy_to_jsonl_one_object_per_line() {
    let out = tmpfile("out.jsonl");
    let out_str = out.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE ejl (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO ejl VALUES (1, 'x'), (2, 'y'), (3, 'z')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        &format!("COPY ejl TO '{out_str}' WITH (FORMAT JSONL)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let content = std::fs::read_to_string(&out).unwrap();
    let line_count = content.lines().count();
    assert_eq!(line_count, 3, "JSONL should have one line per row");
    for line in content.lines() {
        let _: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON line: {line}"));
    }
    let _ = std::fs::remove_file(&out);
}

// ── Round-trip ────────────────────────────────────────────────────────────────

#[test]
fn roundtrip_csv() {
    let path = tmpfile("rt.csv");
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE src (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE dst (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO src VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Export
    run_ctx(
        &format!("COPY src TO '{path_str}' WITH (FORMAT CSV, HEADER TRUE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Import
    let r = run_ctx(
        &format!("COPY dst FROM '{path_str}' WITH (FORMAT CSV, HEADER TRUE)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 3);

    let result = run_ctx(
        "SELECT id, val FROM dst ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0][1], Value::Text("alpha".into()));
    assert_eq!(got[2][1], Value::Text("gamma".into()));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn roundtrip_jsonl() {
    let path = tmpfile("rt.jsonl");
    let path_str = path.to_str().unwrap();

    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE jsrc (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE jdst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO jsrc VALUES (1, 'one'), (2, 'two')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        &format!("COPY jsrc TO '{path_str}' WITH (FORMAT JSONL)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = run_ctx(
        &format!("COPY jdst FROM '{path_str}' WITH (FORMAT JSONL)"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(r), 2);

    let result = run_ctx(
        "SELECT name FROM jdst ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let got = rows(result);
    assert_eq!(got[0][0], Value::Text("one".into()));
    assert_eq!(got[1][0], Value::Text("two".into()));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_from_missing_file_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE err_t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result = run_ctx(
        "COPY err_t FROM '/nonexistent/path/file.csv'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(result.is_err(), "should error on missing file");
}
