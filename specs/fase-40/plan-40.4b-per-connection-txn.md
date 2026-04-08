# Plan: 40.4b — Per-Connection Transaction State

> **Estado al 2026-04-07**: ~65% ya implementado en commits anteriores.
> Este plan cubre únicamente el trabajo **restante**.

## Trabajo ya completado (no repetir)

| Criterio | Estado |
|---|---|
| `ActiveTxn` struct removida | ✅ |
| `ConnectionTxn` público en `axiomdb-wal` | ✅ |
| `TxnManager.active_set: RwLock<HashSet<TxnId>>` | ✅ |
| `TxnManager.lowest_active_id: AtomicU64` | ✅ |
| `begin()` retorna `ConnectionTxn` | ✅ |
| `commit(conn_txn)` toma ownership | ✅ |
| `rollback(conn_txn, storage)` toma ownership | ✅ |
| `active_snapshot(conn_txn)` RR/RC correcto | ✅ |
| `TransactionSnapshot.active_ids: Arc<HashSet<TxnId>>` | ✅ |
| `autocommit()` wrapper | ✅ |
| WAL rotation y crash recovery | ✅ |
| Todos los `record_*` toman `conn_txn: &mut ConnectionTxn` | ✅ |

## Trabajo restante

| Criterio | Estado |
|---|---|
| `TxnManager.max_committed: AtomicU64` | ❌ sigue u64 |
| Atomicidad: advance + remove bajo mismo write lock | ❌ separados |
| Atomicidad: snapshot lee max_committed + active_set bajo mismo read lock | ❌ separados |
| `ExecutionContext<'a>` struct | ❌ no existe |
| Signature sweep ~106 firmas (`txn: &*TxnManager`) en axiomdb-sql | ❌ |
| `conn_txn` removido de `SessionContext` | ❌ sigue en `ctx.conn_txn` |
| `axiomdb-network` database.rs usa `ExecutionContext` | ❌ |
| `axiomdb-embedded` usa `ExecutionContext` | ❌ |
| Wire protocol smoke test | ❌ |

---

## Archivos a modificar

### Commit 1 — Atomicidad (solo axiomdb-wal)

| Archivo | Cambio |
|---|---|
| `crates/axiomdb-wal/src/txn.rs` | `max_committed: u64` → `AtomicU64` |
| `crates/axiomdb-wal/src/txn_begin_commit.rs` | advance + remove bajo mismo write lock; snapshot bajo read lock |
| `crates/axiomdb-wal/src/txn_inspect.rs` | `snapshot()` y `active_snapshot()` leen `max_committed` dentro del lock |
| `crates/axiomdb-wal/src/txn_construction.rs` | `open()` usa `AtomicU64::new(...)` |

### Commit 2 — ExecutionContext + signature sweep

| Archivo | Cambio |
|---|---|
| `crates/axiomdb-sql/src/exec_ctx.rs` | **CREAR** — `ExecutionContext<'a>` struct |
| `crates/axiomdb-sql/src/lib.rs` | re-export `ExecutionContext` |
| `crates/axiomdb-sql/src/session.rs` | remover `conn_txn: Option<ConnectionTxn>` de `SessionContext` |
| `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` | firma → `(stmt, exec_ctx, conn_txn, ctx)` |
| `crates/axiomdb-sql/src/executor/exec_entry.rs` | actualizar entry points |
| `crates/axiomdb-sql/src/executor/exec_dispatch.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/insert_heap.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/insert_clustered.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/insert_helpers.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/delete.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/update_ctx.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/update_entry.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/update_clustered.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/update_clustered_helpers.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/update_candidates.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_create_table.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_create_index.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_drop_table.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_drop_index.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_alter_column.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_alter_constraint.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_alter_rebuild.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_show.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/ddl_analyze.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/select_core.rs` | `txn: &TxnManager` → `exec_ctx` |
| `crates/axiomdb-sql/src/executor/select_ctx.rs` | `txn: &TxnManager` → `exec_ctx` |
| `crates/axiomdb-sql/src/executor/select_helpers.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/shared.rs` | `conn_txn` viene de parámetro, no de `ctx` |
| `crates/axiomdb-sql/src/executor/staging.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/bulk_empty.rs` | signature sweep |
| `crates/axiomdb-sql/src/executor/agg_*.rs` | snapshot reads usan `exec_ctx.coord` |
| `crates/axiomdb-sql/src/executor/exec_subquery.rs` | `txn: &TxnManager` → `exec_ctx` |
| `crates/axiomdb-sql/src/executor/exec_explain.rs` | signature sweep |
| `crates/axiomdb-network/src/mysql/database.rs` | construir `ExecutionContext`, pasar `conn_txn` por separado |
| `crates/axiomdb-embedded/src/lib.rs` | `conn_txn` ya no en `session.conn_txn` |

