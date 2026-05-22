//! `TxnManager::is_committed` — the predicate the frame checkpoint (subphase 6f)
//! uses to decide which frames are safe to apply + recycle.

use axiomdb_wal::TxnManager;

fn new_mgr() -> (tempfile::TempDir, TxnManager) {
    let dir = tempfile::tempdir().expect("tmp dir");
    let mgr = TxnManager::create(&dir.path().join("t.wal")).expect("create wal");
    (dir, mgr)
}

#[test]
fn is_committed_true_only_after_commit() {
    let (_dir, mgr) = new_mgr();

    // The 0 sentinel is never a committed txn.
    assert!(!mgr.is_committed(0));

    // A begun-but-not-committed txn is active → not committed.
    let t1 = mgr.begin().expect("begin");
    let id1 = t1.txn_id;
    assert!(!mgr.is_committed(id1), "active txn must not read as committed");

    // Committing advances max_committed (Strict default) and removes it from the
    // active set → now committed.
    mgr.commit(t1).expect("commit");
    assert!(mgr.is_committed(id1), "committed txn must read as committed");

    // A never-begun future id is not committed.
    assert!(!mgr.is_committed(id1 + 1000));
}

#[test]
fn is_committed_excludes_active_txn_below_the_watermark() {
    // The crux: an ACTIVE txn whose id is <= max_committed (because a LATER txn
    // committed and advanced the watermark) must still read as NOT committed.
    // This is why is_committed needs the active-set check, not just the watermark.
    let (_dir, mgr) = new_mgr();

    let t1 = mgr.begin().expect("begin t1"); // id1, stays active
    let id1 = t1.txn_id;
    let t2 = mgr.begin().expect("begin t2"); // id2 > id1
    let id2 = t2.txn_id;

    mgr.commit(t2).expect("commit t2"); // max_committed advances to id2 (>= id1)

    assert!(
        !mgr.is_committed(id1),
        "active txn id1 (<= max_committed) must not read as committed"
    );
    assert!(mgr.is_committed(id2), "committed txn id2 must read as committed");
}
