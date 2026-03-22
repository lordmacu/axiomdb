# Progreso — dbyo Motor de Base de Datos

> Actualizado automáticamente con `/subfase-completa`
> Leyenda: ✅ completada | 🔄 en progreso | ⏳ pendiente | ⏸ bloqueada

---

## BLOQUE 1 — Fundamentos del Motor (Fases 1-7)

### Fase 1 — Storage básico `✅` semana 1-3
- [x] 1.1 ✅ Workspace setup — Cargo.toml, estructura de carpetas, CI básico
- [x] 1.2 ✅ Formato de página — `struct Page`, `PageType`, CRC32c checksum, align(64)
- [x] 1.3 ✅ MmapStorage — abrir/crear `.db`, `read_page`, `write_page` con mmap
- [x] 1.4 ✅ MemoryStorage — implementación en RAM para tests (sin I/O)
- [x] 1.5 ✅ Free list — `alloc_page`, `free_page`, bitmap de páginas libres
- [x] 1.6 ✅ Trait StorageEngine — unificar Mmap y Memory con trait intercambiable
- [x] 1.7 ✅ Tests + benchmarks — unit, integration, bench de read/write de páginas
- [x] 1.8 ✅ File locking — `fs2::FileExt::try_lock_exclusive()` en `create()` y `open()`; `Drop` libera el lock; `DbError::FileLocked` (SQLSTATE 55006) si ya está tomado; 2 tests nuevos
- [x] 1.9 ✅ Error logging desde arranque — `tracing_subscriber::fmt()` con `EnvFilter` en `nexusdb-server/main.rs`; `tracing::{info,debug,warn}` en `MmapStorage` (create, open, grow, drop)

### Fase 2 — B+ Tree `✅` semana 3-4
- [x] 2.1 ✅ Estructuras de nodo — `InternalNodePage`, `LeafNodePage`, bytemuck::Pod
- [x] 2.2 ✅ Lookup por key exacto — búsqueda O(log n) desde root hasta hoja
- [x] 2.3 ✅ Insert con split — split de hoja y propagación al nodo interno
- [x] 2.4 ✅ Range scan — iterador lazy con tree traversal (CoW-safe)
- [x] 2.5 ✅ Delete con merge — merge y redistribución de nodos
- [x] 2.6 ✅ Copy-on-Write — raíz atómica con AtomicU64, readers lock-free por diseño
- [x] 2.7 ✅ Prefix compression — `CompressedNode` en memoria para nodos internos
- [x] 2.8 ✅ Tests + benchmarks — 37 tests, benchmarks Criterion vs std::BTreeMap
- [ ] ⚠️ next_leaf linked list stale en CoW — range scan usa tree traversal en su lugar → retomar en Fase 7 (MVCC + epoch reclamation)

### Fase 3 — WAL y transacciones `🔄` semana 5-10
- [x] 3.1 ✅ Formato WAL entry — `[LSN|Type|Table|Key|Old|New|CRC]` + backward scan
- [x] 3.2 ✅ WalWriter — append-only, LSN global, fsync en commit, open() con scan_last_lsn
- [x] 3.3 ✅ WalReader — scan_forward(from_lsn) streaming + scan_backward() con entry_len_2
- [ ] 3.4 ⏳ RowHeader — `struct RowHeader { txn_id_created, txn_id_deleted, row_version, deleted_flag }` — prerequisito de 3.5 y Fase 7
- [ ] 3.5 ⏳ BEGIN / COMMIT / ROLLBACK básico — transacciones sobre RowHeader
- [ ] 3.5a ⏳ Autocommit mode — cada DML sin BEGIN explícito es su propia transacción; flag `autocommit=ON` por defecto (MySQL compatible); `SET autocommit=0` lo desactiva
- [ ] 3.5b ⏳ Implicit transaction start (MySQL mode) — en MySQL, el primer DML sin autocommit inicia txn implícitamente; necesario para compatibilidad con ORMs que no hacen BEGIN explícito
- [ ] 3.5c ⏳ Error semantics mid-transaction — distinguir entre: (a) violación de constraint → rollback del statement, txn continúa; (b) error grave → rollback de la txn completa; definir comportamiento explícito
- [ ] 3.6 ⏳ WAL Checkpoint — flush dirty pages al disco, truncar WAL hasta checkpoint LSN
- [ ] 3.6b ⏳ ENOSPC handling — detectar `ENOSPC` (disco lleno) en writes de WAL y páginas; hacer graceful shutdown con log de error en lugar de corromper el archivo; alertar antes de llegar al límite (umbral configurable)
- [ ] 3.7 ⏳ WAL rotation — max_wal_size configurable, auto-checkpoint por tamaño
- [ ] 3.8 ⏳ Crash recovery state machine — estados explícitos: `CRASHED→RECOVERING→REPLAYING_WAL→VERIFYING→READY`; validar checkpoint metadata; recovery modes: `strict` (abortar si hay inconsistencia) / `permissive` (best-effort, advertir y continuar)
- [ ] 3.8b ⏳ Partial page write detection — al abrir BD, detectar páginas cuyo checksum no coincide (escritura interrumpida por power loss); en modo strict: rechazar; en modo permissive: marcar como corrupta y restaurar desde WAL si hay entrada reciente
- [ ] 3.9 ⏳ Post-recovery integrity check — verificar coherencia índices vs tabla principal después de replay; detectar y reportar divergencia antes de aceptar conexiones
- [ ] 3.10 ⏳ Tests de durabilidad — escribir → simular crash → releer → verificar; cubrir: checkpoint corrupto, partial page write, WAL truncado, indices divergentes post-crash
- [ ] 3.11 ⏳ Catalog bootstrap — páginas reservadas (0-N) para tablas de sistema al crear/abrir BD
- [ ] 3.12 ⏳ CatalogReader/Writer — API para leer/escribir definiciones de tablas, columnas, constraints e índices
- [ ] 3.13 ⏳ Catalog change notifier — pub-sub interno cuando DDL cambia schema (DDL escribe → suscriptores notificados); prerequisito para invalidar plan cache (5.14) y stats (6.11)
- [ ] 3.14 ⏳ Schema binding — el executor resuelve nombres de tabla/columna contra el catalog
- [ ] 3.13 ⏳ Page dirty tracker — bitmap en memoria de páginas modificadas pendientes de flush; base para WAL checkpoint eficiente
- [ ] 3.15 ⏳ Page dirty tracker — bitmap en memoria de páginas modificadas pendientes de flush; base para WAL checkpoint eficiente
- [ ] 3.16 ⏳ Configuración básica (dbyo.toml) — parsear `page_size`, `max_wal_size`, `data_dir`, `fsync` con `config` crate; defaults seguros si falta el archivo

