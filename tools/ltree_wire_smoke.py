#!/usr/bin/env python3
"""Minimal wire smoke test for [20.19 ltree] — run in isolation from the main wire-test.py."""
import os
import socket
import subprocess
import sys
import tempfile
import time

import pymysql

PORT = 13307  # use a different port to avoid conflicts with the main wire test
TARGET_DIR = "/home/cristian.guest/axiomdb-target"
BINARY = f"{TARGET_DIR}/release/axiomdb-server"

PASS = 0
FAIL = 0


def ok(label, cond, got=None):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  ✓ {label}")
    else:
        FAIL += 1
        suffix = f" (got: {got!r})" if got is not None else ""
        print(f"  ✗ {label}{suffix}")


# ── Start a fresh server ──────────────────────────────────────────────────────

data_dir = tempfile.mkdtemp(prefix="axiomdb-ltree-smoke-")
env = os.environ.copy()
env["AXIOMDB_DATA"] = data_dir
env["AXIOMDB_PORT"] = str(PORT)

proc = subprocess.Popen(
    [BINARY], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
)

import atexit
import shutil


def cleanup():
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    shutil.rmtree(data_dir, ignore_errors=True)


atexit.register(cleanup)

for _ in range(50):
    try:
        with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
            break
    except OSError:
        time.sleep(0.1)
else:
    print("Server did not start within 5s")
    sys.exit(1)

print(f"Server ready on :{PORT}\n")

conn = pymysql.connect(
    host="127.0.0.1", port=PORT, user="root", password="", db="axiomdb"
)
cur = conn.cursor()

# ── [20.19 ltree] ─────────────────────────────────────────────────────────────

print("[20.19 ltree]")

cur.execute("CREATE TABLE _t_ltree (id INT, path LTREE)")
cur.execute("INSERT INTO _t_ltree VALUES (1, 'org.eng.backend'), (2, 'org.hr')")
cur.execute("SELECT path FROM _t_ltree WHERE id = 1")
v = cur.fetchone()[0]
ok("INSERT + SELECT LTREE column roundtrip", v == "org.eng.backend", v)

cur.execute("SELECT 'org.eng'::LTREE @> 'org.eng.backend'::LTREE")
ok("@> ancestor returns true", cur.fetchone()[0] == 1)

cur.execute("SELECT 'org.eng.backend'::LTREE ~ 'org.*.backend'")
ok("~ lquery wildcard matches correctly", cur.fetchone()[0] == 1)

cur.execute("SELECT nlevel('a.b.c'::LTREE)")
ok("nlevel('a.b.c') = 3", cur.fetchone()[0] == 3)

cur.execute("SELECT subpath('a.b.c.d'::LTREE, 1, 2)")
ok("subpath('a.b.c.d', 1, 2) = 'b.c'", cur.fetchone()[0] == "b.c")

cur.execute("SELECT lca('org.eng.backend'::LTREE, 'org.eng.frontend'::LTREE)")
ok("lca returns common ancestor 'org.eng'", cur.fetchone()[0] == "org.eng")

cur.execute("SELECT 'a.b'::LTREE || 'c.d'::LTREE")
ok("|| concatenates two ltree paths", cur.fetchone()[0] == "a.b.c.d")

cur.execute("SELECT index('a.b.c.a.b'::LTREE, 'a.b'::LTREE)")
ok("index('a.b.c.a.b', 'a.b') = 0", cur.fetchone()[0] == 0)

cur.execute("DROP TABLE IF EXISTS _t_ltree")
conn.close()

# ── Result ────────────────────────────────────────────────────────────────────

total = PASS + FAIL
sym = "✓" if FAIL == 0 else "✗"
suffix = f"  ({FAIL} failed)" if FAIL else ""
print(f"\n{sym} {PASS}/{total} passed{suffix}")
sys.exit(0 if FAIL == 0 else 1)
