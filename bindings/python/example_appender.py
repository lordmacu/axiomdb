"""
Example + smoke test: AxiomDB Python binding — Appender fast-path.

Compares plain SQL INSERT loop vs Appender for bulk-loading 5000 rows.
"""

import os
import tempfile
import time

from axiomdb import AxiomDB, AxiomDBError


def main():
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = os.path.join(tmpdir, "demo.db")
        print(f"Opening {db_path}")

        with AxiomDB(db_path) as db:
            # Same schema for both runs.
            db.execute(
                """CREATE TABLE users (
                       id INT NOT NULL,
                       name TEXT NOT NULL,
                       age INT NOT NULL,
                       active BOOL NOT NULL,
                       score REAL NOT NULL,
                       email TEXT NOT NULL,
                       PRIMARY KEY (id)
                   )"""
            )

            N = 5000
            # ── 1. SQL INSERT loop (autocommit) ─────────────────────────
            t0 = time.perf_counter()
            for i in range(1, N + 1):
                active = "TRUE" if i % 2 == 0 else "FALSE"
                age = 18 + (i % 62)
                score = 100.0 + (i % 1000) * 0.1
                db.execute(
                    f"INSERT INTO users VALUES "
                    f"({i}, 'user_{i:06}', {age}, {active}, {score:.1f}, 'u{i}@b.local')"
                )
            t_sql = time.perf_counter() - t0
            print(
                f"\n[SQL INSERT autocommit]  {N} rows in {t_sql*1000:6.1f} ms "
                f"=> {N/t_sql:7.0f} rows/s"
            )

            # ── 2. Appender fast-path ───────────────────────────────────
            db.execute("DROP TABLE users")
            db.execute(
                """CREATE TABLE users (
                       id INT NOT NULL,
                       name TEXT NOT NULL,
                       age INT NOT NULL,
                       active BOOL NOT NULL,
                       score REAL NOT NULL,
                       email TEXT NOT NULL,
                       PRIMARY KEY (id)
                   )"""
            )

            t0 = time.perf_counter()
            with db.appender("users") as app:
                for i in range(1, N + 1):
                    app.append_row(
                        i,
                        f"user_{i:06}",
                        18 + (i % 62),
                        i % 2 == 0,
                        100.0 + (i % 1000) * 0.1,
                        f"u{i}@b.local",
                    )
            t_app = time.perf_counter() - t0
            print(
                f"[Appender (typed)]       {N} rows in {t_app*1000:6.1f} ms "
                f"=> {N/t_app:7.0f} rows/s"
            )

            # ── Verify both runs landed the same data ───────────────────
            rows = db.query("SELECT COUNT(*) FROM users")
            count = next(iter(rows[0].values()))
            assert count == N, f"expected {N}, got {count}"
            print(f"\nVerified {N} rows persisted via Appender.")

            print(f"\nSpeedup: {t_sql / t_app:.1f}×")

            # ── Error handling smoke ─────────────────────────────────────
            try:
                with db.appender("missing_table") as _app:
                    pass
            except AxiomDBError as e:
                print(f"\n[error path] appender on missing table → {e}")

            # ── Rollback via context-manager exception ───────────────────
            db.execute("DROP TABLE users")
            db.execute("CREATE TABLE r (id INT)")
            try:
                with db.appender("r") as app:
                    app.append_int(1)
                    app.end_row()
                    raise RuntimeError("forced rollback")
            except RuntimeError:
                pass
            count_rows = db.query("SELECT COUNT(*) FROM r")
            count = next(iter(count_rows[0].values()))
            assert count == 0, f"rollback should be empty, got {count}"
            print("[rollback] forced exception inside `with` rolled back ✓")


if __name__ == "__main__":
    main()
