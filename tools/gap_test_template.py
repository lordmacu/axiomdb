#!/usr/bin/env python3
"""Temporary gap verification — GAP-X.Y: [description]"""
import os, signal, subprocess, sys, tempfile, time, socket
import pymysql

PORT = 13307
_server_proc = None
_data_dir = None

def start_server():
    global _server_proc, _data_dir
    debug = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(debug) and os.path.isfile(release):
        binary = debug if os.path.getmtime(debug) > os.path.getmtime(release) else release
    elif os.path.isfile(release):
        binary = release
    elif os.path.isfile(debug):
        binary = debug
    else:
        print("Server binary not found — build first"); sys.exit(1)
    _data_dir = tempfile.mkdtemp(prefix="axiomdb-gap-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = _data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen([binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1): return
        except OSError:
            time.sleep(0.1)
    stop_server()
    print(f"Server did not start on :{PORT} within 5s"); sys.exit(1)

def stop_server():
    global _server_proc, _data_dir
    if _server_proc:
        _server_proc.terminate()
        try: _server_proc.wait(timeout=3)
        except subprocess.TimeoutExpired: _server_proc.kill()
        _server_proc = None
    if _data_dir and os.path.isdir(_data_dir):
        import shutil; shutil.rmtree(_data_dir, ignore_errors=True); _data_dir = None

PASS = 0; FAIL = 0

def ok(label, cond, got=None):
    global PASS, FAIL
    if cond: print(f"  PASS {label}"); PASS += 1
    else: print(f"  FAIL {label}" + (f" (got: {got!r})" if got is not None else "")); FAIL += 1

def connect():
    return pymysql.connect(host="127.0.0.1", port=PORT, user="root",
        password="", autocommit=False)

def test_gap():
    """GAP-X.Y: [description]"""
    print("\n-- GAP-X.Y: [title] --")
    c = connect(); cur = c.cursor()
    cur.execute("CREATE DATABASE IF NOT EXISTS gap_test")
    cur.execute("USE gap_test")

    # Control (confirms infra works)
    # ok("control: SELECT works", ...)

    # Gap test
    try:
        pass
    except Exception as e:
        ok("gap description", False, got=str(e))

    # Cleanup
    for tbl in ["table1"]:
        try: cur.execute(f"DROP TABLE IF EXISTS {tbl}")
        except: pass
    c.commit(); c.close()
    c2 = connect()
    try: c2.cursor().execute("DROP DATABASE gap_test"); c2.commit()
    except: pass
    c2.close()

if __name__ == "__main__":
    start_server()
    try: test_gap()
    finally: stop_server()
    print(f"\n{'='*60}\n  Result: {PASS} passed, {FAIL} failed\n{'='*60}")
    sys.exit(1 if FAIL else 0)
