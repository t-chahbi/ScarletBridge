#!/usr/bin/env bash
set -euo pipefail

if command -v x86_64-w64-mingw32-ar >/dev/null 2>&1; then
    exec x86_64-w64-mingw32-ar "$@"
fi

if command -v llvm-ar >/dev/null 2>&1; then
    exec llvm-ar "$@"
fi

ZIG_BIN=""
if command -v zig >/dev/null 2>&1; then
    ZIG_BIN="$(command -v zig)"
elif command -v mise >/dev/null 2>&1; then
    ZIG_BIN="$(mise which zig 2>/dev/null || true)"
fi

if [[ -n "$ZIG_BIN" ]]; then
    exec "$ZIG_BIN" ar "$@"
fi

echo "error: no Windows archiver (x86_64-w64-mingw32-ar, llvm-ar, or zig ar) found." >&2
exit 1
