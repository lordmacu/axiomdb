# AxiomDB — Database Engine in Rust

> University project: portable, fast database engine with SQL, indexes, FK, and concurrency.
> Goal: outperform MySQL on specific benchmarks.

---

## Decision Summary

| Decision | Choice | Reason |
|---|---|---|
| Language | **Rust** | No GC, memory control, maximum speed |
| Wire protocol | **MySQL** | PHP/Python connect without custom drivers |
| Embedded | **C FFI + cdylib** | In-process like SQLite, zero latency |
| Storage | **mmap + 8 KB pages** | Eliminates InnoDB double-buffering |
| Index | **Copy-on-Write B+ Tree** | Lock-free readers = high concurrency |
| Durability | **WAL append-only** | No double-write buffer |
| Concurrency | **Tokio async** | Thousands of connections without thread overhead |
| Queries | **Vectorized + SIMD** | 10-50× on scans vs row-by-row |

---

## Usage Modes

**Server mode** — MySQL wire protocol on port 3306. PHP/Python/Node connect without custom drivers.

**Embedded mode** — in-process via C FFI (like SQLite). Zero network latency. Produces `.so`/`.dll`/`.dylib`.

```
Mode          Latency    Multiple processes   Ideal for
Server        ~0.1ms     Yes                  Web, APIs, microservices
Embedded      ~1µs       No (one process)     Desktop, CLI, scripts
```

Compatible clients (no changes needed): PHP PDO::mysql, Python PyMySQL, MySQL Workbench, DBeaver.

---

## General Architecture

```
Clients (MySQL wire / C FFI / embedding)
    ↓
axiomdb-network  ←→  axiomdb-embedded
    ↓                       ↓
           axiomdb-sql (parser → planner → executor)
                    ↓
           axiomdb-catalog (schema, stats)
                    ↓
           axiomdb-mvcc (transactions, snapshot isolation)
                    ↓
           axiomdb-index (B+ Tree CoW, FTS, HNSW, GIN)
                    ↓
     axiomdb-wal ←→ axiomdb-storage (mmap, TOAST)
                    ↓
           axiomdb-types (Value enum, row codec)
                    ↓
           axiomdb-core (DbError, traits)
```

---

## Type System — Implemented (current Value enum)

```
Variant                    SQL Type            Notes
Null                       NULL                any column
Bool(bool)                 BOOLEAN
Int(i32)                   INT / INTEGER
BigInt(i64)                BIGINT
Real(f64)                  REAL / DOUBLE       NaN forbidden in codec
Decimal(i128, u8)          DECIMAL(p,s)        mantissa × 10^(-scale)
Text(String)               TEXT / VARCHAR      u24 prefix, TOAST if >2KB
Bytes(Vec<u8>)             BLOB / BYTEA        u24 prefix, TOAST if >2KB
Date(i32)                  DATE                days since 1970-01-01
Timestamp(i64)             TIMESTAMP           µs since 1970-01-01 UTC
Uuid([u8; 16])             UUID                big-endian byte order
Json(String)               JSON                validated UTF-8 text (Phase 11.4)
Jsonb(Arc<Vec<u8>>)        JSONB               binary layout (Phase 11.16)
```

ColumnType (on-disk u8 discriminant) must always match DataType. Never add one without the other.
TOAST sentinel in codec: `__toast__:page_id:compressed:raw_len` (3-byte marker on high u24 bits).

---

## Type System — Future Planned Types

| SQL Type | Phase | Notes |
|---|---|---|
| `TINYINT`, `SMALLINT`, `UINT`, `UBIGINT`, `HUGEINT` | 20 | Additional integer sizes |
| `FLOAT` (f32) | 20 | 4-byte float |
| `CHAR(n)`, `VARCHAR(n)` | 20 | Fixed/bounded strings |
| `CITEXT` | 20 | Case-insensitive text |
| `BIT(n)`, `VARBIT(n)` | 20 | Bit vectors |
| `TIME`, `TIMETZ`, `TIMESTAMPTZ`, `INTERVAL` | 20 | Date/time extensions |
| `INET`, `CIDR`, `MACADDR` | 20 | Network types |
| `VECTOR(n)` | 22 | AI embeddings, f32 per element |
| `T[]` (arrays) | 20 | Any type, TOAST |
| `ENUM` | 20 | u32 index, validated on insert |
| `RANGE(T)` | 20 | int4range, daterange, tsrange |
| `COMPOSITE` | 20 | CREATE TYPE … AS (…) |
| `DOMAIN` | 20 | CHECK constraint on base type |

---

## Type System — Key Design Decisions

**NULL bitmap**: 1 bit per column in the row header (not `Option<T>` per value). Saves 7 bytes × nullable_columns per row.

**DECIMAL**: `i128` mantissa + `u8` scale, exact arithmetic. Never use f64 for money.

**UUID v7 over v4**: timestamp prefix (48 bits) + random (80 bits) = nearly sequential inserts = fewer B+ Tree splits. UUID v7 PK: ~250K inserts/s vs UUID v4: ~150K inserts/s.

**TIMESTAMPTZ**: always store in µs UTC internally, convert on display. TIMESTAMP without timezone creates ambiguity across server/client timezone differences.

**Column encoding** (future, for analytics): Plain (high cardinality), Dictionary (status/country ≤256 values → 7× savings), Delta (timestamps/IDs), RunLength (sorted repeated), BitPacking (small integer range), FrameOfReference (time windows). Planner chooses automatically based on sample statistics.

**VarInt encoding** (future): 1 byte for 0-127, 2 bytes for 128-16383, etc. Saves 87% on small IDs.

**PAX layout** (future, Phase 9+ analytics): columnar within page = 3× less I/O for analytics queries vs row-major NSM.

---

## Storage Engine

- **mmap**: file mapped into address space; OS page cache handles eviction; no double-buffering.
- **8 KB pages**: CRC32c checksum in header. Free list tracks available pages. `alloc_page` / `free_page`.
- **TOAST**: values >2 KB spill to overflow pages via `clustered_overflow.rs`. Refcounted BLOB chain uses `ABOB` versioned header with first-page refcount (`incref_blob` / `free_blob`). Legacy clustered overflow uses simpler chain (non-refcounted).
- **Buffer pool** (Phase 11.8): LRU per shard. Eviction scans at most once; exits if all candidates pinned.
- **Write combining** (Phase 11.10): coalesce adjacent dirty pages before flush.
- **Prefetch** (Phase 11.9): bounded read-ahead hints on sequential access.

