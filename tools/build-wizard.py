#!/usr/bin/env python3
"""
AxiomDB Build Wizard v2.1 — interactive build profile selector.

No external dependencies — pure Python 3.6+ stdlib.

Usage:
    python3 tools/build-wizard.py              # interactive wizard
    python3 tools/build-wizard.py --run        # wizard + auto build
    python3 tools/build-wizard.py --run --test --strip --timings
    python3 tools/build-wizard.py --check      # fast cargo check --workspace
    python3 tools/build-wizard.py --profiles   # quick reference
    python3 tools/build-wizard.py --last       # repeat last build
    python3 tools/build-wizard.py --list       # list saved profiles
    python3 tools/build-wizard.py --save web   # save selection as "web"
    python3 tools/build-wizard.py --load web   # load and run saved profile
    python3 tools/build-wizard.py --delete web # delete saved profile
    python3 tools/build-wizard.py --ci --profile web  # CI/CD non-interactive
"""
import argparse, json, os, subprocess, sys, time, shutil
from pathlib import Path

# ── ANSI ──────────────────────────────────────────────────────────────────────

RESET   = "\033[0m"
BOLD    = "\033[1m"
DIM     = "\033[2m"
GREEN   = "\033[32m"
YELLOW  = "\033[33m"
CYAN    = "\033[36m"
BLUE    = "\033[34m"
MAGENTA = "\033[35m"
RED     = "\033[31m"
WHITE   = "\033[97m"
BG_SEL  = "\033[48;5;236m"  # dark grey background for selected item

NO_COLOR = os.environ.get("NO_COLOR") or not sys.stdout.isatty()

def c(text, *codes):
    if NO_COLOR: return text
    return "".join(codes) + str(text) + RESET

def clear():
    if not NO_COLOR:
        os.system("clear" if os.name != "nt" else "cls")

def fmt_size(path):
    """Return human-readable file size, or None if not found."""
    try:
        s = Path(path).stat().st_size
        for unit in ("B", "KB", "MB", "GB"):
            if s < 1024: return f"{s:.0f} {unit}"
            s /= 1024
        return f"{s:.1f} GB"
    except FileNotFoundError:
        return None

# ── Raw terminal input (arrow keys) ───────────────────────────────────────────

def _getch():
    """Read one keypress. Returns a string: character, 'UP', 'DOWN', 'SPACE'."""
    if os.name == "nt":  # Windows fallback
        import msvcrt
        ch = msvcrt.getwch()
        if ch in ("\xe0", "\x00"):
            ch2 = msvcrt.getwch()
            return "UP" if ch2 == "H" else "DOWN" if ch2 == "P" else ""
        return ch
    # Unix (macOS / Linux)
    import tty, termios
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        ch = sys.stdin.read(1)
        if ch == "\x1b":
            ch2 = sys.stdin.read(1)
            ch3 = sys.stdin.read(1)
            if ch2 == "[":
                if ch3 == "A": return "UP"
                if ch3 == "B": return "DOWN"
        return ch
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)

# ── Logo ──────────────────────────────────────────────────────────────────────

_L1 = "    ___   _  __ ________  __  ___   ____  ____ "
_L2 = "   /   | | |/ //  _/ __ \\/  |/  /  / __ \\/ __ )"
_L3 = "  / /| | |   / / // / / / /|_/ /  / / / / __  |"
_L4 = " / ___ |/   |_/ // /_/ / /  / /  / /_/ / /_/ / "
_L5 = "/_/  |_/_/|_/___/\\____/_/  /_/  /_____/_____/  "

def header(subtitle=""):
    print()
    for line in [_L1, _L2, _L3, _L4, _L5]:
        print(c(line, CYAN, BOLD))
    print()
    print(c("  Build Wizard  v2.1", WHITE, BOLD), end="")
    print(c(f"  ·  {subtitle}", DIM) if subtitle else "")
    print(c("  " + "─" * 60, DIM))
    print()

# ── Arrow-key menu ─────────────────────────────────────────────────────────────

def arrow_menu(title, options, hint=""):
    """
    Single-select menu with ↑↓ arrow keys.
    options: list of (key, label, description)
    Returns (index, key).
    """
    if NO_COLOR:
        # Fallback: numbered list
        print(c(f"  {title}", BOLD, WHITE))
        for i, (_, label, desc) in enumerate(options, 1):
            print(f"  {c(str(i)+'.', CYAN)} {label}  {c(desc, DIM)}")
        while True:
            try:
                n = int(input(c(f"  Choice [1-{len(options)}]: ", CYAN))) - 1
                if 0 <= n < len(options): return n, options[n][0]
            except (ValueError, EOFError): pass
        return

    idx   = 0
    drawn = False   # local — never persists between calls
    # line count: 1 title + 1 hint/blank + N options + 1 trailing blank = N+3
    lines = len(options) + 3

    while True:
        if drawn:
            # Move cursor up AND clear everything below — handles any residual text
            print(f"\033[{lines}A\033[J", end="", flush=True)

        print(c(f"  {title}", BOLD, WHITE))
        if hint: print(c(f"  {hint}", DIM))
        else:    print()
        for i, (key, label, desc) in enumerate(options):
            if i == idx:
                cursor = c("❯", CYAN, BOLD)
                row    = BG_SEL + BOLD + f"  {cursor} {label:<40}" + RESET
                dsc    = c(f"  {desc}", CYAN, DIM)
            else:
                row    = f"    {c(label, WHITE):<40}"
                dsc    = c(f"  {desc}", DIM)
            print(f"{row}{dsc}")
        print()
        sys.stdout.flush()
        drawn = True

        key = _getch()
        if key == "UP":             idx = (idx - 1) % len(options)
        elif key == "DOWN":         idx = (idx + 1) % len(options)
        elif key in ("\r", "\n"):   return idx, options[idx][0]
        elif key == "\x03":         sys.exit(0)


