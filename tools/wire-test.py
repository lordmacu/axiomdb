#!/usr/bin/env python3
"""
AxiomDB wire protocol test.
Updated at each subphase close — always overwrite this file, never create new ones.

Last updated: subphase 5.9b (@@in_transaction + SHOW WARNINGS)
"""
import sys
import pymysql

PORT = 13306
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


def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
    )


# ── Setup ─────────────────────────────────────────────────────────────────────

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

# ── Connectivity / basics ─────────────────────────────────────────────────────

print("\n[Connectivity]")
cur.execute("SELECT 1")
ok("SELECT 1", cur.fetchone() == (1,))
cur.execute("SELECT version()")
ok("version() contains AxiomDB", "AxiomDB" in cur.fetchone()[0])

# ── Result ────────────────────────────────────────────────────────────────────

conn.close()
total = PASS + FAIL
print(f"\n{'✓' if FAIL == 0 else '✗'} {PASS}/{total} passed" +
      (f"  ({FAIL} failed)" if FAIL else ""))
sys.exit(0 if FAIL == 0 else 1)
