//! # nexusdb-server — Binario del servidor NexusDB
//! Escucha en :3306 (MySQL) y :5432 (PostgreSQL) simultáneamente.
//! Stub — implementación en Fase 5.

use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    // Inicializar logging estructurado.
    // El nivel se controla con la variable de entorno RUST_LOG.
    // Ejemplos:
    //   RUST_LOG=debug nexusdb-server     → todo el detalle
    //   RUST_LOG=nexusdb_storage=debug    → solo el crate de storage
    //   (sin RUST_LOG)                    → nivel info por defecto
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "NexusDB iniciando");
    info!("Servidor no implementado aún — Fase 5");
}
