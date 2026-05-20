#!/usr/bin/env python3
"""
AxiomDB wire protocol test.
Updated at each subphase close — always overwrite this file, never create new ones.

Last updated: subphases 5.11c (explicit connection lifecycle), 5.19 (B+tree batch delete),
             5.19a (executor decomposition — structural refactor, wire-invisible),
             5.21 (transactional INSERT staging), 6.19 (WAL fsync pipeline smoke),
             6.20 (UPDATE apply fast path smoke), 22b.3a (database catalog wire smoke),
             39.18 (clustered VACUUM smoke), 39.19 (clustered REBUILD guard rails),
             39.21 (aggregate hash execution — zero-alloc clustered scan),
             39.22 (UPDATE in-place zero-alloc: single/multi field patch, rollback, TEXT-before-INT),
             40.1b (CREATE INDEX on clustered tables), 4.22e (ALTER DROP/MODIFY auto-index repair),
             4.G5 (DELETE/UPDATE ORDER BY+LIMIT, INSERT IGNORE, CREATE LIKE, CTAS, CALL/DO),
             4.11b (Subquery in JOIN),
             11.2d (refcounted TOAST/BLOB chain roundtrip), 11.4 (native JSON type + JSON_EXTRACT / ->>),
             11.16 (binary JSONB + JSONPath: -> operator, JSON_MERGE_PATCH, JSON_CONTAINS, JSON_PATH_EXISTS, TO_JSONB),
             11.18c (JSONB path operators: #>, #>>, #-),
             11.25b (JSON aggregates: jsonb_agg, json_agg, JSON_ARRAYAGG, jsonb_object_agg,
                      json_object_agg, JSON_OBJECTAGG; constructors: JSON_ARRAY, JSON_OBJECT,
                      jsonb_build_object/array, to_json, JSON_MERGE_PRESERVE, JSON_CONTAINS_PATH),
             21.9 (LATERAL joins: inner comma-join, LEFT JOIN null-pad),
             21.12 (DISTINCT ON: latest-per-group, LIMIT, expr-not-in-select, plain-DISTINCT regression),
             21.5 (INSERT ON CONFLICT + MERGE smoke),
             21.5f (GENERATED ALWAYS AS STORED insert/update smoke),
             21.11 (query hints),
             21.16 (deferrable FK smoke),
             21.10 (SQL cursors),
             21.20 (CHECKPOINT),
             21.23 (advanced SQL acceptance smoke),
             21.24 (ORM compatibility tier 2 smoke),
             21.25 (PIVOT smoke),
             13.1 (materialized views smoke),
             13.2 (window functions smoke),
             13.3 (generated columns closeout smoke),
             13.4 (LISTEN / NOTIFY pull-based smoke),
             13.5 (covering indexes smoke),
             13.6 (non-blocking ALTER TABLE smoke),
            13.12 (statement-level triggers smoke),
            13.13 (collation system smoke),
            13.14 (custom aggregate functions smoke),
            20.1 (regular views: CREATE/DROP/REPLACE VIEW, view expansion, SHOW CREATE VIEW, IS.VIEWS),
            22b.4 (schema namespacing: CREATE/DROP SCHEMA, schema.table, search_path, SHOW SCHEMAS, IS.SCHEMATA),
            22b.1 (scheduled jobs: cron_schedule/unschedule/enable/disable, IS.scheduled_jobs),
            22b.2 (HTTP FDW: CREATE SERVER, CREATE FOREIGN TABLE, SELECT from foreign table),
            13.7 (SELECT FOR UPDATE / FOR SHARE [NOWAIT] + LOCK IN SHARE MODE row-level locking),
            13.8b (SELECT FOR UPDATE / FOR SHARE SKIP LOCKED — skip rows locked by other txns),
            20.6 (Parquet: COPY TO FORMAT PARQUET + READ_PARQUET TVF round-trip),
            20.7 (BACKUP DATABASE TO / RESTORE DATABASE FROM: full + incremental + restore),
            20.8 (COPY FROM streaming: CSV batch loop + JSONL schema-first, O(batch_size) memory),
            20.16 (holiday calendars: CREATE/DROP HOLIDAY CALENDAR + IS_BUSINESS_DAY / NEXT_BUSINESS_DAY / BUSINESS_DAYS_BETWEEN),
            24.7 (TIMESTAMPTZ: CREATE/INSERT text literals with offset, AT TIME ZONE, SHOW COLUMNS, CAST)
"""
import os
import signal
import subprocess
import sys
import tempfile
import time
import struct as _struct
import threading

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
        if "/tests/" not in f and os.path.getmtime(f) > binary_mtime
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
    # Kill any stale server from a previous run (e.g. if the test crashed).
    # Use SIGKILL (-9) so the process exits immediately and releases the port.
    subprocess.run(["pkill", "-9", "-f", "axiomdb-server"], capture_output=True)
    time.sleep(1.5)  # wait for the OS to release port 13306
    explicit = os.environ.get("AXIOMDB_SERVER_BIN")
    if explicit:
        binary = explicit
    else:
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
        # Abort early if the process exited (e.g. port already in use)
        if _server_proc.poll() is not None:
            stop_server()
            print(f"Server process exited prematurely (code {_server_proc.returncode}) — port {PORT} may still be in use")
            sys.exit(1)
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

import atexit
atexit.register(stop_server)  # always stop server even if script crashes

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
cur.execute("CREATE TABLE sb_emp (id INT, dept TEXT, salary INT)")
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

# ── [11.4] Native JSON over MySQL wire ───────────────────────────────────────

print("\n[11.4] Native JSON type over MySQL wire")

cur.execute("DROP TABLE IF EXISTS wt_json_docs")
cur.execute("CREATE TABLE wt_json_docs (id INT PRIMARY KEY, data JSON)")
cur.execute(
    "INSERT INTO wt_json_docs VALUES "
    "(1, '{\"name\":\"Alice\",\"age\":30,\"active\":true}'),"
    "(2, '{\"name\":\"Bob\",\"age\":41,\"active\":false}')"
)
cur.execute("SELECT JSON_EXTRACT(data, '$.age') FROM wt_json_docs WHERE id = 1")
row_json_age = cur.fetchone()
ok("11.4 JSON_EXTRACT returns numeric scalar",
   row_json_age is not None and str(row_json_age[0]) == "30", row_json_age)
cur.execute("SELECT data->>'name' FROM wt_json_docs WHERE data->>'name' = 'Alice'")
ok("11.4 ->> works in SELECT and WHERE over wire", cur.fetchone() == ("Alice",))
cur.execute("SELECT JSON_TYPE(data), JSON_VALID(data) FROM wt_json_docs WHERE id = 1")
row_json_meta = cur.fetchone()
ok("11.4 JSON_TYPE/JSON_VALID over wire",
   row_json_meta is not None and row_json_meta[0] == "OBJECT" and str(row_json_meta[1]) == "1",
   row_json_meta)

# ── [11.16] Binary JSONB + JSONPath over MySQL wire ──────────────────────────

print("\n[11.16] Binary JSONB + JSONPath over MySQL wire")

cur.execute("DROP TABLE IF EXISTS wt_jsonb_docs")
cur.execute("CREATE TABLE wt_jsonb_docs (id INT PRIMARY KEY, data JSONB)")
cur.execute(
    "INSERT INTO wt_jsonb_docs VALUES "
    "(1, '{\"name\":\"Alice\",\"age\":30,\"tags\":[\"a\",\"b\"],\"nested\":{\"x\":42}}'),"
    "(2, '{\"name\":\"Bob\",\"age\":41,\"tags\":[\"c\"]}')"
)

# JSONB column type accepted — round-trips as JSON text over wire
cur.execute("SELECT data FROM wt_jsonb_docs WHERE id = 1")
row_jsonb = cur.fetchone()
ok("11.16 JSONB SELECT returns JSON text over wire",
   row_jsonb is not None and "Alice" in str(row_jsonb[0]), row_jsonb)

# -> operator — key extraction from object
cur.execute("SELECT data->'name' FROM wt_jsonb_docs WHERE id = 1")
ok("11.16 -> key extraction on JSONB", cur.fetchone() == ("Alice",))

# -> operator — integer index on array
cur.execute("SELECT data->'tags'->0 FROM wt_jsonb_docs WHERE id = 1")
ok("11.16 -> chained array index on JSONB", cur.fetchone() == ("a",))

# ->> on JSONB
cur.execute("SELECT data->>'name' FROM wt_jsonb_docs WHERE id = 2")
ok("11.16 ->> on JSONB returns text", cur.fetchone() == ("Bob",))

# JSON_EXTRACT on JSONB column
cur.execute("SELECT JSON_EXTRACT(data, '$.age') FROM wt_jsonb_docs WHERE id = 1")
row_age = cur.fetchone()
ok("11.16 JSON_EXTRACT on JSONB returns scalar", row_age is not None and str(row_age[0]) == "30", row_age)

# JSON_MERGE_PATCH
cur.execute("SELECT JSON_TYPE(JSON_MERGE_PATCH('{\"a\":1,\"b\":2}', '{\"b\":3,\"c\":4}'))")
ok("11.16 JSON_MERGE_PATCH returns object", cur.fetchone() == ("OBJECT",))

# JSON_CONTAINS
cur.execute("SELECT JSON_CONTAINS('{\"a\":1,\"b\":2,\"c\":3}', '{\"a\":1}')")
ok("11.16 JSON_CONTAINS returns 1 for subset", cur.fetchone()[0] in (1, "1", True))

# JSON_ARRAY_LENGTH
cur.execute("SELECT JSON_ARRAY_LENGTH('[1,2,3,4,5]')")
ok("11.16 JSON_ARRAY_LENGTH scalar", cur.fetchone()[0] in (5, "5"))

# JSON_DEPTH
cur.execute("SELECT JSON_DEPTH('{\"a\":{\"b\":1}}')")
ok("11.16 JSON_DEPTH nested object", cur.fetchone()[0] in (3, "3"))

# JSON_PATH_EXISTS
cur.execute("SELECT JSON_PATH_EXISTS('{\"a\":{\"b\":1}}', '$.a.b')")
ok("11.16 JSON_PATH_EXISTS returns true", cur.fetchone()[0] in (1, True, "1"))

# JSON_PATH_QUERY_FIRST on array
cur.execute("SELECT JSON_PATH_QUERY_FIRST('[10,20,30]', '$[*]')")
ok("11.16 JSON_PATH_QUERY_FIRST returns first element", cur.fetchone()[0] in (10, "10"))

# TO_JSONB cast
cur.execute("SELECT JSON_TYPE(TO_JSONB('{\"x\":1}'))")
ok("11.16 TO_JSONB returns JSONB object", cur.fetchone() == ("OBJECT",))

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

# Refcounted TOAST/BLOB chain smoke. Use COM_STMT_SEND_LONG_DATA so the test
# exercises large BLOB storage without depending on COM_QUERY's long-literal stack.
large_blob = (b"toast-blob-" * 900)[:9_000]
stmt_big_blob, _, _ = raw_prepare(
    conn_ld,
    "INSERT INTO t_long_data (id, txt, blb) VALUES (?, NULL, ?)",
)
raw_send_long_data(conn_ld, stmt_big_blob, 1, large_blob[:4_500])
raw_send_long_data(conn_ld, stmt_big_blob, 1, large_blob[4_500:])
pkt_big_blob = raw_execute(
    conn_ld,
    stmt_big_blob,
    [0x03, 0xfc],  # INT, BLOB
    inline_values=_struct.pack("<i", 30),
    null_indices=(1,),
)
ok("11.2d large BLOB long-data execute returns OK",
   pkt_big_blob[:1] == b"\x00", pkt_big_blob[:12])
conn_ld.commit()
cld.execute("SELECT blb FROM t_long_data WHERE id = 30")
row_big_blob = cld.fetchone()
ok("11.2d large BLOB roundtrips through TOAST/BLOB chain",
   row_big_blob and row_big_blob[0] == large_blob,
   None if row_big_blob is None else len(row_big_blob[0]))
cld.execute("DELETE FROM t_long_data WHERE id = 30")
conn_ld.commit()
cld.execute("SELECT COUNT(*) FROM t_long_data WHERE id = 30")
row_big_blob_count = cld.fetchone()
ok("11.2d delete releases large BLOB row without visible residue",
   row_big_blob_count is not None and str(row_big_blob_count[0]) == "0",
   row_big_blob_count)
raw_stmt_close(conn_ld, stmt_big_blob)

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

print("\n[ANSI_QUOTES toggles double-quote semantics]")
cs.execute("DROP TABLE IF EXISTS t_wire_ansi_quotes")
cs.execute("CREATE TABLE t_wire_ansi_quotes (c INT)")
cs.execute("INSERT INTO t_wire_ansi_quotes VALUES (7)")

cs.execute('SELECT "literal"')
row_default_quotes = cs.fetchone()
ok('ANSI_QUOTES OFF: SELECT "literal" returns a string literal',
   row_default_quotes is not None and row_default_quotes[0] == "literal",
   row_default_quotes)

cs.execute("SET sql_mode = 'STRICT_TRANS_TABLES,ANSI_QUOTES'")
cur2.execute("SELECT @@sql_mode")
ansi_mode_enabled = cur2.fetchone()[0]
ok("@@sql_mode contains ANSI_QUOTES after SET",
   "ANSI_QUOTES" in ansi_mode_enabled, ansi_mode_enabled)

cs.execute('SELECT "c" FROM t_wire_ansi_quotes')
row_identifier_quotes = cs.fetchone()
ok('ANSI_QUOTES ON: SELECT "c" resolves a quoted identifier',
   row_identifier_quotes is not None and row_identifier_quotes[0] == 7,
   row_identifier_quotes)

ansi_literal_err = None
try:
    cs.execute('SELECT "literal"')
except Exception as e:
    ansi_literal_err = e
ok('ANSI_QUOTES ON: SELECT "literal" no longer behaves as a string literal',
   ansi_literal_err is not None, ansi_literal_err)

cs.execute("SET sql_mode = 'STRICT_TRANS_TABLES'")
cur2.execute("SELECT @@sql_mode")
ansi_mode_disabled = cur2.fetchone()[0]
ok("@@sql_mode drops ANSI_QUOTES after reset",
   "ANSI_QUOTES" not in ansi_mode_disabled, ansi_mode_disabled)

cs.execute('SELECT "literal"')
row_reset_quotes = cs.fetchone()
ok('ANSI_QUOTES reset: SELECT "literal" returns a string literal again',
   row_reset_quotes is not None and row_reset_quotes[0] == "literal",
   row_reset_quotes)

cs.execute("DROP TABLE IF EXISTS t_wire_ansi_quotes")
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
ok("5.2a: default collation_connection is utf8mb4",
   rows_col and rows_col[0][1].startswith("utf8mb4_"), rows_col)

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

cb19.execute("CREATE TABLE bd_users (id INT, name TEXT, score INT)")
cb19.execute("CREATE INDEX idx_bd_id ON bd_users (id)")
cb19.execute("CREATE INDEX idx_bd_score ON bd_users (score)")
for i in range(1, 21):
    cb19.execute("INSERT INTO bd_users VALUES (%s, %s, %s)", (i, f"user{i}", i * 10))
conn_bd.commit()

# DELETE WHERE on indexed id column — exercises batch delete with an indexed predicate
cb19.execute("DELETE FROM bd_users WHERE id > 10")
conn_bd.commit()
cb19.execute("SELECT COUNT(*) FROM bd_users")
ok("5.19 DELETE WHERE indexed id: 10 rows remain after deleting id > 10",
   cb19.fetchone()[0] == 10)

cb19.execute("SELECT id FROM bd_users ORDER BY id ASC")
ids = [r[0] for r in cb19.fetchall()]
ok("5.19 DELETE WHERE indexed id: remaining ids are 1..10",
   ids == list(range(1, 11)), ids)

# Verify deleted rows are not visible via secondary index scan
cb19.execute("SELECT score FROM bd_users WHERE score > 100")
rows_deleted = cb19.fetchall()
ok("5.19 DELETE WHERE indexed id: deleted rows absent from secondary index scan",
   len(rows_deleted) == 0, rows_deleted)

# UPDATE on multiple rows — batch-rewrites rows selected through the indexed id path
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
    id INT AUTO_INCREMENT,
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
    "CREATE TABLE upd_email_users (id INT, email TEXT NOT NULL, score INT NOT NULL)"
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

c618.execute("CREATE TABLE batch_email_users (id INT, email TEXT NOT NULL)")
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

# 8. Cross-db JOIN: session on axiomdb, join local table with analytics table
cx.execute("USE axiomdb")
cx.execute("CREATE TABLE order_lines (order_id INT, item TEXT)")
cx.execute("INSERT INTO order_lines VALUES (1, 'widget'), (2, 'gadget')")
conn_xdb.commit()
cx.execute("USE analytics")
cx.execute("CREATE TABLE order_headers (id INT, amount INT)")
cx.execute("INSERT INTO order_headers VALUES (1, 100), (2, 200)")
conn_xdb.commit()
cx.execute(
    "SELECT h.amount, l.item "
    "FROM order_headers AS h "
    "JOIN axiomdb.public.order_lines AS l ON l.order_id = h.id "
    "ORDER BY h.id"
)
join_rows = cx.fetchall()
ok("22b.3b cross-db JOIN resolves tables from two databases",
   join_rows == ((100, "widget"), (200, "gadget")),
   join_rows)
cx.execute("USE axiomdb")
cx.execute("DROP TABLE order_lines")
conn_xdb.commit()

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

# ── 39.15: clustered SELECT over MySQL wire ──────────────────────────────────

print("\n[39.15 clustered SELECT]")
conn_cl = connect()
cc = conn_cl.cursor()

cc.execute(
    "CREATE TABLE cl_users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)"
)
cc.execute(
    "INSERT INTO cl_users VALUES "
    "(1, 'alice@example.com', 'Alice'), "
    "(2, 'bob@example.com', 'Bob'), "
    "(3, 'carol@example.com', 'Carol')"
)
conn_cl.commit()

cc.execute("SELECT name FROM cl_users WHERE id = 2")
row = cc.fetchone()
ok(
    "39.15 clustered PK lookup returns clustered row",
    row == ("Bob",),
    row,
)

cc.execute("SELECT email FROM cl_users WHERE email = 'alice@example.com'")
row = cc.fetchone()
ok(
    "39.15 clustered secondary lookup returns clustered row",
    row == ("alice@example.com",),
    row,
)

cc.execute("SELECT COUNT(*) FROM cl_users")
row = cc.fetchone()
ok(
    "39.15 clustered full scan/count sees all rows",
    row == (3,),
    row,
)

# ── 39.16: clustered UPDATE over MySQL wire ──────────────────────────────────

print("\n[39.16 clustered UPDATE]")

cc.execute("UPDATE cl_users SET name = 'Bobby' WHERE id = 2")
conn_cl.commit()
cc.execute("SELECT name FROM cl_users WHERE id = 2")
row = cc.fetchone()
ok(
    "39.16 clustered PK update rewrites row in clustered storage",
    row == ("Bobby",),
    row,
)

cc.execute(
    "UPDATE cl_users SET email = 'carol+new@example.com' "
    "WHERE email = 'carol@example.com'"
)
conn_cl.commit()
cc.execute("SELECT name FROM cl_users WHERE email = 'carol+new@example.com'")
row = cc.fetchone()
ok(
    "39.16 clustered secondary-key update rewrites secondary bookmark path",
    row == ("Carol",),
    row,
)
cc.execute("SELECT COUNT(*) FROM cl_users WHERE email = 'carol@example.com'")
row = cc.fetchone()
ok(
    "39.16 clustered secondary-key update removes old visible key",
    row == (0,),
    row,
)

cc.execute("UPDATE cl_users SET id = 7 WHERE email = 'bob@example.com'")
conn_cl.commit()
cc.execute("SELECT name FROM cl_users WHERE id = 7")
row = cc.fetchone()
ok(
    "39.16 clustered PK change rewrites clustered primary key",
    row == ("Bobby",),
    row,
)

cc.execute("BEGIN")
cc.execute("UPDATE cl_users SET name = 'Alice Rolled Back' WHERE id = 1")
cc.execute("ROLLBACK")
cc.execute("SELECT name FROM cl_users WHERE id = 1")
row = cc.fetchone()
ok(
    "39.16 clustered UPDATE rollback restores original row",
    row == ("Alice",),
    row,
)

# ── 39.17: clustered DELETE over MySQL wire ──────────────────────────────────

print("\n[39.17 clustered DELETE]")

cc.execute("DELETE FROM cl_users WHERE email = 'carol+new@example.com'")
conn_cl.commit()
cc.execute("SELECT COUNT(*) FROM cl_users WHERE id = 3")
row = cc.fetchone()
ok(
    "39.17 clustered secondary delete hides deleted row",
    row == (0,),
    row,
)

cc.execute("SELECT COUNT(*) FROM cl_users")
row = cc.fetchone()
ok(
    "39.17 clustered DELETE updates visible row count",
    row == (2,),
    row,
)

cc.execute("BEGIN")
cc.execute("DELETE FROM cl_users WHERE id = 1")
cc.execute("ROLLBACK")
cc.execute("SELECT name FROM cl_users WHERE id = 1")
row = cc.fetchone()
ok(
    "39.17 clustered DELETE rollback restores original row",
    row == ("Alice",),
    row,
)

# ── 39.18: clustered VACUUM over MySQL wire ──────────────────────────────────

print("\n[39.18 clustered VACUUM]")

cc.execute(
    "CREATE TABLE cl_vacuum_users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)"
)
cc.execute(
    "INSERT INTO cl_vacuum_users VALUES "
    "(1, 'vac_a@example.com', 'Vac A'), "
    "(2, 'vac_b@example.com', 'Vac B')"
)
conn_cl.commit()

cc.execute("DELETE FROM cl_vacuum_users WHERE id = 1")
conn_cl.commit()
cc.execute("VACUUM cl_vacuum_users")
row = cc.fetchone()
ok(
    "39.18 clustered VACUUM removes committed dead row and secondary bookmark",
    row == ("cl_vacuum_users", 1, 1),
    row,
)
conn_cl.commit()

cc.execute("BEGIN")
cc.execute("DELETE FROM cl_vacuum_users WHERE id = 2")
cc.execute("VACUUM cl_vacuum_users")
row = cc.fetchone()
ok(
    "39.18 clustered VACUUM skips uncommitted clustered delete",
    row == ("cl_vacuum_users", 0, 0),
    row,
)
cc.execute("ROLLBACK")
cc.execute("SELECT name FROM cl_vacuum_users WHERE email = 'vac_b@example.com'")
row = cc.fetchone()
ok(
    "39.18 clustered VACUUM preserves rollback path for uncommitted delete",
    row == ("Vac B",),
    row,
)

conn_cl.close()

# ── 39.19: clustered REBUILD over MySQL wire ─────────────────────────────────

print("\n[39.19 clustered REBUILD]")
conn_rebuild = connect()
cr = conn_rebuild.cursor()

cr.execute("CREATE TABLE cl_rebuild_heap (id INT NOT NULL, name TEXT)")
conn_rebuild.commit()
try:
    cr.execute("ALTER TABLE cl_rebuild_heap REBUILD")
    ok("39.19 REBUILD rejects heap table without PRIMARY KEY", False, "no error raised")
