mod common;

use axiomdb_types::Value;

use common::{affected_count, rows, run_ctx, setup_ctx};

#[test]
fn test_update_join_sets_target_from_joined_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT, role_id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE roles (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 10, 'old'), (2, 20, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO roles VALUES (10, 'admin'), (20, 'guest')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "UPDATE users u JOIN roles r ON u.role_id = r.id SET u.name = r.label WHERE r.label = 'admin'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let result = run_ctx(
        "SELECT id, name FROM users ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        rows(result),
        vec![
            vec![Value::Int(1), Value::Text("admin".into())],
            vec![Value::Int(2), Value::Text("old".into())],
        ]
    );
}

#[test]
fn test_delete_join_deletes_target_alias() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT, role_id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE roles (id INT, label TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 10, 'Alice'), (2, 20, 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO roles VALUES (10, 'active'), (20, 'inactive')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "DELETE u FROM users u JOIN roles r ON u.role_id = r.id WHERE r.label = 'inactive'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let result = run_ctx(
        "SELECT id, name FROM users ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        rows(result),
        vec![vec![Value::Int(1), Value::Text("Alice".into())]]
    );
}
