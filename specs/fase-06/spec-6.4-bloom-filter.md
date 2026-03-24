# Spec: Bloom Filter por índice (Subfase 6.4)

## What to build (not how)

Un Bloom filter in-memory por cada índice secundario que permite al executor
descartar point lookups (`WHERE col = ?`) sin leer ninguna página del B-Tree
cuando la clave definitivamente no existe.

El filtro es puramente una optimización de rendimiento — nunca afecta la
corrección de los resultados.

---

## Comportamiento exacto

### Durante CREATE INDEX
Al construir el B-Tree desde la tabla, también poblar el Bloom filter con
cada clave indexada.

### Durante INSERT
Después de insertar en el B-Tree secundario, agregar la clave al filtro.

### Durante DELETE / UPDATE
No modificar el filtro (las claves borradas permanecen como posibles
false positives). El filtro puede volverse subóptimo pero nunca incorrecto:
- "definitivamente no existe" = verdad absoluta (sin false negatives)
- "posiblemente existe" = va al B-Tree → si no lo encuentra, correcto
- Estrategia: marcar el filtro como `dirty` para reconstruirlo en `ANALYZE TABLE` (futuro)

### Durante SELECT con IndexLookup
Antes de llamar `BTree::lookup_in`, consultar el filtro:
```
might_exist = bloom_registry.check(index_id, &encoded_key)
if !might_exist → return empty immediately (0 page reads)
if  might_exist → proceed with B-Tree lookup (normal path)
```

### Durante DROP INDEX
Eliminar la entrada del filtro del BloomRegistry.

---

## Tipos e interfaces

### `BloomRegistry` (nuevo, en `axiomdb-sql/src/bloom.rs`)

```rust
/// Per-database registry of Bloom filters, one per secondary index.
///
/// Shared across all connections via `&mut BloomRegistry` threaded through
/// the executor. Protected by the same `Mutex<Database>` that serializes
/// all writes in Phase 6.
pub struct BloomRegistry {
    filters: HashMap<u32, IndexBloom>,  // keyed by index_id
}

/// Bloom filter for one index.
struct IndexBloom {
    filter: bloomfilter::Bloom<Vec<u8>>,
    /// Number of keys currently in the filter (used for sizing on rebuild).
    item_count: usize,
    /// True if DELETE/UPDATE has made the filter potentially stale.
    dirty: bool,
}

impl BloomRegistry {
    pub fn new() -> Self;

    /// Creates a new filter for `index_id` sized for `expected_items`.
    /// Called at CREATE INDEX time.
    pub fn create(&mut self, index_id: u32, expected_items: usize);

    /// Adds `key` to the filter for `index_id`.
    /// Called after every successful B-Tree insert.
    pub fn add(&mut self, index_id: u32, key: &[u8]);

    /// Returns `true` if `key` MIGHT exist; `false` if it DEFINITELY does not.
    /// Returns `true` (conservative) if the filter for `index_id` does not exist.
    pub fn might_exist(&self, index_id: u32, key: &[u8]) -> bool;

    /// Marks the filter for `index_id` as dirty (stale due to deletes).
    pub fn mark_dirty(&mut self, index_id: u32);

    /// Removes the filter for `index_id`. Called at DROP INDEX.
    pub fn remove(&mut self, index_id: u32);

    /// Returns the number of filters currently in the registry.
    pub fn len(&self) -> usize;

    pub fn is_empty(&self) -> bool;
}
```

### Cambio en `execute_with_ctx`

