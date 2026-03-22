# /unsafe-review — Auditar bloques unsafe

## Encontrar todos los bloques unsafe

```bash
# Listar todos los unsafe en el proyecto
grep -rn "unsafe" crates/ --include="*.rs" | grep -v "// SAFETY:"

# Ver contexto completo de cada unsafe
grep -rn -A 5 -B 2 "unsafe {" crates/ --include="*.rs"
```

## Para cada bloque unsafe, responder estas preguntas

### 1. ¿Es realmente necesario?

Intentar alternativas safe primero:

```rust
// ¿Puede resolverse con bytemuck?
use bytemuck::{Pod, Zeroable};
#[repr(C)]
#[derive(Pod, Zeroable, Clone, Copy)]
struct Page { ... }
let page: &Page = bytemuck::from_bytes(&bytes);  // safe!

// ¿Puede resolverse con rkyv?
use rkyv::{Archive, Deserialize};
let archived = unsafe { rkyv::access_unchecked::<Page>(&bytes) };
// rkyv hace el unsafe internamente con invariantes garantizados

// ¿Puede restructurarse para evitar el raw pointer?
```

### 2. ¿Qué invariante garantiza que es seguro?

El comentario SAFETY debe ser específico, no genérico:

```rust
// ❌ MAL — demasiado vago
// SAFETY: es seguro

// ❌ MAL — no explica el invariante
// SAFETY: confiamos en que el puntero es válido

// ✅ BIEN — específico y verificable
// SAFETY: `ptr` es válido porque:
//   1. Se obtiene de `mmap.as_ptr()` que siempre retorna memoria válida
//   2. `page_id < self.total_pages` verificado en la línea 42
//   3. La alineación de Page (align=64) es compatible con el puntero del mmap
//   4. El mmap vive mientras `StorageEngine` existe (garantizado por Arc<Mmap>)
let page = unsafe { &*(ptr as *const Page) };
```

### 3. ¿Hay test que verifica el contrato?

```rust
#[test]
fn test_safety_invariant_mmap_pointer() {
    // Verificar que el unsafe es realmente seguro en los casos límite
    let storage = MmapStorage::create_temp();

    // Caso límite: última página válida
    let last_page = storage.total_pages() - 1;
    let result = storage.read_page(last_page);
    assert!(result.is_ok());

    // Verificar que falla apropiadamente fuera de rango
    let result = storage.read_page(storage.total_pages());
    assert!(matches!(result, Err(DbError::PageNotFound { .. })));
}
```

### 4. ¿Está encapsulado correctamente?

```rust
// ❌ MAL — unsafe expuesto al caller
pub fn get_page_ptr(id: u64) -> *const Page { ... }

// ✅ BIEN — unsafe encapsulado, interfaz pública es safe
pub fn read_page(&self, id: u64) -> Result<&Page, DbError> {
    if id >= self.total_pages {
        return Err(DbError::PageNotFound { page_id: id });
    }
    let ptr = self.mmap.as_ptr().add(id as usize * PAGE_SIZE);
    // SAFETY: [invariante completo aquí]
    Ok(unsafe { &*(ptr as *const Page) })
}
```

## Checklist por bloque unsafe

```
[ ] ¿Intenté bytemuck/rkyv/restructurar primero?
[ ] ¿El comentario SAFETY explica el invariante específico?
[ ] ¿El comentario menciona por qué cada condición se cumple?
[ ] ¿Hay test que verifica el contrato en casos límite?
[ ] ¿La función pública tiene firma safe aunque internamente use unsafe?
[ ] ¿Corrí miri sobre este código?
```

```bash
# Verificar con miri (detecta UB en unsafe)
cargo +nightly miri test nombre_del_test_unsafe
```
