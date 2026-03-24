# Plan: 3.19 — WAL Group Commit

## Files to create/modify

| Acción | Archivo | Qué hace |
|---|---|---|
| Modify | `crates/axiomdb-storage/src/config.rs` | Añade `group_commit_interval_ms` + `group_commit_max_batch` a `DbConfig` |
| Modify | `crates/axiomdb-wal/src/txn.rs` | Añade `commit_deferred()`, `advance_committed()`, `wal_flush_and_fsync()` |
| **Create** | `crates/axiomdb-wal/src/commit_coordinator.rs` | `CommitCoordinator`, `CommitTicket`, lógica de batching |
| Modify | `crates/axiomdb-wal/src/lib.rs` | Re-exporta `CommitCoordinator` |
| Modify | `crates/axiomdb-core/src/error.rs` | Nuevo variant `WalCommitFailed` |
| Modify | `crates/axiomdb-network/src/mysql/database.rs` | Añade `coordinator: Option<CommitCoordinator>`, modifica `execute_query`/`execute_stmt` |
| **Create** | `crates/axiomdb-network/src/mysql/group_commit.rs` | Función `spawn_group_commit_task` + background loop |
| Modify | `crates/axiomdb-network/src/mysql/handler.rs` | Libera el lock antes de awaitar confirmación de fsync |
| Modify | `crates/axiomdb-network/src/mysql/mod.rs` | Expone `group_commit` módulo |
| **Create** | `crates/axiomdb-wal/tests/integration_group_commit.rs` | Tests de batching, crash, fsync failure |
| Modify | `crates/axiomdb-sql/benches/executor_e2e.rs` | Bench `insert_concurrent_N` con 1/4/8/16 conexiones |

---

## Algoritmo / Estructuras de datos

### CommitCoordinator

```rust
// axiomdb-wal/src/commit_coordinator.rs

pub struct CommitCoordinatorConfig {
    pub interval_ms: u64,   // 0 = deshabilitado
    pub max_batch: usize,   // fsync inmediato si hay >= max_batch waiters
}

struct CommitTicket {
    txn_id: TxnId,
    reply: oneshot::Sender<Result<(), DbError>>,
}

// Clone-able handle. pending y trigger son Arc internamente.
pub struct CommitCoordinator {
    pending: Arc<tokio::sync::Mutex<Vec<CommitTicket>>>,
    trigger: Arc<Notify>,
    config: CommitCoordinatorConfig,
}

impl CommitCoordinator {
    pub fn new(config: CommitCoordinatorConfig) -> Self { ... }

    /// Registra una txn DML como "esperando fsync".
    /// Llama trigger.notify_one() si pending.len() >= config.max_batch.
    /// NO bloquea — devuelve receiver inmediatamente.
    pub async fn register_pending(
        &self,
        txn_id: TxnId,
    ) -> oneshot::Receiver<Result<(), DbError>> { ... }

    /// Drena todos los tickets pendientes atomicamente.
    /// Llamado por el background task.
    async fn drain_pending(&self) -> Vec<CommitTicket> { ... }
}
```

### Background task

```rust
// axiomdb-network/src/mysql/group_commit.rs

pub fn spawn_group_commit_task(db: Arc<tokio::sync::Mutex<Database>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let coordinator = {
                let guard = db.lock().await;
                match &guard.coordinator {
                    Some(c) => (c.trigger.clone(), c.config.clone()),
                    None => return,  // deshabilitado, termina el task
                }
            };

            // Espera: timer O trigger (max_batch alcanzado)
            tokio::select! {
                _ = coordinator.trigger.notified() => {}
                _ = tokio::time::sleep(
                    Duration::from_millis(coordinator.config.interval_ms)
                ) => {}
            }

            // Drena tickets FUERA del Database lock
            let tickets = {
                let guard = db.lock().await;
                match &guard.coordinator {
                    Some(c) => c.drain_pending().await,
                    None => return,
                }
            };

            if tickets.is_empty() {
                continue;
            }

            // Adquiere Database lock para flush + fsync + advance_committed
            let fsync_result: Result<(), DbError> = {
                let mut guard = db.lock().await;
                let r = guard.txn.wal_flush_and_fsync();
                if r.is_ok() {
                    let ids: Vec<TxnId> = tickets.iter().map(|t| t.txn_id).collect();
                    guard.txn.advance_committed(&ids);
                }
                r
            }; // lock liberado

            // Notifica a todos los waiters
            for ticket in tickets {
                let payload = fsync_result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| e.clone());
                let _ = ticket.reply.send(payload);
            }

            if fsync_result.is_err() {
                tracing::error!("group commit fsync failed — database in degraded state");
            }
        }
    })
}
```

