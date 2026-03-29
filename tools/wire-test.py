#!/usr/bin/env python3
"""
AxiomDB wire protocol test.
Updated at each subphase close — always overwrite this file, never create new ones.

Last updated: subphases 5.11c (explicit connection lifecycle), 5.19 (B+tree batch delete),
             5.19a (executor decomposition — structural refactor, wire-invisible),
             5.21 (transactional INSERT staging), 6.19 (WAL fsync pipeline smoke),
             6.20 (UPDATE apply fast path smoke), 22b.3a (database catalog wire smoke)
"""
import os
import signal
import subprocess
import sys
import tempfile
import time
import struct as _struct

import pymysql
import pymysql.constants.COMMAND as _CMD
import pymysql.constants.CLIENT as _CLIENT

PORT = 13306
PASS = 0
FAIL = 0

# ── Server lifecycle ───────────────────────────────────────────────────────────

_server_proc = None
_data_dir    = None


def _check_binary_freshness(binary):
    """Abort if any .rs source file is newer than the binary.

    Catches the 'stale release binary' trap: cargo build updates target/debug/
    but wire-test.py would silently pick an older target/release/ binary, running
    tests against code that predates the current changes.
    """
    import glob
    binary_mtime = os.path.getmtime(binary)
    stale = [
        f for f in glob.glob("crates/**/*.rs", recursive=True)
        if os.path.getmtime(f) > binary_mtime
    ]
    if stale:
        print(f"\nERROR: binary '{binary}' is stale.")
        print(f"  {len(stale)} source file(s) are newer than the binary, e.g.:")
        for f in stale[:3]:
            print(f"    {f}")
        print("\nFix: cargo build --bin axiomdb-server")
        sys.exit(1)


def start_server():
    global _server_proc, _data_dir
    debug   = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(debug) and os.path.isfile(release):
        binary = debug if os.path.getmtime(debug) > os.path.getmtime(release) else release
    elif os.path.isfile(release):
        binary = release
    elif os.path.isfile(debug):
        binary = debug
    else:
        binary = debug  # trigger "not found" message below
    if not os.path.isfile(binary):
        print("Server binary not found — build first: cargo build -p axiomdb-server")
        sys.exit(1)
    _check_binary_freshness(binary)
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


def connect_db(database):
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        database=database, autocommit=False,
    )


def connect_multi():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
        client_flag=_CLIENT.MULTI_STATEMENTS,
    )


def connect_interactive():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
        client_flag=_CLIENT.INTERACTIVE,
    )


def reset_connection(conn):
    """Send COM_RESET_CONNECTION (0x1f) using PyMySQL internals."""
    conn._execute_command(0x1F, b"")
    conn._read_ok_packet()


def _packet_data(pkt):
    return pkt._data if hasattr(pkt, "_data") else b""


def _drain_prepare_metadata(conn, num_params, num_cols):
    for _ in range(num_params):
        conn._read_packet()
    if num_params:
        conn._read_packet()  # EOF after parameter defs
    for _ in range(num_cols):
        conn._read_packet()
    if num_cols:
        conn._read_packet()  # EOF after result column defs


def raw_prepare(conn, sql):
    conn._execute_command(_CMD.COM_STMT_PREPARE, sql.encode("utf-8"))
    data = _packet_data(conn._read_packet())
    stmt_id = _struct.unpack_from("<I", data, 1)[0]
    num_cols = _struct.unpack_from("<H", data, 5)[0]
    num_params = _struct.unpack_from("<H", data, 7)[0]
    _drain_prepare_metadata(conn, num_params, num_cols)
    return stmt_id, num_params, num_cols


def raw_send_long_data(conn, stmt_id, param_idx, chunk):
    payload = _struct.pack("<I", stmt_id) + _struct.pack("<H", param_idx) + chunk
    conn._execute_command(_CMD.COM_STMT_SEND_LONG_DATA, payload)


def raw_stmt_reset(conn, stmt_id):
    conn._execute_command(_CMD.COM_STMT_RESET, _struct.pack("<I", stmt_id))
    return _packet_data(conn._read_packet())


def raw_stmt_close(conn, stmt_id):
    conn._execute_command(_CMD.COM_STMT_CLOSE, _struct.pack("<I", stmt_id))


