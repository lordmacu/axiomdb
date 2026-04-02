//! # axiomdb-wal — append-only Write-Ahead Log, transactions, crash recovery
//!
//! - 3.1: WAL binary format (`WalEntry`, `EntryType`)
//! - 3.2: Append-only writer (`WalWriter`)
//! - 3.3: Reader with CRC validation (`WalReader`)
//! - 3.5: Transaction manager (`TxnManager`)
//! - 3.6: Checkpoint (`Checkpointer`)

mod checkpoint;
mod clustered;
mod entry;
pub mod fsync_pipeline;
mod reader;
mod recovery;
mod rotation;
mod sync;
mod txn;
mod writer;

pub use checkpoint::Checkpointer;
pub use clustered::ClusteredRowImage;
pub use entry::{EntryType, WalEntry, MIN_ENTRY_LEN};
pub use fsync_pipeline::{AcquireResult, CommitRx, FsyncPipeline};
pub use reader::{BackwardIter, ForwardIter, WalReader};
pub use recovery::{CrashRecovery, RecoveryOp, RecoveryResult, RecoveryState};
pub use rotation::WalRotator;
pub use txn::{decode_physical_loc, Savepoint, TxnManager, UndoOp, PHYSICAL_LOC_LEN};
pub use writer::{WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};
