//! Benchmarks for the row codec — encode_row / decode_row.
//!
//! Measures: rows/second and MB/second for typical row shapes.

use axiomdb_types::{decode_row, encode_row, encoded_len, DataType, Value};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// ── Shared row fixtures ────────────────────────────────────────────────────────

/// A realistic user row: id, name, email, age, balance, active, created_at.
fn user_row() -> (Vec<Value>, Vec<DataType>) {
    let values = vec![
        Value::BigInt(42),
        Value::Text("Alice Wonderland".into()),
        Value::Text("alice@example.com".into()),
        Value::Int(30),
        Value::Real(1234.56),
        Value::Bool(true),
        Value::Timestamp(1_704_067_200_000_000),
    ];
    let schema = vec![
        DataType::BigInt,
        DataType::Text,
        DataType::Text,
        DataType::Int,
        DataType::Real,
        DataType::Bool,
        DataType::Timestamp,
    ];
    (values, schema)
}

/// A row with every type (one of each).
fn all_types_row() -> (Vec<Value>, Vec<DataType>) {
    let values = vec![
        Value::Bool(true),
        Value::Int(42),
        Value::BigInt(i64::MAX),
        Value::Real(3.14),
        Value::Decimal(123456, 2),
        Value::Text("hello world".into()),
        Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        Value::Date(19722),
        Value::Timestamp(1_704_067_200_000_000),
        Value::Uuid([0u8; 16]),
    ];
    let schema = vec![
        DataType::Bool,
        DataType::Int,
        DataType::BigInt,
        DataType::Real,
        DataType::Decimal,
        DataType::Text,
        DataType::Bytes,
        DataType::Date,
        DataType::Timestamp,
        DataType::Uuid,
    ];
    (values, schema)
}

/// A row with NULLs in half the columns.
fn nullable_row() -> (Vec<Value>, Vec<DataType>) {
    let values = vec![
        Value::BigInt(1),
        Value::Null,
        Value::Text("present".into()),
        Value::Null,
        Value::Int(99),
        Value::Null,
    ];
    let schema = vec![
        DataType::BigInt,
        DataType::Text,
        DataType::Text,
        DataType::Int,
        DataType::Int,
        DataType::Bool,
    ];
    (values, schema)
}

// ── encode_row benchmarks ─────────────────────────────────────────────────────

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/encode");

    let (user_v, user_s) = user_row();
    let encoded_size = encode_row(&user_v, &user_s).unwrap().len();
    group.throughput(Throughput::Bytes(encoded_size as u64));
    group.bench_function("user_row_7cols", |b| {
        b.iter(|| encode_row(black_box(&user_v), black_box(&user_s)).unwrap())
    });

    let (all_v, all_s) = all_types_row();
    let encoded_size = encode_row(&all_v, &all_s).unwrap().len();
    group.throughput(Throughput::Bytes(encoded_size as u64));
    group.bench_function("all_types_10cols", |b| {
        b.iter(|| encode_row(black_box(&all_v), black_box(&all_s)).unwrap())
    });

    let (null_v, null_s) = nullable_row();
    let encoded_size = encode_row(&null_v, &null_s).unwrap().len();
    group.throughput(Throughput::Bytes(encoded_size as u64));
    group.bench_function("nullable_half_nulls", |b| {
        b.iter(|| encode_row(black_box(&null_v), black_box(&null_s)).unwrap())
    });

    group.finish();
}

// ── decode_row benchmarks ─────────────────────────────────────────────────────

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/decode");

    let (user_v, user_s) = user_row();
    let encoded = encode_row(&user_v, &user_s).unwrap();
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("user_row_7cols", |b| {
        b.iter(|| decode_row(black_box(&encoded), black_box(&user_s)).unwrap())
    });

    let (all_v, all_s) = all_types_row();
    let encoded = encode_row(&all_v, &all_s).unwrap();
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("all_types_10cols", |b| {
        b.iter(|| decode_row(black_box(&encoded), black_box(&all_s)).unwrap())
    });

    group.finish();
}

// ── encoded_len benchmark ─────────────────────────────────────────────────────

fn bench_encoded_len(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/encoded_len");

    let (user_v, _) = user_row();
    group.bench_function("user_row_7cols", |b| {
        b.iter(|| encoded_len(black_box(&user_v)))
    });

    group.finish();
}

// ── Bulk throughput: encode N rows ────────────────────────────────────────────

fn bench_bulk_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/bulk");

    for n_rows in [100u64, 1_000, 10_000] {
        let (v, s) = user_row();
        group.throughput(Throughput::Elements(n_rows));
        group.bench_with_input(
            BenchmarkId::new("encode_user_rows", n_rows),
            &n_rows,
            |b, &n| {
                b.iter(|| {
                    for _ in 0..n {
                        black_box(encode_row(&v, &s).unwrap());
                    }
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_encode,
    bench_decode,
    bench_encoded_len,
    bench_bulk_encode,
);
criterion_main!(benches);