### Fase 4 — SQL Parser + Executor `⏳` semana 11-25
<!-- Grupo A — Prerequisitos del executor -->
- [ ] 4.0 ⏳ Row codec — encode/decode `Value[]` ↔ bytes con null_bitmap; cubre tipos básicos: BOOL, INT, BIGINT, REAL, DOUBLE, DECIMAL, TEXT, VARCHAR, DATE, TIMESTAMP, NULL
<!-- Grupo B — Parser (AST primero, luego gramática) -->
- [ ] 4.1 ⏳ AST definitions — tipos del árbol sintáctico (nodos Expr, Stmt, TableRef, ColumnDef)
- [ ] 4.2 ⏳ Lexer/Tokenizer — tokens SQL con `nom`
- [ ] 4.2b ⏳ Input sanitization en parser — validar que SQL malformado retorna error SQL claro, nunca `panic`; límite de longitud de query configurable (`max_query_size`); fuzz-test inmediato con entradas aleatorias
- [ ] 4.3 ⏳ Parser DDL — `CREATE TABLE`, `CREATE INDEX`, `DROP TABLE`, `DROP INDEX`
- [ ] 4.3a ⏳ Column constraints en DDL — `NOT NULL`, `DEFAULT expr`, `UNIQUE`, `PRIMARY KEY`, `REFERENCES fk`; parseados como parte de `CREATE TABLE`; prerequisito del executor básico
- [ ] 4.3b ⏳ CHECK constraint básico en DDL — `CHECK (expr)` a nivel columna y tabla; parseado en `CREATE TABLE`; evaluado en INSERT/UPDATE (mueve a Fase 21.6 el CHECK avanzado con DOMAIN)
- [ ] 4.3c ⏳ AUTO_INCREMENT / SERIAL básico — `INT AUTO_INCREMENT` (MySQL) y `SERIAL` (PostgreSQL-compat); genera secuencia interna por tabla; `LAST_INSERT_ID()` retorna el último valor; prerequisito del executor básico (no esperar a Fase 24)
- [ ] 4.3d ⏳ Max identifier length — límite de 64 caracteres para nombres de tabla, columna, índice (compatible MySQL/PostgreSQL); error SQL claro al exceder
- [ ] 4.4 ⏳ Parser DML — `SELECT`, `INSERT`, `UPDATE`, `DELETE`
<!-- Grupo C — Executor básico -->
- [ ] 4.5 ⏳ Executor básico — conectar AST con storage + B+ Tree + catalog (usa 3.12 schema binding); **depende de: 4.1-4.4, 4.18 semántica, 3.12 schema binding**
- [ ] 4.5a ⏳ SELECT sin FROM — `SELECT 1`, `SELECT NOW()`, `SELECT VERSION()`; ORMs y herramientas lo usan como health check al conectar; no requiere ninguna tabla
- [ ] 4.6 ⏳ INSERT ... SELECT — insertar resultado de query directamente
- [ ] 4.7 ⏳ SQLSTATE codes — códigos de error estándar SQL (23505, 42P01, etc.)
<!-- Grupo D — SQL fundamental (necesario antes de wire protocol) -->
- [ ] 4.8 ⏳ JOIN — INNER, LEFT, RIGHT, CROSS con nested loop join básico
- [ ] 4.9a ⏳ GROUP BY hash-based — tabla hash para agrupar; óptimo para alta cardinalidad
- [ ] 4.9b ⏳ GROUP BY sort-based — ordenar primero, luego stream; óptimo cuando datos ya ordenados (índice)
- [ ] 4.9c ⏳ Aggregate functions — COUNT, SUM, MIN, MAX, AVG, COUNT DISTINCT; implementar con estado por grupo
- [ ] 4.9d ⏳ HAVING clause — filtrar grupos post-agregación; necesita evaluar expression sobre estados de grupo
- [ ] 4.10 ⏳ ORDER BY + LIMIT/OFFSET — sort en memoria + paginación
- [ ] 4.10b ⏳ ORDER BY multi-columna con dirección mixta — `ORDER BY a ASC, b DESC, c ASC`; comparador compuesto que respeta dirección por columna; test con NULLs en cada posición
- [ ] 4.10c ⏳ NULLS FIRST / NULLS LAST — `ORDER BY precio ASC NULLS LAST`; comportamiento predeterminado MySQL (NULLs primero en ASC) vs PostgreSQL (NULLs último en ASC); configurable
- [ ] 4.10d ⏳ LIMIT/OFFSET parametrizados — `LIMIT $1 OFFSET $2` en prepared statements; evitar reconstruir plan por cada valor de paginación
- [ ] 4.11 ⏳ Subqueries escalares — `(SELECT MAX(id) FROM t)` en WHERE y SELECT
- [ ] 4.12 ⏳ DISTINCT — `SELECT DISTINCT col1, col2` eliminar duplicados; implementar con hash set o sort; interactúa con ORDER BY
- [ ] 4.12b ⏳ CAST + coerción de tipos básica — conversión explícita e implícita entre tipos compatibles
<!-- Grupo E — Funciones del sistema y DevEx -->
- [ ] 4.13 ⏳ version() / current_user / session_user / current_database() — ORMs llaman esto al conectar
- [ ] 4.14 ⏳ LAST_INSERT_ID() / lastval() — obtener último ID auto-generado (MySQL + PG compat)
- [ ] 4.15 ⏳ CLI interactiva — REPL tipo `sqlite3` shell
- [ ] 4.15b ⏳ DEBUG/VERBOSE mode — flag `--verbose` en CLI y servidor; loggear AST, plan elegido, execution stats por query; necesario para debugging durante desarrollo de Fases 4-10
- [ ] 4.16 ⏳ Tests SQL — suite completa: DDL + DML + JOIN + GROUP BY + ORDER BY + subqueries
<!-- Grupo F — Expression layer y semántica (requeridos por el executor para WHERE, SELECT expressions) -->
- [ ] 4.17 ⏳ Expression evaluator — árbol de evaluación para aritmética (`+`, `-`, `*`, `/`), booleanos (`AND`, `OR`, `NOT`), comparaciones (`=`, `<`, `>`), `LIKE`, `BETWEEN`, `IN (list)`, `IS NULL`
- [ ] 4.17b ⏳ NULL semantics sistemáticas — `NULL + 1 = NULL`, `NULL = NULL → UNKNOWN`, `NULL IN (1,2) = NULL`; las 3 lógicas (TRUE/FALSE/UNKNOWN); `IS NULL` vs `= NULL`; funciones que propagan NULL; sin esto las queries de agregación producen resultados incorrectos silenciosamente
- [ ] 4.18 ⏳ Semantic analyzer — validar existencia de tabla/columna vs catalog, resolución de ambigüedades, error SQL claro por cada violación
- [ ] 4.18b ⏳ Type coercion matrix — reglas explícitas de cuándo/cómo coercionar tipos: `'42'→INT`, `INT→BIGINT`, `DATE→TIMESTAMP`; definir modo MySQL-compatible (permisivo) vs modo estricto; errores claros en conversiones inválidas
- [ ] 4.19 ⏳ Built-in functions básicas — `ABS`, `LENGTH`, `SUBSTR`, `UPPER`, `LOWER`, `TRIM`, `COALESCE`, `NOW()`, `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `ROUND`, `FLOOR`, `CEIL`
<!-- Grupo G — Introspección + DDL de modificación (necesario para ORMs y migraciones tempranas) -->
- [ ] 4.20 ⏳ SHOW TABLES / SHOW COLUMNS / DESCRIBE — introspección básica; ORMs y GUI clients la usan al conectar
- [ ] 4.21 ⏳ TRUNCATE TABLE — vaciar tabla sin WAL entry por fila; más rápido que DELETE sin WHERE
- [ ] 4.22 ⏳ ALTER TABLE básico — `ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN`, `RENAME TABLE` (blocking, sin concurrent); prerequisito para cualquier migración
- [ ] 4.22b ⏳ ALTER TABLE ADD/DROP CONSTRAINT — `ADD CONSTRAINT fk_name FOREIGN KEY`, `DROP CONSTRAINT`, `ADD UNIQUE (col)`, `ADD CHECK (expr)`; sin esto los ORMs no pueden modificar constraints post-creación
- [ ] 4.24 ⏳ CASE WHEN en cualquier contexto — `CASE WHEN x THEN a ELSE b END` en SELECT, WHERE, ORDER BY, GROUP BY, HAVING; la Fase 28.7 lo lista pero se necesita desde Fase 4 para queries básicas de cualquier ORM
- [ ] 4.25 ⏳ Error handling framework — códigos SQLSTATE estándar (23505, 42P01, 40001), propagación sin panic hasta el cliente, recovery desde errores de constraint y tipo; base para todos los demás módulos

### Fase 5 — MySQL Wire Protocol `⏳` semana 26-30
- [ ] 5.1 ⏳ TCP listener con Tokio — aceptar conexiones en :3306
- [ ] 5.2 ⏳ MySQL handshake — Server Greeting + Client Response
- [ ] 5.2a ⏳ Charset/collation negotiation en handshake — `character_set_client`, `character_set_results`, `collation_connection` enviados en Server Greeting; cliente elige charset; sin esto clientes MySQL modernos no pueden conectar o muestran caracteres incorrectos
- [ ] 5.3 ⏳ Autenticación — `mysql_native_password` básico (SHA1-based para compatibilidad MySQL 5.x)
- [ ] 5.3b ⏳ caching_sha2_password — auth plugin de MySQL 8.0+; requerido por MySQL Workbench, DBeaver y clientes modernos; full auth + fast auth path
- [ ] 5.4 ⏳ COM_QUERY handler — recibir SQL, ejecutar, responder
- [ ] 5.4a ⏳ max_allowed_packet enforcement — limitar tamaño de packet entrante (default 64MB); rechazar con error si excede; prevenir OOM por query maliciosa o accidental
- [ ] 5.5 ⏳ Result set serialization — columns + rows en wire protocol (text protocol)
- [ ] 5.5a ⏳ Binary result encoding por tipo — MySQL binary protocol para prepared statements: DATE como `{year,month,day}`, DECIMAL como string de precisión exacta, BLOB como length-prefixed bytes, BIGINT como little-endian 8 bytes; sin esto los tipos se corrompen en prepared statement results
- [ ] 5.6 ⏳ Error packets — serializar `DbError` como MySQL error
- [ ] 5.7 ⏳ Test con cliente real — PHP PDO o Python PyMySQL conecta y hace query
- [ ] 5.8 ⏳ Tests unitarios del protocolo — verificar paquetes handshake/COM_QUERY/error/result-set sin cliente externo
- [ ] 5.9 ⏳ Session state — variables de sesión por conexión: current_database, SET/SHOW, autocommit
- [ ] 5.10 ⏳ COM_STMT_PREPARE / COM_STMT_EXECUTE — prepared statements sobre wire protocol; todos los ORMs los usan, evitan parse overhead por query
- [ ] 5.11 ⏳ COM_PING / COM_QUIT / COM_RESET_CONNECTION / COM_INIT_DB — comandos de gestión de conexión que clientes envían automáticamente
- [ ] 5.11b ⏳ COM_STMT_SEND_LONG_DATA — envío chunked de parámetros grandes (BLOBs, TEXTs) en multiple packets; requerido para INSERT de imágenes/documentos vía prepared statements
- [ ] 5.11c ⏳ Connection state machine explícita — estados: `CONNECTED→AUTH→IDLE→EXECUTING→CLOSING`; timeout handling por estado; detectar socket cerrado abruptamente (keepalive TCP)
- [ ] 5.12 ⏳ Multi-statement queries — responder múltiples SELECTs separados por `;` en un solo COM_QUERY (PHP legacy, scripts SQL)
- [ ] 5.13 ⏳ Prepared statement plan cache — cachear plan compilado por statement_id; reutilizar sin re-parsear en executions sucesivas; suscribirse a catalog change notifier (3.13) para invalidar automáticamente cuando schema cambia; LRU eviction con límite configurable
- [ ] 5.14 ⏳ Benchmarks throughput — medir queries/segundo con 1, 4, 16, 64 conexiones concurrentes; baseline para comparar con MySQL

### Fase 6 — Índices secundarios + FK `⏳` semana 31-39
- [ ] 6.1 ⏳ Múltiples B+ Trees por tabla — un árbol por índice
- [ ] 6.1b ⏳ Composite indexes — índices multi-columna (a, b, c) con comparación lexicográfica
- [ ] 6.2 ⏳ CREATE INDEX — crear árbol y popularlo desde datos existentes
- [ ] 6.3 ⏳ Query planner básico — elegir índice vs full scan con estadísticas simples
- [ ] 6.4 ⏳ Bloom filter por índice — evitar I/O para keys inexistentes
- [ ] 6.5 ⏳ Foreign key checker — validación en INSERT/UPDATE con índice inverso
- [ ] 6.6 ⏳ ON DELETE CASCADE / RESTRICT / SET NULL
- [ ] 6.7 ⏳ Partial UNIQUE index — `UNIQUE WHERE condition` para soft delete
- [ ] 6.8 ⏳ Fill factor — `WITH (fillfactor=70)` para tablas con muchos inserts
- [ ] 6.9 ⏳ Tests de FK e índices — violaciones, cascadas, restricciones
- [ ] 6.10 ⏳ Index statistics bootstrap — al CREATE INDEX: contar filas, estimar NDV (distinct values) por columna; alimenta query planner (6.3)
- [ ] 6.11 ⏳ Auto-update statistics — recalcular stats cuando INSERT/DELETE supera umbral configurable (20% de la tabla); evita planes obsoletos
- [ ] 6.12 ⏳ ANALYZE comando SQL — `ANALYZE [TABLE [columna]]` para forzar actualización de estadísticas manualmente
- [ ] 6.13 ⏳ Index-only scans — cuando columnas del SELECT están todas en el índice, no leer la tabla principal (covering scan)
- [ ] 6.14 ⏳ MVCC en índices secundarios — cada entrada del índice incluye `(key, RecordId, txn_id_visible_desde)`; UPDATE de columna indexada inserta nueva versión sin borrar la anterior; vacuum limpia versiones muertas del índice
- [ ] 6.15 ⏳ Index corruption detection — al abrir BD verificar checksum de índices; detectar divergencia índice vs tabla; `REINDEX` automático si diverge (recovery mode)

### Fase 7 — Concurrencia + MVCC `⏳` semana 40-48
- [ ] 7.1 ⏳ MVCC visibility rules — reglas de snapshot_id sobre RowHeader (struct definido en 3.4): qué filas son visibles; implementar READ COMMITTED (snapshot por statement) y REPEATABLE READ (snapshot por transacción) explícitamente
- [ ] 7.2 ⏳ Transaction manager — contador atómico de txn_id global
- [ ] 7.3 ⏳ Snapshot isolation — reglas de visibilidad por snapshot_id
- [ ] 7.4 ⏳ Readers lockless con CoW — verificar que reads no bloquean writes
- [ ] 7.5 ⏳ Writer serialization — solo 1 writer a la vez por tabla (luego mejorar)
- [ ] 7.6 ⏳ ROLLBACK — marcar filas del txn como deleted
- [ ] 7.7 ⏳ Tests de concurrencia — N readers + N writers simultáneos
- [ ] 7.8 ⏳ Epoch-based reclamation — liberar páginas CoW cuando ningún snapshot activo las referencia
- [ ] 7.9 ⏳ Resolver gap next_leaf CoW — linked list entre hojas en Copy-on-Write (DEFERRED de 2.8)
- [ ] 7.10 ⏳ Lock timeout — esperar lock con timeout configurable (`lock_timeout`); error `LockTimeoutError` si expira; evita deadlocks simples sin detector
- [ ] 7.11 ⏳ MVCC vacuum básico — purgar versiones de filas muertas (txn_id_deleted < oldest_active_snapshot); libera espacio sin bloquear reads
- [ ] 7.12 ⏳ Savepoints básicos — `SAVEPOINT sp1`, `ROLLBACK TO sp1`, `RELEASE sp1`; ORMs los usan para errores parciales en transacciones largas
- [ ] 7.13 ⏳ Tests de aislamiento — verificar READ COMMITTED y REPEATABLE READ con transacciones concurrentes; probar dirty reads, non-repeatable reads, phantom reads; usar transacciones concurrentes reales (no mocks)
- [ ] 7.14 ⏳ Cascading rollback prevention — si txn A aborta y txn B leyó datos de A (dirty read), B también debe abortar; verificar que READ COMMITTED lo previene estructuralmente
- [ ] 7.15 ⏳ Transaction ID overflow prevention básico — `txn_id` es u64; log warning al 50% y 90% de capacidad; plan de VACUUM FREEZE (completo en Fase 34) pero la detección debe ser temprana

---

## BLOQUE 2 — Optimizaciones de Ejecución (Fases 8-10)

### Fase 8 — Optimizaciones SIMD `⏳` semana 19-20
- [ ] 8.1 ⏳ Vectorized filter — evaluar predicados en chunks de 1024 filas
- [ ] 8.2 ⏳ SIMD AVX2 con `wide` — comparar 8-32 valores por instrucción
- [ ] 8.3 ⏳ Query planner mejorado — selectividad, índice vs scan con stats
- [ ] 8.4 ⏳ EXPLAIN básico — mostrar plan elegido (tipo de join, índice o full scan, costo estimado)
- [ ] 8.5 ⏳ Benchmarks SIMD vs MySQL — point lookup, range scan, seq scan
- [ ] 8.6 ⏳ Tests de correctness SIMD — verificar que resultados SIMD son idénticos a row-by-row sin SIMD
- [ ] 8.7 ⏳ CPU feature detection runtime — detectar AVX2/SSE4.2 al arrancar; seleccionar implementación óptima; fallback scalar en CPUs antiguas (ARM, CI)
- [ ] 8.8 ⏳ Benchmark SIMD vs scalar vs MySQL — tabla comparativa por operación (filter, sum, count); documentar speedup real en `docs/fase-8.md`

### Fase 9 — DuckDB-inspired + Join Algorithms `⏳` semana 21-23
- [ ] 9.1 ⏳ Morsel-driven parallelism — dividir en chunks de 100K, Rayon
- [ ] 9.2 ⏳ Operator fusion — scan+filter+project en un solo loop lazy
- [ ] 9.3 ⏳ Late materialization — predicados baratos primero, leer cols caras al final
- [ ] 9.4 ⏳ Benchmarks con paralelismo — medir scaling con N cores
- [ ] 9.5 ⏳ Tests de correctness vectorized — verificar que fusion/morsel/late-mat producen resultados idénticos al executor básico
<!-- Join algorithms: nested loop (4.8) es O(n*m); hash y sort-merge son esenciales para queries reales -->
- [ ] 9.6 ⏳ Hash join — build phase (tabla pequeña en hash map) + probe phase (scan tabla grande); O(n+m) vs O(n*m) del nested loop
- [ ] 9.7 ⏳ Sort-merge join — ordenar ambas tablas por join key + merge; óptimo cuando los datos ya vienen ordenados (índice)
- [ ] 9.8 ⏳ Spill to disk — cuando hash table o sort buffer excede `work_mem`, hacer spill a temp files; sin OOM en joins grandes
- [ ] 9.9 ⏳ Adaptive join selection — query planner elige nested loop / hash / sort-merge según estadísticas de tamaño y selectividad
- [ ] 9.10 ⏳ Benchmarks join algorithms — comparar 3 estrategias con diferentes tamaños; confirmar que hash join supera nested loop en >10K rows

### Fase 10 — Modo embebido + FFI `⏳` semana 24-25
- [ ] 10.1 ⏳ Refactor motor como `lib.rs` reutilizable
- [ ] 10.2 ⏳ C FFI — `dbyo_open`, `dbyo_execute`, `dbyo_close` con `#[no_mangle]`
- [ ] 10.3 ⏳ Compilar como `cdylib` — `.so` / `.dll` / `.dylib`
- [ ] 10.4 ⏳ Binding Python — `ctypes` demo funcionando
- [ ] 10.5 ⏳ Test embebido — misma BD usada desde servidor y desde librería
- [ ] 10.6 ⏳ Binding Node.js (Neon) — módulo nativo `.node` para Electron y apps Node; API async/await
- [ ] 10.7 ⏳ Benchmark modo embebido vs servidor — comparar latencia in-process vs TCP loopback para demostrar ventaja embebida

