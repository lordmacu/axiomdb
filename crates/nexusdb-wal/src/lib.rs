//! # nexusdb-wal — append-only Write-Ahead Log, transactions, crash recovery
//!
//! - 3.1: WAL binary format (`WalEntry`, `EntryType`)
//! - 3.2: Append-only writer (`WalWriter`)
//! - 3.3: Reader with CRC validation (`WalReader`)
//! - 3.5: Transaction manager (`TxnManager`)
//! - 3.6: Checkpoint (`Checkpointer`)

mod checkpoint;
mod entry;
mod reader;
mod txn;
mod writer;

pub use checkpoint::Checkpointer;
pub use entry::{EntryType, WalEntry, MIN_ENTRY_LEN};
pub use reader::{BackwardIter, ForwardIter, WalReader};
pub use txn::{TxnManager, UndoOp};
pub use writer::{WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};
