mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze, execute_snapshot, parse, Expr, MergeActionCondition, MergeActionKind, Stmt,
};

#[test]
fn parses_merge_update_delete_insert_do_nothing() {
    let stmt = parse(
        "MERGE INTO users AS u \
         USING (VALUES (1, 'new')) AS s(id, name) \
         ON u.id = s.id \
         WHEN MATCHED AND u.name <> s.name THEN UPDATE SET name = s.name \
         WHEN MATCHED THEN DELETE \
         WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name) \
         WHEN NOT MATCHED THEN DO NOTHING",
        None,
    )
    .unwrap();

    let Stmt::Merge(merge) = stmt else {
        panic!("expected MERGE");
    };

    assert_eq!(merge.target.name, "users");
    assert_eq!(merge.target.alias.as_deref(), Some("u"));
    assert_eq!(merge.actions.len(), 4);
    assert_eq!(merge.actions[0].condition, MergeActionCondition::Matched);
    assert!(merge.actions[0].guard.is_some());
    assert!(matches!(
        merge.actions[0].kind,
        MergeActionKind::Update(ref assignments) if assignments.len() == 1
    ));
    assert!(matches!(merge.actions[1].kind, MergeActionKind::Delete));
    assert_eq!(merge.actions[2].condition, MergeActionCondition::NotMatched);
    assert!(matches!(
        merge.actions[2].kind,
        MergeActionKind::Insert {
            columns: Some(ref columns),
            ref values,
        } if columns == &["id".to_string(), "name".to_string()] && values.len() == 2
    ));
    assert!(matches!(merge.actions[3].kind, MergeActionKind::DoNothing));
}

#[test]
fn parses_merge_not_matched_by_target() {
    let stmt = parse(
        "MERGE INTO dst USING src ON dst.id = src.id \
         WHEN NOT MATCHED BY TARGET THEN INSERT VALUES (src.id)",
        None,
    )
    .unwrap();

    let Stmt::Merge(merge) = stmt else {
        panic!("expected MERGE");
    };
    assert_eq!(merge.actions[0].condition, MergeActionCondition::NotMatched);
}

#[test]
fn merge_not_matched_by_source_is_not_implemented() {
    let err = parse(
        "MERGE INTO dst USING src ON dst.id = src.id \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
        None,
    )
    .unwrap_err();

    assert!(matches!(
        err,
        DbError::NotImplemented { ref feature }
            if feature.contains("NOT MATCHED BY SOURCE")
    ));
}

#[test]
fn merge_requires_at_least_one_when_branch() {
    let err = parse("MERGE INTO dst USING src ON dst.id = src.id", None).unwrap_err();
    assert!(matches!(err, DbError::ParseError { message, .. } if message.contains("WHEN")));
}

#[test]
fn merge_resolves_source_and_target_qualified_columns() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE TABLE src (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let stmt = parse(
        "MERGE INTO dst AS d USING src AS s ON d.id = s.id \
         WHEN MATCHED THEN UPDATE SET name = s.name \
         WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name)",
        None,
    )
    .unwrap();
    let analyzed = analyze(stmt, &storage, execute_snapshot(&txn)).unwrap();

    let Stmt::Merge(merge) = analyzed else {
        panic!("expected MERGE");
    };
    let Expr::BinaryOp { left, right, .. } = merge.on else {
        panic!("expected binary ON predicate");
    };
    assert!(matches!(*left, Expr::Column { col_idx: 0, .. }));
    assert!(matches!(*right, Expr::Column { col_idx: 2, .. }));

    let MergeActionKind::Update(assignments) = &merge.actions[0].kind else {
        panic!("expected UPDATE action");
    };
    assert!(matches!(
        assignments[0].value,
        Expr::Column { col_idx: 3, .. }
    ));

    let MergeActionKind::Insert { values, .. } = &merge.actions[1].kind else {
        panic!("expected INSERT action");
    };
    assert!(matches!(values[0], Expr::Column { col_idx: 2, .. }));
    assert!(matches!(values[1], Expr::Column { col_idx: 3, .. }));
}

#[test]
fn merge_rejects_ambiguous_unqualified_column() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE dst (id INT PRIMARY KEY)",
        &mut storage,
        &mut txn,
    );
    common::run("CREATE TABLE src (id INT)", &mut storage, &mut txn);

    let stmt = parse(
        "MERGE INTO dst USING src ON id = id \
         WHEN MATCHED THEN UPDATE SET id = src.id",
        None,
    )
    .unwrap();
    let err = analyze(stmt, &storage, execute_snapshot(&txn)).unwrap_err();
    assert!(matches!(err, DbError::AmbiguousColumn { name, .. } if name == "id"));
}

#[test]
fn merge_when_matched_updates_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 'new')) AS s(id, name) \
         ON d.id = s.id WHEN MATCHED THEN UPDATE SET name = s.name",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 1);

    let rows = common::rows(
        common::run_ctx(
            "SELECT id, name FROM dst",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![vec![
            axiomdb_types::Value::Int(1),
            axiomdb_types::Value::Text("new".into())
        ]]
    );
}

#[test]
fn merge_when_matched_deletes_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'a'), (2, 'b')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1)) AS s(id) \
         ON d.id = s.id WHEN MATCHED THEN DELETE",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 1);

    let rows = common::rows(
        common::run_ctx(
            "SELECT id FROM dst ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec![axiomdb_types::Value::Int(2)]]);
}

#[test]
fn merge_when_not_matched_inserts_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 'same'), (2, 'new')) AS s(id, name) \
         ON d.id = s.id WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 1);

    let rows = common::rows(
        common::run_ctx(
            "SELECT id, name FROM dst ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec![
                axiomdb_types::Value::Int(1),
                axiomdb_types::Value::Text("old".into())
            ],
            vec![
                axiomdb_types::Value::Int(2),
                axiomdb_types::Value::Text("new".into())
            ],
        ]
    );
}

#[test]
fn merge_unique_key_equality_updates_and_inserts() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX uq_dst_id ON dst(id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 'updated'), (2, 'inserted')) AS s(id, name) \
         ON d.id = s.id \
         WHEN MATCHED THEN UPDATE SET name = s.name \
         WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 2);

    let rows = common::rows(
        common::run_ctx(
            "SELECT id, name FROM dst ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec![
                axiomdb_types::Value::Int(1),
                axiomdb_types::Value::Text("updated".into())
            ],
            vec![
                axiomdb_types::Value::Int(2),
                axiomdb_types::Value::Text("inserted".into())
            ],
        ]
    );
}

#[test]
fn merge_action_order_first_match_wins() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 'new')) AS s(id, name) \
         ON d.id = s.id \
         WHEN MATCHED THEN DO NOTHING \
         WHEN MATCHED THEN UPDATE SET name = s.name",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 0);

    let rows = common::rows(
        common::run_ctx(
            "SELECT name FROM dst",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec![axiomdb_types::Value::Text("old".into())]]);
}

#[test]
fn merge_rejects_multiple_source_rows_for_one_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 'a'), (1, 'b')) AS s(id, name) \
         ON d.id = s.id WHEN MATCHED THEN UPDATE SET name = s.name",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::CardinalityViolation { .. }));
}
