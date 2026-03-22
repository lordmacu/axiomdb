# Progreso — dbyo Motor de Base de Datos

> Actualizado automáticamente con `/subfase-completa`
> Leyenda: ✅ completada | 🔄 en progreso | ⏳ pendiente | ⏸ bloqueada

---

## BLOQUE 1 — Fundamentos del Motor (Fases 1-7)

### Fase 1 — Storage básico `✅` semana 1-2
- [x] 1.1 ✅ Workspace setup — Cargo.toml, estructura de carpetas, CI básico
- [x] 1.2 ✅ Formato de página — `struct Page`, `PageType`, CRC32c checksum, align(64)
- [x] 1.3 ✅ MmapStorage — abrir/crear `.db`, `read_page`, `write_page` con mmap
- [x] 1.4 ✅ MemoryStorage — implementación en RAM para tests (sin I/O)
- [x] 1.5 ✅ Free list — `alloc_page`, `free_page`, bitmap de páginas libres
- [x] 1.6 ✅ Trait StorageEngine — unificar Mmap y Memory con trait intercambiable
- [x] 1.7 ✅ Tests + benchmarks — unit, integration, bench de read/write de páginas

### Fase 2 — B+ Tree `⏳` semana 3-4
- [ ] 2.1 ⏳ Estructuras de nodo — `BTreeNode`, hojas e internos, linked list de hojas
- [ ] 2.2 ⏳ Lookup por key exacto — búsqueda O(log n) desde root hasta hoja
- [ ] 2.3 ⏳ Insert con split — split de hoja y propagación al nodo interno
- [ ] 2.4 ⏳ Range scan — recorrer linked list de hojas para rangos
- [ ] 2.5 ⏳ Delete con merge — merge y redistribución de nodos
- [ ] 2.6 ⏳ Copy-on-Write — raíz atómica con AtomicU64 + CAS, readers sin locks
- [ ] 2.7 ⏳ Prefix compression — comprimir keys con prefijo común en nodos internos
- [ ] 2.8 ⏳ Tests + benchmarks — correctness, concurrencia, benchmark vs BTreeMap

### Fase 3 — WAL y transacciones `⏳` semana 5
- [ ] 3.1 ⏳ Formato WAL entry — `[LSN|Type|Table|Key|Old|New|CRC]`
- [ ] 3.2 ⏳ WalWriter — append-only, fsync configurable
- [ ] 3.3 ⏳ WalReader — leer desde LSN específico, validar CRC
- [ ] 3.4 ⏳ BEGIN / COMMIT / ROLLBACK básico
- [ ] 3.5 ⏳ Crash recovery — replay del WAL al abrir la BD
- [ ] 3.6 ⏳ Tests de durabilidad — escribir → simular crash → releer → verificar

### Fase 4 — SQL Parser + Executor `⏳` semana 6-7
- [ ] 4.1 ⏳ Lexer/Tokenizer — tokens SQL con `nom`
- [ ] 4.2 ⏳ Parser DDL — `CREATE TABLE`, `CREATE INDEX`, `DROP TABLE`
- [ ] 4.3 ⏳ Parser DML — `SELECT`, `INSERT`, `UPDATE`, `DELETE`
- [ ] 4.4 ⏳ AST definitions — tipos del árbol sintáctico
- [ ] 4.5 ⏳ Executor básico — conectar AST con storage + B+ Tree
- [ ] 4.6 ⏳ INSERT ... SELECT — insertar resultado de query directamente
- [ ] 4.7 ⏳ SQLSTATE codes — códigos de error estándar SQL (23505, 42P01, etc.)
- [ ] 4.8 ⏳ version() / current_user / session_user / current_database() — ORMs llaman esto al conectar
- [ ] 4.9 ⏳ LAST_INSERT_ID() / lastval() — obtener último ID auto-generado (MySQL + PG compat)
- [ ] 4.8 ⏳ CLI interactiva — REPL tipo `sqlite3` shell
- [ ] 4.9 ⏳ Tests SQL — suite de queries DDL + DML básicas

### Fase 5 — MySQL Wire Protocol `⏳` semana 8
- [ ] 5.1 ⏳ TCP listener con Tokio — aceptar conexiones en :3306
- [ ] 5.2 ⏳ MySQL handshake — Server Greeting + Client Response
- [ ] 5.3 ⏳ Autenticación — `mysql_native_password` básico
- [ ] 5.4 ⏳ COM_QUERY handler — recibir SQL, ejecutar, responder
- [ ] 5.5 ⏳ Result set serialization — columns + rows en wire protocol
- [ ] 5.6 ⏳ Error packets — serializar `DbError` como MySQL error
- [ ] 5.7 ⏳ Test con cliente real — PHP PDO o Python PyMySQL conecta y hace query