---

## B+ Tree (Copy-on-Write)

- **CoW invariant**: readers never block. Each write allocates new pages; old pages remain until GC.
- **Root**: `AtomicU64` for lock-free root swap after each write.
- **Prefix compression**: common key prefixes stored once per node; saves 3-5× keys per node.
- **Bloom filter per index**: 1% false positive rate eliminates unnecessary I/O on point lookups.
- **Sparse index**: every 8192nd key stored; 8192× less RAM than dense index.
- **Covering index**: INCLUDE columns stored in leaf nodes; avoid table heap fetch for covered queries.
- **Partial index**: predicate stored in `IndexDef`; only rows matching predicate are indexed.
- **Clustered table** (Phase 39): `CREATE TABLE ... PRIMARY KEY` → clustered tree. Row data stored in B+ Tree leaf directly (InnoDB-style). Secondary indexes store `secondary_key ++ PK_bookmark`.
- **Safe descent rule**: Delete requires stricter predicate than insert — leaf must have >MIN_KEYS_LEAF keys AND not trigger CoW rebalance. Clustered trees use byte occupancy, not key count.

---

## WAL (Write-Ahead Log)

- Append-only log. Each record: entry type + txn_id + payload + CRC32c.
- `TxnManager`: snapshot isolation. `txn_id_created` / `txn_id_deleted` in each row header.
- Crash recovery: replay from last checkpoint, undo uncommitted.
- Clustered WAL key = PK bytes (not page_id/slot_id). Payload = exact logical row image.
- Rollback restores logical row state, not exact page topology.
- fsync pipeline: leader-based coalescing (`Acquired` / `Queued(rx)` / `Expired`). No timer-based coordinator.
- WAL rotation + checkpointing maintains bounded log size.

---

## Indexes

- **Secondary B+ Tree**: key = encoded column values + RecordId (heap) or PK bookmark (clustered).
- **FTS** (Phase 11.7): inverted index + BM25 ranking. Tokenizer → posting lists. Boolean, phrase, prefix. `CREATE VIRTUAL TABLE ... USING fts(...)`. `MATCH` operator. `to_tsvector` / `ts_rank` / `ts_headline`.
- **HNSW** (Phase 22): vector similarity search for `VECTOR(n)`.
- **GIN** (Phase 11.17): for JSONB containment and FTS. Inverted index on document terms.
- **GiST / BRIN / Hash** (future phases): for geometric, range, and equality-only columns.
- **Expression indexes**: index on `LOWER(email)`, `YEAR(created_at)`, etc.

---

## Foreign Keys

- Enforced via reverse index: child table has index on FK columns; parent delete/update probes child index.
- `ON DELETE CASCADE / RESTRICT / SET NULL` supported.
- Clustered parent DELETE collects row values before mutation; passes to FK enforcer (no heap RecordId dependency).

---

## MVCC / Transactions

- Snapshot isolation: each transaction sees rows where `txn_id_created <= snapshot_txn` and `txn_id_deleted > snapshot_txn` (or not deleted).
- Savepoints: `SAVEPOINT / ROLLBACK TO / RELEASE`.
- SSI (serializable snapshot isolation): future phase.
- VACUUM: clustered purge walks leaf chain, removes cells where `txn_id_deleted < oldest_safe_txn`. Cleans secondary bookmarks by physical row existence, not snapshot visibility.
- Deadlock detection: DFS on wait graph every 100ms.

---

## Vectorized Execution + Parallelism

- **Morsel-driven** (DuckDB-inspired): 1M rows split into ~100K-row morsels, distributed via Rayon. 7× speedup on 8 cores.
- **Operator fusion**: scan + filter + project in one loop, no intermediate buffers.
- **Late materialization**: apply cheap predicates first (e.g., `age > 25`), fetch expensive columns only for surviving rows.
- **SIMD** (Phase 8): AVX2 via `wide` crate. TINYINT: 32 values/instruction; BIGINT: 4 values/instruction.
- **Zone maps** (Phase 8): min/max per page block → skip entire pages on inequality predicates.
- **EXPLAIN ANALYZE**: actual timing per node, rows estimated vs actual.

---

## Optimization Decisions (by source)

### PostgreSQL-inspired (implemented)
- Partial indexes: `WHERE predicate` in index def → 10-100× smaller for sparse columns
- EXPLAIN ANALYZE: real timing per plan node
- TOAST: inline spill to overflow pages + LZ4 compression when large
- Covering indexes: `INCLUDE (cols)` in leaf nodes → index-only scans
- Materialized views: `CREATE MATERIALIZED VIEW` + `REFRESH` (incremental future)
- Window functions: `RANK / ROW_NUMBER / LAG / LEAD / SUM OVER (PARTITION BY ... ORDER BY ...)`
- Generated columns: STORED (written on INSERT/UPDATE) and VIRTUAL (computed on read)
- LISTEN/NOTIFY: pub-sub without external broker
- Non-blocking schema changes: shadow table + WAL delta + atomic rename
- Startup index integrity: verify and auto-repair heap↔index divergence before serving

### DuckDB-inspired (implemented)
- Morsel-driven parallelism (see above)
- Operator fusion (see above)
- Late materialization (see above)
- Hash join + sort-merge join with spill to disk (Phase 9)
- Adaptive join selection (cost-based per query)

### SQLite-inspired (implemented)
- In-memory mode: `:memory:` = MemoryStorage (HashMap<page_id, [u8;8192]>)
- FTS inverted index pattern
- Overflow chain: key + MVCC header always inline, only row tail spills
- Explicit PK → clustered table; no PK → heap (no hidden rowid by default)

### RocksDB-inspired (implemented)
- Bloom filters per index (see above)
- Sparse index (see above)
- Prefix compression in B+ Tree nodes (see above)

