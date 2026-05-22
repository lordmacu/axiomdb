//! Integration tests for the 6f frame-checkpointer trigger (project B, Lever 2).
//!
//! The background [`FrameCheckpointer`] bounds the frame-only redo log at the soft
//! threshold without the commit path paying the checkpoint latency, and shutdown
//! drains + joins it cleanly. The concurrency test also guards the `sync_frame_log`
//! checkpoint-read-guard coordination (a commit's frame sync must not race a
//! checkpoint's log recycle).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axiomdb_storage::checkpointer::FrameCheckpointer;
use axiomdb_storage::{MmapStorage, Page, PageType, StorageEngine, PAGE_SIZE};

/// One on-disk frame = a 36-byte header + a full page image.
const FRAME_SIZE: u64 = 36 + PAGE_SIZE as u64;

/// Allocates + writes one data page (each `write_page` appends a frame in frame-only
/// mode). No txn stamp ⇒ `txn_id == 0` ⇒ treated as committed by the checkpoint.
fn write_one(storage: &MmapStorage, fill: u8) {
    let pid = storage.alloc_page(PageType::Data).unwrap();
    let mut p = Page::new(PageType::Data, pid);
    p.body_mut()[0] = fill;
    p.update_checksum();
    storage.write_page(pid, &p).unwrap();
}

/// Spins until `cond()` holds or `timeout` elapses (returns whether it held).
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

#[test]
fn background_checkpointer_bounds_the_frame_log() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bound.db");

    let mut storage = MmapStorage::create(&db).unwrap();
    storage.enable_frame_only_redo(&db).unwrap();
    let storage = Arc::new(storage);

    // Soft threshold ≈ 8 frames so it trips quickly; fast poll so the test is brisk.
    let soft = 8 * FRAME_SIZE;
    let storage_dyn: Arc<dyn StorageEngine + Send + Sync> = storage.clone();
    let mut cp = FrameCheckpointer::spawn(
        storage_dyn,
        Arc::new(|_t: u64| true),
        soft,
        Duration::from_millis(20),
    );
    storage.set_checkpoint_trigger(cp.trigger());

    // Write WAY past soft (≈ 25× the threshold). The background thread must recycle as
    // we go so the log never settles far above soft.
    for i in 0..200u32 {
        write_one(&storage, (i & 0xff) as u8);
    }

    // The checkpointer is async; eventually it drives the log back under the band.
    let bounded = wait_until(Duration::from_secs(5), || {
        storage.frame_log_durable_len() <= soft * 2
    });
    assert!(
        bounded,
        "frame log not bounded by the checkpointer: {} > {}",
        storage.frame_log_durable_len(),
        soft * 2
    );

    cp.stop_and_join();
}

#[test]
fn stop_and_join_runs_a_final_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("drain.db");

    let mut storage = MmapStorage::create(&db).unwrap();
    storage.enable_frame_only_redo(&db).unwrap();
    let storage = Arc::new(storage);

    // Soft = u64::MAX ⇒ the background thread never checkpoints on its own; only the
    // final forced checkpoint on stop drains the log.
    let storage_dyn: Arc<dyn StorageEngine + Send + Sync> = storage.clone();
    let mut cp = FrameCheckpointer::spawn(
        storage_dyn,
        Arc::new(|_t: u64| true),
        u64::MAX,
        Duration::from_millis(20),
    );
    storage.set_checkpoint_trigger(cp.trigger());

    for i in 0..16u32 {
        write_one(&storage, (i & 0xff) as u8);
    }
    let before = storage.frame_log_durable_len();
    assert!(before > 0, "frames written ⇒ log non-empty");
    // Confirm the background thread did NOT spontaneously checkpoint (soft = MAX).
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        storage.frame_log_durable_len(),
        before,
        "no spontaneous checkpoint under an infinite soft threshold"
    );

    cp.stop_and_join(); // signals stop ⇒ final forced checkpoint ⇒ join

    assert!(
        storage.frame_log_durable_len() < before,
        "final checkpoint on shutdown recycled the log ({} < {before})",
        storage.frame_log_durable_len()
    );
}

#[test]
fn concurrent_commits_and_background_checkpoint_do_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("concurrent.db");

    let mut storage = MmapStorage::create(&db).unwrap();
    storage.enable_frame_only_redo(&db).unwrap();
    let storage = Arc::new(storage);

    // Small soft so the checkpointer recycles often, maximizing the chance a recycle
    // overlaps a writer's `sync_frame_log` (the race the checkpoint read-guard closes).
    let soft = 4 * FRAME_SIZE;
    let storage_dyn: Arc<dyn StorageEngine + Send + Sync> = storage.clone();
    let mut cp = FrameCheckpointer::spawn(
        storage_dyn,
        Arc::new(|_t: u64| true),
        soft,
        Duration::from_millis(5),
    );
    storage.set_checkpoint_trigger(cp.trigger());

    // 4 writers, each appending frames AND syncing the frame log at the commit
    // boundary while the checkpointer recycles concurrently. Without the read-guard in
    // `sync_frame_log` a recycle could strand a syncer ⇒ this test would hang.
    let mut handles = Vec::new();
    for w in 0..4u32 {
        let s = storage.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50u32 {
                write_one(&s, ((w * 50 + i) & 0xff) as u8);
                s.sync_frame_log().unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    cp.stop_and_join(); // final checkpoint ⇒ fully recycled log

    assert!(
        storage.frame_log_durable_len() <= soft,
        "log bounded after concurrent writers + checkpoints: {} <= {soft}",
        storage.frame_log_durable_len()
    );
}