### Fase 6 — Índices secundarios + FK `⏳` semana 9
- [ ] 6.1 ⏳ Múltiples B+ Trees por tabla — un árbol por índice
- [ ] 6.2 ⏳ CREATE INDEX — crear árbol y popularlo desde datos existentes
- [ ] 6.3 ⏳ Query planner básico — elegir índice vs full scan con estadísticas simples
- [ ] 6.4 ⏳ Bloom filter por índice — evitar I/O para keys inexistentes
- [ ] 6.5 ⏳ Foreign key checker — validación en INSERT/UPDATE con índice inverso
- [ ] 6.6 ⏳ ON DELETE CASCADE / RESTRICT / SET NULL
- [ ] 6.7 ⏳ Partial UNIQUE index — `UNIQUE WHERE condition` para soft delete
- [ ] 6.8 ⏳ Fill factor — `WITH (fillfactor=70)` para tablas con muchos inserts
- [ ] 6.9 ⏳ Tests de FK e índices — violaciones, cascadas, restricciones

### Fase 7 — Concurrencia + MVCC `⏳` semana 10
- [ ] 7.1 ⏳ RowHeader — `txn_id_created`, `txn_id_deleted`, `row_version`
- [ ] 7.2 ⏳ Transaction manager — contador atómico de txn_id
- [ ] 7.3 ⏳ Snapshot isolation — reglas de visibilidad por snapshot_id
- [ ] 7.4 ⏳ Readers lockless con CoW — verificar que reads no bloquean writes
- [ ] 7.5 ⏳ Writer serialization — solo 1 writer a la vez por tabla (luego mejorar)
- [ ] 7.6 ⏳ ROLLBACK — marcar filas del txn como deleted
- [ ] 7.7 ⏳ Tests de concurrencia — N readers + N writers simultáneos

---

## BLOQUE 2 — Optimizaciones de Ejecución (Fases 8-10)

### Fase 8 — Optimizaciones SIMD `⏳` semana 11-12
- [ ] 8.1 ⏳ Vectorized filter — evaluar predicados en chunks de 1024 filas
- [ ] 8.2 ⏳ SIMD AVX2 con `wide` — comparar 8-32 valores por instrucción
- [ ] 8.3 ⏳ Query planner mejorado — selectividad, índice vs scan con stats
- [ ] 8.4 ⏳ EXPLAIN básico — mostrar plan elegido
- [ ] 8.5 ⏳ Benchmarks SIMD vs MySQL — point lookup, range scan, seq scan

### Fase 9 — DuckDB-inspired `⏳` semana 13-14
- [ ] 9.1 ⏳ Morsel-driven parallelism — dividir en chunks de 100K, Rayon
- [ ] 9.2 ⏳ Operator fusion — scan+filter+project en un solo loop lazy
- [ ] 9.3 ⏳ Late materialization — predicados baratos primero, leer cols caras al final
- [ ] 9.4 ⏳ Benchmarks con paralelismo — medir scaling con N cores

### Fase 10 — Modo embebido + FFI `⏳` semana 15-16
- [ ] 10.1 ⏳ Refactor motor como `lib.rs` reutilizable
- [ ] 10.2 ⏳ C FFI — `dbyo_open`, `dbyo_execute`, `dbyo_close` con `#[no_mangle]`
- [ ] 10.3 ⏳ Compilar como `cdylib` — `.so` / `.dll` / `.dylib`
- [ ] 10.4 ⏳ Binding Python — `ctypes` demo funcionando
- [ ] 10.5 ⏳ Test embebido — misma BD usada desde servidor y desde librería

---

## BLOQUE 3 — Features Avanzadas (Fases 11-15)

### Fase 11 — Robustez e índices `⏳` semana 17-18
- [ ] 11.1 ⏳ Sparse index — una entrada cada N filas para timestamps
- [ ] 11.2 ⏳ TOAST — valores >2KB a páginas de overflow con LZ4
- [ ] 11.3 ⏳ In-memory mode — `open(":memory:")` sin disco
- [ ] 11.4 ⏳ JSON nativo — tipo JSON, `->>`  extracción con jsonpath
- [ ] 11.4b ⏳ JSONB_SET — actualizar campo JSON sin reescribir el documento completo
- [ ] 11.4c ⏳ JSONB_DELETE_PATH — eliminar campo específico de JSONB
- [ ] 11.5 ⏳ Partial indexes — `CREATE INDEX ... WHERE condition`
- [ ] 11.6 ⏳ FTS básico — tokenizer + índice invertido + BM25 ranking
- [ ] 11.7 ⏳ FTS avanzado — frases, booleanos, prefijos, stop words en español