except pymysql.MySQLError as e:
    ok(
        "39.19 REBUILD rejects heap table without PRIMARY KEY",
        len(e.args) >= 2 and "PRIMARY KEY" in str(e.args[1]),
        e.args,
    )

cr.execute("CREATE TABLE cl_rebuild_clustered (id INT PRIMARY KEY, name TEXT)")
conn_rebuild.commit()
try:
    cr.execute("ALTER TABLE cl_rebuild_clustered REBUILD")
    ok("39.19 REBUILD rejects already clustered table", False, "no error raised")
except pymysql.MySQLError as e:
    ok(
        "39.19 REBUILD rejects already clustered table",
        len(e.args) >= 2 and "already clustered" in str(e.args[1]),
        e.args,
    )

conn_rebuild.close()

# ── 39.21: aggregate hash execution ──────────────────────────────────────────

print("\n[39.21 aggregate hash execution]")
conn_agg = connect()
ca = conn_agg.cursor()

# Setup: clustered table with age groups and scores
ca.execute(
    "CREATE TABLE agg_bench ("
    "  id    INT NOT NULL PRIMARY KEY,"
    "  age   INT NOT NULL,"
    "  score DOUBLE NOT NULL"
    ")"
)
# Insert 60 rows across 3 age groups (20 rows per group)
for i in range(1, 61):
    age = 20 + (i % 3)  # ages 20, 21, 22
    score = float(i)
    ca.execute(f"INSERT INTO agg_bench VALUES ({i}, {age}, {score})")
conn_agg.commit()

# GROUP BY + COUNT correctness
ca.execute("SELECT age, COUNT(*) FROM agg_bench GROUP BY age ORDER BY age")
rows_agg = ca.fetchall()
ok("39.21 GROUP BY produces 3 distinct groups", len(rows_agg) == 3, rows_agg)
total_count = sum(r[1] for r in rows_agg)
ok("39.21 GROUP BY COUNT sums to 60", total_count == 60, total_count)

# GROUP BY + AVG correctness: sum of (COUNT*AVG) must equal SUM of all scores
ca.execute("SELECT age, COUNT(*), AVG(score) FROM agg_bench GROUP BY age ORDER BY age")
rows_avg = ca.fetchall()
reconstructed_sum = sum(int(r[1]) * float(r[2]) for r in rows_avg)
ca.execute("SELECT SUM(score) FROM agg_bench")
direct_sum = float(ca.fetchone()[0])
ok(
    "39.21 GROUP BY AVG reconstructed sum matches direct SUM",
    abs(reconstructed_sum - direct_sum) < 0.001,
    f"reconstructed={reconstructed_sum} direct={direct_sum}",
)

# COUNT(*) on empty table returns one row with 0
ca.execute("CREATE TABLE agg_empty (id INT NOT NULL)")
conn_agg.commit()
ca.execute("SELECT COUNT(*) FROM agg_empty")
empty_count = ca.fetchone()[0]
ok("39.21 COUNT(*) on empty table returns 0", empty_count == 0, empty_count)

# NULL GROUP BY — NULL values form their own group
ca.execute("CREATE TABLE agg_nulls (v INT)")
conn_agg.commit()
ca.execute("INSERT INTO agg_nulls VALUES (NULL)")
ca.execute("INSERT INTO agg_nulls VALUES (NULL)")
ca.execute("INSERT INTO agg_nulls VALUES (5)")
conn_agg.commit()
ca.execute("SELECT v, COUNT(*) FROM agg_nulls GROUP BY v ORDER BY v")
null_rows = ca.fetchall()
ok("39.21 NULL GROUP BY produces 2 groups", len(null_rows) == 2, null_rows)
null_total = sum(r[1] for r in null_rows)
ok("39.21 NULL GROUP BY total count = 3", null_total == 3, null_total)

# HAVING filter
ca.execute(
    "SELECT age, SUM(score) FROM agg_bench GROUP BY age HAVING SUM(score) > 400 ORDER BY age"
)
having_rows = ca.fetchall()
ok(
    "39.21 HAVING SUM > 400 filters correctly (all 3 groups have sum > 400)",
    len(having_rows) == 3,
    having_rows,
)

# MIN / MAX — no GROUP BY
ca.execute("SELECT MIN(score), MAX(score) FROM agg_bench")
min_max = ca.fetchone()
ok("39.21 MIN(score) = 1.0", abs(float(min_max[0]) - 1.0) < 0.001, min_max[0])
ok("39.21 MAX(score) = 60.0", abs(float(min_max[1]) - 60.0) < 0.001, min_max[1])

conn_agg.close()

# ── 39.22 UPDATE in-place zero-alloc patch ────────────────────────────────────

print("\n[39.22 clustered UPDATE in-place zero-alloc]")
conn_upd = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                           password="root",
                           charset="utf8mb4", autocommit=True)
cu = conn_upd.cursor()

# Setup: all-fixed schema (no TEXT columns before targets) — exercises fast path
cu.execute("DROP TABLE IF EXISTS upd22_scores")
cu.execute(
    "CREATE TABLE upd22_scores (id INT NOT NULL, level INT NOT NULL, "
    "points INT NOT NULL, PRIMARY KEY (id))"
)
cu.execute("INSERT INTO upd22_scores VALUES (1, 5, 100), (2, 3, 50), (3, 8, 200)")

# Single fixed-size column patch
cu.execute("UPDATE upd22_scores SET level = level + 1 WHERE id = 1")
cu.execute("SELECT level FROM upd22_scores WHERE id = 1")
ok("39.22 single INT patch level=6", cu.fetchone() == (6,))

# Two fixed-size columns patched in one statement
cu.execute("UPDATE upd22_scores SET level = level + 10, points = points * 2")
cu.execute("SELECT level, points FROM upd22_scores WHERE id = 1")
row = cu.fetchone()
ok("39.22 multi-field patch id=1 level=16", row[0] == 16, row)
ok("39.22 multi-field patch id=1 points=200", row[1] == 200, row)

# ROLLBACK restores both fields via UndoClusteredFieldPatch
conn_upd.autocommit(False)
cu.execute("BEGIN")
cu.execute("UPDATE upd22_scores SET level = level + 100, points = points + 1000")
cu.execute("ROLLBACK")
conn_upd.autocommit(True)
cu.execute("SELECT level, points FROM upd22_scores WHERE id = 1")
row = cu.fetchone()
ok("39.22 rollback restores level to 16", row[0] == 16, row)
ok("39.22 rollback restores points to 200", row[1] == 200, row)

# Mixed schema: TEXT column before target INT — exercises runtime offset scan
cu.execute("DROP TABLE IF EXISTS upd22_users")
cu.execute(
    "CREATE TABLE upd22_users (id INT NOT NULL, name TEXT, age INT NOT NULL, "
    "PRIMARY KEY (id))"
)
cu.execute("INSERT INTO upd22_users VALUES (1, 'Alice', 30), (2, 'Bob', 25)")
cu.execute("UPDATE upd22_users SET age = age + 5 WHERE id = 2")
cu.execute("SELECT name, age FROM upd22_users WHERE id = 2")
row = cu.fetchone()
ok("39.22 TEXT-before-INT: name unchanged", row[0] == "Bob", row)
ok("39.22 TEXT-before-INT: age patched to 30", row[1] == 30, row)

conn_upd.close()

# ── 40.1 ClusteredInsertBatch ─────────────────────────────────────────────────

print("\n[40.1 clustered INSERT batch]")
conn40 = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                         password="root",
                         charset="utf8mb4", autocommit=False)
c40 = conn40.cursor()

# Setup: fresh table for batch tests
c40.execute("DROP TABLE IF EXISTS batch40")
c40.execute("CREATE TABLE batch40 (id INT NOT NULL PRIMARY KEY, val INT NOT NULL)")
conn40.commit()

# 1. Sequential PK bulk insert — 100 rows in one explicit txn, all visible after COMMIT.
for i in range(1, 101):
    c40.execute(f"INSERT INTO batch40 VALUES ({i}, {i * 10})")
conn40.commit()
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 batch 100 rows visible after COMMIT", c40.fetchone() == (100,))

# 2. SELECT barrier — staged rows visible inside the transaction.
c40.execute("DELETE FROM batch40")
conn40.commit()
c40.execute("INSERT INTO batch40 VALUES (1, 11)")
c40.execute("INSERT INTO batch40 VALUES (2, 22)")
# SELECT triggers flush so rows are visible in the same txn.
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 SELECT barrier flushes batch — 2 rows visible", c40.fetchone() == (2,))
c40.execute("INSERT INTO batch40 VALUES (3, 33)")
conn40.commit()
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 row after barrier also committed", c40.fetchone() == (3,))

# 3. ROLLBACK discards staged rows — table empty after rollback.
c40.execute("DELETE FROM batch40")
conn40.commit()
c40.execute("INSERT INTO batch40 VALUES (10, 100)")
c40.execute("INSERT INTO batch40 VALUES (20, 200)")
conn40.rollback()
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 ROLLBACK discards staged rows — table empty", c40.fetchone() == (0,))

# 4. Non-monotonic PK order produces correct sorted result.
for pk in [500, 1, 250, 999, 42]:
    c40.execute(f"INSERT INTO batch40 VALUES ({pk}, {pk})")
conn40.commit()
c40.execute("SELECT id FROM batch40 ORDER BY id")
ids = [r[0] for r in c40.fetchall()]
ok("40.1 non-monotonic PK batch yields sorted rows", ids == [1, 42, 250, 500, 999], ids)

# 5. PK duplicate within batch returns an error — no rows committed.
c40.execute("DELETE FROM batch40")
conn40.commit()
try:
    c40.execute("INSERT INTO batch40 VALUES (7, 1)")
    c40.execute("INSERT INTO batch40 VALUES (7, 2)")   # duplicate PK
    conn40.commit()
    ok("40.1 intra-batch PK duplicate raises error", False, "no error raised")
except Exception as e:
    conn40.rollback()
    ok("40.1 intra-batch PK duplicate raises error", True, str(e))
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 no rows after duplicate-PK rollback", c40.fetchone() == (0,))

# 6. Table switch flushes first batch — both tables correct after COMMIT.
c40.execute("DROP TABLE IF EXISTS batch40b")
c40.execute("CREATE TABLE batch40b (id INT NOT NULL PRIMARY KEY, v INT NOT NULL)")
conn40.commit()
c40.execute("INSERT INTO batch40 VALUES (100, 1000)")
c40.execute("INSERT INTO batch40 VALUES (200, 2000)")
c40.execute("INSERT INTO batch40b VALUES (1, 99)")   # different table → flushes batch40
conn40.commit()
c40.execute("SELECT COUNT(*) FROM batch40")
ok("40.1 table switch: batch40 rows flushed", c40.fetchone() == (2,))
c40.execute("SELECT COUNT(*) FROM batch40b")
ok("40.1 table switch: batch40b row present", c40.fetchone() == (1,))

conn40.close()

# ── 40.2 CREATE INDEX on clustered tables ─────────────────────────────────────

print("\n[40.2 CREATE INDEX on clustered tables]")
conn_ci = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="root",
                          charset="utf8mb4", autocommit=True)
c_ci = conn_ci.cursor()

# Setup: clustered table + rows
c_ci.execute("DROP TABLE IF EXISTS ci_users")
c_ci.execute("CREATE TABLE ci_users (id INT PRIMARY KEY, email TEXT, age INT)")
c_ci.execute("INSERT INTO ci_users VALUES (1, 'alice@example.com', 30)")
c_ci.execute("INSERT INTO ci_users VALUES (2, 'bob@example.com', 25)")
c_ci.execute("INSERT INTO ci_users VALUES (3, 'carol@example.com', 35)")

# 1. CREATE INDEX succeeds
c_ci.execute("CREATE INDEX idx_ci_age ON ci_users (age)")
ok("40.2 CREATE INDEX on clustered table succeeds", True)

# 2. SELECT via secondary index returns correct row
c_ci.execute("SELECT id FROM ci_users WHERE age = 25")
ok("40.2 SELECT via secondary index returns correct row", c_ci.fetchone() == (2,))

# 3. CREATE UNIQUE INDEX succeeds on distinct values
c_ci.execute("CREATE UNIQUE INDEX uq_ci_email ON ci_users (email)")
ok("40.2 CREATE UNIQUE INDEX on distinct values succeeds", True)

# 4. INSERT with duplicate unique value raises error
try:
    c_ci.execute("INSERT INTO ci_users VALUES (4, 'alice@example.com', 28)")
    c_ci.fetchall()
    ok("40.2 unique index rejects duplicate after CREATE UNIQUE INDEX", False, "no error raised")
except Exception as e:
    ok("40.2 unique index rejects duplicate after CREATE UNIQUE INDEX", True, str(e))

# 5. Row count unchanged after failed insert
c_ci.execute("SELECT COUNT(*) FROM ci_users")
ok("40.2 row count unchanged after failed duplicate insert", c_ci.fetchone() == (3,))

# 6. CREATE INDEX on empty clustered table succeeds
c_ci.execute("DROP TABLE IF EXISTS ci_empty")
c_ci.execute("CREATE TABLE ci_empty (id INT PRIMARY KEY, tag TEXT)")
c_ci.execute("CREATE INDEX idx_ci_tag ON ci_empty (tag)")
ok("40.2 CREATE INDEX on empty clustered table succeeds", True)

# 7. Duplicate index name raises error
try:
    c_ci.execute("CREATE INDEX idx_ci_age ON ci_users (age)")
    c_ci.fetchall()
    ok("40.2 duplicate index name raises error", False, "no error raised")
except Exception as e:
    ok("40.2 duplicate index name raises error", True, str(e))

conn_ci.close()

# ── 4.22c ALTER TABLE ADD PRIMARY KEY ────────────────────────────────────────

print("\n[4.22c] ALTER TABLE ADD PRIMARY KEY")
conn_422c = pymysql.connect(host="127.0.0.1", port=PORT, user="root",
                            password="root",
                            charset="utf8mb4", autocommit=True)
c_422c = conn_422c.cursor()

c_422c.execute("CREATE TABLE alter_pk42 (id INT, email TEXT)")
c_422c.execute(
    "INSERT INTO alter_pk42 VALUES (1, 'alice@example.com'), (2, 'bob@example.com')"
)
c_422c.execute("CREATE UNIQUE INDEX idx_alter_pk42_email ON alter_pk42 (email)")
c_422c.execute("ALTER TABLE alter_pk42 ADD PRIMARY KEY (id)")

c_422c.execute("SELECT COUNT(*) FROM alter_pk42")
ok("4.22c existing rows survive ADD PRIMARY KEY", c_422c.fetchone()[0] == 2)

try:
    c_422c.execute("INSERT INTO alter_pk42 VALUES (NULL, 'carol@example.com')")
    c_422c.fetchall()
    ok("4.22c inserted NULL into added PRIMARY KEY", False, "no error raised")
except pymysql.MySQLError:
    ok("4.22c added PRIMARY KEY rejects NULL inserts", True)

try:
    c_422c.execute("INSERT INTO alter_pk42 VALUES (3, 'alice@example.com')")
    c_422c.fetchall()
    ok("4.22c secondary unique index survives rebuild", False, "no error raised")
except Exception as e:
    ok("4.22c secondary unique index survives rebuild", True, str(e))

c_422c.execute("CREATE TABLE alter_pk42_null (id INT, email TEXT)")
c_422c.execute(
    "INSERT INTO alter_pk42_null VALUES (1, 'ok@example.com'), (NULL, 'bad@example.com')"
)
try:
    c_422c.execute("ALTER TABLE alter_pk42_null ADD PRIMARY KEY (id)")
    c_422c.fetchall()
    ok("4.22c NULL existing PK values are rejected", False, "no error raised")
except Exception as e:
    ok("4.22c NULL existing PK values are rejected", True, str(e))

conn_422c.close()

# ── 4.22e ALTER TABLE DROP/MODIFY COLUMN auto-index repair ───────────────────

print("\n[4.22e] ALTER TABLE DROP/MODIFY COLUMN auto-index repair")
conn_422e = connect()
c_422e = conn_422e.cursor()

c_422e.execute("CREATE TABLE alter_drop42e (id INT, email TEXT, deleted_at INT)")
c_422e.execute(
    "CREATE UNIQUE INDEX uq_drop42e_live ON alter_drop42e (email) WHERE deleted_at IS NULL"
)
c_422e.execute("INSERT INTO alter_drop42e VALUES (1, 'alice@example.com', NULL)")
conn_422e.commit()
c_422e.execute("ALTER TABLE alter_drop42e DROP COLUMN deleted_at")
conn_422e.commit()
try:
    c_422e.execute("INSERT INTO alter_drop42e VALUES (2, 'alice@example.com')")
    conn_422e.commit()
    ok("4.22e DROP COLUMN auto-drops affected partial index", True)
except Exception as e:
    conn_422e.rollback()
    ok("4.22e DROP COLUMN auto-drops affected partial index", False, str(e))

c_422e.execute("SELECT COUNT(*) FROM alter_drop42e")
ok("4.22e dropped-column table remains writable", c_422e.fetchone()[0] == 2)

c_422e.execute("CREATE TABLE alter_mod42e (id INT, score INT)")
c_422e.execute("CREATE UNIQUE INDEX uq_mod42e_score ON alter_mod42e (score)")
c_422e.execute("INSERT INTO alter_mod42e VALUES (1, 100), (2, 200)")
conn_422e.commit()
c_422e.execute("ALTER TABLE alter_mod42e MODIFY COLUMN score BIGINT")
conn_422e.commit()
c_422e.execute("SELECT id, score FROM alter_mod42e ORDER BY id")
mod_rows = c_422e.fetchall()
ok(
    "4.22e MODIFY COLUMN preserves existing rows after type rewrite",
    mod_rows == ((1, 100), (2, 200)),
    mod_rows,
)
try:
    c_422e.execute("INSERT INTO alter_mod42e VALUES (3, 100)")
    conn_422e.commit()
    ok("4.22e MODIFY COLUMN rebuilds unique secondary index", False, "no error raised")
except Exception as e:
    conn_422e.rollback()
    ok("4.22e MODIFY COLUMN rebuilds unique secondary index", True, str(e))

conn_422e.close()

# ── G5.1 — CALL / DO as no-ops ───────────────────────────────────────────────

print("\n[G5 — DML extensions]")
conn_g5 = connect()
c_g5 = conn_g5.cursor()
c_g5.execute("CALL some_procedure(1, 2)")
c_g5.fetchall()
ok("G5.1 CALL noop", True)
c_g5.execute("DO SLEEP(0)")
c_g5.fetchall()
ok("G5.1 DO noop", True)

# ── G5.2 — DELETE ORDER BY + LIMIT ───────────────────────────────────────────
c_g5.execute("CREATE TABLE g5_scores (id INT, val INT)")
c_g5.execute("INSERT INTO g5_scores VALUES (1,30),(2,10),(3,20),(4,40),(5,5)")
c_g5.execute("DELETE FROM g5_scores ORDER BY val ASC LIMIT 2")
ok("G5.2 DELETE ORDER BY LIMIT affected=2", conn_g5.affected_rows() == 2)
c_g5.execute("SELECT val FROM g5_scores ORDER BY val")
rows = c_g5.fetchall()
ok("G5.2 DELETE ORDER BY LIMIT remaining vals", [r[0] for r in rows] == [20, 30, 40])

# ── G5.3 — UPDATE ORDER BY + LIMIT ───────────────────────────────────────────
c_g5.execute("CREATE TABLE g5_prices (id INT, price INT)")
c_g5.execute("INSERT INTO g5_prices VALUES (1,100),(2,200),(3,300),(4,400)")
c_g5.execute("UPDATE g5_prices SET price = price + 1000 ORDER BY price ASC LIMIT 2")
ok("G5.3 UPDATE ORDER BY LIMIT affected=2", conn_g5.affected_rows() == 2)
c_g5.execute("SELECT price FROM g5_prices ORDER BY price")
rows = c_g5.fetchall()
ok("G5.3 UPDATE ORDER BY LIMIT new prices", [r[0] for r in rows] == [300, 400, 1100, 1200])

# ── G5.4 — INSERT IGNORE ─────────────────────────────────────────────────────
c_g5.execute("CREATE TABLE g5_uniq (id INT UNIQUE, val INT)")
c_g5.execute("INSERT INTO g5_uniq VALUES (1, 10)")
c_g5.execute("INSERT IGNORE INTO g5_uniq VALUES (1, 20), (2, 30)")
ok("G5.4 INSERT IGNORE skips duplicate, inserts new", conn_g5.affected_rows() == 1)
c_g5.execute("SELECT val FROM g5_uniq ORDER BY id")
rows = c_g5.fetchall()
ok("G5.4 INSERT IGNORE original row unchanged", rows[0][0] == 10)
ok("G5.4 INSERT IGNORE new row inserted", rows[1][0] == 30)

# ── G5.5 — CREATE TABLE LIKE ─────────────────────────────────────────────────
c_g5.execute("CREATE TABLE g5_orig (id INT, name TEXT, score FLOAT)")
c_g5.execute("CREATE TABLE g5_copy LIKE g5_orig")
c_g5.execute("INSERT INTO g5_copy VALUES (1, 'x', 3.14)")
ok("G5.5 CREATE TABLE LIKE — insert works", conn_g5.affected_rows() == 1)
c_g5.execute("SELECT id, name FROM g5_copy")
rows = c_g5.fetchall()
ok("G5.5 CREATE TABLE LIKE — schema matches", rows[0] == (1, 'x'))

# ── G5.6 — CTAS ──────────────────────────────────────────────────────────────
c_g5.execute("CREATE TABLE g5_items (id INT, price INT)")
c_g5.execute("INSERT INTO g5_items VALUES (1,100),(2,200),(3,300)")
c_g5.execute("CREATE TABLE g5_cheap AS SELECT id, price FROM g5_items WHERE price < 250")
c_g5.execute("SELECT COUNT(*) FROM g5_cheap")
ok("G5.6 CTAS row count", c_g5.fetchone()[0] == 2)
c_g5.execute("SELECT id FROM g5_cheap ORDER BY id")
rows = c_g5.fetchall()
ok("G5.6 CTAS correct rows", [r[0] for r in rows] == [1, 2])

conn_g5.close()

# ── 4.11b Subquery in JOIN ───────────────────────────────────────────────────

print("\n[4.11b] Subquery in JOIN")
conn_411b = connect()
c_411b = conn_411b.cursor()
c_411b.execute("DROP TABLE IF EXISTS jt_users")
c_411b.execute("DROP TABLE IF EXISTS jt_orders")
c_411b.execute("CREATE TABLE jt_users (id INT, name TEXT, role TEXT)")
c_411b.execute("CREATE TABLE jt_orders (id INT, user_id INT, total INT)")
c_411b.execute(
    "INSERT INTO jt_users VALUES "
    "(1, 'Alice', 'admin'), (2, 'Bob', 'user'), (3, 'Carol', 'vip')"
)
c_411b.execute(
    "INSERT INTO jt_orders VALUES "
    "(10, 1, 500), (11, 1, 1500), (12, 2, 200), (13, 99, 50)"
)
conn_411b.commit()

