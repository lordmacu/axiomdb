# /bench — Measure performance correctly

Never optimize without measuring. Never merge without verifying there was no regression.

## Benchmark setup

```toml
# crate's Cargo.toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "storage_bench"
harness = false
```

```rust
// benches/storage_bench.rs
use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};

fn bench_point_lookup(c: &mut Criterion) {
    let engine = setup_bench_engine_1m_rows();

    c.bench_function("point_lookup_pk", |b| {
        b.iter(|| {
            engine.execute("SELECT * FROM users WHERE id = 42")
        })
    });
}

fn bench_range_scan(c: &mut Criterion) {
    let engine = setup_bench_engine_1m_rows();

    let mut group = c.benchmark_group("range_scan");
    for size in [1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| engine.execute(&format!("SELECT * FROM users LIMIT {size}")))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_point_lookup, bench_range_scan);
criterion_main!(benches);
```

## Workflow before optimizing

```bash
# 1. Save baseline BEFORE the change
cargo bench --workspace -- --save-baseline before

# 2. Make the change

# 3. Measure AFTER
cargo bench --workspace -- --baseline before

# Criterion automatically shows:
# point_lookup_pk: 1.2µs → 0.8µs  (-33%) ✅ improvement
# range_scan/10000: 45ms → 48ms   (+6%)  ⚠️  minor regression
```

## Performance budget

| Operation             | Target       | Maximum    | Action if exceeded      |
|-----------------------|--------------|------------|------------------------|
| Point lookup PK       | 800k ops/s   | 600k ops/s | Blocker — investigate  |
| Range scan 10K rows   | 45ms         | 60ms       | Blocker — investigate  |
| INSERT with WAL       | 180k ops/s   | 150k ops/s | Blocker — investigate  |
| Seq scan 1M rows      | 0.8s         | 1.2s       | Blocker — investigate  |
| Concurrent reads x16  | linear scale | <2x drop   | Blocker — investigate  |

## Compare vs MySQL (final goal)

```bash
# Install sysbench
brew install sysbench

# Benchmark MySQL
sysbench oltp_point_select \
  --mysql-host=localhost --mysql-port=3306 \
  --mysql-db=test --mysql-user=root \
  --tables=1 --table-size=1000000 \
  run > /tmp/mysql_results.txt

# Benchmark AxiomDB (same sysbench, different port)
sysbench oltp_point_select \
  --mysql-host=localhost --mysql-port=3306 \  # axiomdb speaks MySQL protocol
  --mysql-db=test --mysql-user=root \
  --tables=1 --table-size=1000000 \
  run > /tmp/axiomdb_results.txt

# Compare
diff /tmp/mysql_results.txt /tmp/axiomdb_results.txt
```
