//! Phase 21.6b — exclusion constraints backed by owned helper UNIQUE indexes.

mod common;

use axiomdb_catalog::{
    CatalogReader, ConstraintDef, ConstraintKind, IndexDef, DEFAULT_DATABASE_NAME,
};
use axiomdb_core::error::DbError;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn table_indexes_and_constraints(
    storage: &MemoryStorage,
    txn: &TxnManager,
    table_name: &str,
) -> (Vec<IndexDef>, Vec<ConstraintDef>) {
    let mut reader = CatalogReader::new(storage, txn.snapshot()).unwrap();
    let table = reader
        .get_table_in_database(DEFAULT_DATABASE_NAME, "public", table_name)
        .unwrap()
        .unwrap_or_else(|| panic!("table '{table_name}' not found"));
    let indexes = reader.list_indexes(table.id).unwrap();
    let constraints = reader.list_constraints(table.id).unwrap();
    (indexes, constraints)
}

fn scalar_count(storage: &mut MemoryStorage, txn: &mut TxnManager, sql: &str) -> i64 {
    let rows = rows(run(sql, storage, txn));
    match &rows[0][0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        other => panic!("expected integer count, got {other:?}"),
    }
}

#[test]
fn create_table_insert_conflict_surfaces_exclusion_violation() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE bookings (
            room_id INT,
            slot_id INT,
            EXCLUDE USING btree (room_id WITH =, slot_id WITH =)
        )",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO bookings VALUES (1, 10)",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "INSERT INTO bookings VALUES (1, 10)",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ExclusionViolation { ref table, ref constraint }
                if table == "bookings" && constraint == "bookings_room_id_slot_id_excl"
        ),
        "{err:?}"
    );
}

#[test]
fn exclusion_nulls_do_not_conflict() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE nullable_excl (
            slug TEXT,
            CONSTRAINT slug_excl EXCLUDE USING btree (slug WITH =)
        )",
        &mut storage,
        &mut txn,
    );

    run(
        "INSERT INTO nullable_excl VALUES (NULL)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO nullable_excl VALUES (NULL)",
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar_count(&mut storage, &mut txn, "SELECT COUNT(*) FROM nullable_excl",),
        2
    );
}

#[test]
fn update_conflict_surfaces_exclusion_violation() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE reservations (
            room_id INT,
            slot_id INT,
            CONSTRAINT room_slot_excl EXCLUDE USING btree (room_id WITH =, slot_id WITH =)
        )",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO reservations VALUES (1, 1), (1, 2)",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "UPDATE reservations SET slot_id = 1 WHERE room_id = 1 AND slot_id = 2",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ExclusionViolation { ref table, ref constraint }
                if table == "reservations" && constraint == "room_slot_excl"
        ),
        "{err:?}"
    );
}

#[test]
fn alter_add_exclusion_rejects_existing_conflict() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE schedule_slots (room_id INT, slot_id INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO schedule_slots VALUES (1, 1), (1, 1)",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "ALTER TABLE schedule_slots
         ADD CONSTRAINT room_slot_excl
         EXCLUDE USING btree (room_id WITH =, slot_id WITH =)",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ExclusionViolation { ref table, ref constraint }
                if table == "schedule_slots" && constraint == "room_slot_excl"
        ),
        "{err:?}"
    );

    let (indexes, constraints) = table_indexes_and_constraints(&storage, &txn, "schedule_slots");
    assert!(
        constraints.is_empty(),
        "unexpected constraints: {constraints:?}"
    );
    assert!(
        !indexes
            .iter()
            .any(|idx| idx.name == "__axiom_excl_idx_room_slot_excl"),
        "helper index leaked after failed ALTER ADD: {indexes:?}"
    );
}

#[test]
fn drop_constraint_removes_helper_index_and_direct_drop_is_rejected() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE docs (
            slug TEXT,
            CONSTRAINT slug_excl EXCLUDE USING btree (slug WITH =)
        )",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO docs VALUES ('intro')", &mut storage, &mut txn);

    let (indexes_before, constraints_before) =
        table_indexes_and_constraints(&storage, &txn, "docs");
    assert!(constraints_before
        .iter()
        .any(|c| c.kind == ConstraintKind::Exclusion && c.name == "slug_excl"));
    assert!(indexes_before
        .iter()
        .any(|idx| idx.name == "__axiom_excl_idx_slug_excl"));

    let err = run_result(
        "DROP INDEX __axiom_excl_idx_slug_excl ON docs",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "{err:?}");

    run(
        "ALTER TABLE docs DROP CONSTRAINT slug_excl",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO docs VALUES ('intro')", &mut storage, &mut txn);

    let (indexes_after, constraints_after) = table_indexes_and_constraints(&storage, &txn, "docs");
    assert!(
        constraints_after.is_empty(),
        "unexpected constraints after drop: {constraints_after:?}"
    );
    assert!(
        !indexes_after
            .iter()
            .any(|idx| idx.name == "__axiom_excl_idx_slug_excl"),
        "helper index still present after DROP CONSTRAINT: {indexes_after:?}"
    );
}

#[test]
fn create_table_like_copies_exclusion_constraint() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE src_docs (
            slug TEXT,
            CONSTRAINT slug_excl EXCLUDE USING btree (slug WITH =)
        )",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE dst_docs LIKE src_docs",
        &mut storage,
        &mut txn,
    );

    let (indexes, constraints) = table_indexes_and_constraints(&storage, &txn, "dst_docs");
    assert!(constraints.iter().any(|c| {
        c.kind == ConstraintKind::Exclusion
            && c.name == "slug_excl"
            && c.owned_index_id != 0
            && c.exclude_elements.len() == 1
    }));
    assert!(indexes
        .iter()
        .any(|idx| idx.name == "__axiom_excl_idx_slug_excl"));

    run(
        "INSERT INTO dst_docs VALUES ('guide')",
        &mut storage,
        &mut txn,
    );
    let err = run_result(
        "INSERT INTO dst_docs VALUES ('guide')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ExclusionViolation { ref table, ref constraint }
                if table == "dst_docs" && constraint == "slug_excl"
        ),
        "{err:?}"
    );
}

#[test]
fn information_schema_reports_exclusion_without_unique_leak() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE meta_docs (
            slug TEXT,
            CONSTRAINT slug_excl EXCLUDE USING btree (slug WITH =)
        )",
        &mut storage,
        &mut txn,
    );

    let tc_rows = rows(run(
        "SELECT CONSTRAINT_NAME, CONSTRAINT_TYPE
         FROM information_schema.TABLE_CONSTRAINTS
         WHERE TABLE_NAME = 'meta_docs'
         ORDER BY CONSTRAINT_NAME",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        tc_rows,
        vec![vec![
            Value::Text("slug_excl".into()),
            Value::Text("EXCLUSION".into()),
        ]]
    );

    let kcu_rows = rows(run(
        "SELECT CONSTRAINT_NAME, COLUMN_NAME
         FROM information_schema.KEY_COLUMN_USAGE
         WHERE TABLE_NAME = 'meta_docs'
         ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        kcu_rows,
        vec![vec![
            Value::Text("slug_excl".into()),
            Value::Text("slug".into()),
        ]]
    );
}