def arrow_checkbox(title, options, hint=""):
    """
    Multi-select with ↑↓ to move, Space to toggle, Enter to confirm.
    options: list of (key, label, description, default_checked)
    Returns list of selected keys.
    """
    checked = {i for i, (_, _, _, d) in enumerate(options) if d}
    idx = 0

    if NO_COLOR:
        for i, (k, l, d, _) in enumerate(options, 1):
            mark = "[x]" if (i-1) in checked else "[ ]"
            print(f"  {mark} {i}. {l}  {c(d, DIM)}")
        raw = input(c("  Toggle numbers (space-separated), Enter to confirm: ", CYAN))
        for n in raw.split():
            try:
                i = int(n) - 1
                if 0 <= i < len(options):
                    checked.discard(i) if i in checked else checked.add(i)
            except ValueError: pass
        return [options[i][0] for i in sorted(checked)]

    while True:
        print(f"\033[{len(options)+5}A", end="") if hasattr(arrow_checkbox, "_drawn") else None
        arrow_checkbox._drawn = True

        print(c(f"  {title}", BOLD, WHITE))
        print(c(f"  {hint or 'Space to toggle · Enter to confirm · 0 to clear all'}", DIM))
        print()
        for i, (key, label, desc, _) in enumerate(options):
            box   = c("✓", GREEN, BOLD) if i in checked else c("·", DIM)
            arrow = c("❯ ", CYAN, BOLD) if i == idx else "  "
            lbl   = c(label, WHITE, BOLD) if i in checked else c(label, WHITE)
            dsc   = c(f"  {desc}", GREEN, DIM) if i in checked else c(f"  {desc}", DIM)
            print(f"  {arrow}[{box}] {lbl}{dsc}")
        print()
        print(c("  ↑↓ move  ·  space toggle  ·  0 clear  ·  enter confirm", DIM))

        key = _getch()
        if key == "UP":     idx = (idx - 1) % len(options)
        elif key == "DOWN":  idx = (idx + 1) % len(options)
        elif key == " ":
            checked.discard(idx) if idx in checked else checked.add(idx)
        elif key == "0":     checked.clear()
        elif key in ("\r", "\n"):
            arrow_checkbox._drawn = False
            return [options[i][0] for i in sorted(checked)]
        elif key == "\x03": sys.exit(0)


def confirm(msg, default=True):
    if NO_COLOR:
        r = input(f"  {msg} {'[Y/n]' if default else '[y/N]'}: ").strip().lower()
        return default if r == "" else r in ("y", "yes", "s", "si")
    prompt = c(f"  {msg} ", WHITE) + c("[Y/n] " if default else "[y/N] ", CYAN)
    r = input(prompt).strip().lower()
    return default if r == "" else r in ("y", "yes", "s", "si")

# ── Environment detection ─────────────────────────────────────────────────────

def detect_env(workspace):
    """Return dict with environment info."""
    env = {}

    # Rust/Cargo version
    try:
        out = subprocess.check_output(["rustc", "--version"], stderr=subprocess.DEVNULL, text=True).strip()
        env["rust"] = out.split()[1]
        env["rust_ok"] = True
    except Exception:
        env["rust"] = None
        env["rust_ok"] = False

    # Installed cross-compilation targets
    try:
        out = subprocess.check_output(["rustup", "target", "list", "--installed"],
                                       stderr=subprocess.DEVNULL, text=True)
        env["targets"] = set(out.splitlines())
    except Exception:
        env["targets"] = set()

    # Cross-compilation tools
    env["cross"]    = shutil.which("cross")          is not None
    env["zigbuild"] = shutil.which("cargo-zigbuild") is not None

    # Existing builds
    builds = {}
    for name, path in [
        ("server",   workspace / "target/release/axiomdb-server"),
        ("embedded", workspace / "target/release/libaxiomdb_embedded.dylib"),
        ("linux",    workspace / "target/x86_64-unknown-linux-musl/release/axiomdb-server"),
    ]:
        s = fmt_size(path)
        if s:
            mtime = time.strftime("%Y-%m-%d %H:%M",
                                   time.localtime(path.stat().st_mtime))
            builds[name] = {"size": s, "date": mtime, "path": str(path)}
    env["builds"] = builds

    return env


def print_env_status(env):
    """Print a compact environment status block."""
    rust_icon = c("✓", GREEN) if env["rust_ok"] else c("✗", RED)
    rust_ver  = c(env["rust"], WHITE) if env["rust_ok"] else c("not found — install rustup.rs", RED)
    print(f"  {rust_icon} Rust {rust_ver}")

    cross_parts = []
    if env.get("zigbuild"): cross_parts.append(c("cargo-zigbuild", GREEN))
    if env.get("cross"):    cross_parts.append(c("cross", GREEN))
    if not cross_parts:     cross_parts.append(c("none — Linux cross-compile unavailable", DIM))
    print(f"  {c('~', DIM)} Cross tools: {', '.join(cross_parts)}")

    if env["builds"]:
        for name, info in env["builds"].items():
            print(f"  {c('~', DIM)} Previous build: {c(name, CYAN)}  "
                  f"{c(info['size'], WHITE)}  {c(info['date'], DIM)}")
    print()

    if not env["rust_ok"]:
        print(c("  Rust is required. Install it from https://rustup.rs", RED))
        sys.exit(1)

# ── Profiles ──────────────────────────────────────────────────────────────────

