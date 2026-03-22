# NexusDB — Motor de Base de Datos en Rust

## Fuente de verdad
El archivo `db.md` contiene el diseño completo: arquitectura, tipos, fases, crates y decisiones.
**Léelo antes de cualquier tarea.**

---

## Flujo de ingeniería obligatorio

```
/brainstorm → /spec-task → /plan-task → /implement-task → /review-task
```

Nunca saltar pasos. Specs antes que código. Planes antes que implementación.
Reviews antes de cerrar. Sin excepciones.

---

## /brainstorm — Explorar antes de proponer

1. Leer `db.md` sección de la fase actual
2. Leer archivos relevantes del codebase
3. Hacer preguntas al usuario ANTES de proponer:
   - ¿Comportamiento esperado exacto?
   - ¿Casos borde conocidos?
   - ¿Restricciones de rendimiento?
4. Proponer 2-3 enfoques con trade-offs (no el mejor solo)
5. Escribir sprint con dependencias si hay subtareas:

```
Sprint: [nombre fase]
├── Tarea 1: [descripción] — sin dependencias
├── Tarea 2: [descripción] — depende de Tarea 1
└── Tarea 3: [descripción] — depende de Tarea 1
```

**Output:** sprint + enfoque acordado.

---

## /spec-task — Requisitos antes de implementar

Guardar en `specs/fase-N/spec-nombre.md`:

```markdown
# Spec: [nombre]

## Qué construir (no cómo)
[comportamiento exacto]

## Inputs / Outputs
- Input: [tipos exactos]
- Output: [tipos exactos]
- Errores: [cuándo y cuáles]

## Casos de uso
1. [caso feliz]
2. [caso borde 1]
3. [caso borde 2]

## Criterios de aceptación
- [ ] [criterio verificable]

## Fuera del alcance
- [qué NO hace]

## Dependencias
- [qué debe existir antes]
```

El spec debe ser autocontenido — una sesión nueva debe entenderlo sin contexto extra.

---

## /plan-task — Plan técnico antes de codear

Guardar en `specs/fase-N/plan-nombre.md`:

```markdown
# Plan: [nombre]

## Archivos a crear/modificar
- `crates/dbyo-X/src/Y.rs` — [qué hace]

## Algoritmo / Estructura de datos
[pseudocódigo del enfoque]

## Fases de implementación
1. [paso concreto verificable]
2. [paso concreto verificable]

## Tests a escribir
- unit: [qué probar]
- integration: [qué probar end-to-end]
- bench: [qué medir]

## Antipatrones a evitar
- [NO hacer X porque Y]

## Riesgos
- [riesgo conocido → mitigación]
```

---

## /implement-task — Ejecutar fase por fase

Una fase a la vez. Al terminar cada fase — **protocolo de cierre obligatorio:**

```
1. cargo test --workspace pasa limpio
2. cargo clippy --workspace -- -D warnings sin errores
3. cargo fmt --check sin diferencias
4. Escribir docs/fase-N.md
5. Actualizar docs/progreso.md — marcar subfase con [x] ✅ y fase padre con 🔄
6. Actualizar memory/project_state.md
7. Actualizar memory/architecture.md
8. Actualizar memory/lessons.md si hubo aprendizajes
9. Commit con formato Conventional Commits
10. Confirmar al usuario
```

### Items no implementados — regla anti-gaps

Si algo no se puede implementar completamente (scope, limitación técnica, dependencia futura):

**NO** dejarlo sin marcar. En su lugar:

1. En el spec, agregar sección:
   ```
   ## ⚠️ DEFERRED
   - [descripción del gap] → pendiente en subfase X.Y
   ```

2. En `docs/progreso.md`, agregar línea bajo la subfase:
   ```
   - [ ] ⚠️ [descripción corta] — gap identificado, retomar en [subfase]
   ```

**Por qué:** Los gaps silenciosos se convierten en bugs o trabajo duplicado en fases futuras.
El objetivo es tener visibilidad total del estado real del proyecto en todo momento.

### Principio de implementación máxima