---

## Algoritmos

### Commit 1: AtomicU64 + atomicidad DuckDB/PostgreSQL

**Insight de research**: DuckDB usa `atomic<transaction_t> last_commit` escrito DENTRO del
`transaction_lock` mutex. PostgreSQL usa `ProcArrayLock` (exclusive) cubriendo tanto el
advance de `latestCompletedXid` como la remoción del active set.

Para AxiomDB: `max_committed: AtomicU64` escrito dentro de `active_set.write()`.
Esto permite:
- `snapshot()` lea ambos sin lock para "best-effort" (métricas)
- `snapshot()` lea ambos DENTRO de `active_set.read()` para garantía estricta (MVCC)

```rust
// TxnManager — struct change
max_committed: AtomicU64,  // era: max_committed: u64

// TxnManager::commit() — advance + remove ATÓMICAMENTE bajo write lock
{
    let mut set = self.active_set.write().unwrap();
    // DuckDB pattern: advance under lock
    self.max_committed.store(txn_id, Ordering::Release);
    set.remove(&txn_id);
    let new_lowest = set.iter().copied().min().unwrap_or(0);
    self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
}
// ← fuera del lock: handle deferred_free_pages, last_clustered_roots

// TxnManager::snapshot() — lee ambos bajo read lock (PostgreSQL ProcArrayLock pattern)
pub fn snapshot(&self) -> TransactionSnapshot {
    let set = self.active_set.read().unwrap();
    let mc = self.max_committed.load(Ordering::Acquire); // dentro del lock
    let active_ids = Arc::new(set.clone());
    TransactionSnapshot {
        snapshot_id: mc + 1,
        current_txn_id: 0,
        active_ids,
    }
}

// TxnManager::active_snapshot() — igual, lee mc dentro del lock para RC
pub fn active_snapshot(&self, conn_txn: &ConnectionTxn) -> TransactionSnapshot {
    if conn_txn.isolation_level.uses_frozen_snapshot() {
        // RR/Serializable: snapshot congelado en BEGIN (ya correcto, no cambia)
        TransactionSnapshot { ... }
    } else {
        // READ COMMITTED: snapshot fresco — lee mc dentro del lock
        let set = self.active_set.read().unwrap();
        let mc = self.max_committed.load(Ordering::Acquire);
        let active_ids = Arc::new(set.clone());
        TransactionSnapshot {
            snapshot_id: mc + 1,
            current_txn_id: conn_txn.txn_id,
            active_ids,
        }
    }
}
```

**Sitios a actualizar** tras `AtomicU64`:
- `self.max_committed = x` → `self.max_committed.store(x, Ordering::Release)` (~8 sitios)
- `self.max_committed + 1` → `self.max_committed.load(Ordering::Acquire) + 1` (~5 sitios)
- `result.max_committed` en recovery → `AtomicU64::new(result.max_committed)` (1 sitio)

**Nota**: `advance_committed()` y `advance_committed_single()` también mueven max_committed
bajo la lógica del pipeline fsync — deben adquirir `active_set.write()` o simplemente usar
`fetch_max()` en `AtomicU64` con `Ordering::Release` (correcto porque el pipeline no necesita
modificar `active_set` en ese momento — la remoción ya ocurrió en el commit original).

