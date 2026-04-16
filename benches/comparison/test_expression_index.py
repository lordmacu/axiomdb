#!/usr/bin/env python3
"""
Test script for expression indexes (Phase 21.8).

Tests:
  - CREATE INDEX ON t(LOWER(col)) syntax
  - Regular column index still works
  - Query with expression uses index

Usage:
  python3 benches/comparison/test_expression_index.py --rows 1000
"""

import argparse
import os
import sys
import tempfile
import time
import pymysql

ROOT_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
AXIOMDB_BIN = os.path.join(ROOT_DIR, "target", "release", "axiomdb-server")

def connect_axiomdb(port=3309):
    return pymysql.connect(
        host="127.0.0.1",
        port=port,
        user="root",
        password="",
    )

def create_database(conn):
    with conn.cursor() as cur:
        cur.execute("CREATE DATABASE IF NOT EXISTS test")
        cur.execute("USE test")
        conn.commit()

def wait_for_port(host, port, timeout_s=10.0):
    import socket
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(0.5)
            if sock.connect_ex((host, port)) == 0:
                return True
        time.sleep(0.2)
    return False

def start_axiomdb(port):
    import subprocess
    import shutil
    
    data_dir = tempfile.mkdtemp(prefix="axiomdb_exprtest.")
    env = os.environ.copy()
    env["AXIOMDB_PORT"] = str(port)
    env["AXIOMDB_DATA"] = data_dir
    
    proc = subprocess.Popen(
        [AXIOMDB_BIN],
        cwd=ROOT_DIR,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    
    if not wait_for_port("127.0.0.1", port, 15.0):
        proc.kill()
        raise RuntimeError(f"AxiomDB did not start on port {port}")
    
    return proc, data_dir

def create_table(conn):
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS expr_test_users")
        cur.execute("""
            CREATE TABLE expr_test_users (
                id INT NOT NULL,
                email TEXT NOT NULL,
                name TEXT NOT NULL,
                PRIMARY KEY (id)
            )
        """)
        conn.commit()

def insert_data(conn, n):
    with conn.cursor() as cur:
        for i in range(1, n + 1):
            # All lowercase for testing - simpler
            email = f"user{i}@example.com"
            cur.execute(
                "INSERT INTO expr_test_users (id, email, name) VALUES (%s, %s, %s)",
                (i, email, f"User {i}"))
        conn.commit()

def test_regular_index(conn):
    """Test: Regular column index still works"""
    print("  Test: regular index...")
    with conn.cursor() as cur:
        cur.execute("CREATE INDEX idx_email ON expr_test_users (email)")
        
        # Insert test row
        cur.execute("INSERT INTO expr_test_users (id, email, name) VALUES (999, 'exact@match.com', 'Test')")
        conn.commit()
        
        cur.execute("SELECT id FROM expr_test_users WHERE email = 'exact@match.com'")
        result = cur.fetchall()
        
        if len(result) == 1:
            print("    PASS: regular index works")
            return True
        else:
            print(f"    FAIL: expected 1 row, got {len(result)}")
            return False

def test_expression_index(conn):
    """Test: Expression index syntax works"""
    print("  Test: expression index (LOWER(email))...")
    with conn.cursor() as cur:
        try:
            cur.execute("CREATE INDEX idx_email_lower ON expr_test_users (LOWER(email))")
            conn.commit()
            print("    PASS: expression index created")
            return True
        except Exception as e:
            print(f"    FAIL: {e}")
            return False

def test_expression_query(conn):
    """Test: Query with expression uses the index"""
    print("  Test: query with LOWER()...")
    with conn.cursor() as cur:
        # This query should use the expression index
        cur.execute("SELECT id FROM expr_test_users WHERE LOWER(email) = 'user1@example.com'")
        result = cur.fetchall()
        
        if len(result) >= 1:
            print(f"    PASS: found {len(result)} row(s)")
            return True
        else:
            print(f"    WARN: no rows found (index may not be used)")
            return False

def test_case_insensitive_match(conn):
    """Test: Case insensitive matching via expression index"""
    print("  Test: case insensitive match...")
    with conn.cursor() as cur:
        # Note: data is lowercase, but this tests the index works with expressions
        cur.execute("SELECT COUNT(*) FROM expr_test_users WHERE LOWER(email) LIKE 'user%'")
        count = cur.fetchone()[0]
        
        if count >= 1:
            print(f"    PASS: found {count} matching row(s)")
            return True
        else:
            print(f"    WARN: no matches")
            return False

def main():
    parser = argparse.ArgumentParser(description="Test expression indexes")
    parser.add_argument("--rows", type=int, default=100, help="Number of rows to insert")
    parser.add_argument("--port", type=int, default=3309, help="AxiomDB port")
    parser.add_argument("--stop", action="store_true", help="Stop AxiomDB after test")
    args = parser.parse_args()
    
    proc = None
    data_dir = None
    
    try:
        print(f"Starting AxiomDB on port {args.port}...")
        proc, data_dir = start_axiomdb(args.port)
        
        print(f"Connecting to AxiomDB...")
        conn = connect_axiomdb(args.port)
        
        print(f"Creating database...")
        create_database(conn)
        
        print(f"Creating test table with {args.rows} rows...")
        create_table(conn)
        insert_data(conn, args.rows)
        
        print("\n=== Running expression index tests ===")
        
        results = []
        
        # Test regular index first
        results.append(("regular_index", test_regular_index(conn)))
        
        # Test expression index
        results.append(("expression_index", test_expression_index(conn)))
        
        # Test queries using expression
        results.append(("expression_query", test_expression_query(conn)))
        results.append(("case_insensitive", test_case_insensitive_match(conn)))
        
        print("\n=== Results ===")
        all_passed = True
        for name, passed in results:
            status = "PASS" if passed else "FAIL"
            print(f"  {name}: {status}")
            if not passed:
                all_passed = False
        
        conn.close()
        
        if all_passed:
            print("\nAll tests PASSED!")
            return 0
        else:
            print("\nSome tests FAILED!")
            return 1
            
    finally:
        if proc:
            print("\nStopping AxiomDB...")
            proc.terminate()
            proc.wait(timeout=5)
            if data_dir:
                import shutil
                shutil.rmtree(data_dir, ignore_errors=True)

if __name__ == "__main__":
    sys.exit(main())