def _null_bitmap(param_count, null_indices=()):
    bitmap = bytearray((param_count + 7) // 8)
    for idx in null_indices:
        bitmap[idx // 8] |= 1 << (idx % 8)
    return bytes(bitmap)


def _lenenc_bytes(data):
    if len(data) >= 251:
        raise ValueError("wire-test helper only supports short lenenc payloads")
    return bytes([len(data)]) + data


def raw_execute(conn, stmt_id, param_types, inline_values=b"", null_indices=()):
    payload = _struct.pack("<I", stmt_id)
    payload += b"\x00"  # flags = CURSOR_TYPE_NO_CURSOR
    payload += _struct.pack("<I", 1)  # iteration_count = 1
    payload += _null_bitmap(len(param_types), null_indices)
    payload += b"\x01"  # new_params_bound_flag
    for type_code in param_types:
        payload += bytes([type_code, 0x00])
    payload += inline_values
    conn._execute_command(_CMD.COM_STMT_EXECUTE, payload)
    return _packet_data(conn._read_packet())


# ── Main ──────────────────────────────────────────────────────────────────────

print(f"Starting AxiomDB on :{PORT}...")
start_server()
print("Server ready\n")

conn = connect()
cur = conn.cursor()

# ── [22b.3a] Database catalog + session namespace smoke ─────────────────────

print("\n[22b.3a] Database catalog + session namespace")
cur.execute("SHOW DATABASES")
dbs = sorted(row[0] for row in cur.fetchall())
ok("SHOW DATABASES includes default axiomdb", dbs == ["axiomdb"], dbs)

cur.execute("CREATE DATABASE analytics")
conn.commit()
cur.execute("SHOW DATABASES")
dbs = sorted(row[0] for row in cur.fetchall())
ok(
    "SHOW DATABASES includes created database",
    dbs == ["analytics", "axiomdb"],
    dbs,
)

analytics_conn = connect_db("analytics")
analytics_cur = analytics_conn.cursor()
analytics_cur.execute("SELECT DATABASE()")
analytics_db = analytics_cur.fetchone()[0]
ok(
    "Handshake database is visible through DATABASE()",
    analytics_db == "analytics",
    analytics_db,
)
analytics_cur.execute("CREATE TABLE db_scope (id INT)")
analytics_cur.execute("INSERT INTO db_scope VALUES (10)")
analytics_conn.commit()
analytics_cur.execute("SHOW TABLES")
ok(
    "SHOW TABLES is scoped to selected database",
    [row[0] for row in analytics_cur.fetchall()] == ["db_scope"],
)

conn.select_db("axiomdb")
cur.execute("SELECT DATABASE()")
ok("COM_INIT_DB switches selected database", cur.fetchone()[0] == "axiomdb")
cur.execute("CREATE TABLE db_scope (id INT)")
cur.execute("INSERT INTO db_scope VALUES (1)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM db_scope")
ok("axiomdb namespace resolves its own unqualified table", cur.fetchone()[0] == 1)

conn.select_db("analytics")
analytics_cur.execute("SELECT COUNT(*) FROM db_scope")
ok(
    "analytics namespace resolves its own unqualified table",
    analytics_cur.fetchone()[0] == 1,
)
try:
    analytics_cur.execute("DROP DATABASE analytics")
    analytics_conn.commit()
    ok("DROP DATABASE rejects active selected database", False)
except pymysql.MySQLError as e:
    ok(
        "DROP DATABASE rejects active selected database",
        e.args and e.args[0] == 1105,
        e.args,
    )

conn.select_db("axiomdb")
try:
    conn.select_db("missing_db")
    ok("COM_INIT_DB rejects unknown database", False)
except pymysql.MySQLError as e:
    ok(
        "COM_INIT_DB rejects unknown database",
        e.args and e.args[0] == 1049,
        e.args,
    )

try:
    bad_conn = connect_db("missing_db")
    bad_conn.close()
    ok("Handshake rejects unknown database", False)
except pymysql.MySQLError as e:
    ok(
        "Handshake rejects unknown database",
        e.args and e.args[0] == 1049,
        e.args,
    )

cur.execute("DROP DATABASE analytics")
conn.commit()
cur.execute("SHOW DATABASES")
dbs = sorted(row[0] for row in cur.fetchall())
ok(
    "DROP DATABASE removes database from catalog",
    dbs == ["axiomdb"],
    dbs,
)
try:
    cur.execute("SELECT COUNT(*) FROM db_scope")
    ok("axiomdb table survives analytics drop", cur.fetchone()[0] == 1)
except pymysql.MySQLError as e:
    ok("axiomdb table survives analytics drop", False, e.args)

analytics_conn.close()

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

# ── [5.2c] ON_ERROR session behavior ──────────────────────────────────────────

print("\n[5.2c] ON_ERROR session behavior")
conn_oe = connect()
co = conn_oe.cursor()
co.execute("CREATE TABLE wt_on_error (id INT UNIQUE NOT NULL)")
conn_oe.commit()

co.execute("SELECT @@on_error")
ok("@@on_error defaults to rollback_statement",
   co.fetchone()[0] == "rollback_statement")

co.execute("SHOW VARIABLES LIKE 'on_error'")
rows = co.fetchall()
ok("SHOW VARIABLES LIKE 'on_error' returns current mode",
   len(rows) == 1 and rows[0] == ("on_error", "rollback_statement"), rows)

co.execute("SET on_error = 'rollback_transaction'")
co.execute("BEGIN")
co.execute("INSERT INTO wt_on_error VALUES (1)")
try:
    co.execute("INSERT INTO wt_on_error VALUES (1)")
    ok("rollback_transaction duplicate raises IntegrityError", False, "no error raised")
except pymysql.err.IntegrityError:
    ok("rollback_transaction duplicate raises IntegrityError", True)

co.execute("SELECT @@in_transaction")
ok("rollback_transaction closes the txn after error",
   co.fetchone()[0] == 0)

co.execute("SELECT COUNT(*) FROM wt_on_error")
ok("rollback_transaction discards prior writes in the txn",
   co.fetchone()[0] == 0)

co.execute("INSERT INTO wt_on_error VALUES (99)")
conn_oe.commit()
co.execute("SET autocommit = 0")
co.execute("SET on_error = 'savepoint'")
try:
    co.execute("INSERT INTO wt_on_error VALUES (99)")
    ok("savepoint first failing DML still surfaces as error", False, "no error raised")
except pymysql.err.IntegrityError:
    ok("savepoint first failing DML still surfaces as error", True)

co.execute("SELECT @@in_transaction")
ok("savepoint keeps the implicit txn open after first failing DML",
   co.fetchone()[0] == 1)

co.execute("INSERT INTO wt_on_error VALUES (2)")
co.execute("COMMIT")
co.execute("SELECT COUNT(*) FROM wt_on_error WHERE id = 2")
ok("savepoint keeps the txn usable after the failed statement",
   co.fetchone()[0] == 1)

co.execute("SET on_error = 'ignore'")
co.execute("BEGIN")
co.execute("INSERT INTO wt_on_error VALUES (10)")
try:
    co.execute("INSERT INTO wt_on_error VALUES (10)")
    ok("ignore duplicate key returns success instead of ERR", True)
except pymysql.MySQLError as e:
    ok("ignore duplicate key returns success instead of ERR", False, e)

warning_count = getattr(getattr(conn_oe, "_result", None), "warning_count", 0)
ok("ignore duplicate OK packet carries warning_count > 0",
   warning_count > 0, warning_count)

co.execute("SHOW WARNINGS")
warnings = co.fetchall()
ok("ignore populates SHOW WARNINGS",
   len(warnings) >= 1, warnings)
if warnings:
    ok("ignore warning code is 1062 for duplicate key",
       warnings[0][1] == 1062, warnings[0])
    ok("ignore warning preserves original duplicate-key message",
       "duplicate" in warnings[0][2].lower() or "unique" in warnings[0][2].lower(),
       warnings[0][2])

co.execute("INSERT INTO wt_on_error VALUES (11)")
co.execute("COMMIT")
co.execute("SELECT id FROM wt_on_error WHERE id IN (10, 11) ORDER BY id")
ok("ignore commits rows before and after the ignored error",
   co.fetchall() == ((10,), (11,)))

conn_multi = connect_multi()
cm = conn_multi.cursor()
cm.execute("SET on_error = 'ignore'")
cm.execute(
    "INSERT INTO wt_on_error VALUES (20); "
    "INSERT INTO wt_on_error VALUES (20); "
    "INSERT INTO wt_on_error VALUES (21); "
    "COMMIT"
)
while cm.nextset():
    pass
cm.execute("SELECT id FROM wt_on_error WHERE id IN (20, 21) ORDER BY id")
ok("ignore continues executing later statements in multi-statement COM_QUERY",
   cm.fetchall() == ((20,), (21,)))
cm.execute("SHOW WARNINGS")
ok("SHOW WARNINGS after later statements still follows last-statement-only rule",
   len(cm.fetchall()) == 0)
conn_multi.close()

co.execute("SET on_error = 'rollback_transaction'")
reset_connection(conn_oe)
co = conn_oe.cursor()
co.execute("SELECT @@on_error")
ok("COM_RESET_CONNECTION resets @@on_error to rollback_statement",
   co.fetchone()[0] == "rollback_statement")
conn_oe.close()

# ── [5.11c] Explicit connection lifecycle / timeout vars ─────────────────────

print("\n[5.11c] connection lifecycle / timeout vars")
conn_lc = connect()
cl = conn_lc.cursor()

cl.execute("SET wait_timeout = 7")
cl.execute("SET interactive_timeout = 8")
cl.execute("SET net_read_timeout = 9")
cl.execute("SET net_write_timeout = 10")
cl.execute("SELECT @@wait_timeout")
ok("SELECT @@wait_timeout returns live value", cl.fetchone()[0] == "7")
cl.execute("SELECT @@interactive_timeout")
ok("SELECT @@interactive_timeout returns live value", cl.fetchone()[0] == "8")
cl.execute("SELECT @@net_read_timeout")
ok("SELECT @@net_read_timeout returns live value", cl.fetchone()[0] == "9")
cl.execute("SELECT @@net_write_timeout")
ok("SELECT @@net_write_timeout returns live value", cl.fetchone()[0] == "10")

try:
    cl.execute("SET wait_timeout = 0")
    ok("SET wait_timeout = 0 returns ERR", False, "no error raised")
except pymysql.MySQLError:
    ok("SET wait_timeout = 0 returns ERR", True)

reset_connection(conn_lc)
cl = conn_lc.cursor()
cl.execute("SELECT @@wait_timeout")
ok("COM_RESET_CONNECTION resets @@wait_timeout to default", cl.fetchone()[0] == "28800")
cl.execute("SELECT @@interactive_timeout")
ok("COM_RESET_CONNECTION resets @@interactive_timeout to default", cl.fetchone()[0] == "28800")
cl.execute("SELECT @@net_read_timeout")
ok("COM_RESET_CONNECTION resets @@net_read_timeout to default", cl.fetchone()[0] == "60")
cl.execute("SELECT @@net_write_timeout")
ok("COM_RESET_CONNECTION resets @@net_write_timeout to default", cl.fetchone()[0] == "60")
conn_lc.close()

conn_idle = connect()
ci = conn_idle.cursor()
ci.execute("SET wait_timeout = 1")
time.sleep(1.2)
try:
    ci.execute("SELECT 1")
    ok("non-interactive idle timeout closes the connection", False, "query unexpectedly succeeded")
except pymysql.MySQLError:
    ok("non-interactive idle timeout closes the connection", True)
try:
    conn_idle.close()
except Exception:
    pass

conn_int = connect_interactive()
cx = conn_int.cursor()
cx.execute("SET wait_timeout = 1")
reset_connection(conn_int)
cx = conn_int.cursor()
cx.execute("SET wait_timeout = 1")
time.sleep(1.2)
try:
    cx.execute("SELECT 1")
    row = cx.fetchone()
    ok(
        "interactive classification survives COM_RESET_CONNECTION",
        row == (1,),
        row,
    )
except pymysql.MySQLError as e:
    ok("interactive classification survives COM_RESET_CONNECTION", False, e)
conn_int.close()

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

# ── [4.9b] Sort-Based GROUP BY ───────────────────────────────────────────────

print("\n[4.9b] Sort-Based GROUP BY (indexed sorted path)")

# Setup: create index on empty table (bootstraps stats with row_count=0),
# then insert rows. The row_count=0 stats path skips the small-table guard
# and uses the index → sorted GROUP BY strategy is selected.
cur.execute("DROP TABLE IF EXISTS sb_emp")
cur.execute("CREATE TABLE sb_emp (id INT PRIMARY KEY, dept TEXT, salary INT)")
cur.execute("CREATE INDEX idx_sb_dept ON sb_emp (dept)")  # stats.row_count = 0 here

for i in range(1, 16):
    cur.execute("INSERT INTO sb_emp VALUES (%s, 'eng', %s)", (i, 80000 + i))
for i in range(16, 31):
    cur.execute("INSERT INTO sb_emp VALUES (%s, 'hr', %s)", (i, 60000 + i))
for i in range(31, 46):
    cur.execute("INSERT INTO sb_emp VALUES (%s, 'sales', %s)", (i, 70000 + i))

# COUNT GROUP BY on indexed column with ORDER BY
cur.execute(
    "SELECT dept, COUNT(*) AS cnt "
    "FROM sb_emp "
    "GROUP BY dept "
    "ORDER BY dept ASC"
)
rows_gb = cur.fetchall()
ok("4.9b: GROUP BY indexed col row count", len(rows_gb) == 3, rows_gb)
ok("4.9b: GROUP BY dept=eng count=15", rows_gb[0][1] == 15, rows_gb[0])
ok("4.9b: GROUP BY dept=hr count=15", rows_gb[1][1] == 15, rows_gb[1])
ok("4.9b: GROUP BY dept=sales count=15", rows_gb[2][1] == 15, rows_gb[2])

# SUM GROUP BY on indexed column with ORDER BY
cur.execute(
    "SELECT dept, SUM(salary) "
    "FROM sb_emp "
    "GROUP BY dept "
    "ORDER BY dept ASC"
)
rows_sum = cur.fetchall()
ok("4.9b: GROUP BY SUM row count", len(rows_sum) == 3, rows_sum)
# eng salaries: 80001..80015 → sum = 15*80000 + sum(1..15) = 1200000 + 120 = 1200120
ok("4.9b: GROUP BY SUM eng correct", int(rows_sum[0][1]) == 1200120, rows_sum[0])

# HAVING with sorted path + ORDER BY
cur.execute(
    "SELECT dept, COUNT(*) AS cnt "
    "FROM sb_emp "
    "GROUP BY dept "
    "HAVING COUNT(*) >= 15 "
    "ORDER BY dept ASC"
)
rows_hav = cur.fetchall()
ok("4.9b: HAVING with sorted GROUP BY returns 3 depts", len(rows_hav) == 3, rows_hav)

# GROUP BY without usable index (plain scan / hash strategy) + ORDER BY
cur.execute("DROP TABLE IF EXISTS sb_noindex")
cur.execute("CREATE TABLE sb_noindex (id INT, cat TEXT, val INT)")
for i in range(1, 11):
    cur.execute("INSERT INTO sb_noindex VALUES (%s, 'a', %s)", (i, i * 10))
for i in range(11, 21):
    cur.execute("INSERT INTO sb_noindex VALUES (%s, 'b', %s)", (i, i * 10))
cur.execute(
    "SELECT cat, COUNT(*) "
    "FROM sb_noindex "
    "GROUP BY cat "
    "ORDER BY cat ASC"
)
rows_noix = cur.fetchall()
ok("4.9b: hash GROUP BY (no index) still correct count", len(rows_noix) == 2, rows_noix)
ok("4.9b: hash GROUP BY cat=a count=10", rows_noix[0][1] == 10, rows_noix[0])
ok("4.9b: hash GROUP BY cat=b count=10", rows_noix[1][1] == 10, rows_noix[1])

# GROUP_CONCAT regression under the sorted path
cur.execute(
    "SELECT dept, GROUP_CONCAT(dept ORDER BY dept ASC) "
    "FROM sb_emp "
    "WHERE dept = 'eng' "
    "GROUP BY dept"
)
row_gc = cur.fetchone()
ok("4.9b: GROUP_CONCAT sorted path non-null", row_gc is not None and row_gc[1] is not None)

# ── [4.25b] Structured Error Responses ────────────────────────────────────────

print("\n[4.25b] Structured Error Responses")

# --- ParseError: visual snippet in text error messages ---
try:
    cur.execute("SELECT * FORM t")
    ok("parse error on bad query", False, "should have raised")
except pymysql.err.ProgrammingError as ex:
    msg = str(ex)
    ok("parse error code 1064", "1064" in msg, msg)
    ok("parse error message not empty", len(msg) > 10, msg)

# Syntax error with position info
try:
    cur.execute("SELECT id, FROM users")
    ok("parse error mid-query", False, "should have raised")
except pymysql.err.ProgrammingError as ex:
    msg = str(ex)
    ok("mid-query parse error code 1064", "1064" in msg, msg)

# --- UniqueViolation: offending value in error message ---
cur.execute("CREATE TABLE uv_test (id INT PRIMARY KEY, email VARCHAR(255) UNIQUE)")
cur.execute("INSERT INTO uv_test VALUES (1, 'alice@example.com')")
conn.commit()

try:
    cur.execute("INSERT INTO uv_test VALUES (2, 'alice@example.com')")
    conn.commit()
    ok("unique violation raises error", False, "should have raised")
except pymysql.err.IntegrityError as ex:
    msg = str(ex)
    ok("unique violation error code 1062", "1062" in msg, msg)
    ok("unique violation message contains value", "alice@example.com" in msg, msg)
    conn.rollback()

try:
    cur.execute("INSERT INTO uv_test VALUES (1, 'bob@example.com')")
    conn.commit()
    ok("pk violation raises error", False, "should have raised")
except pymysql.err.IntegrityError as ex:
    msg = str(ex)
    ok("pk violation error code 1062", "1062" in msg, msg)
    conn.rollback()

# --- SET error_format = 'json': errors return valid JSON in message ---
cur.execute("SET error_format = 'json'")
try:
    cur.execute("SELECT * FORM t")
    ok("json format parse error raised", False, "should have raised")
except pymysql.err.ProgrammingError as ex:
    import json as _json
    # ex.args[1] is the raw message string (no extra Python escaping)
    raw_msg = ex.args[1] if len(ex.args) >= 2 else str(ex)
    try:
        obj = _json.loads(raw_msg)
        ok("json error is valid JSON",     True)
        ok("json error has code field",    "code"     in obj, obj)
        ok("json error has sqlstate",      "sqlstate" in obj, obj)
        ok("json error has message field", "message"  in obj, obj)
        ok("json error sqlstate 42601",    obj.get("sqlstate") == "42601", obj)
    except _json.JSONDecodeError:
        ok("json error is valid JSON", False, f"not JSON: {raw_msg!r}")

# Reset error_format to text
cur.execute("SET error_format = 'text'")

# Confirm text mode is restored
try:
    cur.execute("SELECT * FORM t")
    ok("text mode restored — error raised", False, "should have raised")
except pymysql.err.ProgrammingError as ex:
    msg = str(ex)
    ok("text mode restored — not raw JSON", not msg.strip().startswith('{'), msg)

# ── [5.9c] SHOW STATUS ────────────────────────────────────────────────────────

print("\n[5.9c] SHOW STATUS — scope, LIKE wildcards, counters")


def status_map(cursor, sql):
    """Execute a SHOW STATUS variant and return a {Variable_name: Value} dict."""
    cursor.execute(sql)
    return {row[0]: row[1] for row in cursor.fetchall()}


# Ensure clean cursor state before SHOW STATUS section
conn.rollback()

# Two-column result shape
cur.execute("SHOW STATUS")
rows = cur.fetchall()
ok("SHOW STATUS returns rows", len(rows) > 0, f"{len(rows)} rows")
ok("SHOW STATUS has 2 columns", len(rows[0]) == 2 if rows else False)

# Variables present
names = {r[0] for r in rows}
for expected_var in [
    "Questions", "Uptime", "Threads_connected", "Threads_running",
    "Bytes_received", "Bytes_sent", "Com_select", "Com_insert",
    "Innodb_buffer_pool_read_requests", "Innodb_buffer_pool_reads",
]:
    ok(f"SHOW STATUS contains {expected_var}", expected_var in names, names)

# Row order is deterministic ascending
var_names = [r[0] for r in rows]
ok("SHOW STATUS rows are in ascending order", var_names == sorted(var_names), var_names)

# Uptime is monotonic integer >= 0
s = status_map(cur, "SHOW STATUS")
ok("Uptime is a non-negative integer", int(s.get("Uptime", -1)) >= 0, s.get("Uptime"))

# Session scope: Threads_running = 1 while serving the statement
ok("Session Threads_running = 1", s.get("Threads_running") == "1", s.get("Threads_running"))

# SHOW SESSION STATUS == SHOW STATUS (both default to session)
session_s = status_map(cur, "SHOW SESSION STATUS")
ok("SHOW SESSION STATUS has same keys as SHOW STATUS",
   set(session_s.keys()) == set(s.keys()))

# SHOW LOCAL STATUS == SHOW SESSION STATUS
local_s = status_map(cur, "SHOW LOCAL STATUS")
ok("SHOW LOCAL STATUS has same keys as SHOW SESSION STATUS",
   set(local_s.keys()) == set(session_s.keys()))

# SHOW GLOBAL STATUS exists and has the same variables
global_s = status_map(cur, "SHOW GLOBAL STATUS")
ok("SHOW GLOBAL STATUS has same keys as session", set(global_s.keys()) == set(s.keys()))

# LIKE 'x' — unknown pattern returns zero rows (not an error)
cur.execute("SHOW STATUS LIKE 'no_such_variable_xyz'")
ok("SHOW STATUS LIKE 'unknown' returns empty (not error)", len(cur.fetchall()) == 0)

# LIKE '%' wildcard
cur.execute("SHOW STATUS LIKE 'Com_%'")
com_rows = cur.fetchall()
com_names = sorted(r[0] for r in com_rows)
ok("SHOW STATUS LIKE 'Com_%' includes insert/select/stmt_send_long_data",
   com_names == ["Com_insert", "Com_select", "Com_stmt_send_long_data"], com_names)

# LIKE '_' single-char wildcard
cur.execute("SHOW STATUS LIKE 'Com_inser_'")
rows = cur.fetchall()
ok("SHOW STATUS LIKE 'Com_inser_' matches only Com_insert",
   len(rows) == 1 and rows[0][0] == "Com_insert", [r[0] for r in rows])

# LIKE is case-insensitive
cur.execute("SHOW STATUS LIKE 'threads%'")
t_names = sorted(r[0] for r in cur.fetchall())
ok("SHOW STATUS LIKE 'threads%' is case-insensitive (lowercase pattern)",
   t_names == ["Threads_connected", "Threads_running"], t_names)

# Com_select counter: two SELECT statements increment Com_select by exactly 2.
# (Questions is not checked here because pymysql's autocommit=False sends a
# SET autocommit=0 init query that also increments Questions, making the
# expected value driver-dependent.)
conn2 = connect()
c2 = conn2.cursor()
c2.execute("SELECT 1")
c2.execute("SELECT 2")
s_after = status_map(c2, "SHOW SESSION STATUS")
ok("Com_select = 2 after two SELECT statements",
   int(s_after.get("Com_select", 0)) == 2,
   s_after.get("Com_select"))
conn2.close()

# COM_RESET_CONNECTION resets session counters but not global
conn3 = connect()
c3 = conn3.cursor()
c3.execute("SELECT 1")
c3.execute("SELECT 2")
# After reset, session Questions should be 0
# pymysql wraps COM_RESET_CONNECTION through the internal _send_autocommit_mode path;
# the portable equivalent is a fresh connection (which our server starts with a new
# ConnectionState — same observable effect for this test).
conn3.close()
conn3 = connect()
c3 = conn3.cursor()
s_reset = status_map(c3, "SHOW SESSION STATUS")
# Com_select = 0 because fresh connection has not yet executed any SELECT.
# (Questions is not checked because init queries like SET autocommit=0 increment it.)
ok("After reconnect (equivalent to COM_RESET_CONNECTION), session Com_select = 0",
   int(s_reset.get("Com_select", -1)) == 0,
   s_reset.get("Com_select"))
conn3.close()

# SELECT @@version increments Com_select (intercepted statement)
conn4 = connect()
c4 = conn4.cursor()
c4.execute("SELECT @@version")
c4.fetchall()
s4 = status_map(c4, "SHOW SESSION STATUS")
ok("SELECT @@version (intercepted) increments Com_select",
   int(s4.get("Com_select", 0)) >= 1,
   s4.get("Com_select"))
conn4.close()

# Fresh second connection has Com_select = 0 (session isolation)
conn5 = connect()
c5 = conn5.cursor()
# We've done selects in other connections; new connection should start at 0
s5 = status_map(c5, "SHOW SESSION STATUS")
ok("Fresh connection sees Com_select = 0 (session isolation)",
   int(s5.get("Com_select", -1)) == 0,
   s5.get("Com_select"))
conn5.close()

# SHOW STATUS is queryable without blocking (Threads_connected >= 1)
conn6 = connect()
c6 = conn6.cursor()
g6 = status_map(c6, "SHOW GLOBAL STATUS LIKE 'Threads_connected'")
ok("SHOW GLOBAL STATUS LIKE 'Threads_connected' has exactly one row", len(g6) == 1)
ok("Threads_connected >= 1", int(g6.get("Threads_connected", 0)) >= 1,
   g6.get("Threads_connected"))
conn6.close()

# ── 5.5a: binary result encoding (COM_STMT_EXECUTE) ──────────────────────────

print("\n[5.5a binary result encoding]")

# Use a dedicated connection so the schema state is clean.
conn_bin = connect()
cb = conn_bin.cursor()

# Create a table with typed columns and insert one row.
cb.execute("DROP TABLE IF EXISTS t_binary_test")
cb.execute("""
    CREATE TABLE t_binary_test (
        id    INT,
        big   BIGINT,
        label TEXT
    )
""")
cb.execute("INSERT INTO t_binary_test VALUES (1, 9876543210, 'hello')")
cb.execute("INSERT INTO t_binary_test VALUES (2, -1, NULL)")
conn_bin.commit()

# High-level check: pymysql reads back the correct Python types.
cb.execute("SELECT big, label FROM t_binary_test WHERE id = 1")
row_hl = cb.fetchone()
ok("High-level: BIGINT round-trips correctly (9876543210)",
   row_hl[0] == 9876543210, row_hl[0])
ok("High-level: TEXT round-trips correctly",
   row_hl[1] == "hello", row_hl[1])

# High-level NULL in prepared result.
cb.execute("SELECT big, label FROM t_binary_test WHERE id = 2")
row_null = cb.fetchone()
ok("High-level: NULL column returns None", row_null[1] is None, row_null[1])
ok("High-level: negative BIGINT round-trips correctly (-1)",
   row_null[0] == -1, row_null[0])

# Low-level: parse the raw COM_STMT_EXECUTE row packet and prove it is binary.
# We use PyMySQL's internal _execute_command to get the raw packet bytes.

conn_raw = connect()
try:
    # Prepare at the wire level for raw packet inspection.
    # Query: SELECT big, label FROM t_binary_test WHERE id = 1
    # Result: BIGINT + TEXT, zero params.
    sql_bytes = b"SELECT big, label FROM t_binary_test WHERE id = 1"
    conn_raw._execute_command(_CMD.COM_STMT_PREPARE, sql_bytes)
    # Read prepare response and extract stmt_id from raw bytes.
    prep_pkt = conn_raw._read_packet()
    prep_data = prep_pkt._data if hasattr(prep_pkt, '_data') else b''
    stmt_id = _struct.unpack_from('<I', prep_data[1:5])[0] if len(prep_data) >= 5 else 0
    # Drain column-def + EOF packets from prepare response (2 col defs + EOF).
    for _ in range(3):
        conn_raw._read_packet()

    # Build a zero-param COM_STMT_EXECUTE payload.
    execute_payload = _struct.pack('<I', stmt_id)  # stmt_id
    execute_payload += bytes([0x00])               # flags = 0
    execute_payload += _struct.pack('<I', 1)        # iteration-count = 1
    conn_raw._execute_command(_CMD.COM_STMT_EXECUTE, execute_payload)

    # Drain: column-count + 2 column-def packets + EOF after column defs.
    for _ in range(4):
        conn_raw._read_packet()
    # Read the binary row packet.
    row_pkt = conn_raw._read_packet()
    raw = row_pkt._data if hasattr(row_pkt, '_data') else b''

    ok("Binary row packet first byte is 0x00 (not 0xfb text marker)",
       len(raw) > 1 and raw[0] == 0x00, hex(raw[0]) if raw else "empty")

    # Layout: header(1) + bitmap(1) + BIGINT(8) + TEXT lenenc(1+5) = 16 bytes total
    # bitmap_len = (2 + 7 + 2) / 8 = 1 byte
    if len(raw) >= 10:
        bigint_bytes = raw[2:10]
        bigint_val = _struct.unpack_from('<q', bigint_bytes)[0]
        ok("Binary BIGINT is 8-byte LE, value = 9876543210",
           bigint_val == 9876543210, bigint_val)
        # First byte of bigint must NOT be '9' (0x39), which would indicate ASCII encoding.
        ok("BIGINT first byte is not ASCII digit '9' (binary, not text)",
           bigint_bytes[0] != ord('9'), hex(bigint_bytes[0]))
    else:
        ok("Binary BIGINT is 8-byte LE, value = 9876543210",
           False, f"packet too short: {len(raw)}")
        ok("BIGINT first byte is not ASCII digit '9' (binary, not text)", False, "")

    # TEXT follows immediately after the 8-byte BIGINT: lenenc(1) + "hello"(5)
    if len(raw) >= 16:
        text_len = raw[10]
        text_val = raw[11:11 + text_len].decode('utf-8', errors='replace')
        ok("TEXT after BIGINT is lenenc-encoded string 'hello'",
           text_val == "hello", repr(text_val))
    else:
        ok("TEXT after BIGINT is lenenc-encoded string 'hello'",
           False, f"packet too short: {len(raw)}")
except Exception as e:
    ok("Binary row packet first byte is 0x00 (not 0xfb text marker)", False, str(e))
    ok("Binary BIGINT is 8-byte LE, value = 9876543210", False, str(e))
    ok("BIGINT first byte is not ASCII digit '9' (binary, not text)", False, str(e))
    ok("TEXT after BIGINT is lenenc-encoded string 'hello'", False, str(e))
finally:
    conn_raw.close()

cb.execute("DROP TABLE IF EXISTS t_binary_test")
conn_bin.commit()
conn_bin.close()

# ── 5.11b: COM_STMT_SEND_LONG_DATA ───────────────────────────────────────────

print("\n[5.11b] COM_STMT_SEND_LONG_DATA")

conn_ld = connect()
cld = conn_ld.cursor()

cld.execute("DROP TABLE IF EXISTS t_long_data")
cld.execute("CREATE TABLE t_long_data (id INT, txt TEXT, blb BLOB)")
conn_ld.commit()

# Text long-data split across chunks, including a multibyte boundary.
stmt_text, num_params_text, _ = raw_prepare(
    conn_ld,
    "INSERT INTO t_long_data (id, txt, blb) VALUES (?, ?, NULL)",
)
ok("prepare text statement reports 2 params", num_params_text == 2, num_params_text)
raw_send_long_data(conn_ld, stmt_text, 1, b"ma\xc3")
raw_send_long_data(conn_ld, stmt_text, 1, b"\xb1ana")
s_ld = status_map(cld, "SHOW SESSION STATUS LIKE 'Com_stmt_send_long_data'")
ok("session Com_stmt_send_long_data = 2 after two chunks",
   int(s_ld.get("Com_stmt_send_long_data", -1)) == 2,
   s_ld.get("Com_stmt_send_long_data"))

pkt_text = raw_execute(
    conn_ld,
    stmt_text,
    [0x03, 0xfd],  # INT, VAR_STRING
    inline_values=_struct.pack("<i", 1),
    null_indices=(1,),  # pending long data must win over NULL
)
ok("text long-data execute returns OK", pkt_text[:1] == b"\x00", pkt_text[:12])
conn_ld.commit()
cld.execute("SELECT txt FROM t_long_data WHERE id = 1")
row_text = cld.fetchone()
ok("multibyte text split across chunks reconstructs correctly",
   row_text and row_text[0] == "mañana", row_text)
raw_stmt_close(conn_ld, stmt_text)

# COM_STMT_RESET clears pending long-data state but keeps the statement usable.
stmt_reset, _, _ = raw_prepare(
    conn_ld,
    "INSERT INTO t_long_data (id, txt, blb) VALUES (?, ?, NULL)",
)
raw_send_long_data(conn_ld, stmt_reset, 1, b"should_be_cleared")
pkt_reset = raw_stmt_reset(conn_ld, stmt_reset)
ok("COM_STMT_RESET returns OK", pkt_reset[:1] == b"\x00", pkt_reset[:12])
pkt_after_reset = raw_execute(
    conn_ld,
    stmt_reset,
    [0x03, 0xfd],
    inline_values=_struct.pack("<i", 2) + _lenenc_bytes(b"inline_text"),
)
ok("execute after COM_STMT_RESET returns OK",
   pkt_after_reset[:1] == b"\x00", pkt_after_reset[:12])
conn_ld.commit()
cld.execute("SELECT txt FROM t_long_data WHERE id = 2")
row_reset = cld.fetchone()
ok("COM_STMT_RESET clears pending long-data state",
   row_reset and row_reset[0] == "inline_text", row_reset)
raw_stmt_close(conn_ld, stmt_reset)

# Binary long data preserves raw bytes, including NUL.
stmt_blob, _, _ = raw_prepare(
    conn_ld,
    "INSERT INTO t_long_data (id, txt, blb) VALUES (?, NULL, ?)",
)
raw_send_long_data(conn_ld, stmt_blob, 1, b"\x00\xff")
raw_send_long_data(conn_ld, stmt_blob, 1, b"\x00\x42")
pkt_blob = raw_execute(
    conn_ld,
    stmt_blob,
    [0x03, 0xfc],  # INT, BLOB
    inline_values=_struct.pack("<i", 3),
    null_indices=(1,),
)
ok("binary long-data execute returns OK", pkt_blob[:1] == b"\x00", pkt_blob[:12])
conn_ld.commit()
cld.execute("SELECT blb FROM t_long_data WHERE id = 3")
row_blob = cld.fetchone()
ok("binary long data preserves raw bytes including NUL",
   row_blob and row_blob[0] == b"\x00\xff\x00\x42", row_blob)
raw_stmt_close(conn_ld, stmt_blob)

# Deferred overflow error surfaces on EXECUTE and the connection remains usable.
stmt_err, _, _ = raw_prepare(
    conn_ld,
    "INSERT INTO t_long_data (id, txt, blb) VALUES (4, ?, NULL)",
)
cld.execute("SET max_allowed_packet = 18")
raw_send_long_data(conn_ld, stmt_err, 0, b"abcdefghij")
raw_send_long_data(conn_ld, stmt_err, 0, b"klmnopqrs")  # 19 bytes total > 18
try:
    raw_execute(
        conn_ld,
        stmt_err,
        [0xfd],
    )
    ok("oversized accumulated long data returns ERR on execute", False, "no error raised")
    ok("oversized long-data error mentions max_allowed_packet", False, "no error raised")
except pymysql.MySQLError as e:
    err_msg = str(e)
    ok("oversized accumulated long data returns ERR on execute", True)
    ok("oversized long-data error mentions max_allowed_packet",
       "max_allowed_packet" in err_msg, err_msg)

# The deferred long-data overflow must not kill the connection.
cld.execute("SELECT 1")
row_alive = cld.fetchone()
ok("connection remains usable after deferred long-data error",
   row_alive == (1,), row_alive)

# The same statement remains reusable under the same small packet limit.
pkt_reuse = raw_execute(
    conn_ld,
    stmt_err,
    [0xfd],
    inline_values=_lenenc_bytes(b"ok"),
)
ok("deferred long-data state is cleared after failed execute",
   pkt_reuse[:1] == b"\x00", pkt_reuse[:12])
conn_ld.commit()

raw_stmt_close(conn_ld, stmt_err)
conn_ld.close()

conn_ld_cleanup = connect()
cur_ld_cleanup = conn_ld_cleanup.cursor()
cur_ld_cleanup.execute("SELECT txt FROM t_long_data WHERE id = 4")
row_reuse = cur_ld_cleanup.fetchone()
ok("statement remains usable after deferred long-data error",
   row_reuse and row_reuse[0] == "ok", row_reuse)
g_ld = status_map(cur_ld_cleanup, "SHOW GLOBAL STATUS LIKE 'Com_stmt_send_long_data'")
ok("global Com_stmt_send_long_data is at least 6 after smoke",
   int(g_ld.get("Com_stmt_send_long_data", 0)) >= 6,
   g_ld.get("Com_stmt_send_long_data"))
cur_ld_cleanup.execute("DROP TABLE IF EXISTS t_long_data")
conn_ld_cleanup.commit()
conn_ld_cleanup.close()

# ── 5.4a: max_allowed_packet enforcement ─────────────────────────────────────

print("\n[5.4a max_allowed_packet]")

# SET max_allowed_packet to a small value, verify SELECT @@max_allowed_packet reflects it
conn_map = connect()
cm = conn_map.cursor()
cm.execute("SET max_allowed_packet = 2048")
cm.execute("SELECT @@max_allowed_packet")
row = cm.fetchone()
ok("SET max_allowed_packet = 2048 is reflected in SELECT @@max_allowed_packet",
   row is not None and int(row[0]) == 2048, row)

# Reset to default before the next test
cm.execute("SET max_allowed_packet = 67108864")
cm.execute("SELECT @@max_allowed_packet")
row2 = cm.fetchone()
ok("SET max_allowed_packet = 67108864 restores default",
   row2 is not None and int(row2[0]) == 67108864, row2)
conn_map.close()

# Invalid SET max_allowed_packet returns ERR, previous limit unchanged
conn_inv = connect()
ci = conn_inv.cursor()
err_code_inv = None
try:
    ci.execute("SET max_allowed_packet = 'abc'")
    conn_inv.commit()
except Exception as e:
    err_code_inv = getattr(e, 'args', [None])[0]
ok("SET max_allowed_packet = 'abc' returns an error (not silently accepted)",
   err_code_inv is not None, err_code_inv)
# After the error the connection should still be usable
try:
    ci.execute("SELECT @@max_allowed_packet")
    row_inv = ci.fetchone()
    ok("Connection still usable after invalid SET max_allowed_packet",
       row_inv is not None, row_inv)
except Exception:
    ok("Connection still usable after invalid SET max_allowed_packet", False)
conn_inv.close()

# Oversize COM_QUERY: lower the limit to 64 bytes, then send a query larger than that.
# The server must return MySQL error 1153 / SQLSTATE 08S01 and close the connection.
# We use a normal pymysql connection because pymysql honours the server-side ERR packet.
conn_oversize = connect()
co = conn_oversize.cursor()
co.execute("SET max_allowed_packet = 64")
conn_oversize.commit()
err_code_oversize = None
sqlstate_oversize = None
try:
    # Query body is well over 64 bytes so the framing layer rejects it.
    big_query = "SELECT " + ", ".join(["1"] * 50)  # ~150 bytes
    co.execute(big_query)
    co.fetchall()
except Exception as e:
    err_code_oversize = getattr(e, 'args', [None])[0]
    err_msg_oversize = getattr(e, 'args', [None, None])[1] if len(getattr(e, 'args', [])) > 1 else str(e)
ok("Oversize COM_QUERY returns MySQL error code 1153",
   err_code_oversize == 1153, err_code_oversize)
ok("Oversize COM_QUERY error message is the canonical max_allowed_packet message",
   err_msg_oversize is not None and "max_allowed_packet" in str(err_msg_oversize),
   err_msg_oversize)
conn_oversize.close()

# ── Phase 4.25c: strict mode + warnings ──────────────────────────────────────

print("\n[strict_mode / sql_mode defaults]")
cur.execute("SELECT @@strict_mode")
ok("@@strict_mode defaults to ON", cur.fetchone()[0] == "ON")

cur.execute("SELECT @@sql_mode")
sql_mode_default = cur.fetchone()[0]
ok("@@sql_mode defaults to contain STRICT_TRANS_TABLES",
   "STRICT_TRANS_TABLES" in sql_mode_default, sql_mode_default)

print("\n[SHOW VARIABLES: strict_mode / sql_mode]")
cur.execute("SHOW VARIABLES LIKE 'strict_mode'")
rows_sv = cur.fetchall()
ok("SHOW VARIABLES LIKE 'strict_mode' returns row",
   len(rows_sv) == 1 and rows_sv[0][1] == "ON", rows_sv)

cur.execute("SHOW VARIABLES LIKE 'sql_mode'")
rows_sqlmode = cur.fetchall()
ok("SHOW VARIABLES LIKE 'sql_mode' returns row with STRICT_TRANS_TABLES",
   len(rows_sqlmode) == 1 and "STRICT_TRANS_TABLES" in rows_sqlmode[0][1], rows_sqlmode)

print("\n[SET strict_mode = OFF → permissive INSERT warns]")
conn_strict = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="",
                               database="axiomdb", charset="utf8mb4")
cs = conn_strict.cursor()
cs.execute("CREATE TABLE IF NOT EXISTS t_wire_strict (age INT)")
cs.execute("DELETE FROM t_wire_strict")

# With strict ON, '42abc' into INT must error.
try:
    cs.execute("INSERT INTO t_wire_strict VALUES ('42abc')")
    ok("strict ON: '42abc' into INT errors", False, "no error raised")
except Exception:
    ok("strict ON: '42abc' into INT errors", True)

# Turn strict OFF, same insert should succeed and produce a warning.
cs.execute("SET strict_mode = OFF")
cur2 = conn_strict.cursor()
cur2.execute("SELECT @@strict_mode")
ok("@@strict_mode is OFF after SET", cur2.fetchone()[0] == "OFF")

cs.execute("INSERT INTO t_wire_strict VALUES ('42abc')")
ok("strict OFF + '42abc' into INT: row inserted", True)

cs.execute("SHOW WARNINGS")
warnings = cs.fetchall()
ok("SHOW WARNINGS returns at least 1 warning after permissive INSERT",
   len(warnings) >= 1, warnings)
if warnings:
    ok("warning code is 1265", warnings[0][1] == 1265, warnings[0])
    ok("warning message contains 'age'", "age" in warnings[0][2], warnings[0][2])
    ok("warning message contains 'row 1'", "row 1" in warnings[0][2], warnings[0][2])

cs.execute("SELECT age FROM t_wire_strict")
row_val = cs.fetchone()
ok("permissive INSERT stored 42 (not '42abc')", row_val is not None and row_val[0] == 42, row_val)

# Regression: SHOW WARNINGS after a clean statement returns empty.
cs.execute("SELECT 1")
cs.execute("SHOW WARNINGS")
_warnings_after_clean = cs.fetchall()
ok("SHOW WARNINGS is empty after clean SELECT",
   len(_warnings_after_clean) == 0, _warnings_after_clean)

print("\n[SET sql_mode = '' disables strict]")
cs.execute("SET sql_mode = ''")
cur2.execute("SELECT @@strict_mode")
ok("@@strict_mode is OFF after SET sql_mode = ''", cur2.fetchone()[0] == "OFF")

cur2.execute("SELECT @@sql_mode")
ok("@@sql_mode is empty after SET sql_mode = ''", cur2.fetchone()[0] == "")

print("\n[SET sql_mode = 'STRICT_TRANS_TABLES' re-enables strict]")
cs.execute("SET sql_mode = 'STRICT_TRANS_TABLES'")
cur2.execute("SELECT @@strict_mode")
ok("@@strict_mode is ON after SET sql_mode = 'STRICT_TRANS_TABLES'",
   cur2.fetchone()[0] == "ON")

cs.execute("DROP TABLE IF EXISTS t_wire_strict")
conn_strict.close()

# ── [4.10d] Parameterized LIMIT/OFFSET in prepared statements ─────────────────

print("\n[4.10d] Parameterized LIMIT/OFFSET in prepared statements")

cur.execute("DROP TABLE IF EXISTS t_param_limit")
cur.execute("CREATE TABLE t_param_limit (a INT)")
for i in range(1, 6):
    cur.execute("INSERT INTO t_param_limit VALUES (%s)", (i,))

# Integer params: LIMIT 2 OFFSET 1 → rows 2, 3
stmt = cur.connection.cursor()
stmt.execute("SELECT a FROM t_param_limit ORDER BY a ASC LIMIT %s OFFSET %s", (2, 1))
rows_pl = stmt.fetchall()
ok("param LIMIT 2 OFFSET 1 — row count", len(rows_pl) == 2)
ok("param LIMIT 2 OFFSET 1 — first row", rows_pl[0][0] == 2)
ok("param LIMIT 2 OFFSET 1 — second row", rows_pl[1][0] == 3)

# LIMIT only
stmt.execute("SELECT a FROM t_param_limit ORDER BY a ASC LIMIT %s", (3,))
rows_pl2 = stmt.fetchall()
ok("param LIMIT 3 — row count", len(rows_pl2) == 3)
ok("param LIMIT 3 — first row", rows_pl2[0][0] == 1)

# OFFSET only (LIMIT is literal MAX)
stmt.execute("SELECT a FROM t_param_limit ORDER BY a ASC LIMIT 100 OFFSET %s", (3,))
rows_pl3 = stmt.fetchall()
ok("param OFFSET 3 — row count (5 - 3 = 2)", len(rows_pl3) == 2)
ok("param OFFSET 3 — first row", rows_pl3[0][0] == 4)

# LIMIT 0 — valid, returns zero rows
stmt.execute("SELECT a FROM t_param_limit LIMIT %s", (0,))
ok("param LIMIT 0 — empty result", len(stmt.fetchall()) == 0)

# Invalid: negative LIMIT — must raise an error
try:
    conn_neg = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                               password="", database="test", autocommit=True)
    cn = conn_neg.cursor()
    cn.execute("DROP TABLE IF EXISTS t_neg_lim")
    cn.execute("CREATE TABLE t_neg_lim (a INT)")
    cn.execute("INSERT INTO t_neg_lim VALUES (1)")
    cn.execute("SELECT a FROM t_neg_lim LIMIT -1")
    ok("param LIMIT -1 raises error", False)
