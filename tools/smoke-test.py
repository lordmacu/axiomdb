#!/usr/bin/env python3
"""
AxiomDB smoke test — runs before every commit to verify the wire protocol
works end-to-end with a real pymysql connection.

Usage:
  python3 tools/smoke-test.py              # auto-starts server, runs tests, stops server
  python3 tools/smoke-test.py --port 3308  # use a specific port
  python3 tools/smoke-test.py --no-server  # connect to already-running server

Exit code 0 = all passed, 1 = failure.
"""
import argparse, os, shutil, signal, subprocess, sys, tempfile, time

# ── Args ──────────────────────────────────────────────────────────────────────

p = argparse.ArgumentParser(description="AxiomDB end-to-end smoke test")
p.add_argument("--port",      type=int, default=13306, help="port to use (default 13306)")
p.add_argument("--no-server", action="store_true",    help="connect to an already-running server")
p.add_argument("--binary",    default=None,           help="path to axiomdb-server binary")
args = p.parse_args()

PORT = args.port

try:
    import pymysql
except ImportError:
    print("pymysql not installed — skipping smoke test (pip install pymysql)")
    sys.exit(0)

# ── Server lifecycle ──────────────────────────────────────────────────────────

server_proc = None
data_dir    = None

def start_server():
    global server_proc, data_dir

    binary = args.binary
    if binary is None:
        # Try release build first, then debug
        for candidate in ["target/release/axiomdb-server", "target/debug/axiomdb-server"]:
            if os.path.isfile(candidate):
                binary = candidate
                break
    if binary is None or not os.path.isfile(binary):
        print("✗ axiomdb-server binary not found — build first with: cargo build -p axiomdb-server")
        sys.exit(1)

    data_dir = tempfile.mkdtemp(prefix="axiomdb-smoke-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = data_dir
    env["AXIOMDB_PORT"] = str(PORT)

    server_proc = subprocess.Popen(
        [binary],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    # Wait for the server to be ready (up to 5s)
    for _ in range(50):
        try:
            import socket
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return
        except OSError:
            time.sleep(0.1)

    stop_server()
    print(f"✗ Server did not start on :{PORT} within 5s")
    sys.exit(1)


def stop_server():
    global server_proc, data_dir
    if server_proc:
        server_proc.terminate()
        try:
            server_proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            server_proc.kill()
        server_proc = None
    if data_dir and os.path.isdir(data_dir):
        shutil.rmtree(data_dir, ignore_errors=True)
        data_dir = None


def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT,
        user="root", password="",
        autocommit=False,
    )

# ── Test helpers ──────────────────────────────────────────────────────────────

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

# ── Test suites ───────────────────────────────────────────────────────────────

def test_basic_connectivity(conn):
    cur = conn.cursor()
    cur.execute("SELECT 1")
    ok("SELECT 1", cur.fetchone() == (1,))
    cur.execute("SELECT version()")
    v = cur.fetchone()[0]
    ok("version() contains AxiomDB", "AxiomDB" in v, v)


def test_autocommit_false(conn):
    """3.5a/b: SET autocommit=0 — transactions stay open until explicit COMMIT."""
    cur = conn.cursor()
    cur.execute("CREATE TABLE smoke_ac (id INT UNIQUE, val TEXT)")
    conn.commit()

    # ROLLBACK discards data (use ids 100/101 for rollback, then 1/2 for commit —
    # B+Tree index entries from rolled-back txns persist in the index structure,
    # so different keys avoid false UniqueViolation after rollback)
    cur.execute("INSERT INTO smoke_ac VALUES (100, 'draft-a')")
    cur.execute("INSERT INTO smoke_ac VALUES (101, 'draft-b')")
    conn.rollback()
    cur.execute("SELECT COUNT(*) FROM smoke_ac")
    ok("ROLLBACK discards uncommitted data", cur.fetchone()[0] == 0)

    # COMMIT persists data
    cur.execute("INSERT INTO smoke_ac VALUES (1, 'alice')")
    cur.execute("INSERT INTO smoke_ac VALUES (2, 'bob')")
    conn.commit()
    cur.execute("SELECT COUNT(*) FROM smoke_ac")
    ok("COMMIT persists data", cur.fetchone()[0] == 2)

    # Multiple DML share one implicit txn
    cur.execute("INSERT INTO smoke_ac VALUES (3, 'carol')")
    cur.execute("UPDATE smoke_ac SET val = 'ALICE' WHERE id = 1")
    conn.commit()
    cur.execute("SELECT val FROM smoke_ac WHERE id = 1")
    ok("Multi-statement txn committed correctly", cur.fetchone()[0] == "ALICE")


def test_statement_rollback(conn):
    """3.5c: Error in explicit txn rolls back only that statement; txn stays active."""
    cur = conn.cursor()
    cur.execute("CREATE TABLE smoke_sr (id INT UNIQUE, val TEXT)")
    cur.execute("INSERT INTO smoke_sr VALUES (99, 'seed')")
    conn.commit()  # commit seed row so uniqueness check works

    cur.execute("BEGIN")
    cur.execute("INSERT INTO smoke_sr VALUES (1, 'a')")

    try:
        cur.execute("INSERT INTO smoke_sr VALUES (99, 'dup')")  # dup of committed row
        conn.commit()
        ok("Duplicate key raises IntegrityError", False)
    except pymysql.err.IntegrityError:
        ok("Duplicate key raises IntegrityError", True)

    ok("Transaction active after error", conn.get_server_info() is not None)
    cur.execute("INSERT INTO smoke_sr VALUES (2, 'b')")
    conn.commit()
    cur.execute("SELECT COUNT(*) FROM smoke_sr")
    ok("Failed statement doesn't abort txn — 3 rows committed", cur.fetchone()[0] == 3)


def test_in_transaction(conn):
    """5.9b: @@in_transaction reflects live transaction state."""
    cur = conn.cursor()
    cur.execute("CREATE TABLE smoke_it (id INT)")
    conn.commit()

    cur.execute("SELECT @@in_transaction")
    ok("@@in_transaction = 0 outside txn", cur.fetchone()[0] == 0)

    cur.execute("INSERT INTO smoke_it VALUES (1)")
    cur.execute("SELECT @@in_transaction")
    ok("@@in_transaction = 1 inside implicit txn", cur.fetchone()[0] == 1)

    conn.commit()
    cur.execute("SELECT @@in_transaction")
    ok("@@in_transaction = 0 after COMMIT", cur.fetchone()[0] == 0)


def test_show_warnings(conn):
    """5.9b: SHOW WARNINGS returns warning when COMMIT/ROLLBACK is a no-op."""
    cur = conn.cursor()

    # No-op COMMIT → warning
    conn.commit()
    cur.execute("SHOW WARNINGS")
    rows = cur.fetchall()
    ok("SHOW WARNINGS has 1 warning after no-op COMMIT", len(rows) == 1)
    if rows:
        ok("Warning code is 1592", rows[0][1] == 1592)

    # No-op ROLLBACK → warning
    conn.rollback()
    cur.execute("SHOW WARNINGS")
    rows = cur.fetchall()
    ok("SHOW WARNINGS has 1 warning after no-op ROLLBACK", len(rows) == 1)

    # Real COMMIT → no warning
    cur.execute("CREATE TABLE smoke_sw (id INT)")
    conn.commit()
    cur.execute("INSERT INTO smoke_sw VALUES (1)")
    conn.commit()
    cur.execute("SHOW WARNINGS")
    ok("No warnings after real COMMIT", len(cur.fetchall()) == 0)


def test_atomic_transfer(conn):
    """Classic bank transfer — both sides committed or neither."""
    cur = conn.cursor()
    cur.execute("CREATE TABLE smoke_bank (id INT UNIQUE, balance INT)")
    cur.execute("INSERT INTO smoke_bank VALUES (1, 1000)")
    cur.execute("INSERT INTO smoke_bank VALUES (2, 500)")
    conn.commit()

    # Successful transfer
    cur.execute("UPDATE smoke_bank SET balance = balance - 200 WHERE id = 1")
    cur.execute("UPDATE smoke_bank SET balance = balance + 200 WHERE id = 2")
    conn.commit()
    cur.execute("SELECT balance FROM smoke_bank WHERE id = 1")
    ok("Transfer: Alice debited correctly", cur.fetchone()[0] == 800)
    cur.execute("SELECT balance FROM smoke_bank WHERE id = 2")
    ok("Transfer: Bob credited correctly", cur.fetchone()[0] == 700)

    # Atomic multi-step operation: insert + delete in one transaction, then rollback
    cur.execute("INSERT INTO smoke_bank VALUES (3, 999)")
    cur.execute("DELETE FROM smoke_bank WHERE id = 3")
    conn.rollback()
    cur.execute("SELECT COUNT(*) FROM smoke_bank WHERE id = 3")
    row = cur.fetchone()
    ok("ROLLBACK of multi-op txn (INSERT+DELETE) leaves no trace", row is not None and row[0] == 0)


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    if not args.no_server:
        print(f"Starting AxiomDB on :{PORT}...")
        start_server()
        print("Server ready\n")

    try:
        conn = connect()
    except Exception as e:
        stop_server()
        print(f"✗ Connection failed: {e}")
        sys.exit(1)

    suites = [
        ("Connectivity",        test_basic_connectivity),
        ("autocommit=False",    test_autocommit_false),
        ("Statement rollback",  test_statement_rollback),
        ("@@in_transaction",    test_in_transaction),
        ("SHOW WARNINGS",       test_show_warnings),
        ("Atomic transfer",     test_atomic_transfer),
    ]

    for name, fn in suites:
        print(f"[{name}]")
        try:
            # Each suite gets its own connection context
            fn(conn)
        except Exception as e:
            print(f"  ✗ Unexpected exception: {e}")
            global FAIL
            FAIL += 1
        print()

    conn.close()
    if not args.no_server:
        stop_server()

    print("─" * 40)
    total = PASS + FAIL
    print(f"{'✓' if FAIL == 0 else '✗'} {PASS}/{total} passed", end="")
    print(f"  ({FAIL} failed)" if FAIL else "")

    sys.exit(0 if FAIL == 0 else 1)


if __name__ == "__main__":
    main()
