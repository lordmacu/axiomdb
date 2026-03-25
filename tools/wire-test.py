#!/usr/bin/env python3
"""
AxiomDB wire protocol test.
Updated at each subphase close — always overwrite this file, never create new ones.

Last updated: subphase 4.19d (DATE_FORMAT, STR_TO_DATE, FIND_IN_SET, date extractors)
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

# ── [4.19d] DATE_FORMAT / STR_TO_DATE / FIND_IN_SET / date extractors ─────────

print("\n[4.19d] DATE_FORMAT")

cur.execute("SELECT DATE_FORMAT(NULL, '%Y-%m-%d')")
ok("DATE_FORMAT(NULL, ...) = NULL", cur.fetchone()[0] is None)

# STR_TO_DATE('2025-03-25', ...) returns a Date value; DATE_FORMAT formats it
cur.execute("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%m-%d')")
v = cur.fetchone()[0]
ok("DATE_FORMAT(date, '%Y-%m-%d') = '2025-03-25'", v == "2025-03-25", v)

cur.execute("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%d/%m/%Y')")
v = cur.fetchone()[0]
ok("DATE_FORMAT(date, '%d/%m/%Y') = '25/03/2025'", v == "25/03/2025", v)

cur.execute(
    "SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'), '%H:%i:%s')"
)
v = cur.fetchone()[0]
ok("DATE_FORMAT(timestamp, '%H:%i:%s') = '14:30:45'", v == "14:30:45", v)

# Unknown specifier passes through literally
cur.execute("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%X-%d')")
v = cur.fetchone()[0]
ok("DATE_FORMAT unknown specifier passthrough: '%Y-%X-%d'", v == "2025-%X-25", v)

print("\n[4.19d] STR_TO_DATE")

cur.execute("SELECT STR_TO_DATE('not-a-date', '%Y-%m-%d')")
ok("STR_TO_DATE bad input = NULL", cur.fetchone()[0] is None)

cur.execute("SELECT STR_TO_DATE(NULL, '%Y-%m-%d')")
ok("STR_TO_DATE(NULL, ...) = NULL", cur.fetchone()[0] is None)

# Round-trip: parse then format recovers the original string
cur.execute(
    "SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%m-%d')"
)
v = cur.fetchone()[0]
ok("STR_TO_DATE round-trip '%Y-%m-%d'", v == "2025-03-25", v)

# Alternate separator
cur.execute(
    "SELECT DATE_FORMAT(STR_TO_DATE('25/03/2025', '%d/%m/%Y'), '%Y-%m-%d')"
)
v = cur.fetchone()[0]
ok("STR_TO_DATE slash separator", v == "2025-03-25", v)

# Invalid day-in-month
cur.execute("SELECT STR_TO_DATE('2025-02-30', '%Y-%m-%d')")
ok("STR_TO_DATE Feb-30 = NULL", cur.fetchone()[0] is None)

print("\n[4.19d] FIND_IN_SET")

cur.execute("SELECT FIND_IN_SET('b', 'a,b,c')")
ok("FIND_IN_SET('b','a,b,c') = 2", cur.fetchone()[0] == 2)

cur.execute("SELECT FIND_IN_SET('z', 'a,b,c')")
ok("FIND_IN_SET('z','a,b,c') = 0", cur.fetchone()[0] == 0)

cur.execute("SELECT FIND_IN_SET('B', 'a,b,c')")
ok("FIND_IN_SET case-insensitive 'B' = 2", cur.fetchone()[0] == 2)

cur.execute("SELECT FIND_IN_SET(NULL, 'a,b,c')")
ok("FIND_IN_SET(NULL, ...) = NULL", cur.fetchone()[0] is None)

cur.execute("SELECT FIND_IN_SET('a', NULL)")
ok("FIND_IN_SET(..., NULL) = NULL", cur.fetchone()[0] is None)

cur.execute("SELECT FIND_IN_SET('a', '')")
ok("FIND_IN_SET('a', '') = 0", cur.fetchone()[0] == 0)

print("\n[4.19d] year/month/day/hour/minute/second extractors")

cur.execute(
    "SELECT year(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s')), "
    "       month(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s')), "
    "       day(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'))"
)
row = cur.fetchone()
ok("year(ts) = 2025", row[0] == 2025, row[0])
ok("month(ts) = 3", row[1] == 3, row[1])
ok("day(ts) = 25", row[2] == 25, row[2])

cur.execute(
    "SELECT hour(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s')), "
    "       minute(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s')), "
    "       second(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'))"
)
row = cur.fetchone()
ok("hour(ts) = 14", row[0] == 14, row[0])
ok("minute(ts) = 30", row[1] == 30, row[1])
ok("second(ts) = 45", row[2] == 45, row[2])

# NOW() extractors — just check they return plausible values
cur.execute("SELECT year(NOW()), month(NOW()), day(NOW())")
row = cur.fetchone()
ok("year(NOW()) in 2020-2100", 2020 <= row[0] <= 2100, row[0])
ok("month(NOW()) in 1-12", 1 <= row[1] <= 12, row[1])
ok("day(NOW()) in 1-31", 1 <= row[2] <= 31, row[2])

# ── 4.9e GROUP_CONCAT ────────────────────────────────────────────────────────

print("\n[4.9e] GROUP_CONCAT / string_agg")

cur.execute("CREATE TABLE gc_tags (post_id INT NOT NULL, tag TEXT)")
for (pid, tag) in [(1,'rust'),(1,'db'),(1,'async'),(2,'rust'),(2,'web'),(3,None)]:
    if tag is None:
        cur.execute("INSERT INTO gc_tags VALUES (%s, NULL)", (pid,))
    else:
        cur.execute("INSERT INTO gc_tags VALUES (%s, %s)", (pid, tag))

# Basic GROUP_CONCAT with ORDER BY — deterministic order
cur.execute(
    "SELECT GROUP_CONCAT(tag ORDER BY tag ASC) FROM gc_tags WHERE post_id = 1"
)
ok("GROUP_CONCAT ordered ASC", cur.fetchone()[0] == "async,db,rust")

# Custom SEPARATOR
cur.execute(
    "SELECT GROUP_CONCAT(tag ORDER BY tag ASC SEPARATOR ' | ') FROM gc_tags WHERE post_id = 1"
)
ok("GROUP_CONCAT custom separator", cur.fetchone()[0] == "async | db | rust")

# ORDER BY DESC
cur.execute(
    "SELECT GROUP_CONCAT(tag ORDER BY tag DESC) FROM gc_tags WHERE post_id = 1"
)
ok("GROUP_CONCAT ORDER BY DESC", cur.fetchone()[0] == "rust,db,async")

# NULL values skipped
cur.execute("SELECT GROUP_CONCAT(tag) FROM gc_tags WHERE post_id = 3")
ok("GROUP_CONCAT all-NULL → NULL", cur.fetchone()[0] is None)

# Empty group → NULL
cur.execute("SELECT GROUP_CONCAT(tag) FROM gc_tags WHERE post_id = 99")
ok("GROUP_CONCAT empty group → NULL", cur.fetchone()[0] is None)

# DISTINCT deduplication
cur.execute("CREATE TABLE gc_dup (v TEXT)")
cur.execute("INSERT INTO gc_dup VALUES ('a')")
cur.execute("INSERT INTO gc_dup VALUES ('b')")
cur.execute("INSERT INTO gc_dup VALUES ('a')")
cur.execute("INSERT INTO gc_dup VALUES ('c')")
cur.execute("SELECT GROUP_CONCAT(DISTINCT v ORDER BY v ASC) FROM gc_dup")
ok("GROUP_CONCAT DISTINCT", cur.fetchone()[0] == "a,b,c")

# string_agg alias
cur.execute("SELECT string_agg(tag, ', ') FROM gc_tags WHERE post_id = 2")
row = cur.fetchone()[0]
ok("string_agg separator present", row is not None and ', ' in row)
ok("string_agg contains rust", row is not None and 'rust' in row)

# GROUP BY query
cur.execute(
    "SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC) "
    "FROM gc_tags GROUP BY post_id ORDER BY post_id ASC"
)
rows = cur.fetchall()
ok("GROUP_CONCAT GROUP BY row count", len(rows) == 3)
ok("GROUP_CONCAT GROUP BY post_id=1", rows[0][1] == "async,db,rust")
ok("GROUP_CONCAT GROUP BY post_id=2", rows[1][1] == "rust,web")
ok("GROUP_CONCAT GROUP BY post_id=3 NULL", rows[2][1] is None)

# HAVING with GROUP_CONCAT
cur.execute(
    "SELECT post_id FROM gc_tags "
    "GROUP BY post_id "
    "HAVING GROUP_CONCAT(tag ORDER BY tag ASC) LIKE '%rust%' "
    "ORDER BY post_id ASC"
)
rows = cur.fetchall()
ok("HAVING GROUP_CONCAT LIKE row count", len(rows) == 2, [r[0] for r in rows])
post_ids_having = sorted(int(r[0]) for r in rows)
ok("HAVING GROUP_CONCAT LIKE has post_id=1", 1 in post_ids_having, post_ids_having)
ok("HAVING GROUP_CONCAT LIKE has post_id=2", 2 in post_ids_having, post_ids_having)

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
