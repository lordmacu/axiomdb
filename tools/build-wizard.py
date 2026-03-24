#!/usr/bin/env python3
"""
AxiomDB Build Wizard — interactive build profile selector.

Usage:
    python3 tools/build-wizard.py
    python3 tools/build-wizard.py --run      # build immediately after selection
"""
import argparse
import os
import subprocess
import sys
import time

# ── ANSI colors ────────────────────────────────────────────────────────────────

RESET  = "\033[0m"
BOLD   = "\033[1m"
DIM    = "\033[2m"
GREEN  = "\033[32m"
YELLOW = "\033[33m"
CYAN   = "\033[36m"
BLUE   = "\033[34m"
MAGENTA= "\033[35m"
RED    = "\033[31m"
WHITE  = "\033[97m"
BG_DARK= "\033[48;5;234m"

def c(text, *codes):
    return "".join(codes) + text + RESET

def clear():
    os.system("clear" if os.name != "nt" else "cls")

# ── Header ─────────────────────────────────────────────────────────────────────

LOGO = f"""
{c('  ██████╗ ██╗  ██╗██╗ ██████╗ ███╗   ███╗ ██████╗  █████╗ ', CYAN, BOLD)}
{c(' ██╔══██╗╚██╗██╔╝██║██╔═══██╗████╗ ████║██╔══██╗ ██╔══██╗', CYAN, BOLD)}
{c(' ███████║ ╚███╔╝ ██║██║   ██║██╔████╔██║██║  ██║ ███████║', CYAN, BOLD)}
{c(' ██╔══██║ ██╔██╗ ██║██║   ██║██║╚██╔╝██║██║  ██║ ██╔══██║', CYAN, BOLD)}
{c(' ██║  ██║██╔╝ ██╗██║╚██████╔╝██║ ╚═╝ ██║██████╔╝ ██║  ██║', CYAN, BOLD)}
{c(' ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝ ╚═════╝ ╚═╝     ╚═╝╚═════╝  ╚═╝  ╚═╝', CYAN, BOLD)}
"""

def header(subtitle=""):
    print(LOGO)
    print(c(f"  Build Wizard  v0.1.0", WHITE, BOLD), end="")
    if subtitle:
        print(c(f"  ·  {subtitle}", DIM))
    else:
        print()
    print(c("  " + "─" * 60, DIM))
    print()

# ── Menu helpers ───────────────────────────────────────────────────────────────

def menu(title, options, hint=""):
    """
    Show a numbered menu. Returns (index, key) of the chosen option.
    options: list of (key, label, description)
    """
    print(c(f"  {title}", BOLD, WHITE))
    if hint:
        print(c(f"  {hint}", DIM))
    print()
    for i, (key, label, desc) in enumerate(options, 1):
        num  = c(f"  {i}.", CYAN, BOLD)
        lbl  = c(f" {label}", WHITE, BOLD)
        dsc  = c(f"\n       {desc}", DIM) if desc else ""
        print(f"{num}{lbl}{dsc}")
    print()

    while True:
        try:
            raw = input(c("  Choose [1", CYAN) +
                        c(f"-{len(options)}", CYAN) +
                        c("]: ", CYAN)).strip()
            n = int(raw)
            if 1 <= n <= len(options):
                return n - 1, options[n - 1][0]
        except (ValueError, EOFError):
            pass
        print(c("  Please enter a number between 1 and " + str(len(options)), RED))

def checkbox(title, options, hint=""):
    """
    Multi-select checkbox. Returns list of selected keys.
    options: list of (key, label, description, default_checked)
    """
    checked = {i for i, (_, _, _, d) in enumerate(options) if d}

    print(c(f"  {title}", BOLD, WHITE))
    if hint:
        print(c(f"  {hint}", DIM))
    print()

    while True:
        for i, (key, label, desc, _) in enumerate(options):
            check = c("✓", GREEN, BOLD) if i in checked else c("·", DIM)
            num   = c(f"  {i+1}.", CYAN)
            lbl   = c(f" {label}", WHITE, BOLD if i in checked else "")
            dsc   = c(f"  {desc}", DIM) if desc else ""
            print(f"  [{check}] {num}{lbl}{dsc}")
        print()
        print(c("  Toggle [number], ", DIM) + c("done", GREEN) + c(" [d], ", DIM) +
              c("none", DIM) + c(" [0]: ", DIM))

        raw = input(c("  > ", CYAN)).strip().lower()

        if raw == "d" or raw == "":
            return [options[i][0] for i in checked]
        if raw == "0":
            checked = set()
            continue
        try:
            n = int(raw) - 1
            if 0 <= n < len(options):
                if n in checked:
                    checked.remove(n)
                else:
                    checked.add(n)
        except ValueError:
            pass