```rust
// advance_committed_single — solo actualiza max_committed sin tocar active_set
// (el txn ya fue removido del active_set en commit())
pub fn advance_committed_single(&mut self, txn_id: TxnId) {
    // fetch_max es más seguro que store para evitar regresión
    self.max_committed.fetch_max(txn_id, Ordering::Release);
}
pub fn advance_committed(&mut self, txn_ids: &[TxnId]) {
    if let Some(&max) = txn_ids.iter().max() {
        self.max_committed.fetch_max(max, Ordering::Release);
    }
}
```

### Commit 2: ExecutionContext + signature sweep

**Insight de research**: DataFusion usa `Arc<TaskContext>` — contexto inmutable por query,
contiene refs read-only a runtime/catalogo/funciones. Para AxiomDB, `'a` lifetime es
suficiente (no se necesita `Arc` bajo single-writer con `&mut Database` lock).

**Ubicación**: `axiomdb-sql/src/exec_ctx.rs`
(no en `axiomdb-wal` — `BloomRegistry` vive en `axiomdb-sql` y no puede depender de `axiomdb-wal`)

```rust
// crates/axiomdb-sql/src/exec_ctx.rs
use axiomdb_storage::StorageEngine;
use axiomdb_wal::TxnManager;
use crate::bloom::BloomRegistry;

/// Bundles shared read-only references for executor functions.
///
/// Inspired by DataFusion's `TaskContext` and DuckDB's `ClientContext`:
/// immutable during query execution, zero-overhead (lifetime refs, no Arc).
///
/// In Phase 40.11 `coord` will be renamed to `TxnCoordinator`. Adding
/// `LockManager` in 40.11 only requires one new field here — no further
/// signature sweeps.
pub struct ExecutionContext<'a> {
    pub storage: &'a dyn StorageEngine,
    pub coord:   &'a TxnManager,
    pub bloom:   &'a BloomRegistry,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(
        storage: &'a dyn StorageEngine,
        coord: &'a TxnManager,
        bloom: &'a BloomRegistry,
    ) -> Self {
        Self { storage, coord, bloom }
    }
}
```

**Nota**: `wal` no se incluye en `ExecutionContext` porque las funciones del executor
no escriben al WAL directamente — eso va por `coord.record_*()` que accede al WAL
internamente. Solo `database.rs` y `Db::execute()` acceden al WAL para el pipeline
de fsync.

**SessionContext — remover conn_txn**:

```rust
// session.rs — ANTES
pub struct SessionContext {
    // ...
    pub conn_txn: Option<ConnectionTxn>,
    // ...
}

// session.rs — DESPUÉS
pub struct SessionContext {
    // ...
    // conn_txn removido — pasa como parámetro explícito
    // ...
    pub in_explicit_txn: bool,  // flag que indica si hay txn activa (para autocommit logic)
}
```

`in_explicit_txn` reemplaza al check `ctx.conn_txn.is_some()` en los lugares que solo
necesitan saber si hay una transacción activa (no el estado de la misma).

**Firma canónica del executor** después del sweep:

```rust
// ANTES
pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>

// DESPUÉS
pub fn execute_with_ctx(
    stmt: Stmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

**Nota sobre mutabilidad**: `exec_ctx` es `&ExecutionContext` (inmutable) porque
`storage` en `ExecutionContext` ya es `&dyn StorageEngine` (interior mutability vía
`PageLockTable` implementado en 40.3). `TxnManager` sigue necesitando `&mut` para
`begin/commit/rollback` — estos se llaman FUERA del executor (en `execute_with_ctx`
wrapper) antes de pasar `conn_txn` adentro.

**Patrón para el entry point en database.rs**:

```rust
// database.rs — DESPUÉS
let mut conn_txn = if session.in_explicit_txn {
    // El conn_txn se pasa desde HandlerState (futuro) o se recupera
    // Por ahora: no cambia en network porque Database.execute_query toma &mut self
    // Ruta de transición: conn_txn vive en session hasta 40.10
    // DEFER: mover conn_txn de session.conn_txn a HandlerState es 40.10
    // En 40.4b: solo removerlo de SessionContext en la capa SQL interna
};