### TxnManager additions

```rust
// axiomdb-wal/src/txn.rs

/// Escribe Commit entry al BufWriter, sin flush ni fsync.
/// Solo para txns DML (undo_ops non-empty).
/// Para txns read-only: usa flush_no_sync() como siempre y devuelve None.
/// Devuelve Some(txn_id) si la txn fue DML (caller debe registrar con CommitCoordinator).
/// Devuelve None si fue read-only (ya flusheada a OS page cache).
pub fn commit_deferred(&mut self) -> Result<Option<TxnId>, DbError> {
    let active = self.active.take().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
    self.wal.append_with_buf(&mut entry, &mut self.wal_scratch)?;

    if active.undo_ops.is_empty() {
        // Read-only: flush a OS page cache, no fsync needed
        self.wal.flush_no_sync()?;
        self.max_committed = txn_id;
        Ok(None)
    } else {
        // DML: Commit escrito al buffer pero NO flusheado ni fsynced.
        // max_committed NO avanza aquí — solo avanza cuando fsync confirma.
        Ok(Some(txn_id))
    }
}

/// Avanza max_committed al máximo de los txn_ids dados.
/// Llamado por el background task DESPUÉS de que fsync tiene éxito,
/// mientras se tiene el Database lock.
pub fn advance_committed(&mut self, txn_ids: &[TxnId]) {
    if let Some(&max) = txn_ids.iter().max() {
        if max > self.max_committed {
            self.max_committed = max;
        }
    }
}

/// Flush + fsync: BufWriter → OS page cache → disco.
/// Llamado por el background task (CommitCoordinator) mientras tiene el lock.
pub fn wal_flush_and_fsync(&mut self) -> Result<(), DbError> {
    self.wal.commit()  // existente: flush() + sync_all()
}
```

### Database changes

```rust
// axiomdb-network/src/mysql/database.rs

pub type CommitRx = oneshot::Receiver<Result<(), DbError>>;

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    pub coordinator: Option<CommitCoordinator>,
}

// execute_query retorna (QueryResult, Option<CommitRx>)
// CommitRx = Some cuando group commit está habilitado Y la txn fue DML
// CommitRx = None en todos los demás casos (disabled, read-only)
pub fn execute_query(
    &mut self,
    sql: &str,
    session: &mut SessionContext,
    schema_cache: &mut SchemaCache,
) -> Result<(QueryResult, Option<CommitRx>), DbError> {
    let stmt = parse(sql, None)?;
    let snap = self.txn.active_snapshot().unwrap_or_else(|_| self.txn.snapshot());
    let analyzed = analyze_cached(stmt, &self.storage, snap, schema_cache)?;
    let result = execute_with_ctx(analyzed, &mut self.storage, &mut self.txn, session)?;

    // Group commit path: solo cuando coordinator existe y txn fue DML
    if self.coordinator.is_some() {
        // commit_deferred() escribe Commit al buffer, NO flushed.
        // Devuelve Some(txn_id) si DML, None si read-only.
        if let Some(txn_id) = self.txn.commit_deferred()? {
            let rx = self.coordinator.as_ref().unwrap()
                .register_pending(txn_id).await;
            return Ok((result, Some(rx)));
        }
        return Ok((result, None));
    }

    // Original path: commit() con fsync inline
    // (cuando coordinator es None — disabled)
    Ok((result, None))
}
```

⚠️ **Nota de diseño**: `execute_query` actualmente no es `async`. El `register_pending` que llama a `Mutex::lock().await` requiere async. Solución: hacer `execute_query` async, o usar `tokio::sync::Mutex::blocking_lock()` (disponible en contexto síncrono dentro de un runtime). Dado que `database.rs` ya vive dentro del contexto Tokio (todas las llamadas vienen de `handler.rs` async), usar `blocking_lock()` es correcto y evita cambiar todas las firmas.

Alternativa más limpia: no hacer `execute_query` async — en su lugar, hacer que `register_pending` use `std::sync::Mutex` (no async) para la queue. Esto funciona porque la operación es solo un push a un Vec, que es O(1) y nunca bloquea en la práctica.

