//! WAL writer benchmarks — single-writer vs concurrent group commit.
//!
//! Measures throughput for sequential WAL appends under the `ConcurrentWalWriter`.
//! The concurrent scenario (multiple threads submitting in parallel) demonstrates
//! the group-commit advantage: one fsync covers N transactions.

use std::sync::Arc;

use axiomdb_wal::{ConcurrentWalWriter, EntryType, WalEntry};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_entry(txn_id: u64) -> WalEntry {
    WalEntry::new(
        0,
        txn_id,
        EntryType::Insert,
        1,
        b"benchmark_key".to_vec(),
        vec![],
        vec![0u8; 64], // 64-byte payload
    )
}

// ── Sequential throughput (single thread, no contention) ─────────────────────

fn bench_sequential_append(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench.wal");
    let w = ConcurrentWalWriter::create(&path).unwrap();

    let mut group = c.benchmark_group("wal/sequential");

    for n in [1usize, 10, 100, 1000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                for i in 0..n {
                    let mut e = make_entry(i as u64 + 1);
                    w.append(&mut e).unwrap();
                }
                w.flush_no_sync().unwrap();
            });
        });
    }

    group.finish();
}

// ── Concurrent throughput (N threads, group commit) ───────────────────────────

fn bench_concurrent_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal/concurrent");

    for threads in [1usize, 2, 4, 8] {
        const APPENDS_PER_THREAD: usize = 100;
        let total = threads * APPENDS_PER_THREAD;

        group.throughput(Throughput::Elements(total as u64));
        group.bench_with_input(
            BenchmarkId::new("threads", threads),
            &threads,
            |b, &threads| {
                let dir = tempdir().unwrap();
                let path = dir.path().join("bench.wal");
                let w = Arc::new(ConcurrentWalWriter::create(&path).unwrap());

                b.iter(|| {
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let w2 = Arc::clone(&w);
                            std::thread::spawn(move || {
                                for i in 0..APPENDS_PER_THREAD {
                                    let mut e = make_entry((t * APPENDS_PER_THREAD + i) as u64 + 1);
                                    w2.append(&mut e).unwrap();
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    // One fsync covers all threads' entries (group commit).
                    w.flush_no_sync().unwrap();
                });
            },
        );
    }

    group.finish();
}

// ── LSN reservation cost (lock-free atomic op) ────────────────────────────────

fn bench_lsn_reservation(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench.wal");
    let w = ConcurrentWalWriter::create(&path).unwrap();

    c.bench_function("wal/lsn_reserve_single", |b| {
        b.iter(|| {
            w.reserve_lsn();
        });
    });

    c.bench_function("wal/lsn_reserve_batch_100", |b| {
        b.iter(|| {
            w.reserve_lsns(100);
        });
    });
}

criterion_group!(
    benches,
    bench_sequential_append,
    bench_concurrent_append,
    bench_lsn_reservation
);
criterion_main!(benches);
