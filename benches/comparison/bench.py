#!/usr/bin/env python3
"""
AxiomDB Comparison Benchmark Orchestrator

Runs the selected scenario(s) in parallel across MySQL, PostgreSQL, and AxiomDB.
Each engine runs its benchmark INSIDE its own Docker container (localhost, no network overhead).

Usage:
  python3 bench.py                        # interactive — asks what to bench
  python3 bench.py full_scan              # single scenario
  python3 bench.py insert_batch count_star  # multiple scenarios
  python3 bench.py --all                  # all scenarios
  python3 bench.py full_scan --rows 50000 # custom dataset size
  python3 bench.py --list                 # show available scenarios

Scenarios:
  insert_batch       INSERT N rows in 1 transaction
  insert_autocommit  INSERT 300 rows, 1 txn per row
  full_scan          SELECT * full table scan
  select_where       SELECT WHERE active=TRUE (~50% rows)
  point_lookup       100 individual PK lookups
  range_scan         SELECT WHERE id range 10%
  count_star         SELECT COUNT(*)
  group_by           GROUP BY age + COUNT(*) + AVG(score)
"""

import argparse, json, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor, as_completed

# ── Scenario registry ─────────────────────────────────────────────────────────

SCENARIOS = {
    "insert_batch":      "INSERT N rows in 1 transaction",
    "insert_autocommit": "INSERT 300 rows, 1 txn per row",
    "full_scan":         "SELECT * full table scan",
    "select_where":      "SELECT WHERE active=TRUE (~50%)",
    "point_lookup":      "100 PK lookups  ⚠️  AxiomDB: full scan until Phase 5",
    "range_scan":        "Range scan 10%  ⚠️  AxiomDB: full scan until Phase 5",
    "count_star":        "SELECT COUNT(*)  ⚠️  AxiomDB: full scan until Phase 5",
    "group_by":          "GROUP BY age + COUNT(*) + AVG(score)",
}

CONTAINERS = {
    "mysql":    "axiomdb_bench_mysql",
    "postgres": "axiomdb_bench_pg",
    "axiomdb":  "axiomdb_bench_axiomdb",
}

AXIOMDB_BINARY = "target/release/axiomdb_bench"

# ── Engine runners ────────────────────────────────────────────────────────────

def run_mysql(scenario, rows):
    result = subprocess.run(
        ["docker", "exec", CONTAINERS["mysql"],
         "python3", "/bench/bench.py", "--scenario", scenario, "--rows", str(rows)],
        capture_output=True, text=True, timeout=300,
    )
    if result.returncode != 0:
        return {"engine": "MySQL 8.0", "scenario": scenario, "error": result.stderr.strip()}
    return json.loads(result.stdout.strip())

def run_postgres(scenario, rows):
    result = subprocess.run(
        ["docker", "exec", CONTAINERS["postgres"],
         "python3", "/bench/bench.py", "--scenario", scenario, "--rows", str(rows)],
        capture_output=True, text=True, timeout=300,
    )
    if result.returncode != 0:
        return {"engine": "PostgreSQL 16", "scenario": scenario, "error": result.stderr.strip()}
    return json.loads(result.stdout.strip())

def run_axiomdb(scenario, rows):
    # Build binary on host
    build = subprocess.run(
        ["cargo", "build", "--release", "-p", "axiomdb-bench-comparison"],
        capture_output=True, text=True,
    )
    if build.returncode != 0:
        return {"engine": "AxiomDB", "scenario": scenario,
                "error": f"build failed: {build.stderr[-200:]}"}

    # Copy binary into container (fresh each time so code changes are picked up)
    subprocess.run(
        ["docker", "cp", AXIOMDB_BINARY, f"{CONTAINERS['axiomdb']}:/bench/axiomdb_bench"],
        capture_output=True, check=True,
    )

    result = subprocess.run(
        ["docker", "exec", CONTAINERS["axiomdb"],
         "/bench/axiomdb_bench", "--scenario", scenario, "--rows", str(rows)],
        capture_output=True, text=True, timeout=600,
    )
    if result.returncode != 0:
        return {"engine": "AxiomDB", "scenario": scenario, "error": result.stderr.strip()}
    return json.loads(result.stdout.strip())

# ── Output ────────────────────────────────────────────────────────────────────

def fmt_ops(n):
    s = str(int(n))
    groups = []
    while s:
        groups.append(s[-3:])
        s = s[:-3]
    return ",".join(reversed(groups))

