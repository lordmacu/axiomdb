//! Benchmarks for the binary JSONB format — encode, decode, key lookup, containment.

use axiomdb_types::{jsonb_contains, jsonb_merge_patch, JsonbDecoder, JsonbEncoder, JsonbRef};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn small_object() -> serde_json::Value {
    serde_json::json!({"name": "Alice", "age": 30, "active": true})
}

fn medium_object() -> serde_json::Value {
    serde_json::json!({
        "id": 42,
        "name": "Alice Wonderland",
        "email": "alice@example.com",
        "age": 30,
        "balance": 1234.56,
        "active": true,
        "tags": ["admin", "user", "premium"],
        "address": {
            "street": "123 Main St",
            "city": "Metropolis",
            "zip": "12345"
        }
    })
}

fn large_array() -> serde_json::Value {
    let items: Vec<serde_json::Value> = (0..100)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "name": format!("item_{i:04}"),
                "value": i as f64 * 1.5
            })
        })
        .collect();
    serde_json::Value::Array(items)
}

fn object_1000_keys() -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for i in 0..1000 {
        map.insert(format!("key{i:04}"), serde_json::json!(i));
    }
    serde_json::Value::Object(map)
}

// ── Encode benchmarks ─────────────────────────────────────────────────────────

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/encode");

    let small = small_object();
    group.throughput(Throughput::Elements(1));
    group.bench_function("small_object_3keys", |b| {
        b.iter(|| JsonbEncoder::encode(black_box(&small)).unwrap())
    });

    let medium = medium_object();
    group.bench_function("medium_object_8keys", |b| {
        b.iter(|| JsonbEncoder::encode(black_box(&medium)).unwrap())
    });

    let large = large_array();
    group.bench_function("array_100_objects", |b| {
        b.iter(|| JsonbEncoder::encode(black_box(&large)).unwrap())
    });

    let big = object_1000_keys();
    group.bench_function("object_1000_keys", |b| {
        b.iter(|| JsonbEncoder::encode(black_box(&big)).unwrap())
    });

    group.finish();
}

// ── Decode benchmarks ─────────────────────────────────────────────────────────

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/decode");

    let small_blob = JsonbEncoder::encode(&small_object()).unwrap();
    group.throughput(Throughput::Bytes(small_blob.len() as u64));
    group.bench_function("small_object_3keys", |b| {
        b.iter(|| JsonbDecoder::decode(black_box(&small_blob)).unwrap())
    });

    let medium_blob = JsonbEncoder::encode(&medium_object()).unwrap();
    group.throughput(Throughput::Bytes(medium_blob.len() as u64));
    group.bench_function("medium_object_8keys", |b| {
        b.iter(|| JsonbDecoder::decode(black_box(&medium_blob)).unwrap())
    });

    let large_blob = JsonbEncoder::encode(&large_array()).unwrap();
    group.throughput(Throughput::Bytes(large_blob.len() as u64));
    group.bench_function("array_100_objects", |b| {
        b.iter(|| JsonbDecoder::decode(black_box(&large_blob)).unwrap())
    });

    group.finish();
}

// ── Key lookup benchmarks (binary search) ─────────────────────────────────────

fn bench_key_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/key_lookup");

    for n_keys in [10u64, 100, 1000] {
        let mut map = serde_json::Map::new();
        for i in 0..n_keys {
            map.insert(format!("key{i:04}"), serde_json::json!(i));
        }
        let blob = JsonbEncoder::encode(&serde_json::Value::Object(map)).unwrap();

        // Lookup a key that exists (in the middle)
        let target = format!("key{:04}", n_keys / 2);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("hit", n_keys),
            &(blob.clone(), target),
            |b, (blob, key)| {
                let r = JsonbRef::new(blob);
                b.iter(|| r.get_key(black_box(key)).unwrap())
            },
        );

        // Lookup a key that does not exist
        group.bench_with_input(BenchmarkId::new("miss", n_keys), &blob, |b, blob| {
            let r = JsonbRef::new(blob);
            b.iter(|| r.get_key(black_box("zzz_not_found")).unwrap())
        });
    }

    group.finish();
}

// ── Array index benchmarks ────────────────────────────────────────────────────

fn bench_array_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/array_index");

    for n_elems in [10u64, 100, 1000] {
        let arr: Vec<serde_json::Value> = (0..n_elems).map(|i| serde_json::json!(i)).collect();
        let blob = JsonbEncoder::encode(&serde_json::Value::Array(arr)).unwrap();

        let mid = (n_elems / 2) as i64;
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("get_index", n_elems),
            &(blob, mid),
            |b, (blob, idx)| {
                let r = JsonbRef::new(blob);
                b.iter(|| r.get_index(black_box(*idx)).unwrap())
            },
        );
    }

    group.finish();
}

// ── Containment benchmarks ────────────────────────────────────────────────────

fn bench_containment(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/contains");

    let doc = JsonbEncoder::encode(&serde_json::json!({
        "a": 1, "b": 2, "c": 3, "d": 4, "e": 5,
        "nested": {"x": 10, "y": 20}
    }))
    .unwrap();

    let candidate_hit = JsonbEncoder::encode(&serde_json::json!({"a": 1, "c": 3})).unwrap();
    let candidate_miss = JsonbEncoder::encode(&serde_json::json!({"a": 99})).unwrap();

    group.bench_function("hit", |b| {
        b.iter(|| jsonb_contains(black_box(&doc), black_box(&candidate_hit)).unwrap())
    });

    group.bench_function("miss", |b| {
        b.iter(|| jsonb_contains(black_box(&doc), black_box(&candidate_miss)).unwrap())
    });

    group.finish();
}

// ── Merge patch benchmarks ────────────────────────────────────────────────────

fn bench_merge_patch(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/merge_patch");

    let target = serde_json::json!({"a": 1, "b": 2, "c": 3, "d": 4});
    let patch = serde_json::json!({"b": 99, "e": 5, "c": null});

    group.bench_function("small_object", |b| {
        b.iter(|| jsonb_merge_patch(black_box(&target), black_box(&patch)))
    });

    group.finish();
}

// ── Roundtrip (encode + decode) ───────────────────────────────────────────────

fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonb/roundtrip");

    let medium = medium_object();
    group.throughput(Throughput::Elements(1));
    group.bench_function("medium_object", |b| {
        b.iter(|| {
            let blob = JsonbEncoder::encode(black_box(&medium)).unwrap();
            JsonbDecoder::decode(black_box(&blob)).unwrap()
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_encode,
    bench_decode,
    bench_key_lookup,
    bench_array_index,
    bench_containment,
    bench_merge_patch,
    bench_roundtrip,
);
criterion_main!(benches);
