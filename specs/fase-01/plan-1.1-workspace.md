# Plan: 1.1 — Workspace Setup

## Pasos de implementación

1. `rust-toolchain.toml` — fijar Rust stable 1.80
2. `Cargo.toml` workspace root — todos los crates + deps compartidas + perfiles
3. 19 crates con `cargo new --lib` — estructura mínima cada uno
4. `nexusdb-core` — definir `DbError` con thiserror (única dep real)
5. `.rustfmt.toml` y `.clippy.toml` — estilo uniforme
6. `.github/workflows/ci.yml` — test + clippy + fmt
7. Verificar: `cargo build --workspace` limpio

## Decisiones
- `thiserror` como única dependencia real en esta subfase
- Todos los demás crates dependen de `nexusdb-core`
- `nexusdb-server` es `[[bin]]`, `nexusdb-embedded` es `[lib] crate-type = ["cdylib","rlib"]`
