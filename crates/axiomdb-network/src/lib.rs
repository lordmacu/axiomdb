//! # axiomdb-network — MySQL wire protocol (Phase 5)
//!
//! Implements the MySQL wire protocol so any MySQL-compatible client can
//! connect to AxiomDB on port 3306 without a custom driver.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use axiomdb_network::mysql::{SharedDatabase, handle_connection};
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let db = Arc::new(
//!         SharedDatabase::open(std::path::Path::new("./data")).unwrap()
//!     );
//!     let listener = tokio::net::TcpListener::bind("0.0.0.0:3306").await.unwrap();
//!     loop {
//!         let (stream, _) = listener.accept().await.unwrap();
//!         let db = Arc::clone(&db);
//!         tokio::spawn(async move {
//!             handle_connection(stream, db, 1).await;
//!         });
//!     }
//! }
//! ```

pub mod mysql;
pub mod scheduler;

pub use axiomdb_storage::{DbConfig, WalDurabilityPolicy};
