# /bench — Medir rendimiento correctamente

Nunca optimizar sin medir. Nunca mergear sin verificar que no se regresó.

## Setup de benchmarks

```toml
# Cargo.toml del crate
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

## Workflow antes de optimizar

```bash
# 1. Guardar baseline ANTES del cambio
cargo bench --workspace -- --save-baseline before

# 2. Hacer el cambio

# 3. Medir DESPUÉS
cargo bench --workspace -- --baseline before

# Criterion muestra automáticamente:
# point_lookup_pk: 1.2µs → 0.8µs  (-33%) ✅ mejora
# range_scan/10000: 45ms → 48ms   (+6%)  ⚠️  regresión menor
```

## Presupuesto de rendimiento

| Operación             | Objetivo     | Máximo     | Acción si supera máximo |
|-----------------------|--------------|------------|------------------------|
| Point lookup PK       | 800k ops/s   | 600k ops/s | Bloqueante — investigar |
| Range scan 10K rows   | 45ms         | 60ms       | Bloqueante — investigar |
| INSERT con WAL        | 180k ops/s   | 150k ops/s | Bloqueante — investigar |
| Seq scan 1M rows      | 0.8s         | 1.2s       | Bloqueante — investigar |
| Concurrent reads x16  | escala lineal| <2x drop   | Bloqueante — investigar |

## Comparar vs MySQL (objetivo final)

```bash
# Instalar sysbench
brew install sysbench

# Benchmark MySQL
sysbench oltp_point_select \
  --mysql-host=localhost --mysql-port=3306 \
  --mysql-db=test --mysql-user=root \
  --tables=1 --table-size=1000000 \
  run > /tmp/mysql_results.txt

# Benchmark dbyo (mismo sysbench, diferente puerto)
sysbench oltp_point_select \
  --mysql-host=localhost --mysql-port=3306 \  # dbyo habla MySQL protocol
  --mysql-db=test --mysql-user=root \
  --tables=1 --table-size=1000000 \
  run > /tmp/dbyo_results.txt

# Comparar
diff /tmp/mysql_results.txt /tmp/dbyo_results.txt
```