### TimescaleDB-inspired (future Phases 14-19)
- Table partitioning: `PARTITION BY RANGE / HASH / LIST`
- Partition pruning in query planner
- Auto-compression: partitions older than threshold → columnar + LZ4
- Continuous aggregates with incremental refresh
- TTL per row: `expires_at` + background reaper Tokio task

### Redis-inspired (implemented + future)
- LRU eviction for in-memory mode (implemented)
- Lua scripting `EVAL` with `query()` / `execute()` atomic (Phase 16)

### MongoDB-inspired (future Phase 15)
- Change streams CDC: read WAL → emit `ChangeEvent::Insert/Update/Delete`
- Watch queries for embedded mode: `WATCH SELECT ...` → tokio channel

### DoltDB-inspired (future Phase 15)
- Git for data: commits (snapshot table roots), branches (CoW over B+ Tree), checkout, merge, diff
- `dolt_commit`, `dolt_branch`, `dolt_merge`, `dolt_diff` as SQL procedures

### Apache Arrow (future Phase 15)
- Columnar format for analytical result streaming
- Zero-copy with IPC format for Python/R/Spark interop

---

## Crates — Workspace Structure

```
axiomdb-core        ← DbError, traits — zero external deps
axiomdb-types       ← Value, DataType, row codec, JSONB layout
axiomdb-storage     ← mmap pages, free list, TOAST, overflow chains, buffer pool
axiomdb-wal         ← WAL writer/reader, crash recovery, TxnManager
axiomdb-index       ← B+ Tree CoW, FTS inverted index, future: HNSW/GIN/GiST
axiomdb-mvcc        ← snapshot isolation, savepoints, VACUUM, SSI (future)
axiomdb-catalog     ← schema, ColumnType, statistics, information_schema
axiomdb-sql         ← parser (logos), AST, planner, optimizer, executor, functions
axiomdb-functions   ← built-in scalar/aggregate functions
axiomdb-network     ← MySQL wire protocol, prepared statements, COM_STMT_*
axiomdb-security    ← RBAC, RLS, TLS, Argon2, audit (future)
axiomdb-replication ← streaming WAL replication, PITR (future)
axiomdb-plugins     ← WASM runtime, Lua scripting (future)
axiomdb-cache       ← query result cache (future)
axiomdb-geo         ← geometric types, R-Tree (future)
axiomdb-vector      ← VECTOR(n), HNSW, quantization (future)
axiomdb-migrations  ← CLI migrations, schema versioning (future)
axiomdb-sync        ← Mobile sync: HLC, delta sync, CRDTs (future Phase 36)
axiomdb-embedded    ← cdylib: C FFI + flutter_rust_bridge entry point
axiomdb-server      ← TCP daemon binary
```

Dependency order (no cycles):
```
axiomdb-core → axiomdb-types → axiomdb-storage / axiomdb-wal
→ axiomdb-index → axiomdb-mvcc → axiomdb-catalog → axiomdb-sql
→ axiomdb-network / axiomdb-embedded → axiomdb-server
```

---

## SQL Supported (implemented)

### DDL
```sql
CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL, data JSONB);
CREATE INDEX idx ON t(col1, col2);
CREATE INDEX partial ON t(col1) WHERE col2 = 'active';
CREATE INDEX covering ON t(col1) INCLUDE (col2, col3);
CREATE UNIQUE INDEX ON users(email) WHERE deleted_at IS NULL;
DROP TABLE / DROP INDEX / REINDEX;
ALTER TABLE ADD COLUMN / DROP COLUMN / MODIFY COLUMN / ADD PRIMARY KEY;
ALTER TABLE ADD CONSTRAINT / DROP CONSTRAINT;
CREATE DATABASE / DROP DATABASE / USE db;
CREATE SCHEMA / SET search_path;
SHOW TABLES / SHOW DATABASES / SHOW CREATE TABLE / DESCRIBE;
```

### DML
```sql
INSERT INTO t (id, name) VALUES (1, 'Alice');
INSERT ... ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name;
INSERT INTO backup SELECT * FROM orders WHERE year = 2025;
SELECT *, COALESCE, NULLIF, GREATEST, LEAST, DISTINCT ON;
SELECT ... FROM ... JOIN ... ON ... WHERE ... GROUP BY ... HAVING ... ORDER BY ... LIMIT ... OFFSET;
UPDATE t SET col = val WHERE ...; (indexed candidate discovery, stable-RID fast path)
DELETE FROM t WHERE ...;
RETURNING clause on INSERT / UPDATE / DELETE;
MERGE (UPSERT);
WITH cte AS (...) SELECT ...; (CTE + WITH RECURSIVE)
LATERAL joins, subqueries;
Window functions: RANK / ROW_NUMBER / LAG / LEAD / SUM OVER (PARTITION BY);
GENERATE_SERIES, UNNEST;
```

### Transactions
```sql
BEGIN / COMMIT / ROLLBACK;
SAVEPOINT s / ROLLBACK TO s / RELEASE s;
SET TRANSACTION ISOLATION LEVEL (snapshot isolation default);
@@autocommit, SET SESSION VARIABLES;
```

### JSON / JSONB functions (implemented)
```sql
data->>'key'            -- text extract (JSON_EXTRACT)
data->'key'             -- JSONB sub-document extract (Phase 11.16)
JSON_EXTRACT, JSON_SET, JSON_REMOVE, JSON_KEYS, JSON_VALID, JSON_TYPE
JSON_MERGE_PATCH, JSON_CONTAINS, JSON_OVERLAPS
JSON_PATH_EXISTS, JSON_PATH_QUERY, JSON_PATH_QUERY_FIRST
JSON_ARRAY_LENGTH, JSON_DEPTH, JSON_PRETTY
TO_JSONB, JSONB(text), CAST(expr AS JSONB)
```

### FTS (implemented)
```sql
CREATE VIRTUAL TABLE docs USING fts(body);
SELECT * FROM docs WHERE MATCH(body) AGAINST ('rust database' IN BOOLEAN MODE);
SELECT ts_rank(to_tsvector('english', body), to_tsquery('rust & fast'));
```

---

## Plan de desarrollo (fases)