def print_result(scenario, results):
    desc = SCENARIOS.get(scenario, scenario)
    print(f"\n  ┌─ {scenario} — {desc}")

    engines = ["MySQL 8.0", "PostgreSQL 16", "AxiomDB"]
    for engine in engines:
        r = next((x for x in results if x.get("engine") == engine), None)
        if r is None:
            print(f"  │  {engine:<16}  (no result)")
        elif "error" in r:
            print(f"  │  {engine:<16}  ERROR: {r['error'][:60]}")
        else:
            ms  = r["mean_ms"]
            ops = fmt_ops(r["ops_per_s"])
            note = f"  [{r['note']}]" if r.get("note") else ""
            print(f"  │  {engine:<16}  {ms:8.1f} ms   {ops:>12}/s{note}")

    # Winner
    valid = [r for r in results if "ops_per_s" in r and r["ops_per_s"] > 0]
    if len(valid) > 1:
        best = max(valid, key=lambda r: r["ops_per_s"])
        print(f"  └─ Fastest: {best['engine']} ({fmt_ops(best['ops_per_s'])}/s)")

# ── Scenario selection ────────────────────────────────────────────────────────

def ask_scenarios():
    print("\nAvailable scenarios:")
    names = list(SCENARIOS.keys())
    for i, name in enumerate(names, 1):
        print(f"  {i}. {name:<22} — {SCENARIOS[name]}")
    print(f"  {len(names)+1}. all\n")
    ans = input("Enter number(s) or name(s) [default: all]: ").strip()
    if not ans or ans == "all" or ans == str(len(names)+1):
        return names
    selected = []
    for token in ans.replace(",", " ").split():
        if token.isdigit():
            idx = int(token) - 1
            if 0 <= idx < len(names):
                selected.append(names[idx])
        elif token in SCENARIOS:
            selected.append(token)
    return selected or names

# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(description="AxiomDB comparison benchmark")
    p.add_argument("scenarios",   nargs="*", help="Scenario name(s)")
    p.add_argument("--all",       action="store_true", help="Run all scenarios")
    p.add_argument("--rows",      type=int, default=10_000, help="Row count (default 10000)")
    p.add_argument("--list",      action="store_true", help="List scenarios and exit")
    args = p.parse_args()

    if args.list:
        print("\nScenarios:")
        for name, desc in SCENARIOS.items():
            print(f"  {name:<22} {desc}")
        return

    if args.all:
        selected = list(SCENARIOS.keys())
    elif args.scenarios:
        invalid = [s for s in args.scenarios if s not in SCENARIOS]
        if invalid:
            print(f"Unknown scenario(s): {', '.join(invalid)}")
            print(f"Available: {', '.join(SCENARIOS)}")
            sys.exit(1)
        selected = args.scenarios
    else:
        selected = ask_scenarios()

    if not selected:
        print("No scenarios selected."); return

    print(f"\n  Engines:   MySQL 8.0 | PostgreSQL 16 | AxiomDB")
    print(f"  Rows:      {args.rows:,}")
    print(f"  Runs:      5 measured + 2 warmup")
    print(f"  Resources: 2 CPU / 2 GB RAM per container | fsync ON")
    print(f"  Scenarios: {', '.join(selected)}")

    # Build AxiomDB binary once upfront
    print("\n  Building AxiomDB binary...")
    build = subprocess.run(
        ["cargo", "build", "--release", "-p", "axiomdb-bench-comparison"],
        capture_output=True, text=True,
    )
    if build.returncode != 0:
        print(f"  Build failed:\n{build.stderr[-300:]}")
        sys.exit(1)

    # Copy binary to container once
    subprocess.run(
        ["docker", "cp", AXIOMDB_BINARY,
         f"{CONTAINERS['axiomdb']}:/bench/axiomdb_bench"],
        capture_output=True, check=True,
    )
    print("  Binary ready.\n")

    for scenario in selected:
        print(f"\n  Running: {scenario}...", flush=True)
        t0 = time.perf_counter()

        # Run all three in parallel
        with ThreadPoolExecutor(max_workers=3) as ex:
            futures = {
                ex.submit(run_mysql,    scenario, args.rows): "mysql",
                ex.submit(run_postgres, scenario, args.rows): "pg",
                ex.submit(run_axiomdb,  scenario, args.rows): "axiomdb",
            }
            results = []
            for future in as_completed(futures):
                try:
                    results.append(future.result())
                except Exception as e:
                    results.append({"engine": futures[future], "error": str(e)})

        elapsed = time.perf_counter() - t0
        print_result(scenario, results)
        print(f"  (wall time: {elapsed:.1f}s running all 3 in parallel)")

    print()

if __name__ == "__main__":
    main()