### Fase 12 — Testing + JIT `⏳` semana 19-20
- [ ] 12.1 ⏳ Deterministic simulation testing — `FaultInjector` con semilla
- [ ] 12.2 ⏳ EXPLAIN ANALYZE — tiempos reales por nodo del plan
- [ ] 12.3 ⏳ JIT básico con LLVM — compilar predicados simples a código nativo
- [ ] 12.4 ⏳ Benchmarks finales bloque 1 — comparar con MySQL y SQLite

### Fase 13 — PostgreSQL avanzado `⏳` semana 21-22
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — pub-sub nativo con `DashMap` de channels
- [ ] 13.5 ⏳ Covering indexes — `INCLUDE (col1, col2)` en hojas del B+ Tree
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + swap atómico

### Fase 14 — TimescaleDB + Redis inspired `⏳` semana 23-24
- [ ] 14.1 ⏳ Table partitioning — `PARTITION BY RANGE/HASH/LIST`
- [ ] 14.2 ⏳ Partition pruning — query planner evita particiones no relevantes
- [ ] 14.3 ⏳ Compresión automática de particiones históricas — LZ4 columnar
- [ ] 14.4 ⏳ Continuous aggregates — refresh incremental solo del delta nuevo
- [ ] 14.5 ⏳ TTL por fila — `WITH TTL 3600` + background reaper en Tokio
- [ ] 14.6 ⏳ LRU eviction — para modo in-memory con límite de RAM

### Fase 15 — MongoDB + DoltDB + Arrow `⏳` semana 25-26
- [ ] 15.1 ⏳ Change streams CDC — taildel WAL, emitir eventos Insert/Update/Delete
- [ ] 15.2 ⏳ Git para datos — commits, branches, checkout con snapshot de roots
- [ ] 15.3 ⏳ Git merge — merge de branches con detección de conflictos
- [ ] 15.4 ⏳ Apache Arrow output — resultados en formato columnar para Python/pandas

---

## BLOQUE 4 — Lógica y Seguridad (Fases 16-17)

### Fase 16 — Lógica del servidor `⏳` semana 27-29
- [ ] 16.1 ⏳ SQL UDFs escalares — `CREATE FUNCTION ... AS $$ ... $$`
- [ ] 16.2 ⏳ SQL UDFs de tabla — retornan múltiples filas
- [ ] 16.3 ⏳ Triggers BEFORE/AFTER — con condición `WHEN` y `SIGNAL`
- [ ] 16.3b ⏳ INSTEAD OF triggers — lógica de INSERT/UPDATE/DELETE sobre vistas
- [ ] 16.4 ⏳ Lua runtime — `mlua`, EVAL con `query()` y `execute()` atómicos
- [ ] 16.5 ⏳ WASM runtime — `wasmtime`, sandbox, límites de memoria y timeout
- [ ] 16.6 ⏳ CREATE FUNCTION LANGUAGE wasm FROM FILE — cargar plugin .wasm

### Fase 17 — Seguridad `⏳` semana 30-31
- [ ] 17.1 ⏳ CREATE USER / CREATE ROLE — modelo de usuarios y roles
- [ ] 17.2 ⏳ GRANT / REVOKE — permisos por tabla y por columna
- [ ] 17.3 ⏳ Row-Level Security — políticas con `USING` expr aplicadas automáticamente
- [ ] 17.4 ⏳ Argon2id — hashing de passwords + Scram-SHA-256 en handshake
- [ ] 17.5 ⏳ TLS 1.3 — conexiones cifradas con `tokio-rustls`
- [ ] 17.6 ⏳ Statement timeout — por usuario, sesión y global
- [ ] 17.7 ⏳ Audit trail — `CREATE AUDIT POLICY` con log automático
- [ ] 17.8 ⏳ Account lockout — rastreo de intentos fallidos + bloqueo automático
- [ ] 17.9 ⏳ Password policy — longitud mínima, complejidad, expiración, historial
- [ ] 17.10 ⏳ IP allowlist por usuario — pg_hba.conf con reglas por IP/CIDR
- [ ] 17.11 ⏳ Connection rate limiting — max conexiones por segundo por usuario/IP
- [ ] 17.12 ⏳ Log levels y rotación — trace/debug/info/warn/error + rotación diaria

---

## BLOQUE 5 — Alta Disponibilidad (Fases 18-19)