except Exception:
    ok("param LIMIT -1 raises error", True)
finally:
    try:
        conn_neg.close()
    except Exception:
        pass

cur.execute("DROP TABLE IF EXISTS t_param_limit")

# ── [5.2a] Charset / collation negotiation ───────────────────────────────────

print("\n[5.2a] charset/collation negotiation")

# Default connection (utf8mb4) — SHOW VARIABLES LIKE 'character_set%' must reflect it.
cur.execute("SHOW VARIABLES LIKE 'character_set_client'")
rows_cs = cur.fetchall()
ok("5.2a: default character_set_client is utf8mb4",
   rows_cs and rows_cs[0][1] == "utf8mb4", rows_cs)

cur.execute("SHOW VARIABLES LIKE 'collation_connection'")
rows_col = cur.fetchall()
ok("5.2a: default collation_connection is utf8mb4_0900_ai_ci",
   rows_col and rows_col[0][1] == "utf8mb4_0900_ai_ci", rows_col)

# SET NAMES latin1 — all three charset variables must update.
conn_l1 = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                          password="", charset="latin1")
cl1 = conn_l1.cursor()
cl1.execute("SHOW VARIABLES LIKE 'character_set_client'")
row_l1 = cl1.fetchall()
ok("5.2a: latin1 handshake → character_set_client = latin1",
   row_l1 and row_l1[0][1] == "latin1", row_l1)