---

> **🏁 MVP CHECKPOINT — semana ~50**
> Al completar Fase 10, NexusDB debe poder:
> - Aceptar conexiones MySQL desde PHP/Python/Node
> - Ejecutar DDL (CREATE TABLE, ALTER TABLE, DROP) y DML (SELECT/INSERT/UPDATE/DELETE)
> - Transacciones con COMMIT/ROLLBACK/SAVEPOINTS
> - Índices secundarios y FK
> - Crash recovery completo
> - Vectorized execution básico
> - Usable como biblioteca embebida desde C/Python
>
> **ORM target en este punto:** Django ORM y SQLAlchemy con queries básicas.

---

## BLOQUE 3 — Features Avanzadas (Fases 11-15)

### Fase 11 — Robustez e índices `⏳` semana 26-27
- [ ] 11.1 ⏳ Sparse index — una entrada cada N filas para timestamps
- [ ] 11.2 ⏳ TOAST — valores >2KB a páginas de overflow con LZ4
- [ ] 11.3 ⏳ In-memory mode — `open(":memory:")` sin disco
- [ ] 11.4 ⏳ JSON nativo — tipo JSON, `->>`  extracción con jsonpath
- [ ] 11.4b ⏳ JSONB_SET — actualizar campo JSON sin reescribir el documento completo
- [ ] 11.4c ⏳ JSONB_DELETE_PATH — eliminar campo específico de JSONB
- [ ] 11.5 ⏳ Partial indexes — `CREATE INDEX ... WHERE condition`
- [ ] 11.6 ⏳ FTS básico — tokenizer + índice invertido + BM25 ranking
- [ ] 11.7 ⏳ FTS avanzado — frases, booleanos, prefijos, stop words en español
- [ ] 11.8 ⏳ Buffer pool manager — LRU page cache explícito (no solo mmap del OS); dirty list, flush scheduler, prefetch para seq scan
- [ ] 11.9 ⏳ Page prefetching — al detectar scan secuencial, prefetch N páginas adelante con `madvise(MADV_SEQUENTIAL)` o read-ahead propio
- [ ] 11.10 ⏳ Write combining — agrupar writes a páginas calientes en un solo fsync por commit; reduce IOPS en write-heavy workloads

