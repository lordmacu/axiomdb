# Plan: 5.13 — Prepared Statement Plan Cache

## Files to modify

| Archivo | Cambio |
|---|---|
| `crates/axiomdb-storage/src/config.rs` | Añade `max_prepared_stmts_per_connection: usize` (default 1024) |
| `crates/axiomdb-network/src/mysql/database.rs` | Añade `schema_version: Arc<AtomicU64>`; incrementa en DDL |
| `crates/axiomdb-network/src/mysql/session.rs` | Añade `compiled_at_version: u64`, `last_used_seq: u64` a `PreparedStatement`; LRU eviction en `prepare_statement()` |
| `crates/axiomdb-network/src/mysql/handler.rs` | Clona `Arc<AtomicU64>` al inicio; check de versión + re-análisis en COM_STMT_EXECUTE |

---

## Algoritmo

### 1. `schema_version` en Database

```rust
// database.rs
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    pub coordinator: Option<CommitCoordinator>,
    /// Global monotonic counter. Incremented after every successful DDL.
    /// Connections clone this Arc at connect time and poll it lock-free.
    pub schema_version: Arc<AtomicU64>,
}
```

Helper en database.rs:

```rust
/// Returns true if `stmt` is a DDL statement that changes the schema.
fn is_schema_changing(stmt: &axiomdb_sql::ast::Stmt) -> bool {
    use axiomdb_sql::ast::Stmt::*;
    matches!(
        stmt,
        CreateTable(_) | DropTable(_) | AlterTable(_)
        | CreateIndex(_) | DropIndex(_) | TruncateTable(_)
    )
}
```

En `execute_query()` y `execute_stmt()`, después de `execute_with_ctx` exitoso:

```rust
// execute_query:
let stmt = parse(sql, None)?;
let ddl = is_schema_changing(&stmt);
// ... analyze + execute ...
let result = execute_with_ctx(analyzed, ...)?;
if ddl {
    self.schema_version.fetch_add(1, Ordering::Release);
}
Ok((result, self.take_commit_rx()))

// execute_stmt:
let ddl = is_schema_changing(&stmt);
let result = execute_with_ctx(stmt, ...)?;
if ddl {
    self.schema_version.fetch_add(1, Ordering::Release);
}
Ok((result, self.take_commit_rx()))
```

---

### 2. `PreparedStatement` — nuevos campos

```rust
// session.rs
pub struct PreparedStatement {
    pub stmt_id: u32,
    pub sql_template: String,
    pub param_count: u16,
    pub param_types: Vec<u16>,
    pub analyzed_stmt: Option<axiomdb_sql::ast::Stmt>,
    /// schema_version snapshot at PREPARE time (or last successful re-analyze).
    /// If this differs from Database.schema_version, the plan is stale.
    pub compiled_at_version: u64,
    /// Logical clock for LRU eviction. Set to ConnectionState.execute_seq
    /// on each use. The stmt with the lowest value is evicted first.
    pub last_used_seq: u64,
}
```

---

### 3. LRU eviction en `prepare_statement()`

```rust
// ConnectionState:
pub struct ConnectionState {
    // ... existing fields ...
    pub max_prepared_stmts: usize,   // from DbConfig at connect time
    execute_seq: u64,                // incremented on each EXECUTE
}

impl ConnectionState {
    pub fn prepare_statement(&mut self, sql: String, version: u64) -> (u32, u16) {
        // Evict LRU entry if at capacity
        if self.prepared_statements.len() >= self.max_prepared_stmts {
            let lru_id = self.prepared_statements
                .iter()
                .min_by_key(|(_, ps)| ps.last_used_seq)
                .map(|(id, _)| *id);
            if let Some(id) = lru_id {
                self.prepared_statements.remove(&id);
            }
        }

        let param_count = count_params(&sql);
        let stmt_id = self.next_stmt_id;
        self.next_stmt_id = self.next_stmt_id.wrapping_add(1).max(1);
        if self.next_stmt_id == 0 { self.next_stmt_id = 1; }

        self.prepared_statements.insert(stmt_id, PreparedStatement {
            stmt_id,
            sql_template: sql,
            param_count,
            param_types: vec![],
            analyzed_stmt: None,
            compiled_at_version: version,  // snapshot of current schema
            last_used_seq: 0,
        });
        (stmt_id, param_count)
    }

    pub fn next_execute_seq(&mut self) -> u64 {
        self.execute_seq += 1;
        self.execute_seq
    }
}
```