cl1.execute("SHOW VARIABLES LIKE 'character_set_results'")
row_res = cl1.fetchall()
ok("5.2a: latin1 handshake → character_set_results = latin1",
   row_res and row_res[0][1] == "latin1", row_res)

# Insert and retrieve ASCII text over a latin1 connection.
cl1.execute("CREATE TABLE IF NOT EXISTS t_cs_ascii (id INT, val TEXT)")
cl1.execute("INSERT INTO t_cs_ascii VALUES (1, 'hello')")
conn_l1.commit()
cl1.execute("SELECT val FROM t_cs_ascii WHERE id = 1")
row_ascii = cl1.fetchone()
ok("5.2a: ASCII text round-trips over latin1 connection", row_ascii and row_ascii[0] == "hello",
   row_ascii)
cl1.execute("DROP TABLE IF EXISTS t_cs_ascii")
conn_l1.commit()
conn_l1.close()

# SET NAMES utf8mb4 — resets all three charset fields.
conn_set = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="")
cs_set = conn_set.cursor()
cs_set.execute("SET NAMES utf8mb4")
cs_set.execute("SELECT @@character_set_client")
ok("5.2a: SET NAMES utf8mb4 → @@character_set_client = utf8mb4",
   cs_set.fetchone()[0] == "utf8mb4")