### Fase 12 — Testing + JIT `⏳` semana 28-29
- [ ] 12.1 ⏳ Deterministic simulation testing — `FaultInjector` con semilla
- [ ] 12.2 ⏳ EXPLAIN ANALYZE — tiempos reales por nodo del plan; formato de salida JSON compatible con PostgreSQL (`{"Plan":{"Node Type":..., "Actual Rows":..., "Actual Total Time":..., "Buffers":{}}}`) y formato texto indentado para psql/CLI; métricas: actual rows, loops, shared/local buffers hit/read, planning time, execution time
- [ ] 12.3 ⏳ JIT básico con LLVM — compilar predicados simples a código nativo
- [ ] 12.4 ⏳ Benchmarks finales bloque 1 — comparar con MySQL y SQLite
- [ ] 12.5 ⏳ Fuzz testing SQL parser — `cargo fuzz` sobre el parser con entradas aleatorias; registrar crashes como tests de regresión
- [ ] 12.6 ⏳ Fuzz testing storage — páginas con bytes aleatorios, corrupciones deliberadas; verificar que crash recovery maneja datos corruptos
- [ ] 12.7 ⏳ ORM compatibility tier 1 — Django ORM y SQLAlchemy conectan, ejecutan migraciones simples y queries SELECT/INSERT/UPDATE/DELETE sin errores; documentar workarounds si los hay

### Fase 13 — PostgreSQL avanzado `⏳` semana 30-31
- [ ] 13.1 ⏳ Materialized views — `CREATE MATERIALIZED VIEW` + `REFRESH`
- [ ] 13.2 ⏳ Window functions — `RANK`, `ROW_NUMBER`, `LAG`, `LEAD`, `SUM OVER`
- [ ] 13.3 ⏳ Generated columns — `GENERATED ALWAYS AS ... STORED/VIRTUAL`
- [ ] 13.4 ⏳ LISTEN / NOTIFY — pub-sub nativo con `DashMap` de channels
- [ ] 13.5 ⏳ Covering indexes — `INCLUDE (col1, col2)` en hojas del B+ Tree
- [ ] 13.6 ⏳ Non-blocking ALTER TABLE — shadow table + WAL delta + swap atómico
- [ ] 13.7 ⏳ Row-level locking — bloquear fila específica durante UPDATE/DELETE; reduce contención vs per-table lock de 7.5
- [ ] 13.8 ⏳ Deadlock detection — DFS en grafo de espera cuando lock_timeout expira; matar la transacción más joven

### Fase 14 — TimescaleDB + Redis inspired `⏳` semana 32-33
- [ ] 14.1 ⏳ Table partitioning — `PARTITION BY RANGE/HASH/LIST`
- [ ] 14.2 ⏳ Partition pruning — query planner evita particiones no relevantes
- [ ] 14.3 ⏳ Compresión automática de particiones históricas — LZ4 columnar
- [ ] 14.4 ⏳ Continuous aggregates — refresh incremental solo del delta nuevo
- [ ] 14.5 ⏳ TTL por fila — `WITH TTL 3600` + background reaper en Tokio
- [ ] 14.6 ⏳ LRU eviction — para modo in-memory con límite de RAM
- [ ] 14.7 ⏳ Chunk-level compression statistics — track ratio de compresión por partición; decide cuándo comprimir automáticamente
- [ ] 14.8 ⏳ Benchmarks time-series — insertar 1M rows con timestamp; comparar range scan vs TimescaleDB

### Fase 15 — MongoDB + DoltDB + Arrow `⏳` semana 34-35
- [ ] 15.1 ⏳ Change streams CDC — taildel WAL, emitir eventos Insert/Update/Delete
- [ ] 15.2 ⏳ Git para datos — commits, branches, checkout con snapshot de roots
- [ ] 15.3 ⏳ Git merge — merge de branches con detección de conflictos
- [ ] 15.4 ⏳ Apache Arrow output — resultados en formato columnar para Python/pandas
- [ ] 15.5 ⏳ Flight SQL — protocolo Arrow Flight para transferencia columnar de alta velocidad (Python, Rust, Java sin JDBC)
- [ ] 15.6 ⏳ Tests CDC + Git — verificar streams de cambios y merge de branches con conflictos reales

---

## BLOQUE 4 — Lógica y Seguridad (Fases 16-17)