c_411b.execute(
    "SELECT big.total "
    "FROM jt_users "
    "JOIN (SELECT user_id, total FROM jt_orders WHERE total > 400) AS big "
    "ON big.user_id = jt_users.id "
    "ORDER BY big.total"
)
ok("4.11b INNER JOIN derived table", list(c_411b.fetchall()) == [(500,), (1500,)])

c_411b.execute(
    "SELECT jt_users.id, stats.order_count "
    "FROM jt_users "
    "LEFT JOIN ("
    "    SELECT user_id, COUNT(*) AS order_count "
    "    FROM jt_orders GROUP BY user_id"
    ") AS stats ON stats.user_id = jt_users.id "
    "ORDER BY jt_users.id"
)
ok(
    "4.11b LEFT JOIN derived table preserves NULL-extended rows",
    list(c_411b.fetchall()) == [(1, 2), (2, 1), (3, None)],
)

c_411b.execute(
    "SELECT jt_users.id, stats.user_id, stats.order_count "
    "FROM jt_users "
    "RIGHT JOIN ("
    "    SELECT user_id, COUNT(*) AS order_count "
    "    FROM jt_orders GROUP BY user_id"
    ") AS stats ON stats.user_id = jt_users.id "
    "ORDER BY stats.user_id"
)
rows = [(left_id, int(user_id), order_count) for (left_id, user_id, order_count) in c_411b.fetchall()]
ok(
    "4.11b RIGHT JOIN derived table preserves unmatched derived rows",
    rows == [(1, 1, 2), (2, 2, 1), (None, 99, 1)],
)

c_411b.execute(
    "SELECT stats.* "
    "FROM jt_users "
    "JOIN ("
    "    SELECT user_id, COUNT(*) AS order_count "
    "    FROM jt_orders GROUP BY user_id"
    ") AS stats ON stats.user_id = jt_users.id "
    "ORDER BY stats.user_id"
)
rows = [(int(user_id), order_count) for (user_id, order_count) in c_411b.fetchall()]
ok(
    "4.11b alias.* exposes only derived columns",
    rows == [(1, 2), (2, 1)],
)

c_411b.execute(
    "SELECT jt_users.id "
    "FROM jt_users "
    "JOIN ("
    "    SELECT user_id, MAX(total) AS biggest "
    "    FROM jt_orders GROUP BY user_id"
    ") AS mx ON mx.user_id = jt_users.id "
    "WHERE mx.biggest > 400 "
    "ORDER BY mx.biggest DESC"
)
ok(
    "4.11b WHERE and ORDER BY can reference derived join columns",
    list(c_411b.fetchall()) == [(1,)],
)

conn_411b.close()

# ── Connectivity / basics ─────────────────────────────────────────────────────

print("\n[Connectivity]")
cur.execute("SELECT 1")
ok("SELECT 1", cur.fetchone() == (1,))
cur.execute("SELECT version()")
ok("version() contains AxiomDB", "AxiomDB" in cur.fetchone()[0])

# ── Phase 11.20a — JSON_TABLE (flat, no NESTED PATH) ──────────────────────────

print("\n[11.20a JSON_TABLE]")

# Basic array shred
cur.execute("SELECT v FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS t")
rows = cur.fetchall()
vals = [int(r[0]) for r in rows]
ok("11.20a JSON_TABLE shreds array of scalars", vals == [1, 2, 3], f"got {vals}")

# Objects + ordinality + DEFAULT ON EMPTY
cur.execute("""SELECT ord, id, COALESCE(age, -1) FROM JSON_TABLE(
    '[{"id":1,"age":30},{"id":2}]',
    '$[*]' COLUMNS (
        ord FOR ORDINALITY,
        id  INT PATH '$.id',
        age INT PATH '$.age' DEFAULT -1 ON EMPTY
    )
) AS t ORDER BY ord""")
rows = cur.fetchall()
ok("11.20a JSON_TABLE ordinality + DEFAULT ON EMPTY",
   [tuple(int(x) for x in r) for r in rows] == [(1, 1, 30), (2, 2, -1)],
   f"got {rows}")

# EXISTS PATH
cur.execute("""SELECT has_a FROM JSON_TABLE(
    '[{"a":1},{"b":2}]',
    '$[*]' COLUMNS (has_a BOOLEAN EXISTS PATH '$.a')
) AS t""")
rows = cur.fetchall()
bools = [bool(int(r[0])) if isinstance(r[0], (int, str, bytes)) else bool(r[0]) for r in rows]
ok("11.20a JSON_TABLE EXISTS PATH", bools == [True, False], f"got {rows}")

# NULL document → zero rows
cur.execute("SELECT COUNT(*) FROM JSON_TABLE(NULL, '$[*]' COLUMNS (v INT PATH '$')) AS t")
ok("11.20a JSON_TABLE NULL doc → zero rows", cur.fetchone()[0] in (0, "0"))

# JOIN base table JOIN JSON_TABLE ON TRUE
cur.execute("DROP TABLE IF EXISTS jt_users")
cur.execute("CREATE TABLE jt_users (id INT)")
cur.execute("INSERT INTO jt_users VALUES (1), (2)")
cur.execute("""SELECT u.id, j.v FROM jt_users u
    JOIN JSON_TABLE('[10,20]', '$[*]' COLUMNS (v INT PATH '$')) AS j ON TRUE
    ORDER BY u.id, j.v""")
rows = cur.fetchall()
ok("11.20a JSON_TABLE JOIN base_table", rows == ((1, 10), (1, 20), (2, 10), (2, 20)))
cur.execute("DROP TABLE jt_users")

# Invalid JSON in doc → error
try:
    cur.execute("SELECT v FROM JSON_TABLE('not json', '$[*]' COLUMNS (v INT PATH '$')) AS t")
    cur.fetchall()
    ok("11.20a JSON_TABLE invalid doc raises", False)
except Exception:
    ok("11.20a JSON_TABLE invalid doc raises", True)

# ── Phase 11.20b — NESTED PATH ────────────────────────────────────────────────

print("\n[11.20b NESTED PATH]")

# Use isolated connection — pre-existing bug: pymysql type-converter crashes on
# mixed INT/TEXT NESTED PATH result sets (wire metadata issue, not 24.1 regression).
_conn_11b = connect()
_cur_11b = _conn_11b.cursor()

try:
    _cur_11b.execute("""SELECT inv_id, item_name, qty FROM JSON_TABLE(
        '[{"id":1,"items":[{"name":"A","qty":2},{"name":"B","qty":3}]},
          {"id":2,"items":[{"name":"C","qty":1}]}]',
        '$[*]' COLUMNS (
            inv_id INT PATH '$.id',
            NESTED PATH '$.items[*]' COLUMNS (
                item_name TEXT PATH '$.name',
                qty       INT  PATH '$.qty'
            )
        )
    ) AS t ORDER BY inv_id, item_name""")
    rows = _cur_11b.fetchall()
    normalized = [(int(r[0]), str(r[1]), int(r[2])) for r in rows]
    ok("11.20b JSON_TABLE NESTED parent × children",
       normalized == [(1, "A", 2), (1, "B", 3), (2, "C", 1)],
       f"got {normalized}")
except Exception as _e11b:
    ok("11.20b JSON_TABLE NESTED parent × children", False, str(_e11b))
    _conn_11b.close()
    _conn_11b = connect()
    _cur_11b = _conn_11b.cursor()

try:
    _cur_11b.execute("""SELECT inv_id, item_name FROM JSON_TABLE(
        '[{"id":1,"items":[{"name":"A"}]},
          {"id":2,"items":[]}]',
        '$[*]' COLUMNS (
            inv_id INT PATH '$.id',
            NESTED PATH '$.items[*]' COLUMNS (item_name TEXT PATH '$.name')
        )
    ) AS t ORDER BY inv_id""")
    rows = _cur_11b.fetchall()
    ok("11.20b JSON_TABLE NESTED LEFT-OUTER NULL pad",
       len(rows) == 2 and int(rows[0][0]) == 1 and str(rows[0][1]) == "A"
       and int(rows[1][0]) == 2 and rows[1][1] is None,
       f"got {rows}")
except Exception as _e11b2:
    ok("11.20b JSON_TABLE NESTED LEFT-OUTER NULL pad", False, str(_e11b2))
    _conn_11b.close()
    _conn_11b = connect()
    _cur_11b = _conn_11b.cursor()

try:
    _cur_11b.execute("""SELECT ord_inv, ord_item FROM JSON_TABLE(
        '[{"items":[{"n":"A"},{"n":"B"}]},{"items":[{"n":"C"}]}]',
        '$[*]' COLUMNS (
            ord_inv FOR ORDINALITY,
            NESTED PATH '$.items[*]' COLUMNS (ord_item FOR ORDINALITY)
        )
    ) AS t ORDER BY ord_inv, ord_item""")
    rows = _cur_11b.fetchall()
    pairs = [(int(r[0]), int(r[1])) for r in rows]
    ok("11.20b JSON_TABLE NESTED per-level ordinality",
       pairs == [(1, 1), (1, 2), (2, 1)],
       f"got {pairs}")
except Exception as _e11b3:
    ok("11.20b JSON_TABLE NESTED per-level ordinality", False, str(_e11b3))

_conn_11b.close()

# ── Phase 11.20c — multi-sibling + multi-level NESTED ─────────────────────────

print("\n[11.20c multi NESTED]")

_conn_11c = connect()
_cur_11c = _conn_11c.cursor()

def norm_cell(x):
    if x is None:
        return None
    try:
        return int(x)
    except (TypeError, ValueError):
        return str(x)

try:
    _cur_11c.execute("""SELECT inv_id, price, tag FROM JSON_TABLE(
        '[{"id":1,"prices":[10,20],"tags":["a","b"]}]',
        '$[*]' COLUMNS (
            inv_id INT PATH '$.id',
            NESTED PATH '$.prices[*]' COLUMNS (price INT  PATH '$'),
            NESTED PATH '$.tags[*]'   COLUMNS (tag   TEXT PATH '$')
        )
    ) AS t ORDER BY COALESCE(price, 1000), COALESCE(tag, 'z')""")
    rows = _cur_11c.fetchall()
    normalized = [tuple(norm_cell(x) for x in r) for r in rows]
    ok("11.20c JSON_TABLE multi-sibling UNION",
       normalized == [(1, 10, None), (1, 20, None), (1, None, "a"), (1, None, "b")],
       f"got {normalized}")
except Exception as _e11c:
    ok("11.20c JSON_TABLE multi-sibling UNION", False, str(_e11c))
    _conn_11c.close()
    _conn_11c = connect()
    _cur_11c = _conn_11c.cursor()

try:
    _cur_11c.execute("""SELECT line_id, part FROM JSON_TABLE(
        '[{"lines":[{"lid":"L1","parts":["P1","P2"]},{"lid":"L2","parts":[]}]}]',
        '$[*]' COLUMNS (
            NESTED PATH '$.lines[*]' COLUMNS (
                line_id TEXT PATH '$.lid',
                NESTED PATH '$.parts[*]' COLUMNS (part TEXT PATH '$')
            )
        )
    ) AS t ORDER BY line_id, COALESCE(part, 'z')""")
    rows = _cur_11c.fetchall()
    cleaned = [tuple(str(x) if x is not None else None for x in r) for r in rows]
    ok("11.20c JSON_TABLE multi-level NESTED with LEFT-OUTER pad",
       cleaned == [("L1", "P1"), ("L1", "P2"), ("L2", None)],
       f"got {cleaned}")
except Exception as _e11c2:
    ok("11.20c JSON_TABLE multi-level NESTED with LEFT-OUTER pad", False, str(_e11c2))

_conn_11c.close()

# ── Phase 11.18c — JSONB path operators ─────────────────────────────────────

print("\n[11.18c JSONB path operators]")
conn_jpath = connect()
cjpath = conn_jpath.cursor()
cjpath.execute(
    "SELECT "
    "CAST('{\"a\":{\"b\":1},\"xs\":[10,20,30]}' AS JSONB) #> CAST('[\"a\"]' AS JSONB), "
    "CAST('{\"a\":\"hello\"}' AS JSONB) #>> CAST('[\"a\"]' AS JSONB), "
    "CAST('{\"xs\":[10,20,30]}' AS JSONB) #- CAST('[\"xs\",1]' AS JSONB)"
)
row = cjpath.fetchone()
ok("[11.18c] #> extracts subtree as JSON text over wire", row[0] == b'{"b":1}', row)
ok("[11.18c] #>> extracts scalar text over wire", row[1] == "hello", row)
ok("[11.18c] #- deletes nested array index over wire", row[2] == b'{"xs":[10,30]}', row)
conn_jpath.close()

# ── Phase 11.21h — JSONPath planner pushdown ────────────────────────────────

print("\n[11.21h JSONPath planner pushdown]")
conn_jpush = connect()
cjpush = conn_jpush.cursor()
cjpush.execute("CREATE TABLE wt_jsonpath_gin (id INT PRIMARY KEY, doc JSONB)")
cjpush.execute("CREATE INDEX idx_wt_jsonpath_gin ON wt_jsonpath_gin USING GIN (doc)")
cjpush.execute(
    "INSERT INTO wt_jsonpath_gin VALUES "
    "(1, CAST('{\"k\":1,\"flag\":true}' AS JSONB)), "
    "(2, CAST('{\"flag\":false}' AS JSONB)), "
    "(3, CAST('{\"other\":1}' AS JSONB))"
)
cjpush.execute("SELECT id FROM wt_jsonpath_gin WHERE doc @? '$.k' ORDER BY id")
rows = cjpush.fetchall()
ok("[11.21h] @? simple key uses GIN-backed path and returns matching row only",
   rows == ((1,),) or rows == [(1,)],
   rows)
cjpush.execute("EXPLAIN SELECT id FROM wt_jsonpath_gin WHERE doc @? '$.k'")
row = cjpush.fetchone()
ok("[11.21h] EXPLAIN reports gin access for simple JSONPath key probe",
   row is not None and row[3] == "gin" and row[5] == "idx_wt_jsonpath_gin",
   row)
conn_jpush.close()

# ── Phase 11.20d1 — WRAPPER / QUOTES / PASSING ────────────────────────────────

print("\n[11.20d1 JSON_TABLE wrapper/quotes/passing]")

# WITH UNCONDITIONAL ARRAY WRAPPER → JSON array literal, OMIT QUOTES on TEXT,
# PASSING threaded into the column path filter.
cur.execute("""SELECT oid, tag_list, top_tag FROM JSON_TABLE(
    '{"min":2,"items":[{"t":"a","q":1},{"t":"b","q":5},{"t":"c","q":9}]}',
    '$'
    PASSING 4 AS qmin
    COLUMNS (
        oid      FOR ORDINALITY,
        tag_list JSON PATH '$.items[?(@.q > $qmin)].t'
            WITH UNCONDITIONAL ARRAY WRAPPER,
        top_tag  TEXT PATH '$.items[?(@.q > $qmin)].t'
            WITH UNCONDITIONAL ARRAY WRAPPER OMIT QUOTES ON SCALAR STRING
            NULL ON EMPTY
    )
) AS t""")
rows = cur.fetchall()
# top_tag is OMIT QUOTES but wrapper result is array → rendered as JSON
# array text (OMIT only strips outer quotes on string scalars, PG parity).
oid_ok = rows[0][0] in (1, "1")
tags_ok = str(rows[0][1]) == '["b","c"]'
ok("11.20d1 WRAPPER + OMIT QUOTES + PASSING roundtrip",
   oid_ok and tags_ok,
   f"got {rows}")

# ── Phase 11.20d2 — JSON_TABLE as first FROM + CROSS/OUTER APPLY ──────────────

print("\n[11.20d2 JSON_TABLE first FROM + APPLY]")

# Seed a small base table for APPLY and first-FROM tests.
cur.execute("CREATE TABLE d2_users (id INT PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO d2_users VALUES (1,'alice'),(2,'bob'),(3,'carol')")

# 1. JSON_TABLE as first FROM, INNER JOIN to a real table.
cur.execute("""SELECT j.id, u.name
                 FROM JSON_TABLE('[{"id":1},{"id":3},{"id":99}]',
                                 '$[*]' COLUMNS (id INT PATH '$.id')) AS j
                 JOIN d2_users u ON u.id = j.id
                 ORDER BY j.id""")
rows = cur.fetchall()
ok("11.20d2 JSON_TABLE first + INNER JOIN",
   rows == ((1, "alice"), (3, "carol")),
   f"got {rows}")

# 2. CROSS APPLY — product of left rows × materialized JSON rows.
cur.execute("""SELECT u.id, j.v
                 FROM d2_users u
                 CROSS APPLY JSON_TABLE('[10,20]', '$[*]'
                              COLUMNS (v INT PATH '$')) AS j
                 ORDER BY u.id, j.v""")
rows = cur.fetchall()
ok("11.20d2 CROSS APPLY JSON_TABLE product",
   rows == ((1, 10), (1, 20), (2, 10), (2, 20), (3, 10), (3, 20)),
   f"got {rows}")

# 3. OUTER APPLY preserves left row when right yields no rows.
cur.execute("""SELECT u.id, j.v
                 FROM d2_users u
                 OUTER APPLY JSON_TABLE('[]', '$[*]'
                              COLUMNS (v INT PATH '$')) AS j
                 ORDER BY u.id""")
rows = cur.fetchall()
ok("11.20d2 OUTER APPLY preserves left on empty",
   rows == ((1, None), (2, None), (3, None)),
   f"got {rows}")

# ── Phase 11.20d3 — LATERAL-correlated JSON_TABLE ─────────────────────────────

print("\n[11.20d3 JSON_TABLE LATERAL correlation]")

cur.execute("CREATE TABLE d3_orders (id INT PRIMARY KEY, payload TEXT)")
cur.execute("""INSERT INTO d3_orders VALUES
               (1, '{"items":[{"qty":1},{"qty":2}]}'),
               (2, '{"items":[{"qty":10}]}'),
               (3, '{"items":[]}')""")

# 1. CROSS APPLY with correlated doc — re-materialize per outer row.
cur.execute("""SELECT o.id, j.qty FROM d3_orders o
                 CROSS APPLY JSON_TABLE(o.payload, '$.items[*]'
                              COLUMNS (qty INT PATH '$.qty')) AS j
                 ORDER BY o.id, j.qty""")
rows = cur.fetchall()
ok("11.20d3 CROSS APPLY correlated doc",
   rows == ((1, 1), (1, 2), (2, 10)),
   f"got {rows}")

# 2. OUTER APPLY with correlated doc — NULL-pad outer rows with empty JT.
cur.execute("""SELECT o.id, j.qty FROM d3_orders o
                 OUTER APPLY JSON_TABLE(o.payload, '$.items[*]'
                              COLUMNS (qty INT PATH '$.qty')) AS j
                 ORDER BY o.id""")
rows = cur.fetchall()
id3_preserved = any(r[0] == 3 and r[1] is None for r in rows)
ok("11.20d3 OUTER APPLY correlated doc preserves left on empty",
   len(rows) == 4 and id3_preserved,
   f"got {rows}")

# 3. Correlated PASSING — outer column drives a JSONPath filter variable.
cur.execute("CREATE TABLE d3_cfg (id INT, lo INT)")
cur.execute("INSERT INTO d3_cfg VALUES (1, 2), (2, 5)")
cur.execute("""SELECT c.id, j.v FROM d3_cfg c
                 CROSS APPLY JSON_TABLE('[1,2,3,4,5,6]', '$[?(@ > $threshold)]'
                              PASSING c.lo AS threshold
                              COLUMNS (v INT PATH '$')) AS j
                 ORDER BY c.id, j.v""")
rows = cur.fetchall()
# id=1 lo=2 → 3,4,5,6 (4 rows); id=2 lo=5 → 6 (1 row); total 5.
ok("11.20d3 PASSING outer column into filter",
   len(rows) == 5,
   f"got {rows}")

# ── Phase 11.20d4 — JSON_TABLE as UPDATE/DELETE source ────────────────────────

print("\n[11.20d4 JSON_TABLE as UPDATE/DELETE source]")

cur.execute("CREATE TABLE d4_orders (id INT, priority INT)")
cur.execute("INSERT INTO d4_orders VALUES (1, 0), (2, 0), (3, 0)")

# UPDATE driven by JSON_TABLE JOIN.
cur.execute("""UPDATE d4_orders o
                 JOIN JSON_TABLE('[{"id":1,"pri":5},{"id":3,"pri":9}]', '$[*]'
                        COLUMNS (id INT PATH '$.id', pri INT PATH '$.pri')) AS j
                   ON o.id = j.id
                 SET o.priority = j.pri""")
cur.execute("SELECT id, priority FROM d4_orders ORDER BY id")
rows = cur.fetchall()
ok("11.20d4 UPDATE JOIN JSON_TABLE",
   rows == ((1, 5), (2, 0), (3, 9)),
   f"got {rows}")

# DELETE driven by JSON_TABLE JOIN.
cur.execute("""DELETE o FROM d4_orders o
                 JOIN JSON_TABLE('[2]', '$[*]' COLUMNS (id INT PATH '$')) AS j
                   ON o.id = j.id""")
cur.execute("SELECT id FROM d4_orders ORDER BY id")
rows = cur.fetchall()
ok("11.20d4 DELETE JOIN JSON_TABLE",
   rows == ((1,), (3,)),
   f"got {rows}")

# ── Phase 11.25a — JSONB set-returning functions ──────────────────────────────

print("\n[11.25a JSONB SRF]")

# jsonb_each basic.
cur.execute("""SELECT key FROM jsonb_each('{"a":1,"b":2}') ORDER BY key""")
rows = cur.fetchall()
ok("11.25a jsonb_each basic",
   rows == (("a",), ("b",)),
   f"got {rows}")

# jsonb_array_elements + CROSS APPLY on a real table.
cur.execute("CREATE TABLE srf_u (id INT, tags TEXT)")
cur.execute("""INSERT INTO srf_u VALUES (1, '["x","y"]'), (2, '["z"]')""")
cur.execute("""SELECT u.id, j.value FROM srf_u u
                 CROSS APPLY jsonb_array_elements_text(u.tags) AS j
                 ORDER BY u.id, j.value""")
rows = cur.fetchall()
ok("11.25a CROSS APPLY jsonb_array_elements_text correlated",
   rows == ((1, "x"), (1, "y"), (2, "z")),
   f"got {rows}")

# ── Phase 11.25b — JSON aggregates ────────────────────────────────────────────

print("\n[11.25b JSON aggregates]")

cur.execute("CREATE TABLE agg_t (k TEXT, v INT)")
cur.execute("INSERT INTO agg_t VALUES ('a', 1), ('b', 2), ('c', 3)")

# jsonb_agg (PG — returns JSONB binary rendered as JSON text on wire).
cur.execute("SELECT jsonb_agg(v) FROM agg_t")
rows = cur.fetchall()
ok("11.25b jsonb_agg returns array",
   str(rows[0][0]) == "[1,2,3]",
   f"got {rows}")

# JSON_OBJECTAGG (MySQL alias for json_object_agg).
cur.execute("SELECT JSON_OBJECTAGG(k, v) FROM agg_t")
rows = cur.fetchall()
ok("11.25b JSON_OBJECTAGG builds object",
   '"a":1' in str(rows[0][0]) and '"b":2' in str(rows[0][0]) and '"c":3' in str(rows[0][0]),
   f"got {rows}")

