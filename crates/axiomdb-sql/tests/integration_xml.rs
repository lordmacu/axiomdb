//! Phase 20.20 — XMLType integration tests.

mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar(result: QueryResult) -> Value {
    let mut r = rows(result);
    assert_eq!(r.len(), 1, "expected exactly 1 row");
    assert_eq!(r[0].len(), 1, "expected exactly 1 column");
    r.remove(0).remove(0)
}

fn run(sql: &str) -> Value {
    let (mut storage, mut txn) = common::setup();
    scalar(common::run(sql, &mut storage, &mut txn))
}

fn run_err(sql: &str) -> axiomdb_core::error::DbError {
    let (mut storage, mut txn) = common::setup();
    common::run_result(sql, &mut storage, &mut txn).expect_err("expected an error")
}

// ── Step 1: Core type ─────────────────────────────────────────────────────────

#[test]
fn create_table_with_xml_column() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (id INT, doc XML)", &mut storage, &mut txn);
    common::run(
        "INSERT INTO t VALUES (1, '<root><a>hello</a></root>')",
        &mut storage,
        &mut txn,
    );
    let result = common::run("SELECT doc FROM t WHERE id = 1", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::Xml("<root><a>hello</a></root>".into()));
}

#[test]
fn xml_xmltype_keyword_alias() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (id INT, doc XMLTYPE)", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES (1, '<a/>')", &mut storage, &mut txn);
    let result = common::run("SELECT doc FROM t", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::Xml("<a/>".into()));
}

#[test]
fn xml_cast_valid() {
    assert_eq!(run("SELECT CAST('<root/>' AS XML)"), Value::Xml("<root/>".into()));
}

#[test]
fn xml_cast_colon_syntax() {
    assert_eq!(run("SELECT '<root/>'::XML"), Value::Xml("<root/>".into()));
}

#[test]
fn xml_cast_invalid_returns_error() {
    let err = run_err("SELECT CAST('<bad' AS XML)");
    assert!(
        matches!(err, axiomdb_core::error::DbError::InvalidCoercion { .. }),
        "expected InvalidCoercion, got: {err:?}"
    );
}

#[test]
fn xml_cast_empty_returns_error() {
    let err = run_err("SELECT CAST('' AS XML)");
    assert!(
        matches!(err, axiomdb_core::error::DbError::InvalidCoercion { .. }),
        "expected InvalidCoercion, got: {err:?}"
    );
}

#[test]
fn xml_to_text_cast() {
    assert_eq!(run("SELECT CAST('<a/>'::XML AS TEXT)"), Value::Text("<a/>".into()));
}

#[test]
fn xml_is_well_formed_ok() {
    assert_eq!(run("SELECT xml_is_well_formed('<a/>')"), Value::Int(1));
}

#[test]
fn xml_is_well_formed_full_document() {
    assert_eq!(
        run("SELECT xml_is_well_formed('<?xml version=\"1.0\"?><root/>')"),
        Value::Int(1)
    );
}

#[test]
fn xml_is_well_formed_bad() {
    assert_eq!(run("SELECT xml_is_well_formed('<broken')"), Value::Int(0));
}

#[test]
fn xml_is_well_formed_null() {
    assert_eq!(run("SELECT xml_is_well_formed(NULL)"), Value::Null);
}

#[test]
fn xml_null_propagation() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (id INT, doc XML)", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES (2, NULL)", &mut storage, &mut txn);
    let result = common::run("SELECT doc FROM t WHERE id = 2", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn xml_declaration_accepted() {
    let doc = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><root/>";
    let v = run(&format!("SELECT CAST('{doc}' AS XML)"));
    assert_eq!(v, Value::Xml(doc.into()));
}

#[test]
fn xml_multi_row_insert_select() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (id INT, doc XML)", &mut storage, &mut txn);
    common::run(
        "INSERT INTO t VALUES (1, '<a/>'), (2, '<b/>'), (3, '<c/>')",
        &mut storage,
        &mut txn,
    );
    let result = common::run("SELECT COUNT(*) FROM t", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::BigInt(3));
}

// ── Step 2: XMLELEMENT ────────────────────────────────────────────────────────

#[test]
fn xmlelement_simple() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME a, 'hello')"),
        Value::Xml("<a>hello</a>".into())
    );
}

#[test]
fn xmlelement_attrs() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME a, XMLATTRIBUTES('42' AS id), 'text')"),
        Value::Xml("<a id=\"42\">text</a>".into())
    );
}