Siempre implementar la versión más completa y correcta posible, sin importar la complejidad.
No simplificar por comodidad. Si algo es difícil, encontrar la forma correcta de hacerlo.
Solo marcar como DEFERRED cuando existe una **dependencia real** de otra fase o una
limitación externa documentada — nunca por complejidad o conveniencia.

**El contexto se compacta.** Escribir como si el lector no estuvo en esta sesión.

### Formato de commit

```
feat(fase-N): descripción concisa

- detalle 1
- detalle 2

Fase N/34 completada. Ver docs/fase-N.md
Spec: specs/fase-N/ | Tests: crates/dbyo-X/tests/
```

### Cuenta de GitHub

Siempre usar la cuenta **lordmacu** (personal, NO la cuenta del trabajo).
El repo está configurado con:
- `user.name = lordmacu`
- `user.email = lordmacu@users.noreply.github.com`
- GitHub CLI autenticado como lordmacu

No incluir Co-Authored-By de Claude en ningún commit de este proyecto.

### Ramas git

```
main              → código estable
fase-N-nombre     → desarrollo de la fase
hotfix/nombre     → fixes urgentes sobre main
```

Nunca pushear directamente a main. Merge solo después de /review-task completo.

---

## /review-task — Auditar antes de cerrar

**OBLIGATORIO antes de cerrar cualquier fase.** No se puede hacer commit de cierre sin pasar esta revisión completa.

### Paso 1 — Revisión por subagente Explore

Lanzar un agente Explore con instrucciones de revisar:

1. **Cada criterio de aceptación** de todos los specs de la fase — marcar ✅/❌
2. **`unwrap()` en código de producción** (`src/`, excluyendo `#[cfg(test)]`) → bloqueante si existe
3. **`unsafe` sin comentario `SAFETY:`** → bloqueante si existe
4. **Tests de integración** en `tests/` — ¿existen? → bloqueante si no
5. **Benchmarks** en `benches/` — ¿compilan? → bloqueante si no
6. **Lógica de los tests** — ¿los assertions son correctos? ¿hay tests que siempre pasan sin verificar nada real?
7. **Gaps no identificados** — ¿hay funcionalidad prometida en el spec que no está implementada?

El subagente debe retornar un reporte con:
- Lista de criterios cumplidos/no cumplidos por subfase
- Lista de blockers encontrados (con archivo:línea)
- Lista de gaps o deferred items

### Paso 2 — Fixes de blockers

Corregir **todos** los blockers antes de continuar. No hay excepciones.

### Paso 3 — Checklist de cierre
   ```
   [ ] Todos los criterios de aceptación de todos los specs ✅
   [ ] cargo test --workspace ✅
   [ ] cargo clippy -- -D warnings ✅
   [ ] cargo fmt --check ✅
   [ ] Sin unwrap() en src/ (solo en tests y benches) ✅
   [ ] Todo unsafe tiene comentario SAFETY: ✅
   [ ] Tests de integración en tests/ ✅
   [ ] Benchmarks compilan ✅
   [ ] Lógica de tests revisada (no assertions vacías) ✅
   [ ] docs/progreso.md actualizado ✅
   [ ] Commit hecho ✅
   ```

---

## /debug — Debugging sistemático

Cuando algo no funciona:

1. **Reproducir** con el test mínimo que demuestra el bug
2. **Formular hipótesis** — al menos 2, no asumir la primera
3. **Diseñar experimento** para validar/descartar cada hipótesis
4. **Fix en el lugar correcto** — no parchar síntomas
5. **Test de regresión** — que nunca vuelva

Nunca hacer cambios aleatorios esperando que funcione.

---

## /bench — Comparar rendimiento

Antes de cualquier optimización:

```bash
# 1. Baseline ANTES del cambio
cargo bench --bench [nombre] > /tmp/before.txt

# 2. Hacer el cambio

# 3. Medir DESPUÉS
cargo bench --bench [nombre] > /tmp/after.txt

# 4. Comparar
cargo install critcmp
critcmp /tmp/before.txt /tmp/after.txt
```

Si hay regresión > 5% en operación crítica: bloqueante.

### Presupuesto de rendimiento (no regresar)