```
Fase 1 — Storage básico             (semana 1-2)
  ✓ Leer/escribir páginas en disco
  ✓ mmap del archivo .db
  ✓ Formato de página con checksum
  ✓ Free list de páginas

Fase 2 — B+ Tree                    (semana 3-4)
  ✓ Nodos internos y hojas
  ✓ Insert con split de nodos
  ✓ Lookup por key exacto
  ✓ Range scan por linked list de hojas
  ✓ Delete con merge/redistribución

Fase 3 — WAL y transacciones        (semana 5)
  ✓ Append-only WAL
  ✓ BEGIN / COMMIT / ROLLBACK
  ✓ Crash recovery (replay del WAL)

Fase 4 — SQL Parser + Executor      (semana 6-7)
  ✓ Parser para DDL y DML básico
  ✓ Executor conectado al storage
  ✓ CLI interactiva (como sqlite3 shell)

Fase 5 — MySQL wire protocol        (semana 8)
  ✓ TCP server en Tokio
  ✓ MySQL handshake + autenticación básica
  ✓ PHP y Python conectan sin driver custom

Fase 6 — Índices secundarios + FK   (semana 9)
  ✓ Múltiples B+ Trees por tabla
  ✓ Validación de FK en INSERT/UPDATE/DELETE
  ✓ ON DELETE CASCADE / RESTRICT

Fase 7 — Concurrencia + MVCC        (semana 10)
  ✓ Copy-on-Write en B+ Tree
  ✓ Snapshot isolation para reads
  ✓ Readers concurrentes + writer global serializado

Fase 8 — Optimizaciones SIMD        (semana 11-12)
  ✓ Vectorized execution en table scans
  ✓ SIMD para predicados simples (AVX2 con crate `wide`)
  ✓ Query planner básico (usar índice vs full scan)
  ✓ Benchmarks vs MySQL

Fase 9 — DuckDB-inspired            (semana 13-14)
  ✓ Morsel-driven parallelism (Rayon, un morsel por core)
  ✓ Operator fusion (scan+filter+project en un pipeline lazy)
  ✓ Late materialization (predicados baratos primero, materializar al final)
  ✓ Benchmarks actualizados con paralelismo

Fase 10 — Modo embebido + FFI       (semana 15-16)
  ✓ Refactor del motor como crate reutilizable (lib.rs)
  ✓ C FFI: axiomdb_open / axiomdb_execute / axiomdb_close
  ✓ Compilar como cdylib (.so / .dll / .dylib)
  ✓ Binding Python (ctypes) para pruebas
  ✓ Binding Node.js via Neon (opcional, para Electron)
  ✓ Tests: misma BD usada desde servidor y desde librería
  ✓ [MOBILE] flutter_rust_bridge: bindings Dart generados desde la API Rust de axiomdb-embedded
  ✓ [MOBILE] Cross-compilación declarada: aarch64-apple-ios, aarch64-linux-android, x86_64-linux-android
  ✓ [MOBILE] Perfil de release mobile: opt-level="z", lto=true, strip=true, panic=abort
  ✓ [MOBILE] Tests de integración: misma BD usada desde Flutter (Dart) y desde C FFI
  ✓ [DESKTOP] Tauri plugin: axiomdb-embedded como plugin Tauri — produce .msi/.dmg/.AppImage con axiomdb dentro
  ✓ [DESKTOP] npm package axiomdb-node: wheels precompilados por plataforma (win32-x64, darwin-arm64, linux-x64)
  ✓ [DESKTOP] Universal binary macOS: lipo -create arm64 + x86_64 → libaxiomdb.dylib corre en Intel y Apple Silicon
  ✓ [DESKTOP] cargo-bundle: `cargo bundle --release` genera .app (macOS), .deb (Linux), instalador (Windows)

Fase 11 — Robustez (RocksDB + SQLite inspired)  (semana 17-18)
  ✓ Bloom filters en cada índice B+ Tree
  ✓ Sparse index para columnas de timestamp/secuencia
  ✓ Prefix compression en nodos internos del B+ Tree
  ✓ TOAST: valores >2KB a páginas de overflow con LZ4
  ✓ In-memory mode (":memory:")
  ✓ JSON como tipo nativo con extracción por path
  ✓ Partial indexes con predicado en CREATE INDEX
  ✓ Full-Text Search: tokenizer + índice invertido + BM25 ranking
  ✓ CREATE VIRTUAL TABLE ... USING fts(...)
  ✓ MATCH operator con soporte de frases, booleanos y prefijos
  ✓ TSVECTOR — tipo de columna para documentos FTS pre-procesados almacenables
  ✓ TSQUERY — tipo de columna para queries FTS almacenadas y reutilizables
  ✓ to_tsvector() / to_tsquery() / ts_rank() / ts_headline() como funciones SQL
  ✓ GENERATED ALWAYS AS (to_tsvector('english', body)) STORED — columna FTS automática

Fase 12 — Testing + JIT             (semana 19-20)
  ✓ Deterministic simulation testing (FaultInjector con semilla)
  ✓ EXPLAIN ANALYZE con tiempos reales por nodo
  ✓ JIT compilation básica con LLVM (predicados simples)
  ✓ Benchmarks finales vs MySQL y SQLite

Fase 13 — PostgreSQL avanzado       (semana 21-22)
  ✓ Materialized views (CREATE MATERIALIZED VIEW + REFRESH)
  ✓ Window functions (RANK, ROW_NUMBER, LAG, LEAD, SUM OVER)
  ✓ Generated/computed columns (STORED y VIRTUAL)
  ✓ LISTEN / NOTIFY pub-sub nativo
  ✓ Covering indexes (INCLUDE columns en B+ Tree hojas)
  ✓ Non-blocking ALTER TABLE (shadow table + WAL delta)

Fase 14 — TimescaleDB + Redis       (semana 23-24)
  ✓ Table partitioning (RANGE, HASH, LIST)
  ✓ Partition pruning en el query planner
  ✓ Compresión automática de particiones históricas (LZ4 columnar)
  ✓ Continuous aggregates con refresh incremental
  ✓ TTL por fila con background reaper en Tokio
  ✓ LRU eviction para modo in-memory

Fase 15 — MongoDB + DoltDB + Arrow  (semana 25-26)
  ✓ Change streams CDC basado en WAL
  ✓ Git para datos: commits, branches, checkout, merge, diff
  ✓ Apache Arrow como formato de salida para queries analíticas
  ✓ Benchmarks finales completos vs MySQL, SQLite, DuckDB
  ✓ [MOBILE] WATCH queries para modo embebido: WATCH SELECT ... notifica cambios vía canal Tokio
  ✓ [MOBILE] LiveQueryManager: WalSubscriber que filtra eventos por tabla/predicado y emite a suscriptores
  ✓ [MOBILE] API Rust: watch_query(sql) -> tokio::sync::broadcast::Receiver<QueryResult>
  ✓ [MOBILE] Dart binding (flutter_rust_bridge): Stream<QueryResult> desde WATCH — reactive UI sin polling
  ✓ Database branching (Neon/PlanetScale-style): axiomdb branch create <nombre> --from <base>
  ✓ Branches usan Copy-on-Write sobre el B+ Tree — sin duplicar datos, solo las páginas que divergen
  ✓ axiomdb branch list / merge / delete / diff — gestión completa desde CLI
  ✓ Casos de uso: probar migraciones sin riesgo, review apps por PR, entornos de CI/CD aislados
  ✓ Kafka sink nativo: CREATE KAFKA SINK ... FROM TABLE ... BROKER ... TOPIC — sin Debezium externo
  ✓ Kafka source: CREATE KAFKA SOURCE ... — consumir topics de Kafka como tablas de AxiomDB
  ✓ Formatos Kafka: JSON, Avro (con Schema Registry), Protobuf
  ✓ Kafka exactly-once semantics usando transacciones + idempotent producer

Fase 16 — Lógica del servidor       (semana 27-29)
  ✓ SQL UDFs escalares y de tabla (CREATE FUNCTION ... AS $$ ... $$)
  ✓ Triggers BEFORE/AFTER con WHEN condicional y SIGNAL para errores
  ✓ Lua runtime (mlua): EVAL con query() y execute() atómicos
  ✓ WASM runtime (wasmtime): CREATE FUNCTION LANGUAGE wasm FROM FILE
  ✓ Sandbox WASM: límites de memoria, timeout, sin acceso externo
  ✓ Tests: plugin de riesgo crediticio en Rust compilado a WASM
  ✓ CREATE PROCEDURE con lenguaje procedural nativo PL/pgSQL: BEGIN/END, DECLARE, IF/ELSIF/LOOP
  ✓ RAISE EXCEPTION / RAISE NOTICE con SQLSTATE personalizado dentro de procedures
  ✓ CREATE EXCEPTION nombre 'mensaje default' — excepción nombrada como objeto del schema (estilo Firebird)
  ✓ ALTER EXCEPTION nombre 'nuevo mensaje' — actualizar mensaje default sin tocar procedures
  ✓ DROP EXCEPTION nombre — eliminar excepción del schema
  ✓ Lanzar por nombre desde procedures/triggers: EXCEPTION nombre; o EXCEPTION nombre 'override msg';
  ✓ GRANT / REVOKE USAGE ON EXCEPTION nombre TO rol — control de acceso por excepción
  ✓ Parámetros IN / OUT / INOUT y RETURNS TABLE en procedures
  ✓ CALL statement + transacciones explícitas dentro de procedures (COMMIT/ROLLBACK internos)
  ✓ Variables de cursor dentro de procedures: OPEN / FETCH / CLOSE
  ✓ Migración transparente: procedures PostgreSQL existentes ejecutan sin modificaciones
  ✓ Webhooks HTTP nativos: CREATE WEBHOOK ... ON TABLE ... AFTER INSERT/UPDATE/DELETE URL ...
  ✓ Webhooks con filtro: FILTER (NEW.total > 1000) — solo dispara si se cumple la condición
  ✓ Headers configurables: Authorization, Content-Type, custom headers por webhook
  ✓ Retry automático con backoff exponencial: 3 intentos, espera 1s/5s/30s ante fallo HTTP
  ✓ Webhook log: tabla interna con historial de entregas, status HTTP, payload, errores
  ✓ Webhook signatures: HMAC-SHA256 del payload para verificación en el receptor
  ✓ Async delivery: los webhooks no bloquean la transacción — se encolan en Tokio task queue

Fase 17 — Seguridad                 (semana 30-31)
  ✓ CREATE USER / CREATE ROLE / GRANT / REVOKE
  ✓ Permisos por tabla y por columna
  ✓ Row-Level Security con políticas por tabla
  ✓ Autenticación Argon2id + Scram-SHA-256 en wire protocol
  ✓ TLS 1.3 para todas las conexiones (tokio-rustls)
  ✓ Statement timeout por usuario/sesión/global
  ✓ JWT authentication: SET axiomdb.jwt_secret — tokens firmados con HS256/RS256/ES256
  ✓ JWT claims mapeados a variables de sesión: current_setting('axiomdb.user_id') en RLS policies
  ✓ OAuth2 / OIDC: integración con Google, GitHub, Okta, Auth0, Azure AD como identity providers
  ✓ OIDC discovery automático: configura provider con solo la URL del issuer
  ✓ IP allowlist: allowed_ips = ["10.0.0.0/8"] en axiomdb.toml — rechaza conexiones fuera del rango
  ✓ IP blocklist: blocked_ips = ["1.2.3.4"] — bloquear IPs específicas o rangos CIDR
  ✓ SQL injection detection: análisis de patrones conocidos (UNION-based, blind, stacked queries)
  ✓ SQL injection action: log / block / alert configurable por severidad
  ✓ HashiCorp Vault integration: TLS certs, passwords y encryption keys leídos de Vault en runtime
  ✓ Vault dynamic secrets: credenciales de corta vida rotadas automáticamente sin reiniciar el servidor
  ✓ AWS Secrets Manager / GCP Secret Manager como alternativas a Vault

Fase 18 — Alta disponibilidad       (semana 32-33)
  ✓ Streaming replication (primary → replicas vía WAL)
  ✓ Réplicas sincrónicas y asincrónicas configurables
  ✓ Point-in-Time Recovery (PITR) usando WAL acumulado
  ✓ Hot backup sin lockear la BD
  ✓ Dump SQL portable (SOURCE / DUMP)
  ✓ Backup automático a S3/GCS/Azure Blob: schedule cron + destination URL + retention policy
  ✓ Backup encryption en tránsito y en destino: AES-256 con clave separada de la del storage
  ✓ Backup verification: restore automático en DB temporal para verificar integridad del backup
  ✓ axiomdb backup restore --from s3://... --to-time "2025-01-15 14:30:00" (PITR desde S3)
  ✓ Database cloning: axiomdb clone <origen> <destino> — copia instantánea via Copy-on-Write
  ✓ Clone diferencial: solo almacena páginas que difieren del origen — sin duplicar datos
  ✓ format_version + min_compatible en página 0 del .db
  ✓ Política de compatibilidad N-1: versión nueva siempre lee archivos creados por la versión anterior
  ✓ axiomdb upgrade --data-dir: backup automático → migración → verificación → rollback si falla
  ✓ Blue-green upgrade sin downtime: nueva versión arranca en puerto alterno, 0 segundos de downtime
  ✓ axiomdb downgrade --check / --to-version X
  ✓ axiomdb dump --compatible-with X: exporta omitiendo features incompatibles con versión destino

Fase 19 — Mantenimiento + observabilidad  (semana 34-35)
  ✓ Auto-vacuum en Tokio background task
  ✓ VACUUM / VACUUM ANALYZE / VACUUM FULL / VACUUM CONCURRENTLY
  ✓ Deadlock detection con grafo de espera (DFS, cada 100ms)
  ✓ pg_stat_statements: calls, tiempo total, cache hits
  ✓ Slow query log en JSON con plan de ejecución
  ✓ Connection pooling integrado (Semaphore + idle pool)
  ✓ OpenTelemetry (OTEL): traces de queries con span por etapa (parse, plan, execute, commit)
  ✓ OTEL exporters: OTLP/gRPC y OTLP/HTTP — compatible con Jaeger, Zipkin, Datadog, Honeycomb
  ✓ Prometheus endpoint: GET /metrics
  ✓ Grafana dashboard prebuilt: JSON exportable, listo para importar
  ✓ Alertas automáticas: disk >90%, replication_lag >30s, connections >90%, slow query, cache_hit <80%
  ✓ Health check endpoint: GET /health — JSON con estado de cada subsistema

Fase 20 — Tipos + importación/exportación  (semana 36-37)
  ✓ Views regulares (CREATE VIEW) y actualizables
  ✓ Sequences (CREATE SEQUENCE, NEXTVAL, CURRVAL)
  ✓ ENUMs (CREATE TYPE ... AS ENUM)
  ✓ Arrays (TEXT[], FLOAT[], ANY(), @>)
  ✓ COPY FROM/TO: CSV, JSON, JSONL, Parquet
  ✓ READ_PARQUET() como función de tabla directa
  ✓ Backup incremental y restore completo
  ✓ CREATE RECURSIVE VIEW — sintaxis SQL:1999 para vistas sobre CTEs recursivos
  ✓ WITH CHECK OPTION / WITH CASCADED CHECK OPTION

Fase 21 — SQL avanzado              (semana 38-39)
  ✓ Savepoints (SAVEPOINT / ROLLBACK TO / RELEASE)
  ✓ CTEs (WITH) y CTEs recursivos (WITH RECURSIVE)
  ✓ RETURNING en INSERT / UPDATE / DELETE
  ✓ MERGE / UPSERT (INSERT ON CONFLICT + MERGE estándar)
  ✓ CHECK constraints y DOMAIN types
  ✓ Tablas temporales (TEMP) y sin log (UNLOGGED)
  ✓ Expression indexes (índices sobre LOWER(), YEAR(), etc.)
  ✓ LATERAL joins
  ✓ Cursores (DECLARE / FETCH / CLOSE)
  ✓ Query hints (/*+ INDEX() HASH_JOIN() PARALLEL() */)
  ✓ [MOBILE] Soft deletes built-in: CREATE TABLE ... WITH (soft_delete = true)
  ✓ Temporal tables (SQL:2011): PERIOD FOR SYSTEM_TIME, GENERATED ALWAYS AS ROW START/END
  ✓ WITH (SYSTEM_VERSIONING = ON) — trackea valid_from/valid_to automáticamente
  ✓ FOR SYSTEM_TIME AS OF <timestamp> — viaje en el tiempo a cualquier momento
  ✓ Bitemporal: application-time + system-time en la misma tabla

Fase 22 — Features de producto      (semana 40-42)
  ✓ Vector similarity search: tipo VECTOR(n), índice HNSW, operador <=>
  ✓ Búsqueda fuzzy: SIMILARITY(), trigramas GIN index
  ✓ Scheduled jobs: cron_schedule() con expresiones cron
  ✓ Foreign Data Wrappers: CREATE FOREIGN TABLE ... SERVER
  ✓ Multi-database: CREATE DATABASE, USE, cross-database queries
  ✓ Schema namespacing: CREATE SCHEMA, schema.tabla
  ✓ Schema migrations CLI: axiomdb migrate up/down/status
  ✓ GraphQL API nativa — puerto :3308, schema autodescubierto, queries/mutations/subscriptions
  ✓ GraphQL subscriptions vía WAL stream — WebSocket, eventos en tiempo real sin polling
  ✓ GraphQL DataLoader integrado — batch loading automático, cero N+1
  ✓ GraphQL introspection — compatible con Apollo Studio, Postman, codegen
  ✓ OData v4 nativo — puerto :3309, PowerBI/Excel/Tableau/SAP sin drivers ni ODBC
  ✓ OData $metadata — EDMX autodescubierto desde catálogo
  ✓ OData $filter/$select/$orderby/$top/$skip/$count/$expand/$batch
  ✓ OData interno: parser URL → AST OData → SqlStatement → executor existente
  ✓ Auto REST API (axiomdb-rest, PostgREST-style): puerto :3310, schema autodescubierto
  ✓ REST CRUD automático: GET/POST/PATCH/DELETE /rest/<tabla>
  ✓ REST filtros, joins por FK, RLS via JWT, bulk operations, upsert
  ✓ OpenAPI spec autogenerada: GET /rest/ → swagger.json

Fase 38 — AxiomDB-Wasm: Browser Database Engine  (semana 122-130)
  ✓ Compilar crates core a wasm32-unknown-unknown (parser, executor, B+ Tree, buffer pool, MVCC)
  ✓ Feature-gate código OS-dependiente (#[cfg(not(target_arch = "wasm32"))])
  ✓ Allocator Wasm (wee_alloc/dlmalloc) — binario ≤200KB gzipped
  ✓ OpfsStorageEngine — implementa StorageEngine trait sobre Origin Private File System (OPFS)
  ✓ WAL sobre OPFS — mismo formato, crash recovery al recargar página
  ✓ Fallback a IndexedDB para browsers sin OPFS sync access
  ✓ wasm-bindgen API: AxiomDB.open(), .execute(), .query()
  ✓ Web Worker wrapper — todas las operaciones off-main-thread vía postMessage
  ✓ TypeScript definitions (.d.ts) con generics para resultados
  ✓ npm package @axiomdb/browser — ESM + CJS, zero dependencies
  ✓ Live queries: db.watch(sql, callback) — re-ejecuta cuando cambian tablas afectadas
  ✓ React / Vue / Svelte hooks/composables/stores para reactive UI
  ✓ Multi-tab: SharedWorker/BroadcastChannel — single writer, sin conflictos de lock OPFS
  ✓ Sync engine offline-first: CRDT (LWW por columna con HLC), delta sync vía WebSocket
  ✓ Encryption at rest: AES-256-GCM por página, clave nunca toca disco
  ✓ DevTools extension: inspeccionar tablas, queries, WAL, sync status
  ✓ Tests en Playwright: Chrome, Firefox, Safari con OPFS real
  ✓ Benchmarks: latency vs native (≤3×), INSERT ≥50K/s, cold start <100ms
```

