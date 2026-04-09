#!/bin/bash
# Wrapper linker for macOS 26 beta: strips com.apple.provenance xattr from
# compiled binaries so syspolicyd does not block test execution.
# Remove this wrapper once Apple fixes the beta security daemon behaviour.

# Find the output file from linker args (-o <path>)
OUTPUT=""
PREV=""
for arg in "$@"; do
    if [[ "$PREV" == "-o" ]]; then
        OUTPUT="$arg"
    fi
    PREV="$arg"
done

# Run the real linker
cc "$@"
STATUS=$?

# Strip provenance xattr if compilation succeeded
if [[ $STATUS -eq 0 && -n "$OUTPUT" && -f "$OUTPUT" ]]; then
    xattr -d com.apple.provenance "$OUTPUT" 2>/dev/null || true
fi

exit $STATUS