PROFILES = {
    "web": {
        "name": "Web / Cloud Server",
        "emoji": "🌐",
        "desc": "MySQL wire protocol, Docker, VPS, cloud",
        "cmd_base": "cargo build -p axiomdb-server --release",
        "output": "target/release/axiomdb-server",
        "size_est": "~1.7 MB",
        "includes": [
            ("✓", "MySQL wire protocol (:3306)", "Any MySQL client connects without custom driver"),
            ("✓", "Full SQL engine",             "SELECT/INSERT/UPDATE/DELETE/JOIN"),
            ("✓", "Storage + WAL + recovery",    "Crash-safe, ACID"),
            ("✓", "Secondary indexes + planner", "B+ Tree, CREATE INDEX"),
            ("✓", "Prepared statements",         "COM_STMT_PREPARE/EXECUTE + plan cache"),
            ("✓", "Transactions",                "BEGIN/COMMIT/ROLLBACK"),
        ],
        "optional_features": ["tls", "metrics", "replication"],
        "docker": True,
        "ci": True,
    },
    "desktop": {
        "name": "Desktop Application",
        "emoji": "🖥️",
        "desc": "Tauri, native GUI, offline app",
        "cmd_base": "cargo build -p axiomdb-embedded --release",
        "output": "target/release/libaxiomdb_embedded.{dylib,so,dll}",
        "size_est": "~975 KB (.dylib) / ~22 MB (.a static)",
        "includes": [
            ("✓", "Rust API: Db::open/execute/query", "Native Rust integration"),
            ("✓", "C FFI: axiomdb_open/execute/close", "Swift, Kotlin, Python, C"),
            ("✓", "Static library (.a)",               "iOS, Unity, Electron"),
            ("✓", "Full SQL + storage + WAL",          "Same durability as server"),
            ("✓", "No TCP overhead",                   "Direct in-process calls"),
        ],
        "optional_features": ["full-text", "vectors"],
        "docker": False,
        "ci": True,
    },
    "mobile": {
        "name": "Mobile App (iOS / Android)",
        "emoji": "📱",
        "desc": "iOS (Swift), Android (Kotlin), React Native",
        "cmd_base": "cargo build -p axiomdb-embedded --release --target aarch64-apple-ios",
        "output": "target/aarch64-apple-ios/release/libaxiomdb_embedded.a",
        "size_est": "~800 KB (.a)",
        "includes": [
            ("✓", "Static library (.a)",          "Link in Xcode / Android NDK"),
            ("✓", "C FFI",                        "Swift, Kotlin, React Native"),
            ("✓", "Full SQL + storage + WAL",     "Works offline, crash-safe"),
            ("✓", "No network dependency",        "100% local"),
        ],
        "optional_features": ["vectors"],
        "required_target": "aarch64-apple-ios",
        "docker": False,
        "ci": True,
    },
    "linux-musl": {
        "name": "Linux x86-64 (cross from macOS)",
        "emoji": "🐧",
        "desc": "Static musl binary — VPS, Docker, CI",
        "cmd_base": None,   # resolved dynamically from env
        "output": "target/x86_64-unknown-linux-musl/release/axiomdb-server",
        "size_est": "~2.1 MB (static musl)",
        "includes": [
            ("✓", "Static musl binary",           "Zero libc dependency — runs on any Linux"),
            ("✓", "MySQL wire protocol (:3306)",  "Identical to web profile"),
            ("✓", "Full SQL + WAL + recovery",    "Crash-safe, ACID"),
            ("✓", "Docker FROM scratch",          "Copy single binary, no base image needed"),
        ],
        "optional_features": ["tls", "metrics"],
        "required_target": "x86_64-unknown-linux-musl",
        "cross_build": True,
        "docker": True,
        "ci": True,
    },
    "rust-embedded": {
        "name": "Embedded in Rust App",
        "emoji": "⚙️",
        "desc": "Add as cargo dependency, sync API",
        "cmd_base": 'cargo add axiomdb-embedded --path crates/axiomdb-embedded',
        "output": "(library — links into your binary)",
        "size_est": "Adds ~800 KB",
        "includes": [
            ("✓", "Db::open/execute/query/run", "Synchronous Rust API"),
            ("✓", "begin/commit/rollback",      "Explicit transactions"),
            ("✓", "Full SQL + storage",         "Same engine as server"),
        ],
        "optional_features": ["async-api", "full-text", "vectors"],
        "docker": False,
        "ci": False,
    },
    "async-rust": {
        "name": "Async Rust App (Axum / Tokio)",
        "emoji": "⚡",
        "desc": "Async API, works with async Rust services",
        "cmd_base": "cargo build -p axiomdb-embedded --features async-api --release",
        "output": "(library — links into your binary)",
        "size_est": "Adds ~1.1 MB",
        "includes": [
            ("✓", "AsyncDb::open/execute/query", "All ops return Future"),
            ("✓", "tokio::spawn_blocking",       "Never blocks the async executor"),
            ("✓", "Clone-able handle",           "Share across tasks with Arc"),
        ],
        "optional_features": ["full-text", "vectors"],
        "docker": False,
        "ci": False,
    },
    "wasm": {
        "name": "WebAssembly (Browser)",
        "emoji": "🌍",
        "desc": "In-browser, in-memory only — future",
        "cmd_base": "cargo build -p axiomdb-embedded --target wasm32-unknown-unknown --features wasm --release",
        "output": "target/wasm32-unknown-unknown/release/axiomdb_embedded.wasm",
        "size_est": "~400 KB (estimated)",
        "includes": [
            ("✓", "In-memory storage",       "No mmap in browser context"),
            ("✓", "Full SQL engine",         "Same query support as server"),
            ("⚠", "No WAL / no fsync",       "Not crash-safe"),
            ("⚠", "Future — Phase 10+",      "Not yet implemented"),
        ],
        "optional_features": [],
        "future": True,
        "docker": False,
        "ci": False,
    },
    "custom": {
        "name": "Custom (choose features manually)",
        "emoji": "🔧",
        "desc": "Pick individual features manually",
        "cmd_base": None,
        "output": "(depends on features)",
        "size_est": "(varies)",
        "includes": [],
        "optional_features": [],
        "docker": False,
        "ci": False,
    },
}

OPTIONAL_FEATURES = {
    "tls":        ("TLS/SSL",            "Encrypt connections (rustls) +~500 KB",   "future"),
    "metrics":    ("Prometheus metrics", "Expose /metrics endpoint +~200 KB",       "future"),
    "replication":("Replication",        "WAL streaming to replicas +~300 KB",      "future"),
    "full-text":  ("Full-text search",   "Tokenizer + inverted index +~400 KB",     "future"),
    "vectors":    ("Vector embeddings",  "pgvector-style HNSW +~600 KB",            "future"),
    "async-api":  ("Async Rust API",     "tokio::spawn_blocking wrapper +~100 KB",  "available"),
}

CUSTOM_FEATURES = [
    ("wire-protocol", "Wire Protocol (MySQL :3306)",   "TCP server, COM_QUERY/STMT",    True),
    ("c-ffi",         "C FFI exports",                 "axiomdb_open/execute/close",    False),
    ("async-api",     "Async Rust API",                "tokio AsyncDb wrapper",         False),
    ("tls",           "TLS/SSL (future)",              "rustls encrypted connections",  False),
    ("metrics",       "Prometheus metrics (future)",   "/metrics endpoint",             False),
    ("replication",   "Replication (future)",          "WAL streaming",                 False),
    ("full-text",     "Full-text search (future)",     "Tokenizer + inverted index",    False),
    ("vectors",       "Vector embeddings (future)",    "pgvector-style HNSW",           False),
]

# ── Config persistence ─────────────────────────────────────────────────────────

def config_path(workspace):
    return workspace / ".axiomdb-build.json"

