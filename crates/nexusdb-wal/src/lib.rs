//! # nexusdb-wal — Write-Ahead Log append-only, crash recovery
//!
//! Implementa el formato binario del WAL (subfase 3.1), el writer append-only
//! (subfase 3.2), el reader con validación de CRC (subfase 3.3), y el crash
//! recovery (subfase 3.5).

mod entry;
mod reader;
mod writer;

pub use entry::{EntryType, WalEntry, MIN_ENTRY_LEN};
pub use reader::{BackwardIter, ForwardIter, WalReader};
pub use writer::{WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};