```rust
// Before:
pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>

// After:
pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,   // ← nuevo
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

Los call sites a actualizar:
1. `axiomdb-network/src/mysql/database.rs:68` — `execute_query`
2. `axiomdb-network/src/mysql/database.rs:78` — `execute_stmt`
3. `axiomdb-sql/src/executor.rs` — llamadas recursivas internas para subqueries

### Cambio en `Database`

```rust
pub struct Database {
    pub storage: MmapStorage,
    pub txn: TxnManager,
    pub bloom: BloomRegistry,   // ← nuevo
}
```

---

## Parámetros del filtro

| Parámetro | Valor | Razón |
|-----------|-------|-------|
| Bits por clave | ~9.6 (ceil) → 10 | 1% FPR por defecto |
| FPR objetivo | 1% | Equilibrio memoria/rendimiento |
| Hash functions | 7 (derivado de FPR) | `bloomfilter` lo calcula automáticamente |
| Tipo de clave | `Vec<u8>` | Claves ya codificadas por `encode_index_key` |
| Crate | `bloomfilter = "3"` | Versión actual en crates.io |

Sizing: `Bloom::new_for_fp_rate(expected_items, 0.01)` — el crate calcula el número
óptimo de bits y funciones de hash automáticamente.

Para `expected_items` usamos el row count del table scan durante `CREATE INDEX`.
Margen de crecimiento: `max(row_count * 2, 1000)` para evitar resize inmediato.

---

## Inputs / Outputs

- Input: `encoded_key: &[u8]` (output de `encode_index_key`)
- Output: `might_exist: bool`
- Errores: ninguno — el filtro devuelve `true` (conservador) ante cualquier duda

---

## Use cases

1. **Happy path — miss eliminado**:
   ```sql
   CREATE INDEX users_email_idx ON users (email);
   SELECT * FROM users WHERE email = 'noexiste@ejemplo.com';
   -- Bloom: "definitely not" → 0 B-Tree pages read → empty result
   ```

2. **Happy path — hit normal**:
   ```sql
   SELECT * FROM users WHERE email = 'alice@ejemplo.com';
   -- Bloom: "might exist" → B-Tree lookup → found → row returned
   ```

3. **Happy path — false positive (correcto)**:
   ```sql
   -- email 'ghost@ejemplo.com' no existe pero está en el filter (false positive)
   SELECT * FROM users WHERE email = 'ghost@ejemplo.com';
   -- Bloom: "might exist" → B-Tree lookup → NOT FOUND → empty result
   -- Resultado: correcto (vacío), solo 1 B-Tree traversal extra
   ```

4. **Edge case — clave eliminada (stale)**:
   ```sql
   DELETE FROM users WHERE email = 'alice@ejemplo.com';
   SELECT * FROM users WHERE email = 'alice@ejemplo.com';
   -- Bloom: "might exist" (stale) → B-Tree lookup → NOT FOUND → empty
   -- Resultado: correcto. El filter pierde 1 oportunidad de skip.
   ```

5. **Edge case — índice sin filtro (pre-6.4)**:
   ```sql
   -- Índice creado antes de Phase 6.4 → no tiene entrada en BloomRegistry
   -- might_exist devuelve true (conservative) → B-Tree lookup normal
   ```

6. **Edge case — NULL key**:
   Los NULLs no se insertan en el B-Tree secundario (Phase 6.2). No se agregan
   al Bloom filter tampoco. `might_exist` con encoded(NULL) devuelve false →
   optimización adicional: queries `WHERE col = NULL` se saltan el B-Tree.

---

## Acceptance criteria

- [ ] `BloomRegistry` implementado con `new`, `create`, `add`, `might_exist`, `mark_dirty`, `remove`
- [ ] `BloomRegistry::might_exist` devuelve `true` (conservador) para `index_id` desconocido
- [ ] `execute_with_ctx` acepta `bloom: &mut BloomRegistry` como nuevo parámetro
- [ ] `Database` struct tiene campo `bloom: BloomRegistry`
- [ ] `Database::execute_query` y `Database::execute_stmt` pasan `&mut self.bloom`
- [ ] Llamadas recursivas internas del executor pasan el mismo `bloom`
- [ ] `execute_create_index` puebla el filtro durante el table scan
- [ ] `index_maintenance::insert_into_indexes` llama `bloom.add` después de B-Tree insert
- [ ] `index_maintenance::delete_from_indexes` llama `bloom.mark_dirty`
- [ ] `execute_select` (IndexLookup path) llama `bloom.might_exist` antes de `BTree::lookup_in`
- [ ] `execute_drop_index` llama `bloom.remove`
- [ ] Test: CREATE INDEX + SELECT WHERE (miss) → bloom elimina B-Tree reads (verificar con row count = 0 en result)
- [ ] Test: CREATE INDEX + SELECT WHERE (hit) → resultado correcto con bloom activo
- [ ] Test: INSERT después de CREATE INDEX → nueva clave es encontrable via bloom
- [ ] Test: DELETE + SELECT → resultado correcto (stale pero correcto)
- [ ] Test: DROP INDEX → `might_exist` devuelve true (conservative) después del drop
- [ ] Test: índice sin filter en registry → `might_exist` devuelve true, no panic
- [ ] No `unwrap()` en `src/` (solo en tests)
- [ ] `cargo test --workspace` pasa limpio

## Out of scope

- Persistir el filtro a disco (siempre se reconstruye en runtime o en `CREATE INDEX`)
- `ANALYZE TABLE` para reconstruir filtros dirty (Phase 6.12)
- Reconstruir el filtro desde el B-Tree en startup del servidor
- Bloom filter para el índice primario (no indexamos por PK en B-Tree secundario)
- Counting Bloom / Cuckoo filter (supuesto: workload no es DELETE-heavy extremo)

## Dependencies

- `bloomfilter = "3"` (nuevo crate, agregar a workspace y a `axiomdb-sql/Cargo.toml`)
- Subfase 6.2b completada (`insert_into_indexes`, `delete_from_indexes` existen)
- Subfase 6.3 completada (planner produce `AccessMethod::IndexLookup`)