### Fase 18 — Alta disponibilidad `⏳` semana 32-33
- [ ] 18.1 ⏳ Streaming replication — enviar WAL en tiempo real al replica
- [ ] 18.2 ⏳ Replica apply — recibir y aplicar WAL entries
- [ ] 18.3 ⏳ Synchronous commit configurable — off, local, remote_write, remote_apply
- [ ] 18.4 ⏳ Cascading replication — replica retransmite a sub-replicas
- [ ] 18.5 ⏳ Hot standby — reads desde réplica mientras aplica WAL
- [ ] 18.6 ⏳ PITR — restaurar al segundo exacto usando WAL archivado
- [ ] 18.7 ⏳ Hot backup — `BACKUP DATABASE` sin lockear

### Fase 19 — Mantenimiento + observabilidad `⏳` semana 34-35
- [ ] 19.1 ⏳ Auto-vacuum — background task en Tokio, umbral configurable por tabla
- [ ] 19.2 ⏳ VACUUM CONCURRENTLY — compactar sin bloquear reads ni writes
- [ ] 19.3 ⏳ Deadlock detection — DFS en grafo de espera cada 100ms
- [ ] 19.4 ⏳ pg_stat_statements — fingerprint + calls + tiempo + cache hits
- [ ] 19.5 ⏳ Slow query log — JSON con plan de ejecución
- [ ] 19.6 ⏳ Connection pooling — Semaphore + idle pool integrado
- [ ] 19.7 ⏳ pg_stat_activity — ver y cancelar queries en ejecución
- [ ] 19.8 ⏳ pg_stat_progress_vacuum — progreso de vacuum en tiempo real
- [ ] 19.9 ⏳ lock_timeout — error si espera un lock más de N ms
- [ ] 19.10 ⏳ deadlock_timeout — cuánto esperar antes de ejecutar detector de deadlock
- [ ] 19.11 ⏳ idle_in_transaction_session_timeout — matar transacciones abandonadas
- [ ] 19.12 ⏳ pg_stat_user_tables — seq_scan, idx_scan, n_live_tup, n_dead_tup por tabla
- [ ] 19.13 ⏳ pg_stat_user_indexes — idx_scan, idx_tup_read por índice
- [ ] 19.14 ⏳ Table/index bloat detection — ratio dead_tup/live_tup con umbral de alerta
- [ ] 19.15 ⏳ REINDEX TABLE / INDEX / DATABASE — reconstruir índices corruptos o hinchados
- [ ] 19.16 ⏳ REINDEX CONCURRENTLY — reconstruir índice sin bloquear writes

---

## BLOQUE 6 — Tipos y SQL Completo (Fases 20-21)

### Fase 20 — Tipos + importación/exportación `⏳` semana 36-37
- [ ] 20.1 ⏳ Views regulares — `CREATE VIEW` y views actualizables
- [ ] 20.2 ⏳ Sequences — `CREATE SEQUENCE`, `NEXTVAL`, `CURRVAL`
- [ ] 20.3 ⏳ ENUMs — `CREATE TYPE ... AS ENUM` con validación y orden semántico
- [ ] 20.4 ⏳ Arrays — `TEXT[]`, `FLOAT[]`, `ANY()`, `@>`
- [ ] 20.5 ⏳ COPY FROM/TO — importar/exportar CSV, JSON, JSONL
- [ ] 20.6 ⏳ Parquet — `READ_PARQUET()` directo + exportar con `crate parquet`
- [ ] 20.7 ⏳ Backup incremental — diff desde último backup + restore completo

### Fase 21 — SQL avanzado `⏳` semana 38-39
- [ ] 21.1 ⏳ Savepoints — `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`
- [ ] 21.2 ⏳ CTEs — `WITH` queries
- [ ] 21.3 ⏳ CTEs recursivos — `WITH RECURSIVE` para árboles y jerarquías
- [ ] 21.4 ⏳ RETURNING — en INSERT, UPDATE, DELETE
- [ ] 21.5 ⏳ MERGE / UPSERT — `ON CONFLICT DO UPDATE` + `MERGE` estándar
- [ ] 21.6 ⏳ CHECK constraints + DOMAIN types
- [ ] 21.7 ⏳ Tablas TEMP y UNLOGGED
- [ ] 21.8 ⏳ Expression indexes — `CREATE INDEX ON users(LOWER(email))`
- [ ] 21.9 ⏳ LATERAL joins
- [ ] 21.10 ⏳ Cursores — `DECLARE`, `FETCH`, `CLOSE`
- [ ] 21.11 ⏳ Query hints — `/*+ INDEX() HASH_JOIN() PARALLEL() */`
- [ ] 21.12 ⏳ DISTINCT ON — primera fila por grupo `SELECT DISTINCT ON (user_id) *`
- [ ] 21.13 ⏳ NULLS FIRST / NULLS LAST — `ORDER BY precio ASC NULLS LAST`
- [ ] 21.14 ⏳ CREATE TABLE AS SELECT — crear tabla desde resultado de query
- [ ] 21.15 ⏳ CREATE TABLE LIKE — clonar estructura de otra tabla
- [ ] 21.16 ⏳ DEFERRABLE constraints — diferir verificación de FK al COMMIT (INITIALLY DEFERRED)
- [ ] 21.17 ⏳ IS DISTINCT FROM / IS NOT DISTINCT FROM — comparación NULL-safe (1 IS DISTINCT FROM NULL → true)
- [ ] 21.18 ⏳ NATURAL JOIN — join automático por columnas con mismo nombre
- [ ] 21.19 ⏳ FETCH FIRST n ROWS ONLY / OFFSET n ROWS — alias SQL estándar para LIMIT
- [ ] 21.20 ⏳ CHECKPOINT — forzar escritura del WAL al disco manualmente