### Fase 16 — Lógica del servidor `⏳` semana 36-38
- [ ] 16.1 ⏳ SQL UDFs escalares — `CREATE FUNCTION ... AS $$ ... $$`
- [ ] 16.2 ⏳ SQL UDFs de tabla — retornan múltiples filas
- [ ] 16.3 ⏳ Triggers BEFORE/AFTER — con condición `WHEN` y `SIGNAL`
- [ ] 16.3b ⏳ INSTEAD OF triggers — lógica de INSERT/UPDATE/DELETE sobre vistas
- [ ] 16.4 ⏳ Lua runtime — `mlua`, EVAL con `query()` y `execute()` atómicos
- [ ] 16.5 ⏳ WASM runtime — `wasmtime`, sandbox, límites de memoria y timeout
- [ ] 16.6 ⏳ CREATE FUNCTION LANGUAGE wasm FROM FILE — cargar plugin .wasm
- [ ] 16.7 ⏳ Stored procedures — `CREATE PROCEDURE` con control de flujo (`IF`, `LOOP`, `WHILE`, `BEGIN/END`)
- [ ] 16.8 ⏳ Exception handling en procedures — `DECLARE ... HANDLER FOR SQLSTATE`, re-raise, cleanup handlers
- [ ] 16.9 ⏳ Tests de UDFs y triggers — correctitud, error handling, condiciones de WHEN, INSTEAD OF sobre vistas

### Fase 17 — Seguridad `⏳` semana 39-40
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
- [ ] 17.13 ⏳ SQL injection prevention — prepared statements obligatorios en wire protocol; detectar y bloquear interpolación directa en APIs internas
- [ ] 17.14 ⏳ Tests de seguridad — intentos de bypass de RLS, brute force, SQL injection, privilege escalation

---

## BLOQUE 5 — Alta Disponibilidad (Fases 18-19)

### Fase 18 — Alta disponibilidad `⏳` semana 41-43
- [ ] 18.1 ⏳ Streaming replication — enviar WAL en tiempo real al replica
- [ ] 18.2 ⏳ Replica apply — recibir y aplicar WAL entries
- [ ] 18.3 ⏳ Synchronous commit configurable — off, local, remote_write, remote_apply
- [ ] 18.4 ⏳ Cascading replication — replica retransmite a sub-replicas
- [ ] 18.5 ⏳ Hot standby — reads desde réplica mientras aplica WAL
- [ ] 18.6 ⏳ PITR — restaurar al segundo exacto usando WAL archivado
- [ ] 18.7 ⏳ Hot backup — `BACKUP DATABASE` sin lockear
- [ ] 18.8 ⏳ WAL archiving — copiar segmentos WAL a almacenamiento externo (S3/local) automáticamente; prerequisito para PITR (18.6)
- [ ] 18.9 ⏳ Replica lag monitoring — métrica `replication_lag_bytes` y `replication_lag_seconds` expuestas en sistema virtual `sys.replication_status`
- [ ] 18.10 ⏳ Automatic failover básico — detectar primary down + promover standby; configuración mínima sin Raft

### Fase 19 — Mantenimiento + observabilidad `⏳` semana 44-46
- [ ] 19.1 ⏳ Auto-vacuum — background task en Tokio, umbral configurable por tabla
- [ ] 19.2 ⏳ VACUUM CONCURRENTLY — compactar sin bloquear reads ni writes
- [ ] 19.3 ⏳ Deadlock detection — DFS en grafo de espera cada 100ms
- [ ] 19.4 ⏳ Statement fingerprinting — normalizar SQL (remover literales, reemplazar por `$1`, `$2`); hash del resultado para agrupar queries idénticas con parámetros distintos; prerequisito de pg_stat_statements y slow query log
- [ ] 19.4b ⏳ pg_stat_statements — fingerprint (via 19.4) + calls + tiempo total/min/max/stddev + cache hits/misses por query
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
- [ ] 19.17 ⏳ Prometheus metrics endpoint — `/metrics` HTTP en puerto configurable; exponer ops/s, latencia p99, cache hit rate, replication lag
- [ ] 19.18 ⏳ Health check endpoint — `/health` y `/ready` para load balancers; verificar WAL, storage y réplicas
- [ ] 19.19 ⏳ pg_stat_wal — bytes escritos, syncs, tiempo de sync; detectar WAL como cuello de botella
- [ ] 19.20 ⏳ Audit trail infrastructure — escribir audit logs async (buffer circular, sin bloquear writer); formato JSON con: usuario, IP, SQL, bind params, rows_affected, duration, resultado; rotación diaria; prerequisito de 17.7 (CREATE AUDIT POLICY)

---

## BLOQUE 6 — Tipos y SQL Completo (Fases 20-21)

### Fase 20 — Tipos + importación/exportación `⏳` semana 47-48
- [ ] 20.1 ⏳ Views regulares — `CREATE VIEW` y views actualizables
- [ ] 20.2 ⏳ Sequences — `CREATE SEQUENCE`, `NEXTVAL`, `CURRVAL`
- [ ] 20.3 ⏳ ENUMs — `CREATE TYPE ... AS ENUM` con validación y orden semántico
- [ ] 20.4 ⏳ Arrays — `TEXT[]`, `FLOAT[]`, `ANY()`, `@>`
- [ ] 20.5 ⏳ COPY FROM/TO — importar/exportar CSV, JSON, JSONL
- [ ] 20.6 ⏳ Parquet — `READ_PARQUET()` directo + exportar con `crate parquet`
- [ ] 20.7 ⏳ Backup incremental — diff desde último backup + restore completo
- [ ] 20.8 ⏳ COPY streaming — importar CSV/JSON line-by-line sin cargar en memoria; soportar archivos >RAM
- [ ] 20.9 ⏳ Parquet write — exportar query result a Parquet con compresión Snappy/Zstd; útil para pipelines de datos

### Fase 21 — SQL avanzado `⏳` semana 49-51
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
- [ ] 21.16 ⏳ DEFERRABLE constraints — `DEFERRABLE INITIALLY DEFERRED/IMMEDIATE`; buffer de violaciones pendientes por transacción; verificar todas al COMMIT; rollback completo si alguna falla; prerequisito para imports masivos sin orden FK
- [ ] 21.17 ⏳ IS DISTINCT FROM / IS NOT DISTINCT FROM — comparación NULL-safe (1 IS DISTINCT FROM NULL → true)
- [ ] 21.18 ⏳ NATURAL JOIN — join automático por columnas con mismo nombre
- [ ] 21.19 ⏳ FETCH FIRST n ROWS ONLY / OFFSET n ROWS — alias SQL estándar para LIMIT
- [ ] 21.20 ⏳ CHECKPOINT — forzar escritura del WAL al disco manualmente
- [ ] 21.21 ⏳ GROUPING SETS / ROLLUP / CUBE — agregar múltiples niveles de GROUP BY en una sola query
- [ ] 21.22 ⏳ VALUES como tabla inline — `SELECT * FROM (VALUES (1,'a'), (2,'b')) AS t(id, name)`
- [ ] 21.23 ⏳ Tests SQL avanzado — suite cubriendo CTE, window functions, MERGE, savepoints, cursores
- [ ] 21.24 ⏳ ORM compatibility tier 2 — Prisma y ActiveRecord conectan; migraciones con RETURNING, GENERATED IDENTITY y deferred FK; documentar incompatibilidades

---

## BLOQUE 7 — Features de Producto (Fases 22-23)

### Fase 22 — Vector search + búsqueda avanzada `⏳` semana 52-54
- [ ] 22.1 ⏳ Vector similarity — `VECTOR(n)`, operadores `<=>`, `<->`, `<#>`
- [ ] 22.2 ⏳ HNSW index — `CREATE INDEX USING hnsw(col vector_cosine_ops)`
- [ ] 22.3 ⏳ Búsqueda fuzzy — `SIMILARITY()`, trigramas, `LEVENSHTEIN()`
- [ ] 22.4 ⏳ ANN benchmarks — comparar HNSW vs pgvector vs FAISS en recall@10 y QPS; documentar tradeoff calidad/velocidad
- [ ] 22.5 ⏳ IVFFlat index alternativo — opción de índice con menor RAM que HNSW para colecciones >10M vectores

### Fase 22b — Platform features `⏳` semana 55-57
- [ ] 22b.1 ⏳ Scheduled jobs — `cron_schedule()` con `tokio-cron-scheduler`
- [ ] 22b.2 ⏳ Foreign Data Wrappers — HTTP + PostgreSQL como fuentes externas
- [ ] 22b.3 ⏳ Multi-database — `CREATE DATABASE`, `USE`, cross-db queries
- [ ] 22b.4 ⏳ Schema namespacing — `CREATE SCHEMA`, `schema.tabla`
- [ ] 22b.5 ⏳ Schema migrations CLI — `dbyo migrate up/down/status`
- [ ] 22b.6 ⏳ FDW pushdown — enviar predicados SQL al origen remoto cuando es posible; evitar traer filas innecesarias