def confirm(msg, default=True):
    prompt = " [Y/n] " if default else " [y/N] "
    raw = input(c(f"  {msg}", WHITE) + c(prompt, CYAN)).strip().lower()
    if raw == "":
        return default
    return raw in ("y", "yes", "s", "si", "sí")

def badge(label, color):
    return color + BOLD + f" {label} " + RESET

def info_box(title, lines):
    width = max(len(l) for l in lines + [title]) + 6
    print(c("  ┌" + "─" * width + "┐", DIM))
    print(c("  │", DIM) + c(f"  {title}", BOLD, WHITE) + " " * (width - len(title) - 2) + c("│", DIM))
    print(c("  │" + " " * width + "│", DIM))
    for line in lines:
        padding = width - len(line) - 2
        print(c("  │", DIM) + f"  {line}" + " " * padding + c("│", DIM))
    print(c("  └" + "─" * width + "┘", DIM))
    print()

# ── Profile definitions ────────────────────────────────────────────────────────

PROFILES = {
    "web": {
        "name":    "Web / Cloud Server",
        "emoji":   "🌐",
        "cmd":     "cargo build -p axiomdb-server --release",
        "output":  "target/release/axiomdb-server",
        "size_est":"~1.7 MB",
        "includes": [
            ("✓", "MySQL wire protocol (port 3306)",  "Any MySQL client connects without custom driver"),
            ("✓", "SQL engine (parser + executor)",   "Full SELECT/INSERT/UPDATE/DELETE/JOIN"),
            ("✓", "Storage (mmap + WAL + recovery)",  "Crash-safe, ACID"),
            ("✓", "Secondary indexes + planner",      "B+ Tree, CREATE INDEX, index-based queries"),
            ("✓", "Prepared statements + plan cache", "COM_STMT_PREPARE/EXECUTE"),
            ("✓", "Session state + transactions",     "BEGIN/COMMIT/ROLLBACK, SET vars"),
        ],
        "optional_features": ["tls", "metrics", "replication"],
    },
    "desktop": {
        "name":    "Desktop Application",
        "emoji":   "🖥️",
        "cmd":     "cargo build -p axiomdb-embedded --release",
        "output":  "target/release/libaxiomdb_embedded.{dylib,so,dll}",
        "size_est":"~975 KB (.dylib) / ~22 MB (.a static)",
        "includes": [
            ("✓", "Rust API: Db::open / execute / query", "Native Rust integration"),
            ("✓", "C FFI: axiomdb_open / execute / close", "Use from C, Swift, Kotlin, Python"),
            ("✓", "Static library (.a)",                  "Embed in iOS, Unity, Electron"),
            ("✓", "Full SQL engine + storage + WAL",      "Same durability as server"),
            ("✓", "No wire protocol — no TCP overhead",   "Direct in-process calls"),
        ],
        "optional_features": ["full-text", "vectors"],
    },
    "mobile": {
        "name":    "Mobile App (iOS / Android)",
        "emoji":   "📱",
        "cmd":     "cargo build -p axiomdb-embedded --release --target aarch64-apple-ios",
        "output":  "target/aarch64-apple-ios/release/libaxiomdb_embedded.a",
        "size_est":"~800 KB (.a)",
        "includes": [
            ("✓", "Static library (.a) for iOS/Android", "Link into Xcode / Android NDK project"),
            ("✓", "C FFI API",                           "Call from Swift, Kotlin, React Native"),
            ("✓", "Full SQL + storage + WAL",            "Works offline, crash-safe"),
            ("✓", "No network dependency",               "100% local, no server needed"),
        ],
        "optional_features": ["vectors"],
        "note": "Requires target: rustup target add aarch64-apple-ios",
    },
    "rust-embedded": {
        "name":    "Embedded in Rust App",
        "emoji":   "⚙️",
        "cmd":     'cargo add axiomdb-embedded --path crates/axiomdb-embedded',
        "output":  "(library crate — no binary)",
        "size_est":"Adds ~800 KB to your binary",
        "includes": [
            ("✓", "Db::open / execute / query / run",   "Synchronous Rust API"),
            ("✓", "begin / commit / rollback",          "Explicit transaction control"),
            ("✓", "Full SQL + storage",                 "Same engine as server"),
        ],
        "optional_features": ["async-api", "full-text", "vectors"],
    },
    "async-rust": {
        "name":    "Async Rust App (Tokio / Axum)",
        "emoji":   "⚡",
        "cmd":     "cargo build -p axiomdb-embedded --features async-api --release",
        "output":  "(library crate — no binary)",
        "size_est":"Adds ~1.1 MB to your binary (+ tokio)",
        "includes": [
            ("✓", "AsyncDb::open / execute / query",  "All ops return Future"),
            ("✓", "tokio::spawn_blocking isolation",  "Never blocks the async executor"),
            ("✓", "Clone-able handle",                "Share across tasks with Arc<Mutex<>>"),
        ],
        "optional_features": ["full-text", "vectors"],
    },
    "wasm": {
        "name":    "WebAssembly (Browser)",
        "emoji":   "🌍",
        "cmd":     "cargo build -p axiomdb-embedded --target wasm32-unknown-unknown --features wasm --release",
        "output":  "target/wasm32-unknown-unknown/release/axiomdb_embedded.wasm",
        "size_est":"~400 KB (estimated, future)",
        "includes": [
            ("✓", "In-memory storage (no mmap in browser)", "Data lives in WASM memory"),
            ("✓", "Full SQL engine",                        "Same query support as server"),
            ("⚠", "No WAL / no fsync",                      "Not crash-safe in browser context"),
            ("⚠", "Future feature",                         "Not yet implemented — Phase 10+"),
        ],
        "optional_features": [],
    },
    "custom": {
        "name":    "Custom (choose features manually)",
        "emoji":   "🔧",
        "cmd":     None,
        "output":  "(determined by features)",
        "size_est": "(varies)",
        "includes": [],
        "optional_features": [],
    },
}