**Decisión**: `CommitCoordinator::pending` usa `std::sync::Mutex<Vec<CommitTicket>>` (no Tokio Mutex), y `register_pending` es síncrono. Solo el background task necesita async (para `tokio::time::sleep` y `trigger.notified()`).

### Handler changes

```rust
// handler.rs — COM_QUERY (línea ~207)

// ANTES:
let result = {
    let mut guard = db.lock().await;
    guard.execute_query(sql, &mut session, &mut schema_cache)
};

// DESPUÉS:
let (result, commit_rx) = {
    let mut guard = db.lock().await;
    guard.execute_query(sql, &mut session, &mut schema_cache)?
    // guard se suelta aquí, ANTES del await
};

// Await fsync confirmation FUERA del lock
if let Some(rx) = commit_rx {
    rx.await.map_err(|_| DbError::Internal("commit coordinator dropped".into()))??;
}

match result { ... }  // serializar y enviar al cliente
```

Lo mismo para COM_STMT_EXECUTE (dos call sites: `execute_stmt` y `execute_query`).

### Config changes

```rust
// axiomdb-storage/src/config.rs

pub struct DbConfig {
    // ... existentes ...

    /// Intervalo en ms para batching de fsyncs del WAL.
    /// 0 = deshabilitado (default). Recomendado: 1ms en producción.
    #[serde(default)]
    pub group_commit_interval_ms: u64,

    /// Número máximo de txns en un batch antes de forzar fsync inmediato.
    /// Solo aplica cuando group_commit_interval_ms > 0.
    #[serde(default = "default_group_commit_max_batch")]
    pub group_commit_max_batch: usize,
}

fn default_group_commit_max_batch() -> usize { 64 }
```

---

## Fases de implementación

### Fase 1 — Foundations (sin cambios visibles aún)
1. Añadir `group_commit_interval_ms` + `group_commit_max_batch` a `DbConfig`
2. Añadir `WalCommitFailed` a `DbError`
3. Añadir `commit_deferred()`, `advance_committed()`, `wal_flush_and_fsync()` a `TxnManager`
   - `commit_deferred()` con `undo_ops.is_empty()` check
   - Tests unitarios para ambos métodos
4. Crear `commit_coordinator.rs`: `CommitCoordinator`, `CommitTicket`, `register_pending()`, `drain_pending()`
   - `pending` usa `std::sync::Mutex<Vec<CommitTicket>>`
   - `register_pending()` es síncrono
   - Tests unitarios: register → drain roundtrip, max_batch trigger
5. Re-exportar `CommitCoordinator` desde `axiomdb-wal/src/lib.rs`

### Fase 2 — Database integration
6. Añadir `coordinator: Option<CommitCoordinator>` a `Database`
7. Modificar `execute_query()` y `execute_stmt()`:
   - Cuando `coordinator.is_some()` y txn fue DML: usar `commit_deferred()` + `register_pending()`
   - Return type cambia a `Result<(QueryResult, Option<CommitRx>), DbError>`
8. Tests de integración en `database.rs` que verifican ambos paths (con/sin coordinator)

### Fase 3 — Background task + server wiring
9. Crear `group_commit.rs` con `spawn_group_commit_task(db: Arc<Mutex<Database>>)`
10. En server startup (o `main.rs`): if `config.group_commit_interval_ms > 0`, crear
    `CommitCoordinator`, asignar a `db.coordinator`, llamar `spawn_group_commit_task`
11. Verificar que el task termina limpiamente cuando `Database` se dropea (Weak o channel de shutdown)

### Fase 4 — Handler wiring
12. Actualizar los 3 call sites en `handler.rs` (COM_QUERY, COM_STMT_EXECUTE fast+fallback):
    - Desestructurar `(result, commit_rx)`
    - Drop del guard antes del await
    - `if let Some(rx) = commit_rx { rx.await??; }`
13. Extraer helper `async fn await_commit(rx: Option<CommitRx>) -> Result<(), DbError>`
    para evitar duplicar la lógica en los 3 call sites

