# Plan: SERIAL / SMALLSERIAL type shorthands

Phase: 24 — Complete type system
Task: 24.1b — SERIAL and SMALLSERIAL column type sugar
Spec: specs/fase-24/spec-24.1b-serial-smallserial.md
Status: in-progress

## Summary

Single-step plan. Extend the BIGSERIAL pre-type block in `parse_column_def`
to also detect `Token::Serial` (→ INT AUTO_INCREMENT) and `Ident("SMALLSERIAL")`
(→ SMALLINT AUTO_INCREMENT). Then add integration tests and wire smoke assertions,
run the closing protocol.

## Dependencies

Must be done first:
- [x] spec-24.1b approved
- [x] plan-24.1 completed (SmallInt type, BIGSERIAL pattern exist)

Blocks (until this plan is done):
- [ ] nothing

## Affected files

Modified files:
- `crates/axiomdb-sql/src/parser/ddl.rs` — extend serial detection block
- `crates/axiomdb-sql/tests/integration_integer_types.rs` — 4 new tests
- `tools/wire-test.py` — 4 new assertions

---

## Step 1 — Parser + tests + wire smoke + close

**Goal:** SERIAL and SMALLSERIAL parse correctly; all tests and wire assertions pass.
**Files:** `ddl.rs`, `integration_integer_types.rs`, `tools/wire-test.py`
**Approach:** Extend existing BIGSERIAL block; add tests; wire smoke; close.

### Parser change

Replace the BIGSERIAL-only block (lines ~562–584 in `ddl.rs`) with a unified
serial-kind detector:

```rust
// Serial shorthands: BIGSERIAL → BigInt, SERIAL → Int, SMALLSERIAL → SmallInt.
// Each synthesizes DataType + ColumnConstraint::AutoIncrement at parse time.
let serial_type: Option<DataType> =
    if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("BIGSERIAL")) {
        p.advance();
        Some(DataType::BigInt)
    } else if matches!(p.peek(), Token::Serial) {
        p.advance();
        Some(DataType::Int)
    } else if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("SMALLSERIAL")) {
        p.advance();
        Some(DataType::SmallInt)
    } else {
        None
    };

let (data_type, type_len, is_char, declared_type_name, array_ndims, array_size_hints) =
    if let Some(dt) = serial_type.clone() {
        (dt, 0u16, false, None::<crate::ast::TableRef>, 0u8, vec![])
    } else {
        parse_column_data_type(p)?
    };
let mut constraints = Vec::new();
if serial_type.is_some() {
    constraints.push(ColumnConstraint::AutoIncrement);
}
```

### Tests to add (integration_integer_types.rs)

```rust
#[test]
fn serial_auto_increments() {
    let (mut s, mut txn) = common::setup();
    common::run("CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT)", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('a')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('b')", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT id FROM t ORDER BY id", &mut s, &mut txn));
    assert_eq!(out, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn smallserial_auto_increments() {
    let (mut s, mut txn) = common::setup();
    common::run("CREATE TABLE t (id SMALLSERIAL PRIMARY KEY, name TEXT)", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('x')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('y')", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT id FROM t ORDER BY id", &mut s, &mut txn));
    assert_eq!(out, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn show_columns_reports_serial_types() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a SERIAL PRIMARY KEY, b SMALLSERIAL, c BIGSERIAL)",
        &mut s,
        &mut txn,
    );
    let rows = common::rows(common::run("SHOW COLUMNS FROM t", &mut s, &mut txn));
    let types: Vec<String> = rows
        .iter()
        .map(|row| match &row[1] {
            Value::Text(s) => s.to_lowercase(),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(types.iter().any(|t| t == "int"), "expected int, got {types:?}");
    assert!(
        types.iter().any(|t| t == "smallint"),
        "expected smallint, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "bigint"),
        "expected bigint, got {types:?}"
    );
}

#[test]
fn serial_trailing_form_regression() {
    // col INT SERIAL (trailing form) must still work
    let (mut s, mut txn) = common::setup();
    common::run("CREATE TABLE t (id INT SERIAL PRIMARY KEY, val TEXT)", &mut s, &mut txn);
    common::run("INSERT INTO t (val) VALUES ('r')", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT id FROM t", &mut s, &mut txn));
    assert_eq!(out, vec![vec![Value::Int(1)]]);
}
```

### Wire smoke assertions (tools/wire-test.py)

Add before the Result block:

```python
# ── 24.1b SERIAL / SMALLSERIAL ────────────────────────────────────────────────
cur.execute("DROP TABLE IF EXISTS _wire_serial")
cur.execute("CREATE TABLE _wire_serial (id SERIAL PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO _wire_serial (name) VALUES ('a')")
cur.execute("INSERT INTO _wire_serial (name) VALUES ('b')")
cur.execute("SELECT id FROM _wire_serial ORDER BY id")
sr = cur.fetchall()
ok("[24.1b serial] SERIAL auto-increments from 1", sr[0][0] == 1, sr[0][0])
ok("[24.1b serial] SERIAL second row is 2", sr[1][0] == 2, sr[1][0])

cur.execute("DROP TABLE IF EXISTS _wire_smallserial")
cur.execute("CREATE TABLE _wire_smallserial (id SMALLSERIAL PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO _wire_smallserial (name) VALUES ('x')")
cur.execute("INSERT INTO _wire_smallserial (name) VALUES ('y')")
cur.execute("SELECT id FROM _wire_smallserial ORDER BY id")
ssr = cur.fetchall()
ok("[24.1b serial] SMALLSERIAL auto-increments from 1", ssr[0][0] == 1, ssr[0][0])
ok("[24.1b serial] SMALLSERIAL second row is 2", ssr[1][0] == 2, ssr[1][0])

cur.execute("DROP TABLE IF EXISTS _wire_serial")
cur.execute("DROP TABLE IF EXISTS _wire_smallserial")
```

### Verification

```bash
limactl shell axiomdb -- cargo nextest run -p axiomdb-sql --test integration_integer_types
limactl shell axiomdb -- cargo nextest run --workspace
limactl shell axiomdb -- cargo clippy --workspace -- -D warnings
limactl shell axiomdb -- cargo fmt --check
# build macOS binary then:
pkill axiomdb-server; python3 tools/wire-test.py
```

### Commit

```
feat(fase-24): complete subphase 24.1b — SERIAL / SMALLSERIAL

Implements specs/fase-24/spec-24.1b-serial-smallserial.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Token::Serial conflict (trailing vs type position) | low | pre-type block runs before constraint loop; order is safe |
| SMALLSERIAL auto-increment exceeds SMALLINT range in tests | very low | tests only insert 2 rows |

## Rollback plan

`git reset --hard` to spec commit if needed. No catalog changes, fully reversible.

## Estimated effort

Total: 20 min
Step 1: 20 min (parser 5min, tests 10min, wire 5min)
