//! # axiomdb-server — AxiomDB server binary
//!
//! Listens on :3306 (MySQL wire protocol).
//! Any MySQL-compatible client can connect without a custom driver.
//!
//! ## Usage
//!
//! ```bash
//! axiomdb-server                    # uses ./data directory
//! AXIOMDB_DATA=/var/lib/axiomdb axiomdb-server
//! AXIOMDB_PORT=3307 axiomdb-server  # custom port
//! ```

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Initialize structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let data_dir = std::env::var("AXIOMDB_DATA").unwrap_or_else(|_| "./data".into());
    let port: u16 = std::env::var("AXIOMDB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3306);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %data_dir,
        port,
        "AxiomDB starting"
    );

    // Open (or create) the database.
    let db = match axiomdb_network::mysql::Database::open(std::path::Path::new(&data_dir)) {
        Ok(db) => {
            info!("database opened successfully");
            Arc::new(Mutex::new(db))
        }
        Err(e) => {
            tracing::error!(err = %e, "failed to open database");
            std::process::exit(1);
        }
    };

    // Bind TCP listener.
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            info!(%addr, "listening for MySQL connections");
            l
        }
        Err(e) => {
            tracing::error!(err = %e, %addr, "failed to bind");
            std::process::exit(1);
        }
    };

    // Accept loop — spawn one task per connection.
    let mut conn_id = 1u32;
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let db = Arc::clone(&db);
                let id = conn_id;
                tokio::spawn(async move {
                    axiomdb_network::mysql::handle_connection(stream, db, id).await;
                });
                conn_id = conn_id.wrapping_add(1);
            }
            Err(e) => {
                tracing::warn!(err = %e, "accept error — continuing");
            }
        }
    }
}
