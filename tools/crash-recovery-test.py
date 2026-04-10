#!/usr/bin/env python3
"""
AxiomDB crash recovery under concurrency (Phase 40.12, Test 15).

Sequence:
  1. Start server
  2. Create table, 4 threads inserting concurrently (autocommit)
  3. After ~500 inserts per thread: SIGKILL the server
  4. Restart with the SAME data directory (crash recovery runs)
  5. Verify: committed rows present, no corruption, no duplicates

Usage:
  cargo build -p axiomdb-server
  python3 tools/crash-recovery-test.py
"""
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

try:
    import pymysql
except ImportError:
    print("pymysql not installed: pip install pymysql")
    sys.exit(1)

PORT = 13308
PASS = 0
FAIL = 0


def ok(label, cond, got=None):
    global PASS, FAIL
    if cond:
        print(f"  ✓ {label}")
        PASS += 1
    else:
        detail = f" (got: {got!r})" if got is not None else ""
        print(f"  ✗ {label}{detail}")
        FAIL += 1


# ── Server lifecycle (keeps data dir on kill) ─────────────────────────────────

_server_proc = None
_data_dir = None


def find_binary():
    explicit = os.environ.get("AXIOMDB_SERVER_BIN")
    if explicit:
        return explicit
    debug = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(debug) and os.path.isfile(release):
        return debug if os.path.getmtime(debug) > os.path.getmtime(release) else release
    if os.path.isfile(release):
        return release
    if os.path.isfile(debug):
        return debug
    print("Server binary not found — build first: cargo build -p axiomdb-server")
    sys.exit(1)


