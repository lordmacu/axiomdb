#!/bin/bash
# Wrapper for local macOS 26 beta builds: after each rustc invocation, strip
# com.apple.provenance from the output directory so proc-macro dylibs can be
# dlopen()'d by subsequent rustc processes without hanging in dyld.
# Remove this wrapper once Apple fixes the beta security daemon behaviour.

set -u

REAL_RUSTC="$1"
shift

STRIP_DIR=""
STRIP_OUTPUT=""
PREV=""

strip_provenance_path() {
    local path="$1"
    if [[ -e "$path" ]]; then
        xattr -d com.apple.provenance "$path" 2>/dev/null || true
    fi
}

strip_provenance_dir_dylibs() {
    local dir="$1"
    if [[ ! -d "$dir" ]]; then
        return 0
    fi

    while IFS= read -r -d '' path; do
        strip_provenance_path "$path"
    done < <(
        find "$dir" -maxdepth 1 -type f \
            \( -name '*.dylib' -o -name '*.so' -o -name '*.bundle' \) \
            -print0 2>/dev/null
    )
}

for arg in "$@"; do
    case "$arg" in
        --out-dir=*)
            STRIP_DIR="${arg#--out-dir=}"
            ;;
        -o*)
            if [[ "$arg" != "-o" ]]; then
                STRIP_OUTPUT="${arg#-o}"
            fi
            ;;
        *)
            if [[ "$PREV" == "--out-dir" ]]; then
                STRIP_DIR="$arg"
            elif [[ "$PREV" == "-o" ]]; then
                STRIP_OUTPUT="$arg"
            fi
            ;;
    esac
    PREV="$arg"
done

if [[ -n "$STRIP_OUTPUT" ]]; then
    strip_provenance_path "$STRIP_OUTPUT"
fi
if [[ -n "$STRIP_DIR" ]]; then
    strip_provenance_dir_dylibs "$STRIP_DIR"
fi

"$REAL_RUSTC" "$@"
STATUS=$?

if [[ $STATUS -eq 0 ]]; then
    if [[ -n "$STRIP_OUTPUT" ]]; then
        strip_provenance_path "$STRIP_OUTPUT"
    fi
    if [[ -n "$STRIP_DIR" ]]; then
        strip_provenance_dir_dylibs "$STRIP_DIR"
    fi
fi

exit $STATUS