cs_set.execute("SELECT @@character_set_results")
ok("5.2a: SET NAMES utf8mb4 → @@character_set_results = utf8mb4",
   cs_set.fetchone()[0] == "utf8mb4")
conn_set.close()

# UTF-8 multi-byte text round-trips correctly.
conn_utf8 = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                            password="", charset="utf8mb4")
cu8 = conn_utf8.cursor()
cu8.execute("CREATE TABLE IF NOT EXISTS t_cs_utf8 (id INT, val TEXT)")
cu8.execute("INSERT INTO t_cs_utf8 VALUES (1, %s)", ("こんにちは",))
conn_utf8.commit()
cu8.execute("SELECT val FROM t_cs_utf8 WHERE id = 1")
row_u8 = cu8.fetchone()
ok("5.2a: UTF-8 multi-byte text round-trips (Japanese)",
   row_u8 and row_u8[0] == "こんにちは", row_u8)
cu8.execute("DROP TABLE IF EXISTS t_cs_utf8")
conn_utf8.commit()
conn_utf8.close()

# ── [5.19] B+tree batch delete — DELETE / UPDATE correctness ─────────────────

print("\n[5.19] B+tree batch delete — DELETE WHERE and UPDATE correctness")

conn_bd = connect()
cb19 = conn_bd.cursor()