#[test]
fn xmlelement_escaping() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME a, '<>&')"),
        Value::Xml("<a>&lt;&gt;&amp;</a>".into())
    );
}

#[test]
fn xmlelement_null_content_omitted() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME a, NULL)"),
        Value::Xml("<a></a>".into())
    );
}

#[test]
fn xmlelement_null_attr_omitted() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME a, XMLATTRIBUTES(NULL AS id), 'x')"),
        Value::Xml("<a>x</a>".into())
    );
}

#[test]
fn xmlelement_nested() {
    assert_eq!(
        run("SELECT XMLELEMENT(NAME elem1, XMLELEMENT(NAME elem2, 'val'))"),
        Value::Xml("<elem1><elem2>val</elem2></elem1>".into())
    );
}

// ── Step 2: XMLFOREST ─────────────────────────────────────────────────────────

#[test]
fn xmlforest_basic() {
    assert_eq!(
        run("SELECT XMLFOREST('bob' AS name, 42 AS age)"),
        Value::Xml("<name>bob</name><age>42</age>".into())
    );
}

#[test]
fn xmlforest_null_omitted() {
    assert_eq!(
        run("SELECT XMLFOREST(NULL AS x, 'y' AS y)"),
        Value::Xml("<y>y</y>".into())
    );
}

#[test]
fn xmlforest_all_null_empty() {
    assert_eq!(
        run("SELECT XMLFOREST(NULL AS x)"),
        Value::Xml("".into())
    );
}

// ── Step 2: XMLROOT ───────────────────────────────────────────────────────────

#[test]
fn xmlroot_basic() {
    assert_eq!(
        run("SELECT XMLROOT('<a/>'::XML, VERSION '1.0')"),
        Value::Xml("<?xml version=\"1.0\"?><a/>".into())
    );
}

#[test]
fn xmlroot_standalone_yes() {
    assert_eq!(
        run("SELECT XMLROOT('<a/>'::XML, VERSION '1.0', STANDALONE YES)"),
        Value::Xml("<?xml version=\"1.0\" standalone=\"yes\"?><a/>".into())
    );
}

#[test]
fn xmlroot_strips_existing_decl() {
    assert_eq!(
        run("SELECT XMLROOT('<?xml version=\"1.0\"?><a/>'::XML, VERSION '1.1')"),
        Value::Xml("<?xml version=\"1.1\"?><a/>".into())
    );
}

#[test]
fn xmlroot_null_doc() {
    assert_eq!(run("SELECT XMLROOT(NULL, VERSION '1.0')"), Value::Null);
}

// ── Step 2: XMLCONCAT ─────────────────────────────────────────────────────────

#[test]
fn xmlconcat_basic() {
    assert_eq!(
        run("SELECT XMLCONCAT('<a/>'::XML, '<b/>'::XML)"),
        Value::Xml("<a/><b/>".into())
    );
}

#[test]
fn xmlconcat_null_skipped() {
    assert_eq!(
        run("SELECT XMLCONCAT('<a/>'::XML, NULL, '<b/>'::XML)"),
        Value::Xml("<a/><b/>".into())
    );
}

#[test]
fn xmlconcat_all_null() {
    assert_eq!(run("SELECT XMLCONCAT(NULL, NULL)"), Value::Null);
}

// ── Step 2: XMLQUERY ──────────────────────────────────────────────────────────

#[test]
fn xmlquery_basic() {
    assert_eq!(
        run("SELECT XMLQUERY('/root/a/text()' PASSING '<root><a>hi</a></root>'::XML)"),
        Value::Text("hi".into())
    );
}

#[test]
fn xmlquery_no_match() {
    assert_eq!(
        run("SELECT XMLQUERY('/root/missing/text()' PASSING '<root><a>hi</a></root>'::XML)"),
        Value::Null
    );
}

#[test]
fn xmlquery_null_doc() {
    assert_eq!(
        run("SELECT XMLQUERY('/root/text()' PASSING NULL)"),
        Value::Null
    );
}

#[test]
fn xmlquery_attr() {
    assert_eq!(
        run("SELECT XMLQUERY('/root/a/@id' PASSING '<root><a id=\"42\">x</a></root>'::XML)"),
        Value::Text("42".into())
    );
}

#[test]
fn xmlquery_invalid_xml_returns_null() {
    assert_eq!(
        run("SELECT XMLQUERY('/root' PASSING '<broken'::TEXT)"),
        Value::Null
    );
}
