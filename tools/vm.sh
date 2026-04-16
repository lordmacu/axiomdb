#!/usr/bin/env bash
# Run build/test commands inside the Lima axiomdb VM.
# Source stays on macOS (edited in VSCode), target goes to VM-native filesystem.
#
# Usage:
#   tools/vm.sh build [args]          # cargo build [args]
#   tools/vm.sh test [args]           # cargo nextest run [args]
#   tools/vm.sh clippy                # cargo clippy --workspace -- -D warnings
#   tools/vm.sh fmt                   # cargo fmt --check
#   tools/vm.sh shell                 # open an interactive shell in the VM

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
    limactl shell "$VM" -- bash -c \
      "source ~/.cargo/env && cargo fmt --check 2>&1"
    ;;
  shell)
    limactl shell "$VM"
    ;;
  *)
    echo "Usage: $0 {build|test|clippy|fmt|shell} [extra args]"
    echo
    echo "Examples:"
    echo "  $0 build -p axiomdb-sql"
    echo "  $0 test --workspace"
    echo "  $0 test -p axiomdb-sql --test integration_delete_apply"
    echo "  $0 clippy"
    echo "  $0 fmt"
    exit 1
    ;;
esac
