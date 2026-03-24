# Research: DuckDB y OceanBase — INSERT, SELECT, UPDATE, DELETE

> Investigación del código fuente de DuckDB y OceanBase comparada con AxiomDB.
> Objetivo: identificar técnicas adoptables para mejorar AxiomDB.
>
> Repositorios: `research/duckdb/` · `research/oceanbase/`

---

## 1. Arquitecturas en una línea

| DB | Tipo | Storage | MVCC | Wire protocol |
|---|---|---|---|---|
| **DuckDB** | OLAP embebido | Columnar (RowGroups) | ChunkInfo per-vector | DuckDB propio |
| **OceanBase** | OLTP distribuido | LSM-tree (MemTable + SSTable) | Version chains (linked list) | MySQL compat |
| **AxiomDB** | OLTP embebido/server | Row-based slotted pages 16KB | RowHeader inline (txn_id_created/deleted) | MySQL compat |

---

## 2. DuckDB

### 2.1 Arquitectura de storage

DuckDB es **columnar**: los datos se guardan columna por columna, no fila por fila.

```
Tabla "users" (id, name, age):
  RowGroup 0 (filas 0-122879)
    Column "id":   [1, 2, 3, ...]  ← segmento comprimido
    Column "name": ["Alice", "Bob", ...]
    Column "age":  [30, 25, ...]
  RowGroup 1 (filas 122880-...)
    ...
```

- **RowGroup**: ~122,880 filas por grupo (configurable)
- Cada columna tiene su propio segmento con compresión (RLE, dictionary, bitpacking)
- **Zoneamps**: estadísticas por RowGroup (min, max) para skip de grupos enteros

**Archivo clave:**
`research/duckdb/src/storage/row_group.cpp`

### 2.2 MVCC

DuckDB usa `ChunkInfo` por vector (1024 filas):

```cpp
// research/duckdb/src/include/duckdb/storage/table/chunk_info.hpp:81
struct ChunkConstantInfo {
  transaction_t insert_id;  // todas insertadas en esta txn
  transaction_t delete_id;  // todas deletadas en esta txn
};

struct ChunkVectorInfo {
  // Para vectores con cambios heterogéneos
  transaction_t *inserted_data;  // insert_id por fila
  transaction_t *deleted_data;   // delete_id por fila
};
```

Visibilidad:
```
visible si: insert_id <= txn.start_time AND delete_id > txn.start_time
```

**Diferencia con AxiomDB**: AxiomDB guarda `txn_id_created` + `txn_id_deleted` **en cada RowHeader** (24 bytes por fila). DuckDB los agrupa por vector de 1024 filas — más compacto cuando todas las filas de un vector fueron insertadas en la misma txn.

### 2.3 SELECT

**Entry point:** `src/execution/operator/scan/physical_table_scan.cpp:17`

Pipeline:
```
PhysicalTableScan
  ├── GlobalState: inicializa max_threads, dynamic filters
  ├── LocalState: estado por thread
  └── GetData():
        ├── CheckZonemap → salta RowGroups completos si min/max no matchean WHERE
        ├── ColumnScan → carga solo columnas del SELECT (projection pushdown)
        ├── SelectionVector → filtros sin copiar datos (predicate pushdown)
        └── DataChunk de 1024 filas → pipeline operator siguiente
```

**Vectorized execution (DataChunk):**
```cpp
// research/duckdb/src/include/duckdb/common/types/data_chunk.hpp:44
struct DataChunk {
  vector<Vector> data;  // una columna por Vector
  idx_t count;          // filas en este chunk
  // capacity = STANDARD_VECTOR_SIZE = 1024
};
```

Cada `Vector` puede ser:
- **Flat**: array lineal (hot path)
- **Constant**: un solo valor para todas las 1024 filas (e.g. literal)
- **Dictionary**: índices en un diccionario (compresión)

**Clave del rendimiento de DuckDB:** nunca procesa fila por fila — siempre 1024 filas en un batch con SIMD implícito del compilador.

### 2.4 INSERT

**Entry point:** `src/execution/operator/persistent/physical_insert.cpp:27`

Dos rutas:

**Ruta normal (PhysicalInsert):**
```
DataChunk de input
  → Validar constraints (UNIQUE, NOT NULL, FK)
  → LocalAppendState → OptimisticWriteCollection (RAM local a la txn)
  → ART index actualizado localmente
  → En COMMIT: merge a RowGroupCollection principal
```

**Ruta batch (PhysicalBatchInsert):**
```
research/duckdb/src/execution/operator/persistent/physical_batch_insert.cpp
  → CollectionMerger agrupa batches
  → OptimisticDataWriter::WriteUnflushedRowGroups() → escribe RowGroups completos
  → WAL entry por RowGroup (no por fila)
```

**Diferencia clave con AxiomDB**: DuckDB guarda los inserts en **LocalStorage en RAM** hasta el commit. AxiomDB escribe inmediatamente al heap y loguea al WAL. DuckDB puede hacer rollback simplemente descartando el LocalStorage.

### 2.5 UPDATE

**Estrategia: Delete + Insert** (igual que PostgreSQL)

```
research/duckdb/src/execution/operator/persistent/physical_update.cpp:1
  1. Lee fila original (con row_id)
  2. Evalúa expresiones SET
  3. Valida constraints sobre nueva fila
  4. DELETE fila vieja (marca delete_id en ChunkVectorInfo)
  5. INSERT fila nueva (nueva entrada en RowGroup)
  6. Actualiza ART indexes: delete old key + insert new key
```

**Por qué no in-place:** columnar storage no puede hacer update in-place fácilmente (la columna está comprimida). AxiomDB puede hacer in-place porque el heap es row-based.

### 2.6 DELETE

**Entry point:** `src/execution/operator/persistent/physical_delete.cpp:1`

```
Fast path (sin RETURNING, sin unique indexes):
  → table.Delete(delete_state, row_ids)
  → ChunkVectorInfo::Delete(): marca delete_id = current_txn_id
  → O(1) por fila — solo escribe 8 bytes (transaction_t)

Slow path (RETURNING o unique indexes):
  → Recolecta datos de la fila
  → ART::Delete(key) para cada índice
  → Dedup cross-thread bajo return_lock
```

**GC:** Lazy. Las filas deletadas quedan en los RowGroups hasta un Checkpoint/Vacuum. No hay purga inmediata.

### 2.7 Índices — ART (Adaptive Radix Tree)

DuckDB usa ART en lugar de B+ Tree:

```
research/duckdb/src/include/duckdb/execution/index/art/
  ├── art.hpp           — estructura principal
  ├── art_key.hpp       — serialización de keys
  ├── node256.hpp       — nodo con 256 ramas (branch-heavy)
  └── prefix.hpp        — compresión de prefijos
```

**ART vs B+ Tree (AxiomDB):**
| | ART | B+ Tree |
|---|---|---|
| Cache locality | Mejor (fanout adaptivo) | Peor (fanout fijo) |
| Range scan | Peor (no linked leaves) | Mejor (leaves enlazadas) |
| Memory | Adaptivo (4 tipos de nodo) | Fijo (ORDER) |
| Complejidad | Mayor | Menor |

---

## 3. OceanBase

### 3.1 Arquitectura de storage (LSM-tree)

```
Escrituras → MemTable (RAM, skip list)
              ↓ cuando llega al límite (~100MB)
             freeze → MemTable inmutable
              ↓ background compaction
             SSTable (disco, immutable, sorted)
              ↓ periodic major merge
             SSTable compactado (versiones antiguas eliminadas)
```

**MemTable:** `research/oceanbase/src/storage/memtable/ob_memtable.h:275`
- Skip list interno (ObQueryEngine)
- Cada entrada: `(row_key, ObMvccTransNode chain)`
- Hot data → L1/L2 cache friendly

**SSTable:** `research/oceanbase/src/storage/blocksstable/ob_sstable.h`
- Macro blocks de 2MB → micro blocks
- Puede ser row-oriented o column-oriented
- Immutable: nunca se modifica, solo se reemplaza en compaction

### 3.2 MVCC — Version Chains

OceanBase mantiene una **cadena de versiones** por fila:

```
row_key → [ObMvccTransNode(v3, tx=100)] → [ObMvccTransNode(v2, tx=50)] → [v1, tx=10]
                ↑ más reciente                                              ↑ más antiguo
```

Cada `ObMvccTransNode` tiene:
- `created_tx_id` + `created_scn` (Sequence Clock Number)
- `deleted_tx_id` + `deleted_scn`
- Puntero a versión anterior

Visibilidad: `created_scn <= read_snapshot < deleted_scn`

**Diferencia con AxiomDB**: AxiomDB usa `txn_id_created/deleted` inline en el RowHeader — simple pero solo guarda 2 versiones. OceanBase mantiene cadenas arbitrariamente largas para readers históricos.

### 3.3 SELECT

**Entry point:** `src/sql/engine/table/ob_table_scan_op.h`

Pipeline:
```
ObTableScanOp
  ↓
DAS (Data Access Service) — abstracción de tareas por partición
  ↓
AccessService::table_scan()  [ob_access_service.h:112]
  ↓
Merge iterators:
  ├── MemTable iterator (datos calientes en RAM)
  └── SSTable iterators (datos fríos en disco, lazy read)
  ↓
MVCC filter (read snapshot visibility)
  ↓
Resultado al SQL layer
```

**El merge es clave:** cuando buscas una fila, OceanBase la busca en MemTable primero (más reciente), luego en SSTables de más nuevo a más antiguo, hasta encontrar la versión visible para tu snapshot. Esto es más costoso que AxiomDB (que busca solo en el heap), pero escala mucho mejor con actualizaciones concurrentes.

**Vectorización:** El DAS layer soporta procesamiento por batches:
`research/oceanbase/src/sql/das/ob_das_dml_vec_iter.h`

### 3.4 INSERT

**Entry point:** `src/sql/engine/dml/ob_table_insert_op.h:117`

```
ObTableInsertOp::inner_open()
  → calc_tablet_loc() — routing a partición correcta
  → process_insert_row() — validar constraints, triggers, defaults
  → write_row_to_das_buffer() — acumular en buffer
  → flush cuando buffer lleno:
       multi_set(MemTable) — inserta N filas en skip list
```

**Batch optimization (DAS buffer):**
```cpp
// research/oceanbase/src/sql/das/ob_das_insert_op.h:93
int ObDASInsertOp::insert_rows() {
  // Flush buffer → AccessService::insert_rows()
  // One call covers N rows in MemTable
}
```

**Multi-row insert en MemTable:** `ob_memtable.h:283` — `multi_set()` inserta N filas en el skip list con un solo lock acquisition.

**Por qué OceanBase es tan rápido en INSERT:** las filas van a MemTable (RAM), no al disco. La durabilidad viene del WAL separado. El disco se toca solo en la compaction, que es asíncrona.

### 3.5 UPDATE

**Estrategia: Append-Only** (LSM style)

```
UPDATE users SET age=31 WHERE id=1
  1. Buscar fila actual en MemTable/SSTable (versión visible)
  2. Marcar versión actual: deleted_scn = current_scn
  3. Insertar nueva versión: insert_scn = current_scn, age=31
  4. Ambas versiones coexisten en MemTable
  5. Compaction las limpia cuando no hay snapshot antiguo que las necesite
```

**Si cambia la partition key:**
```cpp
// ob_table_update_op.h:98
calc_tablet_loc(upd_ctdef, old_tablet_loc, new_tablet_loc)
// Si cambia partition → DELETE de partición vieja + INSERT en nueva
```

### 3.6 DELETE

```
ObTableDeleteOp
  → delete_row_to_das()
  → MemTable: crear ObMvccTransNode con deleted_scn = current_scn
  → Fila permanece en MemTable con marca de delete
  → Índices marcados para delete (no borrado físico inmediato)
  → Compaction limpia filas con deleted_scn < compaction_snapshot
```

**AccessService interface:** `ob_access_service.h:146`
```cpp
int delete_rows(ls_id, tablet_id, tx_desc, dml_param, column_ids, row_iter, affected_rows);
```

### 3.7 Compaction

Tres niveles: `research/oceanbase/src/storage/compaction/ob_tablet_merge_ctx.h:37`