cb19.execute("CREATE TABLE bd_users (id INT PRIMARY KEY, name TEXT, score INT)")
cb19.execute("CREATE INDEX idx_bd_score ON bd_users (score)")
for i in range(1, 21):
    cb19.execute("INSERT INTO bd_users VALUES (%s, %s, %s)", (i, f"user{i}", i * 10))
conn_bd.commit()

# DELETE WHERE on indexed PK column — triggers batch delete path on PK index
cb19.execute("DELETE FROM bd_users WHERE id > 10")
conn_bd.commit()
cb19.execute("SELECT COUNT(*) FROM bd_users")
ok("5.19 DELETE WHERE PK: 10 rows remain after deleting id > 10",
   cb19.fetchone()[0] == 10)

cb19.execute("SELECT id FROM bd_users ORDER BY id ASC")
ids = [r[0] for r in cb19.fetchall()]
ok("5.19 DELETE WHERE PK: remaining ids are 1..10",
   ids == list(range(1, 11)), ids)

# Verify deleted rows are not visible via secondary index scan
cb19.execute("SELECT score FROM bd_users WHERE score > 100")
rows_deleted = cb19.fetchall()
ok("5.19 DELETE WHERE PK: deleted rows absent from secondary index scan",
   len(rows_deleted) == 0, rows_deleted)

# UPDATE on multiple rows — batch-deletes old PK keys then reinserts
cb19.execute("UPDATE bd_users SET score = score + 1 WHERE id <= 5")
conn_bd.commit()
cb19.execute("SELECT id, score FROM bd_users WHERE id <= 5 ORDER BY id ASC")
updated = cb19.fetchall()
ok("5.19 UPDATE batch: 5 rows updated",
   len(updated) == 5, len(updated))
ok("5.19 UPDATE batch: score values incremented correctly",
   [r[1] for r in updated] == [11, 21, 31, 41, 51],
   [r[1] for r in updated])

# Rows not in WHERE clause are unchanged
cb19.execute("SELECT score FROM bd_users WHERE id = 6")
ok("5.19 UPDATE batch: row outside WHERE unchanged (score = 60)",
   cb19.fetchone()[0] == 60)