OPTIONAL_FEATURES = {
    "tls":         ("TLS/SSL",           "Encrypt connections (rustls) — adds ~500 KB",  "future"),
    "metrics":     ("Prometheus metrics","Expose /metrics endpoint — adds ~200 KB",       "future"),
    "replication": ("Replication",       "WAL streaming to replicas — adds ~300 KB",      "future"),
    "full-text":   ("Full-text search",  "Tokenizer + inverted index — adds ~400 KB",     "future"),
    "vectors":     ("Vector embeddings", "pgvector-style HNSW — adds ~600 KB",            "future"),
    "async-api":   ("Async Rust API",    "tokio::spawn_blocking wrapper — adds ~100 KB",  "available"),
}

CUSTOM_FEATURES = [
    ("wire-protocol", "Wire Protocol (MySQL :3306)",    "TCP server, COM_QUERY/STMT handling", True),
    ("c-ffi",         "C FFI exports",                  "axiomdb_open/execute/close",          False),
    ("async-api",     "Async Rust API",                 "tokio-based AsyncDb wrapper",         False),
    ("tls",           "TLS/SSL (future)",               "rustls — encrypted connections",      False),
    ("metrics",       "Prometheus metrics (future)",    "/metrics endpoint",                   False),
    ("replication",   "Replication (future)",           "WAL streaming to replicas",           False),
    ("full-text",     "Full-text search (future)",      "Tokenizer + inverted index",          False),
    ("vectors",       "Vector embeddings (future)",     "pgvector-style HNSW",                 False),
]

# ── Workflow ───────────────────────────────────────────────────────────────────

def step_target():
    """Step 1: choose target platform."""
    clear()
    header("Step 1 of 3 — Choose your target")

    options = [
        ("web",          "🌐 Web / Cloud Server",          "MySQL wire protocol, Docker, VPS, cloud"),
        ("desktop",      "🖥️  Desktop Application",         "Tauri, native GUI, offline app"),
        ("mobile",       "📱 Mobile App",                   "iOS (Swift), Android (Kotlin), React Native"),
        ("rust-embedded","⚙️  Embedded in Rust app",         "Add as cargo dependency, sync API"),
        ("async-rust",   "⚡ Async Rust (Axum / Tokio)",    "Async API, works with async Rust services"),
        ("wasm",         "🌍 WebAssembly (browser)",        "In-browser, in-memory only (future)"),
        ("custom",       "🔧 Custom",                       "Pick individual features manually"),
    ]

    idx, key = menu(
        "What are you building?",
        options,
        hint="Use AxiomDB as the database engine for your project."
    )
    return key

