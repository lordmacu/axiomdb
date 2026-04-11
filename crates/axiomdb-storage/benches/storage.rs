use axiomdb_storage::{
    clustered_overflow, MemoryStorage, MmapStorage, Page, PageType, StorageEngine,
};
use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use tempfile::tempdir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_data_page(page_id: u64) -> Page {
    let mut page = Page::new(PageType::Data, page_id);
    // Realistic data — avoids OS/compiler optimizations for zeros.
    page.body_mut()
        .iter_mut()
        .enumerate()
        .for_each(|(i, b)| *b = (i % 251) as u8);
    page.update_checksum();
    page
}

// ── MemoryStorage benchmarks ──────────────────────────────────────────────────

fn bench_memory_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/alloc");
    group.throughput(Throughput::Elements(1));

    group.bench_function("alloc_page", |b| {
        b.iter_batched(
            MemoryStorage::new,
            |s| s.alloc_page(PageType::Data).unwrap(),
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_memory_write_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/write_read");
    group.throughput(Throughput::Bytes(axiomdb_storage::PAGE_SIZE as u64));

    group.bench_function("write_page", |b| {
        b.iter_batched(
            || {
                let s = MemoryStorage::new();
                let id = s.alloc_page(PageType::Data).unwrap();
                (s, id, make_data_page(id))
            },
            |(s, id, page)| s.write_page(id, &page).unwrap(),
            BatchSize::SmallInput,
        )
    });

    group.bench_function("read_page", |b| {
        b.iter_batched(
            || {
                let s = MemoryStorage::new();
                let id = s.alloc_page(PageType::Data).unwrap();
                let page = make_data_page(id);
                s.write_page(id, &page).unwrap();
                (s, id)
            },
            |(s, id)| {
                let page = s.read_page(id).unwrap();
                black_box(page.body()[0])
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_memory_sequential_reads(c: &mut Criterion) {
    const N_PAGES: u64 = 1000;
    let mut group = c.benchmark_group("memory/sequential");
    group.throughput(Throughput::Elements(N_PAGES));

    group.bench_function(BenchmarkId::new("read_sequential", N_PAGES), |b| {
        b.iter_batched(
            || {
                let s = MemoryStorage::new();
                let ids: Vec<u64> = (0..N_PAGES)
                    .map(|_| {
                        let id = s.alloc_page(PageType::Data).unwrap();
                        let page = make_data_page(id);
                        s.write_page(id, &page).unwrap();
                        id
                    })
                    .collect();
                (s, ids)
            },
            |(s, ids)| {
                ids.iter().for_each(|&id| {
                    s.read_page(id).unwrap();
                });
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ── MmapStorage benchmarks ────────────────────────────────────────────────────
//
// The storage is created ONCE before the measurement loop. This way we measure
// only the real operation (alloc, write, read) without including create()/mmap()/set_len().

fn bench_mmap_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap/alloc");
    group.throughput(Throughput::Elements(1));

    group.bench_function("alloc_page", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_alloc.db");
        let storage = MmapStorage::create(&path).unwrap();
        // Pre-grow to 10_000 pages so the benchmark does not trigger grows.
        storage.grow(10_000).unwrap();

        b.iter(|| {
            let id = storage.alloc_page(PageType::Data).unwrap();
            // Free immediately to reuse the same page and prevent
            // the storage from growing during measurement.
            storage.free_page(id).unwrap();
            // Flush deferred frees back to bitmap so the page can be
            // reallocated on the next iteration.
            storage.release_deferred_frees(u64::MAX).unwrap();
        });
    });

    group.finish();
}

fn bench_mmap_write_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap/write_read");
    group.throughput(Throughput::Bytes(axiomdb_storage::PAGE_SIZE as u64));

    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_wr.db");
    let storage = MmapStorage::create(&path).unwrap();
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    let page = make_data_page(page_id);

    // Measure only the 16 KB copy to mmap.
    group.bench_function("write_page", |b| {
        b.iter(|| storage.write_page(page_id, &page).unwrap());
    });

    // Measure only zero-copy access to mmap + verify CRC32c.
    group.bench_function("read_page", |b| {
        b.iter(|| {
            let p = storage.read_page(page_id).unwrap();
            black_box(p.body()[0])
        });
    });

    group.finish();
}

fn bench_mmap_sequential_reads(c: &mut Criterion) {
    const N_PAGES: u64 = 1000;
    let mut group = c.benchmark_group("mmap/sequential");
    group.throughput(Throughput::Elements(N_PAGES));

    // One-time setup: storage with 1000 pages already written.
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_seq.db");
    let storage = MmapStorage::create(&path).unwrap();
    storage.grow(N_PAGES + 64).unwrap();
    let ids: Vec<u64> = (0..N_PAGES)
        .map(|_| {
            let id = storage.alloc_page(PageType::Data).unwrap();
            let page = make_data_page(id);
            storage.write_page(id, &page).unwrap();
            id
        })
        .collect();

    // Measure only the 1000 reads.
    group.bench_function(BenchmarkId::new("read_sequential", N_PAGES), |b| {
        b.iter(|| {
            ids.iter().for_each(|&id| {
                storage.read_page(id).unwrap();
            });
        });
    });

    group.finish();
}

// ── CRC32c throughput ─────────────────────────────────────────────────────────

fn bench_checksum_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("page/checksum");
    group.throughput(Throughput::Bytes(axiomdb_storage::PAGE_SIZE as u64));

    group.bench_function("verify_checksum", |b| {
        let page = make_data_page(42);
        b.iter(|| page.verify_checksum().unwrap())
    });

    group.bench_function("update_checksum", |b| {
        b.iter_batched(
            || make_data_page(42),
            |mut page| page.update_checksum(),
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ── Refcounted TOAST/BLOB overflow chains ────────────────────────────────────

fn bench_refcounted_blob_chain(c: &mut Criterion) {
    const SMALL_BLOB: usize = 12 * 1024;
    const LARGE_BLOB: usize = 128 * 1024;

    let small_payload: Vec<u8> = (0..SMALL_BLOB).map(|i| (i % 251) as u8).collect();
    let large_payload: Vec<u8> = (0..LARGE_BLOB).map(|i| (i % 251) as u8).collect();

    let mut group = c.benchmark_group("overflow/refcounted_blob");

    group.throughput(Throughput::Bytes(SMALL_BLOB as u64));
    group.bench_function("write_12kb", |b| {
        b.iter_batched(
            MemoryStorage::new,
            |storage| {
                let first =
                    clustered_overflow::write_refcounted_chain(&storage, None, &small_payload)
                        .unwrap()
                        .unwrap();
                black_box(first);
            },
            BatchSize::SmallInput,
        );
    });

    group.throughput(Throughput::Bytes(LARGE_BLOB as u64));
    group.bench_function("read_128kb", |b| {
        b.iter_batched(
            || {
                let storage = MemoryStorage::new();
                let first =
                    clustered_overflow::write_refcounted_chain(&storage, None, &large_payload)
                        .unwrap()
                        .unwrap();
                (storage, first)
            },
            |(storage, first)| {
                let out = clustered_overflow::read_blob_chain(&storage, first, LARGE_BLOB).unwrap();
                black_box(out.len());
            },
            BatchSize::SmallInput,
        );
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("incref_free_shared_128kb", |b| {
        b.iter_batched(
            || {
                let storage = MemoryStorage::new();
                let first =
                    clustered_overflow::write_refcounted_chain(&storage, None, &large_payload)
                        .unwrap()
                        .unwrap();
                (storage, first)
            },
            |(storage, first)| {
                let refs = clustered_overflow::incref_blob(&storage, first).unwrap();
                black_box(refs);
                clustered_overflow::free_blob(&storage, first).unwrap();
                clustered_overflow::free_blob(&storage, first).unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Phase 40.9: batch allocation benchmark ──────────────────────────────────

fn bench_batch_alloc_vs_per_page(c: &mut Criterion) {
    use axiomdb_storage::LocalPageBatch;
    use std::sync::Arc;

    let mut group = c.benchmark_group("alloc/batch_vs_single");
    group.throughput(Throughput::Elements(1000));

    // Single-page alloc baseline (1000 allocs under Mutex).
    group.bench_function("single_mutex_1000", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_single.db");
        let storage = MmapStorage::create(&path).unwrap();
        storage.grow(100_000).unwrap();

        b.iter(|| {
            for _ in 0..1000 {
                let id = storage.alloc_page(PageType::Data).unwrap();
                storage.free_page(id).unwrap();
            }
            storage.flush().unwrap();
        });
    });

    // Batch alloc (1000 allocs via LocalPageBatch — ~15 mutex acquisitions).
    group.bench_function("batch_local_1000", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_batch.db");
        let storage = MmapStorage::create(&path).unwrap();
        storage.grow(100_000).unwrap();

        b.iter(|| {
            let mut batch = LocalPageBatch::new();
            for _ in 0..1000 {
                let id = batch.pop_or_refill(&storage, PageType::Data).unwrap();
                batch.push_freed(id);
            }
            let (avail, freed) = batch.take_for_commit();
            storage.free_page_batch(&avail).unwrap();
            for pid in freed {
                storage.recycle_page(pid).unwrap();
            }
            storage.flush().unwrap();
        });
    });

    // 8-thread concurrent single-page alloc (10K allocs per thread).
    group.bench_function("8t_single_10k", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_8t_single.db");
        let storage = Arc::new(MmapStorage::create(&path).unwrap());
        storage.grow(100_000).unwrap();

        b.iter(|| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let s = Arc::clone(&storage);
                    std::thread::spawn(move || {
                        for _ in 0..10_000 {
                            let id = s.alloc_page(PageType::Data).unwrap();
                            s.free_page(id).unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            storage.flush().unwrap();
        });
    });

    // 8-thread concurrent batch alloc (10K allocs per thread via LocalPageBatch).
    group.bench_function("8t_batch_10k", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_8t_batch.db");
        let storage = Arc::new(MmapStorage::create(&path).unwrap());
        storage.grow(100_000).unwrap();

        b.iter(|| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let s = Arc::clone(&storage);
                    std::thread::spawn(move || {
                        let mut batch = LocalPageBatch::new();
                        for _ in 0..10_000 {
                            let id = batch.pop_or_refill(s.as_ref(), PageType::Data).unwrap();
                            batch.push_freed(id);
                        }
                        let (avail, freed) = batch.take_for_commit();
                        s.free_page_batch(&avail).unwrap();
                        for pid in freed {
                            s.recycle_page(pid).unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            storage.flush().unwrap();
        });
    });

    group.finish();
}

// ── Registration ─────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_memory_alloc,
    bench_memory_write_read,
    bench_memory_sequential_reads,
    bench_mmap_alloc,
    bench_mmap_write_read,
    bench_mmap_sequential_reads,
    bench_checksum_throughput,
    bench_refcounted_blob_chain,
    bench_batch_alloc_vs_per_page,
);
criterion_main!(benches);