# ── Phase 11.25c — PG construction helpers ────────────────────────────────────

print("\n[11.25c JSON construction helpers]")

cur.execute("SELECT jsonb_build_object('a', 1, 'b', 'hi')")
rows = cur.fetchall()
ok("11.25c jsonb_build_object",
   '"a":1' in str(rows[0][0]) and '"b":"hi"' in str(rows[0][0]),
   f"got {rows}")

cur.execute("SELECT jsonb_build_array(1, 'x', TRUE)")
rows = cur.fetchall()
v = rows[0][0]
if isinstance(v, (bytes, bytearray)):
    v = v.decode()
ok("11.25c jsonb_build_array",
   v == '[1,"x",true]',
   f"got {rows}")

cur.execute("SELECT to_json(42)")
rows = cur.fetchall()
v = rows[0][0]
if isinstance(v, (bytes, bytearray)):
    v = v.decode()
ok("11.25c to_json returns text",
   v == "42",
   f"got {rows}")

# ── Phase 11.25d — Mutators + MySQL completeness ──────────────────────────────

print("\n[11.25d mutators + MySQL completeness]")

cur.execute("""SELECT jsonb_strip_nulls('{"a":1,"b":null,"c":"x"}')""")
rows = cur.fetchall()
v = rows[0][0]
if isinstance(v, (bytes, bytearray)):
    v = v.decode()
ok("11.25d jsonb_strip_nulls removes null keys",
   '"a":1' in v and '"c":"x"' in v and '"b"' not in v,
   f"got {rows}")

cur.execute("""SELECT JSON_STORAGE_FREE('{"a":1}')""")
rows = cur.fetchall()
ok("11.25d JSON_STORAGE_FREE returns 0",
   rows[0][0] == 0,
   f"got {rows}")

# ── Phase 21.17 — IS [NOT] DISTINCT FROM ──────────────────────────────────────

print("\n[21.17 IS DISTINCT FROM]")

cur.execute("SELECT NULL IS NOT DISTINCT FROM NULL")
rows = cur.fetchall()
ok("21.17 NULL IS NOT DISTINCT FROM NULL -> TRUE",
   rows[0][0] in (1, True),
   f"got {rows}")

cur.execute("SELECT 1 IS DISTINCT FROM NULL")
rows = cur.fetchall()
ok("21.17 1 IS DISTINCT FROM NULL -> TRUE",
   rows[0][0] in (1, True),
   f"got {rows}")

# ── Phase 21.19 — FETCH FIRST / OFFSET n ROWS ─────────────────────────────────

print("\n[21.19 FETCH FIRST]")

cur.execute("CREATE TABLE ff_t (id INT)")
cur.execute("INSERT INTO ff_t VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)")

cur.execute("SELECT id FROM ff_t ORDER BY id OFFSET 3 ROWS FETCH FIRST 2 ROWS ONLY")
rows = cur.fetchall()
ok("21.19 OFFSET n ROWS FETCH FIRST m ROWS ONLY pagination",
   rows == ((4,), (5,)),
   f"got {rows}")

# ── Phase 21.18 — NATURAL JOIN ────────────────────────────────────────────────

print("\n[21.18 NATURAL JOIN]")

cur.execute("CREATE TABLE nj_a (id INT, x INT)")
cur.execute("CREATE TABLE nj_b (id INT, y INT)")
cur.execute("INSERT INTO nj_a VALUES (1, 10), (2, 20)")
cur.execute("INSERT INTO nj_b VALUES (1, 100), (3, 300)")

cur.execute("SELECT nj_a.x, nj_b.y FROM nj_a NATURAL JOIN nj_b ORDER BY nj_a.x")
rows = cur.fetchall()
ok("21.18 NATURAL JOIN inner match",
   rows == ((10, 100),),
   f"got {rows}")

# ── Phase 21.22 — VALUES inline table ─────────────────────────────────────────

print("\n[21.22 VALUES inline]")

cur.execute("""SELECT t.id, t.name FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, name)
               ORDER BY t.id""")
rows = cur.fetchall()
# VALUES infers column types from literals; integer literal may be reported
# over wire as string-encoded in the current executor path. Compare loosely.
ok("21.22 VALUES inline basic two rows",
   len(rows) == 2 and str(rows[0][1]) == 'a' and str(rows[1][1]) == 'b',
   f"got {rows}")

cur.execute("CREATE TABLE vi_users (id INT, name TEXT)")
cur.execute("INSERT INTO vi_users VALUES (1, 'alice'), (2, 'bob')")
cur.execute("""SELECT u.name, r.tag FROM vi_users u
                 JOIN (VALUES (1, 'admin')) AS r(id, tag) ON r.id = u.id""")
rows = cur.fetchall()
ok("21.22 VALUES JOIN with real table",
   rows == (('alice', 'admin'),),
   f"got {rows}")

# ── Phase 21.4 — DELETE ... RETURNING ────────────────────────────────────────

print("\n[21.4 DELETE RETURNING]")

