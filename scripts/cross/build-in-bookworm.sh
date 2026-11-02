#!/usr/bin/env bash
# Build the HARDENED synaps binary inside a Debian bookworm container (glibc
# 2.36 = the media-library bridge runtime, node:24-bookworm-slim), so the
# produced binary never requires a newer glibc symbol than the runtime has.
# From an x86_64 host this cross-compiles aarch64 (GNU cross-gcc) and builds
# x86_64 natively. Produces the same artifacts as CI's `Hardened release`:
#   dist/hardened/synaps-hardened-<target>.tar.xz(.sha256)
#
# Runs scripts/build-hardened.sh unchanged inside the container, then verifies
# the ELF arch + max required GLIBC version and smoke-runs `synaps --version`
# (under qemu-user-static for the foreign arch) so a glibc/arch mistake is
# caught here rather than at the bridge image build on the production box.
#
# Usage:
#   scripts/cross/build-in-bookworm.sh                              # aarch64
#   scripts/cross/build-in-bookworm.sh x86_64-unknown-linux-gnu
#   scripts/cross/build-in-bookworm.sh aarch64-unknown-linux-gnu
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${CROSS_IMAGE:-synaps-hardened-cross:bookworm}"
TARGET="${1:-aarch64-unknown-linux-gnu}"

case "$TARGET" in
  aarch64-unknown-linux-gnu) OBJDUMP=aarch64-linux-gnu-objdump; RUNNER="qemu-aarch64-static -L /usr/aarch64-linux-gnu"; MACHINE=AArch64 ;;
  x86_64-unknown-linux-gnu)  OBJDUMP=objdump;                   RUNNER="";                                             MACHINE="X86-64" ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

docker build -q -t "$IMAGE" -f "$ROOT_DIR/scripts/cross/Dockerfile.aarch64-gnu" "$ROOT_DIR/scripts/cross" >/dev/null
echo "cross: image $IMAGE ready"

# Reuse the host crate cache when present (read-write: cargo may add to it).
REG_MOUNT=()
if [ -d "$HOME/.cargo/registry" ]; then
  REG_MOUNT=(-v "$HOME/.cargo/registry:/usr/local/cargo/registry")
fi

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e HOME=/tmp/home -e CARGO_HOME=/usr/local/cargo \
  -e CARGO_NET_GIT_FETCH_WITH_CLI=true \
  -v "$ROOT_DIR:/src" -w /src \
  "${REG_MOUNT[@]}" \
  "$IMAGE" \
  bash -euo pipefail -c '
    mkdir -p /tmp/home
    scripts/build-hardened.sh "$0"
    BIN="target/$0/release/synaps"
    echo "cross: ELF:"; readelf -h "$BIN" | grep -E "Class|Machine"
    readelf -h "$BIN" | grep -q "Machine:.*$3" || { echo "cross: FATAL wrong machine (want $3)" >&2; exit 2; }
    maxglibc="$($1 -T "$BIN" | grep -o "GLIBC_[0-9.]*" | sort -Vu | tail -1)"
    echo "cross: max GLIBC symbol version required: $maxglibc (runtime bookworm = 2.36)"
    [ "$(printf "%s\n%s\n" "$maxglibc" GLIBC_2.36 | sort -V | tail -1)" = GLIBC_2.36 ] || { echo "cross: FATAL requires glibc newer than the runtime" >&2; exit 2; }
    echo -n "cross: smoke --version: "; $2 "$BIN" --version
  ' "$TARGET" "$OBJDUMP" "$RUNNER" "$MACHINE"
echo "cross: artifacts:"; ls -la "$ROOT_DIR/dist/hardened/" | grep "$TARGET"