```
Mini Merge:  MemTable → SSTable (cuando MemTable lleno, ~segundos)
Minor Merge: SSTable + SSTable (para mantener niveles LSM, ~minutos)
Major Merge: Todos los niveles → 1 SSTable (elimina versiones antiguas, ~horas)
```

Major Merge es el equivalente del VACUUM de PostgreSQL. Libera espacio de filas con `deleted_scn < oldest_active_snapshot`.

---

## 4. Comparativa con AxiomDB

### Storage

| Aspecto | DuckDB | OceanBase | AxiomDB |
|---|---|---|---|
| Formato | Columnar (RowGroups) | LSM (MemTable+SSTable) | Row-based slotted pages |
| Página | Variable (RowGroup) | Macro block 2MB | 16 KB fijo |
| Escritura | LocalStorage optimista → merge | MemTable (RAM) → SSTable | Directo al heap + WAL |
| Durabilidad | WAL append-only | WAL + MemTable freeze | WAL append-only |
| Compresión | Por segmento columnar | Por micro block (row/col) | Sin compresión (fase futura) |

### MVCC

| Aspecto | DuckDB | OceanBase | AxiomDB |
|---|---|---|---|
| Granularidad | ChunkVectorInfo (1024 filas) | Version chain por fila | RowHeader inline |
| Versiones | 2 (insert_id + delete_id) | N versiones en cadena | 2 (created + deleted) |
| UPDATE | Delete + Insert | Append nuevo nodo | Mark deleted + Insert |
| GC | Lazy (Checkpoint) | Compaction (Minor/Major) | VACUUM pendiente (Phase 7.11) |

### INSERT/UPDATE/DELETE

| Operación | DuckDB | OceanBase | AxiomDB |
|---|---|---|---|
| INSERT unit | DataChunk (1024 rows) → LocalStorage | DAS buffer → multi_set(MemTable) | insert_batch → heap pages + WAL PageWrite |
| UPDATE | Delete + Insert (columnar) | Append-only (LSM) | Mark deleted + Insert (row-based) |
| DELETE | Marca delete_id en ChunkVectorInfo | Append delete_scn al version chain | Marca txn_id_deleted en RowHeader |
| Index update | ART delete + insert | Sincrónico o deferred | Sincrónico (B+ Tree CoW) |

### Concurrencia

| Aspecto | DuckDB | OceanBase | AxiomDB actual | AxiomDB Phase 7 |
|---|---|---|---|---|
| Readers | Lock-free (MVCC) | Lock-free (snapshot) | Lock-free (CoW B+ Tree) | Lock-free ✅ |
| Writers | LocalStorage optimista | Row-level locks | **Global mutex** | Row-level locks |
| Isolation | Snapshot isolation | Read Committed / Snapshot | Read Committed básico | Snapshot isolation |

---

## 5. Técnicas adoptables por AxiomDB

### ✅ Adoptar — compatible con arquitectura actual

#### A. Vectorized Execution (Phase 8)
DuckDB procesa 1024 filas por "DataChunk". AxiomDB procesa fila por fila.

```
Impacto estimado: +5-10× en full scans y aggregations
Esfuerzo: Phase 8 (ya planificado)
Referencia: duckdb/src/include/duckdb/common/types/data_chunk.hpp:44
```

Implementación: añadir `DataChunk` struct + morsel-driven pipeline en el executor.

#### B. Zonemap / Bloom filter skip (Phase 6.4 ya planificado)
DuckDB: `RowGroup::CheckZonemap()` salta RowGroups completos.
OceanBase: Bloom filters en SSTable blocks.
AxiomDB: Bloom filter per index (ya en roadmap como 6.4).

```
Impacto: evita B-Tree traversal para keys inexistentes
Esfuerzo: medium (Phase 6.4)
Referencia: duckdb/src/storage/table/row_group.hpp:137
```

#### C. LocalStorage optimista para txns (Phase 7)
DuckDB acumula inserts en LocalStorage hasta COMMIT. Si hay ROLLBACK, descarta sin tocar el heap.

```
Impacto: rollback O(1) en vez de undo ops
Esfuerzo: Phase 7 (junto con MVCC completo)
Referencia: duckdb/src/include/duckdb/transaction/local_storage.hpp
```