### Fase 22c — GraphQL API nativa `⏳` semana 58-60
- [ ] 22c.1 ⏳ Servidor GraphQL en puerto `:3308` — schema autodescubierto del catálogo
- [ ] 22c.2 ⏳ GraphQL queries y mutations — mapeadas a point lookups y range scans del B+ Tree
- [ ] 22c.3 ⏳ GraphQL subscriptions — WAL como stream de eventos, WebSocket, sin polling
- [ ] 22c.4 ⏳ GraphQL DataLoader — batch loading automático, elimina el problema N+1
- [ ] 22c.5 ⏳ GraphQL introspection — schema completo para Apollo Studio, Postman, codegen
- [ ] 22c.6 ⏳ GraphQL persisted queries — hash del query pre-registrado; evita transmitir el documento completo en producción
- [ ] 22c.7 ⏳ Tests GraphQL end-to-end — queries, mutations, subscriptions con cliente real (gqlgen/graphql-request)

### Fase 22d — OData v4 nativo `⏳` semana 61-63
- [ ] 22d.1 ⏳ Endpoint HTTP `:3309` — compatible con PowerBI, Excel, Tableau, SAP sin drivers
- [ ] 22d.2 ⏳ OData `$metadata` — documento EDMX autodescubierto desde el catálogo (PowerBI lo consume al conectar)
- [ ] 22d.3 ⏳ OData queries — `$filter`, `$select`, `$orderby`, `$top`, `$skip`, `$count` mapeados a SQL
- [ ] 22d.4 ⏳ OData `$expand` — JOINs por FK: `/odata/orders?$expand=customer` sin SQL manual
- [ ] 22d.5 ⏳ OData batch requests — múltiples operaciones en un solo HTTP request (`$batch`)
- [ ] 22d.6 ⏳ OData autenticación — Bearer token + Basic Auth para conectores enterprise
- [ ] 22d.7 ⏳ Tests OData end-to-end — conectar Excel/PowerBI real + suite $filter/$expand/$batch automatizada

### Fase 23 — Retrocompatibilidad `⏳` semana 64-66
- [ ] 23.1 ⏳ Lector SQLite nativo — parsear formato binario `.db`/`.sqlite`
- [ ] 23.2 ⏳ ATTACH sqlite — `ATTACH 'file.sqlite' AS src USING sqlite`
- [ ] 23.3 ⏳ Migración desde MySQL — `dbyo migrate from-mysql` con `mysql_async`
- [ ] 23.4 ⏳ Migración desde PostgreSQL — `dbyo migrate from-postgres` con `tokio-postgres`
- [ ] 23.5 ⏳ PostgreSQL wire protocol — puerto 5432, psql y psycopg2 conectan
- [ ] 23.6 ⏳ Ambos protocolos simultáneos — :3306 MySQL + :5432 PostgreSQL
- [ ] 23.7 ⏳ Tests de compatibilidad ORM — Django ORM, SQLAlchemy, ActiveRecord, Prisma conectan sin cambios
- [ ] 23.8 ⏳ Dump / restore compatibility — leer dumps de `mysqldump` y `pg_dump --format=plain`
- [ ] 23.9 ⏳ ORM compatibility tier 3 — Typeorm (async), psycopg3 (Python), SQLx (Rust compile-time) conectan; benchmark queries/s vs PostgreSQL nativo

---

> **🏁 PRODUCTION-READY CHECKPOINT — semana ~67**
> Al completar Fase 23, NexusDB debe poder:
> - MySQL + PostgreSQL wire protocols simultáneos
> - Todos los ORMs principales (Django, SQLAlchemy, Prisma, ActiveRecord, Typeorm, psycopg3)
> - Schema migrations con herramientas estándar (Alembic, Rails migrate, Prisma migrate)
> - Importar BDs existentes desde MySQL/PostgreSQL/SQLite
> - Observabilidad completa (métricas, logs, EXPLAIN ANALYZE en JSON)
>
> **ORM target en este punto:** todos los ORMs del tier 3 sin workarounds.

---

## BLOQUE 8 — Sistema de Tipos Completo (Fases 24-26)

### Fase 24 — Tipos completos `⏳` semana 67-69
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
- [ ] 24.13 ⏳ Domain types — `CREATE DOMAIN email AS TEXT CHECK (VALUE ~ '^.+@.+$')` con herencia de constraints
- [ ] 24.14 ⏳ Tests de tipos completos — coerción, overflow, precisión DECIMAL, timezone conversions

### Fase 25 — Optimizaciones de tipos `⏳` semana 70-72
- [ ] 25.1 ⏳ VarInt encoding — enteros 1-9 bytes según valor + zigzag para negativos
- [ ] 25.2 ⏳ JSONB binario — tabla de offsets para acceso O(log k) sin parsear
- [ ] 25.3 ⏳ VECTOR cuantización — f16 (2x ahorro) e int8 (4x ahorro)
- [ ] 25.4 ⏳ PAX layout — columnar dentro de cada página 8KB
- [ ] 25.5 ⏳ Estadísticas por columna — histogram, correlación, most_common
- [ ] 25.6 ⏳ ANALYZE — actualizar estadísticas manual y automático
- [ ] 25.7 ⏳ Zero-copy rkyv — nodos B+ Tree sin deserializar desde mmap
- [ ] 25.8 ⏳ Compresión por tipo — Delta, BitPack, LZ4, ZSTD según la columna
- [ ] 25.9 ⏳ Benchmarks encoding — comparar VarInt vs fixed, PAX vs NSM, zero-copy vs deserializar

### Fase 26 — Cotejamiento completo `⏳` semana 73-75
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
- [ ] 26.11 ⏳ Benchmark collation overhead — costo de ICU4X vs memcmp simple; documentar cuándo vale la pena collation completa

---

## BLOQUE 9 — SQL Profesional (Fases 27-30)

### Fase 27 — Query Optimizer real `⏳` semana 76-78
- [ ] 27.1 ⏳ Join ordering — programación dinámica, 2^N subconjuntos
- [ ] 27.2 ⏳ Predicate pushdown — mover filtros cerca de los datos
- [ ] 27.3 ⏳ Subquery unnesting — convertir subqueries correlacionados a JOINs
- [ ] 27.4 ⏳ Join elimination — FK garantiza unicidad, quitar JOIN innecesario
- [ ] 27.5 ⏳ Cardinality estimation — histogramas + correlación de columnas
- [ ] 27.6 ⏳ Modelo de costos calibrado — seq_page_cost, random_page_cost
- [ ] 27.7 ⏳ Parallel query planning — dividir plan en sub-planes ejecutables en Rayon desde el optimizer
- [ ] 27.8 ⏳ Plan caching y re-use — reutilizar plan para queries estructuralmente idénticas (prepared statements)
- [ ] 27.9 ⏳ Benchmarks optimizer — medir tiempo de planificación vs calidad del plan con TPC-H queries
- [ ] 27.10 ⏳ Adaptive cardinality estimation — corregir estimaciones al final de la ejecución con estadísticas reales; actualizar histogramas automáticamente; evitar planes malos en queries repetidas
- [ ] 27.11 ⏳ OR-to-UNION rewrite — `WHERE a=1 OR b=2` → `SELECT WHERE a=1 UNION SELECT WHERE b=2`; permite usar dos índices distintos vs full scan

### Fase 28 — Completitud SQL `⏳` semana 79-81
- [ ] 28.1 ⏳ Isolation levels — READ COMMITTED, REPEATABLE READ, SERIALIZABLE (SSI)
- [ ] 28.2 ⏳ SELECT FOR UPDATE / FOR SHARE / SKIP LOCKED / NOWAIT
- [ ] 28.3 ⏳ LOCK TABLE — modos ACCESS SHARE, ROW EXCLUSIVE, ACCESS EXCLUSIVE
- [ ] 28.4 ⏳ Advisory locks — `pg_advisory_lock` / `pg_try_advisory_lock`
- [ ] 28.5 ⏳ UNION / UNION ALL / INTERSECT / EXCEPT
- [ ] 28.6 ⏳ EXISTS / NOT EXISTS / IN subquery / subqueries correlacionados
- [ ] 28.7 ⏳ CASE simple y buscado — en SELECT, WHERE, ORDER BY
- [ ] 28.8 ⏳ TABLESAMPLE SYSTEM y BERNOULLI con REPEATABLE
- [ ] 28.9 ⏳ Serializable Snapshot Isolation (SSI) — grafo de dependencias write-read entre transacciones; DFS para detectar ciclos; rollback automático de la transacción más joven al detectar ciclo; prerequisito: 7.1 (MVCC visibility)
- [ ] 28.10 ⏳ Tests de isolation levels — dirty read, non-repeatable read, phantom read; cada test usa transacciones concurrentes reales; verificar que cada nivel previene exactamente lo que debe y no más
- [ ] 28.11 ⏳ SELECT FOR UPDATE / FOR SHARE con skip locked — requerido por job queues (Celery, Sidekiq, Resque); sin esta feature los ORMs de tareas no funcionan