def save_config(workspace, name, profile_key, extras, elapsed=None):
    path = config_path(workspace)
    try:
        data = json.loads(path.read_text()) if path.exists() else {}
    except Exception:
        data = {}
    entry = {"profile": profile_key, "extras": extras}
    if elapsed is not None:
        entry["last_elapsed"] = round(elapsed, 1)
    data[name] = entry
    data["__last__"] = entry
    path.write_text(json.dumps(data, indent=2))
    if not name.startswith("__"):
        print(c(f"  ✓ Saved as '{name}'", GREEN))

def load_config(workspace, name):
    path = config_path(workspace)
    if not path.exists():
        print(c(f"  No saved configs found.", RED)); sys.exit(1)
    data = json.loads(path.read_text())
    if name not in data:
        print(c(f"  Profile '{name}' not found. Available: {', '.join(k for k in data if not k.startswith('__'))}", RED))
        sys.exit(1)
    return data[name]["profile"], data[name]["extras"]

def list_configs(workspace):
    path = config_path(workspace)
    if not path.exists(): return {}
    data = json.loads(path.read_text())
    return {k: v for k, v in data.items() if not k.startswith("__")}

def delete_config(workspace, name):
    path = config_path(workspace)
    if not path.exists():
        print(c("  No saved configs found.", RED)); sys.exit(1)
    try:
        data = json.loads(path.read_text())
    except Exception:
        data = {}
    if name not in data or name.startswith("__"):
        keys = [k for k in data if not k.startswith("__")]
        print(c(f"  Profile '{name}' not found. Available: {', '.join(keys) or 'none'}", RED))
        sys.exit(1)
    del data[name]
    path.write_text(json.dumps(data, indent=2))
    print(c(f"  ✓ Deleted '{name}'", GREEN))

def show_saved_configs(workspace):
    """Print all saved profiles in a human-readable table."""
    configs = list_configs(workspace)
    if not configs:
        print(c("  No saved profiles. Use --save NAME after a build.", DIM))
        return
    print(c("  Saved profiles:", BOLD, WHITE))
    print()
    for name, cfg in configs.items():
        cmd     = build_command(cfg["profile"], cfg.get("extras", []))
        elapsed = cfg.get("last_elapsed")
        time_s  = c(f"  (last: {elapsed}s)", DIM) if elapsed else ""
        print(f"  {c(name, CYAN, BOLD)}{time_s}")
        print(f"    {c(cmd, WHITE)}")
        print()

# ── Build command builder ──────────────────────────────────────────────────────

def build_command(profile_key, extras, env=None):
    profile = PROFILES[profile_key]
    if profile_key == "custom":
        pkg = "axiomdb-server" if "wire-protocol" in extras else "axiomdb-embedded"
        feat = " ".join(extras)
        base = f"cargo build -p {pkg} --no-default-features"
        return f"{base} --features '{feat}' --release" if feat else f"{base} --release"
    if profile_key == "linux-musl":
        env = env or {}
        if env.get("zigbuild"):
            base = "cargo zigbuild -p axiomdb-server --target x86_64-unknown-linux-musl --release"
        elif env.get("cross"):
            base = "cross build -p axiomdb-server --target x86_64-unknown-linux-musl --release"
        else:
            base = "cargo build -p axiomdb-server --target x86_64-unknown-linux-musl --release"
        if extras:
            base += f" --features '{' '.join(extras)}'"
        return base
    base = profile["cmd_base"]
    if not base: return ""
    if extras:
        base += f" --features '{' '.join(extras)}'"
    return base

# ── Build runners ──────────────────────────────────────────────────────────────

def _stream_cargo(cmd, workspace, ci=False, label="Building"):
    """
    Stream a cargo command and show a live progress bar.
    Returns (returncode, elapsed_seconds).
    """
    print()
    print(c(f"  {label}...", BOLD, WHITE))
    print(c("  " + "─" * 60, DIM))
    print()

    start    = time.time()
    compiled = 0

    proc = subprocess.Popen(
        cmd, shell=True, cwd=workspace,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, bufsize=1,
    )

    for raw in iter(proc.stdout.readline, ""):
        line = raw.rstrip()
        if not line: continue

        if ci:
            print(line)
            continue

        if line.startswith("   Compiling") or line.startswith("   Checking"):
            compiled += 1
            crate = line.split()[-2] if len(line.split()) >= 2 else ""
            bar   = ("█" * min(compiled, 20)).ljust(20, "░")
            print(f"\r  {c(bar, CYAN)}  {c(crate, DIM):<40}", end="", flush=True)

        elif line.startswith("    Finished"):
            print(f"\r  {c('█'*20, GREEN)}  {c('Done', GREEN, BOLD):<40}", flush=True)

        elif "error[" in line or line.startswith("error"):
            print(f"\n  {c(line, RED)}")

        elif line.startswith("warning"):
            if "unused" not in line:
                print(f"\n  {c(line, YELLOW)}")

    proc.wait()
    elapsed = time.time() - start
    print()
    print(c("  " + "─" * 60, DIM))
    return proc.returncode, elapsed


def run_build(cmd, workspace, ci=False, timings=False):
    """Run cargo build. Returns (success: bool, elapsed: float)."""
    if timings:
        cmd += " --timings"
    rc, elapsed = _stream_cargo(cmd, workspace, ci=ci, label="Building")
    if rc == 0:
        print(c(f"  ✓ Build succeeded in {elapsed:.1f}s", GREEN, BOLD))
        if timings and not ci:
            _open_timings(workspace)
    else:
        print(c(f"  ✗ Build failed (exit {rc}) in {elapsed:.1f}s", RED, BOLD))
    return rc == 0, elapsed


def run_check(workspace, ci=False):
    """Run cargo check --workspace — fast check without linking. Returns (success, elapsed)."""
    rc, elapsed = _stream_cargo("cargo check --workspace", workspace, ci=ci, label="Checking")
    if rc == 0:
        print(c(f"  ✓ Check passed in {elapsed:.1f}s", GREEN, BOLD))
    else:
        print(c(f"  ✗ Check failed in {elapsed:.1f}s", RED, BOLD))
    return rc == 0, elapsed