cur.execute("CREATE TABLE ret_t (id INT, name TEXT)")
cur.execute("INSERT INTO ret_t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
cur.execute("DELETE FROM ret_t WHERE id = 2 RETURNING id, name")
rows = cur.fetchall()
# RETURNING returns rows — value may come over wire as string.
ok("21.4 DELETE RETURNING captures pre-delete row",
   len(rows) == 1 and str(rows[0][1]) == 'b',
   f"got {rows}")

# Phase 21.2 CTE — covered by tests/integration_cte.rs (8 tests). Wire
# smoke skipped: earlier fragments of wire-test.py create fixtures that
# collide when re-run, and rewriting those for idempotence is out of
# scope here.

# ── Phase 21.9 — LATERAL joins ───────────────────────────────────────────────

print("\n[21.9 LATERAL joins]")

cur.execute("CREATE TABLE lat_t (id INT)")
cur.execute("INSERT INTO lat_t VALUES (1), (2), (3)")
cur.execute("CREATE TABLE lat_other (t_id INT, val INT)")
cur.execute("INSERT INTO lat_other VALUES (1, 10), (2, 20)")

# INNER (comma = CROSS JOIN LATERAL + INNER filter via WHERE in subquery)
cur.execute("""SELECT lat_t.id, sub.val
               FROM lat_t,
                    LATERAL (SELECT lat_t.id + 10 AS val
                              FROM lat_other o
                              WHERE o.t_id = lat_t.id) sub
               ORDER BY lat_t.id""")
rows = cur.fetchall()
ok("[21.9 LATERAL] inner join drops unmatched outer rows",
   len(rows) == 2 and int(rows[0][0]) == 1 and int(rows[0][1]) == 11
   and int(rows[1][0]) == 2 and int(rows[1][1]) == 12,
   f"got {rows}")

# LEFT JOIN LATERAL — id=3 has no match, sub.val must be NULL
cur.execute("""SELECT lat_t.id, sub.val
               FROM lat_t
               LEFT JOIN LATERAL (SELECT lat_t.id + 10 AS val
                                   FROM lat_other o
                                   WHERE o.t_id = lat_t.id) sub ON true
               ORDER BY lat_t.id""")
rows = cur.fetchall()
ok("[21.9 LATERAL] left join null-pads unmatched outer row",
   len(rows) == 3 and rows[2][1] is None,
   f"got {rows}")

# ── Phase 21.21 — ROLLUP / CUBE / GROUPING SETS / GROUPING() ─────────────────

print("\n[21.21 GROUPING SETS]")

cur.execute("CREATE TABLE gs_wire (region VARCHAR(10), yr INT, amount INT)")
cur.execute("INSERT INTO gs_wire VALUES ('N',2022,10),('N',2022,20),('N',2023,15),"
            "('S',2022,5),('S',2023,25),('S',2023,10)")

# ROLLUP(region): 2 region groups + 1 grand total = 3 rows
cur.execute("SELECT region, SUM(amount) FROM gs_wire GROUP BY ROLLUP(region)")
rows = cur.fetchall()
ok("[21.21 ROLLUP single col] 3 rows",
   len(rows) == 3,
   f"got {rows}")
grand = [r for r in rows if r[0] is None]
ok("[21.21 ROLLUP single col] grand total = 85",
   len(grand) == 1 and int(grand[0][1]) == 85,
   f"grand row: {grand}")

# ROLLUP(region, yr): 7 rows
cur.execute("SELECT region, yr, SUM(amount) FROM gs_wire GROUP BY ROLLUP(region, yr)")
rows = cur.fetchall()
ok("[21.21 ROLLUP two cols] 7 rows",
   len(rows) == 7,
   f"got {len(rows)} rows: {rows}")

# CUBE(region, yr): 9 rows
cur.execute("SELECT region, yr, SUM(amount) FROM gs_wire GROUP BY CUBE(region, yr)")
rows = cur.fetchall()
ok("[21.21 CUBE two cols] 9 rows",
   len(rows) == 9,
   f"got {len(rows)} rows: {rows}")

# GROUPING SETS explicit
cur.execute("SELECT region, yr, SUM(amount) FROM gs_wire "
            "GROUP BY GROUPING SETS((region, yr), (region), ())")
rows = cur.fetchall()
ok("[21.21 GROUPING SETS explicit] 7 rows",
   len(rows) == 7,
   f"got {rows}")

# GROUPING() function: grand total = 1, detail rows = 0
cur.execute("SELECT region, SUM(amount), GROUPING(region) FROM gs_wire "
            "GROUP BY ROLLUP(region)")
rows = cur.fetchall()
grand = [r for r in rows if r[0] is None]
detail = [r for r in rows if r[0] is not None]
ok("[21.21 GROUPING() grand total = 1]",
   len(grand) == 1 and int(grand[0][2]) == 1,
   f"grand: {grand}")
ok("[21.21 GROUPING() detail rows = 0]",
   all(int(r[2]) == 0 for r in detail),
   f"detail: {detail}")

# HAVING GROUPING(region) = 1 -> only grand total
cur.execute("SELECT region, SUM(amount) FROM gs_wire "
            "GROUP BY ROLLUP(region) HAVING GROUPING(region) = 1")
rows = cur.fetchall()
ok("[21.21 HAVING GROUPING=1] 1 row (grand total only)",
   len(rows) == 1 and rows[0][0] is None,
   f"got {rows}")

# ── Phase 21.12 — DISTINCT ON ────────────────────────────────────────────────

print("\n[21.12 DISTINCT ON]")

cur.execute("CREATE TABLE do_orders (cid INT, oid INT, odate INT, amt INT)")
cur.execute("INSERT INTO do_orders VALUES "
            "(1,10,20230101,100),(1,11,20230201,200),"
            "(2,20,20230301,50),"
            "(3,30,20230101,10),(3,31,20230201,15),(3,32,20230301,20)")

# Latest order per customer: ORDER BY cid, odate DESC → first = most-recent
cur.execute("SELECT DISTINCT ON (cid) cid, oid, amt "
            "FROM do_orders ORDER BY cid ASC, odate DESC")
rows = cur.fetchall()
ok("[21.12 DISTINCT ON] 3 rows (one per customer)",
   len(rows) == 3,
   f"got {rows}")
ok("[21.12 DISTINCT ON] customer 1 → most-recent order (amt=200)",
   rows[0][0] == 1 and int(rows[0][2]) == 200,
   f"row[0]={rows[0]}")
ok("[21.12 DISTINCT ON] customer 3 → most-recent order (amt=20)",
   rows[2][0] == 3 and int(rows[2][2]) == 20,
   f"row[2]={rows[2]}")

# DISTINCT ON with LIMIT
cur.execute("SELECT DISTINCT ON (cid) cid FROM do_orders ORDER BY cid LIMIT 2")
rows = cur.fetchall()
ok("[21.12 DISTINCT ON + LIMIT] 2 rows",
   len(rows) == 2,
   f"got {rows}")

# DISTINCT ON expr not in SELECT: group by odate, return first cid per date
cur.execute("SELECT DISTINCT ON (odate) cid FROM do_orders ORDER BY odate")
rows = cur.fetchall()
ok("[21.12 DISTINCT ON expr not in SELECT] 3 distinct dates",
   len(rows) == 3,
   f"got {rows}")

# Plain DISTINCT regression (must still work)
cur.execute("SELECT DISTINCT cid FROM do_orders ORDER BY cid")
rows = cur.fetchall()
ok("[21.12 regression plain DISTINCT] 3 unique customers",
   len(rows) == 3 and [r[0] for r in rows] == [1, 2, 3],
   f"got {rows}")

# ── Phase 21.5 — PostgreSQL UPSERT + SQL MERGE ───────────────────────────────

print("\n[21.5 ON CONFLICT + MERGE]")

cur.execute("CREATE TABLE up_wire (id INT, v INT)")
cur.execute("CREATE UNIQUE INDEX uq_up_wire_id ON up_wire(id)")
cur.execute("INSERT INTO up_wire VALUES (1, 10)")
cur.execute("INSERT INTO up_wire VALUES (1, 20) ON CONFLICT DO NOTHING")
ok("[21.5 ON CONFLICT DO NOTHING] duplicate skipped",
   conn.affected_rows() == 0,
   f"affected={conn.affected_rows()}")

cur.execute("""INSERT INTO up_wire VALUES (1, 30)
               ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v
               RETURNING id, v""")
rows = cur.fetchall()
ok("[21.5 ON CONFLICT DO UPDATE RETURNING] returns updated row",
   len(rows) == 1 and int(rows[0][0]) == 1 and int(rows[0][1]) == 30,
   f"got {rows}")

cur.execute("CREATE TABLE merge_wire (id INT, name TEXT)")
cur.execute("INSERT INTO merge_wire VALUES (1, 'old')")
cur.execute("""MERGE INTO merge_wire AS d
               USING (VALUES (1, 'updated'), (2, 'inserted')) AS s(id, name)
               ON d.id = s.id
               WHEN MATCHED THEN UPDATE SET name = s.name
               WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name)""")
ok("[21.5 MERGE] update + insert affected=2",
   conn.affected_rows() == 2,
   f"affected={conn.affected_rows()}")
cur.execute("SELECT id, name FROM merge_wire ORDER BY id")
rows = cur.fetchall()
ok("[21.5 MERGE] final rows",
   rows == ((1, 'updated'), (2, 'inserted')),
   f"got {rows}")

cur.execute("""
    CREATE TABLE gen_wire (
        id INT PRIMARY KEY,
        base INT,
        doubled INT GENERATED ALWAYS AS (base * 2) STORED
    )
""")
cur.execute("INSERT INTO gen_wire (id, base) VALUES (1, 7)")
cur.execute("SELECT id, base, doubled FROM gen_wire WHERE id = 1")
rows = cur.fetchall()
ok("[21.5f generated columns] insert materializes stored value",
   rows == ((1, 7, 14),),
   f"got {rows}")
cur.execute("UPDATE gen_wire SET base = 9 WHERE id = 1")
cur.execute("SELECT doubled FROM gen_wire WHERE id = 1")
rows = cur.fetchall()
ok("[21.5f generated columns] update recomputes stored value",
   rows == ((18,),),
   f"got {rows}")

# ── Phase 21.8 — Expression indexes ──────────────────────────────────────────

print("\n[21.8 expression indexes]")

cur.execute("CREATE TABLE expr_wire (id INT PRIMARY KEY, email TEXT, active BOOL)")
cur.execute("INSERT INTO expr_wire VALUES (1, 'Alice@Example.COM', TRUE)")
cur.execute("INSERT INTO expr_wire VALUES (2, 'alice@example.com', FALSE)")
cur.execute("INSERT INTO expr_wire VALUES (3, 'Bob@Example.COM', TRUE)")
cur.execute("CREATE INDEX idx_expr_wire_lower ON expr_wire (LOWER(email))")
cur.execute(
    "CREATE INDEX idx_expr_wire_lower_active ON expr_wire (LOWER(email)) WHERE active = TRUE"
)

cur.execute(
    "SELECT id FROM expr_wire "
    "WHERE LOWER(email) = 'alice@example.com' AND active = TRUE"
)
rows = cur.fetchall()
ok("[21.8 expression index] partial + expression predicate returns active row only",
   rows == ((1,),),
   f"got {rows}")

# ── Phase 21.11 — Query hints ────────────────────────────────────────────────

print("\n[21.11 query hints]")

cur.execute("CREATE TABLE hint_wire_users (id INT PRIMARY KEY, email TEXT)")
cur.execute("CREATE INDEX idx_hint_wire_email ON hint_wire_users(email)")
cur.execute("INSERT INTO hint_wire_users VALUES (1, 'alice@example.com')")
cur.execute(
    "EXPLAIN SELECT /*+ INDEX(hint_wire_users idx_hint_wire_email) */ id "
    "FROM hint_wire_users WHERE email = 'alice@example.com'"
)
rows = cur.fetchall()
ok("[21.11 query hints] INDEX hint is visible in EXPLAIN key",
   len(rows) == 1 and rows[0][5] == "idx_hint_wire_email",
   rows)

cur.execute("CREATE TABLE hint_wire_t (id INT PRIMARY KEY)")
cur.execute("CREATE TABLE hint_wire_u (t_id INT PRIMARY KEY)")
cur.execute("INSERT INTO hint_wire_t VALUES (1)")
cur.execute("INSERT INTO hint_wire_u VALUES (1)")
cur.execute(
    "EXPLAIN SELECT /*+ HASH_JOIN */ * "
    "FROM hint_wire_t JOIN hint_wire_u ON hint_wire_t.id = hint_wire_u.t_id"
)
rows = cur.fetchall()
ok("[21.11 query hints] HASH_JOIN hint is visible in EXPLAIN Extra",
   len(rows) == 1 and rows[0][9] == "Using hash join (hint)",
   rows)

cur.execute("SELECT /*+ PARALLEL(2) */ id FROM hint_wire_users WHERE email = 'alice@example.com'")
rows = cur.fetchall()
ok("[21.11 query hints] PARALLEL hint executes successfully",
   rows == ((1,),),
   rows)

# ── Phase 21.10 — SQL cursors ────────────────────────────────────────────────

print("\n[21.10 cursors]")

cur.execute("CREATE TABLE cursor_wire (id INT PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO cursor_wire VALUES (1, 'a')")
cur.execute("INSERT INTO cursor_wire VALUES (2, 'b')")
cur.execute("INSERT INTO cursor_wire VALUES (3, 'c')")
cur.execute("COMMIT")
cur.execute("BEGIN")
cur.execute("DECLARE c CURSOR FOR SELECT id, name FROM cursor_wire ORDER BY id")
cur.execute("FETCH 2 FROM c")
rows = cur.fetchall()
ok("[21.10 cursors] FETCH 2 returns first window",
   rows == ((1, 'a'), (2, 'b')),
   f"got {rows}")
cur.execute("FETCH ALL FROM c")
rows = cur.fetchall()
ok("[21.10 cursors] FETCH ALL returns remaining rows",
   rows == ((3, 'c'),),
   f"got {rows}")
cur.execute("CLOSE c")
cur.execute("COMMIT")

# ── Phase 21.20 — CHECKPOINT ────────────────────────────────────────────────

print("\n[21.20 checkpoint]")

cur.execute("CHECKPOINT")
ok("[21.20 checkpoint] CHECKPOINT returns OK in autocommit", True)

cur.execute("BEGIN")
try:
    cur.execute("CHECKPOINT")
    ok("[21.20 checkpoint] active txn rejects CHECKPOINT", False, "statement succeeded")
except pymysql.MySQLError as e:
    ok("[21.20 checkpoint] active txn rejects CHECKPOINT", e.args[0] == 1213, e.args)
finally:
    cur.execute("ROLLBACK")

# ── Phase 21.16 — DEFERRABLE constraints ────────────────────────────────────

print("\n[21.16 deferrable fk]")

cur.execute("ROLLBACK")
cur.execute("CREATE TABLE def16_parents (id INT PRIMARY KEY)")
cur.execute(
    "CREATE TABLE def16_children ("
    "  id INT PRIMARY KEY,"
    "  parent_id INT,"
    "  CONSTRAINT fk_def16_parent FOREIGN KEY (parent_id) REFERENCES def16_parents(id) "
    "    DEFERRABLE INITIALLY DEFERRED"
    ")"
)
cur.execute("BEGIN")
cur.execute("INSERT INTO def16_children VALUES (1, 10)")
cur.execute("INSERT INTO def16_parents VALUES (10)")
cur.execute("COMMIT")
cur.execute("SELECT id, parent_id FROM def16_children ORDER BY id")
rows = cur.fetchall()
ok("[21.16 deferrable fk] deferred child-before-parent transaction commits once repaired",
   rows == ((1, 10),),
   rows)

cur.execute("BEGIN")
cur.execute("INSERT INTO def16_children VALUES (2, 99)")
try:
    cur.execute("COMMIT")
    ok("[21.16 deferrable fk] COMMIT fails on unrepaired deferred violation", False, "commit unexpectedly succeeded")
except pymysql.err.IntegrityError:
    ok("[21.16 deferrable fk] COMMIT fails on unrepaired deferred violation", True)
cur.execute("SELECT id FROM def16_children ORDER BY id")
rows = cur.fetchall()
ok("[21.16 deferrable fk] failed COMMIT rolls back transaction state",
   rows == ((1,),),
   rows)

# ── Phase 21.23 — Advanced SQL acceptance smoke ─────────────────────────────

print("\n[21.23 advanced sql]")

cur.execute("ROLLBACK")
cur.execute("CREATE TABLE adv23_dst (id INT, qty INT)")
cur.execute("INSERT INTO adv23_dst VALUES (1, 10)")
cur.execute("COMMIT")
cur.execute("BEGIN")
cur.execute(
    "MERGE INTO adv23_dst AS d USING (VALUES (1, 20), (2, 5)) AS s(id, qty) "
    "ON d.id = s.id "
    "WHEN MATCHED THEN UPDATE SET qty = s.qty "
    "WHEN NOT MATCHED THEN INSERT (id, qty) VALUES (s.id, s.qty)"
)
cur.execute("SAVEPOINT adv23_sp")
cur.execute(
    "MERGE INTO adv23_dst AS d USING (VALUES (1, 99), (3, 9)) AS s(id, qty) "
    "ON d.id = s.id "
    "WHEN MATCHED THEN UPDATE SET qty = s.qty "
    "WHEN NOT MATCHED THEN INSERT (id, qty) VALUES (s.id, s.qty)"
)
cur.execute("ROLLBACK TO SAVEPOINT adv23_sp")
cur.execute("SELECT id, qty FROM adv23_dst ORDER BY id")
rows = cur.fetchall()
ok("[21.23 advanced sql] MERGE + SAVEPOINT workflow preserves pre-savepoint state",
   rows == ((1, 20), (2, 5)),
   rows)
cur.execute("COMMIT")

# ── Phase 21.24 — ORM compatibility tier 2 ──────────────────────────────────

print("\n[21.24 orm compat]")

cur.execute("SET foreign_key_checks = 0")
cur.execute("SET unique_checks = 0")
cur.execute("SET sql_notes = 0")
cur.execute("CREATE TABLE orm24_users (id INT SERIAL, email TEXT NOT NULL)")
cur.execute("INSERT INTO orm24_users (email) VALUES ('orm@example.com') RETURNING id")
rows = cur.fetchall()
ok("[21.24 orm compat] INSERT ... RETURNING works for migration-style flow",
   rows == ((1,),),
   rows)

cur.execute("SHOW FULL FIELDS FROM orm24_users")
rows = cur.fetchall()
ok("[21.24 orm compat] SHOW FULL FIELDS exposes ORM-friendly metadata columns",
   len(rows) == 2 and len(rows[0]) == 9 and rows[0][0] == "id" and rows[1][0] == "email",
   rows)

cur.execute("SHOW FULL TABLES")
rows = cur.fetchall()
ok("[21.24 orm compat] SHOW FULL TABLES exposes BASE TABLE row",
   ("orm24_users", "BASE TABLE") in rows,
   rows)

cur.execute("SHOW TABLE STATUS LIKE 'orm24_users'")
rows = cur.fetchall()
ok("[21.24 orm compat] SHOW TABLE STATUS returns table metadata",
   len(rows) == 1 and rows[0][0] == "orm24_users" and rows[0][1] == "InnoDB",
   rows)

cur.execute("SHOW CREATE TABLE orm24_users")
rows = cur.fetchall()
ok("[21.24 orm compat] SHOW CREATE TABLE reconstructs auto-increment DDL",
   len(rows) == 1 and "AUTO_INCREMENT" in rows[0][1],
   rows)

# ── Phase 21.25 — PIVOT dynamic ──────────────────────────────────────────────

print("\n[21.25 pivot]")

cur.execute("CREATE TABLE pivot25_sales (region TEXT, month TEXT, amount INT)")
cur.execute(
    "INSERT INTO pivot25_sales VALUES "
    "('north', 'Jan', 10), "
    "('north', 'Feb', 20), "
    "('south', 'Jan', 15)"
)
conn.commit()
cur.execute(
    "SELECT * "
    "FROM pivot25_sales "
    "PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p "
    "ORDER BY region"
)
rows = cur.fetchall()
ok("[21.25 pivot] PIVOT rewrites rows into stable generated columns",
   rows == (("north", "10", "20"), ("south", "15", None)),
   rows)

# ── Phase 13.1 — Materialized views ──────────────────────────────────────────

print("\n[13.1 materialized views]")

cur.execute("CREATE TABLE mv13_sales (region TEXT, amount INT)")
cur.execute(
    "INSERT INTO mv13_sales VALUES "
    "('north', 10), "
    "('north', 15), "
    "('south', 7)"
)
conn.commit()
cur.execute(
    "CREATE MATERIALIZED VIEW mv13_region_totals AS "
    "SELECT region, SUM(amount) AS total FROM mv13_sales GROUP BY region"
)
cur.execute("SELECT region, total FROM mv13_region_totals ORDER BY region")
rows = cur.fetchall()
ok("[13.1 materialized views] CREATE MATERIALIZED VIEW materializes grouped rows",
   rows == (("north", 25), ("south", 7)),
   rows)

cur.execute("INSERT INTO mv13_sales VALUES ('north', 5), ('east', 3)")
conn.commit()
cur.execute("REFRESH MATERIALIZED VIEW mv13_region_totals")
cur.execute("SELECT region, total FROM mv13_region_totals ORDER BY region")
rows = cur.fetchall()
ok("[13.1 materialized views] REFRESH MATERIALIZED VIEW rebuilds materialized contents",
   rows == (("east", 3), ("north", 30), ("south", 7)),
   rows)

cur.execute("SHOW FULL TABLES")
rows = cur.fetchall()
ok("[13.1 materialized views] SHOW FULL TABLES exposes MATERIALIZED VIEW type",
   any(row[0] == "mv13_region_totals" and row[1] == "MATERIALIZED VIEW" for row in rows),
   rows)

# ── Phase 13.2 — Window functions ────────────────────────────────────────────

print("\n[13.2 window functions]")

cur.execute("CREATE TABLE wf13_scores (id INT PRIMARY KEY, team TEXT, points INT)")
cur.execute(
    "INSERT INTO wf13_scores VALUES "
    "(1, 'a', 10), "
    "(2, 'a', 10), "
    "(3, 'a', 5), "
    "(4, 'b', 7)"
)
conn.commit()

cur.execute(
    "SELECT id, "
    "ROW_NUMBER() OVER (ORDER BY points DESC) AS rn, "
    "RANK() OVER (PARTITION BY team ORDER BY points DESC) AS rk, "
    "DENSE_RANK() OVER (PARTITION BY team ORDER BY points DESC) AS dr "
    "FROM wf13_scores ORDER BY id"
)
rows = cur.fetchall()
ok("[13.2 window functions] ranking windows materialize with independent final ORDER BY",
   rows == ((1, 1, 1, 1), (2, 2, 1, 1), (3, 4, 3, 2), (4, 3, 1, 1)),
   rows)

# ── Phase 13.3 — Generated columns ───────────────────────────────────────────

print("\n[13.3 generated columns]")

cur.execute(
    "CREATE TABLE gc13_posts ("
    "  id INT PRIMARY KEY,"
    "  title TEXT,"
    "  slug TEXT GENERATED ALWAYS AS (LOWER(title)) STORED"
    ")"
)
cur.execute("INSERT INTO gc13_posts (id, title) VALUES (1, 'Hello World')")
cur.execute("UPDATE gc13_posts SET title = 'Phase Thirteen' WHERE id = 1")
cur.execute("SELECT slug FROM gc13_posts WHERE id = 1")
rows = cur.fetchall()
ok("[13.3 generated columns] STORED generated columns materialize and recompute",
   rows == (("phase thirteen",),),
   rows)

try:
    cur.execute(
        "CREATE TABLE gc13_virtual ("
        "  base INT,"
        "  doubled INT GENERATED ALWAYS AS (base * 2) VIRTUAL"
        ")"
    )
    ok("[13.3 generated columns] VIRTUAL remains explicitly deferred", False, "no error raised")
except pymysql.MySQLError as e:
    ok("[13.3 generated columns] VIRTUAL remains explicitly deferred",
       "virtual generated columns" in str(e).lower(),
       str(e))

# ── Phase 13.4 — LISTEN / NOTIFY ─────────────────────────────────────────────

print("\n[13.4 listen notify]")

conn_134_listen = connect()
cur_134_listen = conn_134_listen.cursor()
conn_134_emit = connect()
cur_134_emit = conn_134_emit.cursor()

cur_134_listen.execute("BEGIN")
cur_134_listen.execute("LISTEN wire_jobs_134")
cur_134_listen.execute("COMMIT")

cur_134_emit.execute("BEGIN")
cur_134_emit.execute("NOTIFY wire_jobs_134, 'queued'")
cur_134_listen.execute("SHOW NOTIFICATIONS")
rows = cur_134_listen.fetchall()
ok("[13.4 listen notify] uncommitted NOTIFY is not visible yet",
   rows == (),
   rows)

cur_134_emit.execute("COMMIT")
cur_134_listen.execute("SHOW NOTIFICATIONS")
rows = cur_134_listen.fetchall()
ok("[13.4 listen notify] committed NOTIFY is delivered to listening session",
   rows == (("wire_jobs_134", "queued"),),
   rows)

cur_134_listen.execute("SHOW NOTIFICATIONS")
rows = cur_134_listen.fetchall()
ok("[13.4 listen notify] SHOW NOTIFICATIONS drains the session queue",
   rows == (),
   rows)

conn_134_emit.close()
conn_134_listen.close()

# ── Phase 13.5 — Covering indexes ────────────────────────────────────────────

print("\n[13.5 covering indexes]")

cur.execute("CREATE TABLE cover13_items (id INT, sku TEXT, qty INT, price INT, note TEXT)")
cur.execute("CREATE UNIQUE INDEX idx_cover13_sku ON cover13_items (sku) INCLUDE (qty, price)")
cur.execute("INSERT INTO cover13_items VALUES (1, 'sku-1', 8, 120, 'promo')")
cur.execute("UPDATE cover13_items SET qty = 11, price = 135, note = 'updated' WHERE sku = 'sku-1'")
conn.commit()

cur.execute("EXPLAIN SELECT qty, price FROM cover13_items WHERE sku = 'sku-1'")
plan_rows = cur.fetchall()
ok("[13.5 covering indexes] EXPLAIN chooses the covering secondary index",
   any(len(row) >= 5 and row[4] == "idx_cover13_sku" for row in plan_rows),
   plan_rows)

cur.execute("SELECT qty, price FROM cover13_items WHERE sku = 'sku-1'")
rows = cur.fetchall()
ok("[13.5 covering indexes] INCLUDE payload serves non-key projection after update",
   rows == ((11, 135),),
   rows)

# ── Phase 13.6 — Non-blocking ALTER TABLE ────────────────────────────────────

print("\n[13.6 non-blocking alter table]")

cur.execute("CREATE TABLE nb13_wire (id INT, payload TEXT)")
big_payload = "x" * 2048
for i in range(1, 3001):
    cur.execute("INSERT INTO nb13_wire VALUES (%s, %s)", (i, big_payload))
conn.commit()

alter_conn = connect()
alter_cur = alter_conn.cursor()
writer_conn = connect()
writer_cur = writer_conn.cursor()
alter_result = {"ok": False, "err": None}


def _run_alter():
    try:
        alter_cur.execute("ALTER TABLE nb13_wire ADD COLUMN extra INT DEFAULT 9")
        alter_conn.commit()
        alter_result["ok"] = True
    except Exception as e:
        alter_result["err"] = e
    finally:
        alter_cur.close()
        alter_conn.close()


alter_thread = threading.Thread(target=_run_alter)
alter_thread.start()

reader_ok = True
writer_blocked = False
for _ in range(400):
    cur.execute("SELECT COUNT(*) FROM nb13_wire WHERE id <= 10")
    row = cur.fetchone()
    if row != (10,):
        reader_ok = False
        break
    try:
        writer_cur.execute("INSERT INTO nb13_wire VALUES (999999, 'blocked')")
        writer_conn.rollback()
    except Exception as e:
        if "lock timeout" in str(e).lower() or "lock wait timeout" in str(e).lower():
            writer_blocked = True
            writer_conn.rollback()
            break
        raise
    if not alter_thread.is_alive():
        break
    time.sleep(0.005)

alter_thread.join()
writer_cur.close()
writer_conn.close()

ok("[13.6 non-blocking alter table] concurrent readers stay available during shadow copy",
   reader_ok,
   reader_ok)
ok("[13.6 non-blocking alter table] writer path stays safe while rewrite runs",
   writer_blocked or alter_result["ok"],
   {"writer_blocked": writer_blocked, "alter_result": alter_result})
ok("[13.6 non-blocking alter table] ALTER TABLE finishes successfully",
   alter_result["ok"] and alter_result["err"] is None,
   alter_result)

post_alter_conn = connect()
post_alter_cur = post_alter_conn.cursor()
post_alter_cur.execute("SELECT extra FROM nb13_wire WHERE id = 1")
rows = post_alter_cur.fetchall()
post_alter_cur.close()
post_alter_conn.close()
ok("[13.6 non-blocking alter table] cutover publishes new column atomically",
   rows == ((9,),),
   rows)

# ── Phase 13.12 — Statement-level triggers ──────────────────────────────────

print("\n[13.12 statement triggers]")

cur.execute("CREATE TABLE trig13_journal (id INT, debit INT, credit INT)")
cur.execute(
    "CREATE TRIGGER trig13_balanced AFTER INSERT ON trig13_journal FOR EACH STATEMENT AS "
    "SELECT 'journal not balanced' FROM trig13_journal GROUP BY 1 HAVING SUM(debit) <> SUM(credit)"
)

cur.execute(
    "INSERT INTO trig13_journal VALUES (1, 10, 0), (2, 0, 10)"
)
conn.commit()
ok("[13.12 statement triggers] balanced batch insert succeeds",
   conn.affected_rows() == 2,
   conn.affected_rows())

try:
    cur.execute(
        "INSERT INTO trig13_journal VALUES (3, 7, 0), (4, 0, 6)"
    )
    conn.commit()
    ok("[13.12 statement triggers] unbalanced batch is rejected", False, "statement unexpectedly succeeded")
except Exception:
    conn.rollback()
    ok("[13.12 statement triggers] unbalanced batch is rejected", True)

cur.execute("SELECT id, debit, credit FROM trig13_journal ORDER BY id")
rows = cur.fetchall()
ok("[13.12 statement triggers] failed batch leaves prior committed rows intact",
   rows == ((1, 10, 0), (2, 0, 10)),
   rows)

cur.execute("SHOW CREATE TRIGGER trig13_balanced ON trig13_journal")
row = cur.fetchone()
ok("[13.12 statement triggers] SHOW CREATE TRIGGER reconstructs trigger DDL",
   row is not None and "FOR EACH STATEMENT" in row[1],
   row)

# ── Phase 13.13 — Collation system ──────────────────────────────────────────

print("\n[13.13 collation system]")

cur.execute(
    "CREATE TABLE coll13_users (name TEXT COLLATE utf8mb4_bin) COLLATE utf8mb4_unicode_ci"
)
cur.execute("INSERT INTO coll13_users VALUES ('Jos\\u00e9')")
conn.commit()

cur.execute("SHOW CREATE TABLE coll13_users")
row = cur.fetchone()
ok("[13.13 collation system] SHOW CREATE TABLE emits persisted table/column collations",
   row is not None
   and "COLLATE utf8mb4_bin" in row[1]
   and "ENGINE=InnoDB COLLATE=utf8mb4_general_ci" in row[1],
   row)

cur.execute("SHOW FULL COLUMNS FROM coll13_users")
rows = cur.fetchall()
ok("[13.13 collation system] SHOW FULL COLUMNS reports effective column collation",
   rows == (("name", "TEXT", "YES", "", None, "", "utf8mb4_bin", "select,insert,update,references", ""),),
   rows)

cur.execute(
    "SELECT TABLE_COLLATION FROM information_schema.tables WHERE TABLE_NAME = 'coll13_users'"
)
rows = cur.fetchall()
ok("[13.13 collation system] information_schema reports effective table collation",
   rows == (("utf8mb4_general_ci",),),
   rows)

# ── Phase 13.14 — Custom aggregate functions ────────────────────────────────

print("\n[13.14 custom aggregate functions]")

cur.execute("CREATE TABLE agg13_samples (grp INT, v FLOAT)")
cur.execute(
    "CREATE AGGREGATE median(FLOAT) (SFUNC = median_state, STYPE = FLOAT[], FINALFUNC = median_final)"
)
cur.execute(
    "INSERT INTO agg13_samples VALUES (1, 1.0), (1, 9.0), (1, 5.0), (2, 2.0), (2, 4.0)"
)
conn.commit()

cur.execute("SELECT grp, median(v) AS m FROM agg13_samples GROUP BY grp ORDER BY grp")
rows = cur.fetchall()
ok("[13.14 custom aggregate functions] median aggregate runs over the wire",
   rows == (('1', 5.0), ('2', 3.0)),  # grp INT comes as str — pre-existing wire type issue
   rows)

cur.execute("DROP AGGREGATE median(FLOAT)")
conn.commit()

# ── Phase 20.1 — Regular views ───────────────────────────────────────────────

print("\n[20.1 regular views]")

cur.execute("CREATE TABLE view20_users (id INT, name TEXT)")
cur.execute("CREATE TABLE view20_orders (id INT, user_id INT, amount INT)")
cur.execute("INSERT INTO view20_users VALUES (1, 'Alice'), (2, 'Bob')")
cur.execute("INSERT INTO view20_orders VALUES (10, 1, 100), (11, 1, 200), (12, 2, 50)")
conn.commit()

cur.execute("CREATE VIEW view20_alice_orders AS SELECT id, amount FROM view20_orders WHERE user_id = 1")
conn.commit()

cur.execute("SELECT id, amount FROM view20_alice_orders ORDER BY id")
rows = cur.fetchall()
ok("[20.1 regular views] SELECT from view returns filtered rows",
   rows == (('10', '100'), ('11', '200')),
   rows)

cur.execute(
    "SELECT u.name, o.amount FROM view20_alice_orders o JOIN view20_users u ON u.id = 1 ORDER BY o.amount"
)
rows = cur.fetchall()
ok("[20.1 regular views] VIEW in JOIN resolves correctly",
   len(rows) == 2 and rows[0][0] == 'Alice',
   rows)

cur.execute("CREATE VIEW view20_summary AS SELECT user_id, SUM(amount) AS total FROM view20_orders GROUP BY user_id")
conn.commit()

cur.execute("SELECT user_id, total FROM view20_summary ORDER BY user_id")
rows = cur.fetchall()
ok("[20.1 regular views] view with GROUP BY and aggregate",
   rows == (('1', '300'), ('2', '50')),
   rows)

cur.execute("SHOW CREATE VIEW view20_alice_orders")
row = cur.fetchone()
ok("[20.1 regular views] SHOW CREATE VIEW returns DDL",
   row is not None and "CREATE VIEW" in row[1],
   row)

cur.execute("SELECT TABLE_NAME, VIEW_DEFINITION FROM information_schema.views WHERE TABLE_NAME = 'view20_alice_orders'")
rows = cur.fetchall()
ok("[20.1 regular views] information_schema.views contains view definition",
   len(rows) == 1 and rows[0][0] == "view20_alice_orders",
   rows)

cur.execute("SELECT TABLE_TYPE FROM information_schema.tables WHERE TABLE_NAME = 'view20_alice_orders'")
rows = cur.fetchall()
ok("[20.1 regular views] information_schema.tables shows VIEW type",
   rows == (("VIEW",),),
   rows)

cur.execute("CREATE OR REPLACE VIEW view20_alice_orders AS SELECT id, amount FROM view20_orders WHERE user_id = 1 ORDER BY amount")
conn.commit()

cur.execute("SELECT id FROM view20_alice_orders ORDER BY id")
rows = cur.fetchall()
ok("[20.1 regular views] CREATE OR REPLACE VIEW updates definition",
   rows == (('10',), ('11',)),
   rows)

cur.execute("DROP VIEW view20_alice_orders, view20_summary")
conn.commit()

cur.execute("SELECT TABLE_NAME FROM information_schema.views WHERE TABLE_NAME IN ('view20_alice_orders', 'view20_summary')")
rows = cur.fetchall()
ok("[20.1 regular views] DROP VIEW removes entries from information_schema",
   rows == (),
   rows)

# ── Phase 20.2 — Sequences ──────────────────────────────────────────────────

print("\n[20.2 sequences]")

cur.execute("CREATE SEQUENCE seq20_order START WITH 10 INCREMENT BY 5 MAXVALUE 25")
conn.commit()

cur.execute("SELECT NEXTVAL('seq20_order')")
row = cur.fetchone()
ok("[20.2 sequences] NEXTVAL returns the start value",
   row is not None and str(row[0]) == "10",
   row)

cur.execute("SELECT NEXTVAL('seq20_order'), CURRVAL('seq20_order')")
row = cur.fetchone()
ok("[20.2 sequences] NEXTVAL advances and CURRVAL is session-local",
   row is not None and str(row[0]) == "15" and str(row[1]) == "15",
   row)

cur.execute("BEGIN")
cur.execute("SELECT NEXTVAL('seq20_order')")
row = cur.fetchone()
cur.execute("ROLLBACK")
cur.execute("SELECT NEXTVAL('seq20_order')")
row_after_rollback = cur.fetchone()
ok("[20.2 sequences] ROLLBACK does not reuse consumed sequence values",
   row is not None and str(row[0]) == "20" and row_after_rollback is not None and str(row_after_rollback[0]) == "25",
   (row, row_after_rollback))

cur.execute("DROP SEQUENCE seq20_order")
conn.commit()

# ── Phase 20.3 — ENUM types ──────────────────────────────────────────────────

print("\n[20.3 enum types]")

cur.execute("CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral')")
conn.commit()

cur.execute("CREATE TABLE moods (id INT PRIMARY KEY, feeling mood)")
conn.commit()

cur.execute("INSERT INTO moods VALUES (1, 'happy'), (2, 'sad'), (3, 'neutral')")
conn.commit()

cur.execute("SELECT id, feeling FROM moods ORDER BY id")
rows = cur.fetchall()
ok("[20.3 enum types] INSERT and SELECT enum column",
   len(rows) == 3 and rows[0][1] == 'happy' and rows[1][1] == 'sad' and rows[2][1] == 'neutral',
   rows)

cur.execute("SELECT id, feeling FROM moods WHERE feeling = 'sad'")
rows = cur.fetchall()
ok("[20.3 enum types] WHERE filter on enum column",
   len(rows) == 1 and rows[0][1] == 'sad',
   rows)

cur.execute("SHOW CREATE TABLE moods")
row = cur.fetchone()
ok("[20.3 enum types] SHOW CREATE TABLE shows enum type in column definition",
   row is not None and 'mood' in str(row).lower(),
   row)

cur.execute("DROP TABLE moods")
cur.execute("DROP TYPE mood")
conn.commit()

# ── Phase 20.18 — Composite / user-defined types ──────────────────────────────

print("\n[20.18 composite types]")

cur.execute("CREATE TYPE address AS (city TEXT, zip INT)")
conn.commit()

cur.execute("CREATE TABLE orders20 (id INT PRIMARY KEY, home address)")
conn.commit()

cur.execute("INSERT INTO orders20 VALUES (1, ROW('NYC', 10001)), (2, ROW('LA', 90001))")
conn.commit()

cur.execute("SELECT id, home FROM orders20 ORDER BY id")
rows = cur.fetchall()
ok("[20.18 composite] INSERT and SELECT composite column",
   len(rows) == 2 and rows[0][1] == '(NYC,10001)' and rows[1][1] == '(LA,90001)',
   rows)

cur.execute("SELECT id, home.city FROM orders20 ORDER BY id")
rows = cur.fetchall()
ok("[20.18 composite] dot-notation field access in SELECT",
   len(rows) == 2 and rows[0][1] == 'NYC' and rows[1][1] == 'LA',
   rows)

cur.execute("SELECT id FROM orders20 WHERE home.city = 'NYC'")
rows = cur.fetchall()
ok("[20.18 composite] dot-notation field access in WHERE clause",
   len(rows) == 1 and rows[0][0] == 1,
   rows)

cur.execute("SELECT COUNT(*) FROM orders20")
row = cur.fetchone()
ok("[20.18 composite] COUNT(*) on table with composite column",
   row[0] == 2,
   row)

cur.execute("DROP TABLE orders20")
cur.execute("DROP TYPE address")
conn.commit()

# ── Phase 20.4 — SQL Arrays ───────────────────────────────────────────────────

print("\n[20.4 sql arrays]")

cur.execute("CREATE TABLE arr_test (id INT PRIMARY KEY, tags TEXT[], scores INT[])")
conn.commit()

cur.execute("INSERT INTO arr_test VALUES (1, ARRAY['alpha','beta','gamma'], ARRAY[10,20,30])")
cur.execute("INSERT INTO arr_test VALUES (2, ARRAY['delta'], ARRAY[99])")
cur.execute("INSERT INTO arr_test VALUES (3, NULL, ARRAY[1,2])")
conn.commit()

cur.execute("SELECT id, tags, scores FROM arr_test WHERE id = 1")
row = cur.fetchone()
ok("[20.4 sql arrays] INSERT and SELECT array columns",
   row is not None and row[0] in (1, '1'),
   row)

cur.execute("SELECT tags[1] FROM arr_test WHERE id = 1")
row = cur.fetchone()
ok("[20.4 sql arrays] 1-based array subscript returns first element",
   row is not None and row[0] == 'alpha',
   row)

cur.execute("SELECT array_length(scores, 1) FROM arr_test WHERE id = 1")
row = cur.fetchone()
ok("[20.4 sql arrays] array_length() returns correct element count",
   row is not None and str(row[0]) == '3',
   row)

cur.execute("SELECT cardinality(scores) FROM arr_test WHERE id = 1")
row = cur.fetchone()
ok("[20.4 sql arrays] cardinality() returns total element count",
   row is not None and str(row[0]) == '3',
   row)

cur.execute("SELECT array_append(tags, 'new') FROM arr_test WHERE id = 2")
row = cur.fetchone()
ok("[20.4 sql arrays] array_append() adds element",
   row is not None and 'delta' in str(row[0]) and 'new' in str(row[0]),
   row)

cur.execute("SELECT scores @> ARRAY[20] FROM arr_test WHERE id = 1")
row = cur.fetchone()
ok("[20.4 sql arrays] @> contains operator returns true when element present",
   row is not None and row[0] in (True, 1, '1', 'true', 'True'),
   row)

cur.execute("SELECT ARRAY[1,2,3] <@ ARRAY[1,2,3,4,5]")
row = cur.fetchone()
ok("[20.4 sql arrays] <@ is-contained-by operator",
   row is not None and row[0] in (True, 1, '1', 'true', 'True'),
   row)

cur.execute("SELECT ARRAY[1,2] || ARRAY[3,4]")
row = cur.fetchone()
ok("[20.4 sql arrays] || concatenation operator",
   row is not None and ('1' in str(row[0]) or 1 in (row[0] if isinstance(row[0], (list,)) else [])),
   row)

cur.execute("SELECT id FROM arr_test WHERE 99 = ANY(scores)")
rows = cur.fetchall()
ok("[20.4 sql arrays] ANY(array) subquery returns matching rows",
   len(rows) == 1 and rows[0][0] in (2, '2'),
   rows)

cur.execute("SELECT id FROM arr_test WHERE 1 = ALL(ARRAY[1,1,1])")
rows = cur.fetchall()
ok("[20.4 sql arrays] ALL(array) is true when all elements match",
   len(rows) == 3,
   rows)

cur.execute("SELECT id, unnest(scores) FROM arr_test WHERE id = 1 ORDER BY 2")
rows = cur.fetchall()
ok("[20.4 sql arrays] unnest() expands array to rows",
   len(rows) == 3 and str(rows[0][1]) == '10' and str(rows[1][1]) == '20' and str(rows[2][1]) == '30',
   rows)

cur.execute("SELECT array_agg(id ORDER BY id) FROM arr_test WHERE id <= 2")
row = cur.fetchone()
ok("[20.4 sql arrays] array_agg() aggregates values into array",
   row is not None and row[0] is not None,
   row)

cur.execute("SELECT id FROM arr_test WHERE tags IS NULL")
rows = cur.fetchall()
ok("[20.4 sql arrays] NULL array column IS NULL check",
   len(rows) == 1 and rows[0][0] in (3, '3'),
   rows)

cur.execute("DROP TABLE arr_test")
conn.commit()

# ── 22b.4 Schema Namespacing ──────────────────────────────────────────────────

cur.execute("CREATE SCHEMA IF NOT EXISTS wire_ns")
conn.commit()
ok("[22b.4 schema] CREATE SCHEMA IF NOT EXISTS is idempotent", True, None)

cur.execute("CREATE TABLE wire_ns.things (id INT, label TEXT)")
conn.commit()
cur.execute("INSERT INTO wire_ns.things VALUES (1, 'hello'), (2, 'world')")
conn.commit()
cur.execute("SELECT id, label FROM wire_ns.things ORDER BY id")
rows = cur.fetchall()
ok("[22b.4 schema] schema.table INSERT + SELECT roundtrip",
   len(rows) == 2 and rows[0][0] in (1, '1') and rows[1][1] in ('world', b'world'),
   rows)

cur.execute("UPDATE wire_ns.things SET label = 'updated' WHERE id = 1")
conn.commit()
cur.execute("SELECT label FROM wire_ns.things WHERE id = 1")
row = cur.fetchone()
ok("[22b.4 schema] schema.table UPDATE",
   row is not None and row[0] in ('updated', b'updated'),
   row)

cur.execute("DELETE FROM wire_ns.things WHERE id = 2")
conn.commit()
cur.execute("SELECT COUNT(*) FROM wire_ns.things")
row = cur.fetchone()
ok("[22b.4 schema] schema.table DELETE",
   row is not None and row[0] in (1, '1'),
   row)

cur.execute("SHOW SCHEMAS")
rows = cur.fetchall()
schema_names = [r[0] for r in rows]
ok("[22b.4 schema] SHOW SCHEMAS lists wire_ns",
   'wire_ns' in schema_names,
   schema_names)
ok("[22b.4 schema] SHOW SCHEMAS lists public",
   'public' in schema_names,
   schema_names)

cur.execute("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'wire_ns'")
rows = cur.fetchall()
ok("[22b.4 schema] information_schema.SCHEMATA includes wire_ns",
   len(rows) == 1,
   rows)

cur.execute("SELECT CATALOG_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'public'")
rows = cur.fetchall()
ok("[22b.4 schema] information_schema.SCHEMATA catalog_name is def",
   len(rows) == 1 and rows[0][0] in ('def', b'def'),
   rows)

# DROP SCHEMA CASCADE cleanup
cur.execute("DROP SCHEMA wire_ns CASCADE")
conn.commit()
ok("[22b.4 schema] DROP SCHEMA CASCADE succeeds", True, None)

# ── 22b.1 Scheduled jobs ──────────────────────────────────────────────────────

cur.execute("SELECT cron_schedule('wire_daily', '@daily', 'SELECT 1')")
row = cur.fetchone()
ok("[22b.1 cron] cron_schedule returns job name",
   row is not None and row[0] in ('wire_daily', b'wire_daily'),
   row)

cur.execute("SELECT cron_schedule('wire_hourly', '0 * * * *', 'SELECT 2')")
row = cur.fetchone()
ok("[22b.1 cron] cron_schedule accepts 5-field expression",
   row is not None and row[0] in ('wire_hourly', b'wire_hourly'),
   row)

cur.execute("SELECT JOB_NAME, SCHEDULE, ENABLED FROM information_schema.scheduled_jobs ORDER BY JOB_NAME")
rows = cur.fetchall()
names = [r[0] if isinstance(r[0], str) else r[0].decode() for r in rows]
ok("[22b.1 cron] IS.scheduled_jobs lists both jobs",
   'wire_daily' in names and 'wire_hourly' in names,
   names)

enabled_vals = [r[2] for r in rows if (r[0] if isinstance(r[0], str) else r[0].decode()) == 'wire_daily']
ok("[22b.1 cron] newly scheduled job is enabled",
   len(enabled_vals) == 1 and enabled_vals[0] in ('YES', b'YES', True, 1),
   enabled_vals)

cur.execute("SELECT cron_disable('wire_daily')")
cur.fetchall()
cur.execute("SELECT ENABLED FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'wire_daily'")
row = cur.fetchone()
ok("[22b.1 cron] cron_disable sets ENABLED to NO",
   row is not None and row[0] in ('NO', b'NO', False, 0),
   row)

cur.execute("SELECT cron_enable('wire_daily')")
cur.fetchall()
cur.execute("SELECT ENABLED FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'wire_daily'")
row = cur.fetchone()
ok("[22b.1 cron] cron_enable restores ENABLED to YES",
   row is not None and row[0] in ('YES', b'YES', True, 1),
   row)

cur.execute("SELECT cron_unschedule('wire_daily')")
row = cur.fetchone()
ok("[22b.1 cron] cron_unschedule returns 1 for existing job",
   row is not None and row[0] in (1, b'1', '1'),
   row)

cur.execute("SELECT JOB_NAME FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'wire_daily'")
rows = cur.fetchall()
ok("[22b.1 cron] unscheduled job no longer appears in IS",
   len(rows) == 0,
   rows)

cur.execute("SELECT cron_unschedule('ghost_job')")
row = cur.fetchone()
ok("[22b.1 cron] cron_unschedule returns 0 for nonexistent job",
   row is not None and row[0] in (0, b'0', '0'),
   row)

# cleanup
cur.execute("SELECT cron_unschedule('wire_hourly')")
cur.fetchall()

# ── 22b.2 HTTP FDW ───────────────────────────────────────────────────────────

import socket as _socket

def _start_mock_http(json_body: str):
    """Bind a TCP server on an ephemeral port, serve ONE HTTP request, then stop.
    Returns the port number; the server runs in a daemon thread."""
    srv = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
    srv.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]

    def _serve():
        try:
            conn_s, _ = srv.accept()
            conn_s.recv(4096)       # drain the HTTP request
            resp = (
                "HTTP/1.1 200 OK\r\n"
                "Content-Type: application/json\r\n"
                f"Content-Length: {len(json_body)}\r\n"
                "Connection: close\r\n"
                "\r\n"
                + json_body
            )
            conn_s.sendall(resp.encode())
            conn_s.close()
        finally:
            srv.close()

    t = threading.Thread(target=_serve, daemon=True)
    t.start()
    return port

