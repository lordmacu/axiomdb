# /new-crate — Scaffoldear crate en el workspace

## Proceso completo

### 1. Crear el crate

```bash
cd /Users/cristian/dbyo
cargo new --lib crates/dbyo-NOMBRE
```

### 2. Estructura estándar del crate

```
crates/dbyo-NOMBRE/
├── Cargo.toml
├── src/
│   ├── lib.rs          ← solo re-exports públicos y doc del crate
│   ├── error.rs        ← DbError con thiserror
│   └── [módulos].rs
└── tests/
    └── integration.rs  ← tests de integración
```

### 3. Cargo.toml del crate nuevo

```toml
[package]
name    = "dbyo-NOMBRE"
version = "0.1.0"
edition = "2021"

[dependencies]
# Usar versiones del workspace cuando estén definidas
dbyo-core = { path = "../dbyo-core" }
thiserror = { workspace = true }
tracing   = { workspace = true }

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
tempfile  = "3"
```

### 4. src/lib.rs mínimo

```rust
//! # dbyo-NOMBRE
//!
//! [Descripción de una línea de qué hace este crate]
//!
//! ## Ejemplo
//! ```rust
//! use dbyo_nombre::MiTrait;
//! // ejemplo mínimo
//! ```

mod error;
pub use error::Error;

// Traits públicos primero
pub trait MiTrait: Send + Sync {
    fn operacion(&self) -> Result<(), Error>;
}
```

### 5. Agregar al workspace Cargo.toml raíz

```toml
[workspace]
members = [
    # ... existentes ...
    "crates/dbyo-NOMBRE",   # ← agregar aquí en orden alfabético
]
```

### 6. Verificar que no hay ciclo de dependencias

```bash
cargo tree --workspace 2>&1 | grep -E "dbyo-NOMBRE|error\[E"
```

### 7. Test inicial que compile

```rust
// tests/integration.rs
use dbyo_nombre::MiTrait;

#[test]
fn crate_compila_y_trait_existe() {
    // Solo verificar que compila por ahora
    // Los tests reales se agregan en la fase de implementación
}
```

```bash
cargo test -p dbyo-NOMBRE
```

### 8. Actualizar memory/architecture.md

```markdown
## Crates implementados
- `dbyo-NOMBRE` — [descripción de una línea] (Fase N)
```

### Checklist

```
[ ] cargo new --lib ejecutado
[ ] Cargo.toml del crate con dependencias correctas
[ ] Agregado a workspace members
[ ] src/lib.rs con traits públicos documentados
[ ] Test inicial compilando
[ ] cargo test -p dbyo-NOMBRE pasa
[ ] cargo clippy -p dbyo-NOMBRE -- -D warnings pasa
[ ] architecture.md actualizado
```