---

### 4. handler.rs — clone Arc + version check en COM_STMT_EXECUTE

Al inicio de `handle_connection`, después de autenticación:

```rust
// Clone Arc<AtomicU64> once per connection — no lock needed to read it later.
let schema_version: Arc<AtomicU64> = {
    let guard = db.lock().await;
    Arc::clone(&guard.schema_version)
};
let mut conn_state = ConnectionState::new_with_limit(config.max_prepared_stmts_per_connection);
```

En COM_STMT_PREPARE (0x16), pasar version al prepare:

```rust
let current_version = schema_version.load(Ordering::Acquire);
let (stmt_id, param_count) = conn_state.prepare_statement(sql.clone(), current_version);
// Store analyzed_stmt with compiled_at_version = current_version
if let Some(ps) = conn_state.prepared_statements.get_mut(&stmt_id) {
    ps.analyzed_stmt = analyzed_stmt;
    ps.compiled_at_version = current_version;
}
```

En COM_STMT_EXECUTE (0x17), versión check antes del fast path:

```rust
// BEFORE: if let Some(cached) = stmt.analyzed_stmt.clone() {
// AFTER:

let current_version = schema_version.load(Ordering::Acquire);

// Revalidate or re-analyze if plan is stale.
if stmt.compiled_at_version != current_version || stmt.analyzed_stmt.is_none() {
    debug!(conn_id, stmt_id, "plan stale (schema changed), re-analyzing");
    let (new_plan, _cols) = {
        let guard = db.lock().await;
        let snap = guard.txn.active_snapshot().unwrap_or_else(|_| guard.txn.snapshot());
        match axiomdb_sql::parse(&stmt.sql_template, None)
            .and_then(|s| axiomdb_sql::analyze(s, &guard.storage, snap))
        {
            Ok(analyzed) => (Some(analyzed), extract_result_columns(&analyzed_copy)),
            Err(_) => (None, vec![]),
        }
    };
    stmt.analyzed_stmt = new_plan;
    stmt.compiled_at_version = current_version; // update even on error
}

// Mark as recently used (for LRU)
stmt.last_used_seq = conn_state.next_execute_seq();

// Proceed with existing fast path / fallback
if let Some(cached) = stmt.analyzed_stmt.clone() {
    // ... substitute_params_in_ast + execute_stmt (unchanged)
} else {
    // fallback: string substitution path (unchanged)
}
```

---

### 5. config.rs

```rust
/// Maximum number of prepared statements cached per connection.
/// When the limit is reached, the least-recently-used statement is evicted.
/// Default: 1024.
#[serde(default = "default_max_prepared_stmts")]
pub max_prepared_stmts_per_connection: usize,

fn default_max_prepared_stmts() -> usize { 1024 }
```

---

## Fases de implementación

### Fase 1 — config.rs (5 min)
Añadir campo `max_prepared_stmts_per_connection`.

### Fase 2 — database.rs (15 min)
1. Añadir `schema_version: Arc<AtomicU64>` al struct y a `open()`
2. Añadir `is_schema_changing(stmt)` helper
3. En `execute_query`: detectar DDL, incrementar después de éxito
4. En `execute_stmt`: igual
5. `cargo build` limpio

