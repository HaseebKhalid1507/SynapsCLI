#!/usr/bin/env bash
# Build a HARDENED synaps binary (the `command-exec` Cargo feature OFF) and
# package it as a release tarball + sha256 sidecar, matching the layout the
# media-library bridge Dockerfile consumes.
#
# "Hardened" = `cargo build --release --no-default-features`, which compiles the
# `bash` tool out of the binary entirely and never registers the interactive
# `shell_*` tools (see crates/agent-engine/src/tools/registry.rs). The read-only
# MCP path (connect_mcp_server + media-library tools) is unaffected.
#
# Native builds only — run this ON the target arch (no cross-compilation).
# Build on glibc <= the runtime image's (node:24-bookworm-slim = 2.36), so use:
#   * x86_64 → GitHub `ubuntu-22.04`  (glibc 2.35)
#   * aarch64 → GitHub `ubuntu-22.04-arm` (glibc 2.35)
# A newer build host (e.g. ubuntu-24.04 / this dev box) produces a binary that
# requires a newer glibc and will NOT run on bookworm — the Dockerfile's
# `synaps --version` smoke catches that at image-build time.
#
# Usage:
#   scripts/build-hardened.sh                 # autodetect host target triple
#   scripts/build-hardened.sh <target-triple> # e.g. aarch64-unknown-linux-gnu
#
# Outputs (under dist/hardened/):
#   synaps-hardened-<target>.tar.xz
#   synaps-hardened-<target>.tar.xz.sha256
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Resolve the Rust target triple (explicit arg wins; else host default).
if [[ $# -ge 1 && -n "${1:-}" ]]; then
  TARGET="$1"
else
  TARGET="$(rustc -vV | awk -F': ' '/^host:/ {print $2}')"
fi
[[ -n "$TARGET" ]] || { echo "build-hardened: could not resolve target triple" >&2; exit 1; }

OUT_DIR="$ROOT_DIR/dist/hardened"
TAR_NAME="synaps-hardened-${TARGET}.tar.xz"
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

echo "build-hardened: target=$TARGET (command-exec OFF)"

# Ensure the target toolchain is present (no-op when building for the host).
rustup target add "$TARGET" >/dev/null 2>&1 || true

# DEFENSE-IN-DEPTH BUILD: default features disabled removes the command-exec tools.
cargo build --release --no-default-features --bin synaps --target "$TARGET"

BIN="$ROOT_DIR/target/${TARGET}/release/synaps"
[[ -x "$BIN" ]] || { echo "build-hardened: binary not found at $BIN" >&2; exit 1; }

# Fail closed: the whole point of this build is that `bash` is gone. Verify the
# bash tool's description string is absent from the produced binary before we
# ever ship it. (The string is unique to BashTool::description().)
if grep -aq "Execute a bash command and return its output" "$BIN"; then
  echo "build-hardened: FATAL — bash tool present in supposedly-hardened binary" >&2
  exit 2
fi
echo "build-hardened: verified bash tool is absent from the binary"

# Package: a single top-level `synaps` binary (Dockerfile extracts by name).
mkdir -p "$OUT_DIR"
install -m 0755 "$BIN" "$STAGE_DIR/synaps"
tar -C "$STAGE_DIR" -cJf "$OUT_DIR/$TAR_NAME" synaps

# sha256 sidecar (bare "<sha>  <name>" so `sha256sum -c` works).
( cd "$OUT_DIR" && sha256sum "$TAR_NAME" > "$TAR_NAME.sha256" )

SHA="$(awk '{print $1}' "$OUT_DIR/$TAR_NAME.sha256")"
echo "build-hardened: wrote $OUT_DIR/$TAR_NAME"
echo "build-hardened: sha256=$SHA"

# Machine-readable line for CI to capture into the job summary / pin update.
echo "HARDENED_ARTIFACT target=$TARGET file=$TAR_NAME sha256=$SHA"