| Operación             | Objetivo     | Máximo aceptable |
|-----------------------|--------------|------------------|
| Point lookup PK       | 800k ops/s   | 600k ops/s       |
| Range scan 10K rows   | 45ms         | 60ms             |
| INSERT con WAL        | 180k ops/s   | 150k ops/s       |
| Seq scan 1M rows      | 0.8s         | 1.2s             |
| Concurrent reads x16  | lineal       | <2x degradación  |

---

## /unsafe-review — Auditar bloques unsafe

Para cada bloque `unsafe` en el código:

```
1. ¿Por qué es necesario? ¿Existe alternativa safe?
   → Intentar bytemuck, rkyv, o restructurar primero

2. ¿Qué invariante garantiza que es seguro?
   → Documentar con comentario SAFETY:

3. ¿Hay test que verifica el contrato?
   → Si no hay: escribirlo antes de mergear

4. ¿Está encapsulado en función safe pública?
   → El caller no debería ver unsafe
```

```rust
// Formato obligatorio:
// SAFETY: [invariante que garantiza que este código es seguro]
// Específicamente: [qué condición debe cumplirse]
let page = unsafe { &*(ptr as *const Page) };
```

---

## /new-crate — Agregar crate al workspace

```bash
# 1. Crear estructura
cargo new --lib crates/dbyo-X

# 2. Agregar al workspace en Cargo.toml raíz
# members = [..., "crates/dbyo-X"]

# 3. Definir SOLO tipos y traits públicos en src/lib.rs

# 4. Escribir test inicial (vacío pero compilando)

# 5. Actualizar memory/architecture.md
```

Verificar que no hay dependencia circular:
```bash
cargo tree --workspace | grep "dbyo-X"
```

---

## /profile — Encontrar cuellos de botella reales

```bash
# Instalar herramientas una vez
cargo install flamegraph
cargo install cargo-samply

# Perfil con flamegraph
cargo flamegraph --bench [nombre_bench]
open flamegraph.svg

# O con samply (mejor en macOS)
cargo samply record cargo bench --bench [nombre]
```

Proceso:
1. Benchmark que reproduce el caso lento
2. Flamegraph → función más costosa
3. Optimizar SOLO esa función
4. Verificar que el benchmark mejora
5. Verificar que nada más regresó

---

## /fuzz — Testing con entradas aleatorias

Crítico para el parser SQL y el storage engine:

```bash
# Instalar cargo-fuzz
cargo install cargo-fuzz

# Crear target de fuzz
cargo fuzz add fuzz_sql_parser
cargo fuzz add fuzz_storage_pages
cargo fuzz add fuzz_wal_recovery

# Correr (mínimo 60 segundos en CI, más en local)
cargo fuzz run fuzz_sql_parser -- -max_total_time=300

# Por cada crash encontrado:
# 1. Agregar como test de regresión en tests/
# 2. Fixear el bug
# 3. Verificar que el test pasa
```

---

## /checkpoint — Guardar contexto al pausar

Cuando hay que pausar en medio de una tarea:

```bash
# Crear checkpoint
cat > docs/checkpoint-$(date +%Y%m%d).md << 'EOF'
# Checkpoint [fecha]

## Qué se estaba haciendo
[descripción exacta]

## Decisión pendiente
[qué había que decidir]

## Próximo paso exacto
[instrucción precisa para continuar]

## Archivos modificados hasta ahora
[lista]

## Tests que fallan / pasan
[estado actual]
EOF

git add -A && git commit -m "checkpoint: pausar en [descripción]"
```

La próxima sesión: leer el checkpoint antes de todo.

---

## /invariant — Verificar invariantes de la BD

Después de operaciones complejas, verificar que el motor está consistente:

```rust
// Invariantes que siempre deben cumplirse:

// 1. B+ Tree balanceado
assert!(btree.all_leaves_same_depth());

// 2. Free list sin duplicados
assert!(free_list.has_no_duplicates());

// 3. Ninguna página referenciada dos veces
assert!(page_refs.no_double_references());

// 4. WAL LSN siempre creciente
assert!(wal.is_monotonic());

// 5. Todas las FK apuntan a filas existentes
assert!(fk_checker.all_valid());

// 6. Checksum de cada página es correcto
assert!(storage.all_checksums_valid());
```