def run_tests(workspace, ci=False):
    """Run cargo test --workspace. Returns success bool."""
    print()
    print(c("  Running tests...", BOLD, WHITE))
    print(c("  " + "─" * 60, DIM))
    start  = time.time()
    passed = 0
    failed = 0

    proc = subprocess.Popen(
        "cargo test --workspace", shell=True, cwd=workspace,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, bufsize=1,
    )
    for raw in iter(proc.stdout.readline, ""):
        line = raw.rstrip()
        if not line: continue
        if ci:
            print(line); continue
        if " ... ok" in line:
            passed += 1
            fail_s = f"  {c(str(failed)+' failed', RED)}" if failed else ""
            print(f"\r  {c('●', GREEN)} {c(str(passed), WHITE, BOLD)} passed{fail_s}",
                  end="", flush=True)
        elif "FAILED" in line:
            failed += 1
            print(f"\n  {c(line, RED)}")
        elif line.startswith("error"):
            print(f"\n  {c(line, RED)}")
    proc.wait()
    elapsed = time.time() - start
    print()
    print(c("  " + "─" * 60, DIM))
    if proc.returncode == 0:
        print(c(f"  ✓ {passed} tests passed in {elapsed:.1f}s", GREEN, BOLD))
    else:
        print(c(f"  ✗ {failed} test(s) failed in {elapsed:.1f}s", RED, BOLD))
    return proc.returncode == 0


def strip_binary(out_path):
    """Strip debug symbols from binary. Reports size before/after."""
    path = Path(out_path)
    if not path.exists():
        print(c(f"  ⚠  Binary not found at {out_path} — skipping strip", YELLOW))
        return
    before = path.stat().st_size
    try:
        subprocess.run(["strip", str(path)], check=True, stderr=subprocess.DEVNULL)
        after  = path.stat().st_size
        saved  = (1 - after / before) * 100
        print(c(f"  ✓ Stripped: {fmt_size(str(path))}  (saved {saved:.0f}%)", GREEN))
    except FileNotFoundError:
        print(c("  ⚠  strip not found — skipping", YELLOW))
    except subprocess.CalledProcessError as e:
        print(c(f"  ⚠  strip failed: {e}", YELLOW))


def notify_done(profile_name, success, elapsed):
    """Desktop notification when a build finishes (macOS + Linux)."""
    icon   = "✓" if success else "✗"
    status = "succeeded" if success else "FAILED"
    msg    = f"{icon} {profile_name} — {status} in {elapsed:.0f}s"
    if sys.platform == "darwin":
        try:
            subprocess.run(
                ["osascript", "-e",
                 f'display notification "{msg}" with title "AxiomDB Build"'],
                stderr=subprocess.DEVNULL, timeout=3,
            )
        except Exception:
            pass
    elif sys.platform.startswith("linux"):
        try:
            subprocess.run(["notify-send", "AxiomDB Build", msg],
                           stderr=subprocess.DEVNULL, timeout=3)
        except Exception:
            pass


def _open_timings(workspace):
    """Open the cargo-timings HTML report in the default browser."""
    timing_dir = workspace / "target" / "cargo-timings"
    reports = sorted(timing_dir.glob("cargo-timing-*.html")) if timing_dir.exists() else []
    if not reports:
        print(c("  ⚠  No timings report found", YELLOW))
        return
    report = reports[-1]
    print(c(f"  ⏱  Opening timings: {report.name}", CYAN))
    try:
        opener = "open" if sys.platform == "darwin" else "xdg-open"
        subprocess.Popen([opener, str(report)], stderr=subprocess.DEVNULL)
    except Exception:
        print(c(f"  → {report}", DIM))

# ── Artifact generation ────────────────────────────────────────────────────────

def gen_dockerfile(profile_key):
    if profile_key not in ("web", "linux-musl"): return None
    if profile_key == "linux-musl":
        build_cmds    = (
            "RUN rustup target add x86_64-unknown-linux-musl && cargo install cargo-zigbuild\n"
            "RUN cargo zigbuild -p axiomdb-server --target x86_64-unknown-linux-musl --release"
        )
        base_image    = "scratch"
        runtime_setup = ""
        bin_path      = PROFILES["linux-musl"]["output"]
    else:
        build_cmds    = "RUN cargo build -p axiomdb-server --release"
        base_image    = "debian:bookworm-slim"
        runtime_setup = "RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*\n"
        bin_path      = PROFILES["web"]["output"]

    return f"""\
# AxiomDB — production Docker image
# Build: docker build -t axiomdb .
# Run:   docker run -p 3306:3306 -v axiomdb_data:/data axiomdb

FROM rust:1.80-slim AS builder
WORKDIR /app
COPY . .
{build_cmds}

FROM {base_image}
{runtime_setup}COPY --from=builder /app/{bin_path} /usr/local/bin/axiomdb-server
VOLUME ["/data"]
EXPOSE 3306
ENV AXIOMDB_DATA=/data
CMD ["axiomdb-server"]
"""

def gen_github_actions(profile_key, cmd):
    if not PROFILES[profile_key].get("ci"): return None
    build_step = cmd.replace("--release", "--release 2>&1")
    return f"""\
# .github/workflows/build.yml
# Auto-generated by AxiomDB Build Wizard

name: Build AxiomDB

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  build:
    name: Build ({PROFILES[profile_key]['name']})
    runs-on: ${{{{ matrix.os }}}}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: stable

      - name: Cache cargo registry
        uses: actions/cache@v4
        with:
          path: ~/.cargo/registry
          key: ${{{{ runner.os }}}}-cargo-${{{{ hashFiles('**/Cargo.lock') }}}}

      - name: Build
        run: {build_step}

      - name: Test
        run: cargo test --workspace

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: axiomdb-${{{{ runner.os }}}}
          path: {PROFILES[profile_key]['output'].split('{{')[0]}
"""

def gen_makefile_targets(configs):
    lines = ["# AxiomDB Makefile targets — auto-generated by Build Wizard", ""]
    for name, cfg in configs.items():
        cmd = build_command(cfg["profile"], cfg.get("extras", []))
        lines.append(f".PHONY: {name}")
        lines.append(f"{name}:")
        lines.append(f"\t{cmd}")
        lines.append("")
    lines.append(".PHONY: check")
    lines.append("check:")
    lines.append("\tcargo check --workspace")
    lines.append("")
    lines.append(".PHONY: test")
    lines.append("test:")
    lines.append("\tcargo test --workspace")
    lines.append("")
    lines.append(".PHONY: clean")
    lines.append("clean:")
    lines.append("\tcargo clean")
    return "\n".join(lines)

# ── Post-build summary ─────────────────────────────────────────────────────────

