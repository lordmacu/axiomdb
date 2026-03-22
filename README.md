# NexusDB — Motor de Base de Datos en Rust

Motor de base de datos diseñado para superar a MySQL en benchmarks específicos.
Proyecto universitario en Rust con arquitectura moderna.

## Features principales

- **Storage**: mmap + páginas 8KB, sin double-buffering
- **Índices**: Copy-on-Write B+ Tree — readers sin locks
- **Concurrencia**: Tokio async (I/O) + Rayon (CPU) + MVCC
- **Ejecución**: SIMD AVX2 + morsel parallelism + operator fusion
- **Compatibilidad**: MySQL wire protocol (:3306) + PostgreSQL (:5432) simultáneos
- **Embebido**: compila como `.so`/`.dll` para apps de escritorio
- **AI-Native**: VECTOR(n), búsqueda híbrida BM25+HNSW, RAG pipeline nativo

## Quickstart

```bash
# Servidor
cargo run --bin nexusdb-server -- --data-dir ./data

# Conectar con psql
psql -h localhost -p 5432 -U root myapp

# Conectar con mysql client
mysql -h localhost -P 3306 -u root myapp

# Modo embebido (Python)
pip install nexusdb-python
python -c "import nexusdb; db = nexusdb.open('myapp.db'); db.execute('SELECT 1')"
```

## Arquitectura

```
Cliente (MySQL/PostgreSQL protocol)
    ↓
SQL (Parser → Optimizer → Executor)
    ↓
MVCC (Snapshot Isolation + SSI)
    ↓
Índices (B+ Tree CoW + HNSW + GIN + FTS)
    ↓
Storage (mmap + WAL + TOAST)
    ↓
Disco (.db + .wal + .idx)
```

## Plan de desarrollo

35 fases / ~83 semanas. Ver [`docs/progreso.md`](docs/progreso.md) para el estado actual.

## Documentación de diseño

Ver [`db.md`](db.md) para el diseño completo: tipos, optimizaciones, fases y decisiones.

## Nombre

**NexusDB** — una base de datos es el punto central de conexión de toda aplicación.
El nombre refleja exactamente eso: el nexo entre los datos y el mundo.

## Benchmarks objetivo

| Operación           | dbyo         | MySQL 8.0    | Delta   |
|---------------------|--------------|--------------|---------|
| Point lookup PK     | 800k ops/s   | 350k ops/s   | +128%   |
| Range scan 10K rows | 45ms         | 120ms        | -62%    |
| Seq scan 1M rows    | 0.8s         | 3.4s         | -76%    |
| INSERT con WAL      | 180k ops/s   | 95k ops/s    | +89%    |
| Concurrent reads x16| escala lineal| se satura    | +200%+  |

## Licencia

MIT