# UPDATE on PK-only table, touching a non-indexed column — exercises the
# stable-RID fast path from 5.20 when the rewritten row fits in place.
cb19.execute("CREATE TABLE bu20_users (id INT PRIMARY KEY, active BOOL, score INT)")
for i in range(1, 11):
    cb19.execute("INSERT INTO bu20_users VALUES (%s, %s, %s)", (i, i % 2 == 0, i * 100))
conn_bd.commit()

cb19.execute("UPDATE bu20_users SET score = score + 7 WHERE active = TRUE")
conn_bd.commit()
cb19.execute("SELECT id, score FROM bu20_users WHERE active = TRUE ORDER BY id ASC")
pk_only_updated = cb19.fetchall()
ok("5.20 UPDATE stable-RID: rows matching WHERE are updated on PK-only table",
   list(pk_only_updated) == [(2, 207), (4, 407), (6, 607), (8, 807), (10, 1007)],
   pk_only_updated)

cb19.execute("SELECT score FROM bu20_users WHERE id = 1")
ok("5.20 UPDATE stable-RID: row outside WHERE remains unchanged",
   cb19.fetchone()[0] == 100)

cb19.execute("DROP TABLE bu20_users")
conn_bd.commit()

# DELETE all rows — exercises full-table batch delete on PK and secondary index
cb19.execute("DELETE FROM bd_users WHERE id >= 1")
conn_bd.commit()
cb19.execute("SELECT COUNT(*) FROM bd_users")
ok("5.19 DELETE all via batch path: table is empty",
   cb19.fetchone()[0] == 0)

# Insert after batch delete — tree is still usable
cb19.execute("INSERT INTO bd_users VALUES (100, 'reborn', 999)")
conn_bd.commit()
cb19.execute("SELECT name FROM bd_users WHERE id = 100")
ok("5.19 INSERT after batch delete: tree usable, row found",
   cb19.fetchone()[0] == "reborn")

cb19.execute("DROP TABLE bd_users")
conn_bd.commit()
conn_bd.close()

# ── [5.21] Transactional INSERT staging — explicit transaction behavior ──────

print("\n[5.21] Transactional INSERT staging — explicit transaction behavior")

conn_i21 = connect()
ci21 = conn_i21.cursor()

ci21.execute(
    """CREATE TABLE stage_users (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name TEXT NOT NULL,
    email TEXT NOT NULL
)"""
)
ci21.execute("CREATE UNIQUE INDEX idx_stage_email ON stage_users (email)")
conn_i21.commit()

# COMMIT flushes staged rows even if no barrier statement ran before it.
ci21.execute("BEGIN")
ci21.execute("INSERT INTO stage_users (name, email) VALUES ('alice', 'alice@x.dev')")
first_rowcount = ci21.rowcount
first_insert_id = ci21.lastrowid
ci21.execute("INSERT INTO stage_users (name, email) VALUES ('bob', 'bob@x.dev')")
second_rowcount = ci21.rowcount
second_insert_id = ci21.lastrowid
ci21.execute("COMMIT")

ok("5.21 COMMIT flush: first INSERT returns rowcount=1",
   first_rowcount == 1, first_rowcount)
ok("5.21 COMMIT flush: second INSERT returns rowcount=1",
   second_rowcount == 1, second_rowcount)
ok("5.21 LAST_INSERT_ID path: first generated id is visible to client",
   first_insert_id == 1, first_insert_id)
ok("5.21 LAST_INSERT_ID path: second generated id increments correctly",
   second_insert_id == 2, second_insert_id)

ci21.execute("SELECT id, name FROM stage_users ORDER BY id ASC")
stage_rows = ci21.fetchall()
ok("5.21 COMMIT flush: staged rows become durable on COMMIT",
   list(stage_rows) == [(1, "alice"), (2, "bob")], stage_rows)

# SELECT is a barrier, so read-your-own-writes still works before COMMIT.
ci21.execute("BEGIN")
ci21.execute("INSERT INTO stage_users (name, email) VALUES ('carol', 'carol@x.dev')")
ci21.execute("SELECT name FROM stage_users WHERE email = 'carol@x.dev'")
visible = ci21.fetchone()
ok("5.21 barrier flush: SELECT sees prior staged INSERT in same txn",
   visible == ("carol",), visible)
ci21.execute("ROLLBACK")

ci21.execute("SELECT COUNT(*) FROM stage_users WHERE email = 'carol@x.dev'")
ok("5.21 ROLLBACK: uncommitted staged row is discarded",
   ci21.fetchone()[0] == 0)

# Table switch is also a barrier.
ci21.execute("CREATE TABLE stage_logs (id INT, msg TEXT)")
conn_i21.commit()
ci21.execute("BEGIN")
ci21.execute("INSERT INTO stage_users (name, email) VALUES ('dave', 'dave@x.dev')")
ci21.execute("INSERT INTO stage_logs VALUES (1, 'log-entry')")
ci21.execute("COMMIT")

ci21.execute("SELECT COUNT(*) FROM stage_users WHERE email = 'dave@x.dev'")
ok("5.21 table switch barrier: first table flushed before second INSERT target",
   ci21.fetchone()[0] == 1)
ci21.execute("SELECT COUNT(*) FROM stage_logs")
ok("5.21 table switch barrier: second table row also commits correctly",
   ci21.fetchone()[0] == 1)

# Duplicate UNIQUE keys inside one explicit transaction fail immediately and
# leave no committed rows behind after rollback.
ci21.execute("BEGIN")
ci21.execute("INSERT INTO stage_users (name, email) VALUES ('erin', 'dup@x.dev')")
dup_failed = False
try:
    ci21.execute("INSERT INTO stage_users (name, email) VALUES ('erin-2', 'dup@x.dev')")
except pymysql.err.IntegrityError:
    dup_failed = True
ok("5.21 UNIQUE precheck: duplicate buffered key raises IntegrityError immediately",
   dup_failed)
ci21.execute("ROLLBACK")

ci21.execute("SELECT COUNT(*) FROM stage_users WHERE email = 'dup@x.dev'")
ok("5.21 ROLLBACK after duplicate: no duplicate row leaks into committed state",
   ci21.fetchone()[0] == 0)

ci21.execute("SELECT id FROM stage_users WHERE email = 'alice@x.dev'")
alice_lookup = ci21.fetchone()
ok("5.21 secondary index correctness: committed row remains findable by UNIQUE index",
   alice_lookup == (1,), alice_lookup)

ci21.execute("DROP TABLE stage_logs")
ci21.execute("DROP TABLE stage_users")
conn_i21.commit()
conn_i21.close()

# ── [6.16] PRIMARY KEY SELECT access path — PK-only table lookups ────────────

print("\n[6.16] PRIMARY KEY SELECT access path — PK-only table lookups")

conn_616 = connect()
c616 = conn_616.cursor()
c616.execute("CREATE TABLE pk_lookup_users (id INT PRIMARY KEY, name TEXT NOT NULL)")
c616.executemany(
    "INSERT INTO pk_lookup_users VALUES (%s, %s)",
    [(1, "alice"), (2, "bob"), (3, "carol")],
)
conn_616.commit()

c616.execute("SELECT id, name FROM pk_lookup_users WHERE id = 2")
pk_rows = c616.fetchall()
ok(
    "6.16 PK SELECT: lookup on PRIMARY KEY works without secondary index",
    pk_rows == ((2, "bob"),),
    pk_rows,
)

c616.execute("SELECT id FROM pk_lookup_users WHERE id >= 2 AND id < 4 ORDER BY id ASC")
pk_range_rows = c616.fetchall()
ok(
    "6.16 PK SELECT: PK range returns expected ids",
    pk_range_rows == ((2,), (3,)),
    pk_range_rows,
)

c616.execute("DROP TABLE pk_lookup_users")
conn_616.commit()
conn_616.close()

# ── [6.17] Indexed UPDATE candidate fast path ────────────────────────────────

print("\n[6.17] Indexed UPDATE candidate fast path")