---

## BLOQUE 7 — Features de Producto (Fases 22-23)

### Fase 22 — Features de producto `⏳` semana 40-42
- [ ] 22.1 ⏳ Vector similarity — `VECTOR(n)`, operadores `<=>`, `<->`, `<#>`
- [ ] 22.2 ⏳ HNSW index — `CREATE INDEX USING hnsw(col vector_cosine_ops)`
- [ ] 22.3 ⏳ Búsqueda fuzzy — `SIMILARITY()`, trigramas, `LEVENSHTEIN()`
- [ ] 22.4 ⏳ Scheduled jobs — `cron_schedule()` con `tokio-cron-scheduler`
- [ ] 22.5 ⏳ Foreign Data Wrappers — HTTP + PostgreSQL como fuentes externas
- [ ] 22.6 ⏳ Multi-database — `CREATE DATABASE`, `USE`, cross-db queries
- [ ] 22.7 ⏳ Schema namespacing — `CREATE SCHEMA`, `schema.tabla`
- [ ] 22.8 ⏳ Schema migrations CLI — `dbyo migrate up/down/status`

### Fase 23 — Retrocompatibilidad `⏳` semana 43-45
- [ ] 23.1 ⏳ Lector SQLite nativo — parsear formato binario `.db`/`.sqlite`
- [ ] 23.2 ⏳ ATTACH sqlite — `ATTACH 'file.sqlite' AS src USING sqlite`
- [ ] 23.3 ⏳ Migración desde MySQL — `dbyo migrate from-mysql` con `mysql_async`
- [ ] 23.4 ⏳ Migración desde PostgreSQL — `dbyo migrate from-postgres` con `tokio-postgres`
- [ ] 23.5 ⏳ PostgreSQL wire protocol — puerto 5432, psql y psycopg2 conectan
- [ ] 23.6 ⏳ Ambos protocolos simultáneos — :3306 MySQL + :5432 PostgreSQL

---

## BLOQUE 8 — Sistema de Tipos Completo (Fases 24-26)

### Fase 24 — Tipos completos `⏳` semana 46-48
- [ ] 24.1 ⏳ Enteros: TINYINT, SMALLINT, BIGINT, HUGEINT + variantes U
- [ ] 24.1b ⏳ SERIAL / BIGSERIAL — tipos convenientes de auto-increment (INT + SEQUENCE + DEFAULT)
- [ ] 24.1c ⏳ GENERATED ALWAYS AS IDENTITY — estándar SQL moderno para auto-increment
- [ ] 24.2 ⏳ REAL/FLOAT4 separado de DOUBLE — `f32` vs `f64`
- [ ] 24.3 ⏳ DECIMAL exacto — `rust_decimal` con fast path `i64+scale`
- [ ] 24.4 ⏳ CITEXT — comparaciones case-insensitive automáticas
- [ ] 24.5 ⏳ BYTEA/BLOB — binario con TOAST automático
- [ ] 24.6 ⏳ BIT(n) / VARBIT(n) — cadenas de bits con `bitvec`
- [ ] 24.7 ⏳ TIMESTAMPTZ — siempre UTC interno, conversión al mostrar
- [ ] 24.8 ⏳ INTERVAL — meses/días/µs separados con aritmética de calendario
- [ ] 24.9 ⏳ UUID v4/v7 — `[u8;16]`, v7 ordenable para PKs
- [ ] 24.10 ⏳ INET, CIDR, MACADDR — tipos de red con operadores
- [ ] 24.11 ⏳ RANGE(T) — `int4range`, `daterange`, `tsrange` con `@>` y `&&`
- [ ] 24.12 ⏳ COMPOSITE types — `CREATE TYPE ... AS (campos)`