let exec_ctx = ExecutionContext::new(&self.storage, &self.txn, &self.bloom);
let result = execute_with_ctx(analyzed, &exec_ctx, &mut conn_txn, session);
```

**Ruta de transición para conn_txn en network/embedded**:
Para evitar un cambio demasiado disruptivo, `conn_txn` se mantiene fuera de
`SessionContext` pero se pasa desde el llamador. En `Database::execute_query`,
se extrae de un campo local `self.active_conn_txn: Option<ConnectionTxn>` que
reemplaza `session.conn_txn`. En `Db` (embedded), igual: `self.conn_txn: Option<ConnectionTxn>`.

---

## Fases de implementación

### Fase 1: max_committed AtomicU64 (Commit 1)

1. Cambiar `max_committed: u64` → `AtomicU64` en `TxnManager` struct (`txn.rs`)
2. Actualizar `txn_construction.rs`: `max_committed: AtomicU64::new(result.max_committed)`
3. Actualizar `txn_begin_commit.rs`:
   - `commit()`: mover advance + remove dentro del mismo `active_set.write()` block
   - `advance_committed()` / `advance_committed_single()`: usar `fetch_max` con `Ordering::Release`
4. Actualizar `txn_inspect.rs`:
   - `snapshot()`: leer `max_committed` dentro de `active_set.read()` lock
   - `active_snapshot()`: igual para READ COMMITTED path
   - `snapshot_for_active()`: igual
   - `max_committed()` accessor: `self.max_committed.load(Ordering::Acquire)`
5. Verificar: `begin_with_isolation()` — `snapshot_id_at_begin = max_committed.load(Acquire) + 1`
6. Correr: `cargo nextest run -p axiomdb-wal`

### Fase 2: ExecutionContext struct (inicio Commit 2)

1. Crear `crates/axiomdb-sql/src/exec_ctx.rs` con `ExecutionContext<'a>`
2. Re-exportar desde `crates/axiomdb-sql/src/lib.rs`
3. Verificar compilación: `cargo check -p axiomdb-sql`

### Fase 3: Remover conn_txn de SessionContext

1. En `session.rs`: remover campo `conn_txn: Option<ConnectionTxn>`
2. Añadir campo `in_explicit_txn: bool` (reemplaza `conn_txn.is_some()` checks)
3. En `Database` (network): añadir `active_conn_txn: Option<ConnectionTxn>`
4. En `Db` (embedded): ya tiene `conn_txn: Option<ConnectionTxn>` propio — verificar
5. Corregir todos los `ctx.conn_txn` → acceder desde el nuevo lugar
6. `cargo check -p axiomdb-sql -p axiomdb-network -p axiomdb-embedded`

### Fase 4: Signature sweep (resto de Commit 2)

Estrategia: cambiar de afuera hacia adentro.

1. `exec_with_ctx.rs` — entry point principal del executor
2. `exec_dispatch.rs` — dispatcher central
3. DML: `insert_heap_ctx.rs`, `insert_clustered_ctx.rs`, `delete.rs`, `update_ctx.rs`
4. DDL: `ddl_create_table.rs`, `ddl_alter_column.rs`, `ddl_alter_constraint.rs`, etc.
5. SELECT: `select_ctx.rs`, `select_core.rs` — `txn: &TxnManager` → `exec_ctx.coord`
6. Helpers: `shared.rs`, `staging.rs`, `bulk_empty.rs`, `exec_subquery.rs`
7. Leaf helpers: `insert_heap.rs`, `insert_clustered.rs`, `update_clustered.rs`, etc.
8. `cargo nextest run -p axiomdb-sql` — debe pasar limpio

### Fase 5: Network + embedded (Commit 2 continuación)

1. `database.rs`: construir `ExecutionContext`, pasar `&mut self.active_conn_txn` unwrap
2. `embedded/src/lib.rs`: actualizar `execute()` / `query()` para usar `ExecutionContext`
3. `cargo nextest run -p axiomdb-network -p axiomdb-embedded`

