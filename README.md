# AxiomDB — Database Engine in Rust

Database engine designed to outperform MySQL on specific benchmarks.
University project in Rust with modern architecture.

## Main features

- **Storage**: mmap + 8KB pages, no double-buffering
- **Indexes**: Copy-on-Write B+ Tree — lock-free readers
- **Concurrency**: Tokio async (I/O) + Rayon (CPU) + MVCC
- **Execution**: SIMD AVX2 + morsel parallelism + operator fusion
- **Compatibility**: MySQL wire protocol (:3306) + PostgreSQL (:5432) simultaneous
- **Embedded**: compiles as `.so`/`.dll` for desktop apps
- **AI-Native**: VECTOR(n), hybrid BM25+HNSW search, native RAG pipeline

## Quickstart

```bash
# Server
cargo run --bin axiomdb-server -- --data-dir ./data

# Connect with psql
psql -h localhost -p 5432 -U root axiomdb

# Connect with mysql client
mysql -h localhost -P 3306 -u root axiomdb

# Embedded mode (Python)
pip install axiomdb-python
python -c "import axiomdb; db = axiomdb.open('axiomdb.db'); db.execute('SELECT 1')"
```

## Architecture

```
Client (MySQL/PostgreSQL protocol)
    ↓
SQL (Parser → Optimizer → Executor)
    ↓
MVCC (Snapshot Isolation + SSI)
    ↓
Indexes (B+ Tree CoW + HNSW + GIN + FTS)
    ↓
Storage (mmap + WAL + TOAST)
    ↓
Disk (.db + .wal + .idx)
```

## Development plan

35 phases / ~83 weeks. See [`docs/progreso.md`](docs/progreso.md) for current status.

## Design documentation

See [`db.md`](db.md) for the complete design: types, optimizations, phases and decisions.

## Name

**AxiomDB** — a database is the central connection point of every application.
The name reflects exactly that: the axiom between data and the world.

## Target benchmarks

| Operation           | AxiomDB      | MySQL 8.0    | Delta   |
|---------------------|--------------|--------------|---------|
| Point lookup PK     | 800k ops/s   | 350k ops/s   | +128%   |
| Range scan 10K rows | 45ms         | 120ms        | -62%    |
| Seq scan 1M rows    | 0.8s         | 3.4s         | -76%    |
| INSERT with WAL     | 180k ops/s   | 95k ops/s    | +89%    |
| Concurrent reads x16| linear scale | saturates    | +200%+  |

## License

MIT