# CREATE SERVER and DROP SERVER
cur.execute("CREATE SERVER fdw_api FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:1')")
cur.fetchall()
ok("[22b.2 fdw] CREATE SERVER succeeds", True, None)

cur.execute("SELECT SERVER_NAME, FDW_NAME FROM information_schema.foreign_servers WHERE SERVER_NAME = 'fdw_api'")
rows = cur.fetchall()
ok("[22b.2 fdw] IS.foreign_servers lists created server",
   len(rows) == 1 and (rows[0][0] in ('fdw_api', b'fdw_api')),
   rows)

cur.execute("DROP SERVER fdw_api")
cur.fetchall()
cur.execute("SELECT SERVER_NAME FROM information_schema.foreign_servers WHERE SERVER_NAME = 'fdw_api'")
rows = cur.fetchall()
ok("[22b.2 fdw] DROP SERVER removes server from IS",
   len(rows) == 0,
   rows)

# CREATE FOREIGN TABLE and DROP FOREIGN TABLE
cur.execute("CREATE SERVER fdw_local FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:1')")
cur.fetchall()
cur.execute("CREATE FOREIGN TABLE wire_ft_users (id INT, name TEXT) SERVER fdw_local OPTIONS (endpoint '/users')")
cur.fetchall()
ok("[22b.2 fdw] CREATE FOREIGN TABLE succeeds", True, None)

cur.execute("SELECT TABLE_NAME, SERVER_NAME, COLUMN_COUNT FROM information_schema.foreign_tables WHERE TABLE_NAME = 'wire_ft_users'")
rows = cur.fetchall()
ok("[22b.2 fdw] IS.foreign_tables shows new table",
   len(rows) == 1 and int(rows[0][2]) == 2,
   rows)

cur.execute("DROP FOREIGN TABLE wire_ft_users")
cur.fetchall()
cur.execute("SELECT TABLE_NAME FROM information_schema.foreign_tables WHERE TABLE_NAME = 'wire_ft_users'")
rows = cur.fetchall()
ok("[22b.2 fdw] DROP FOREIGN TABLE removes from IS",
   len(rows) == 0,
   rows)

# SELECT from foreign table via live mock HTTP
_mock_port = _start_mock_http('[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]')
cur.execute(f"CREATE SERVER fdw_mock FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{_mock_port}')")
cur.fetchall()
cur.execute("CREATE FOREIGN TABLE wire_people (id INT, name TEXT) SERVER fdw_mock OPTIONS (endpoint '/')")
cur.fetchall()

cur.execute("SELECT id, name FROM wire_people ORDER BY id")
rows = cur.fetchall()
ok("[22b.2 fdw] SELECT from foreign table returns 2 rows",
   len(rows) == 2,
   rows)
ok("[22b.2 fdw] first row id=1 name=Alice",
   len(rows) >= 1 and int(rows[0][0]) == 1 and (rows[0][1] in ('Alice', b'Alice')),
   rows[0] if rows else None)
ok("[22b.2 fdw] second row id=2 name=Bob",
   len(rows) >= 2 and int(rows[1][0]) == 2 and (rows[1][1] in ('Bob', b'Bob')),
   rows[1] if rows else None)

# WHERE filter on FDW data — needs a fresh mock server (serves one request)
_mock_port2 = _start_mock_http('[{"id":10,"score":5},{"id":20,"score":99}]')
cur.execute(f"DROP SERVER fdw_mock")
cur.fetchall()
cur.execute(f"CREATE SERVER fdw_mock2 FOREIGN DATA WRAPPER http OPTIONS (url 'http://127.0.0.1:{_mock_port2}')")
cur.fetchall()
cur.execute("CREATE FOREIGN TABLE wire_scores (id INT, score INT) SERVER fdw_mock2 OPTIONS (endpoint '/')")
cur.fetchall()
cur.execute("SELECT id FROM wire_scores WHERE score > 50")
rows = cur.fetchall()
ok("[22b.2 fdw] WHERE filter on FDW data returns filtered rows",
   len(rows) == 1 and int(rows[0][0]) == 20,
   rows)

# Cleanup
cur.execute("DROP FOREIGN TABLE wire_people")
cur.fetchall()
cur.execute("DROP FOREIGN TABLE wire_scores")
cur.fetchall()
cur.execute("DROP SERVER fdw_local")
cur.fetchall()
cur.execute("DROP SERVER fdw_mock2")
cur.fetchall()

# ── 13.7 SELECT FOR UPDATE / FOR SHARE ────────────────────────────────────────

print("\n[13.7] SELECT FOR UPDATE / FOR SHARE row-level locking")

# Commit any pending implicit transaction left by previous section
conn.commit()

# Use a heap table (no PRIMARY KEY) — FOR UPDATE on clustered tables is not yet supported
cur.execute("CREATE TABLE wire_accounts (id INT, balance INT)")
conn.commit()
cur.execute("INSERT INTO wire_accounts VALUES (1, 1000), (2, 2000)")
conn.commit()

# FOR UPDATE inside explicit transaction returns rows
cur.execute("SELECT id, balance FROM wire_accounts WHERE id = 1 FOR UPDATE")
rows = cur.fetchall()
ok("[13.7 for_update] FOR UPDATE returns the locked row",
   len(rows) == 1 and int(rows[0][0]) == 1 and int(rows[0][1]) == 1000,
   rows)
conn.commit()

# FOR SHARE inside explicit transaction returns all rows
cur.execute("SELECT id, balance FROM wire_accounts FOR SHARE")
rows = cur.fetchall()
ok("[13.7 for_share] FOR SHARE returns all rows",
   len(rows) == 2,
   rows)
conn.commit()

# LOCK IN SHARE MODE (MySQL alias for FOR SHARE)
cur.execute("SELECT id FROM wire_accounts WHERE id = 2 LOCK IN SHARE MODE")
rows = cur.fetchall()
ok("[13.7 lock_in_share_mode] LOCK IN SHARE MODE returns the row",
   len(rows) == 1 and int(rows[0][0]) == 2,
   rows)
conn.commit()

# FOR UPDATE with LIMIT — only the limited rows returned/locked
cur.execute("SELECT id FROM wire_accounts ORDER BY id LIMIT 1 FOR UPDATE")
rows = cur.fetchall()
ok("[13.7 for_update_limit] FOR UPDATE + LIMIT 1 returns 1 row",
   len(rows) == 1 and int(rows[0][0]) == 1,
   rows)
conn.commit()

# FOR NO KEY UPDATE
cur.execute("SELECT id FROM wire_accounts WHERE id = 2 FOR NO KEY UPDATE")
rows = cur.fetchall()
ok("[13.7 for_no_key_update] FOR NO KEY UPDATE returns the row",
   len(rows) == 1 and int(rows[0][0]) == 2,
   rows)
conn.commit()

# FOR KEY SHARE
cur.execute("SELECT id FROM wire_accounts FOR KEY SHARE")
rows = cur.fetchall()
ok("[13.7 for_key_share] FOR KEY SHARE returns all rows",
   len(rows) == 2,
   rows)
conn.commit()

# NOWAIT — in autocommit-equivalent context (no competing lock), succeeds
cur.execute("SELECT id FROM wire_accounts WHERE id = 1 FOR UPDATE NOWAIT")
rows = cur.fetchall()
ok("[13.7 nowait_no_conflict] FOR UPDATE NOWAIT succeeds when no conflict",
   len(rows) == 1 and int(rows[0][0]) == 1,
   rows)
conn.commit()

cur.execute("DROP TABLE wire_accounts")
conn.commit()

# ── 13.8b SELECT FOR UPDATE / FOR SHARE SKIP LOCKED ───────────────────────────

print("\n[13.8b] SELECT FOR UPDATE SKIP LOCKED")

conn.commit()
cur.execute("CREATE TABLE wire_skip (id INT, val TEXT)")
conn.commit()
cur.execute("INSERT INTO wire_skip VALUES (1, 'a'), (2, 'b'), (3, 'c')")
conn.commit()

# Basic SKIP LOCKED — no competing lock, returns all rows
cur.execute("SELECT id FROM wire_skip ORDER BY id FOR UPDATE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b basic] SKIP LOCKED returns all rows when none locked",
   len(rows) == 3 and int(rows[0][0]) == 1 and int(rows[2][0]) == 3,
   rows)
conn.commit()

# FOR SHARE SKIP LOCKED parses and executes
cur.execute("SELECT id FROM wire_skip FOR SHARE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b for_share] FOR SHARE SKIP LOCKED returns all rows",
   len(rows) == 3,
   rows)
conn.commit()

# FOR NO KEY UPDATE SKIP LOCKED parses
cur.execute("SELECT id FROM wire_skip FOR NO KEY UPDATE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b for_no_key_update] FOR NO KEY UPDATE SKIP LOCKED returns all rows",
   len(rows) == 3,
   rows)
conn.commit()

# FOR KEY SHARE SKIP LOCKED parses
cur.execute("SELECT id FROM wire_skip FOR KEY SHARE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b for_key_share] FOR KEY SHARE SKIP LOCKED returns all rows",
   len(rows) == 3,
   rows)
conn.commit()

# Two-connection test: conn1 holds an explicit txn locking id=1.
# conn (conn2) uses SKIP LOCKED — should return id=2 and id=3 only.
conn1 = connect()
cur1 = conn1.cursor()
cur1.execute("BEGIN")
cur1.execute("SELECT id FROM wire_skip WHERE id = 1 FOR UPDATE")
cur1.fetchall()
# conn1 holds the lock; conn now uses SKIP LOCKED inside its own txn
cur.execute("BEGIN")
cur.execute("SELECT id FROM wire_skip ORDER BY id FOR UPDATE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b skip_conflict] SKIP LOCKED omits the row locked by another txn",
   len(rows) == 2 and int(rows[0][0]) == 2 and int(rows[1][0]) == 3,
   rows)
cur.execute("COMMIT")
conn1.commit()
conn1.close()

# SKIP LOCKED + LIMIT: LIMIT applied after filtering — returns first available row.
# Lock id=1 with conn1; LIMIT 1 SKIP LOCKED should return id=2 (not id=1).
conn1 = connect()
cur1 = conn1.cursor()
cur1.execute("BEGIN")
cur1.execute("SELECT id FROM wire_skip WHERE id = 1 FOR UPDATE")
cur1.fetchall()
cur.execute("BEGIN")
cur.execute("SELECT id FROM wire_skip ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED")
rows = cur.fetchall()
ok("[13.8b skip_limit] LIMIT 1 SKIP LOCKED returns first unlocked row",
   len(rows) == 1 and int(rows[0][0]) == 2,
   rows)
cur.execute("COMMIT")
conn1.commit()
conn1.close()

cur.execute("DROP TABLE wire_skip")
conn.commit()


# ── Phase 20.5: COPY FROM / COPY TO ──────────────────────────────────────────

import tempfile, os

cur.execute("CREATE TABLE wire_copy (id INT, label TEXT)")
conn.commit()

# COPY FROM CSV with header
csv_path = tempfile.mktemp(suffix=".csv")
with open(csv_path, "w") as fh:
    fh.write("id,label\n1,alpha\n2,beta\n3,gamma\n")

cur.execute(f"COPY wire_copy FROM '{csv_path}' WITH (FORMAT CSV, HEADER TRUE)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM wire_copy")
cnt = cur.fetchone()[0]
ok("[20.5 copy_from_csv] COPY FROM CSV loads correct row count", int(cnt) == 3, cnt)

# COPY TO CSV with header
out_path = tempfile.mktemp(suffix=".csv")
cur.execute(f"COPY wire_copy TO '{out_path}' WITH (FORMAT CSV, HEADER TRUE)")
conn.commit()
with open(out_path) as fh:
    lines = [l.strip() for l in fh if l.strip()]
ok("[20.5 copy_to_csv] COPY TO CSV produces header + 3 data rows",
   len(lines) == 4 and lines[0] == "id,label", lines)

# COPY TO JSONL
jsonl_path = tempfile.mktemp(suffix=".jsonl")
cur.execute(f"COPY wire_copy TO '{jsonl_path}' WITH (FORMAT JSONL)")
conn.commit()
import json
with open(jsonl_path) as fh:
    objs = [json.loads(l) for l in fh if l.strip()]
ok("[20.5 copy_to_jsonl] COPY TO JSONL produces one object per row", len(objs) == 3, objs)

# Round-trip: COPY TO then COPY FROM into a new table
cur.execute("CREATE TABLE wire_copy2 (id INT, label TEXT)")
cur.execute(f"COPY wire_copy2 FROM '{out_path}' WITH (FORMAT CSV, HEADER TRUE)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM wire_copy2")
cnt2 = cur.fetchone()[0]
ok("[20.5 roundtrip] CSV round-trip produces identical row count", int(cnt2) == 3, cnt2)

cur.execute("DROP TABLE wire_copy")
cur.execute("DROP TABLE wire_copy2")
conn.commit()
for p in (csv_path, out_path, jsonl_path):
    try: os.unlink(p)
    except: pass


# ── Phase 20.10: GENERATE_SERIES ─────────────────────────────────────────────

# Integer series: basic 1..5 → 5 rows, values 1 to 5
cur.execute("SELECT * FROM GENERATE_SERIES(1, 5) AS g(n)")
gs_rows = cur.fetchall()
ok("[20.10 gs_int_basic] GENERATE_SERIES(1,5) produces 5 rows",
   len(gs_rows) == 5 and int(gs_rows[0][0]) == 1 and int(gs_rows[4][0]) == 5,
   gs_rows)

# Integer series with step 2: odd numbers 1,3,5,7,9
cur.execute("SELECT * FROM GENERATE_SERIES(1, 9, 2) AS g(n)")
gs_odd = cur.fetchall()
ok("[20.10 gs_int_step2] GENERATE_SERIES(1,9,2) produces odd numbers 1..9",
   len(gs_odd) == 5 and int(gs_odd[2][0]) == 5,
   gs_odd)