def step_extras(profile_key):
    """Step 2: choose optional features."""
    profile = PROFILES[profile_key]
    available = profile.get("optional_features", [])

    if profile_key == "custom":
        return step_custom_features()

    if not available:
        return []

    real_extras = [(k, v) for k, v in OPTIONAL_FEATURES.items()
                   if k in available and v[2] == "available"]
    future_extras = [(k, v) for k, v in OPTIONAL_FEATURES.items()
                     if k in available and v[2] == "future"]

    if not real_extras and not future_extras:
        return []

    clear()
    header("Step 2 of 3 — Optional extras")

    if real_extras:
        opts = [(k, v[0], v[1], False) for k, v in real_extras]
        selected = checkbox(
            "Available now — add to your build?",
            opts,
            hint="Space to toggle, d to confirm."
        )
    else:
        selected = []

    if future_extras:
        print()
        print(c("  Coming soon (not available yet):", DIM))
        for k, v in future_extras:
            print(c(f"  · {v[0]}", YELLOW) + c(f" — {v[1]}", DIM))
        print()

    return selected

def step_custom_features():
    """Custom feature selection."""
    clear()
    header("Step 2 of 3 — Choose features")

    opts = [(k, l, d, default) for k, l, d, default in CUSTOM_FEATURES]
    return checkbox(
        "Select features to include:",
        opts,
        hint="Toggle numbers, d to confirm."
    )

def step_summary(profile_key, extras):
    """Step 3: show summary and confirm."""
    profile = PROFILES[profile_key]

    clear()
    header("Step 3 of 3 — Build summary")

    # Profile badge
    status = badge("READY", GREEN) if profile_key != "wasm" else badge("FUTURE", YELLOW)
    print(f"  {profile['emoji']}  {c(profile['name'], BOLD, WHITE)}  {status}")
    print()

    # What's included
    print(c("  Included in this build:", BOLD))
    for icon, label, desc in profile["includes"]:
        ico = c(icon, GREEN if icon == "✓" else YELLOW)
        print(f"    {ico} {c(label, WHITE)}  {c(desc, DIM)}")

    if extras:
        print()
        print(c("  Selected extras:", BOLD))
        for e in extras:
            if e in OPTIONAL_FEATURES:
                v = OPTIONAL_FEATURES[e]
                tag = badge("available", GREEN) if v[2] == "available" else badge("future", YELLOW)
                print(f"    {c('+', CYAN, BOLD)} {c(v[0], WHITE)}  {c(v[1], DIM)}  {tag}")

    # Output
    print()
    print(c("  Output:", BOLD))
    print(f"    {c(profile['output'], CYAN)}")
    print(f"    {c('Estimated size:', DIM)} {c(profile['size_est'], WHITE)}")

    if "note" in profile:
        print()
        print(c(f"  ⚠  {profile['note']}", YELLOW))

    # Build command
    print()
    cmd = build_command(profile_key, extras)
    print(c("  Build command:", BOLD))
    print(f"\n    {c(cmd, CYAN, BOLD)}\n")

    return cmd

def build_command(profile_key, extras):
    """Generate the cargo build command."""
    profile = PROFILES[profile_key]

    if profile_key == "custom":
        features = " ".join(extras)
        if "wire-protocol" in extras:
            pkg = "axiomdb-server"
        else:
            pkg = "axiomdb-embedded"
        if features:
            return f"cargo build -p {pkg} --no-default-features --features '{features}' --release"
        else:
            return f"cargo build -p {pkg} --no-default-features --release"

    base = profile["cmd"]
    if extras:
        feat_str = " ".join(extras)
        base += f" --features '{feat_str}'"
    return base

def run_build(cmd):
    """Execute the build command with live output."""
    print(c("  Building…", DIM))
    print(c("  " + "─" * 60, DIM))
    print()

    start = time.time()
    result = subprocess.run(cmd, shell=True, cwd=find_workspace_root())
    elapsed = time.time() - start

    print()
    print(c("  " + "─" * 60, DIM))

    if result.returncode == 0:
        print(c(f"  ✓ Build succeeded in {elapsed:.1f}s", GREEN, BOLD))
    else:
        print(c(f"  ✗ Build failed (exit code {result.returncode})", RED, BOLD))

    return result.returncode == 0