### Fase 25 — Optimizaciones de tipos `⏳` semana 49-51
- [ ] 25.1 ⏳ VarInt encoding — enteros 1-9 bytes según valor + zigzag para negativos
- [ ] 25.2 ⏳ JSONB binario — tabla de offsets para acceso O(log k) sin parsear
- [ ] 25.3 ⏳ VECTOR cuantización — f16 (2x ahorro) e int8 (4x ahorro)
- [ ] 25.4 ⏳ PAX layout — columnar dentro de cada página 8KB
- [ ] 25.5 ⏳ Estadísticas por columna — histogram, correlación, most_common
- [ ] 25.6 ⏳ ANALYZE — actualizar estadísticas manual y automático
- [ ] 25.7 ⏳ Zero-copy rkyv — nodos B+ Tree sin deserializar desde mmap
- [ ] 25.8 ⏳ Compresión por tipo — Delta, BitPack, LZ4, ZSTD según la columna

### Fase 26 — Cotejamiento completo `⏳` semana 52-54
- [ ] 26.1 ⏳ CollationEngine con ICU4X — niveles Primary/Secondary/Tertiary
- [ ] 26.2 ⏳ Sufijos _ci / _cs / _ai / _as / _bin por columna
- [ ] 26.3 ⏳ Configuración en cascada — servidor → BD → tabla → columna → query
- [ ] 26.4 ⏳ Unicode Normalization — NFC al guardar, NFKC para búsqueda
- [ ] 26.5 ⏳ Sort keys en B+ Tree — `memcmp` correcto con collation
- [ ] 26.6 ⏳ UPPER/LOWER locale-aware — `icu_casemap`, no ASCII simple
- [ ] 26.7 ⏳ LENGTH en codepoints — no en bytes
- [ ] 26.8 ⏳ LIKE respeta collation — `jos%` encuentra `José González`
- [ ] 26.9 ⏳ Encodings legacy — latin1, utf16 con conversión vía `encoding_rs`
- [ ] 26.10 ⏳ ~20 collations configuradas — es_419, en_US, pt_BR, fr_FR, ar...

---

## BLOQUE 9 — SQL Profesional (Fases 27-30)

### Fase 27 — Query Optimizer real `⏳` semana 55-57
- [ ] 27.1 ⏳ Join ordering — programación dinámica, 2^N subconjuntos
- [ ] 27.2 ⏳ Predicate pushdown — mover filtros cerca de los datos
- [ ] 27.3 ⏳ Subquery unnesting — convertir subqueries correlacionados a JOINs
- [ ] 27.4 ⏳ Join elimination — FK garantiza unicidad, quitar JOIN innecesario
- [ ] 27.5 ⏳ Cardinality estimation — histogramas + correlación de columnas
- [ ] 27.6 ⏳ Modelo de costos calibrado — seq_page_cost, random_page_cost

### Fase 28 — Completitud SQL `⏳` semana 58-60
- [ ] 28.1 ⏳ Isolation levels — READ COMMITTED, REPEATABLE READ, SERIALIZABLE (SSI)
- [ ] 28.2 ⏳ SELECT FOR UPDATE / FOR SHARE / SKIP LOCKED / NOWAIT
- [ ] 28.3 ⏳ LOCK TABLE — modos ACCESS SHARE, ROW EXCLUSIVE, ACCESS EXCLUSIVE
- [ ] 28.4 ⏳ Advisory locks — `pg_advisory_lock` / `pg_try_advisory_lock`
- [ ] 28.5 ⏳ UNION / UNION ALL / INTERSECT / EXCEPT
- [ ] 28.6 ⏳ EXISTS / NOT EXISTS / IN subquery / subqueries correlacionados
- [ ] 28.7 ⏳ CASE simple y buscado — en SELECT, WHERE, ORDER BY
- [ ] 28.8 ⏳ TABLESAMPLE SYSTEM y BERNOULLI con REPEATABLE

