//! # axiomdb-wal — append-only Write-Ahead Log, transactions, crash recovery
//!
//! - 3.1: WAL binary format (`WalEntry`, `EntryType`)
//! - 3.2: Append-only writer (`WalWriter`)
//! - 3.3: Reader with CRC validation (`WalReader`)
//! - 3.5: Transaction manager (`TxnManager`)
//! - 3.6: Checkpoint (`Checkpointer`)

mod checkpoint;
mod clustered;
mod concurrent_writer;
mod entry;
pub mod fsync_pipeline;
mod reader;
mod recovery;
mod rotation;
mod sync;
mod txn;
mod writer;

pub use checkpoint::Checkpointer;
pub use clustered::{ClusteredFieldPatchEntry, ClusteredRowImage, FieldDelta};
pub use concurrent_writer::{wal_fsyncs, ConcurrentWalWriter};
pub use entry::{EntryType, WalEntry, MIN_ENTRY_LEN};
pub use fsync_pipeline::{AcquireResult, CommitRx, FsyncPipeline};
pub use reader::{BackwardIter, ForwardIter, WalReader};
pub use recovery::{CrashRecovery, RecoveryOp, RecoveryResult, RecoveryState};
pub use rotation::WalRotator;
pub use txn::{
    decode_physical_loc, ConnectionTxn, IndexUndoRecord, Savepoint, TxnManager, UndoOp,
    PHYSICAL_LOC_LEN,
};
pub use writer::{WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};

/// Diagnostic toggle (single-fsync-commit A/B): when `true`, `commit_durable` ALSO fsyncs
/// the logical WAL under frame-only — reproducing the pre-single-fsync 2-fsync commit — so a
/// bench can interleave 2-fsync vs 1-fsync in ONE process (the trustworthy A/B). Default
/// `false`: one relaxed atomic load per commit, no prod cost. Never set outside the A/B.
pub static FORCE_DOUBLE_FSYNC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Sets the diagnostic double-fsync toggle (see [`FORCE_DOUBLE_FSYNC`]).
pub fn set_force_double_fsync(on: bool) {
    FORCE_DOUBLE_FSYNC.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the diagnostic double-fsync toggle is on.
pub fn force_double_fsync() -> bool {
    FORCE_DOUBLE_FSYNC.load(std::sync::atomic::Ordering::Relaxed)
}
