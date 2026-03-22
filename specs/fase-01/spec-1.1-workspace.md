# Spec: 1.1 — Workspace Setup

## Qué construir
Estructura completa del workspace Rust con todos los crates declarados,
configuración de calidad de código y CI en GitHub Actions.

## Criterios de aceptación
- [ ] `cargo build --workspace` compila sin errores ni warnings
- [ ] `cargo test --workspace` pasa (sin tests reales aún, solo que compile)
- [ ] `cargo clippy --workspace -- -D warnings` pasa limpio
- [ ] `cargo fmt --check` pasa limpio
- [ ] GitHub Actions corre automáticamente en cada push a main

## Estructura a crear
```
nexusdb/
├── Cargo.toml              ← workspace root con todos los crates
├── rust-toolchain.toml     ← fijar versión de Rust
├── .rustfmt.toml           ← estilo de código
├── .clippy.toml            ← reglas de linting
├── .github/
│   └── workflows/
│       └── ci.yml          ← test + clippy + fmt en cada push
└── crates/
    ├── nexusdb-core/       ← tipos base, traits, errores
    ├── nexusdb-types/      ← Value enum, DataType
    ├── nexusdb-storage/    ← mmap, páginas, WAL
    ├── nexusdb-wal/        ← Write-Ahead Log
    ├── nexusdb-index/      ← B+ Tree, HNSW, FTS
    ├── nexusdb-mvcc/       ← transacciones, snapshots
    ├── nexusdb-catalog/    ← schema, estadísticas
    ├── nexusdb-sql/        ← parser, planner, executor
    ├── nexusdb-functions/  ← funciones built-in
    ├── nexusdb-network/    ← MySQL + PostgreSQL wire protocol
    ├── nexusdb-security/   ← RBAC, RLS, TLS
    ├── nexusdb-replication/← streaming replication
    ├── nexusdb-plugins/    ← WASM, Lua
    ├── nexusdb-cache/      ← query cache
    ├── nexusdb-geo/        ← tipos geométricos
    ├── nexusdb-vector/     ← embeddings, HNSW
    ├── nexusdb-migrations/ ← CLI migrations
    ├── nexusdb-server/     ← binario servidor (bin)
    └── nexusdb-embedded/   ← librería embebida (cdylib)

## Fuera del alcance
- Implementar lógica real (eso es Fase 1.2+)
- Tests reales (solo que compile)
- Dependencias externas aún (solo std por ahora excepto thiserror)
