//! # axiomdb-server
//!
//! AxiomDB server binary. Behavior depends on the build profile:
//!
//! ## Profiles
//!
//! | Profile | Command | What runs |
//! |---|---|---|
//! | **server** (default) | `cargo build -p axiomdb-server` | MySQL wire protocol on :3306 |
//! | **embedded** | `--no-default-features` | Prints usage for in-process embedding |
//!
//! ## Environment variables
//!
//! ```bash
//! AXIOMDB_DATA=/var/lib/axiomdb   # data directory (default: ./data)
//! AXIOMDB_PORT=3307               # TCP port (default: 3306, wire-protocol only)
//! RUST_LOG=debug                  # log level
//! ```

// ── Wire-protocol server ───────────────────────────────────────────────────────

#[cfg(feature = "wire-protocol")]
#[tokio::main]
async fn main() {
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tracing::info;
    use tracing_subscriber::EnvFilter;

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

    let mut conn_id = 1u32;
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                if let Err(e) = stream.set_nodelay(true) {
                    tracing::warn!(err = %e, "TCP_NODELAY failed");
                }
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

// ── Embedded mode (no wire protocol) ─────────────────────────────────────────

#[cfg(not(feature = "wire-protocol"))]
fn main() {
    eprintln!(
        "axiomdb-server v{} — embedded mode (wire-protocol disabled)",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!();
    eprintln!("This binary was compiled without the wire-protocol feature.");
    eprintln!("To use AxiomDB embedded in your app, use the axiomdb-embedded crate:");
    eprintln!();
    eprintln!("  [dependencies]");
    eprintln!("  axiomdb-embedded = {{ path = \"...\" }}");
    eprintln!();
    eprintln!("To build the full server with wire protocol:");
    eprintln!("  cargo build -p axiomdb-server --features wire-protocol");
    std::process::exit(1);
}
