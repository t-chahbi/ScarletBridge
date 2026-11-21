#!/usr/bin/env bash
set -euo pipefail

if command -v x86_64-w64-mingw32-dlltool >/dev/null 2>&1; then
    exec x86_64-w64-mingw32-dlltool "$@"
fi

DLLTOOL_BIN=""
if command -v llvm-dlltool >/dev/null 2>&1; then
    DLLTOOL_BIN="$(command -v llvm-dlltool)"
elif command -v zig >/dev/null 2>&1; then
    DLLTOOL_BIN="zig dlltool"
elif command -v mise >/dev/null 2>&1; then
    ZIG_BIN="$(mise which zig 2>/dev/null || true)"
    if [[ -n "$ZIG_BIN" ]]; then
        DLLTOOL_BIN="$ZIG_BIN dlltool"
    fi
fi

if [[ -n "$DLLTOOL_BIN" ]]; then
    # Filter GNU-only flags that llvm-dlltool doesn't recognize.
    args=()
    skip_next=0
    for arg in "$@"; do
        if [[ "$skip_next" == "1" ]]; then
            skip_next=0
            continue
        fi
        case "$arg" in
            --temp-prefix|--no-leading-underscore)
                if [[ "$arg" == "--temp-prefix" ]]; then
                    skip_next=1
                fi
                continue
                ;;
            --temp-prefix=*)
                continue
                ;;
            *)
                args+=("$arg")
                ;;
        esac
    done
    exec $DLLTOOL_BIN "${args[@]}"
fi

echo "error: no dlltool (x86_64-w64-mingw32-dlltool, llvm-dlltool, or zig dlltool) found." >&2
exit 1