---

## Future Phases — Detailed Specs

### Phase 23 — Advanced Analytics
- Columnar storage format (PAX layout within 8 KB pages)
- Vectorized aggregation: SUM/MIN/MAX/COUNT in SIMD batches
- Zone maps per column block for min/max predicate skipping
- Adaptive column compression: Delta/BitPack/RLE chosen per data distribution
- `ANALYZE TABLE` updates column statistics (null_frac, cardinality, histogram)
- Cost-based planner uses statistics to choose index vs seq scan

### Phase 24 — Geospatial
- `POINT`, `LINESTRING`, `POLYGON`, `GEOMETRY` types
- R-Tree index for bounding-box queries
- `ST_Within`, `ST_Intersects`, `ST_Distance`, `ST_Area`, `ST_Union` functions
- PostGIS-compatible SQL surface for ORMs that target PostGIS

### Phase 25 — Full JIT (LLVM)
- Compile hot query plans to native code via LLVM IR
- Specialize for constant predicates and known column types at compile time
- `SET jit = on/off` session variable
- Inkwell crate (`inkwell = "0.4"`) — requires LLVM installed
- Threshold: JIT only when estimated rows > 100K (overhead not worth it for small queries)

### Phase 26 — Extensions System
- `CREATE EXTENSION name VERSION '1.0'` with manifest.toml
- Extension registry: validate semver, check dependencies, apply upgrade scripts
- Extension hooks: `PreInsert`, `PostCommit`, `OnQuery`, `OnError`
- WASM extensions: sandboxed, memory-limited, timeout-enforced
- Lua extensions: mlua runtime, can call `query()` / `execute()`

