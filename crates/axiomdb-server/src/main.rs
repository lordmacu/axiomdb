//! # axiomdb-server — AxiomDB server binary
//! Listens on :3306 (MySQL) and :5432 (PostgreSQL) simultaneously.
//! Stub — implementation in Phase 5.

use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize structured logging.
    // Log level is controlled by the RUST_LOG environment variable.
    // Examples:
    //   RUST_LOG=debug axiomdb-server     → full detail
    //   RUST_LOG=axiomdb_storage=debug    → only the storage crate
    //   (no RUST_LOG)                     → info level by default
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "AxiomDB starting");
    info!("Server not yet implemented — Phase 5");
}
