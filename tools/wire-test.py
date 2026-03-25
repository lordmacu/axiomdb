#!/usr/bin/env python3
"""
AxiomDB wire protocol test.
Updated at each subphase close — always overwrite this file, never create new ones.

Last updated: subphase 6.13 (index-only scans + non-unique composite keys)
"""
import os
import signal
import subprocess
import sys
import tempfile
import time

import pymysql

PORT = 13306
PASS = 0
FAIL = 0

# ── Server lifecycle ───────────────────────────────────────────────────────────

_server_proc = None
_data_dir    = None


def start_server():
    global _server_proc, _data_dir
    binary = "target/release/axiomdb-server"
    if not os.path.isfile(binary):
        binary = "target/debug/axiomdb-server"
    if not os.path.isfile(binary):
        print("Server binary not found — build first: cargo build -p axiomdb-server")
        sys.exit(1)
    _data_dir = tempfile.mkdtemp(prefix="axiomdb-wire-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = _data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen(
        [binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    # Wait up to 5s for the server to be ready
    import socket
    for _ in range(50):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return
        except OSError:
            time.sleep(0.1)
    stop_server()
    print(f"Server did not start on :{PORT} within 5s")
    sys.exit(1)


def stop_server():
    global _server_proc, _data_dir
    if _server_proc:
        _server_proc.terminate()
        try:
            _server_proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            _server_proc.kill()
        _server_proc = None
    if _data_dir and os.path.isdir(_data_dir):
        import shutil
        shutil.rmtree(_data_dir, ignore_errors=True)
        _data_dir = None


def ok(label, cond, got=None):
    global PASS, FAIL
    if cond:
        print(f"  ✓ {label}")
        PASS += 1
    else:
        detail = f" (got: {got!r})" if got is not None else ""
        print(f"  ✗ {label}{detail}")
        FAIL += 1


def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
    )


# ── Main ──────────────────────────────────────────────────────────────────────

print(f"Starting AxiomDB on :{PORT}...")
start_server()
print("Server ready\n")

conn = connect()
cur = conn.cursor()

cur.execute("CREATE TABLE wt_accounts (id INT UNIQUE, name TEXT, balance INT)")
cur.execute("CREATE TABLE wt_items    (id INT UNIQUE, val TEXT)")
conn.commit()

# ── [3.5a] SET autocommit=0 respected ────────────────────────────────────────

print("\n[3.5a] autocommit=False — ROLLBACK discards data")
cur.execute("INSERT INTO wt_items VALUES (100, 'draft')")
conn.rollback()
cur.execute("SELECT COUNT(*) FROM wt_items")
ok("ROLLBACK discards uncommitted data", cur.fetchone()[0] == 0)

print("\n[3.5a] autocommit=False — COMMIT persists data")
cur.execute("INSERT INTO wt_accounts VALUES (1, 'Alice', 1000)")
cur.execute("INSERT INTO wt_accounts VALUES (2, 'Bob',   500)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM wt_accounts")
ok("COMMIT persists data", cur.fetchone()[0] == 2)

# ── [3.5b] Implicit transaction start ─────────────────────────────────────────

print("\n[3.5b] Multi-statement transaction shares one implicit txn")
cur.execute("INSERT INTO wt_accounts VALUES (3, 'Carol', 300)")
cur.execute("UPDATE wt_accounts SET balance = 999 WHERE id = 1")
conn.commit()
cur.execute("SELECT balance FROM wt_accounts WHERE id = 1")
ok("Multi-statement txn committed correctly", cur.fetchone()[0] == 999)

# ── [3.5c] Statement-level rollback on error ──────────────────────────────────

print("\n[3.5c] Error in txn — transaction stays active")
cur.execute("BEGIN")
cur.execute("INSERT INTO wt_items VALUES (1, 'a')")
try:
    cur.execute("INSERT INTO wt_accounts VALUES (1, 'dup', 0)")  # dup of committed row
    conn.commit()
    ok("Duplicate raises IntegrityError", False)
except pymysql.err.IntegrityError:
    ok("Duplicate raises IntegrityError", True)
    cur.execute("INSERT INTO wt_items VALUES (2, 'b')")
    conn.commit()
    cur.execute("SELECT COUNT(*) FROM wt_items")
    ok("Txn continues after error — 2 rows committed", cur.fetchone()[0] == 2)

# ── [5.9b] @@in_transaction ────────────────────────────────────────────────────

print("\n[5.9b] @@in_transaction")
cur.execute("SELECT @@in_transaction")
ok("@@in_transaction = 0 outside txn", cur.fetchone()[0] == 0)

cur.execute("INSERT INTO wt_items VALUES (3, 'c')")
cur.execute("SELECT @@in_transaction")
ok("@@in_transaction = 1 inside implicit txn", cur.fetchone()[0] == 1)

conn.commit()
cur.execute("SELECT @@in_transaction")
ok("@@in_transaction = 0 after COMMIT", cur.fetchone()[0] == 0)

# ── [5.9b] SHOW WARNINGS ──────────────────────────────────────────────────────

print("\n[5.9b] SHOW WARNINGS on no-op COMMIT/ROLLBACK")
conn.commit()
cur.execute("SHOW WARNINGS")
rows = cur.fetchall()
ok("SHOW WARNINGS has 1 warning after no-op COMMIT", len(rows) == 1)
ok("Warning code is 1592", len(rows) == 1 and rows[0][1] == 1592)

conn.rollback()
cur.execute("SHOW WARNINGS")
ok("SHOW WARNINGS has 1 warning after no-op ROLLBACK", len(cur.fetchall()) == 1)

cur.execute("INSERT INTO wt_items VALUES (4, 'd')")
conn.commit()
cur.execute("SHOW WARNINGS")
ok("No warnings after real COMMIT", len(cur.fetchall()) == 0)

# ── [6.13] Index-only scans ───────────────────────────────────────────────────

print("\n[6.13] Index-only scans — covered queries skip heap read")

cur.execute("CREATE TABLE iox_scores (id INT, score INT, label TEXT)")
cur.execute("CREATE INDEX idx_score ON iox_scores (score)")
cur.execute("INSERT INTO iox_scores VALUES (1, 10, 'low')")
cur.execute("INSERT INTO iox_scores VALUES (2, 20, 'mid')")
cur.execute("INSERT INTO iox_scores VALUES (3, 30, 'high')")
cur.execute("INSERT INTO iox_scores VALUES (4, 20, 'mid2')")
conn.commit()

# Covered equality — SELECT score WHERE score = 20 (only score in SELECT, score indexed)
cur.execute("SELECT score FROM iox_scores WHERE score = 20")
rows = cur.fetchall()
ok("Index-only scan equality: 2 rows with score=20", len(rows) == 2)
ok("Index-only scan equality: all values = 20", all(r[0] == 20 for r in rows))

# Covered range — SELECT score WHERE score >= 20 AND score <= 30
cur.execute("SELECT score FROM iox_scores WHERE score >= 20 AND score <= 30")
rows = cur.fetchall()
scores = sorted(r[0] for r in rows)
ok("Index-only scan range: scores 20,20,30 returned", scores == [20, 20, 30])

# Non-covered SELECT returns correct full rows (regression)
cur.execute("SELECT id, score, label FROM iox_scores WHERE score = 10")
rows = cur.fetchall()
ok("Non-covered select: 1 row with score=10", len(rows) == 1)
ok("Non-covered select: label = 'low'", rows[0][2] == 'low')

# Non-unique index: duplicate values must work — no DuplicateKey
cur.execute("CREATE TABLE iox_tags (id INT, tag TEXT)")
cur.execute("CREATE INDEX idx_tag ON iox_tags (tag)")
cur.execute("INSERT INTO iox_tags VALUES (1, 'rust')")
cur.execute("INSERT INTO iox_tags VALUES (2, 'go')")
cur.execute("INSERT INTO iox_tags VALUES (3, 'rust')")
cur.execute("INSERT INTO iox_tags VALUES (4, 'rust')")
conn.commit()

cur.execute("SELECT tag FROM iox_tags WHERE tag = 'rust'")
rows = cur.fetchall()
ok("Non-unique index: 3 rows with tag='rust' (duplicate values allowed)", len(rows) == 3)
ok("Non-unique index: all returned tags = 'rust'", all(r[0] == 'rust' for r in rows))

# INCLUDE syntax accepted
try:
    cur.execute("CREATE TABLE iox_include (id INT, val INT, extra TEXT)")
    cur.execute("CREATE INDEX idx_cover ON iox_include (val) INCLUDE (extra)")
    conn.commit()
    ok("INCLUDE (cols) DDL syntax accepted", True)
except Exception as e:
    ok("INCLUDE (cols) DDL syntax accepted", False, e)

# DELETE visibility: deleted row must not appear in index-only scan
cur.execute("DELETE FROM iox_tags WHERE id = 1")
conn.commit()
cur.execute("SELECT tag FROM iox_tags WHERE tag = 'rust'")
rows = cur.fetchall()
ok("Index-only scan: deleted row not returned (MVCC)", len(rows) == 2)

# ── Connectivity / basics ─────────────────────────────────────────────────────

print("\n[Connectivity]")
cur.execute("SELECT 1")
ok("SELECT 1", cur.fetchone() == (1,))
cur.execute("SELECT version()")
ok("version() contains AxiomDB", "AxiomDB" in cur.fetchone()[0])

# ── Result ────────────────────────────────────────────────────────────────────

conn.close()
stop_server()

total = PASS + FAIL
print(f"\n{'✓' if FAIL == 0 else '✗'} {PASS}/{total} passed" +
      (f"  ({FAIL} failed)" if FAIL else ""))
sys.exit(0 if FAIL == 0 else 1)
