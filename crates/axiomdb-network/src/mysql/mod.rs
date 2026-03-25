//! MySQL wire protocol implementation (Phase 5).
//!
//! Provides a complete MySQL-compatible server that accepts connections on
//! port 3306. Any MySQL client (pymysql, PHP PDO, DBeaver, mysql CLI) can
//! connect without a custom driver.
//!
//! ## Public API
//!
//! - [`Database`] — database handle (wraps MmapStorage + TxnManager)
//! - [`handle_connection`] — async handler for one TCP connection
//!
//! ## Modules
//!
//! - [`codec`] — MySQL packet framing (3-byte length + sequence_id)
//! - [`packets`] — packet serialization (handshake, OK, ERR, EOF)
//! - [`auth`] — mysql_native_password challenge-response auth
//! - [`result`] — QueryResult → text protocol wire format
//! - [`error`] — DbError → MySQL error code + SQLSTATE
//! - [`handler`] — connection handler state machine
//! - [`database`] — database engine wrapper

pub mod auth;
pub mod codec;
pub mod commit_coordinator;
pub mod database;
pub mod error;
pub mod group_commit;
pub mod handler;
pub mod json_error;
pub mod packets;
pub mod prepared;
pub mod result;
pub mod session;
pub mod status;

pub use database::Database;
pub use handler::handle_connection;
pub use session::ConnectionState;