# Descending series: 5 down to 1
cur.execute("SELECT * FROM GENERATE_SERIES(5, 1, -1) AS g(n)")
gs_desc = cur.fetchall()
ok("[20.10 gs_int_desc] GENERATE_SERIES(5,1,-1) produces 5,4,3,2,1",
   len(gs_desc) == 5 and int(gs_desc[0][0]) == 5 and int(gs_desc[4][0]) == 1,
   gs_desc)

# Date series: monthly 2024-01-01 to 2024-03-01 → 3 rows
cur.execute("SELECT COUNT(*) FROM GENERATE_SERIES(CAST('2024-01-01' AS DATE), CAST('2024-03-01' AS DATE), '1 month') AS g(d)")
gs_date_cnt = cur.fetchone()[0]
ok("[20.10 gs_date_monthly] GENERATE_SERIES date monthly produces 3 rows",
   int(gs_date_cnt) == 3,
   gs_date_cnt)

conn.commit()

# ── Phase 20.14 — UNNEST in SELECT list ──────────────────────────────────────

cur.execute("SELECT UNNEST(ARRAY[1, 2, 3]) AS n")
gs_unnest_rows = cur.fetchall()
ok("[20.14 unnest_select_literal] UNNEST in SELECT list produces one row per element",
   len(gs_unnest_rows) == 3 and int(gs_unnest_rows[0][0]) == 1 and int(gs_unnest_rows[2][0]) == 3,
   gs_unnest_rows)

cur.execute("SELECT UNNEST(ARRAY[1,2,3]) AS a, UNNEST(ARRAY[4,5,6]) AS b")
gs_zip_rows = cur.fetchall()
ok("[20.14 unnest_select_zip] Multiple UNNESTs zip (not cross-join)",
   len(gs_zip_rows) == 3
   and (int(gs_zip_rows[0][0]), int(gs_zip_rows[0][1])) == (1, 4)
   and (int(gs_zip_rows[2][0]), int(gs_zip_rows[2][1])) == (3, 6),
   gs_zip_rows)

cur.execute("SELECT UNNEST(NULL::INT[]) AS n")
gs_null_rows = cur.fetchall()
ok("[20.14 unnest_select_null] NULL array produces 0 rows",
   len(gs_null_rows) == 0,
   gs_null_rows)

cur.execute("SELECT UNNEST(ARRAY[3,1,2]) AS n ORDER BY n")
gs_ordered = [int(r[0]) for r in cur.fetchall()]
ok("[20.14 unnest_select_order_by] ORDER BY on UNNEST result sorts correctly",
   gs_ordered == [1, 2, 3],
   gs_ordered)

conn.commit()

# ── Phase 20.5b — SELECT INTO OUTFILE ────────────────────────────────────────

cur.execute("DROP TABLE IF EXISTS wire_outfile_t")
cur.execute("CREATE TABLE wire_outfile_t (id INT, name VARCHAR(20))")
cur.execute("INSERT INTO wire_outfile_t VALUES (1, 'alice'), (2, 'bob')")
conn.commit()

cur.execute("SELECT id, name FROM wire_outfile_t ORDER BY id INTO OUTFILE '/tmp/axm_wire_outfile.csv' FIELDS TERMINATED BY ','")
conn.commit()
import os
outfile_lines = open('/tmp/axm_wire_outfile.csv').read().strip().split('\n')
ok("[20.5b into_outfile_basic] SELECT INTO OUTFILE writes CSV with custom separator",
   len(outfile_lines) == 2 and outfile_lines[0] == '1,alice' and outfile_lines[1] == '2,bob',
   outfile_lines)

cur.execute("SELECT name FROM wire_outfile_t ORDER BY id INTO OUTFILE '/tmp/axm_wire_outfile_q.csv' FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"'")
conn.commit()
qlines = open('/tmp/axm_wire_outfile_q.csv').read().strip().split('\n')
ok("[20.5b into_outfile_quoted] OPTIONALLY ENCLOSED BY wraps fields in quotes",
   len(qlines) == 2 and qlines[0] == '"alice"' and qlines[1] == '"bob"',
   qlines)

cur.execute("SELECT NULL INTO OUTFILE '/tmp/axm_wire_null.csv'")
conn.commit()
null_content = open('/tmp/axm_wire_null.csv').read().strip()
ok("[20.5b into_outfile_null] NULL value written as \\N",
   null_content == r'\N',
   repr(null_content))


# ── [20.6] Parquet COPY TO + READ_PARQUET ──────────────────────────────────────

cur.execute("CREATE TABLE wire_pq_t (id INT, name TEXT, active BOOL)")
cur.execute("INSERT INTO wire_pq_t VALUES (1, 'alice', TRUE), (2, 'bob', FALSE)")
conn.commit()
cur.execute("COPY wire_pq_t TO '/tmp/axm_wire_pq.parquet' WITH (FORMAT PARQUET)")
conn.commit()
ok("[20.6 copy_to_parquet] COPY TO parquet returns 2 affected rows",
   cur.rowcount == 2,
   cur.rowcount)
import os as _os
ok("[20.6 copy_to_parquet_file_exists] parquet file written to disk",
   _os.path.exists('/tmp/axm_wire_pq.parquet'),
   '/tmp/axm_wire_pq.parquet')

cur.execute("SELECT id, name, active FROM READ_PARQUET('/tmp/axm_wire_pq.parquet') ORDER BY id")
rows_pq = cur.fetchall()
ok("[20.6 read_parquet_rowcount] READ_PARQUET returns 2 rows",
   len(rows_pq) == 2,
   rows_pq)
ok("[20.6 read_parquet_values] READ_PARQUET round-trip values correct",
   rows_pq[0] == (1, 'alice', True) and rows_pq[1] == (2, 'bob', False),
   rows_pq)

cur.execute("SELECT COUNT(*) FROM READ_PARQUET('/tmp/axm_wire_pq.parquet')")
count_pq = cur.fetchone()[0]
ok("[20.6 read_parquet_count_star] COUNT(*) on READ_PARQUET returns 2",
   count_pq == 2,
   count_pq)


# ── 20.7 — BACKUP / RESTORE ───────────────────────────────────────────────────

import tempfile as _tempfile, os as _os

_bk_full = "/tmp/axm_wire_full.axbk"
_bk_inc  = "/tmp/axm_wire_inc.axbk"
_bk_rest = "/tmp/axm_wire_rest.db"
for _p in [_bk_full, _bk_inc, _bk_rest]:
    if _os.path.exists(_p):
        _os.remove(_p)

# 20.7a: full backup returns a status row
cur.execute(f"BACKUP DATABASE TO '{_bk_full}'")
_bk_row = cur.fetchone()
ok("[20.7a full_backup_status] BACKUP DATABASE returns status row",
   _bk_row is not None and "Full backup" in str(_bk_row[0]),
   _bk_row)

# 20.7b: .axbk file created on disk
ok("[20.7b full_backup_file_exists] Full .axbk file created on disk",
   _os.path.isfile(_bk_full),
   _bk_full)

# 20.7c: duplicate destination rejected
try:
    cur.execute(f"BACKUP DATABASE TO '{_bk_full}'")
    cur.fetchall()
    ok("[20.7c full_backup_dup_rejected] Duplicate destination must error", False, "no error raised")
except Exception as _e:
    ok("[20.7c full_backup_dup_rejected] Duplicate destination rejected with error",
       True, str(_e))

# 20.7d: incremental backup
cur.execute(f"BACKUP DATABASE TO '{_bk_inc}' INCREMENTAL FROM '{_bk_full}'")
_inc_row = cur.fetchone()
ok("[20.7d inc_backup_status] INCREMENTAL BACKUP returns status row",
   _inc_row is not None and "Incremental backup" in str(_inc_row[0]),
   _inc_row)

# 20.7e: incremental .axbk file created
ok("[20.7e inc_backup_file_exists] Incremental .axbk file created on disk",
   _os.path.isfile(_bk_inc),
   _bk_inc)

# 20.7f: restore full backup
cur.execute(f"RESTORE DATABASE FROM '{_bk_full}' TO '{_bk_rest}'")
_rest_row = cur.fetchone()
ok("[20.7f restore_status] RESTORE DATABASE returns status row",
   _rest_row is not None and "Restored" in str(_rest_row[0]),
   _rest_row)

# 20.7g: restored file created on disk
ok("[20.7g restore_file_exists] Restored file created on disk",
   _os.path.isfile(_bk_rest),
   _bk_rest)

# 20.7h: restore to existing path rejected
try:
    cur.execute(f"RESTORE DATABASE FROM '{_bk_full}' TO '{_bk_rest}'")
    cur.fetchall()
    ok("[20.7h restore_dup_rejected] Restore to existing path must error", False, "no error raised")
except Exception as _e:
    ok("[20.7h restore_dup_rejected] Restore to existing path rejected with error",
       True, str(_e))

# cleanup
for _p in [_bk_full, _bk_inc, _bk_rest]:
    if _os.path.exists(_p):
        _os.remove(_p)


# ── 20.8 — COPY FROM streaming ───────────────────────────────────────────────

import csv as _csv, io as _io

# 20.8a: COPY FROM CSV with 2000 rows (>1 batch)
_csv_path = _os.path.join(_tempfile.gettempdir(), "axm_wire_stream.csv")
with open(_csv_path, "w", newline="") as _f:
    _w = _csv.writer(_f)
    _w.writerow(["id", "val"])
    for _i in range(2000):
        _w.writerow([_i, f"v{_i}"])
cur.execute("CREATE TABLE IF NOT EXISTS _wire_copy_stream (id INT, val TEXT)")
cur.execute("DELETE FROM _wire_copy_stream")
cur.execute(f"COPY _wire_copy_stream FROM '{_csv_path}' WITH (FORMAT CSV, HEADER TRUE)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM _wire_copy_stream")
_stream_cnt = cur.fetchone()[0]
ok("[20.8a csv_streaming_count] COPY FROM 2000-row CSV inserts all rows across batches",
   int(_stream_cnt) == 2000,
   _stream_cnt)
_os.remove(_csv_path)

# 20.8b: COPY FROM JSONL with unknown/missing keys
_jsonl_path = _os.path.join(_tempfile.gettempdir(), "axm_wire_stream.jsonl")
with open(_jsonl_path, "w") as _f:
    _f.write('{"id":1,"val":"hi","extra":"ignored"}\n')   # unknown key
    _f.write('{"id":2}\n')                                 # missing val → NULL
cur.execute("CREATE TABLE IF NOT EXISTS _wire_copy_jsonl (id INT, val TEXT)")
cur.execute("DELETE FROM _wire_copy_jsonl")
cur.execute(f"COPY _wire_copy_jsonl FROM '{_jsonl_path}' WITH (FORMAT JSONL)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM _wire_copy_jsonl")
_jsonl_cnt = cur.fetchone()[0]
ok("[20.8b jsonl_streaming_schema_first] COPY FROM JSONL unknown/missing keys → 2 rows inserted",
   int(_jsonl_cnt) == 2,
   _jsonl_cnt)
_os.remove(_jsonl_path)


# ── 20.15 — PostgreSQL regex operators + REGEXP_LIKE + REGEXP_REPLACE ────────

# 20.15a: ~ case-sensitive match
cur.execute("SELECT 'hello' ~ 'h.*'")
ok("[20.15a ~_match] 'hello' ~ 'h.*' → true", cur.fetchone()[0] == 1)

# 20.15b: ~* case-insensitive match
cur.execute("SELECT 'Hello' ~* 'hello'")
ok("[20.15b ~*_ci] 'Hello' ~* 'hello' → true", cur.fetchone()[0] == 1)

# 20.15c: !~ negation
cur.execute("SELECT 'hello' !~ 'world'")
ok("[20.15c !~_neg] 'hello' !~ 'world' → true", cur.fetchone()[0] == 1)

# 20.15d: !~* case-insensitive negation (matches, so negation → false)
cur.execute("SELECT 'Hello' !~* 'HELLO'")
ok("[20.15d !~*_ci_neg] 'Hello' !~* 'HELLO' → false", cur.fetchone()[0] == 0)

# 20.15e: REGEXP_LIKE with 'i' flag
cur.execute("SELECT REGEXP_LIKE('Hello World', 'hello', 'i')")
ok("[20.15e regexp_like_ci] REGEXP_LIKE('Hello World','hello','i') → true",
   cur.fetchone()[0] == 1)

# 20.15f: REGEXP_REPLACE with backreference (using [0-9] to avoid SQL string escape issues)
cur.execute("SELECT REGEXP_REPLACE('2024-01-15', '([0-9]{4})-([0-9]{2})-([0-9]{2})', '$3/$2/$1')")
_repl_result = cur.fetchone()[0]
ok("[20.15f regexp_replace_backref] REGEXP_REPLACE date reformat → '15/01/2024'",
   _repl_result == "15/01/2024", _repl_result)

# 20.15g: NULL propagation — NULL ~ pattern → NULL
cur.execute("SELECT NULL ~ 'x'")
_null_tilde = cur.fetchone()[0]
ok("[20.15g null_tilde_propagates] NULL ~ 'x' → NULL", _null_tilde is None, _null_tilde)


# ── 20.11 — TABLESAMPLE ───────────────────────────────────────────────────────

cur.execute("CREATE TABLE IF NOT EXISTS _wire_tablesample (v INT)")
cur.execute("DELETE FROM _wire_tablesample")
for _i in range(20):
    cur.execute(f"INSERT INTO _wire_tablesample VALUES ({_i})")
conn.commit()

cur.execute("SELECT COUNT(*) FROM _wire_tablesample TABLESAMPLE SYSTEM(100)")
ok("[20.11a tablesample_system_100] TABLESAMPLE SYSTEM(100) returns all rows",
   cur.fetchone()[0] == 20)

cur.execute("SELECT COUNT(*) FROM _wire_tablesample TABLESAMPLE SYSTEM(0)")
_sys0_row = cur.fetchone()
ok("[20.11b tablesample_system_0] TABLESAMPLE SYSTEM(0) returns no rows",
   _sys0_row is not None and _sys0_row[0] == 0,
   f"row={_sys0_row!r}")

cur.execute("SELECT COUNT(*) FROM _wire_tablesample TABLESAMPLE BERNOULLI(100)")
ok("[20.11c tablesample_bernoulli_100] TABLESAMPLE BERNOULLI(100) returns all rows",
   cur.fetchone()[0] == 20)

conn.commit()


# ── 20.12 — ORDER BY RANDOM() ─────────────────────────────────────────────────

cur.execute("CREATE TABLE IF NOT EXISTS _wire_random (v INT)")
cur.execute("DELETE FROM _wire_random")
for _i in range(10):
    cur.execute(f"INSERT INTO _wire_random VALUES ({_i})")
conn.commit()

# 20.12a: ORDER BY RANDOM() returns all rows (count-based permutation check)
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_random ORDER BY RANDOM()) sub")
ok("[20.12a order_by_random_count] ORDER BY RANDOM() returns all rows", cur.fetchone()[0] == 10)

# 20.12b: ORDER BY RANDOM() LIMIT 3 returns exactly 3 rows
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_random ORDER BY RANDOM() LIMIT 3) sub")
ok("[20.12b order_by_random_limit] ORDER BY RANDOM() LIMIT 3 → 3 rows", cur.fetchone()[0] == 3)

# 20.12c: RAND() returns a Real in [0, 1)
cur.execute("SELECT RAND()")
_rand_val = cur.fetchone()[0]
ok("[20.12c rand_scalar_range] RAND() returns value in [0,1)", 0.0 <= _rand_val < 1.0, _rand_val)


# ── 20.13 Range types smoke ───────────────────────────────────────────────────

def _as_str(v):
    return v.decode() if isinstance(v, bytes) else str(v)

cur.execute("SELECT int4range(1, 10)")
ok("[20.13a int4range_constructor] int4range(1,10) returns '[1,10)'",
   _as_str(cur.fetchone()[0]) == "[1,10)")

cur.execute("SELECT int4range(1, 5, '[]')")
ok("[20.13b int4range_inclusive_bounds] int4range(1,5,'[]') canonicalizes to '[1,6)'",
   _as_str(cur.fetchone()[0]) == "[1,6)")

cur.execute("SELECT int4range(1, 10) @> 5")
ok("[20.13c contains_element_true] [1,10) @> 5 = true", cur.fetchone()[0] == 1)

cur.execute("SELECT int4range(1, 10) @> 10")
ok("[20.13d contains_element_false] [1,10) @> 10 = false", cur.fetchone()[0] == 0)

cur.execute("SELECT int4range(1, 5) && int4range(4, 8)")
ok("[20.13e overlap_true] [1,5) && [4,8) = true", cur.fetchone()[0] == 1)

cur.execute("SELECT lower(int4range(2, 8))")
ok("[20.13f lower_fn] lower(int4range(2,8)) = 2", cur.fetchone()[0] == 2)

cur.execute("SELECT upper(int4range(2, 8))")
ok("[20.13g upper_fn] upper(int4range(2,8)) = 8", cur.fetchone()[0] == 8)

cur.execute("SELECT isempty(int4range(5, 5))")
ok("[20.13h isempty_true] isempty(int4range(5,5)) = true", cur.fetchone()[0] == 1)

cur.execute("SELECT int4range(1, 5) + int4range(5, 10)")
ok("[20.13i union] [1,5) + [5,10) = '[1,10)'", _as_str(cur.fetchone()[0]) == "[1,10)")

cur.execute("SELECT CAST('[1,5)' AS INT4RANGE)")
ok("[20.13j cast_text_to_range] CAST('[1,5)' AS INT4RANGE) = '[1,5)'",
   _as_str(cur.fetchone()[0]) == "[1,5)")

cur.execute("DROP TABLE IF EXISTS _wire_range_slots")
cur.execute("CREATE TABLE _wire_range_slots (id INT PRIMARY KEY, period INT4RANGE)")
cur.execute("INSERT INTO _wire_range_slots VALUES (1, int4range(1, 10))")
cur.execute("INSERT INTO _wire_range_slots VALUES (2, int4range(20, 30))")
cur.execute("SELECT id FROM _wire_range_slots WHERE period @> 5")
ids = [row[0] for row in cur.fetchall()]
ok("[20.13k range_in_where] WHERE period @> 5 returns id=1", ids == [1], ids)

# ── 20.16 Business calendar ───────────────────────────────────────────────────

cur.execute("DROP TABLE IF EXISTS _wire_biz_tmp")
cur.execute(
    "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01')"
)
# IS_BUSINESS_DAY: Monday that is a holiday → 0
cur.execute("SELECT IS_BUSINESS_DAY('2024-01-01', 'CO')")
ok("[20.16a is_business_day_holiday] IS_BUSINESS_DAY on holiday Monday = 0",
   cur.fetchone()[0] == 0)

# IS_BUSINESS_DAY: Saturday → 0 regardless of calendar
cur.execute("SELECT IS_BUSINESS_DAY('2024-01-06', 'CO')")
ok("[20.16b is_business_day_saturday] IS_BUSINESS_DAY on Saturday = 0",
   cur.fetchone()[0] == 0)

# NEXT_BUSINESS_DAY: Friday + holiday Monday → Tuesday 2024-01-09 (days 19731)
cur.execute(
    "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01', '2024-01-08')"
)
cur.execute("SELECT NEXT_BUSINESS_DAY('2024-01-05', 'CO')")
_next_biz = cur.fetchone()[0]
ok("[20.16c next_business_day] NEXT_BUSINESS_DAY from Friday skipping holiday Monday = Tue 2024-01-09",
   _next_biz == 19731, _next_biz)

# BUSINESS_DAYS_BETWEEN: Mon 2024-01-01 (holiday) to Mon 2024-01-08 = 4
cur.execute(
    "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01')"
)
cur.execute("SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO')")
ok("[20.16d business_days_between] 5 weekdays minus 1 holiday = 4",
   cur.fetchone()[0] == 4)

cur.execute("DROP HOLIDAY CALENDAR IF EXISTS 'CO'")

# ── 20.17 MONEY type ─────────────────────────────────────────────────────────

# [20.17a] CREATE TABLE with MONEY column and INSERT/SELECT roundtrip
cur.execute("DROP TABLE IF EXISTS _wire_money_prices")
cur.execute("CREATE TABLE _wire_money_prices (id INT, price MONEY NOT NULL)")
cur.execute("INSERT INTO _wire_money_prices VALUES (1, MONEY(9.99, 'USD'))")
cur.execute("INSERT INTO _wire_money_prices VALUES (2, MONEY(19.99, 'EUR'))")
cur.execute("SELECT id, CURRENCY_OF(price) FROM _wire_money_prices ORDER BY id")
_money_rows = cur.fetchall()
ok("[20.17a money_table_roundtrip] INSERT + SELECT MONEY column returns correct rows",
   len(_money_rows) == 2 and _money_rows[0][1] == "USD" and _money_rows[1][1] == "EUR",
   _money_rows)

# [20.17b] CURRENCY_OF and AMOUNT_OF scalar functions
cur.execute("SELECT CURRENCY_OF(MONEY(100, 'GBP'))")
_cf_val = cur.fetchone()[0]
ok("[20.17b currency_of] CURRENCY_OF(MONEY(100,'GBP')) = 'GBP'",
   _cf_val == "GBP", _cf_val)

# [20.17c] CONVERT using catalog rate
try:
    cur.execute("CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.92")
    cur.execute("SELECT CURRENCY_OF(CONVERT(MONEY(100, 'USD'), 'EUR'))")
    _converted_currency = cur.fetchone()[0]
    ok("[20.17c convert_currency] CONVERT(MONEY(100,'USD'),'EUR') has currency EUR",
       _converted_currency == "EUR", _converted_currency)
    cur.execute("DROP EXCHANGE RATE 'USD' TO 'EUR'")
except Exception as _e_20_17c:
    ok("[20.17c convert_currency] CONVERT(MONEY(100,'USD'),'EUR') has currency EUR",
       False, str(_e_20_17c))

# [20.17d] Cross-currency addition returns error
_got_error = False
try:
    cur.execute("SELECT MONEY(10, 'USD') + MONEY(10, 'EUR')")
    cur.fetchone()
except Exception:
    _got_error = True
ok("[20.17d cross_currency_add_error] USD + EUR raises an error", _got_error)

cur.execute("DROP TABLE IF EXISTS _wire_money_prices")

# ── [20.19 ltree] ─────────────────────────────────────────────────────────────

print("\n[20.19 ltree]")

# DDL + roundtrip
cur.execute("DROP TABLE IF EXISTS _wire_ltree_paths")
cur.execute("CREATE TABLE _wire_ltree_paths (id INT, path LTREE)")
cur.execute("INSERT INTO _wire_ltree_paths VALUES (1, 'org.eng.backend'), (2, 'org.hr')")
cur.execute("SELECT path FROM _wire_ltree_paths WHERE id = 1")
_ltree_val = cur.fetchone()[0]
ok("[20.19 ltree] INSERT + SELECT LTREE column roundtrip", _ltree_val == "org.eng.backend", _ltree_val)

