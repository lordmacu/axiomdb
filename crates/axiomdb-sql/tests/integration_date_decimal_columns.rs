mod common;

use axiomdb_types::Value;

#[test]
fn date_column_insert_from_text_and_select() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (d DATE)", &mut storage, &mut txn);
    common::run(
        "INSERT INTO t VALUES ('1970-01-02')",
        &mut storage,
        &mut txn,
    );
    let out = common::rows(common::run("SELECT d FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Date(1)]]);
}

#[test]
fn decimal_column_insert_from_text_and_where_compare() {
    let (mut storage, mut txn) = common::setup();
    // Use DECIMAL(10,2) so '123.45' stores exactly; bare DECIMAL(10,0) would round to 123.
    common::run("CREATE TABLE t (x DECIMAL(10,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('123.45')", &mut storage, &mut txn);
    let out = common::rows(common::run(
        "SELECT x FROM t WHERE x = '123.45'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Decimal(12345, 2)]]);
}
