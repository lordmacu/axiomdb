//! Phase 24.5 — BYTEA/BLOB completeness.
//!
//! Covers the new surface area added on top of the pre-existing BYTEA
//! infrastructure (DDL, codec, TOAST, encode/decode, base64):
//! - `||` concat on bytea
//! - `substring(bytea, ...)` byte-position slicing
//! - `position/instr/locate(needle bytea, haystack bytea)`
//! - `get_byte/set_byte/get_bit/set_bit/bit_count`
//! - `md5/sha1/sha224/sha256/sha384/sha512`
//! - MySQL `BINARY(n)` / `VARBINARY(n)` DDL aliases
//! - `CAST('\xDEADBEEF' AS BYTEA)` and `'\x...'::bytea` hex literal
//! - `CAST(bytea AS TEXT)` lossy UTF-8

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run_ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    match run_ok(sql, storage, txn, bloom, ctx) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn first_cell(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Value {
    let r = rows(sql, storage, txn, bloom, ctx);
    assert!(!r.is_empty(), "expected at least one row for: {sql}");
    r[0][0].clone()
}

// ── DDL: VARBINARY / BINARY aliases ──────────────────────────────────────────

#[test]
fn varbinary_alias_creates_bytea_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ok(
        "CREATE TABLE t (id INT, payload VARBINARY(255))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ok(
        "INSERT INTO t VALUES (1, CAST('\\\\xDEADBEEF' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let v = first_cell(
        "SELECT payload FROM t WHERE id = 1",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
}

#[test]
fn binary_alias_creates_bytea_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ok(
        "CREATE TABLE t (id INT, payload BINARY(16))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ok(
        "INSERT INTO t VALUES (1, CAST('hello' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let v = first_cell(
        "SELECT payload FROM t WHERE id = 1",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(b"hello".to_vec()));
}

// ── Coercion: Text → Bytes with hex form ─────────────────────────────────────

#[test]
fn cast_hex_literal_to_bytea() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT CAST('\\\\x68656c6c6f' AS BYTEA)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(b"hello".to_vec()));
}

#[test]
fn cast_plain_text_to_bytea_uses_utf8_bytes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT CAST('hi' AS BYTEA)", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(v, Value::Bytes(b"hi".to_vec()));
}

#[test]
fn cast_invalid_hex_in_backslash_x_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Odd number of hex digits → InvalidCoercion (would otherwise silently
    // succeed and produce surprising bytes).
    let result = run_ctx(
        "SELECT CAST('\\\\xABC' AS BYTEA)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(
        result.is_err(),
        "expected InvalidCoercion for odd-length '\\\\x...' literal, got {result:?}"
    );
}

#[test]
fn cast_bytea_to_text_lossy_utf8() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT CAST(CAST('hello' AS BYTEA) AS TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("hello".to_string()));
}

// ── Operator: || concat ──────────────────────────────────────────────────────

#[test]
fn bytea_concat_operator() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT CAST('\\\\xDEAD' AS BYTEA) || CAST('\\\\xBEEF' AS BYTEA)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
}

// ── substring / position / instr on Bytes ────────────────────────────────────

#[test]
fn substring_on_bytea() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT SUBSTRING(CAST('\\\\x01020304' AS BYTEA), 2, 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0x02, 0x03]));
}

#[test]
fn substring_on_bytea_without_length_takes_to_end() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT SUBSTRING(CAST('\\\\xAABBCCDD' AS BYTEA), 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0xCC, 0xDD]));
}

#[test]
fn position_on_bytea_returns_one_based_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT POSITION(CAST('\\\\xBEEF' AS BYTEA), CAST('\\\\xDEADBEEF' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(3));
}

#[test]
fn position_on_bytea_returns_zero_when_absent() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT POSITION(CAST('\\\\xFFFF' AS BYTEA), CAST('\\\\xDEADBEEF' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(0));
}

