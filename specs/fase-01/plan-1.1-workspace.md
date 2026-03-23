# Plan: 1.1 — Workspace Setup

## Implementation steps

1. `rust-toolchain.toml` — pin Rust stable 1.80
2. `Cargo.toml` workspace root — all crates + shared deps + profiles
3. 19 crates with `cargo new --lib` — minimal structure for each
4. `axiomdb-core` — define `DbError` with thiserror (the only real dep)
5. `.rustfmt.toml` and `.clippy.toml` — uniform style
6. `.github/workflows/ci.yml` — test + clippy + fmt
7. Verify: `cargo build --workspace` compiles clean

## Decisions
- `thiserror` as the only real dependency in this subfase
- All other crates depend on `axiomdb-core`
- `axiomdb-server` is `[[bin]]`, `axiomdb-embedded` is `[lib] crate-type = ["cdylib","rlib"]`