### Phase 27 — Distributed (Sharding)
- Horizontal sharding: hash-based or range-based partition routing
- Shard coordinator: receives query, routes to correct shard, merges results
- Cross-shard joins: broadcast small tables, partition-wise for large tables
- Distributed transactions: 2PC coordinator between shards
- gRPC between nodes (tonic crate)

### Phase 28 — Advanced Security
- Data masking: `MASKED WITH (FUNCTION = partial(email, 3, '***', 4))` per column
- Differential privacy: add calibrated noise to aggregate results
- Encryption at rest: AES-256-GCM per page, key in separate key store
- Column-level encryption: `ENCRYPTED WITH KEY key_name`
- Audit log: all DDL + DML with user, timestamp, before/after values
- Data tokenization: `TOKENIZE(card_number)` → reversible token via Vault Transit

### Phase 29 — AI-Native Features
- `VECTOR(n)` column type with HNSW index (already Phase 22)
- `EMBEDDING(model, text)` function: calls ONNX model inline, returns VECTOR
- Semantic search: `ORDER BY embedding <=> query_embedding LIMIT 10`
- `AI_CLASSIFY(text, labels[])` function: zero-shot classification
- `AI_SUMMARIZE(text)` function: calls local ONNX model
- pgvector-compatible SQL surface for existing ORMs
- f16 quantization for vector columns: 4× less storage, <1% accuracy loss