def post_build_info(profile_key, cmd, workspace, prev_size=None, elapsed=None):
    profile  = PROFILES[profile_key]
    out_path = workspace / profile["output"].split("{")[0].rstrip(".")
    actual   = fmt_size(out_path)

    print()
    print(c("  " + "─" * 60, DIM))

    # ── Binary size block ──────────────────────────────────────────────────────
    if actual:
        diff = ""
        if prev_size and prev_size != actual:
            diff = c(f"  ← was {prev_size}", DIM)
        print(f"  {c('📦 Binary size', BOLD)}  {c(actual, WHITE, BOLD)}{diff}")
        print(f"  {c(str(out_path), DIM)}")
    else:
        print(f"  {c('📦 Output:', BOLD)} {c(profile['output'], DIM)}")

    if elapsed is not None:
        print(f"  {c('⏱  Build time', BOLD)}   {c(f'{elapsed:.1f}s', WHITE)}")

    print(c("  " + "─" * 60, DIM))
    print()

    # ── How to use ─────────────────────────────────────────────────────────────
    print(c("  Next steps:", BOLD))
    if profile_key in ("web", "linux-musl"):
        print(f"  {c('1.', CYAN)} Start the server:")
        print(f"     {c('./target/release/axiomdb-server', WHITE)}")
        print(f"  {c('2.', CYAN)} Connect with any MySQL client:")
        print(f"     {c('mysql -h 127.0.0.1 -P 3306 -u root', WHITE)}")
        mysql_ex = 'pymysql.connect(host="127.0.0.1", port=3306, user="root")'
        print(f"     {c(mysql_ex, WHITE)}")
        print(f"  {c('3.', CYAN)} Set data dir:")
        print(f"     {c('AXIOMDB_DATA=/var/lib/axiomdb ./axiomdb-server', WHITE)}")

    elif profile_key in ("desktop", "mobile"):
        c_ex = 'AxiomDb* db = axiomdb_open("./myapp.db");'
        print(f"  {c('1.', CYAN)} Link against libaxiomdb_embedded.{{dylib,so,a}}")
        print(f"  {c('2.', CYAN)} C API:")
        print(f"     {c(c_ex, WHITE)}")
        ex2 = 'axiomdb_execute(db, "CREATE TABLE ...");'
        print(f"     {c(ex2, WHITE)}")
        print(f"     {c('axiomdb_close(db);', WHITE)}")

    elif profile_key in ("rust-embedded", "async-rust"):
        dep = 'axiomdb-embedded = { path = "crates/axiomdb-embedded" }'
        r1 = 'let mut db = axiomdb_embedded::Db::open("./myapp.db")?;'
        r2 = 'db.execute("CREATE TABLE users (id INT, name TEXT)")?;'
        r3 = 'let rows = db.query("SELECT * FROM users")?;'
        print(f"  {c('1.', CYAN)} Cargo.toml: {c(dep, WHITE)}")
        print(f"  {c('2.', CYAN)} Rust code:")
        for ex in [r1, r2, r3]:
            print(f"     {c(ex, WHITE)}")
    print()

# ── All-profiles view ──────────────────────────────────────────────────────────

def show_profiles(workspace):
    clear()
    header("All available build profiles")
    configs = list_configs(workspace)

    for key, profile in PROFILES.items():
        if key == "custom": continue
        status = c("future", YELLOW) if profile.get("future") else c("ready", GREEN)
        print(f"  {profile['emoji']}  {c(profile['name'], BOLD, WHITE)}  [{status}]")
        cmd = build_command(key, [])
        print(f"  {c(cmd or '(see wizard)', CYAN)}")
        out_path = workspace / profile["output"].split("{")[0].rstrip(".")
        actual = fmt_size(out_path)
        size = f"built: {actual}" if actual else f"est: {profile['size_est']}"
        print(c(f"  {profile['output']}  ·  {size}", DIM))
        print()

    if configs:
        print(c("  Saved profiles:", BOLD))
        for name, cfg in configs.items():
            cmd     = build_command(cfg["profile"], cfg.get("extras", []))
            elapsed = cfg.get("last_elapsed")
            time_s  = c(f"  ({elapsed}s)", DIM) if elapsed else ""
            print(f"  {c(name, CYAN)}  →  {c(cmd, WHITE)}{time_s}")
        print()

# ── Main wizard flow ───────────────────────────────────────────────────────────

