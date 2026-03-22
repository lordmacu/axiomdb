use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use nexusdb_storage::{MemoryStorage, MmapStorage, Page, PageType, StorageEngine};
use tempfile::tempdir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_data_page(page_id: u64) -> Page {
    let mut page = Page::new(PageType::Data, page_id);
    // Datos realistas — evita optimizaciones del OS/compilador para ceros.
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
            || MemoryStorage::new(),
            |mut s| s.alloc_page(PageType::Data).unwrap(),
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_memory_write_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/write_read");
    group.throughput(Throughput::Bytes(nexusdb_storage::PAGE_SIZE as u64));

    group.bench_function("write_page", |b| {
        b.iter_batched(
            || {
                let mut s = MemoryStorage::new();
                let id = s.alloc_page(PageType::Data).unwrap();
                (s, id, make_data_page(id))
            },
            |(mut s, id, page)| s.write_page(id, &page).unwrap(),
            BatchSize::SmallInput,
        )
    });

    group.bench_function("read_page", |b| {
        b.iter_batched(
            || {
                let mut s = MemoryStorage::new();
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
                let mut s = MemoryStorage::new();
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
// El storage se crea UNA VEZ antes del loop de medición. Así medimos solo la
// operación real (alloc, write, read) sin incluir create()/mmap()/set_len().

fn bench_mmap_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap/alloc");
    group.throughput(Throughput::Elements(1));

    group.bench_function("alloc_page", |b| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bench_alloc.db");
        let mut storage = MmapStorage::create(&path).unwrap();
        // Pre-grow a 10_000 páginas para que el benchmark no dispare grows.
        storage.grow(10_000).unwrap();

        b.iter(|| {
            let id = storage.alloc_page(PageType::Data).unwrap();
            // Liberar inmediatamente para reutilizar la misma página y evitar
            // que el storage crezca durante la medición.
            storage.free_page(id).unwrap();
        });
    });

    group.finish();
}

fn bench_mmap_write_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap/write_read");
    group.throughput(Throughput::Bytes(nexusdb_storage::PAGE_SIZE as u64));

    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_wr.db");
    let mut storage = MmapStorage::create(&path).unwrap();
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    let page = make_data_page(page_id);

    // Medir solo el copy de 16KB al mmap.
    group.bench_function("write_page", |b| {
        b.iter(|| storage.write_page(page_id, &page).unwrap());
    });

    // Medir solo acceso zero-copy al mmap + verify CRC32c.
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

    // Setup único: storage con 1000 páginas ya escritas.
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_seq.db");
    let mut storage = MmapStorage::create(&path).unwrap();
    storage.grow(N_PAGES + 64).unwrap();
    let ids: Vec<u64> = (0..N_PAGES)
        .map(|_| {
            let id = storage.alloc_page(PageType::Data).unwrap();
            let page = make_data_page(id);
            storage.write_page(id, &page).unwrap();
            id
        })
        .collect();

    // Medir solo las 1000 lecturas.
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
    group.throughput(Throughput::Bytes(nexusdb_storage::PAGE_SIZE as u64));

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

// ── Registro ──────────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_memory_alloc,
    bench_memory_write_read,
    bench_memory_sequential_reads,
    bench_mmap_alloc,
    bench_mmap_write_read,
    bench_mmap_sequential_reads,
    bench_checksum_throughput,
);
criterion_main!(benches);