### Fase 29 — Funciones completas `⏳` semana 61-63
- [ ] 29.1 ⏳ Agregaciones avanzadas — `STRING_AGG`, `ARRAY_AGG`, `JSON_AGG`
- [ ] 29.2 ⏳ Agregaciones estadísticas — `PERCENTILE_CONT`, `MODE`, `FILTER`
- [ ] 29.3 ⏳ Window functions completas — `NTILE`, `PERCENT_RANK`, `CUME_DIST`, `FIRST_VALUE`
- [ ] 29.4 ⏳ Funciones de texto — `REGEXP_*`, `LPAD`, `RPAD`, `FORMAT`, `TRANSLATE`
- [ ] 29.5 ⏳ Funciones de fecha — `AT TIME ZONE`, `AGE`, `TO_CHAR`, `TO_DATE`
- [ ] 29.6 ⏳ Timezone database — tzdata embebida, portable sin depender del OS
- [ ] 29.7 ⏳ Funciones matemáticas — trigonometría, logaritmos, `GCD`, `RANDOM`
- [ ] 29.8 ⏳ COALESCE / NULLIF / GREATEST / LEAST — funciones de comparación básicas
- [ ] 29.9 ⏳ GENERATE_SERIES — generador de secuencias numéricas y de fechas
- [ ] 29.10 ⏳ UNNEST — expandir array a filas individuales
- [ ] 29.11 ⏳ ARRAY_TO_STRING / STRING_TO_ARRAY — conversión array ↔ texto
- [ ] 29.12 ⏳ JSON_OBJECT / JSON_ARRAY / JSON_BUILD_OBJECT — constructores JSON
- [ ] 29.13 ⏳ WIDTH_BUCKET — asignar valores a buckets para histogramas
- [ ] 29.14 ⏳ TRIM LEADING/TRAILING/BOTH — `TRIM(LEADING ' ' FROM str)`
- [ ] 29.15 ⏳ pg_sleep(n) — pausar N segundos (útil para tests y simulaciones)
- [ ] 29.16 ⏳ COPY binary protocol — carga masiva en formato binario (más rápido que CSV)

### Fase 30 — Infraestructura pro `⏳` semana 64-66
- [ ] 30.1 ⏳ Índices GIN — para arrays, JSONB y trigramas
- [ ] 30.2 ⏳ Índices GiST — para rangos y geometría
- [ ] 30.3 ⏳ Índices BRIN — tablas enormes con datos ordenados, mínimo espacio
- [ ] 30.4 ⏳ Índices Hash — O(1) para igualdad exacta
- [ ] 30.5 ⏳ CREATE INDEX CONCURRENTLY — sin bloquear writes
- [ ] 30.6 ⏳ information_schema completo — tables, columns, constraints
- [ ] 30.7 ⏳ pg_catalog básico — pg_class, pg_attribute, pg_index
- [ ] 30.8 ⏳ DESCRIBE / SHOW TABLES / SHOW CREATE TABLE
- [ ] 30.9 ⏳ Two-phase commit — `PREPARE TRANSACTION` / `COMMIT PREPARED`
- [ ] 30.10 ⏳ DDL Triggers — `CREATE EVENT TRIGGER ON ddl_command_end`
- [ ] 30.11 ⏳ TABLESPACES — `CREATE TABLESPACE`, almacenamiento tiered
- [ ] 30.12 ⏳ NOT VALID + VALIDATE CONSTRAINT — constraints sin downtime
- [ ] 30.13 ⏳ GUC — `SET/SHOW/ALTER SYSTEM`, configuración dinámica

---

## BLOQUE 10 — Features Finales y AI (Fases 31-34)

### Fase 31 — Features finales `⏳` semana 67-69
- [ ] 31.1 ⏳ Cifrado en reposo — AES-256-GCM por página
- [ ] 31.2 ⏳ Data masking — `MASK_EMAIL()`, `MASK_PHONE()`, políticas por rol
- [ ] 31.3 ⏳ PREPARE / EXECUTE — plan compilado y reutilizable
- [ ] 31.4 ⏳ Estadísticas extendidas — correlación entre columnas (`CREATE STATISTICS`)
- [ ] 31.5 ⏳ FULL OUTER JOIN
- [ ] 31.6 ⏳ Custom aggregates — `CREATE AGGREGATE MEDIAN(...)`
- [ ] 31.7 ⏳ Geospatial — `POINT`, `ST_DISTANCE_KM`, índice R-Tree (`rstar`)
- [ ] 31.8 ⏳ Query result cache — invalidación automática por tabla
- [ ] 31.9 ⏳ Strict mode — sin coerción silenciosa, errores en truncación
- [ ] 31.10 ⏳ Logical replication — `CREATE PUBLICATION` + `CREATE SUBSCRIPTION`
- [ ] 31.11 ⏳ mTLS + pg_hba.conf equivalente

### Fase 32 — Arquitectura final `⏳` semana 70-72
- [ ] 32.1 ⏳ Refactor workspace completo — 18+ crates especializados
- [ ] 32.2 ⏳ Trait StorageEngine intercambiable — Mmap, Memory, Encrypted, Fault
- [ ] 32.3 ⏳ Trait Index intercambiable — BTree, Hash, Gin, Gist, Brin, Hnsw, Fts
- [ ] 32.4 ⏳ Engine central con pipeline completo — cache→parse→rbac→plan→opt→exec→audit
- [ ] 32.5 ⏳ WAL como event bus — replicación, CDC, cache, triggers, audit
- [ ] 32.6 ⏳ Perfiles release — LTO fat, codegen-units=1, panic=abort
- [ ] 32.7 ⏳ CI/CD — GitHub Actions con test + clippy + bench en cada PR