#[test]
fn instr_on_bytea() {
    let (mut s, mut t, mut b, mut c) = setup();
    // INSTR(haystack, needle) — reversed argument order vs POSITION
    let v = first_cell(
        "SELECT INSTR(CAST('\\\\xDEADBEEF' AS BYTEA), CAST('\\\\xADBE' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(2));
}

// ── get_byte / set_byte / get_bit / set_bit / bit_count ──────────────────────

#[test]
fn get_byte_returns_byte_at_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT GET_BYTE(CAST('\\\\x10203040' AS BYTEA), 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(0x30));
}

#[test]
fn get_byte_out_of_range_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = run_ctx(
        "SELECT GET_BYTE(CAST('\\\\x1020' AS BYTEA), 5)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(r.is_err(), "expected error on out-of-range GET_BYTE");
}

#[test]
fn set_byte_replaces_byte_at_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        "SELECT SET_BYTE(CAST('\\\\x10203040' AS BYTEA), 2, 255)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0x10, 0x20, 0xFF, 0x40]));
}

#[test]
fn get_bit_msb_first() {
    let (mut s, mut t, mut b, mut c) = setup();
    // 0xA0 = 1010 0000 — bit 0 (MSB) = 1, bit 1 = 0, bit 2 = 1, bit 3 = 0
    let r = rows(
        "SELECT GET_BIT(CAST('\\\\xA0' AS BYTEA), 0), \
                GET_BIT(CAST('\\\\xA0' AS BYTEA), 1), \
                GET_BIT(CAST('\\\\xA0' AS BYTEA), 2), \
                GET_BIT(CAST('\\\\xA0' AS BYTEA), 7)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Int(0));
    assert_eq!(r[0][2], Value::Int(1));
    assert_eq!(r[0][3], Value::Int(0));
}

#[test]
fn set_bit_flips_specified_bit() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Start 0x00, set bit 0 (MSB of first byte) → 0x80.
    let v = first_cell(
        "SELECT SET_BIT(CAST('\\\\x00' AS BYTEA), 0, 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0x80]));
    // Now clear bit 7 (LSB) of 0xFF → 0xFE.
    let v = first_cell(
        "SELECT SET_BIT(CAST('\\\\xFF' AS BYTEA), 7, 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Bytes(vec![0xFE]));
}

#[test]
fn bit_count_bytea_counts_set_bits() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell(
        // 0xFF + 0x00 + 0x0F = 8 + 0 + 4 = 12 set bits
        "SELECT BIT_COUNT(CAST('\\\\xFF000F' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::BigInt(12));
}

#[test]
fn bit_count_integer_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT BIT_COUNT(7)", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(v, Value::BigInt(3));
}

// ── Hashing: md5 / sha1 / sha256 / sha512 ────────────────────────────────────

#[test]
fn md5_of_empty_string_is_d41d8cd98f00b204e9800998ecf8427e() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT MD5('')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(
        v,
        Value::Text("d41d8cd98f00b204e9800998ecf8427e".to_string())
    );
}

#[test]
fn md5_of_hello_world() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT MD5('hello')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(
        v,
        Value::Text("5d41402abc4b2a76b9719d911017c592".to_string())
    );
}

#[test]
fn md5_of_bytea_input() {
    let (mut s, mut t, mut b, mut c) = setup();
    // CAST('hello' AS BYTEA) hashes to the same MD5 as 'hello' (same bytes).
    let v = first_cell(
        "SELECT MD5(CAST('hello' AS BYTEA))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(
        v,
        Value::Text("5d41402abc4b2a76b9719d911017c592".to_string())
    );
}

#[test]
fn sha1_of_empty_string() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT SHA1('')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(
        v,
        Value::Text("da39a3ee5e6b4b0d3255bfef95601890afd80709".to_string())
    );
}

#[test]
fn sha256_of_empty_string() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT SHA256('')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(
        v,
        Value::Text("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string())
    );
}

#[test]
fn sha512_of_empty_string() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = first_cell("SELECT SHA512('')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(
        v,
        Value::Text(
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
                .to_string()
        )
    );
}

#[test]
fn null_input_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    for func in &["MD5", "SHA1", "SHA256", "SHA512"] {
        let sql = format!("SELECT {func}(NULL)");
        let v = first_cell(&sql, &mut s, &mut t, &mut b, &mut c);
        assert_eq!(v, Value::Null, "{func}(NULL) should be NULL");
    }
}
