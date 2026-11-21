#!/usr/bin/env bash
set -euo pipefail

# Cross-linker for x86_64-pc-windows-gnu PE32+ targets.
# Prefers native mingw-w64-gcc when installed; falls back transparently to `zig cc`.

if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    exec x86_64-w64-mingw32-gcc "$@"
fi

ZIG_BIN=""
if command -v zig >/dev/null 2>&1; then
    ZIG_BIN="$(command -v zig)"
elif command -v mise >/dev/null 2>&1; then
    ZIG_BIN="$(mise which zig 2>/dev/null || true)"
fi

if [[ -n "$ZIG_BIN" ]]; then
    # Adapt GNU gcc/ld flags emitted by rustc for `zig cc -target x86_64-windows-gnu`.
    args=()
    for arg in "$@"; do
        case "$arg" in
            -Wl,--no-eh-frame-hdr|-fno-use-linker-plugin|-Wl,--enable-auto-image-base|-Wl,--disable-auto-image-base|-Wl,-Bdynamic|-Wl,-Bstatic|-nodefaultlibs)
                continue
                ;;
            -Wl,*.def)
                # Strip `-Wl,` so zig cc recognizes the module-definition (.def) file directly.
                args+=("${arg#-Wl,}")
                ;;
            -l:libpthread.a)
                args+=("-lpthread")
                ;;
            -lsynchronization)
                args+=("-lapi-ms-win-core-synch-l1-2-0")
                ;;
            -lgcc_eh)
                args+=("-lunwind")
                ;;
            -lgcc|-lmingwex|-lmingw32|-lmsvcrt)
                # Provided automatically by zig's bundled MinGW-w64 runtime.
                continue
                ;;
            *)
                args+=("$arg")
                ;;
        esac
    done
    exec "$ZIG_BIN" cc -target x86_64-windows-gnu "${args[@]}"
fi

echo "error: neither x86_64-w64-mingw32-gcc nor zig was found in PATH." >&2
echo "Install mingw-w64-gcc (sudo pacman -S mingw-w64-gcc) or run 'mise install'." >&2
exit 1