def wizard(workspace, env, auto_run=False, ci=False, preset_save_name=None,
           do_test=False, do_strip=False, do_timings=False, do_notify=False):
    # ── Step 1: choose target ──────────────────────────────────────────────────
    clear()
    header("Step 1 of 3 — Choose your target")
    print_env_status(env)

    target_options = [
        ("web",          "🌐 Web / Cloud Server",           "MySQL wire protocol, Docker, VPS"),
        ("desktop",      "🖥️  Desktop Application",          "Tauri, native GUI, offline app"),
        ("mobile",       "📱 Mobile App",                    "iOS (Swift), Android (Kotlin)"),
        ("linux-musl",   "🐧 Linux x86-64 (cross)",          "Static musl — VPS, Docker, CI"),
        ("rust-embedded","⚙️  Embedded in Rust app",          "cargo dependency, sync API"),
        ("async-rust",   "⚡ Async Rust (Axum / Tokio)",    "AsyncDb with tokio"),
        ("wasm",         "🌍 WebAssembly",                   "browser, in-memory — future"),
        ("custom",       "🔧 Custom",                        "pick individual features"),
    ]
    _, profile_key = arrow_menu("What are you building?", target_options,
                                 hint="↑↓ to move · Enter to select")

    # Cross-compile tool check for linux-musl
    if profile_key == "linux-musl":
        if not env.get("zigbuild") and not env.get("cross"):
            print()
            print(c("  ⚠  No cross-compilation tool found.", YELLOW))
            print(c("     Best option:  cargo install cargo-zigbuild", WHITE))
            print(c("     Alternative:  cargo install cross", DIM))
            if not confirm("Continue anyway (native cargo, may fail on macOS)?", default=False):
                sys.exit(0)
        elif env.get("zigbuild"):
            print()
            print(c("  ✓ Using cargo-zigbuild for Linux cross-compilation", GREEN))
        else:
            print()
            print(c("  ✓ Using cross for Linux cross-compilation", GREEN))

    # Check required target
    req = PROFILES[profile_key].get("required_target")
    if req and req not in env.get("targets", set()):
        print()
        print(c(f"  ⚠  Target '{req}' not installed.", YELLOW))
        print(c(f"     Run: rustup target add {req}", WHITE))
        if not confirm("Install it now?", default=True):
            sys.exit(0)
        subprocess.run(["rustup", "target", "add", req], cwd=workspace)

    # Future warning
    if PROFILES[profile_key].get("future"):
        clear()
        print()
        print(c("  ⚠  WebAssembly support is not yet implemented (Phase 10+).", YELLOW))
        print(c("     This profile will be available in a future release.", DIM))
        print()
        input(c("  Press Enter to go back...", DIM))
        return wizard(workspace, env, auto_run, ci, preset_save_name,
                      do_test, do_strip, do_timings, do_notify)

    # ── Step 2: optional extras ────────────────────────────────────────────────
    extras = []
    if profile_key == "custom":
        clear()
        header("Step 2 of 3 — Choose features")
        opts = [(k, l, d, default) for k, l, d, default in CUSTOM_FEATURES]
        extras = arrow_checkbox("Select features:", opts)
    else:
        avail = PROFILES[profile_key].get("optional_features", [])
        if avail:
            clear()
            header("Step 2 of 3 — Optional extras")
            now_opts    = [(k, OPTIONAL_FEATURES[k][0], OPTIONAL_FEATURES[k][1], False)
                           for k in avail if OPTIONAL_FEATURES.get(k, ("","",""))[2] == "available"]
            future_opts = [(k, OPTIONAL_FEATURES[k][0], OPTIONAL_FEATURES[k][1], False)
                           for k in avail if OPTIONAL_FEATURES.get(k, ("","",""))[2] == "future"]

            if now_opts:
                extras = arrow_checkbox("Add to your build:", now_opts)
            if future_opts:
                print(c("  Coming soon (not yet available):", DIM))
                for k, label, desc, _ in future_opts:
                    print(c(f"  · {label}", YELLOW) + c(f"  {desc}", DIM))
                print()

    # ── Step 3: summary ────────────────────────────────────────────────────────
    clear()
    header("Step 3 of 3 — Build summary")
    profile = PROFILES[profile_key]
    cmd     = build_command(profile_key, extras, env=env)

    status  = c(" READY ", GREEN, BOLD) if not profile.get("future") else c(" FUTURE ", YELLOW, BOLD)
    print(f"  {profile['emoji']}  {c(profile['name'], BOLD, WHITE)} {status}")
    print()
    print(c("  Included:", BOLD))
    for icon, label, desc in profile["includes"]:
        ico = c(icon, GREEN if icon == "✓" else YELLOW)
        print(f"    {ico} {c(label, WHITE)}  {c(desc, DIM)}")

    if extras:
        print()
        print(c("  Extras:", BOLD))
        for e in extras:
            if e in OPTIONAL_FEATURES:
                v = OPTIONAL_FEATURES[e]
                tag = c(" available ", GREEN) if v[2] == "available" else c(" future ", YELLOW)
                print(f"    {c('+', CYAN, BOLD)} {c(v[0], WHITE)}  {c(v[1], DIM)}{tag}")

    print()
    print(c("  Output:", BOLD))
    prev_size = env["builds"].get("server", {}).get("size") or env["builds"].get("embedded", {}).get("size")
    print(f"    {c(profile['output'], CYAN)}  {c(profile['size_est'], DIM)}")
    print()
    print(c("  Build command:", BOLD))
    print(f"\n    {c(cmd, CYAN, BOLD)}\n")

    # ── Post-build options ─────────────────────────────────────────────────────
    if not ci:
        post_opts = [
            ("test",    "Run tests after build",   "cargo test --workspace",             do_test),
            ("strip",   "Strip binary",            "remove debug symbols, smaller file", do_strip),
            ("timings", "Open build timings",      "HTML report of slow crates",         do_timings),
            ("notify",  "Desktop notification",    "alert when build finishes",          do_notify),
        ]
        selected_post = arrow_checkbox("Post-build actions:", post_opts,
                                        hint="Optional — all off by default")
        do_test    = "test"    in selected_post
        do_strip   = "strip"   in selected_post
        do_timings = "timings" in selected_post
        do_notify  = "notify"  in selected_post

    # ── Artifacts to generate ──────────────────────────────────────────────────
    to_gen = []
    if profile.get("docker") or profile.get("ci"):
        gen_options = []
        if profile.get("docker"):
            gen_options.append(("dockerfile",  "Dockerfile",               "Docker image for production", False))
        if profile.get("ci"):
            gen_options.append(("github_ci",   "GitHub Actions workflow",  ".github/workflows/build.yml", False))
        gen_options.append(    ("makefile",    "Makefile target",          "make build-web / make test",  False))

        if gen_options and not ci:
            print()
            to_gen = arrow_checkbox("Also generate:", gen_options,
                                     hint="Optional files to scaffold")

    # ── Save config ────────────────────────────────────────────────────────────
    save_name = preset_save_name
    if not ci and save_name is None:
        print()
        if confirm("Save this profile for later? (--last always saved automatically)", default=False):
            save_name = input(c("  Profile name: ", CYAN)).strip() or "default"

    # ── Run ────────────────────────────────────────────────────────────────────
    print()
    should_run = auto_run or ci or confirm("Run this build now?", default=True)

    elapsed = None
    ok      = False
    if should_run:
        ok, elapsed = run_build(cmd, workspace, ci=ci, timings=do_timings)

        if ok:
            if do_strip:
                out_path = workspace / profile["output"].split("{")[0].rstrip(".")
                strip_binary(str(out_path))

            if do_test:
                run_tests(workspace, ci=ci)

            if "dockerfile" in to_gen:
                content = gen_dockerfile(profile_key)
                if content:
                    (workspace / "Dockerfile").write_text(content)
                    print(c("  ✓ Dockerfile written", GREEN))

            if "github_ci" in to_gen:
                ci_dir = workspace / ".github/workflows"
                ci_dir.mkdir(parents=True, exist_ok=True)
                content = gen_github_actions(profile_key, cmd)
                if content:
                    (ci_dir / "build.yml").write_text(content)
                    print(c("  ✓ .github/workflows/build.yml written", GREEN))

            if "makefile" in to_gen:
                configs = list_configs(workspace)
                configs[profile_key] = {"profile": profile_key, "extras": extras}
                content = gen_makefile_targets(configs)
                (workspace / "Makefile").write_text(content)
                print(c("  ✓ Makefile written", GREEN))

            post_build_info(profile_key, cmd, workspace, prev_size, elapsed=elapsed)

        if do_notify and elapsed is not None:
            notify_done(profile["name"], ok, elapsed)

        if ci:
            sys.exit(0 if ok else 1)
    else:
        print()
        print(c("  Copy and run when ready:", DIM))
        print(f"  {c(cmd, CYAN, BOLD)}")
        print()

    # Save after build so we can record elapsed time
    save_config(workspace, save_name or "__last_only__", profile_key, extras, elapsed=elapsed)
    if save_name and save_name != "__last_only__":
        save_config(workspace, save_name, profile_key, extras, elapsed=elapsed)

