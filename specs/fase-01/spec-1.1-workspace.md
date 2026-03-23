# Spec: 1.1 — Workspace Setup

## What to build
Complete Rust workspace structure with all crates declared,
code quality configuration, and CI on GitHub Actions.

## Acceptance criteria
- [ ] `cargo build --workspace` compiles without errors or warnings
- [ ] `cargo test --workspace` passes (no real tests yet, just that it compiles)
- [ ] `cargo clippy --workspace -- -D warnings` passes clean
- [ ] `cargo fmt --check` passes clean
- [ ] GitHub Actions runs automatically on every push to main

## Structure to create
```
nexusdb/
├── Cargo.toml              ← workspace root with all crates
├── rust-toolchain.toml     ← pin Rust version
├── .rustfmt.toml           ← code style
├── .clippy.toml            ← linting rules
├── .github/
│   └── workflows/
│       └── ci.yml          ← test + clippy + fmt on every push
└── crates/
    ├── nexusdb-core/       ← base types, traits, errors
    ├── nexusdb-types/      ← Value enum, DataType
    ├── nexusdb-storage/    ← mmap, pages, WAL
    ├── nexusdb-wal/        ← Write-Ahead Log
    ├── nexusdb-index/      ← B+ Tree, HNSW, FTS
    ├── nexusdb-mvcc/       ← transactions, snapshots
    ├── nexusdb-catalog/    ← schema, statistics
    ├── nexusdb-sql/        ← parser, planner, executor
    ├── nexusdb-functions/  ← built-in functions
    ├── nexusdb-network/    ← MySQL + PostgreSQL wire protocol
    ├── nexusdb-security/   ← RBAC, RLS, TLS
    ├── nexusdb-replication/← streaming replication
    ├── nexusdb-plugins/    ← WASM, Lua
    ├── nexusdb-cache/      ← query cache
    ├── nexusdb-geo/        ← geometric types
    ├── nexusdb-vector/     ← embeddings, HNSW
    ├── nexusdb-migrations/ ← CLI migrations
    ├── nexusdb-server/     ← server binary (bin)
    └── nexusdb-embedded/   ← embedded library (cdylib)

## Out of scope
- Implementing real logic (that is Phase 1.2+)
- Real tests (just that it compiles)
- External dependencies yet (only std for now except thiserror)