### Phase 30 — Compatibility Layer
- PostgreSQL wire protocol on port 5432 (in addition to MySQL on 3306)
- `pg_dump` / `pg_restore` compatible format
- `axiomdb migrate --from-mysql <dsn>`: reads INFORMATION_SCHEMA, translates types, copies data
- `axiomdb migrate --from-postgres <dsn>`: pg_dump format import
- ActiveRecord adapter (Ruby gem), SQLAlchemy dialect, Prisma adapter
- ODBC driver (odbc-api crate as cdylib)

### Phase 31 — Temporal + Bitemporal
- Full SQL:2011 temporal tables (already planned in Phase 21)
- Bitemporal queries combining application time + system time
- Historical data stored in compressed separate pages (LZ4)
- `AS OF SYSTEM TIME` queries for audit and compliance
- Time-travel API: `db.as_of(timestamp).query(sql)`

### Phase 32 — Graph Queries
- Property graph model: `(node)-[edge]->(node)` stored in regular tables + index
- SQL/PGQ syntax: `MATCH (n:Person)-[:KNOWS]->(m:Person) WHERE n.name = 'Alice'`
- BFS / DFS traversal algorithms as built-in functions
- Shortest path: `shortest_path(from, to, edge_table)`
- PageRank / connected components as analytical functions