# ── Main ───────────────────────────────────────────────────────────────────────

def find_workspace():
    path = Path(__file__).resolve().parent
    while path != path.parent:
        if (path / "Cargo.toml").exists():
            if "[workspace]" in (path / "Cargo.toml").read_text():
                return path
        path = path.parent
    return Path.cwd()

def main():
    p = argparse.ArgumentParser(
        description="AxiomDB Build Wizard — interactive build profile selector",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
examples:
  python3 tools/build-wizard.py                    interactive wizard
  python3 tools/build-wizard.py --run              wizard + auto build
  python3 tools/build-wizard.py --run --test       build + run tests
  python3 tools/build-wizard.py --run --strip      build + strip binary
  python3 tools/build-wizard.py --run --timings    build + open timing report
  python3 tools/build-wizard.py --run --notify     build + desktop notification
  python3 tools/build-wizard.py --check            fast cargo check --workspace
  python3 tools/build-wizard.py --profiles         show all profiles
  python3 tools/build-wizard.py --last             repeat last build
  python3 tools/build-wizard.py --list             list saved profiles
  python3 tools/build-wizard.py --save prod-web    save as "prod-web"
  python3 tools/build-wizard.py --load prod-web    load and run saved
  python3 tools/build-wizard.py --delete prod-web  delete saved profile
  python3 tools/build-wizard.py --ci --profile web CI/CD non-interactive
        """,
    )
    p.add_argument("--run",      action="store_true", help="auto-build after selection")
    p.add_argument("--profiles", action="store_true", help="show all profiles and exit")
    p.add_argument("--last",     action="store_true", help="repeat the last build")
    p.add_argument("--check",    action="store_true", help="run cargo check --workspace (fast, no linking)")
    p.add_argument("--list",     action="store_true", help="list saved profiles")
    p.add_argument("--save",     metavar="NAME",      help="save selection with a name")
    p.add_argument("--load",     metavar="NAME",      help="load and run a saved profile")
    p.add_argument("--delete",   metavar="NAME",      help="delete a saved profile")
    p.add_argument("--test",     action="store_true", help="run cargo test --workspace after build")
    p.add_argument("--strip",    action="store_true", help="strip debug symbols from binary after build")
    p.add_argument("--timings",  action="store_true", help="open build timings HTML report after build")
    p.add_argument("--notify",   action="store_true", help="send desktop notification when build finishes")
    p.add_argument("--ci",       action="store_true", help="non-interactive CI/CD mode")
    p.add_argument("--profile",  metavar="KEY",       help="profile key for --ci mode",
                   choices=list(PROFILES.keys()))
    p.add_argument("--no-color", action="store_true", help="disable ANSI colors")
    a = p.parse_args()

    global NO_COLOR
    if a.no_color: NO_COLOR = True

    workspace = find_workspace()
    env = detect_env(workspace)

    # ── --check ────────────────────────────────────────────────────────────────
    if a.check:
        ok, _ = run_check(workspace)
        sys.exit(0 if ok else 1)

    # ── --profiles ─────────────────────────────────────────────────────────────
    if a.profiles:
        show_profiles(workspace)
        return

    # ── --list ─────────────────────────────────────────────────────────────────
    if a.list:
        show_saved_configs(workspace)
        return

    # ── --delete ───────────────────────────────────────────────────────────────
    if a.delete:
        delete_config(workspace, a.delete)
        return

    # ── --last ─────────────────────────────────────────────────────────────────
    if a.last:
        profile_key, extras = load_config(workspace, "__last__")
        cmd = build_command(profile_key, extras, env=env)
        print(c(f"  Repeating: {cmd}", CYAN))
        ok, elapsed = run_build(cmd, workspace, timings=a.timings)
        if ok:
            if a.strip:
                out_path = workspace / PROFILES[profile_key]["output"].split("{")[0].rstrip(".")
                strip_binary(str(out_path))
            if a.test:
                run_tests(workspace)
            post_build_info(profile_key, cmd, workspace, elapsed=elapsed)
        if a.notify:
            notify_done(PROFILES[profile_key]["name"], ok, elapsed)
        save_config(workspace, "__last__", profile_key, extras, elapsed=elapsed)
        sys.exit(0 if ok else 1)

    # ── --load ─────────────────────────────────────────────────────────────────
    if a.load:
        profile_key, extras = load_config(workspace, a.load)
        cmd = build_command(profile_key, extras, env=env)
        clear()
        header(f"Loading profile: {a.load}")
        print(c(f"  Command: {cmd}", CYAN))
        print()
        if confirm("Run?", default=True):
            ok, elapsed = run_build(cmd, workspace, timings=a.timings)
            if ok:
                if a.strip:
                    out_path = workspace / PROFILES[profile_key]["output"].split("{")[0].rstrip(".")
                    strip_binary(str(out_path))
                if a.test:
                    run_tests(workspace)
                post_build_info(profile_key, cmd, workspace, elapsed=elapsed)
            if a.notify:
                notify_done(PROFILES[profile_key]["name"], ok, elapsed)
            save_config(workspace, a.load, profile_key, extras, elapsed=elapsed)
            sys.exit(0 if ok else 1)
        return

    # ── --ci --profile ─────────────────────────────────────────────────────────
    if a.ci:
        if not a.profile:
            p.error("--ci requires --profile")
        cmd = build_command(a.profile, [], env=env)
        print(f"CI build: {cmd}")
        ok, elapsed = run_build(cmd, workspace, ci=True)
        if ok and a.test:
            run_tests(workspace, ci=True)
        sys.exit(0 if ok else 1)

    # ── interactive wizard ─────────────────────────────────────────────────────
    wizard(workspace, env, auto_run=a.run, preset_save_name=a.save,
           do_test=a.test, do_strip=a.strip,
           do_timings=a.timings, do_notify=a.notify)

if __name__ == "__main__":
    main()