#### D. DAS buffer + multi_set para batch INSERT
OceanBase bufferiza N filas en DAS antes de llamar a `multi_set(MemTable)`.
AxiomDB ya tiene `insert_rows_batch()` — el patrón es idéntico.

```
Estado: YA IMPLEMENTADO en AxiomDB (Phase 3.17 + 4.16c)
Referencia: oceanbase/src/sql/das/ob_das_insert_op.h:93
```

### ⚠️ Adoptar parcialmente

#### E. Version Chains para MVCC histórico (Phase 7+)
OceanBase mantiene N versiones de cada fila en cadena. AxiomDB solo guarda 2 (current + deleted).

```
Caso de uso: BEGIN READ ONLY AS OF TIMESTAMP '...' (Phase 7.16)
Esfuerzo: alto — requiere cambio en RowHeader y GC
Decisión: implementar en Phase 7.16 (historical reads)
Referencia: oceanbase/src/storage/memtable/mvcc/ob_mvcc_engine.h
```

#### F. Deferred index updates
OceanBase tiene índices "write-only" que se actualizan durante la compaction, no en cada DML.

```
Trade-off: queries más lentas hasta el merge, writes más rápidos
Caso de uso: indexes en tablas de alta inserción
Esfuerzo: medium — requiere marcar indexes como "stale"
Fase sugerida: Phase 6.11 (auto-update statistics) o Phase 7
Referencia: oceanbase/src/storage/memtable/ob_concurrent_control.h:32
```

### ❌ No adoptar (incompatible con diseño actual)

#### G. Pure columnar storage (DuckDB)
AxiomDB es row-based por diseño. Columnar requiere reescribir el storage engine completo.

```
Razón: OLTP necesita row locality para acceso por PK
Alternativa: columnar para analytics (Phase 14 vectorized execution)
```

#### H. LSM-tree (OceanBase)
Reemplazar el heap slotted pages por MemTable+SSTable cambiaría la arquitectura completa.

```
Razón: LSM añade complejidad (compaction, multi-level reads) innecesaria para OLTP embebido
Alternativa: mantener heap + WAL (más simple, igual de rápido para single-node OLTP)
```

---

## 6. Resumen ejecutivo

### ¿Qué hace DuckDB mejor que AxiomDB?
1. **Scans analíticos**: 10-100× más rápido por vectorized + columnar + zonemap
2. **Compresión**: 2-10× menos espacio en disco
3. **Aggregations**: GROUP BY sobre millones de filas es su caso de uso ideal

### ¿Qué hace OceanBase mejor que AxiomDB?
1. **Concurrencia**: row-level locking vs global mutex de AxiomDB
2. **Write throughput a escala**: MemTable absorbe escrituras sin tocar disco
3. **Historical reads**: version chains permiten `AS OF TIMESTAMP`

### ¿Qué hace AxiomDB mejor que ambos?
1. **Simplicidad**: código 10-100× más pequeño, más fácil de mantener
2. **Latencia single-writer**: sin overhead de LSM merge ni columnar projection
3. **Bulk INSERT**: 211K rows/s supera a ambos en insert multi-row (por PageWrite + batch WAL)
4. **DELETE bulk**: 1M rows/s con `WalEntry::Truncate` — sin overhead de undo log por fila
5. **Embebibilidad**: no requiere daemon, fácilmente embebible como SQLite

### Roadmap recomendado basado en esta investigación

```
Phase 6.4  → Bloom filter (DuckDB/OceanBase lo tienen)
Phase 7    → Row-level locks + MVCC completo (OceanBase style)
Phase 7.11 → VACUUM / compaction básica (OceanBase Major Merge simplificado)
Phase 7.16 → Historical reads AS OF TIMESTAMP (versión chains mínimas)
Phase 8    → Vectorized execution DataChunk (DuckDB style, 1024 filas)
```

---

*Investigación basada en el código fuente de:*
- *DuckDB commit HEAD — `research/duckdb/`*
- *OceanBase CE HEAD — `research/oceanbase/`*
- *Fecha: 2026-03-24*