### Fase 29 — Funciones completas `⏳` semana 82-84
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
- [ ] 29.17 ⏳ Funciones de red — `HOST()`, `NETWORK()`, `BROADCAST()`, `MASKLEN()` para tipos INET/CIDR
- [ ] 29.18 ⏳ Tests de funciones — suite cubriendo todos los tipos de función: texto, fecha, matemática, JSON, array

### Fase 30 — Infraestructura pro `⏳` semana 85-87
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
- [ ] 30.14 ⏳ Índice R-Tree nativo — para tipos geoespaciales y rangos multidimensionales (complementa GiST de 30.2)
- [ ] 30.15 ⏳ Benchmarks índices alternativos — GIN/GiST/BRIN/Hash vs B+ Tree en workloads específicos

---

## BLOQUE 10 — Features Finales y AI (Fases 31-34)

### Fase 31 — Features finales `⏳` semana 88-90
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
- [ ] 31.12 ⏳ Connection string DSN — `dbyo://user:pass@host:port/dbname?param=val`; `postgres://` y `mysql://` como alias
- [ ] 31.13 ⏳ Read replicas routing — dirigir queries de solo lectura a réplicas automáticamente desde el connection pool

### Fase 32 — Arquitectura final `⏳` semana 91-93
- [ ] 32.1 ⏳ Refactor workspace completo — 18+ crates especializados
- [ ] 32.2 ⏳ Trait StorageEngine intercambiable — Mmap, Memory, Encrypted, Fault
- [ ] 32.3 ⏳ Trait Index intercambiable — BTree, Hash, Gin, Gist, Brin, Hnsw, Fts
- [ ] 32.4 ⏳ Engine central con pipeline completo — cache→parse→rbac→plan→opt→exec→audit
- [ ] 32.5 ⏳ WAL como event bus — replicación, CDC, cache, triggers, audit
- [ ] 32.6 ⏳ Perfiles release — LTO fat, codegen-units=1, panic=abort
- [ ] 32.7 ⏳ CI/CD — GitHub Actions con test + clippy + bench en cada PR
- [ ] 32.8 ⏳ Plugin API estable — versionar API pública con semver; garantías de ABI para extensiones
- [ ] 32.9 ⏳ Regression test suite — reproducir bugs históricos; red de seguridad para el refactor final

### Fase 33 — AI embeddings + búsqueda híbrida `⏳` semana 94-99
- [ ] 33.1 ⏳ AI_EMBED() — Ollama local (primary) + OpenAI (fallback) + cache
- [ ] 33.2 ⏳ VECTOR GENERATED ALWAYS AS (AI_EMBED(col)) STORED
- [ ] 33.3 ⏳ Búsqueda híbrida — BM25 + HNSW + RRF en una sola query
- [ ] 33.4 ⏳ Re-ranking — cross-encoder para resultados más precisos

### Fase 33b — AI functions `⏳` semana 100-101
- [ ] 33b.1 ⏳ AI_CLASSIFY(), AI_EXTRACT(), AI_SUMMARIZE(), AI_TRANSLATE()
- [ ] 33b.2 ⏳ AI_DETECT_PII() + AI_MASK_PII() — privacidad automática
- [ ] 33b.3 ⏳ Tests AI functions — mocks determinísticos de Ollama/OpenAI para CI; verificar latencia y fallback
- [ ] 33b.4 ⏳ AI function rate limiting — throttle de llamadas al modelo externo; presupuesto de tokens por rol/sesión

### Fase 33c — RAG + Model Store `⏳` semana 102-103
- [ ] 33c.1 ⏳ RAG Pipeline — `CREATE RAG PIPELINE` + `RAG_QUERY()`
- [ ] 33c.2 ⏳ Feature Store — `CREATE FEATURE GROUP` + point-in-time correct
- [ ] 33c.3 ⏳ Model Store ONNX — `CREATE MODEL` + `PREDICT()` + `PREDICT_AB()`
- [ ] 33c.4 ⏳ RAG evaluation — métricas de precisión/recall del RAG pipeline; comparar con baseline de búsqueda BM25

### Fase 33d — AI intelligence + privacidad `⏳` semana 104-106
- [ ] 33d.1 ⏳ Adaptive indexing — sugerencias automáticas de índices basadas en query history
- [ ] 33d.2 ⏳ Text-to-SQL — `NL_QUERY()`, `NL_TO_SQL()`, `NL_EXPLAIN()`
- [ ] 33d.3 ⏳ Anomaly detection — `ANOMALY_SCORE()` + `CREATE ANOMALY DETECTOR`
- [ ] 33d.4 ⏳ Privacidad diferencial — `DP_COUNT`, `DP_AVG` con presupuesto por rol
- [ ] 33d.5 ⏳ Data lineage — `DATA_LINEAGE()` + GDPR Right to be Forgotten

### Fase 34 — Infraestructura distribuida `⏳` semana 107-110
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
- [ ] 34.12 ⏳ Consensus protocol (Raft básico) — para failover automático en cluster; reemplaza failover manual de 18.10
- [ ] 34.13 ⏳ Distributed transactions — two-phase commit entre shards; consistencia cross-shard

### Fase 35 — Deployment y DevEx `⏳` semana 111-113
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
- [ ] 35.11 ⏳ Kubernetes operator — CRD `NexusDBCluster` con replica management y auto-scaling
- [ ] 35.12 ⏳ Helm chart — despliegue en K8s con valores por defecto para producción
- [ ] 35.13 ⏳ Benchmark de producción TPC-H — ejecutar TPC-H completo y publicar resultados; punto de referencia público
- [ ] 35.14 ⏳ Documentación API pública — referencia completa de SQL dialect, wire protocol extensions, C FFI, configuración; autogenerada desde código + hand-written donde necesario
- [ ] 35.15 ⏳ Security audit externo — revisar superficies de ataque antes del release: SQL injection, auth bypass, path traversal en COPY, buffer overflows en parser; usar `cargo-audit` + manual review de unsafe

---

> **🏁 FEATURE-COMPLETE CHECKPOINT — semana ~113**
> Al completar Fase 35, NexusDB es un motor de BD de producción completo:
> - MySQL + PostgreSQL + OData + GraphQL simultáneos
> - AI-native (embeddings, búsqueda híbrida, RAG)
> - Distribución horizontal (sharding + Raft)
> - Deploy en Docker/K8s/systemd
> - Documentación completa y TPC-H publicado

---

## Estadísticas de progreso