def post_build_info(profile_key, cmd):
    """Show what to do after a successful build."""
    profile = PROFILES[profile_key]
    print()
    print(c("  Next steps:", BOLD))

    if profile_key == "web":
        print(f"  {c('1.', CYAN)} Start the server:")
        print(f"     {c('./target/release/axiomdb-server', CYAN)}")
        print(f"  {c('2.', CYAN)} Connect with any MySQL client:")
        print(f"     {c('mysql -h 127.0.0.1 -P 3306 -u root', CYAN)}")
        pymysql_ex = 'pymysql.connect(host="127.0.0.1", port=3306, user="root")'
        print(f"     {c(pymysql_ex, CYAN)}")

    elif profile_key in ("desktop", "mobile"):
        print(f"  {c('1.', CYAN)} Include the library in your project:")
        print(f"     {c('C/Swift/Kotlin:', DIM)} link against libaxiomdb_embedded.dylib/.so/.a")
        print(f"  {c('2.', CYAN)} Call the C API:")
        c_ex1 = 'AxiomDb* db = axiomdb_open("./myapp.db");'
        c_ex2 = 'axiomdb_execute(db, "CREATE TABLE ...");'
        c_ex3 = 'axiomdb_close(db);'
        print(f"     {c(c_ex1, CYAN)}")
        print(f"     {c(c_ex2, CYAN)}")
        print(f"     {c(c_ex3, CYAN)}")

    elif profile_key in ("rust-embedded", "async-rust"):
        print(f"  {c('1.', CYAN)} Add to your Cargo.toml:")
        print(f"     {c('[dependencies]', CYAN)}")
        dep_ex = 'axiomdb-embedded = { path = "path/to/axiomdb/crates/axiomdb-embedded" }'
        print(f"     {c(dep_ex, CYAN)}")
        print(f"  {c('2.', CYAN)} Use in code:")
        r_ex1 = 'let mut db = axiomdb_embedded::Db::open("./myapp.db")?;'
        r_ex2 = 'db.execute("CREATE TABLE users (id INT, name TEXT, PRIMARY KEY(id))")?;'
        r_ex3 = 'let rows = db.query("SELECT * FROM users")?;'
        print(f"     {c(r_ex1, CYAN)}")
        print(f"     {c(r_ex2, CYAN)}")
        print(f"     {c(r_ex3, CYAN)}")

    print()

def find_workspace_root():
    """Find the Cargo workspace root."""
    path = os.path.dirname(os.path.abspath(__file__))
    while path != "/":
        if os.path.exists(os.path.join(path, "Cargo.toml")):
            content = open(os.path.join(path, "Cargo.toml")).read()
            if "[workspace]" in content:
                return path
        path = os.path.dirname(path)
    return os.getcwd()

def show_all_profiles():
    """Show a quick reference of all profiles."""
    clear()
    header("All available build profiles")

    for key, profile in PROFILES.items():
        if key == "custom":
            continue
        status = c("ready", GREEN) if key != "wasm" else c("future", YELLOW)
        print(f"  {profile['emoji']}  {c(profile['name'], BOLD, WHITE)}")
        print(c(f"     {build_command(key, [])}", CYAN))
        print(c(f"     Output: {profile['output']}  ·  {profile['size_est']}  [{status}]", DIM))
        print()

# ── Main ───────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="AxiomDB Build Wizard — interactive build profile selector"
    )
    parser.add_argument("--run",      action="store_true", help="Execute the build after selection")
    parser.add_argument("--profiles", action="store_true", help="Show all available profiles and exit")
    args = parser.parse_args()

    if args.profiles:
        show_all_profiles()
        return

    # Step 1: choose target
    profile_key = step_target()

    # Step 2: optional extras
    extras = step_extras(profile_key)

    # Step 3: summary
    cmd = step_summary(profile_key, extras)

    # Run or print
    print()
    if args.run:
        should_run = True
    elif PROFILES[profile_key].get("output") == "(library crate — no binary)":
        should_run = confirm("Add to Cargo.toml and build?", default=False)
    elif profile_key == "wasm":
        print(c("  ⚠  WebAssembly support is not yet implemented.", YELLOW))
        print(c("     This profile will be available in Phase 10+.", DIM))
        print()
        should_run = False
    else:
        should_run = confirm("Run this build now?", default=True)

    if should_run:
        print()
        ok = run_build(cmd)
        if ok:
            post_build_info(profile_key, cmd)
    else:
        print()
        print(c("  Copy and run when ready:", DIM))
        print(f"  {c(cmd, CYAN, BOLD)}")
        print()


if __name__ == "__main__":
    main()