```bash
# Comando SQL para verificar
SELECT * FROM db_integrity_check();
-- Retorna: OK o lista de violaciones
```

---

## Convenciones de código Rust

### Errores — siempre thiserror

```rust
// CORRECTO
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("página {page_id} no encontrada")]
    PageNotFound { page_id: u64 },

    #[error("checksum inválido en página {page_id}: esperado {expected}, obtenido {got}")]
    ChecksumMismatch { page_id: u64, expected: u32, got: u32 },
}

// PROHIBIDO en src/ — solo en tests y benches
let page = storage.read_page(42).unwrap();

// CORRECTO
let page = storage.read_page(42)?;
let page = storage.read_page(42).map_err(|e| DbError::StorageError(e))?;
```

### Async vs sync

```rust
// I/O y conexiones de red → tokio async
async fn handle_connection(stream: TcpStream) -> Result<()> { ... }

// CPU intensivo → rayon (NO bloquear el runtime de tokio)
fn parallel_scan(table: &Table) -> Vec<Row> {
    table.morsels(100_000).par_iter().flat_map(|m| scan(m)).collect()
}

// Llamar rayon desde tokio:
let result = tokio::task::spawn_blocking(|| parallel_scan(table)).await?;
```

### Unsafe — siempre con SAFETY

```rust
// CORRECTO
// SAFETY: `ptr` apunta a una página válida dentro del mmap.
// El mmap vive mientras `StorageEngine` existe (garantizado por Arc).
// page_id < total_pages verificado antes de esta llamada.
let page = unsafe { &*(ptr as *const Page) };

// PROHIBIDO — unsafe sin justificación
let page = unsafe { &*(ptr as *const Page) };
```

### Testing pyramid

```rust
// Unit: rápido, sin I/O, usar MemoryStorage
#[test]
fn test_btree_insert() {
    let storage = MemoryStorage::new();
    let tree = BTree::new(storage);
    tree.insert(b"key1", RecordId(1)).unwrap();
    assert_eq!(tree.lookup(b"key1").unwrap(), Some(RecordId(1)));
}

// Integration: I/O real, crash recovery
#[test]
fn test_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    // Escribir datos
    let engine = Engine::open(dir.path()).unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    drop(engine); // simular crash

    // Releer — debe recuperarse
    let engine = Engine::open(dir.path()).unwrap();
    let rows = engine.execute("SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);
}

// Bench: medir, no verificar
#[bench]
fn bench_point_lookup(b: &mut Bencher) {
    let engine = setup_bench_engine();
    b.iter(|| engine.execute("SELECT * FROM users WHERE id = 42"))
}
```

---

## Estructura del proyecto

```
dbyo/
├── CLAUDE.md              ← este archivo (flujo de trabajo)
├── db.md                  ← diseño completo (fuente de verdad)
├── Cargo.toml             ← workspace root
├── .claude/
│   └── settings.json      ← hooks del proyecto (fmt + clippy)
├── specs/                 ← specs y planes por fase
│   └── fase-01/
│       ├── spec-storage.md
│       └── plan-storage.md
├── docs/                  ← documentación de lo implementado
│   ├── README.md
│   └── fase-01.md         ← se crea al completar la fase
├── crates/                ← código
│   ├── dbyo-core/
│   ├── dbyo-storage/
│   └── ...
├── tests/                 ← integration tests
├── benches/               ← benchmarks con criterion
└── fuzz/                  ← cargo-fuzz targets
```

---

## Protocolo de memoria — actualizar al completar cada fase

Archivos en `.claude/projects/-Users-cristian-dbyo/memory/`:

| Archivo | Cuándo actualizar |
|---|---|
| `project_state.md` | Siempre al cerrar una fase |
| `architecture.md` | Cuando se crea o modifica un crate |
| `decisions.md` | Cuando se toma una decisión técnica importante |
| `lessons.md` | Cuando algo sorpresivo ocurre (bien o mal) |

---

## Antes de cada sesión

```
1. leer docs/checkpoint-*.md si existe (continuar desde ahí)
2. leer docs/fase-N.md de la última fase completada
3. leer specs/fase-actual/ para recordar el spec
4. correr: cargo test --workspace
5. continuar desde donde se dejó
```