### Phase 33 — Mobile Sync (axiomdb-sync)
- HLC (Hybrid Logical Clock) for distributed ordering without global lock
- Delta sync: send only changed pages since last sync timestamp
- CRDT types: `LWWRegister<T>`, `GCounter`, `PNCounter`, `ORSet<T>`
- Conflict resolution strategies: LWW, server-wins, client-wins, custom merge function
- Offline queue with ordered replay on reconnect
- WebSocket sync protocol: server pushes delta frames, client applies to local OPFS DB

### Phase 34 — Studio + Developer Tools
- `axiomdb-studio`: web UI served on port 3311 (opt-in with `--studio` flag)
  - Table browser, SQL editor with autocomplete, schema designer
  - Query explain visualizer, slow query dashboard, WAL inspector
- VS Code extension: syntax highlighting, autocomplete, explain in-editor
- `axiomdb-wizard`: TUI configuration wizard (ratatui + dialoguer)
  - `--non-interactive` mode for CI/CD

### Phase 35 — Deployment Packaging
- Docker: multi-stage build, final image ~15MB (musl static binary)
- systemd service unit with sandboxing (PrivateTmp, NoNewPrivileges)
- `.deb` (Ubuntu/Debian) and `.rpm` (RHEL/Fedora) packages
- macOS `.dmg` + LaunchAgent for auto-start
- Windows MSI + WiX installer, installs as Windows Service
- AppImage for Linux (single file, no install required)
- `axiomdb-odbc`: cdylib ODBC driver for Excel, R, Tableau, SAP
- `axiomdb-activerecord` gem (Rails), `axiomdb-python` pip package
- `axiomdb-client` Rust SDK: connection pool, typed results, transactions

### Phase 36 — Replication + HA (advanced)
- Semi-synchronous replication: wait for N replicas to confirm WAL before commit
- Automatic failover: Raft consensus for leader election among replicas
- Logical replication: row-level changes streamed to downstream systems
- Bidirectional replication (multi-master, limited conflict resolution)

### Phase 37 — Graph Storage Engine (axiomdb-graph)
- Native graph storage layer over existing B+ Tree + page format
- Vertex and edge pages with adjacency index
- Traversal API: DFS/BFS with depth limit and edge filtering
- Cypher-compatible query surface (subset)

### Phase 39 — Clustered Storage (InnoDB-style)
  ✓ CLOSED — Clustered B+ Tree: PK inline in leaf, secondary key = secondary_cols ++ PK_bookmark
  ✓ Variable-size rows (split by byte volume, not key count)
  ✓ Clustered overflow (key + header inline, tail spills)
  ✓ Clustered secondary bookmarks (no heap RecordId dependency)
  ✓ Clustered WAL (key = PK bytes, payload = exact row image)
  ✓ Clustered crash recovery (committed root map + active timeline replay)
  ✓ Clustered SQL: INSERT / SELECT / UPDATE / DELETE / VACUUM / REBUILD
  ✓ heap→clustered migration via ALTER TABLE ... REBUILD
  ✓ Zero-alloc scan_all_callback for GROUP BY and aggregation
  Open gaps: MVCC version chains, root checkpoint persistence, FK clustered children, separator overflow (39.19b)

### Phase 40 — Buffer Pool + Write Pipeline
  ✓ CLOSED — Buffer pool manager with LRU shards, pin counts, eviction
  ✓ Prefetch hints on sequential access (Phase 11.9)
  ✓ Write combining: coalesce adjacent dirty pages (Phase 11.10)
  ✓ fsync pipeline: leader-based coalescing

```

---

## Benchmark Targets

Conditions: 1M records, 16 concurrent threads, NVMe SSD

| Operation | AxiomDB target | MySQL 8.0 | Speedup goal |
|---|---|---|---|
| Point lookup (PK) | 800K ops/s | ~350K ops/s | ~2.3× |
| Range scan 10K rows | 45ms | ~120ms | ~2.7× |
| Seq scan 1M rows | 0.8s | ~3.4s | ~4.2× |
| INSERT with WAL | 180K ops/s | ~95K ops/s | ~1.9× |
| Concurrent reads ×16 | linear scaling | saturates | ~3×+ |

**Advantages**: concurrent reads (CoW B+ Tree, no locks), table scans (SIMD + morsel), high-selectivity queries (late materialization), simple pipelines (operator fusion), point lookups when tree fits in CPU cache.

**Where MySQL is competitive**: complex query optimizer (years of work), highly varied mixed workloads.

---

## SQLSTATE — Error Code Mapping

AxiomDB maps `DbError` variants to standard SQL states for ORM compatibility:

| SqlState | Code | Scenario |
|---|---|---|
| `SuccessfulCompletion` | 00000 | No error |
| `NoData` | 02000 | Cursor / fetch with no rows |
| `ConnectionException` | 08000 | Network / socket errors |
| `SyntaxError` | 42601 | Parser rejects SQL |
| `UndefinedTable` | 42P01 | Table not found |
| `UndefinedColumn` | 42703 | Column not found |
| `UniqueViolation` | 23505 | UNIQUE / PK constraint |
| `ForeignKeyViolation` | 23503 | FK constraint |
| `NotNullViolation` | 23502 | NOT NULL constraint |
| `CheckViolation` | 23514 | CHECK constraint |
| `InvalidDatetimeFormat` | 22007 | Bad date/time literal |
| `DivisionByZero` | 22012 | Division by zero |
| `SerializationFailure` | 40001 | MVCC conflict, retry |
| `DeadlockDetected` | 40P01 | Deadlock |
| `InsufficientPrivilege` | 42501 | Permission denied |
| `DiskFull` | 53100 | No space left on device |

---

## References

- PostgreSQL source: `research/postgres/` — TOAST, vacuum, B-tree WAL, covering indexes
- MariaDB/InnoDB: `research/mariadb-server/` — clustered storage, FK enforcement, undo
- SQLite: `research/sqlite/` — B-tree cursor, overflow chain, without-rowid
- DuckDB: `research/duckdb/` — morsel parallelism, operator fusion, late materialization
- `memory/architecture.md` — implementation decisions per subphase
- `memory/lessons.md` — lessons learned during development
- `specs/fase-N/` — detailed spec and plan per subphase