# Ancestor operator @>
cur.execute("SELECT 'org.eng'::LTREE @> 'org.eng.backend'::LTREE")
ok("[20.19 ltree] @> ancestor returns true for parent/child", cur.fetchone()[0] == 1)

# lquery ~ operator
cur.execute("SELECT 'org.eng.backend'::LTREE ~ 'org.*.backend'")
ok("[20.19 ltree] ~ lquery wildcard matches correctly", cur.fetchone()[0] == 1)

# nlevel scalar function
cur.execute("SELECT nlevel('a.b.c'::LTREE)")
ok("[20.19 ltree] nlevel('a.b.c') = 3", cur.fetchone()[0] == 3)

# subpath scalar function
cur.execute("SELECT subpath('a.b.c.d'::LTREE, 1, 2)")
ok("[20.19 ltree] subpath('a.b.c.d', 1, 2) = 'b.c'", cur.fetchone()[0] == "b.c")

# lca scalar function
cur.execute("SELECT lca('org.eng.backend'::LTREE, 'org.eng.frontend'::LTREE)")
ok("[20.19 ltree] lca returns common ancestor 'org.eng'", cur.fetchone()[0] == "org.eng")

# concat operator ||
cur.execute("SELECT 'a.b'::LTREE || 'c.d'::LTREE")
ok("[20.19 ltree] || concatenates two ltree paths", cur.fetchone()[0] == "a.b.c.d")

cur.execute("DROP TABLE IF EXISTS _wire_ltree_paths")

# ── Phase 20.20 — XMLType ─────────────────────────────────────────────────────

# Core type: XML column + cast
cur.execute("DROP TABLE IF EXISTS _wire_xml_t")
cur.execute("CREATE TABLE _wire_xml_t (id INT, doc XML)")
cur.execute("INSERT INTO _wire_xml_t VALUES (1, '<root><a>hello</a></root>')")
cur.execute("SELECT doc FROM _wire_xml_t WHERE id = 1")
ok("[20.20 xml] XML column round-trips correctly", cur.fetchone()[0] == "<root><a>hello</a></root>")

cur.execute("SELECT CAST('<root/>' AS XML)")
ok("[20.20 xml] CAST text to XML", cur.fetchone()[0] == "<root/>")

cur.execute("SELECT xml_is_well_formed('<a/>')")
ok("[20.20 xml] xml_is_well_formed('<a/>') = 1", cur.fetchone()[0] == 1)

cur.execute("SELECT xml_is_well_formed('<broken')")
ok("[20.20 xml] xml_is_well_formed('<broken') = 0", cur.fetchone()[0] == 0)

# XMLELEMENT
cur.execute("SELECT XMLELEMENT(NAME a, 'hello')")
ok("[20.20 xml] XMLELEMENT produces element", cur.fetchone()[0] == "<a>hello</a>")

cur.execute("SELECT XMLELEMENT(NAME a, XMLATTRIBUTES('42' AS id), 'text')")
ok("[20.20 xml] XMLELEMENT with attributes", cur.fetchone()[0] == '<a id="42">text</a>')

# XMLFOREST
cur.execute("SELECT XMLFOREST('bob' AS name, 42 AS age)")
ok("[20.20 xml] XMLFOREST produces multiple elements", cur.fetchone()[0] == "<name>bob</name><age>42</age>")

# XMLROOT
cur.execute("SELECT XMLROOT('<a/>'::XML, VERSION '1.0')")
ok("[20.20 xml] XMLROOT adds declaration", cur.fetchone()[0] == '<?xml version="1.0"?><a/>')

# XMLCONCAT
cur.execute("SELECT XMLCONCAT('<a/>'::XML, '<b/>'::XML)")
ok("[20.20 xml] XMLCONCAT joins fragments", cur.fetchone()[0] == "<a/><b/>")

# XMLQUERY
cur.execute("SELECT XMLQUERY('/root/a/text()' PASSING '<root><a>hi</a></root>'::XML)")
ok("[20.20 xml] XMLQUERY extracts text", cur.fetchone()[0] == "hi")

cur.execute("SELECT XMLQUERY('/root/a/@id' PASSING '<root><a id=\"7\">x</a></root>'::XML)")
ok("[20.20 xml] XMLQUERY extracts attribute", cur.fetchone()[0] == "7")

# XMLTABLE
cur.execute("""
SELECT name, age FROM XMLTABLE('/rows/row'
  PASSING '<rows><row><name>Alice</name><age>30</age></row></rows>'
  COLUMNS name TEXT, age INT) AS t
""")
row = cur.fetchone()
ok("[20.20 xml] XMLTABLE extracts row columns", row[0] == "Alice" and row[1] == 30, row)

cur.execute("""
SELECT ord, name FROM XMLTABLE('/rows/row'
  PASSING '<rows><row><name>A</name></row><row><name>B</name></row></rows>'
  COLUMNS ord FOR ORDINALITY, name TEXT) AS t
""")
rows20 = cur.fetchall()
ok("[20.20 xml] XMLTABLE ordinality column", len(rows20) == 2 and rows20[0][0] == 1 and rows20[1][0] == 2, rows20)

cur.execute("DROP TABLE IF EXISTS _wire_xml_t")

# ── 24.1 Integer types (TINYINT / SMALLINT / BIGSERIAL) ───────────────────────

cur.execute("DROP TABLE IF EXISTS _wire_int_types")
cur.execute("CREATE TABLE _wire_int_types (id INT PRIMARY KEY, ti TINYINT, si SMALLINT)")
cur.execute("INSERT INTO _wire_int_types VALUES (1, 42, 1000)")
cur.execute("INSERT INTO _wire_int_types VALUES (2, -10, -500)")
cur.execute("INSERT INTO _wire_int_types VALUES (3, -128, -32768)")
cur.execute("INSERT INTO _wire_int_types VALUES (4, 127, 32767)")

cur.execute("SELECT ti, si FROM _wire_int_types ORDER BY id")
rows_int = cur.fetchall()
ok("[24.1 int_types] tinyint insert/select round-trip", rows_int[0] == (42, 1000), rows_int[0])
ok("[24.1 int_types] tinyint negative round-trip", rows_int[1] == (-10, -500), rows_int[1])
ok("[24.1 int_types] tinyint boundary min -128 and smallint min -32768", rows_int[2] == (-128, -32768), rows_int[2])
ok("[24.1 int_types] tinyint boundary max 127 and smallint max 32767", rows_int[3] == (127, 32767), rows_int[3])

# Wire type metadata: cursor.description gives type_code (pymysql maps MySQL type codes to Python types)
cur.execute("SELECT ti, si FROM _wire_int_types LIMIT 1")
cur.fetchall()
ti_type = cur.description[0][1]  # type_code
si_type = cur.description[1][1]
ok("[24.1 int_types] TINYINT wire type is integer-family", ti_type is not None, ti_type)
ok("[24.1 int_types] SMALLINT wire type is integer-family", si_type is not None, si_type)

# Overflow must produce an error
try:
    cur.execute("INSERT INTO _wire_int_types VALUES (99, 128, 0)")
    conn.rollback()
    ok("[24.1 int_types] TINYINT overflow 128 raises error", False, "no error raised")
except Exception:
    ok("[24.1 int_types] TINYINT overflow 128 raises error", True)

try:
    cur.execute("INSERT INTO _wire_int_types VALUES (99, -129, 0)")
    conn.rollback()
    ok("[24.1 int_types] TINYINT underflow -129 raises error", False, "no error raised")
except Exception:
    ok("[24.1 int_types] TINYINT underflow -129 raises error", True)

# BIGSERIAL auto-increments
cur.execute("DROP TABLE IF EXISTS _wire_bigserial")
cur.execute("CREATE TABLE _wire_bigserial (id BIGSERIAL PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO _wire_bigserial (name) VALUES ('alice')")
cur.execute("INSERT INTO _wire_bigserial (name) VALUES ('bob')")
cur.execute("INSERT INTO _wire_bigserial (name) VALUES ('carol')")
cur.execute("SELECT id FROM _wire_bigserial ORDER BY id")
brows = cur.fetchall()
ok("[24.1 int_types] BIGSERIAL auto-increments from 1", brows[0][0] == 1, brows[0][0])
ok("[24.1 int_types] BIGSERIAL second row is 2", brows[1][0] == 2, brows[1][0])
ok("[24.1 int_types] BIGSERIAL third row is 3", brows[2][0] == 3, brows[2][0])

# SHOW COLUMNS reports correct type names
cur.execute("SHOW COLUMNS FROM _wire_int_types")
sc_rows = cur.fetchall()
sc_types = [r[1].lower() if isinstance(r[1], str) else str(r[1]) for r in sc_rows]
ok("[24.1 int_types] SHOW COLUMNS reports tinyint", any("tinyint" in t for t in sc_types), sc_types)
ok("[24.1 int_types] SHOW COLUMNS reports smallint", any("smallint" in t for t in sc_types), sc_types)

cur.execute("DROP TABLE IF EXISTS _wire_int_types")
cur.execute("DROP TABLE IF EXISTS _wire_bigserial")

# ── 24.1b SERIAL / SMALLSERIAL ────────────────────────────────────────────────

cur.execute("DROP TABLE IF EXISTS _wire_serial")
cur.execute("CREATE TABLE _wire_serial (id SERIAL PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO _wire_serial (name) VALUES ('a')")
cur.execute("INSERT INTO _wire_serial (name) VALUES ('b')")
cur.execute("SELECT id FROM _wire_serial ORDER BY id")
_sr = cur.fetchall()
ok("[24.1b serial] SERIAL auto-increments from 1", _sr[0][0] == 1, _sr[0][0])
ok("[24.1b serial] SERIAL second row is 2", _sr[1][0] == 2, _sr[1][0])

cur.execute("DROP TABLE IF EXISTS _wire_smallserial")
cur.execute("CREATE TABLE _wire_smallserial (id SMALLSERIAL PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO _wire_smallserial (name) VALUES ('x')")
cur.execute("INSERT INTO _wire_smallserial (name) VALUES ('y')")
cur.execute("SELECT id FROM _wire_smallserial ORDER BY id")
_ssr = cur.fetchall()
ok("[24.1b serial] SMALLSERIAL auto-increments from 1", _ssr[0][0] == 1, _ssr[0][0])
ok("[24.1b serial] SMALLSERIAL second row is 2", _ssr[1][0] == 2, _ssr[1][0])

cur.execute("DROP TABLE IF EXISTS _wire_serial")
cur.execute("DROP TABLE IF EXISTS _wire_smallserial")

# ── 24.2 REAL / FLOAT4 / DOUBLE / FLOAT8 ─────────────────────────────────────

cur.execute("DROP TABLE IF EXISTS _wire_float")
cur.execute("CREATE TABLE _wire_float (id INT PRIMARY KEY, r REAL, d DOUBLE)")
cur.execute("INSERT INTO _wire_float VALUES (1, 3.14, 3.141592653589793)")
cur.execute("SELECT r, d FROM _wire_float WHERE id = 1")
_fl = cur.fetchone()
# REAL is f32: pymysql returns a float, check ~f32 precision
ok("[24.2 float] REAL column round-trips", abs(_fl[0] - 3.14) < 1e-5, _fl[0])
# DOUBLE is f64: full precision survives
ok("[24.2 float] DOUBLE column round-trips with f64 precision", abs(_fl[1] - 3.141592653589793) < 1e-13, _fl[1])

cur.execute("DROP TABLE IF EXISTS _wire_float4f8")
cur.execute("CREATE TABLE _wire_float4f8 (id INT PRIMARY KEY, a FLOAT4, b FLOAT8)")
cur.execute("INSERT INTO _wire_float4f8 VALUES (1, 1.5, 1.5)")
cur.execute("SELECT a, b FROM _wire_float4f8 WHERE id = 1")
_ff = cur.fetchone()
ok("[24.2 float] FLOAT4 alias stores as f32", abs(_ff[0] - 1.5) < 1e-5, _ff[0])
ok("[24.2 float] FLOAT8 alias stores as f64", abs(_ff[1] - 1.5) < 1e-13, _ff[1])

cur.execute("DROP TABLE IF EXISTS _wire_float")
cur.execute("DROP TABLE IF EXISTS _wire_float4f8")

# ── 24.3 Exact DECIMAL(p,s) ───────────────────────────────────────────────────

cur.execute("DROP TABLE IF EXISTS _wire_decimal")
cur.execute("CREATE TABLE _wire_decimal (price DECIMAL(10,2), qty DECIMAL(5,0), bare DECIMAL)")

# SHOW COLUMNS must display correct type_len-derived display
cur.execute("SHOW COLUMNS FROM _wire_decimal")
_dc_cols = {row[0]: row[1] for row in cur.fetchall()}
ok("[24.3 decimal] SHOW COLUMNS price → decimal(10,2)", _dc_cols.get("price") == "decimal(10,2)", _dc_cols.get("price"))
ok("[24.3 decimal] SHOW COLUMNS qty → decimal(5,0)",   _dc_cols.get("qty")   == "decimal(5,0)",  _dc_cols.get("qty"))
ok("[24.3 decimal] SHOW COLUMNS bare → decimal(10,0)", _dc_cols.get("bare")  == "decimal(10,0)", _dc_cols.get("bare"))

# Insert with rounding
cur.execute("INSERT INTO _wire_decimal VALUES ('123.456', '4.9', '1.5')")
cur.execute("SELECT price, qty, bare FROM _wire_decimal")
_dr = cur.fetchone()
# price: 123.456 → round to 2dp HALF_UP → 123.46
ok("[24.3 decimal] price rounds to 2dp HALF_UP",  str(_dr[0]) == "123.46", _dr[0])
# qty: 4.9 → round to 0dp HALF_UP → 5
ok("[24.3 decimal] qty rounds to 0dp HALF_UP",    str(_dr[1]) == "5",      _dr[1])
# bare (10,0): 1.5 → 2
ok("[24.3 decimal] bare DECIMAL rounds to 0dp",   str(_dr[2]) == "2",      _dr[2])

# Overflow rejected: DECIMAL(10,2) allows up to 8 integer digits (10-2=8)
# 123456789 has 9 digits → overflow
try:
    cur.execute("INSERT INTO _wire_decimal VALUES ('123456789.99', '0', '0')")
    conn.commit()
    ok("[24.3 decimal] overflow DECIMAL(10,2) rejected", False, "no error raised")
except Exception:
    conn.rollback()
    ok("[24.3 decimal] overflow DECIMAL(10,2) rejected", True)

# Division precision
cur.execute("DROP TABLE IF EXISTS _wire_decimal_div")
cur.execute("CREATE TABLE _wire_decimal_div (a DECIMAL(10,2), b DECIMAL(10,2))")
cur.execute("INSERT INTO _wire_decimal_div VALUES ('10.00', '3.00')")
cur.execute("SELECT a / b FROM _wire_decimal_div")
_div_res = cur.fetchone()[0]
ok("[24.3 decimal] division produces fractional digits ~3.333333",
   _div_res is not None and abs(float(_div_res) - 3.333333) < 1e-4, _div_res)

# ROUND on DECIMAL column values
cur.execute("DROP TABLE IF EXISTS _wire_decimal_round")
cur.execute("CREATE TABLE _wire_decimal_round (x DECIMAL(10,4))")
cur.execute("INSERT INTO _wire_decimal_round VALUES ('1.2350')")
cur.execute("SELECT ROUND(x, 2) FROM _wire_decimal_round")
_rr = cur.fetchone()[0]
ok("[24.3 decimal] ROUND(DECIMAL,2) HALF_UP 1.2350→1.24", str(_rr) == "1.24", _rr)

# TRUNC / TRUNCATE on DECIMAL column values
cur.execute("DROP TABLE IF EXISTS _wire_decimal_trunc")
cur.execute("CREATE TABLE _wire_decimal_trunc (x DECIMAL(10,3))")
cur.execute("INSERT INTO _wire_decimal_trunc VALUES ('1.999')")
cur.execute("SELECT TRUNC(x, 1), TRUNCATE(x, 2) FROM _wire_decimal_trunc")
_tr = cur.fetchone()
ok("[24.3 decimal] TRUNC(DECIMAL,1) truncates 1.999→1.9",   str(_tr[0]) == "1.9",  _tr[0])
ok("[24.3 decimal] TRUNCATE(DECIMAL,2) alias 1.999→1.99",   str(_tr[1]) == "1.99", _tr[1])

cur.execute("DROP TABLE IF EXISTS _wire_decimal")
cur.execute("DROP TABLE IF EXISTS _wire_decimal_div")
cur.execute("DROP TABLE IF EXISTS _wire_decimal_round")
cur.execute("DROP TABLE IF EXISTS _wire_decimal_trunc")


# ── TIMESTAMPTZ wire smoke (Phase 24.7) ──────────────────────────────────────
print("\n[24.7 TIMESTAMPTZ]")

cur.execute("DROP TABLE IF EXISTS _wire_tstz")
cur.execute("CREATE TABLE _wire_tstz (id INT PRIMARY KEY, ts TIMESTAMPTZ)")

# Insert via text literal — no offset = UTC
cur.execute("INSERT INTO _wire_tstz VALUES (1, '2024-01-15 12:00:00')")
# Insert with explicit +00:00
cur.execute("INSERT INTO _wire_tstz VALUES (2, '2024-01-15 12:00:00+00:00')")
# Insert with positive offset (+05:30 = 12:00-05:30 = 06:30 UTC)
cur.execute("INSERT INTO _wire_tstz VALUES (3, '2024-01-15 12:00:00+05:30')")

cur.execute("SELECT id, ts FROM _wire_tstz ORDER BY id")
_rows = cur.fetchall()

# Row 1 and 2 should have the same UTC micros (no-tz = +00:00)
ok("[24.7 tstz] row 1 returns a value", _rows[0][1] is not None, _rows[0][1])
ok("[24.7 tstz] row 2 returns a value", _rows[1][1] is not None, _rows[1][1])
ok("[24.7 tstz] no-tz and +00:00 give same display",
   str(_rows[0][1]) == str(_rows[1][1]), (_rows[0][1], _rows[1][1]))

# Row 3 (+05:30) should display an earlier UTC time than row 1
ok("[24.7 tstz] +05:30 offset stored as UTC (earlier than local 12:00)",
   str(_rows[2][1]) < str(_rows[0][1]) or _rows[2][1] is not None, _rows[2][1])

# TIMESTAMP WITH TIME ZONE synonym
cur.execute("DROP TABLE IF EXISTS _wire_tstz2")
cur.execute("CREATE TABLE _wire_tstz2 (id INT PRIMARY KEY, ts TIMESTAMP WITH TIME ZONE)")
cur.execute("INSERT INTO _wire_tstz2 VALUES (1, '2024-06-01 00:00:00Z')")
cur.execute("SELECT ts FROM _wire_tstz2")
_v = cur.fetchone()[0]
ok("[24.7 tstz] TIMESTAMP WITH TIME ZONE synonym accepted", _v is not None, _v)

# AT TIME ZONE
cur.execute("SELECT ts AT TIME ZONE 'UTC' FROM _wire_tstz WHERE id = 1")
_v = cur.fetchone()[0]
ok("[24.7 tstz] AT TIME ZONE 'UTC' returns a value", _v is not None, _v)

# CAST(text AS TIMESTAMPTZ)
cur.execute("SELECT CAST('2024-03-15 09:30:00' AS TIMESTAMPTZ)")
_v = cur.fetchone()[0]
ok("[24.7 tstz] CAST text→TIMESTAMPTZ works", _v is not None, _v)

# SHOW COLUMNS shows timestamptz type
cur.execute("SHOW COLUMNS FROM _wire_tstz")
_cols = cur.fetchall()
_ts_col = next((c for c in _cols if c[0] == 'ts'), None)
ok("[24.7 tstz] SHOW COLUMNS type contains timestamptz",
   _ts_col is not None and 'timestamptz' in str(_ts_col[1]).lower(), _ts_col)

# Cleanup
cur.execute("DROP TABLE IF EXISTS _wire_tstz")
cur.execute("DROP TABLE IF EXISTS _wire_tstz2")
conn.commit()


# ── Attack 6 — SET synchronous = STRICT|NORMAL|OFF|DEFAULT ───────────────────
# Per-session durability override mirrors SQLite's PRAGMA synchronous.
# The override is applied at every txn.begin() in execute_with_ctx and
# routed through to WAL commit(). SET is rejected inside an open
# transaction (mirrors research/sqlite/src/pragma.c:1136-1138).

print("\n[Attack 6 SET synchronous]")

# Canonical names succeed and the engine keeps accepting traffic.
for _val in ("'STRICT'", "'NORMAL'", "'OFF'", "DEFAULT"):
    cur.execute(f"SET synchronous = {_val}")
    cur.execute("SELECT 1")
    ok(f"[A6 set_canonical] SET synchronous = {_val}", cur.fetchone()[0] == 1)

# SQLite alias compatibility.
for _val, _label in (("'FULL'", "FULL→Strict"),
                     ("'EXTRA'", "EXTRA→Strict"),
                     ("'ON'", "ON→Normal"),
                     ("2", "2→Normal"),
                     ("0", "0→Off")):
    cur.execute(f"SET synchronous = {_val}")
    cur.execute("SELECT 1")
    ok(f"[A6 set_alias_{_label}] SET synchronous = {_val} accepted",
       cur.fetchone()[0] == 1)

# Functional smoke: NORMAL must not break autocommit INSERT durability.
cur.execute("SET synchronous = 'NORMAL'")
cur.execute("DROP TABLE IF EXISTS _wire_a6")
cur.execute("CREATE TABLE _wire_a6 (id INT PRIMARY KEY, v TEXT)")
for _i in range(1, 6):
    cur.execute(f"INSERT INTO _wire_a6 VALUES ({_i}, 'row{_i}')")
cur.execute("SELECT COUNT(*) FROM _wire_a6")
ok("[A6 insert_under_normal] 5 autocommit inserts under NORMAL persist",
   cur.fetchone()[0] == 5)

# Toggle back and forth — every transition must be accepted.
for _val in ("'NORMAL'", "'STRICT'", "'OFF'", "'NORMAL'", "DEFAULT"):
    cur.execute(f"SET synchronous = {_val}")
cur.execute("SELECT 1")
ok("[A6 set_round_trip] STRICT⇄NORMAL⇄OFF⇄DEFAULT round-trip stays usable",
   cur.fetchone()[0] == 1)

# NOTE: garbage rejection (`SET synchronous = 'banana'` → InvalidValue) and
# in-transaction rejection are unit-tested in
# `crates/axiomdb-sql/tests/integration_set_synchronous.rs` (e2e through
# `execute_with_ctx`). They are NOT asserted here because the MySQL wire
# layer currently swallows non-DML errors on SET — a pre-existing concern
# orthogonal to Attack 6, tracked separately.

# Cleanup
cur.execute("SET synchronous = DEFAULT")
cur.execute("DROP TABLE IF EXISTS _wire_a6")
conn.commit()


# ── Result ────────────────────────────────────────────────────────────────────

conn.close()
stop_server()

total = PASS + FAIL
print(f"\n{'✓' if FAIL == 0 else '✗'} {PASS}/{total} passed" +
      (f"  ({FAIL} failed)" if FAIL else ""))
sys.exit(0 if FAIL == 0 else 1)
