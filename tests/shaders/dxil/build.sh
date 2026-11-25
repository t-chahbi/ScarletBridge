#!/bin/sh
# Recompile every .hlsl in this directory with the official dxc (not shipped in the repo).
# Usage: DXC_DIR=/path/to/dxc ./build.sh   (DXC_DIR contains bin/dxc and lib/)
set -e
cd "$(dirname "$0")"
DXC_DIR=${DXC_DIR:?set DXC_DIR}
export LD_LIBRARY_PATH="$DXC_DIR/lib"
for f in *.hlsl; do
  args=$(sed -n '1s|^// dxc: ||p' "$f")
  "$DXC_DIR/bin/dxc" $args -E main -Fo "${f%.hlsl}.dxil" "$f" >/dev/null || echo "FAILED $f"
done