```
Total subfases:  450
Completadas:      16  (4%)  — Fases 1 y 2 completas
En progreso:       3  (1%)  — Fase 3 (3.1-3.3 ✅, 3.4-3.16 pendientes)
Pendientes:      433 (96%)

Bloques:         10 bloques temáticos
Fases:           39 fases (~130+ semanas)  — 35 originales + 22b, 22c, 22d, 33b, 33c, 33d; semanas revisadas al alza con estimaciones realistas en Fases 3-7
Fase actual:     3.4 — RowHeader (prerequisito de transacciones)
Última completada: 3.3 — WalReader (scan_forward + scan_backward)

Historial de revisiones:
  2026-03-22 Rev.1:
    +5 subfases en Fase 3 (3.7-3.11): WAL checkpoint, rotation, System Catalog
    +6 subfases en Fase 4 (4.0, 4.12-4.16): row codec, JOIN, GROUP BY, ORDER BY, subqueries, CAST
    +1 subfase  en Fase 6 (6.1b): composite indexes
    +2 subfases en Fase 7 (7.8-7.9): epoch reclamation, linked list gap
    Fix: renumerados 4.8-CLI y 4.9-Tests → 4.10-4.11 (evitar duplicados)

  2026-03-22 Rev.2 (revisión de dependencias y fases gigantes):
    Fase 3 reestructurada: RowHeader insertado como 3.4; checkpoint (3.6) antes de crash recovery (3.8)
    Fase 4 reorganizada: grupos A-E (prerequisitos → parser → executor → SQL fundamental → DevEx)
    Fase 4 expandida: row codec ahora explicita tipos cubiertos (BOOL, INT, BIGINT, DECIMAL, TEXT, DATE, TIMESTAMP)
    +2 subfases en Fase 5 (5.8-5.9): unit tests protocolo + session state
    Fase 7.1 actualizada: MVCC visibility rules referencia RowHeader de 3.4
    +1 subfase en Fase 8 (8.6): SIMD correctness tests
    +1 subfase en Fase 9 (9.5): vectorized correctness tests
    Fase 22 dividida en 4: Fase 22 (vector), 22b (platform), 22c (GraphQL), 22d (OData)
    Fase 33 dividida en 4: Fase 33 (embeddings), 33b (functions), 33c (RAG+models), 33d (intelligence)

  2026-03-22 Rev.5 (análisis de realismo de cronograma + gaps críticos de producción — +27 subfases):
    Fase 1:  +2 (1.8 file locking — CRÍTICO sin esto hay riesgo de corrupción; 1.9 error logging)
    Fase 3:  +4 (3.5a autocommit, 3.5b implicit txn MySQL, 3.5c error semantics, 3.6b ENOSPC,
                  3.8b partial page write detection, 3.8 expandido con recovery modes)
             Semana: 5-7 → 5-10 (realista)
    Fase 4:  +12 (4.2b input sanitization, 4.3a-d constraints DDL, 4.5a SELECT sin FROM,
                   4.10b-d ORDER BY completo + NULLS FIRST/LAST + LIMIT parametrizado,
                   4.12 DISTINCT, 4.15b DEBUG mode, 4.17b NULL semantics,
                   4.22b ALTER TABLE ADD/DROP CONSTRAINT, 4.24 CASE WHEN,
                   4.25 error handling framework)
             Semana: 7-11 → 11-25 (14 semanas; el análisis mostró 14-15w reales)
    Fase 5:  +4 (5.2a charset negotiation, 5.4a max_allowed_packet, 5.5a binary result encoding)
             Semana: 12-14 → 26-30
    Fase 6:  Semana: 15-16 → 31-39 (8 semanas reales vs 2 estimadas)
    Fase 7:  +1 (7.15 txn ID overflow prevention básico)
             Semana: 17-18 → 40-48 (8 semanas reales vs 2 estimadas)
    MVP checkpoint: semana ~25 → semana ~50 (estimación realista)
    Semanas totales: ~113 → ~130+ (cronograma más honesto)

  2026-03-22 Rev.4 (revisión de gaps ORM, durabilidad y protocolo — +43 subfases más):
    Fase 3:  crash recovery state machine (3.8 expandida), post-recovery integrity (3.9 nueva),
             catalog change notifier (3.13 nueva), renumerados 3.13-3.16
    Fase 4:  GROUP BY dividido en 4 (4.9a-d: hash/sort/stream/HAVING),
             type coercion matrix (4.18b), error handling framework (4.24)
    Fase 5:  caching_sha2_password (5.3b), COM_STMT_SEND_LONG_DATA (5.11b),
             connection state machine (5.11c), plan cache invalidation vía catalog notifier
    Fase 6:  MVCC en índices secundarios (6.14), index corruption detection (6.15)
    Fase 7:  READ COMMITTED/REPEATABLE READ explícitos (7.1), cascading rollback (7.14)
    Fase 12: EXPLAIN ANALYZE formato JSON (12.2 expandida), ORM compat tier 1 (12.7)
    Fase 19: statement fingerprinting (19.4), pg_stat_statements (19.4b expandida),
             audit trail infrastructure (19.20)
    Fase 21: DEFERRABLE constraints con buffer (21.16 expandida), ORM compat tier 2 (21.24)
    Fase 23: ORM compat tier 3 (23.9)
    Fase 27: adaptive cardinality (27.10), OR-to-UNION rewrite (27.11)
    Fase 28: SSI con grafo de dependencias (28.9 expandida), SELECT FOR UPDATE con SKIP LOCKED (28.11)
    Fase 35: API docs (35.14), security audit (35.15)
    Nuevos checkpoints MVP (semana ~25), Production-Ready (semana ~67), Feature-Complete (semana ~113)

  2026-03-22 Rev.3 (análisis profundo de gaps — +62 subfases):
    Fase 3:  +2 (3.13 page dirty tracker, 3.14 dbyo.toml config) — semana 5-7
    Fase 4:  +7 (4.17 expression evaluator, 4.18 semantic analyzer, 4.19 built-ins,
                  4.20 SHOW TABLES/DESCRIBE, 4.21 TRUNCATE, 4.22 ALTER TABLE básico,
                  4.23 hash aggregation) — semana 7-11
    Fase 5:  +5 (5.10 COM_STMT_PREPARE, 5.11 COM_PING/QUIT, 5.12 multi-stmt,
                  5.13 plan cache, 5.14 benchmarks throughput) — semana 12-14
    Fase 6:  +4 (6.10 index stats bootstrap, 6.11 auto-update stats,
                  6.12 ANALYZE cmd, 6.13 index-only scans) — semana 15-16
    Fase 7:  +4 (7.10 lock timeout, 7.11 MVCC vacuum básico,
                  7.12 savepoints, 7.13 isolation tests) — semana 17-18
    Fase 8:  +2 (8.7 CPU feature detection, 8.8 benchmark comparativo) — semana 19-20
    Fase 9:  +5 (9.6 hash join, 9.7 sort-merge join, 9.8 spill to disk,
                  9.9 adaptive join selection, 9.10 benchmarks join) — semana 21-23
    Fase 10: +2 (10.6 Node.js Neon binding, 10.7 benchmark embedded vs server)
    Fase 11: +3 (11.8 buffer pool manager, 11.9 page prefetching, 11.10 write combining)
    Fase 12: +2 (12.5 fuzz SQL parser, 12.6 fuzz storage)
    Fase 13: +2 (13.7 row-level locking, 13.8 deadlock detection — movido de 19.3)
    Fase 14: +2 (14.7 chunk stats, 14.8 benchmarks time-series)
    Fase 15: +2 (15.5 Flight SQL, 15.6 tests CDC+Git)
    Fase 16: +3 (16.7 stored procedures, 16.8 exception handling, 16.9 tests)
    Fase 17: +2 (17.13 SQL injection prevention, 17.14 security tests)
    Fase 18: +3 (18.8 WAL archiving, 18.9 replica lag monitoring, 18.10 automatic failover)
    Fase 19: +3 (19.17 Prometheus metrics, 19.18 health check, 19.19 pg_stat_wal)
    Fase 20: +2 (20.8 COPY streaming, 20.9 Parquet write)
    Fase 21: +3 (21.21 GROUPING SETS/ROLLUP/CUBE, 21.22 VALUES tabla inline, 21.23 tests)
    Fase 22: +2 (22.4 ANN benchmarks, 22.5 IVFFlat index)
    Fase 22b:+1 (22b.6 FDW pushdown)
    Fase 22c:+2 (22c.6 persisted queries, 22c.7 tests GraphQL)
    Fase 22d:+1 (22d.7 tests OData)
    Fase 23: +2 (23.7 ORM compat tests, 23.8 dump/restore compat)
    Fase 24: +2 (24.13 domain types, 24.14 tests tipos completos)
    Fase 25: +1 (25.9 benchmarks encoding)
    Fase 26: +1 (26.11 benchmark collation overhead)
    Fase 27: +3 (27.7 parallel query planning, 27.8 plan cache, 27.9 benchmarks optimizer)
    Fase 28: +2 (28.9 SSI, 28.10 isolation level tests)
    Fase 29: +2 (29.17 funciones de red, 29.18 tests funciones)
    Fase 30: +2 (30.14 R-Tree, 30.15 benchmarks índices)
    Fase 31: +2 (31.12 DSN, 31.13 read replicas routing)
    Fase 32: +2 (32.8 plugin API, 32.9 regression tests)
    Fase 33b:+1 (33b.4 AI rate limiting)
    Fase 33c:+1 (33c.4 RAG evaluation)
    Fase 34: +2 (34.12 Raft consensus, 34.13 distributed transactions)
    Fase 35: +3 (35.11 K8s operator, 35.12 Helm chart, 35.13 TPC-H benchmark)
    Semanas totales: ~83 → ~113 (más realista para un motor completo de producción)
```

---

*Actualizado por `/subfase-completa` al terminar cada subfase.*