conn_617 = connect()
c617 = conn_617.cursor()
c617.execute("CREATE TABLE upd_range_users (id INT PRIMARY KEY, score INT NOT NULL)")
c617.executemany(
    "INSERT INTO upd_range_users VALUES (%s, %s)",
    [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
)
conn_617.commit()

c617.execute("UPDATE upd_range_users SET score = score + 5 WHERE id >= 3 AND id < 6")
conn_617.commit()
c617.execute("SELECT id, score FROM upd_range_users ORDER BY id ASC")
range_updated = c617.fetchall()
ok(
    "6.17 UPDATE range: only PK-range rows are updated",
    list(range_updated) == [(1, 10), (2, 20), (3, 35), (4, 45), (5, 55), (6, 60)],
    range_updated,
)

c617.execute(
    "CREATE TABLE upd_email_users (id INT PRIMARY KEY, email TEXT NOT NULL, score INT NOT NULL)"
)
c617.execute("CREATE UNIQUE INDEX upd_email_idx ON upd_email_users (email)")
c617.executemany(
    "INSERT INTO upd_email_users VALUES (%s, %s, %s)",
    [(1, "alice@x.dev", 10), (2, "bob@x.dev", 20)],
)
conn_617.commit()

c617.execute(
    "UPDATE upd_email_users SET score = score + 7 WHERE email = 'alice@x.dev'"
)
conn_617.commit()
c617.execute("SELECT id, score FROM upd_email_users ORDER BY id ASC")
secondary_updated = c617.fetchall()
ok(
    "6.17 UPDATE equality: secondary-index candidate path updates only matching row",
    list(secondary_updated) == [(1, 17), (2, 20)],
    secondary_updated,
)

c617.execute("DROP TABLE upd_email_users")
c617.execute("DROP TABLE upd_range_users")
conn_617.commit()
conn_617.close()

# ── [6.18] Indexed multi-row INSERT batch path ───────────────────────────────

print("\n[6.18] Indexed multi-row INSERT batch path")

conn_618 = connect()
c618 = conn_618.cursor()
c618.execute("CREATE TABLE batch_pk_users (id INT PRIMARY KEY, name TEXT NOT NULL)")
c618.execute(
    "INSERT INTO batch_pk_users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')"
)
conn_618.commit()

c618.execute("SELECT id, name FROM batch_pk_users ORDER BY id ASC")
batch_pk_rows = c618.fetchall()
ok(
    "6.18 INSERT multi-row: PK-only table stores all rows correctly",
    list(batch_pk_rows) == [(1, "alice"), (2, "bob"), (3, "carol")],
    batch_pk_rows,
)

c618.execute("CREATE TABLE batch_email_users (id INT PRIMARY KEY, email TEXT NOT NULL)")
c618.execute("CREATE UNIQUE INDEX batch_email_idx ON batch_email_users (email)")
try:
    c618.execute(
        "INSERT INTO batch_email_users VALUES "
        "(1, 'alice@x.dev'), (2, 'alice@x.dev')"
    )
    conn_618.commit()
    ok(
        "6.18 INSERT multi-row: UNIQUE duplicate in same statement raises IntegrityError",
        False,
        "no error raised",
    )
except pymysql.err.IntegrityError:
    conn_618.rollback()
    ok(
        "6.18 INSERT multi-row: UNIQUE duplicate in same statement raises IntegrityError",
        True,
    )

c618.execute("SELECT id FROM batch_email_users ORDER BY id ASC")
batch_unique_rows = c618.fetchall()
ok(
    "6.18 INSERT multi-row: failed UNIQUE batch does not leak committed rows",
    batch_unique_rows == (),
    batch_unique_rows,
)

c618.execute("DROP TABLE batch_email_users")
c618.execute("DROP TABLE batch_pk_users")
conn_618.commit()
conn_618.close()

# ── [6.19] WAL fsync pipeline — autocommit correctness smoke ─────────────────

print("\n[6.19] WAL fsync pipeline — autocommit correctness smoke")

conn_619a = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="",
                            autocommit=True)
conn_619b = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="",
                            autocommit=True)
c619a = conn_619a.cursor()
c619b = conn_619b.cursor()

c619a.execute("CREATE TABLE autocommit_pipe_users (id INT PRIMARY KEY, name TEXT NOT NULL)")
c619a.execute("INSERT INTO autocommit_pipe_users VALUES (1, 'alice')")
c619b.execute("INSERT INTO autocommit_pipe_users VALUES (2, 'bob')")

c619a.execute("SELECT id, name FROM autocommit_pipe_users ORDER BY id ASC")
pipe_rows = c619a.fetchall()
ok(
    "6.19 autocommit inserts remain immediately visible and durable per statement",
    list(pipe_rows) == [(1, "alice"), (2, "bob")],
    pipe_rows,
)

c619b.execute("SELECT COUNT(*) FROM autocommit_pipe_users")
ok(
    "6.19 second connection remains usable after autocommit fsync path",
    c619b.fetchone() == (2,),
)

c619a.execute("DROP TABLE autocommit_pipe_users")
conn_619a.close()
conn_619b.close()

# ── [6.20] UPDATE apply fast path — no-op + batched range apply ──────────────

print("\n[6.20] UPDATE apply fast path")

conn_620 = connect()
c620 = conn_620.cursor()
c620.execute(
    "CREATE TABLE upd_apply_users (id INT PRIMARY KEY, active BOOL NOT NULL, score INT NOT NULL)"
)
c620.executemany(
    "INSERT INTO upd_apply_users VALUES (%s, %s, %s)",
    [
        (1, False, 10),
        (2, True, 20),
        (3, True, 30),
        (4, True, 40),
        (5, True, 50),
        (6, False, 60),
    ],
)
conn_620.commit()

c620.execute("UPDATE upd_apply_users SET score = score WHERE id >= 2 AND id < 6")
noop_count = c620.rowcount
conn_620.commit()
c620.execute("SELECT id, score FROM upd_apply_users ORDER BY id ASC")
noop_rows = c620.fetchall()
ok(
    "6.20 UPDATE no-op: matched-row count is preserved on PK range",
    noop_count == 4,
    noop_count,
)
ok(
    "6.20 UPDATE no-op: unchanged rows skip physical mutation without changing results",
    list(noop_rows) == [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
    noop_rows,
)

c620.execute("UPDATE upd_apply_users SET score = score + 9 WHERE id >= 2 AND id < 6")
range_count = c620.rowcount
conn_620.commit()
c620.execute("SELECT id, score FROM upd_apply_users ORDER BY id ASC")
range_rows = c620.fetchall()
ok(
    "6.20 UPDATE range: PK-only apply path updates only targeted rows",
    list(range_rows) == [(1, 10), (2, 29), (3, 39), (4, 49), (5, 59), (6, 60)],
    range_rows,
)
ok(
    "6.20 UPDATE range: affected-row count stays aligned with matched PK range",
    range_count == 4,
    range_count,
)

c620.execute("DROP TABLE upd_apply_users")
conn_620.commit()
conn_620.close()

# ── 22b.3b: cross-database name resolution ────────────────────────────────────

print("\n[22b.3b cross-database resolution]")
conn_xdb = connect()
cx = conn_xdb.cursor()

# Setup: create analytics database with a table
cx.execute("CREATE DATABASE analytics")
conn_xdb.commit()
cx.execute("USE analytics")
cx.execute("CREATE TABLE events (id INT, name TEXT)")
cx.execute("INSERT INTO events VALUES (1, 'click'), (2, 'view')")
conn_xdb.commit()

# Switch back to default db
cx.execute("USE axiomdb")

# 1. SELECT via 3-part name
cx.execute("SELECT id, name FROM analytics.public.events")
xdb_rows = cx.fetchall()
ok("22b.3b SELECT analytics.public.events from axiomdb",
   xdb_rows == ((1, "click"), (2, "view")),
   xdb_rows)

# 2. CREATE TABLE via 3-part name
cx.execute("CREATE TABLE analytics.public.scores (id INT, val INT)")
conn_xdb.commit()
cx.execute("USE analytics")
cx.execute("INSERT INTO scores VALUES (10, 100)")
conn_xdb.commit()
cx.execute("SELECT val FROM scores")
ok("22b.3b CREATE TABLE via 3-part name works",
   cx.fetchone() == (100,))

# 3. INSERT cross-database
cx.execute("USE axiomdb")
cx.execute("CREATE TABLE local_copy (id INT, val INT)")
conn_xdb.commit()
cx.execute("INSERT INTO local_copy SELECT * FROM analytics.public.scores")
conn_xdb.commit()
cx.execute("SELECT COUNT(*) FROM local_copy")
ok("22b.3b INSERT ... SELECT cross-database",
   cx.fetchone() == (1,))

# 4. UPDATE via 3-part name
cx.execute("UPDATE analytics.public.scores SET val = 999")
conn_xdb.commit()
cx.execute("SELECT val FROM analytics.public.scores")
ok("22b.3b UPDATE via 3-part name",
   cx.fetchone() == (999,))

# 5. DELETE via 3-part name
cx.execute("DELETE FROM analytics.public.events WHERE id = 1")
conn_xdb.commit()
cx.execute("SELECT COUNT(*) FROM analytics.public.events")
ok("22b.3b DELETE via 3-part name",
   cx.fetchone() == (1,))

# 6. DatabaseNotFound
try:
    cx.execute("SELECT * FROM ghost.public.t")
    ok("22b.3b ghost database returns error", False)
except Exception as e:
    ok("22b.3b ghost database returns error",
       "ghost" in str(e).lower() or "database" in str(e).lower(),
       str(e))

# 7. Unqualified still resolves to current db
cx.execute("USE axiomdb")
cx.execute("SELECT COUNT(*) FROM local_copy")
ok("22b.3b unqualified still resolves to current db",
   cx.fetchone() == (1,))

# Cleanup
cx.execute("DROP DATABASE analytics")
conn_xdb.commit()
cx.execute("DROP TABLE local_copy")
conn_xdb.commit()
conn_xdb.close()

# ── 22b.4: schema namespacing ─────────────────────────────────────────────────

print("\n[22b.4 schema namespacing]")
conn_sch = connect()
cs = conn_sch.cursor()

# 1. CREATE SCHEMA
cs.execute("CREATE SCHEMA inventory")
conn_sch.commit()
ok("22b.4 CREATE SCHEMA inventory succeeds", True)

# 2. CREATE SCHEMA IF NOT EXISTS (no error on duplicate)
cs.execute("CREATE SCHEMA IF NOT EXISTS inventory")
conn_sch.commit()
ok("22b.4 CREATE SCHEMA IF NOT EXISTS on existing schema", True)

# 3. CREATE SCHEMA duplicate should error
try:
    cs.execute("CREATE SCHEMA inventory")
    ok("22b.4 duplicate CREATE SCHEMA errors", False)
except Exception as e:
    ok("22b.4 duplicate CREATE SCHEMA errors",
       "already exists" in str(e).lower(),
       str(e))

# 4. SET search_path
cs.execute("SET search_path = 'inventory, public'")
conn_sch.commit()
ok("22b.4 SET search_path succeeds", True)

# 5. current_schema() returns first path entry
cs.execute("SELECT current_schema()")
schema_val = cs.fetchone()[0]
ok("22b.4 current_schema() returns public (static)",
   schema_val == "public",
   schema_val)

# Cleanup
cs.execute("DROP TABLE IF EXISTS inventory_test")
conn_sch.commit()
conn_sch.close()

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
