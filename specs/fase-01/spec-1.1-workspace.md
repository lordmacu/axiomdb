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
    ├── axiomdb-core/       ← base types, traits, errors
    ├── axiomdb-types/      ← Value enum, DataType
    ├── axiomdb-storage/    ← mmap, pages, WAL
    ├── axiomdb-wal/        ← Write-Ahead Log
    ├── axiomdb-index/      ← B+ Tree, HNSW, FTS
    ├── axiomdb-mvcc/       ← transactions, snapshots
    ├── axiomdb-catalog/    ← schema, statistics
    ├── axiomdb-sql/        ← parser, planner, executor
    ├── axiomdb-functions/  ← built-in functions
    ├── axiomdb-network/    ← MySQL + PostgreSQL wire protocol
    ├── axiomdb-security/   ← RBAC, RLS, TLS
    ├── axiomdb-replication/← streaming replication
    ├── axiomdb-plugins/    ← WASM, Lua
    ├── axiomdb-cache/      ← query cache
    ├── axiomdb-geo/        ← geometric types
    ├── axiomdb-vector/     ← embeddings, HNSW
    ├── axiomdb-migrations/ ← CLI migrations
    ├── axiomdb-server/     ← server binary (bin)
    └── axiomdb-embedded/   ← embedded library (cdylib)

## Out of scope
- Implementing real logic (that is Phase 1.2+)
- Real tests (just that it compiles)
- External dependencies yet (only std for now except thiserror)