### Fase 6: Wire test + closing

1. `tools/wire-test.py` — agregar:
   - Escenario: `BEGIN` + varios INSERTs + `COMMIT` — rows visibles después
   - Escenario: `BEGIN` + INSERT + `ROLLBACK` — rows NO visibles
   - Escenario: autocommit INSERT inmediatamente visible en SELECT siguiente
2. `cargo nextest run --workspace`
3. `cargo clippy --workspace -- -D warnings`
4. `cargo fmt --check`

---

## Tests a escribir

### Unit (axiomdb-wal)
- `test_max_committed_atomic`: verificar que `max_committed` avanza correctamente tras commit
- `test_snapshot_atomic_visibility`: snapshot construido justo antes de commit no ve ese txn
- `test_active_ids_in_snapshot`: snapshot incluye IDs en-flight en `active_ids`

### Integration (axiomdb-sql)
- Todos los tests existentes deben pasar sin cambios (comportamiento idéntico)

### Wire
- `BEGIN` / multi-INSERT / `COMMIT` / `SELECT` → rows visibles
- `BEGIN` / INSERT / `ROLLBACK` / `SELECT` → rows no visibles
- Autocommit: INSERT + SELECT inmediato → visible

---

## Anti-patterns a evitar

- **NO** leer `max_committed` fuera del `active_set` lock cuando se construye un snapshot
  → viola la invariante de visibilidad (PostgreSQL + DuckDB ambos leen bajo lock)
- **NO** usar `Ordering::Relaxed` para el `store` de `max_committed` en commit
  → el `Release` es necesario para que lectores con `Acquire` vean los datos escritos
- **NO** poner `BloomRegistry` en `axiomdb-wal` para que `ExecutionContext` viva ahí
  → crea dependencia cíclica; `ExecutionContext` vive en `axiomdb-sql`
- **NO** dejar `conn_txn` en `SessionContext` — ese campo es la raíz del problema
  que impide múltiples conexiones concurrentes (40.10)
- **NO** hacer `storage: &mut dyn StorageEngine` en `ExecutionContext`
  → storage ya es `&self` con interior mutability (40.3); `&mut` es incorrecto y bloquearía aliasing

---

## Riesgos

| Riesgo | Mitigación |
|---|---|
| `AtomicU64` ordering incorrecto → snapshot ve datos uncommitted | Usar `Release` en store, `Acquire` en load; ambos dentro del `active_set` lock |
| `advance_committed` en pipeline fsync no tiene active_set — ¿inconsistencia? | No hay inconsistencia: el txn ya fue removido del active_set en `commit()`; `advance_committed` solo avanza el número visible |
| ~106 firmas cambiadas — riesgo de regresión silenciosa | `cargo nextest run -p axiomdb-sql` debe pasar limpio antes de tocar network/embedded |
| `conn_txn` removido de `SessionContext` — tests unitarios en axiomdb-sql que usan `ctx.conn_txn` directamente | Buscar con grep antes de remover; actualizar todos los sitios de uso |
| Lifetime `'a` en `ExecutionContext` — posibles conflictos con borrow checker | `storage` es `&'a dyn StorageEngine` (ya resuelto en 40.3); `TxnManager` es `&'a TxnManager`; no hay mutabilidad compartida |

---

## Notas de research

Comparaciones con los 6 sistemas investigados:

| Sistema | max_committed atomic | Bajo mismo lock | ExecutionContext |
|---|---|---|---|
| PostgreSQL | Sí (TransamVariables, protegida por ProcArrayLock) | Sí (exclusive lock cubre advance + XID clear) | Thread-local implícita (EState) |
| DuckDB | Sí (`atomic<transaction_t> last_commit`) | Sí (transaction_lock cubre assign + remove) | `Arc<ClientContext>` |
| AxiomDB 40.4b | Sí (`AtomicU64`) | **Sí (active_set.write() cubre ambos)** | `ExecutionContext<'a>` |
| AxiomDB actual | No (plain u64) | No (separados) | No existe |
