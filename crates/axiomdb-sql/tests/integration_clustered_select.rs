mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_sql::{key_encoding::encode_index_key, table::lookup_clustered_row};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{rows, run, run_ctx, setup, setup_ctx};

fn insert_users(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)",
        storage,
        txn,
    );
    run("INSERT INTO users VALUES (1, 'Alice', 30)", storage, txn);
    run("INSERT INTO users VALUES (2, 'Bob', 25)", storage, txn);
    run("INSERT INTO users VALUES (3, 'Charlie', 35)", storage, txn);
    run("INSERT INTO users VALUES (4, 'Diana', 28)", storage, txn);
    run("INSERT INTO users VALUES (5, 'Eve', 22)", storage, txn);
}

#[test]
fn select_all_from_clustered_returns_all_rows_in_pk_order() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run("SELECT * FROM users", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 5);
    // Clustered scan returns rows in PK order.
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
    assert_eq!(r[3][0], Value::Int(4));
    assert_eq!(r[4][0], Value::Int(5));
    // Verify full row content.
    assert_eq!(r[0][1], Value::Text("Alice".into()));
    assert_eq!(r[0][2], Value::Int(30));
}

#[test]
fn select_with_pk_point_lookup() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run("SELECT * FROM users WHERE id = 3", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
    assert_eq!(r[0][1], Value::Text("Charlie".into()));
    assert_eq!(r[0][2], Value::Int(35));
}

#[test]
fn select_with_pk_range() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run(
        "SELECT * FROM users WHERE id >= 2 AND id < 5",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[1][0], Value::Int(3));
    assert_eq!(r[2][0], Value::Int(4));
}

#[test]
fn select_with_non_pk_where_full_scan_filter() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run(
        "SELECT id, name FROM users WHERE age > 27",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 3); // Alice(30), Charlie(35), Diana(28)
                            // Verify projected columns (id, name only).
    let ids: Vec<Value> = r.iter().map(|row| row[0].clone()).collect();
    assert!(ids.contains(&Value::Int(1))); // Alice
    assert!(ids.contains(&Value::Int(3))); // Charlie
    assert!(ids.contains(&Value::Int(4))); // Diana
}

#[test]
fn select_count_star_from_clustered() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run("SELECT COUNT(*) FROM users", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(5));
}

#[test]
fn select_with_order_by_pk_returns_correct_order() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run(
        "SELECT id FROM users ORDER BY id DESC",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], Value::Int(5));
    assert_eq!(r[4][0], Value::Int(1));
}

#[test]
fn select_with_limit() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run("SELECT * FROM users LIMIT 3", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 3);
}

#[test]
fn select_with_group_by_on_clustered() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE scores (id INT PRIMARY KEY, team TEXT, score INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (1, 'A', 10)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (2, 'B', 20)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (3, 'A', 30)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (4, 'B', 40)",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "SELECT team, SUM(score) FROM scores GROUP BY team ORDER BY team",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("A".into()));
    assert_eq!(r[0][1], Value::Int(40)); // 10 + 30 (SUM of INT → INT)
    assert_eq!(r[1][0], Value::Text("B".into()));
    assert_eq!(r[1][1], Value::Int(60)); // 20 + 40
}

#[test]
fn select_empty_clustered_table() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE empty_t (id INT PRIMARY KEY, val TEXT)",
        &mut storage,
        &mut txn,
    );

    let result = run("SELECT * FROM empty_t", &mut storage, &mut txn);
    let r = rows(result);
    assert!(r.is_empty());
}

#[test]
fn select_pk_miss_returns_empty() {
    let (mut storage, mut txn) = setup();
    insert_users(&mut storage, &mut txn);

    let result = run("SELECT * FROM users WHERE id = 999", &mut storage, &mut txn);
    let r = rows(result);
    assert!(r.is_empty());
}

#[test]
fn select_after_many_inserts_and_splits() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
    );
    for id in 0..200 {
        let body = "x".repeat(200 + (id % 5) * 40);
        run(
            &format!("INSERT INTO docs VALUES ({id}, '{body}')"),
            &mut storage,
            &mut txn,
        );
    }

    // Full scan.
    let result = run("SELECT COUNT(*) FROM docs", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::BigInt(200));

    // Point lookup.
    let result = run("SELECT * FROM docs WHERE id = 137", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(137));

    // Range scan.
    let result = run(
        "SELECT id FROM docs WHERE id >= 50 AND id < 60",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 10);
}

#[test]
fn select_secondary_lookup_on_clustered_uses_bookmark_path_in_ctx_mode() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 'bob@example.com', 'Bob')",
        &mut storage,
        &mut txn,
    );

    let result = run_ctx(
        "SELECT email FROM users WHERE email = 'bob@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r, vec![vec![Value::Text("bob@example.com".into())]]);
}

#[test]
fn select_pk_lookup_on_clustered_still_works_when_ctx_planner_wants_covering_scan() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    insert_users(&mut storage, &mut txn);

    let result = run_ctx(
        "SELECT id FROM users WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r, vec![vec![Value::Int(3)]]);
}

#[test]
fn select_secondary_range_on_clustered_returns_matching_rows() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT UNIQUE)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice', 25)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 'Bob', 28)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (3, 'Charlie', 30)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (4, 'Diana', 35)",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "SELECT id FROM users WHERE age >= 28 AND age <= 30 ORDER BY id",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r, vec![vec![Value::Int(2)], vec![Value::Int(3)],]);
}

#[test]
fn clustered_select_hides_uncommitted_rows_from_older_snapshot() {
    let (mut storage, mut writer) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut writer,
    );

    let snap_before = writer.snapshot();
    let mut reader = CatalogReader::new(&storage, snap_before).unwrap();
    let table = reader.get_table("public", "users").unwrap().unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    let key = encode_index_key(&[Value::Int(1)]).unwrap();

    writer.begin().unwrap();
    run(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut writer,
    );

    assert!(
        lookup_clustered_row(&storage, &table, &columns, &key, snap_before)
            .unwrap()
            .is_none(),
        "snapshot opened before the clustered insert must not see the uncommitted row"
    );

    writer.commit().unwrap();
    let visible =
        lookup_clustered_row(&storage, &table, &columns, &key, writer.snapshot()).unwrap();
    let (_, values) = visible.expect("committed clustered row must be visible");
    assert_eq!(values, vec![Value::Int(1), Value::Text("Alice".into())]);
}
