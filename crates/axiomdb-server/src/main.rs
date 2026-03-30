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
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerBootstrapConfig {
    bind_host: String,
    port: u16,
    data_dir: std::path::PathBuf,
}

#[cfg(feature = "wire-protocol")]
impl ServerBootstrapConfig {
    fn from_env() -> Result<Self, axiomdb_core::DbError> {
        let axiomdb_url = std::env::var("AXIOMDB_URL").ok();
        let data_dir = std::env::var("AXIOMDB_DATA").ok();
        let port = std::env::var("AXIOMDB_PORT").ok();
        Self::from_env_like(axiomdb_url.as_deref(), data_dir.as_deref(), port.as_deref())
    }

    fn from_env_like(
        axiomdb_url: Option<&str>,
        data_dir: Option<&str>,
        port: Option<&str>,
    ) -> Result<Self, axiomdb_core::DbError> {
        use axiomdb_core::{parse_dsn, ParsedDsn};

        if let Some(url) = axiomdb_url {
            let wire = match parse_dsn(url)? {
                ParsedDsn::Wire(wire) => wire,
                ParsedDsn::Local(_) => {
                    return Err(axiomdb_core::DbError::InvalidDsn {
                        reason: "AXIOMDB_URL must be a wire-endpoint DSN, not a local path".into(),
                    });
                }
            };

            for key in wire.query.keys() {
                if key != "data_dir" {
                    return Err(axiomdb_core::DbError::InvalidDsn {
                        reason: format!("unsupported AXIOMDB_URL query parameter '{key}'"),
                    });
                }
            }

            let data_dir = wire
                .query
                .get("data_dir")
                .cloned()
                .or_else(|| data_dir.map(str::to_owned))
                .unwrap_or_else(|| "./data".into());
            if data_dir.is_empty() {
                return Err(axiomdb_core::DbError::InvalidDsn {
                    reason: "AXIOMDB_URL data_dir cannot be empty".into(),
                });
            }

            return Ok(Self {
                bind_host: wire.host,
                port: wire.port.unwrap_or(3306),
                data_dir: std::path::PathBuf::from(data_dir),
            });
        }

        Ok(Self {
            bind_host: "0.0.0.0".into(),
            port: port.and_then(|value| value.parse().ok()).unwrap_or(3306),
            data_dir: std::path::PathBuf::from(data_dir.unwrap_or("./data")),
        })
    }

    fn bind_addr(&self) -> String {
        if self.bind_host.contains(':') && !self.bind_host.starts_with('[') {
            format!("[{}]:{}", self.bind_host, self.port)
        } else {
            format!("{}:{}", self.bind_host, self.port)
        }
    }
}

#[cfg(feature = "wire-protocol")]
#[tokio::main]
async fn main() {
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tracing::info;
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let config = match ServerBootstrapConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(err = %e, "invalid server bootstrap config");
            std::process::exit(1);
        }
    };

    info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %config.data_dir.display(),
        bind_host = %config.bind_host,
        port = config.port,
        "AxiomDB starting"
    );

    let db_config =
        match axiomdb_network::DbConfig::load(Some(&config.data_dir.join("axiomdb.toml"))) {
            Ok(c) => {
                let dur = c.resolved_wal_durability();
                if dur != axiomdb_network::WalDurabilityPolicy::Strict {
                    info!(?dur, "WAL durability policy (non-default)");
                }
                c
            }
            Err(e) => {
                tracing::error!(err = %e, "failed to load axiomdb.toml");
                std::process::exit(1);
            }
        };

    let db = match axiomdb_network::mysql::Database::open_with_config(&config.data_dir, &db_config)
    {
        Ok(db) => {
            info!("database opened successfully");
            Arc::new(RwLock::new(db))
        }
        Err(e) => {
            tracing::error!(err = %e, "failed to open database");
            std::process::exit(1);
        }
    };

    let addr = config.bind_addr();
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
                if let Err(e) = axiomdb_network::mysql::configure_client_socket(&stream) {
                    tracing::warn!(err = %e, "client socket configuration failed");
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

#[cfg(all(test, feature = "wire-protocol"))]
mod tests {
    use std::path::PathBuf;

    use super::ServerBootstrapConfig;

    #[test]
    fn legacy_bootstrap_keeps_existing_defaults() {
        let config = ServerBootstrapConfig::from_env_like(None, None, None).unwrap();
        assert_eq!(
            config,
            ServerBootstrapConfig {
                bind_host: "0.0.0.0".into(),
                port: 3306,
                data_dir: PathBuf::from("./data"),
            }
        );
    }

    #[test]
    fn legacy_invalid_port_falls_back_to_default() {
        let config =
            ServerBootstrapConfig::from_env_like(None, Some("./custom"), Some("bad")).unwrap();
        assert_eq!(config.port, 3306);
        assert_eq!(config.data_dir, PathBuf::from("./custom"));
    }

    #[test]
    fn axiomdb_url_drives_host_port_and_data_dir() {
        let config = ServerBootstrapConfig::from_env_like(
            Some("axiomdb://127.0.0.1:3307/app?data_dir=%2Fvar%2Flib%2Faxiomdb"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            config,
            ServerBootstrapConfig {
                bind_host: "127.0.0.1".into(),
                port: 3307,
                data_dir: PathBuf::from("/var/lib/axiomdb"),
            }
        );
    }

    #[test]
    fn server_dsn_uses_legacy_data_dir_fallback() {
        let config =
            ServerBootstrapConfig::from_env_like(Some("mysql://[::1]/app"), Some("./data"), None)
                .unwrap();
        assert_eq!(config.bind_host, "::1");
        assert_eq!(config.port, 3306);
        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.bind_addr(), "[::1]:3306");
    }

    #[test]
    fn server_dsn_rejects_unsupported_query_params() {
        let err = ServerBootstrapConfig::from_env_like(
            Some("axiomdb://127.0.0.1/app?sslmode=require"),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            axiomdb_core::DbError::InvalidDsn { reason } if reason.contains("unsupported AXIOMDB_URL query parameter 'sslmode'")
        ));
    }

    #[test]
    fn server_dsn_rejects_local_axiomdb_path() {
        let err = ServerBootstrapConfig::from_env_like(Some("axiomdb:///tmp/app"), None, None)
            .unwrap_err();
        assert!(matches!(
            err,
            axiomdb_core::DbError::InvalidDsn { reason }
                if reason.contains("must be a wire-endpoint DSN")
        ));
    }
}
