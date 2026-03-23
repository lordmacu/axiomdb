//! B+ Tree benchmarks with Criterion.
//!
//! Target comparisons (see CLAUDE.md performance budget):
//! - Point lookup: 800k ops/s target
//! - Range scan 10K rows: 45ms target
//! - INSERT with splits: 180k ops/s target

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use nexusdb_core::RecordId;
use nexusdb_index::BTree;
use nexusdb_storage::MemoryStorage;

fn rid(n: u64) -> RecordId {
    RecordId {
        page_id: n,
        slot_id: 0,
    }
}

fn build_tree(count: usize) -> BTree {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    for i in 0..count {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
    }
    tree
}

fn build_std_btreemap(count: usize) -> BTreeMap<Vec<u8>, u64> {
    let mut map = BTreeMap::new();
    for i in 0..count {
        let key = format!("{:08}", i).into_bytes();
        map.insert(key, i as u64);
    }
    map
}

// ── Point lookup ─────────────────────────────────────────────────────────────

fn bench_point_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("point_lookup");

    for size in [10_000usize, 100_000, 1_000_000] {
        group.throughput(Throughput::Elements(1));

        // Our BTree
        let tree = build_tree(size);
        group.bench_with_input(BenchmarkId::new("nexusdb_btree", size), &size, |b, &n| {
            let mut i = 0usize;
            b.iter(|| {
                let key = format!("{:08}", i % n);
                black_box(tree.lookup(key.as_bytes()).unwrap());
                i += 1;
            });
        });

        // std::collections::BTreeMap as baseline reference
        let std_map = build_std_btreemap(size);
        group.bench_with_input(BenchmarkId::new("std_btreemap", size), &size, |b, &n| {
            let mut i = 0usize;
            b.iter(|| {
                let key = format!("{:08}", i % n).into_bytes();
                black_box(std_map.get(&key));
                i += 1;
            });
        });
    }

    group.finish();
}

// ── Range scan ───────────────────────────────────────────────────────────────

fn bench_range_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("range_scan");

    let total = 100_000;
    let tree = build_tree(total);
    let std_map = build_std_btreemap(total);

    for range_size in [100usize, 1_000, 10_000] {
        group.throughput(Throughput::Elements(range_size as u64));

        let from_i = total / 2;
        let to_i = from_i + range_size - 1;
        let from = format!("{:08}", from_i);
        let to = format!("{:08}", to_i);

        group.bench_with_input(
            BenchmarkId::new("nexusdb_btree", range_size),
            &range_size,
            |b, _| {
                b.iter(|| {
                    let count = tree
                        .range(
                            std::ops::Bound::Included(from.as_bytes()),
                            std::ops::Bound::Included(to.as_bytes()),
                        )
                        .unwrap()
                        .count();
                    black_box(count)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("std_btreemap", range_size),
            &range_size,
            |b, _| {
                b.iter(|| {
                    let count = std_map
                        .range(from.as_bytes().to_vec()..=to.as_bytes().to_vec())
                        .count();
                    black_box(count)
                });
            },
        );
    }

    group.finish();
}

// ── Sequential insert ────────────────────────────────────────────────────────

fn bench_insert_sequential(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_sequential");
    group.throughput(Throughput::Elements(1));

    group.bench_function("nexusdb_btree_1k", |b| {
        b.iter(|| {
            let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
            for i in 0..1_000u64 {
                let key = format!("{:08}", i);
                tree.insert(key.as_bytes(), rid(i)).unwrap();
            }
            black_box(tree.root_page_id())
        });
    });

    group.bench_function("std_btreemap_1k", |b| {
        b.iter(|| {
            let mut map = BTreeMap::new();
            for i in 0..1_000u64 {
                let key = format!("{:08}", i).into_bytes();
                map.insert(key, i);
            }
            black_box(map.len())
        });
    });

    group.finish();
}

// ── Random insert ─────────────────────────────────────────────────────────────

fn bench_insert_random(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_random");
    group.throughput(Throughput::Elements(1));

    // Generate random keys (deterministic pseudorandom)
    let keys: Vec<Vec<u8>> = (0..100_000u64)
        .map(|i| {
            // Scramble with XOR shift to break sequential ordering
            let scrambled = i
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            format!("{:016x}", scrambled).into_bytes()
        })
        .collect();

    group.bench_function("nexusdb_btree_100k_random", |b| {
        b.iter(|| {
            let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
            for (i, key) in keys.iter().enumerate() {
                tree.insert(key, rid(i as u64)).unwrap();
            }
            black_box(tree.root_page_id())
        });
    });

    group.finish();
}

// ── 1M sequential insert — throughput and splits at scale ────────────────────

fn bench_insert_1m_sequential(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_1m_sequential");
    // Extended measurement time for 1M inserts
    group.measurement_time(std::time::Duration::from_secs(60));
    group.sample_size(10);
    group.throughput(Throughput::Elements(1_000_000));

    group.bench_function("nexusdb_btree_1m", |b| {
        b.iter(|| {
            let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
            for i in 0..1_000_000u64 {
                let key = i.to_be_bytes(); // 8 bytes, sequentially ordered
                tree.insert(&key, rid(i)).unwrap();
            }
            black_box(tree.root_page_id())
        });
    });

    group.bench_function("std_btreemap_1m", |b| {
        b.iter(|| {
            let mut map = BTreeMap::new();
            for i in 0..1_000_000u64 {
                map.insert(i.to_be_bytes(), i);
            }
            black_box(map.len())
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_point_lookup,
    bench_range_scan,
    bench_insert_sequential,
    bench_insert_random,
    bench_insert_1m_sequential,
);
criterion_main!(benches);