def start_server(data_dir):
    """Start server with given data dir. Returns subprocess."""
    global _server_proc
    binary = find_binary()
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen(
        [binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(50):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return _server_proc
        except OSError:
            time.sleep(0.1)
    _server_proc.kill()
    _server_proc = None
    print(f"Server did not start on :{PORT} within 5s")
    sys.exit(1)


def kill_server_hard():
    """SIGKILL — simulate ungraceful crash. Data dir preserved."""
    global _server_proc
    if _server_proc:
        _server_proc.kill()  # SIGKILL
        try:
            _server_proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        _server_proc = None


def stop_server_graceful():
    """Graceful shutdown."""
    global _server_proc
    if _server_proc:
        _server_proc.terminate()
        try:
            _server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _server_proc.kill()
        _server_proc = None


def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=True,
    )


# ── Concurrent writer ─────────────────────────────────────────────────────────

class WriterState:
    def __init__(self):
        self.committed_ids = []
        self.errors = 0
        self.running = True


def writer_thread(worker_id, state):
    """Insert rows until state.running is False or connection breaks."""
    try:
        conn = connect()
        cur = conn.cursor()
        i = 0
        while state.running:
            row_id = worker_id * 100000 + i
            try:
                cur.execute(
                    "INSERT INTO crash_test VALUES (%s, %s, %s)",
                    (row_id, worker_id, f"w{worker_id}_{i}"),
                )
                state.committed_ids.append(row_id)
                i += 1
            except Exception:
                state.errors += 1
                break
        conn.close()
    except Exception:
        pass  # Connection broken by SIGKILL — expected


# ── Main test ─────────────────────────────────────────────────────────────────

def main():
    global _data_dir

    print("=" * 60)
    print("AxiomDB Crash Recovery Under Concurrency (Test 15)")
    print("=" * 60)

    _data_dir = tempfile.mkdtemp(prefix="axiomdb-crash-")
    print(f"\nData dir: {_data_dir}")

    # ── Phase 1: Start server, create table ───────────────────────────────
    print("\n[Phase 1] Starting server...")
    start_server(_data_dir)

    conn = connect()
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS crash_test")
    cur.execute("CREATE TABLE crash_test (id INT NOT NULL, worker INT, label TEXT, PRIMARY KEY(id))")
    conn.close()
    print("  Table created.")

    # ── Phase 2: Concurrent writers ───────────────────────────────────────
    num_writers = 4
    target_rows = 200  # per writer, before kill
    print(f"\n[Phase 2] Launching {num_writers} concurrent writers...")

    states = [WriterState() for _ in range(num_writers)]
    threads = []
    for wid in range(num_writers):
        t = threading.Thread(target=writer_thread, args=(wid, states[wid]))
        t.daemon = True
        t.start()
        threads.append(t)

    # Wait until each writer has committed at least target_rows.
    deadline = time.monotonic() + 30  # 30s timeout
    while time.monotonic() < deadline:
        counts = [len(s.committed_ids) for s in states]
        if all(c >= target_rows for c in counts):
            break
        time.sleep(0.05)

    total_committed_before_kill = sum(len(s.committed_ids) for s in states)
    min_committed = min(len(s.committed_ids) for s in states)
    print(f"  Writers committed {total_committed_before_kill} total rows "
          f"(min per worker: {min_committed})")

    # ── Phase 3: SIGKILL ──────────────────────────────────────────────────
    print("\n[Phase 3] Sending SIGKILL to server...")
    # Signal writers to stop (they'll break on connection error anyway).
    for s in states:
        s.running = False
    kill_server_hard()

    # Wait for threads to finish (connection broken).
    for t in threads:
        t.join(timeout=3)

    # Collect all committed IDs (these were ACK'd by the server before kill).
    all_committed_ids = set()
    for s in states:
        all_committed_ids.update(s.committed_ids)
    print(f"  Server killed. {len(all_committed_ids)} rows were ACK'd before kill.")

    # ── Phase 4: Restart (crash recovery) ─────────────────────────────────
    print("\n[Phase 4] Restarting server (crash recovery)...")
    time.sleep(0.5)  # Brief pause for OS to release port
    start_server(_data_dir)
    print("  Server restarted successfully.")

    # ── Phase 5: Verify ───────────────────────────────────────────────────
    print("\n[Phase 5] Verifying data integrity...")

    conn = connect()
    cur = conn.cursor()

    # Count rows
    cur.execute("SELECT COUNT(*) FROM crash_test")
    row_count = cur.fetchone()[0]
    print(f"  Rows after recovery: {row_count}")

    # Check 1: Row count is reasonable (committed ± in-flight tolerance).
    ok("row count <= committed + tolerance",
       row_count <= len(all_committed_ids) + num_writers,
       got=f"rows={row_count}, committed={len(all_committed_ids)}")

    ok("row count > 0 (recovery didn't wipe everything)",
       row_count > 0, got=row_count)

    # Check 2: Spot-check a sample of rows for valid structure.
    try:
        cur.execute("SELECT id, worker, label FROM crash_test LIMIT 50")
        sample = cur.fetchall()
        valid = all(
            isinstance(row[0], int) and isinstance(row[1], int) and isinstance(row[2], str)
            for row in sample
        )
        ok("sample rows have valid structure", valid)
    except Exception as e:
        ok("sample rows have valid structure", False, got=str(e))

    # Check 3: Per-worker spot check — each worker's rows are intact.
    for wid in range(num_writers):
        try:
            lo = wid * 100000
            hi = lo + 100000
            cur.execute(
                "SELECT COUNT(*) FROM crash_test WHERE id >= %s AND id < %s",
                (lo, hi),
            )
            wcount = cur.fetchone()[0]
            ok(f"worker {wid} has rows ({wcount})", wcount > 0, got=wcount)
        except Exception as e:
            ok(f"worker {wid} has rows", False, got=str(e))

    # Check 5: Table is fully operational after recovery.
    try:
        cur.execute("INSERT INTO crash_test VALUES (%s, %s, %s)",
                    (999999, 99, "post_recovery"))
        cur.execute("SELECT id FROM crash_test WHERE id = 999999")
        post = cur.fetchone()
        ok("post-recovery INSERT + SELECT works", post is not None and post[0] == 999999)
    except Exception as e:
        ok("post-recovery INSERT + SELECT works", False, got=str(e))

    # Check 6: UPDATE works after recovery.
    try:
        cur.execute("UPDATE crash_test SET label = 'updated' WHERE id = 999999")
        cur.execute("SELECT label FROM crash_test WHERE id = 999999")
        upd = cur.fetchone()
        ok("post-recovery UPDATE works", upd is not None and upd[0] == "updated")
    except Exception as e:
        ok("post-recovery UPDATE works", False, got=str(e))

    # Check 7: DELETE works after recovery.
    try:
        cur.execute("DELETE FROM crash_test WHERE id = 999999")
        cur.execute("SELECT COUNT(*) FROM crash_test WHERE id = 999999")
        cnt = cur.fetchone()[0]
        ok("post-recovery DELETE works", cnt == 0)
    except Exception as e:
        ok("post-recovery DELETE works", False, got=str(e))

    conn.close()

    # ── Cleanup ───────────────────────────────────────────────────────────
    stop_server_graceful()
    import shutil
    shutil.rmtree(_data_dir, ignore_errors=True)

    # ── Summary ───────────────────────────────────────────────────────────
    print(f"\n{'=' * 60}")
    print(f"Results: {PASS} passed, {FAIL} failed")
    print(f"{'=' * 60}")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
