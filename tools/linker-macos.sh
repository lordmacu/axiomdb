#!/bin/bash
# Wrapper linker for macOS 26 beta: strips com.apple.provenance xattr from
# compiled binaries so syspolicyd does not block test execution.
# Remove this wrapper once Apple fixes the beta security daemon behaviour.
#
# Also uses LLVM's ld64.lld linker (3-5x faster than Apple's default ld) if
# available. Install via `brew install llvm` (keg-only path).

# Find the output file from linker args (-o <path>)
OUTPUT=""
PREV=""
for arg in "$@"; do
    if [[ "$PREV" == "-o" ]]; then
        OUTPUT="$arg"
    fi
    PREV="$arg"
done

# Run the real linker — prefer lld if available for faster link times.
LLD_PATH="/opt/homebrew/opt/llvm/bin/ld64.lld"
if [[ -x "$LLD_PATH" ]]; then
    cc -fuse-ld="$LLD_PATH" "$@"
else
    cc "$@"
fi
STATUS=$?

# Strip provenance xattr if compilation succeeded
if [[ $STATUS -eq 0 && -n "$OUTPUT" && -f "$OUTPUT" ]]; then
    xattr -d com.apple.provenance "$OUTPUT" 2>/dev/null || true
fi

exit $STATUS