### Fase 3 — session.rs (20 min)
1. Añadir `compiled_at_version` + `last_used_seq` a `PreparedStatement`
2. Añadir `max_prepared_stmts` + `execute_seq` a `ConnectionState`
3. Modificar `prepare_statement()`: LRU eviction + pasar `version`
4. Añadir `next_execute_seq()`
5. `cargo test -p axiomdb-network` para verificar tests de session

### Fase 4 — handler.rs (20 min)
1. Clonar `Arc<AtomicU64>` al inicio de `handle_connection`
2. En COM_STMT_PREPARE: pasar `current_version` a `prepare_statement()`
3. En COM_STMT_EXECUTE: añadir version check + re-analysis antes del fast path
4. Actualizar `last_used_seq` en cada EXECUTE
5. `cargo build` limpio

### Fase 5 — Tests (20 min)
Añadir en `crates/axiomdb-network/tests/` o en session.rs:

```
test_schema_version_starts_at_zero
test_schema_version_increments_on_create_table
test_schema_version_increments_on_drop_table
test_schema_version_increments_on_alter_table
test_schema_version_no_increment_on_dml
test_lru_eviction_removes_oldest_stmt
test_lru_eviction_keeps_recently_used
test_compiled_at_version_set_at_prepare
```

---

## Tests a escribir

### Unit tests (session.rs)
- `test_prepare_statement_sets_compiled_at_version` — version en el stmt = version pasada
- `test_lru_evict_at_limit` — al llegar al límite, el stmt con menor `last_used_seq` desaparece
- `test_lru_keep_recently_used` — el stmt usado más recientemente sobrevive a la evicción
- `test_execute_seq_increments` — `next_execute_seq()` es monótonico

### Unit tests (database.rs)
- `test_schema_version_zero_on_open` — arranca en 0
- `test_schema_version_increments_after_create_table` — CREATE TABLE → version = 1
- `test_schema_version_increments_after_drop` — DROP TABLE → version = 2
- `test_schema_version_no_increment_on_insert` — INSERT → versión sin cambios
- `test_schema_version_no_increment_on_failed_ddl` — DDL que falla no incrementa

### Integration test (handler path — simulado sin TCP)
- `test_stale_plan_reanalyzed_after_alter_table` — via Database + handler logic directo:
  1. PREPARE stmt = 'SELECT id, name FROM t WHERE id=?'
  2. ALTER TABLE t DROP COLUMN name → schema_version++
  3. simular COM_STMT_EXECUTE → plan es stale → re-analyze → ColumnNotFound

---

## Anti-patterns a evitar

- **NO incrementar schema_version en DDL fallido** — solo si `execute_with_ctx` retorna `Ok`
- **NO leer schema_version bajo el Database lock** — es `Arc<AtomicU64>`, leer sin lock con `Ordering::Acquire`
- **NO olvidar actualizar `compiled_at_version` en el stmt tras re-análisis fallido** — si el re-análisis falla, actualizar de todas formas para no re-analizar infinitamente en cada execute (el error ya se mandó al cliente)
- **NO hacer LRU eviction con sort O(N log N)** — usar `min_by_key` O(N) que es suficiente para 1024 entries
- **NO añadir `schema_version` como parámetro al executor** — solo en database.rs; el executor no debe saber del cache

---

## Risks

| Riesgo | Mitigación |
|---|---|
| `execute_stmt` en handler recibe un Stmt ya analizado sin saber si es DDL | En `execute_stmt` en database.rs recibe el `Stmt` analizado — `is_schema_changing` puede verificarlo igual (mismo enum) |
| Re-análisis concurrente: connection A re-analiza mientras connection B hace DDL | `schema_version` es atómico; si B incremente durante el re-análisis de A, en el siguiente EXECUTE de A se detectará el cambio y re-analizará de nuevo — correcto |
| `extract_result_columns` necesita el `analyzed` pero se consume en el match | Clonar el analyzed antes del match o capturar columnas antes de mover — cuidado con borrow checker |
