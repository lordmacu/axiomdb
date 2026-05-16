#!/usr/bin/env bash
# Run build/test/wire commands inside the Lima axiomdb VM.
# Source stays on macOS (edited in VSCode/Claude), target goes to VM-native filesystem.
#
# Usage:
#   tools/vm.sh build [args]       # cargo build [args]
#   tools/vm.sh test [args]        # cargo nextest run [args]
#   tools/vm.sh clippy             # cargo clippy --workspace -- -D warnings
#   tools/vm.sh fmt                # cargo fmt (apply in-place, requires writable virtiofs)
#   tools/vm.sh fmt-check          # cargo fmt --check (read-only)
#   tools/vm.sh wire               # build server + run wire-test.py from Lima
#   tools/vm.sh shell              # open interactive shell in the VM

set -euo pipefail

VM="axiomdb"
TARGET_DIR="\$HOME/axiomdb-target"
WORKDIR="/Users/cristian/nexusdb"

CMD="${1:-}"
shift || true

case "$CMD" in
  build)
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && CARGO_TARGET_DIR=$TARGET_DIR cargo build $* 2>&1"
    ;;
  test)
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && CARGO_TARGET_DIR=$TARGET_DIR cargo nextest run $* 2>&1"
    ;;
  clippy)
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && CARGO_TARGET_DIR=$TARGET_DIR cargo clippy --workspace -- -D warnings 2>&1"
    ;;
  fmt)
    # Applies formatting in-place (virtiofs mount is writable since 2026-05-15)
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && cargo fmt 2>&1"
    ;;
  fmt-check)
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && cargo fmt --check 2>&1"
    ;;
  wire)
    # Build release server binary on Lima (Linux), then run wire-test.py from Lima.
    echo "==> Building axiomdb-server (release) on Lima..."
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && CARGO_TARGET_DIR=$TARGET_DIR cargo build -p axiomdb-server --release 2>&1"
    echo "==> Running wire-test.py..."
    limactl shell "$VM" -- bash -c \
      "AXIOMDB_SERVER_BIN=$TARGET_DIR/release/axiomdb-server python3 $WORKDIR/tools/wire-test.py $* 2>&1"
    ;;
  shell)
    limactl shell "$VM"
    ;;
  *)
    echo "Usage: $0 {build|test|clippy|fmt|fmt-check|wire|shell} [extra args]"
    echo
    echo "Examples:"
    echo "  $0 build -p axiomdb-sql"
    echo "  $0 test --workspace"
    echo "  $0 test -p axiomdb-sql --test integration_range_types"
    echo "  $0 clippy"
    echo "  $0 fmt            # format in-place (writable virtiofs)"
    echo "  $0 fmt-check      # check only, no writes"
    echo "  $0 wire           # build Linux server + run wire-test.py"
    echo "  $0 shell          # interactive Lima shell"
    exit 1
    ;;
esac
