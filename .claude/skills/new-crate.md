# /new-crate — Scaffold a crate in the workspace

## Complete process

### 1. Create the crate

```bash
cd /Users/cristian/dbyo
cargo new --lib crates/dbyo-NAME
```

### 2. Standard crate structure

```
crates/dbyo-NAME/
├── Cargo.toml
├── src/
│   ├── lib.rs          ← only public re-exports and crate doc
│   ├── error.rs        ← DbError with thiserror
│   └── [modules].rs
└── tests/
    └── integration.rs  ← integration tests
```

### 3. New crate's Cargo.toml

```toml
[package]
name    = "dbyo-NAME"
version = "0.1.0"
edition = "2021"

[dependencies]
# Use workspace versions when defined
dbyo-core = { path = "../dbyo-core" }
thiserror = { workspace = true }
tracing   = { workspace = true }

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
tempfile  = "3"
```

### 4. Minimal src/lib.rs

```rust
//! # dbyo-NAME
//!
//! [One-line description of what this crate does]
//!
//! ## Example
//! ```rust
//! use dbyo_name::MyTrait;
//! // minimal example
//! ```

mod error;
pub use error::Error;

// Public traits first
pub trait MyTrait: Send + Sync {
    fn operation(&self) -> Result<(), Error>;
}
```

### 5. Add to root workspace Cargo.toml

```toml
[workspace]
members = [
    # ... existing ...
    "crates/dbyo-NAME",   # ← add here in alphabetical order
]
```

### 6. Verify no dependency cycle

```bash
cargo tree --workspace 2>&1 | grep -E "dbyo-NAME|error\[E"
```

### 7. Initial compiling test

```rust
// tests/integration.rs
use dbyo_name::MyTrait;

#[test]
fn crate_compiles_and_trait_exists() {
    // Just verify it compiles for now
    // Real tests are added in the implementation phase
}
```

```bash
cargo test -p dbyo-NAME
```

### 8. Update memory/architecture.md

```markdown
## Implemented crates
- `dbyo-NAME` — [one-line description] (Phase N)
```

### Checklist

```
[ ] cargo new --lib executed
[ ] crate's Cargo.toml with correct dependencies
[ ] Added to workspace members
[ ] src/lib.rs with documented public traits
[ ] Initial test compiling
[ ] cargo test -p dbyo-NAME passes
[ ] cargo clippy -p dbyo-NAME -- -D warnings passes
[ ] architecture.md updated
```