### Fase 33 — AI-Native Layer `⏳` semana 73-76
- [ ] 33.1 ⏳ AI_EMBED() — Ollama local (primary) + OpenAI (fallback) + cache
- [ ] 33.2 ⏳ VECTOR GENERATED ALWAYS AS (AI_EMBED(col)) STORED
- [ ] 33.3 ⏳ Búsqueda híbrida — BM25 + HNSW + RRF en una sola query
- [ ] 33.4 ⏳ Re-ranking — cross-encoder para resultados más precisos
- [ ] 33.5 ⏳ AI_CLASSIFY(), AI_EXTRACT(), AI_SUMMARIZE(), AI_TRANSLATE()
- [ ] 33.6 ⏳ AI_DETECT_PII() + AI_MASK_PII() — privacidad automática
- [ ] 33.7 ⏳ RAG Pipeline — `CREATE RAG PIPELINE` + `RAG_QUERY()`
- [ ] 33.8 ⏳ Feature Store — `CREATE FEATURE GROUP` + point-in-time correct
- [ ] 33.9 ⏳ Model Store ONNX — `CREATE MODEL` + `PREDICT()` + `PREDICT_AB()`
- [ ] 33.10 ⏳ Adaptive indexing — sugerencias automáticas de índices
- [ ] 33.11 ⏳ Text-to-SQL — `NL_QUERY()`, `NL_TO_SQL()`, `NL_EXPLAIN()`
- [ ] 33.12 ⏳ Anomaly detection — `ANOMALY_SCORE()` + `CREATE ANOMALY DETECTOR`
- [ ] 33.13 ⏳ Privacidad diferencial — `DP_COUNT`, `DP_AVG` con presupuesto por rol
- [ ] 33.14 ⏳ Data lineage — `DATA_LINEAGE()` + GDPR Right to be Forgotten

### Fase 34 — Infraestructura distribuida `⏳` semana 77-80
- [ ] 34.1 ⏳ Sharding — `DISTRIBUTED BY HASH/RANGE/LIST` entre N nodos
- [ ] 34.2 ⏳ Scatter-gather — ejecutar plan en shards en paralelo + merge
- [ ] 34.3 ⏳ Rebalanceo de shards — sin downtime
- [ ] 34.4 ⏳ Logical decoding API — `pg_logical_slot_get_changes()` como JSON
- [ ] 34.5 ⏳ DSN estándar — `dbyo://`, `postgres://`, `DATABASE_URL` env var
- [ ] 34.6 ⏳ Extensions system — `CREATE EXTENSION` + `pg_available_extensions`
- [ ] 34.7 ⏳ Extensions WASM — `CREATE EXTENSION FROM FILE '*.wasm'`
- [ ] 34.8 ⏳ VACUUM FREEZE — prevenir Transaction ID Wraparound
- [ ] 34.9 ⏳ Parallel DDL — `CREATE TABLE AS SELECT WITH PARALLEL N`
- [ ] 34.10 ⏳ pgbench equivalente — `dbyo-bench` con escenarios OLTP estándar
- [ ] 34.11 ⏳ Benchmarks finales — comparativa completa vs MySQL, PostgreSQL, SQLite, DuckDB

### Fase 35 — Deployment y DevEx `⏳` semana 81-83
- [ ] 35.1 ⏳ Dockerfile multi-stage — builder Rust + runtime debian-slim
- [ ] 35.2 ⏳ docker-compose.yml — setup completo con volúmenes y env vars
- [ ] 35.3 ⏳ systemd service file — `dbyo.service` para Linux production
- [ ] 35.4 ⏳ dbyo.toml completo — configuración de red, storage, logging, AI, TLS
- [ ] 35.5 ⏳ Log levels y rotación — trace/debug/info/warn/error + rotación diaria/por tamaño
- [ ] 35.6 ⏳ dbyo-client crate — SDK oficial Rust con pool de conexiones
- [ ] 35.7 ⏳ Python package — `pip install dbyo-python` con API estilo psycopg2
- [ ] 35.8 ⏳ Homebrew formula — `brew install dbyo` para macOS
- [ ] 35.9 ⏳ GitHub Actions CI — test + clippy + bench + fuzz en cada PR
- [ ] 35.10 ⏳ Guía de performance tuning — qué parámetros ajustar para cada workload

---

## Estadísticas de progreso

```
Total subfases:  ~225
Completadas:       0  (0%)
En progreso:       0  (0%)
Pendientes:      225 (100%)

Bloques:         10 bloques temáticos
Fases:           35 fases (~83 semanas)
Fase actual:     1.1 — Workspace setup
Última completada: ninguna
```

---

*Actualizado por `/subfase-completa` al terminar cada subfase.*