### Fase 5 — Tests + benchmark
14. `crates/axiomdb-wal/tests/integration_group_commit.rs`:
    - `test_disabled_mode_behavior_unchanged`: con `interval_ms=0`, todo igual
    - `test_batch_of_n_uses_one_fsync`: mock fsync counter, N txns → 1 fsync call
    - `test_crash_before_fsync_loses_data`: commit_deferred, proceso "muere" (drop sin fsync), recovery verifica que row no existe
    - `test_fsync_failure_propagated_to_all_waiters`: inyectar fallo de I/O, todos los waiters reciben Err
    - `test_advance_committed_after_fsync_only`: max_committed no cambia antes de fsync
    - `test_read_only_txn_not_registered`: commit_deferred devuelve None para SELECT
15. Bench en `executor_e2e.rs`:
    - `bench_insert_serial`: 1 conexión, group commit off (baseline)
    - `bench_insert_concurrent_4`: 4 conexiones paralelas con `interval_ms=1`
    - `bench_insert_concurrent_8`: 8 conexiones
    - `bench_insert_concurrent_16`: 16 conexiones
    - Reportar: ops/s, fsyncs/s, batch_size promedio

---

## Tests a escribir

### Unit tests (en txn.rs)
- `commit_deferred_dml_returns_txn_id`: INSERT → commit_deferred → Some(id)
- `commit_deferred_readonly_returns_none`: SELECT → commit_deferred → None
- `advance_committed_advances_max`: advance([3,5,4]) → max_committed = 5
- `advance_committed_no_regression`: max_committed=10, advance([3]) → max_committed = 10

### Unit tests (en commit_coordinator.rs)
- `register_and_drain_roundtrip`: register 3, drain → Vec de 3 tickets
- `max_batch_triggers_notify`: register max_batch-1 → no notify; register 1 more → notify
- `drain_empty_returns_empty`: drain without registering → vec![]

### Integration tests (en integration_group_commit.rs)
- Los 6 tests listados arriba en Fase 5

### Bench (en executor_e2e.rs)
- Los 4 benchmarks concurrentes

---

## Anti-patterns a evitar

- **NO hacer `execute_query` async**: evita cambios en cascada al executor. Usar `std::sync::Mutex` para `CommitCoordinator::pending` para mantener `register_pending()` síncrono.
- **NO avanzar `max_committed` antes de fsync**: si el fsync falla, la txn no puede ser visible. Avanzar prematuramente rompe el invariante de durabilidad.
- **NO holdear el Database lock durante el await del CommitRx**: la lock se suelta en handler.rs ANTES de `rx.await`. Holdear la lock durante el await bloquearía el server entero.
- **NO usar `unwrap()` en production paths del coordinator**: si el receiver se dropea (cliente desconectado), el `reply.send(...)` devuelve Err que se ignora silenciosamente con `let _ = ...`. Correcto.
- **NO crear un Tokio Mutex para `CommitCoordinator::pending`**: causaría que `register_pending()` necesite ser async, infectando `execute_query()`. `std::sync::Mutex` es correcto aquí porque solo se usa para push/drain (O(1), nunca contendido lentamente).
- **NO olvidar el caso de shutdown**: el background task debe terminar si el `Database` se dropea. Usar `Weak<Mutex<Database>>` en el task para detectar que la DB fue dropeada y salir del loop.

---

## Riesgos

| Riesgo | Mitigación |
|---|---|
| Cambio de firma en `execute_query` / `execute_stmt` rompe todos los call sites | Hay exactamente 3 en handler.rs + tests — rastrear con `cargo build` antes de seguir |
| Background task no termina al cerrar el server (leak de JoinHandle) | Usar `tokio::task::JoinHandle` guardado en `Database`; `Database::drop` aborta el task |
| `std::sync::Mutex` en `CommitCoordinator::pending` puede deadlock si se lockea dentro del Tokio runtime desde código síncrono | El Mutex solo se lockea en `register_pending` (dentro de execute_query, síncrono) y `drain_pending` (dentro del background task, con `.lock().unwrap()`). Ninguno de los dos mantiene el Mutex mientras hace await. No hay deadlock posible. |
| fsync failure deja el WAL en estado inconsistente para futuras txns | Después de loguear ERROR, el background task continúa. Futuras txns intentarán su propio fsync. Si el disco está realmente muerto, el process terminará con I/O error. Este comportamiento es correcto — no hay recovery automático de hardware failure en Phase 3. |
| Latencia adicional de `interval_ms` para conexiones de baja carga | Documentado en spec. Mitigable con `group_commit_interval_ms=0` por default y notas en docs. |
